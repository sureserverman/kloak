//! kloak daemon entry point — Stage 5 full implementation.
//!
//! Matches the behaviour of `main()` in [c/src/kloak.c] including:
//! - Refuse non-root (UID 0 required).
//! - `setenv("LC_ALL", "C")` before any locale-sensitive call.
//! - Sleep `startup_delay` ms before touching uinput / libinput.
//! - Enumerate `/dev/input/event*` and attach each via libinput path backend.
//! - Poll loop: drain libinput → translate → schedule → emit due → poll.
//! - inotify hotplug: attach on `IN_CREATE`, detach on `IN_DELETE`.
//! - Exit 0 on escape combo.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("kloak only runs on Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    use kloak::config::{ParseOutcome, USAGE};
    use kloak::hotplug::{HotplugKind, Watcher};
    use kloak::libinput_ctx::LibinputCtx;
    use kloak::time_src::now_ms;
    use kloak::translate::{translate, VertHorizScrollAccum};
    use kloak::uinput::UInput;
    use kloak::urandom::UrandomRng;
    use kloak::Scheduler;

    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::thread::sleep;
    use std::time::Duration;

    // SAFETY: setenv is not thread-safe in general, but this is the very
    // first thing main() does — no threads exist yet and no locale-sensitive
    // calls have been made.
    unsafe {
        std::env::set_var("LC_ALL", "C");
    }

    // Root check must match C: `if (getuid() != 0)`.
    // SAFETY: getuid() has no preconditions.
    if unsafe { libc::getuid() } != 0 {
        eprintln!("FATAL ERROR: Must be run as root!");
        std::process::exit(1);
    }

    let cfg = match kloak::config::parse_args(std::env::args().skip(1)) {
        ParseOutcome::Ok(c) => c,
        ParseOutcome::Help => {
            eprint!("{USAGE}");
            std::process::exit(0);
        }
        ParseOutcome::Error(msg) => {
            eprintln!("{msg}");
            eprint!("{USAGE}");
            std::process::exit(1);
        }
    };

    // Sleep before touching uinput/libinput so that a restarting service
    // doesn't race with the system releasing grabbed devices.
    sleep(Duration::from_millis(cfg.startup_delay_ms as u64));

    let mut rng = UrandomRng::open().unwrap_or_else(|e| {
        eprintln!("FATAL ERROR: Could not open /dev/urandom: {e}");
        std::process::exit(1);
    });

    let uinput = UInput::open().unwrap_or_else(|e| {
        eprintln!("FATAL ERROR: Could not open /dev/uinput: {e}");
        eprintln!(
            "Ensure the 'uinput' kernel module is loaded and this process has CAP_SYS_ADMIN."
        );
        std::process::exit(1);
    });

    let mut li_ctx = LibinputCtx::new(cfg.natural_scrolling);

    // Enumerate /dev/input/event* and attach each.
    enumerate_input_devices(&mut li_ctx);

    let watcher = Watcher::new();

    let mut scheduler = Scheduler::new(cfg.max_delay_ms);
    let mut esc_combo = cfg.esc_combo.clone();
    let mut scroll_accum = VertHorizScrollAccum::default();

    // Main poll loop.
    loop {
        // 1. Drain all pending libinput events → translate → enqueue.
        li_ctx.drain_events(|event| {
            let now = now_ms();
            if translate(
                &event,
                &mut scheduler,
                &mut rng,
                now,
                &mut esc_combo,
                &mut scroll_accum,
            ) {
                // Escape combo triggered.
                std::process::exit(0);
            }
        });

        // 2. Emit all packets whose scheduled time has passed.
        let now = now_ms();
        for sp in scheduler.pop_due(now) {
            uinput.emit_packet(sp.packet).unwrap_or_else(|e| {
                eprintln!("FATAL ERROR: uinput emit failed: {e}");
                std::process::exit(1);
            });
        }

        // 3. Compute poll timeout: milliseconds until next deadline, or -1.
        let timeout = match scheduler.next_deadline() {
            None => PollTimeout::NONE,
            Some(deadline) => {
                let dur = deadline - now_ms();
                if dur <= 0 {
                    PollTimeout::ZERO
                } else {
                    // Clamp to i32::MAX (poll's timeout type).
                    let ms = dur.min(i64::from(i32::MAX)) as i32;
                    PollTimeout::try_from(ms).unwrap_or(PollTimeout::NONE)
                }
            }
        };

        // 4. Poll libinput fd and inotify fd.
        let li_fd_raw = li_ctx.fd();
        let in_fd_raw = watcher.fd();

        // SAFETY: Both fds are valid for the duration of `poll`.
        // We use `BorrowedFd` to satisfy nix's lifetime requirement.
        let li_borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(li_fd_raw) };
        let in_borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(in_fd_raw) };

        let mut pollfds = [
            PollFd::new(li_borrowed, PollFlags::POLLIN),
            PollFd::new(in_borrowed, PollFlags::POLLIN),
        ];

        // poll() returns -1 on EINTR — just retry the loop.
        let _ = poll(&mut pollfds, timeout);

        // 5. If libinput fd is ready, dispatch.
        if pollfds[0]
            .revents()
            .is_some_and(|f| f.contains(PollFlags::POLLIN))
        {
            li_ctx.dispatch();
        }

        // 6. If inotify fd is ready, handle hotplug.
        if pollfds[1]
            .revents()
            .is_some_and(|f| f.contains(PollFlags::POLLIN))
        {
            for ev in watcher.read_events() {
                match ev.kind {
                    HotplugKind::Added => {
                        if !is_self_uinput(&ev.name) {
                            li_ctx.attach(&ev.name);
                        }
                    }
                    HotplugKind::Removed => li_ctx.detach(&ev.name),
                }
            }
        }
    }
}

/// Return true if `/sys/class/input/<name>/device/name` identifies kloak's own
/// uinput output device. Attaching our own sink creates a feedback loop: we
/// EVIOCGRAB it exclusively, the compositor can no longer read the events we
/// emit, and libinput re-feeds them into our translate path.
#[cfg(target_os = "linux")]
fn is_self_uinput(name: &str) -> bool {
    let path = format!("/sys/class/input/{name}/device/name");
    std::fs::read_to_string(path)
        .map(|s| s.trim() == "kloak")
        .unwrap_or(false)
}

/// Enumerate `/dev/input` and attach every `event*` character device.
///
/// Matches `applayer_libinput_init` in kloak.c: opens the directory, iterates
/// entries, filters for `DT_CHR` and `event*` prefix. Additionally skips the
/// kloak-owned uinput sink (see `is_self_uinput`).
#[cfg(target_os = "linux")]
fn enumerate_input_devices(ctx: &mut kloak::libinput_ctx::LibinputCtx) {
    use std::os::unix::fs::FileTypeExt;

    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("FATAL ERROR: Could not open directory '/dev/input': {e}");
            std::process::exit(1);
        }
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("event") {
            continue;
        }
        // Filter for character devices, matching C's `entry->d_type != DT_CHR`.
        if let Ok(ft) = entry.file_type() {
            if !ft.is_char_device() {
                continue;
            }
        }
        if is_self_uinput(&name_str) {
            continue;
        }
        ctx.attach(&name_str);
    }
}
