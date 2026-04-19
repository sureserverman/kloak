//! Typed internal event model.
//!
//! `InputPacket` is the post-libinput, pre-uinput representation of a single
//! logical input. The C daemon used a tagged union (`struct input_packet` +
//! `union input_packet_data`); Rust expresses it as an enum with signed
//! `i32` deltas/codes to match kloak's "prefer signed arithmetic" rule.
//!
//! Scheduling metadata (release time) lives on `ScheduledPacket` in the
//! queue module — the event payload itself does not carry timing.

use std::fmt;

/// Which uinput sink a scheduled packet should be emitted on. Relative-motion
/// and keyboard packets go to `Kbd`; absolute-position packets (and the
/// pointer buttons that share the VM-tablet source device) go to `Pointer`.
/// Kept separate from `InputPacket` because one payload shape (e.g. a
/// `Button`) can legitimately come from either a real mouse or a VM tablet
/// and must route differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    Kbd,
    Pointer,
}

/// Raw input payload produced by libinput translation and consumed by the
/// uinput emitter. See §4, §5, §8 of the behavior matrix for semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPacket {
    /// Keyboard key (EV_KEY, non-pointer).
    Key { code: u32, pressed: bool },
    /// Pointer button (EV_KEY, pointer).
    Button { code: u32, pressed: bool },
    /// Pointer motion. `dx`/`dy` are signed deltas already rounded to i32.
    Motion { dx: i32, dy: i32 },
    /// Scroll in whole ticks (v120 / 120).
    Scroll { vert: i32, horiz: i32 },
    /// Absolute pointer position normalized to 0..=32767 on both axes.
    /// Emitted once per SYN frame from VM-tablet devices.
    AbsPos { x: i32, y: i32 },
}

impl InputPacket {
    /// True if this packet can be coalesced with another of the same kind.
    /// Currently only motion packets coalesce; keys/buttons/scroll never do.
    pub fn coalesces_with_motion(&self) -> bool {
        matches!(self, InputPacket::Motion { .. })
    }
}

impl fmt::Display for InputPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputPacket::Key { code, pressed } => {
                write!(f, "Key(code={code}, pressed={pressed})")
            }
            InputPacket::Button { code, pressed } => {
                write!(f, "Button(code={code}, pressed={pressed})")
            }
            InputPacket::Motion { dx, dy } => write!(f, "Motion(dx={dx}, dy={dy})"),
            InputPacket::Scroll { vert, horiz } => {
                write!(f, "Scroll(vert={vert}, horiz={horiz})")
            }
            InputPacket::AbsPos { x, y } => write!(f, "AbsPos(x={x}, y={y})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_coalesces() {
        assert!(InputPacket::Motion { dx: 0, dy: 0 }.coalesces_with_motion());
    }

    #[test]
    fn abs_pos_does_not_coalesce() {
        assert!(!InputPacket::AbsPos { x: 0, y: 0 }.coalesces_with_motion());
    }

    #[test]
    fn abs_pos_display_format() {
        let p = InputPacket::AbsPos { x: 100, y: 200 };
        assert_eq!(format!("{p}"), "AbsPos(x=100, y=200)");
    }

    #[test]
    fn others_do_not_coalesce() {
        assert!(!InputPacket::Key {
            code: 1,
            pressed: true
        }
        .coalesces_with_motion());
        assert!(!InputPacket::Button {
            code: 272,
            pressed: true
        }
        .coalesces_with_motion());
        assert!(!InputPacket::Scroll { vert: 1, horiz: 0 }.coalesces_with_motion());
    }
}
