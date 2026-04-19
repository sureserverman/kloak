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
//! - EV_ABS ABS_X/ABS_Y — accumulated from VM-tablet devices (QEMU USB
//!   Tablet, virtio-tablet). On SYN_REPORT, normalized against the device's
//!   `abs_x_max`/`abs_y_max` into the uinput sink's 0..=32767 output range
//!   and enqueued as `InputPacket::AbsPos`. Other ABS codes (MT slots,
//!   pressure, tilt) are dropped; real touchpads with ABS_MT_SLOT are
//!   rejected at attach so they never reach this layer.
//! - EV_MSC, EV_LED, EV_REP, … → dropped.

use crate::escape::EscCombo;
use crate::event::Sink;
use crate::queue::{RandBetween, Scheduler};
use crate::scroll::drain_ticks;

// ---------------------------------------------------------------------------
// Kernel evdev ABI constants used by translation.

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_SYN: u16 = 0x00;

const SYN_REPORT: u16 = 0x00;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

/// Lowest pointer-button code in `<linux/input-event-codes.h>`. Codes below
/// this are keyboard keys; codes at or above are mouse/joystick buttons.
const BTN_MISC: u16 = 0x100;

/// Per-device SYN-frame accumulator. Holds the between-SYN deltas plus
/// the device's hi-res wheel capabilities (constant for the lifetime of
/// the device, but kept here to keep `handle_raw_event`'s signature
/// compact and so translate.rs stays independent of the evdev module).
#[derive(Debug, Clone)]
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
    /// Latest raw ABS_X / ABS_Y seen in the current SYN frame; flushed on
    /// SYN_REPORT. Remains `None` outside VM-tablet devices.
    pub pending_abs_x: Option<i32>,
    pub pending_abs_y: Option<i32>,
    /// Last flushed raw ABS_X / ABS_Y for this device. The evdev protocol
    /// only emits the axis that changed within a SYN frame — if the upstream
    /// moves only Y, the frame contains no ABS_X. Retain the prior value
    /// here so `flush_frame` can fill the missing axis instead of defaulting
    /// to 0 (which teleports the cursor to the left/top edge).
    pub last_abs_x: Option<i32>,
    pub last_abs_y: Option<i32>,
    /// `Some(max)` when the device is a VM tablet; drives normalization of
    /// raw ABS_X/Y into the uinput sink's 0..=32767 output range.
    pub abs_x_max: Option<i32>,
    pub abs_y_max: Option<i32>,
    /// Which uinput sink packets from this source device route to. Real
    /// keyboards and relative mice → `Sink::Kbd`; VM tablets → `Sink::Pointer`.
    /// Keyboard keys always go to `Sink::Kbd` regardless (handled inside the
    /// scheduler); buttons and scroll follow this field.
    pub sink: Sink,
    /// Pointer-button events deferred until `SYN_REPORT` on `Sink::Pointer`
    /// devices. A VM tablet emits `[ABS_X, ABS_Y, BTN_LEFT, SYN]` in one
    /// frame — enqueuing the button before the position update would make
    /// the compositor register the click at the stale cursor location and
    /// only then update the cursor. Buffering buttons here and flushing
    /// them after `AbsPos` in `flush_frame` guarantees the queue order
    /// `[AbsPos, Button]`, so `kloak-pointer` emits the position first.
    /// `(code, pressed)`.
    pub pending_buttons: Vec<(u16, bool)>,
}

