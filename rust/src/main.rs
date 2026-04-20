//! kloak daemon entry point.
//!
//! - Refuse non-root.
//! - `setenv("LC_ALL", "C")` before any locale-sensitive call.
//! - Sleep `startup_delay` ms before touching uinput / evdev.
//! - Enumerate `/dev/input/event*` and attach every keyboard/mouse.
//! - Poll loop: drain every readable evdev fd → translate → emit due → poll.
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
    use kloak::evdev::EvdevCtx;
    use kloak::event::Sink;
    use kloak::hotplug::{HotplugKind, Watcher};
    use kloak::time_src::now_ms;
    use kloak::translate::{handle_raw_event, TranslateCtx};
    use kloak::uinput::UInput;
    use kloak::urandom::UrandomRng;
    use kloak::Scheduler;

    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::unix::io::BorrowedFd;
    use std::thread::sleep;
    use std::time::Duration;

    // SAFETY: setenv is not thread-safe in general, but this is the very
    // first thing main() does — no threads exist yet and no locale-sensitive
    // calls have been made.
    unsafe {
        std::env::set_var("LC_ALL", "C");
    }

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

    sleep(Duration::from_millis(cfg.startup_delay_ms as u64));

    let mut rng = UrandomRng::open().unwrap_or_else(|e| {
        eprintln!("FATAL ERROR: Could not open /dev/urandom: {e}");
        std::process::exit(1);
    });

    let uinput_kbd = UInput::open_kbd().unwrap_or_else(|e| {
        eprintln!("FATAL ERROR: Could not open /dev/uinput (kbd sink): {e}");
        eprintln!(
            "Ensure the 'uinput' kernel module is loaded and this process has CAP_SYS_ADMIN."
        );
        std::process::exit(1);
    });
    let uinput_pointer = UInput::open_pointer().unwrap_or_else(|e| {
        eprintln!("FATAL ERROR: Could not open /dev/uinput (pointer sink): {e}");
        eprintln!(
            "Ensure the 'uinput' kernel module is loaded and this process has CAP_SYS_ADMIN."
        );
        std::process::exit(1);
    });

    let mut ctx = EvdevCtx::new();
    enumerate_input_devices(&mut ctx);

    let watcher = Watcher::new();

    let mut scheduler = Scheduler::new(cfg.max_delay_ms);
    let mut esc_combo = cfg.esc_combo.clone();

    // Reused across poll iterations to avoid per-loop allocation.
    let mut event_buf: Vec<(u16, u16, i32)> = Vec::with_capacity(64);

    loop {
        // First-emitter-wins latch, derived fresh from current device state.
        // `true` iff some attached VM tablet already won the race (its
        // `FrameAccum::is_primary_tablet` is set). Deriving rather than
        // persisting means the latch auto-resets when the primary tablet
        // detaches — otherwise a dead winner (e.g. spice vdagent tablet torn
        // down mid-session) would permanently mute every surviving tablet.
        let mut primary_tablet_chosen = ctx
            .devices_mut()
            .any(|d| d.frame.is_primary_tablet);

        // 1. Drain every device's pending events, feed translate.
        let names = ctx.names();
        for name in &names {
            let Some(dev) = ctx.device_mut(name) else {
                continue;
            };
            event_buf.clear();
            dev.drain_into(&mut event_buf);
            for (type_, code, value) in event_buf.drain(..) {
                let now = now_ms();
                let mut tctx = TranslateCtx {
                    scheduler: &mut scheduler,
                    rng: &mut rng,
                    esc_combo: &mut esc_combo,
                    natural_scrolling: cfg.natural_scrolling,
                    primary_tablet_chosen: &mut primary_tablet_chosen,
                };
                if handle_raw_event(type_, code, value, &mut dev.frame, now, &mut tctx) {
                    std::process::exit(0);
                }
            }
        }

        // 2. Emit packets whose scheduled time has passed.
        let now = now_ms();
        for sp in scheduler.pop_due(now) {
            let sink = match sp.sink {
                Sink::Kbd => &uinput_kbd,
                Sink::Pointer => &uinput_pointer,
            };
            sink.emit_packet(sp.packet).unwrap_or_else(|e| {
                eprintln!("FATAL ERROR: uinput emit failed: {e}");
                std::process::exit(1);
            });
        }

        // 3. Compute poll timeout.
        let timeout = match scheduler.next_deadline() {
            None => PollTimeout::NONE,
            Some(deadline) => {
                let dur = deadline - now_ms();
                if dur <= 0 {
                    PollTimeout::ZERO
                } else {
                    let ms = dur.min(i64::from(i32::MAX)) as i32;
                    PollTimeout::try_from(ms).unwrap_or(PollTimeout::NONE)
                }
            }
        };

        // 4. Build pollfd array: one per tracked device + inotify at end.
        let names = ctx.names();
        let mut raw_fds: Vec<std::os::unix::io::RawFd> = Vec::with_capacity(names.len() + 1);
        for name in &names {
            if let Some(dev) = ctx.device_mut(name) {
                raw_fds.push(dev.fd());
            }
        }
        raw_fds.push(watcher.fd());

        // SAFETY: each fd is owned by `ctx` or `watcher`, both of which
        // outlive this poll call.
        let borrowed_fds: Vec<BorrowedFd> = raw_fds
            .iter()
            .map(|&fd| unsafe { BorrowedFd::borrow_raw(fd) })
            .collect();

        let mut pollfds: Vec<PollFd> = borrowed_fds
            .iter()
            .map(|bfd| PollFd::new(*bfd, PollFlags::POLLIN))
            .collect();

        // EINTR → retry by falling through to next iteration.
        let _ = poll(&mut pollfds, timeout);

        // 5. Inotify hotplug — always the last pollfd.
        let inotify_idx = pollfds.len() - 1;
        if pollfds[inotify_idx]
            .revents()
            .is_some_and(|f| f.contains(PollFlags::POLLIN))
        {
            for ev in watcher.read_events() {
                match ev.kind {
                    HotplugKind::Added => {
                        if !is_self_uinput(&ev.name) {
                            ctx.attach(&ev.name);
                        }
                    }
                    HotplugKind::Removed => ctx.detach(&ev.name),
                }
            }
        }
        // Device POLLIN bits are consumed on the next iteration's drain
        // pass — drain is non-blocking, so unconditional draining is fine.
    }
}

