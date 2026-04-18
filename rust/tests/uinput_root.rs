//! Root-required integration tests for the uinput output backend.
//!
//! These tests open `/dev/uinput`, which requires `CAP_SYS_ADMIN`. They are
//! marked `#[ignore]` so `cargo test` stays host-safe for unprivileged CI.
//! Run explicitly on a real Linux host:
//!
//! ```text
//! sudo -E cargo test --manifest-path rust/Cargo.toml --test uinput_root -- --ignored --test-threads=1
//! ```

#![cfg(target_os = "linux")]

use std::fs;
use std::thread::sleep;
use std::time::Duration;

use kloak::event::InputPacket;
use kloak::UInput;

/// Settle time for desktop input plumbing to notice the new device, matching
/// the C smoke test's `sleep(2)`.
const SETTLE: Duration = Duration::from_secs(2);

#[test]
#[ignore = "requires root and /dev/uinput"]
fn root_uinput_open_and_drop() {
    let ui = UInput::open().expect("UInput::open failed; run as root");
    sleep(SETTLE);
    drop(ui);
    // The device path is gone after Drop — no stale /sys entry pointing at
    // "kloak" because UI_DEV_DESTROY + close(fd) tore it down.
}

#[test]
#[ignore = "requires root and /dev/uinput"]
fn root_uinput_appears_in_sysfs() {
    let ui = UInput::open().expect("UInput::open failed; run as root");
    sleep(SETTLE);
    // Under /sys/class/input there will be one input entry whose `name`
    // attribute matches "kloak". We scan rather than pin a number because
    // the kernel allocates event IDs dynamically.
    let mut found = false;
    for entry in fs::read_dir("/sys/class/input").expect("read /sys/class/input") {
        let entry = entry.expect("entry");
        let name_path = entry.path().join("name");
        if let Ok(name) = fs::read_to_string(&name_path) {
            if name.trim() == "kloak" {
                found = true;
                break;
            }
        }
    }
    assert!(found, "no /sys/class/input/*/name == 'kloak' after open");
    drop(ui);
}

#[test]
#[ignore = "requires root and /dev/uinput"]
fn root_uinput_emit_packets() {
    let ui = UInput::open().expect("UInput::open failed; run as root");
    sleep(SETTLE);

    // Key press+release, motion, scroll, button. Each call is a fully
    // terminated evdev group (emit_packet ends with SYN_REPORT).
    ui.emit_packet(InputPacket::Key {
        code: 30, // KEY_A
        pressed: true,
    })
    .expect("emit key down");
    ui.emit_packet(InputPacket::Key {
        code: 30,
        pressed: false,
    })
    .expect("emit key up");
    ui.emit_packet(InputPacket::Motion { dx: 5, dy: -3 })
        .expect("emit motion");
    ui.emit_packet(InputPacket::Scroll { vert: 1, horiz: 0 })
        .expect("emit scroll");
    ui.emit_packet(InputPacket::Button {
        code: 272, // BTN_LEFT
        pressed: true,
    })
    .expect("emit button down");
    ui.emit_packet(InputPacket::Button {
        code: 272,
        pressed: false,
    })
    .expect("emit button up");
}