impl Default for FrameAccum {
    fn default() -> Self {
        Self {
            dx: 0,
            dy: 0,
            vert_v120: 0.0,
            horiz_v120: 0.0,
            has_hi_res_vwheel: false,
            has_hi_res_hwheel: false,
            pending_abs_x: None,
            pending_abs_y: None,
            last_abs_x: None,
            last_abs_y: None,
            abs_x_max: None,
            abs_y_max: None,
            sink: Sink::Kbd,
            pending_buttons: Vec::new(),
        }
    }
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
            } else if accum.sink == Sink::Pointer {
                // Defer pointer-sink buttons until SYN_REPORT so `AbsPos`
                // (flushed first in `flush_frame`) precedes the button in
                // the queue — otherwise the click lands at the stale
                // cursor position.
                accum.pending_buttons.push((code, pressed));
            } else {
                ctx.scheduler
                    .enqueue_button(now, ctx.rng, u32::from(code), pressed, accum.sink);
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
        EV_ABS => match code {
            ABS_X => accum.pending_abs_x = Some(value),
            ABS_Y => accum.pending_abs_y = Some(value),
            // ABS_MT_*, ABS_PRESSURE, ABS_TILT_* — dropped. Real touchpads
            // with these axes are rejected at attach time.
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

/// Drain a SYN frame: enqueue motion if nonzero, enqueue scroll if nonzero,
/// enqueue absolute position if the device is a VM tablet and the frame
/// carried an ABS_X or ABS_Y update.
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
        ctx.scheduler
            .enqueue_scroll(now, ctx.rng, vert, horiz, accum.sink);
    }
    if let (Some(x_max), Some(y_max)) = (accum.abs_x_max, accum.abs_y_max) {
        if accum.pending_abs_x.is_some() || accum.pending_abs_y.is_some() {
            // Fall back to the last-seen value for the axis that didn't
            // update this frame; only fall back to 0 on the very first frame
            // when no prior value exists (matches the kernel's own absinfo
            // initial state).
            let raw_x = accum
                .pending_abs_x
                .or(accum.last_abs_x)
                .unwrap_or(0)
                .clamp(0, x_max);
            let raw_y = accum
                .pending_abs_y
                .or(accum.last_abs_y)
                .unwrap_or(0)
                .clamp(0, y_max);
            let x = (i64::from(raw_x) * 32767 / i64::from(x_max)) as i32;
            let y = (i64::from(raw_y) * 32767 / i64::from(y_max)) as i32;
            ctx.scheduler.enqueue_abs_pos(now, ctx.rng, x, y);
            accum.last_abs_x = Some(raw_x);
            accum.last_abs_y = Some(raw_y);
            accum.pending_abs_x = None;
            accum.pending_abs_y = None;
        }
    }
    // Flush deferred pointer-sink buttons AFTER AbsPos so the queue order
    // is [AbsPos, Button]: kloak-pointer emits the position update first
    // and the compositor registers the click at the up-to-date location.
    for (code, pressed) in accum.pending_buttons.drain(..) {
        ctx.scheduler
            .enqueue_button(now, ctx.rng, u32::from(code), pressed, accum.sink);
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
    fn abs_x_y_flush_on_syn_and_normalize() {
        let mut h = Harness::new();
        h.accum.abs_x_max = Some(1000);
        h.accum.abs_y_max = Some(2000);
        h.feed(EV_ABS, ABS_X, 500, false, false);
        h.feed(EV_ABS, ABS_Y, 1000, false, false);
        assert_eq!(h.scheduler.queue_len(), 0);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);

        let pkts = h.scheduler.pop_due(1_000_000);
        assert_eq!(pkts.len(), 1);
        match pkts[0].packet {
            crate::event::InputPacket::AbsPos { x, y } => {
                // 500/1000 * 32767 = 16383; 1000/2000 * 32767 = 16383.
                assert_eq!(x, 16383);
                assert_eq!(y, 16383);
            }
            _ => panic!("expected AbsPos"),
        }
        assert!(h.accum.pending_abs_x.is_none());
        assert!(h.accum.pending_abs_y.is_none());
    }

    #[test]
    fn abs_without_max_is_dropped() {
        // abs_x_max/abs_y_max None → not a VM tablet; any EV_ABS stays
        // buffered but never flushed (defensive — real touchpads are already
        // filtered at attach so this path shouldn't trigger in practice).
        let mut h = Harness::new();
        h.feed(EV_ABS, ABS_X, 500, false, false);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);
        assert_eq!(h.scheduler.queue_len(), 0);
    }

    #[test]
    fn abs_normalization_clamps_out_of_range() {
        let mut h = Harness::new();
        h.accum.abs_x_max = Some(1000);
        h.accum.abs_y_max = Some(1000);
        h.feed(EV_ABS, ABS_X, 1500, false, false);
        h.feed(EV_ABS, ABS_Y, -10, false, false);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);
        let pkts = h.scheduler.pop_due(1_000_000);
        match pkts[0].packet {
            crate::event::InputPacket::AbsPos { x, y } => {
                assert_eq!(x, 32767);
                assert_eq!(y, 0);
            }
            _ => panic!("expected AbsPos"),
        }
    }

    #[test]
    fn tablet_button_emits_after_abs_pos_in_same_frame() {
        // Regression: when a VM tablet emits [ABS_X, ABS_Y, BTN_LEFT, SYN]
        // the queue must be [AbsPos, Button] so kloak-pointer updates the
        // cursor position before the compositor sees the click.
        let mut h = Harness::new();
        h.accum.abs_x_max = Some(1000);
        h.accum.abs_y_max = Some(1000);
        h.accum.sink = Sink::Pointer;

        h.feed(EV_ABS, ABS_X, 500, false, false);
        h.feed(EV_ABS, ABS_Y, 500, false, false);
        h.feed(EV_KEY, 0x110, 1, false, false); // BTN_LEFT press
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);

        let pkts = h.scheduler.pop_due(1_000_000);
        assert_eq!(pkts.len(), 2, "one AbsPos + one Button");
        assert!(
            matches!(pkts[0].packet, crate::event::InputPacket::AbsPos { .. }),
            "AbsPos must precede Button, got {:?}",
            pkts[0].packet
        );
        assert!(
            matches!(pkts[1].packet, crate::event::InputPacket::Button { .. }),
            "Button must follow AbsPos, got {:?}",
            pkts[1].packet
        );
        assert_eq!(pkts[1].sink, Sink::Pointer);
    }

    #[test]
    fn kbd_sink_button_still_enqueues_immediately() {
        // Buttons on the keyboard sink (real mice) are not deferred: they
        // don't share a SYN frame with AbsPos, so no ordering surgery
        // needed, and deferring would delay them by one poll iteration.
        let mut h = Harness::new();
        h.accum.sink = Sink::Kbd;
        h.feed(EV_KEY, 0x110, 1, false, false);
        // Enqueued before SYN — no buffering on Kbd sink.
        assert_eq!(h.scheduler.queue_len(), 1);
    }

    #[test]
    fn partial_abs_frame_retains_prior_axis_value() {
        // Regression: evdev only emits axes that changed within a SYN frame.
        // A frame carrying only ABS_Y must not teleport X to 0 — it must
        // reuse the last-seen X value.
        let mut h = Harness::new();
        h.accum.abs_x_max = Some(1000);
        h.accum.abs_y_max = Some(1000);

        // First frame: full [ABS_X, ABS_Y, SYN].
        h.feed(EV_ABS, ABS_X, 500, false, false);
        h.feed(EV_ABS, ABS_Y, 500, false, false);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);

        // Second frame: only ABS_Y changes. X should stay at 500, NOT 0.
        h.feed(EV_ABS, ABS_Y, 600, false, false);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);

        // Scheduler coalesces consecutive AbsPos packets — the final value
        // is what matters for the regression check.
        let pkts = h.scheduler.pop_due(1_000_000);
        assert!(!pkts.is_empty());
        let last = pkts.last().unwrap();
        match last.packet {
            crate::event::InputPacket::AbsPos { x, y } => {
                // 500/1000 * 32767 = 16383; 600/1000 * 32767 = 19660.
                assert_eq!(x, 16383, "X must retain prior value, not reset to 0");
                assert_eq!(y, 19660);
            }
            _ => panic!("expected AbsPos"),
        }
    }

    #[test]
    fn abs_mt_slot_is_dropped() {
        // ABS_MT_SLOT = 0x2f. Should be silently ignored by translate.
        let mut h = Harness::new();
        h.accum.abs_x_max = Some(1000);
        h.accum.abs_y_max = Some(1000);
        h.feed(EV_ABS, 0x2f, 5, false, false);
        h.feed(EV_SYN, SYN_REPORT, 0, false, false);
        assert_eq!(h.scheduler.queue_len(), 0);
    }

    #[test]
    fn unknown_event_types_are_dropped() {
        let mut h = Harness::new();
        // EV_MSC = 0x04.
        h.feed(0x04, 0x04, 123, false, false);
        assert_eq!(h.scheduler.queue_len(), 0);
    }
}
