//! inotify-based hotplug watcher for `/dev/input`.
//!
//! Port of `applayer_inotify_init` and `handle_inotify_events` in
//! [c/src/kloak.c].  Only events whose filename starts with `"event"` are
//! yielded; all others are silently discarded, matching the C `strncmp` filter.

use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsFd, AsRawFd, RawFd};

/// Direction of a hotplug event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotplugKind {
    Added,
    Removed,
}

/// A single filtered hotplug event.
#[derive(Debug, Clone)]
pub struct HotplugEvent {
    pub kind: HotplugKind,
    /// Short device name, e.g. `"event3"`.
    pub name: String,
}

/// Watches `/dev/input` for `IN_CREATE | IN_DELETE`.
#[derive(Debug)]
pub struct Watcher {
    inotify: Inotify,
}

impl Default for Watcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Watcher {
    /// Initialize and start watching `/dev/input`.
    pub fn new() -> Self {
        let inotify = Inotify::init(InitFlags::IN_CLOEXEC).unwrap_or_else(|e| {
            eprintln!("FATAL ERROR: Could not initialize inotify: {e}");
            std::process::exit(1);
        });
        inotify
            .add_watch(
                "/dev/input",
                AddWatchFlags::IN_CREATE | AddWatchFlags::IN_DELETE,
            )
            .unwrap_or_else(|e| {
                eprintln!("FATAL ERROR: inotify watch on /dev/input: {e}");
                std::process::exit(1);
            });
        Self { inotify }
    }

    /// The file descriptor to pass to `poll`.
    pub fn fd(&self) -> RawFd {
        self.inotify.as_fd().as_raw_fd()
    }

    /// Read pending inotify events and return filtered hotplug notifications.
    ///
    /// Only yields events whose filename starts with `"event"`.  A blocking
    /// read is performed; call only when `POLLIN` is set on `fd()`.
    pub fn read_events(&self) -> Vec<HotplugEvent> {
        let raw = self.inotify.read_events().unwrap_or_else(|e| {
            eprintln!("FATAL ERROR: inotify read: {e}");
            std::process::exit(1);
        });

        raw.into_iter()
            .filter_map(|ev| {
                let name_os: &OsStr = ev.name.as_deref()?;
                if !name_os.as_bytes().starts_with(b"event") {
                    return None;
                }
                let name = name_os.to_string_lossy().into_owned();
                let kind = if ev.mask.contains(AddWatchFlags::IN_CREATE) {
                    HotplugKind::Added
                } else {
                    HotplugKind::Removed
                };
                Some(HotplugEvent { kind, name })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotplug_kind_eq() {
        assert_eq!(HotplugKind::Added, HotplugKind::Added);
        assert_ne!(HotplugKind::Added, HotplugKind::Removed);
    }
}
