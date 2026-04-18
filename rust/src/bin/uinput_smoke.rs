//! Smoke test for `uinput.rs` — opens `/dev/uinput`, waits 2s for desktops
//! to notice the new device, types `hi\n`, then tears down.
//!
//! Run as root:
//!
//! ```text
//! sudo cargo run --manifest-path rust/Cargo.toml --bin uinput-smoke
//! ```
//!
//! Focus a text field before running. Success = `hi` plus a newline arrive
//! in the focused app. Matches the contract of [c/src/uinput_smoke.c].

use std::io;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use kloak::event::InputPacket;
use kloak::UInput;

/// Keycodes from `<linux/input-event-codes.h>`.
const KEY_H: u32 = 35;
const KEY_I: u32 = 23;
const KEY_ENTER: u32 = 28;

fn press(ui: &UInput, code: u32) -> io::Result<()> {
    ui.emit_packet(InputPacket::Key {
        code,
        pressed: true,
    })?;
    ui.emit_packet(InputPacket::Key {
        code,
        pressed: false,
    })?;
    sleep(Duration::from_millis(40));
    Ok(())
}

fn main() -> ExitCode {
    let ui = match UInput::open() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("uinput open: {e}");
            eprintln!("(need root and /dev/uinput present)");
            return ExitCode::FAILURE;
        }
    };

    sleep(Duration::from_secs(2));

    for (label, code) in [("H", KEY_H), ("I", KEY_I), ("ENTER", KEY_ENTER)] {
        if let Err(e) = press(&ui, code) {
            eprintln!("press {label}: {e}");
            return ExitCode::FAILURE;
        }
    }

    sleep(Duration::from_secs(1));
    ExitCode::SUCCESS
}
