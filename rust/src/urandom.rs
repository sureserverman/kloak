//! `/dev/urandom`-backed unbiased RNG for the jitter scheduler.
//!
//! Port of `random_between()` + `read_random()` in [c/src/kloak.c].
//!
//! The rejection-sampling loop discards raw i64 values that would introduce
//! bias: any value `>= INT64_MAX - (INT64_MAX % maxval)` is rejected and a
//! fresh read is attempted.  This is identical to the C implementation.
//!
//! The fd is held open for the process lifetime — one open per process.

use crate::queue::RandBetween;
use std::fs::File;
use std::io::Read;

#[derive(Debug)]
pub struct UrandomRng {
    file: File,
}

impl UrandomRng {
    pub fn open() -> std::io::Result<Self> {
        let file = File::open("/dev/urandom")?;
        Ok(Self { file })
    }

    /// Read exactly 8 raw bytes and interpret as a non-negative i64.
    ///
    /// Matches `read_random` + the abs/clamp in `random_between`.
    fn read_i64(&mut self) -> i64 {
        let mut buf = [0u8; 8];
        self.file
            .read_exact(&mut buf)
            .expect("FATAL ERROR: could not read from /dev/urandom");
        let raw = i64::from_ne_bytes(buf);
        // Mirror C: `if (randval.val == INT64_MIN) randval.val = 0; else randval.val = llabs(randval.val)`
        if raw == i64::MIN {
            0
        } else {
            raw.abs()
        }
    }
}

impl RandBetween for UrandomRng {
    fn between(&mut self, lower: i64, upper: i64) -> i64 {
        debug_assert!(lower >= 0);
        debug_assert!(upper >= 0);

        if lower >= upper {
            return upper;
        }
        let maxval = upper - lower + 1;
        debug_assert!(maxval > 0);

        // Rejection threshold: discard values that would bias the modulo.
        // Matches C: `while (randval.val >= INT64_MAX - (INT64_MAX % maxval))`
        let threshold = i64::MAX - (i64::MAX % maxval);
        loop {
            let v = self.read_i64();
            if v < threshold {
                return lower + (v % maxval);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng() -> UrandomRng {
        UrandomRng::open().expect("/dev/urandom must be readable in tests")
    }

    #[test]
    fn between_upper_when_lower_eq_upper() {
        let mut r = rng();
        assert_eq!(r.between(50, 50), 50);
    }

    #[test]
    fn between_upper_when_lower_gt_upper() {
        let mut r = rng();
        assert_eq!(r.between(99, 5), 5);
    }

    #[test]
    fn between_zero_is_zero() {
        let mut r = rng();
        assert_eq!(r.between(0, 0), 0);
    }

    #[test]
    fn between_stays_in_range() {
        let mut r = rng();
        for _ in 0..200 {
            let v = r.between(10, 100);
            assert!((10..=100).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn between_zero_to_max_delay_stays_in_range() {
        let mut r = rng();
        for _ in 0..100 {
            let v = r.between(0, 100);
            assert!((0..=100).contains(&v));
        }
    }

    #[test]
    fn between_wide_range_covers_extremes() {
        // Not a distribution test — just a smoke check that both 0 and large
        // values are reachable.
        let mut r = rng();
        let mut saw_small = false;
        let mut saw_large = false;
        for _ in 0..1000 {
            let v = r.between(0, 999);
            if v < 100 {
                saw_small = true;
            }
            if v > 900 {
                saw_large = true;
            }
            if saw_small && saw_large {
                break;
            }
        }
        assert!(saw_small, "never saw a small value in 1000 samples");
        assert!(saw_large, "never saw a large value in 1000 samples");
    }
}
