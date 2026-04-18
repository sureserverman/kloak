//! Raw-evdev event → scheduler translation.
//!
//! Replaces the prior libinput-based translation. We buffer per-device
//! deltas and key events until `EV_SYN/SYN_REPORT`, then flush the SYN frame
//! into the scheduler. Keys are enqueued immediately (the escape combo must
//! observe every press/release in kernel order); pointer motion and scroll
//! are accumulated and emitted at SYN boundaries.
//!
//! Event-type coverage:
//! - EV_KEY  → Key (code < `BTN_MISC`) or Button (code ≥ `BTN_MISC`).
//!   value==2 (autorepeat) is dropped — matches libinput's behaviour and
//!   lets the downstream compositor drive autorepeat on its own clock.
//! - EV_REL REL_X/REL_Y → accumulated motion.
//! - EV_REL REL_WHEEL_HI_RES/REL_HWHEEL_HI_RES → accumulated scroll in v120.
//! - EV_REL REL_WHEEL/REL_HWHEEL → accumulated scroll, scaled ×120, only
//!   when the device doesn't advertise a hi-res counterpart (avoids
//!   double-counting).
//! - EV_ABS — never reached; devices with ABS are filtered out at attach.
//! - EV_MSC, EV_LED, EV_REP, … → dropped.

use crate::escape::EscCombo;
use crate::queue::{RandBetween, Scheduler};
use crate::scroll::drain_ticks;

// ---------------------------------------------------------------------------
// Kernel evdev ABI constants used by translation.

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_SYN: u16 = 0x00;

const SYN_REPORT: u16 = 0x00;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

/// Lowest pointer-button code in `<linux/input-event-codes.h>`. Codes below
/// this are keyboard keys; codes at or above are mouse/joystick buttons.
const BTN_MISC: u16 = 0x100;

/// Per-device SYN-frame accumulator. Holds the between-SYN deltas plus
/// the device's hi-res wheel capabilities (constant for the lifetime of
/// the device, but kept here to keep `handle_raw_event`'s signature
/// compact and so translate.rs stays independent of the evdev module).
#[derive(Debug, Default, Clone)]
pub struct FrameAccum {
    pub dx: i32,
    pub dy: i32,
    /// Scroll in v120 units, combining hi-res and (scaled) low-res events.
    pub vert_v120: f64,
    pub horiz_v120: f64,
    /// `true` when the device advertises `REL_WHEEL_HI_RES`. When set, we
    /// drop the low-res `REL_WHEEL` event to avoid double-counting.
    pub has_hi_res_vwheel: bool,
    pub has_hi_res_hwheel: bool,
}

/// Per-call translation context — everything shared across every raw
/// event within one poll iteration. Grouping these keeps
/// `handle_raw_event`'s signature compact and makes the per-device
/// (`accum`) vs. shared-across-devices split obvious.
pub struct TranslateCtx<'a> {
    pub scheduler: &'a mut Scheduler,
    pub rng: &'a mut dyn RandBetween,
    pub esc_combo: &'a mut EscCombo,
    pub natural_scrolling: bool,
}

