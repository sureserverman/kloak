//! Monotonic millisecond clock helper.
//!
//! Port of `current_time_ms()` in [c/src/kloak.c].  The first call latches
//! `start_time`; subsequent calls return the delta from that origin.  This
//! matches the C contract exactly: the very first call always returns 0.
//!
//! On Linux, `clock_gettime(CLOCK_MONOTONIC)` never fails, so the call is
//! treated as infallible.

use std::sync::OnceLock;

static START_MS: OnceLock<i64> = OnceLock::new();

/// Return the number of milliseconds elapsed since the first call to this
/// function on this process.  The first call always returns 0.
///
/// Precision: milliseconds; source: `CLOCK_MONOTONIC`.
pub fn now_ms() -> i64 {
    let raw = monotonic_ms();
    let start = START_MS.get_or_init(|| raw);
    raw - start
}

/// Raw CLOCK_MONOTONIC in milliseconds since an arbitrary epoch.
///
/// Separated from `now_ms` so tests can check monotonicity without disturbing
/// the global start-time latch.
pub fn monotonic_ms() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `&mut ts` is a valid pointer to an initialised `timespec`.
    // CLOCK_MONOTONIC never fails on Linux; the return value is always 0.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts);
    }
    // The C code asserts `tv_sec < INT64_MAX`; on any real system a signed
    // 64-bit seconds counter won't overflow for ~292 billion years.
    let ms = ts.tv_sec.saturating_mul(1000) + (ts.tv_nsec / 1_000_000);
    debug_assert!(ms >= 0, "monotonic clock went negative");
    ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_ms_is_non_negative() {
        assert!(monotonic_ms() >= 0);
    }

    #[test]
    fn monotonic_ms_is_non_decreasing() {
        let a = monotonic_ms();
        let b = monotonic_ms();
        assert!(b >= a, "time went backwards: {a} > {b}");
    }

    #[test]
    fn now_ms_first_call_is_zero_or_small() {
        // We cannot guarantee exactly 0 because START_MS may already be
        // latched by a prior test, but we can check that it is non-negative
        // and reasonably small (< 60 000 ms for a typical test suite run).
        let t = now_ms();
        assert!(t >= 0, "now_ms returned negative: {t}");
        assert!(t < 120_000, "now_ms implausibly large: {t}");
    }

    #[test]
    fn now_ms_is_non_decreasing() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a, "now_ms went backwards: {a} > {b}");
    }
}