#[cfg(target_os = "linux")]
fn device_name(name: &str) -> Option<String> {
    let path = format!("/sys/class/input/{name}/device/name");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(target_os = "linux")]
fn is_self_uinput(name: &str) -> bool {
    matches!(
        device_name(name).as_deref(),
        Some("kloak") | Some("kloak-kbd") | Some("kloak-pointer")
    )
}

#[cfg(target_os = "linux")]
fn enumerate_input_devices(ctx: &mut kloak::evdev::EvdevCtx) {
    use std::os::unix::fs::FileTypeExt;

    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(e) => {
            eprintln!("FATAL ERROR: Could not open directory '/dev/input': {e}");
            std::process::exit(1);
        }
    };
    // First-emitter-wins replaces the old "prefer spice vdagent tablet"
    // priority rule — every VM tablet is grabbed but only the one that
    // actually emits ABS first drives the cursor, so enumeration order
    // no longer decides correctness. Sort numerically by event suffix so
    // device-attachment order stays deterministic for logging/debugging.
    let mut names: Vec<String> = dir
        .flatten()
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("event")
                && e.file_type().map(|t| t.is_char_device()).unwrap_or(false)
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by_key(|n| {
        n.trim_start_matches("event")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    for name_str in names {
        if is_self_uinput(&name_str) {
            continue;
        }
        ctx.attach(&name_str);
    }
}
