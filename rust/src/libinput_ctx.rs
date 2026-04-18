//! libinput path-backend context wrapper.
//!
//! Port of `li_open_restricted`, `li_close_restricted`, `li_interface`,
//! `attach_input_device`, `detach_input_device`, and the DEVICE_ADDED branch
//! of `handle_libinput_event` in [c/src/kloak.c].
//!
//! The C code uses a singly-linked list (`LIST_HEAD`) for bookkeeping; here
//! we use a `HashMap<String, Device>` — same semantics, idiomatic Rust.
//!
//! Hot-unplug race (C: "Hot-unplug race: remove then re-add"): if `attach`
//! is called for an already-tracked device name, it removes the old entry
//! first so the open-grab sequence runs fresh.

use std::collections::HashMap;
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use input::event::device::DeviceEvent;
use input::event::{Event, EventTrait};
use input::{Device, Libinput, LibinputInterface};

// EVIOCGRAB: exclusive-grab ioctl for evdev.
// Value = _IOW('E', 0x90, int) = 0x40044590 on amd64/aarch64.
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

struct GrabInterface;

impl LibinputInterface for GrabInterface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let path_cstr = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| libc::EINVAL)?;
        // SAFETY: `path_cstr` is a valid NUL-terminated path.  We add
        // O_CLOEXEC so child processes don't inherit the evdev fd.
        let fd = unsafe { libc::open(path_cstr.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(unsafe { *libc::__errno_location() });
        }
        let one: libc::c_int = 1;
        // SAFETY: `fd` is valid; EVIOCGRAB is `_IOW('E', 0x90, int)` which
        // passes a pointer to an int — `&one` has the right type and size.
        let rc = unsafe { libc::ioctl(fd, EVIOCGRAB, &one as *const libc::c_int) };
        if rc < 0 {
            eprintln!(
                "FATAL ERROR: Could not grab evdev device '{}'!",
                path.display()
            );
            // SAFETY: fd is valid and we are about to exit.
            unsafe { libc::close(fd) };
            std::process::exit(1);
        }
        // SAFETY: fd is a valid open file descriptor we own.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        // Drop closes the fd via OwnedFd's Drop impl.
        drop(fd);
    }
}

/// Wraps a libinput path-backend context and the device bookkeeping map.
#[derive(Debug)]
pub struct LibinputCtx {
    li: Libinput,
    /// Device name (e.g. "event0") → libinput Device handle.
    devices: HashMap<String, Device>,
    natural_scrolling: bool,
}

impl LibinputCtx {
    /// Create a new path-backend context.
    pub fn new(natural_scrolling: bool) -> Self {
        let li = Libinput::new_from_path(GrabInterface);
        Self {
            li,
            devices: HashMap::new(),
            natural_scrolling,
        }
    }

    /// The file descriptor to poll for readability before calling `dispatch`.
    pub fn fd(&self) -> RawFd {
        use std::os::unix::io::AsRawFd;
        self.li.as_raw_fd()
    }

    /// Dispatch pending kernel events into the libinput queue.
    pub fn dispatch(&mut self) {
        if let Err(e) = self.li.dispatch() {
            eprintln!("FATAL ERROR: libinput dispatch failed: {e}");
            std::process::exit(1);
        }
    }

    /// Drain all queued events, calling `f` for each.
    ///
    /// Events consumed by the DEVICE_ADDED branch (tap enable, natural
    /// scroll) are handled internally and not forwarded to `f`.
    pub fn drain_events<F>(&mut self, mut f: F)
    where
        F: FnMut(Event),
    {
        for event in self.li.by_ref() {
            // Handle DEVICE_ADDED internally: enable tap and natural scroll.
            if let Event::Device(DeviceEvent::Added(ref _added)) = event {
                let mut dev = event.device();
                if dev.config_tap_finger_count() > 0 {
                    dev.config_tap_set_enabled(true).ok();
                }
                if self.natural_scrolling && dev.config_scroll_has_natural_scroll() {
                    dev.config_scroll_set_natural_scroll_enabled(true).ok();
                }
                // DEVICE_ADDED is not forwarded — no input packet to produce.
                continue;
            }
            f(event);
        }
    }

    /// Attach a device by short name (e.g. `"event0"`).
    ///
    /// If the name is already tracked (hot-unplug race), the old entry is
    /// removed first, matching C `attach_input_device`.
    pub fn attach(&mut self, dev_name: &str) {
        if self.devices.contains_key(dev_name) {
            // Hot-unplug race: detach before re-attaching.
            self.detach(dev_name);
        }
        let path = format!("/dev/input/{dev_name}");
        match self.li.path_add_device(&path) {
            Some(dev) => {
                self.devices.insert(dev_name.to_string(), dev);
            }
            None => {
                // libinput returns None for devices it cannot handle (e.g.
                // not a keyboard/pointer).  Silently skip — matches C behavior
                // where `libinput_path_add_device` returning NULL means "skip".
            }
        }
    }

    /// Detach a device by short name.  No-op if not tracked.
    pub fn detach(&mut self, dev_name: &str) {
        if let Some(dev) = self.devices.remove(dev_name) {
            self.li.path_remove_device(dev);
        }
    }
}
