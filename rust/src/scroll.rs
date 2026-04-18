//! Scroll tick accumulator — port of `get_ticks_from_scroll_accum()` in
//! [c/src/kloak.c]. See §5 of the behavior matrix for the contract.

/// Bytes per tick. Matches the kernel v120 scroll protocol.
pub const UNITS_PER_TICK: i32 = 120;
const UNITS_PER_TICK_F: f64 = 120.0;

/// Drain whole ticks from a scroll accumulator.
///
/// - `accum` is the running scroll value in v120 units (positive = wheel
///   rotated forward / page scrolls up).
/// - Returns the integer tick count equal to `trunc(accum / 120)` and
///   subtracts `ticks * 120` from the accumulator.
/// - A zero accumulator is a no-op and returns 0; the accumulator is left
///   exactly zero.
///
/// # Panics
///
/// Panics if `*accum` is NaN or infinite. The C code asserts the same.
pub fn drain_ticks(accum: &mut f64) -> i32 {
    if *accum == 0.0 {
        return 0;
    }
    assert!(accum.is_finite(), "scroll accumulator must be finite");
    let ticks_f = *accum / UNITS_PER_TICK_F;
    assert!(
        ticks_f <= (i32::MAX / UNITS_PER_TICK) as f64,
        "scroll accumulator overflows i32"
    );
    assert!(
        ticks_f >= (i32::MIN / UNITS_PER_TICK) as f64,
        "scroll accumulator underflows i32"
    );
    // Cast-to-i32 on f64 in Rust saturates on out-of-range and yields 0 for NaN;
    // asserts above make both impossible. Within range, this matches C's
    // truncation toward zero.
    let ticks = ticks_f as i32;
    if ticks != 0 {
        *accum -= f64::from(ticks) * UNITS_PER_TICK_F;
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_noop() {
        let mut a = 0.0;
        assert_eq!(drain_ticks(&mut a), 0);
        assert_eq!(a, 0.0);
    }

    #[test]
    fn exactly_one_tick() {
        let mut a = 120.0;
        assert_eq!(drain_ticks(&mut a), 1);
        assert_eq!(a, 0.0);
    }

    #[test]
    fn fractional_keeps_remainder() {
        let mut a = 179.0;
        assert_eq!(drain_ticks(&mut a), 1);
        assert_eq!(a, 59.0);
    }

    #[test]
    fn negative_truncates_toward_zero() {
        let mut a = -250.0;
        assert_eq!(drain_ticks(&mut a), -2);
        assert_eq!(a, -10.0);
    }

    #[test]
    fn sub_unit_keeps_value() {
        let mut a = 59.0;
        assert_eq!(drain_ticks(&mut a), 0);
        assert_eq!(a, 59.0);
    }

    #[test]
    fn sub_unit_negative_keeps_value() {
        let mut a = -59.0;
        assert_eq!(drain_ticks(&mut a), 0);
        assert_eq!(a, -59.0);
    }

    #[test]
    #[should_panic(expected = "finite")]
    fn nan_panics() {
        let mut a = f64::NAN;
        let _ = drain_ticks(&mut a);
    }
}
