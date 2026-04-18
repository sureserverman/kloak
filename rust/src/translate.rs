//! libinput event → scheduler translation.
//!
//! Port of `register_esc_combo_event` + `handle_libinput_event` in
//! [c/src/kloak.c].
//!
//! The C function `handle_libinput_event` both translates events and calls
//! `libinput_event_destroy`.  In the Rust crate, event lifetime is managed by
//! the `input` crate's Drop impls, so no explicit destroy call is needed.
//!
//! Scroll accumulation state lives in `VertHorizScrollAccum` which the caller
//! owns and passes in on each call so that sub-tick accumulation persists across
//! events.

use crate::escape::EscCombo;
use crate::queue::{RandBetween, Scheduler};
use crate::scroll::drain_ticks;

use input::event::keyboard::{KeyState, KeyboardEventTrait};
use input::event::pointer::{Axis, ButtonState, PointerScrollEvent};
use input::event::Event;

/// Factor to convert finger/continuous scroll value to v120 units.
/// Matches `SCROLL_ANGLE_TO_UNITS_FACTOR_D = 8.0` in kloak.c.
const SCROLL_ANGLE_TO_UNITS: f64 = 8.0;

/// Running scroll accumulator, one axis each.
#[derive(Debug, Default, Clone, Copy)]
pub struct VertHorizScrollAccum {
    pub vert: f64,
    pub horiz: f64,
}

/// Round a float to the nearest integer, with ties going away from zero.
/// Matches C `(int32_t)(dx < 0 ? dx - 0.5 : dx + 0.5)` for the motion path.
fn round_half_away(v: f64) -> i32 {
    // Clamp before converting to avoid saturating-cast UB in release mode
    // (Rust 1.45+ saturates, but explicit clamp is clearer).
    let rounded = if v < 0.0 { v - 0.5 } else { v + 0.5 };
    rounded.max(f64::from(i32::MIN)).min(f64::from(i32::MAX)) as i32
}

/// Translate a single libinput event into the scheduler.
///
/// Returns `true` if the escape combo was triggered (caller should exit 0).
///
/// The DEVICE_ADDED branch (tap/natural-scroll config) is handled in
/// `LibinputCtx::drain_events` before events reach this function, so callers
/// must not forward DEVICE_ADDED events here.
pub fn translate(
    event: &Event,
    scheduler: &mut Scheduler,
    rng: &mut dyn RandBetween,
    now: i64,
    esc_combo: &mut EscCombo,
    accum: &mut VertHorizScrollAccum,
) -> bool {
    match event {
        Event::Keyboard(kb_event) => {
            use input::event::keyboard::KeyboardEvent;
            let KeyboardEvent::Key(ref ke) = kb_event else {
                return false;
            };
            let key = ke.key();
            let pressed = ke.key_state() == KeyState::Pressed;

            // Escape-combo check runs on every keyboard event, matching C
            // `register_esc_combo_event` being called before `handle_libinput_event`.
            if esc_combo.observe(key, pressed) {
                return true;
            }

            scheduler.enqueue_key(now, rng, key, pressed);
        }

        Event::Pointer(ptr_event) => {
            use input::event::pointer::PointerEvent;
            match ptr_event {
                PointerEvent::Button(ref be) => {
                    let btn = be.button();
                    let pressed = be.button_state() == ButtonState::Pressed;
                    scheduler.enqueue_button(now, rng, btn, pressed);
                }

                PointerEvent::Motion(ref me) => {
                    let dx = me.dx();
                    let dy = me.dy();
                    let idx = round_half_away(dx);
                    let idy = round_half_away(dy);
                    if idx != 0 || idy != 0 {
                        scheduler.enqueue_motion(now, rng, idx, idy);
                    }
                }

                PointerEvent::ScrollWheel(ref sw) => {
                    // v120 units fed directly into the accumulator.
                    if sw.has_axis(Axis::Vertical) {
                        accum.vert += sw.scroll_value_v120(Axis::Vertical);
                    }
                    if sw.has_axis(Axis::Horizontal) {
                        accum.horiz += sw.scroll_value_v120(Axis::Horizontal);
                    }
                    flush_scroll(scheduler, rng, now, accum);
                }

                PointerEvent::ScrollFinger(ref sf) => {
                    if sf.has_axis(Axis::Vertical) {
                        accum.vert += sf.scroll_value(Axis::Vertical) * SCROLL_ANGLE_TO_UNITS;
                    }
                    if sf.has_axis(Axis::Horizontal) {
                        accum.horiz += sf.scroll_value(Axis::Horizontal) * SCROLL_ANGLE_TO_UNITS;
                    }
                    flush_scroll(scheduler, rng, now, accum);
                }

                PointerEvent::ScrollContinuous(ref sc) => {
                    if sc.has_axis(Axis::Vertical) {
                        accum.vert += sc.scroll_value(Axis::Vertical) * SCROLL_ANGLE_TO_UNITS;
                    }
                    if sc.has_axis(Axis::Horizontal) {
                        accum.horiz += sc.scroll_value(Axis::Horizontal) * SCROLL_ANGLE_TO_UNITS;
                    }
                    flush_scroll(scheduler, rng, now, accum);
                }

                // Gestures, absolute motion, and deprecated Axis events are
                // dropped — matching `/* Gestures and other events intentionally dropped. */`
                // in the C source.
                _ => {}
            }
        }

        // Device / Touch / TabletTool / TabletPad / Switch / Gesture — all dropped.
        _ => {}
    }

    false
}

/// Drain whole ticks from both accumulators and enqueue a scroll packet.
fn flush_scroll(
    scheduler: &mut Scheduler,
    rng: &mut dyn RandBetween,
    now: i64,
    accum: &mut VertHorizScrollAccum,
) {
    let vert = drain_ticks(&mut accum.vert);
    let horiz = drain_ticks(&mut accum.horiz);
    scheduler.enqueue_scroll(now, rng, vert, horiz);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_half_away_positive() {
        // 2.5 → 3 (away from zero)
        assert_eq!(round_half_away(2.5), 3);
        // 2.4 → 2
        assert_eq!(round_half_away(2.4), 2);
    }

    #[test]
    fn round_half_away_negative() {
        // -2.5 → -3 (away from zero)
        assert_eq!(round_half_away(-2.5), -3);
        // -2.4 → -2
        assert_eq!(round_half_away(-2.4), -2);
    }

    #[test]
    fn round_half_away_zero() {
        assert_eq!(round_half_away(0.0), 0);
    }

    #[test]
    fn round_half_away_clamps_large() {
        assert_eq!(round_half_away(1e18_f64), i32::MAX);
        assert_eq!(round_half_away(-1e18_f64), i32::MIN);
    }

    #[test]
    fn scroll_accum_flush_produces_ticks() {
        let mut accum = VertHorizScrollAccum {
            vert: 120.0,
            ..Default::default()
        };
        let vert = drain_ticks(&mut accum.vert);
        assert_eq!(vert, 1);
        assert_eq!(accum.vert, 0.0);
    }
}