impl std::fmt::Debug for TranslateCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn RandBetween` isn't Debug; elide it to keep the trait small.
        f.debug_struct("TranslateCtx")
            .field("scheduler", &self.scheduler)
            .field("esc_combo", &self.esc_combo)
            .field("natural_scrolling", &self.natural_scrolling)
            .finish_non_exhaustive()
    }
}

/// Feed one raw evdev event (kernel `input_event` tuple) into the
/// accumulator / scheduler. Returns `true` when an `EV_KEY` event completed
/// the escape combo — caller should exit 0.
///
/// `natural_scrolling`: if true, invert both scroll axes before emission.
/// Matches the C `-n`/`--natural-scrolling` flag.
pub fn handle_raw_event(
    type_: u16,
    code: u16,
    value: i32,
    accum: &mut FrameAccum,
    now: i64,
    ctx: &mut TranslateCtx<'_>,
) -> bool {
    match type_ {
        EV_KEY => {
            // value: 0=release, 1=press, 2=autorepeat. Drop autorepeat.
            if value == 2 {
                return false;
            }
            let pressed = value == 1;
            if code < BTN_MISC {
                // Escape combo only runs on keyboard keys, matching C.
                if ctx.esc_combo.observe(u32::from(code), pressed) {
                    return true;
                }
                ctx.scheduler
                    .enqueue_key(now, ctx.rng, u32::from(code), pressed);
            } else {
                ctx.scheduler
                    .enqueue_button(now, ctx.rng, u32::from(code), pressed);
            }
        }
        EV_REL => match code {
            REL_X => accum.dx = accum.dx.saturating_add(value),
            REL_Y => accum.dy = accum.dy.saturating_add(value),
            REL_WHEEL_HI_RES => accum.vert_v120 += f64::from(value),
            REL_HWHEEL_HI_RES => accum.horiz_v120 += f64::from(value),
            REL_WHEEL => {
                if !accum.has_hi_res_vwheel {
                    accum.vert_v120 += f64::from(value) * 120.0;
                }
            }
            REL_HWHEEL => {
                if !accum.has_hi_res_hwheel {
                    accum.horiz_v120 += f64::from(value) * 120.0;
                }
            }
            _ => {}
        },
        EV_SYN => {
            if code == SYN_REPORT {
                flush_frame(accum, now, ctx);
            }
            // SYN_DROPPED (code==3) is ignored: queue overflow means the
            // client should re-sync its state with EVIOCGKEY/etc., but
            // kloak is a stateless passthrough — the next SYN_REPORT will
            // resume correct emission.
        }
        _ => {
            // EV_MSC, EV_LED, EV_REP, EV_SND, EV_FF — all dropped.
        }
    }
    false
}

/// Drain a SYN frame: enqueue motion if nonzero, enqueue scroll if nonzero.
fn flush_frame(accum: &mut FrameAccum, now: i64, ctx: &mut TranslateCtx<'_>) {
    if accum.dx != 0 || accum.dy != 0 {
        ctx.scheduler
            .enqueue_motion(now, ctx.rng, accum.dx, accum.dy);
        accum.dx = 0;
        accum.dy = 0;
    }
    if accum.vert_v120 != 0.0 || accum.horiz_v120 != 0.0 {
        let vert = drain_ticks(&mut accum.vert_v120);
        let horiz = drain_ticks(&mut accum.horiz_v120);
        let (vert, horiz) = if ctx.natural_scrolling {
            (-vert, -horiz)
        } else {
            (vert, horiz)
        };
        ctx.scheduler.enqueue_scroll(now, ctx.rng, vert, horiz);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::escape::EscCombo;
    use crate::queue::RandBetween;

    struct MinRng;
    impl RandBetween for MinRng {
        fn between(&mut self, lower: i64, _upper: i64) -> i64 {
            lower
        }
    }

    fn default_combo() -> EscCombo {
        EscCombo::parse("KEY_RIGHTSHIFT,KEY_ESC").unwrap()
    }

    /// Bundle test scaffolding so each case stays a handful of lines.
    struct Harness {
        accum: FrameAccum,
        scheduler: Scheduler,
        rng: MinRng,
        esc_combo: EscCombo,
        natural_scrolling: bool,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                accum: FrameAccum::default(),
                scheduler: Scheduler::new(100),
                rng: MinRng,
                esc_combo: default_combo(),
                natural_scrolling: false,
            }
        }

        fn with_natural(mut self) -> Self {
            self.natural_scrolling = true;
            self
        }

        fn feed(&mut self, type_: u16, code: u16, value: i32, hi_v: bool, hi_h: bool) -> bool {
            self.accum.has_hi_res_vwheel = hi_v;
            self.accum.has_hi_res_hwheel = hi_h;
            let mut ctx = TranslateCtx {
                scheduler: &mut self.scheduler,
                rng: &mut self.rng,
                esc_combo: &mut self.esc_combo,
                natural_scrolling: self.natural_scrolling,
            };
            handle_raw_event(type_, code, value, &mut self.accum, 0, &mut ctx)
        }
    }

    #[test]
    fn key_press_enqueues() {
        let mut h = Harness::new();
        assert!(!h.feed(EV_KEY, 30, 1, false, false));
        assert_eq!(h.scheduler.queue_len(), 1);
    }

    #[test]
    fn autorepeat_is_dropped() {
        let mut h = Harness::new();
        // value == 2 is autorepeat.
        h.feed(EV_KEY, 30, 2, false, false);
        assert_eq!(h.scheduler.queue_len(), 0);
    }

    #[test]
    fn button_code_goes_to_button_branch() {
        let mut h = Harness::new();
        // BTN_LEFT = 0x110.
        h.feed(EV_KEY, 0x110, 1, false, false);
        assert_eq!(h.scheduler.queue_len(), 1);
    }

    #[test]
    fn escape_combo_triggers_on_keyboard_key() {
        let mut h = Harness::new();
        // KEY_RIGHTSHIFT = 54 press, then KEY_ESC = 1 press.
        assert!(!h.feed(EV_KEY, 54, 1, false, false));
        assert!(h.feed(EV_KEY, 1, 1, false, false));
    }

    #[test]
    fn motion_accumulates_until_syn() {
        let mut h = Harness::new();
        h.feed(EV_REL, REL_X, 3, false, false);
        h.feed(EV_REL, REL_Y, -5, false, false);
        assert_eq!(h.scheduler.queue_len(), 0);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);
        assert_eq!(h.scheduler.queue_len(), 1);
        assert_eq!(h.accum.dx, 0);
        assert_eq!(h.accum.dy, 0);
    }

    #[test]
    fn hi_res_wheel_skips_low_res_duplicate() {
        // Modern mouse emits both REL_WHEEL=1 and REL_WHEEL_HI_RES=120 in
        // the same SYN frame. Only the hi-res value should feed the accum.
        let mut h = Harness::new();
        h.feed(EV_REL, REL_WHEEL_HI_RES, 120, true, false);
        h.feed(EV_REL, REL_WHEEL, 1, true, false);
        h.feed(EV_SYN, SYN_REPORT, 0, true, false);
        assert_eq!(h.scheduler.queue_len(), 1);
    }

    #[test]
    fn low_res_only_scales_to_v120() {
        // Old mouse only emits REL_WHEEL; we scale ×120 into the accum.
        let mut h = Harness::new();
        h.feed(EV_REL, REL_WHEEL, 1, false, false);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);
        assert_eq!(h.scheduler.queue_len(), 1);
    }

    #[test]
    fn natural_scrolling_inverts_vertical() {
        let mut h = Harness::new().with_natural();
        h.feed(EV_REL, REL_WHEEL_HI_RES, 120, true, false);
        h.feed(EV_SYN, SYN_REPORT, 0, true, false);
        let packets = h.scheduler.pop_due(1000);
        assert_eq!(packets.len(), 1);
        match packets[0].packet {
            crate::event::InputPacket::Scroll { vert, horiz: _ } => assert_eq!(vert, -1),
            _ => panic!("expected Scroll"),
        }
    }

    #[test]
    fn unknown_event_types_are_dropped() {
        let mut h = Harness::new();
        // EV_MSC = 0x04.
        h.feed(0x04, 0x04, 123, false, false);
        assert_eq!(h.scheduler.queue_len(), 0);
    }
}
