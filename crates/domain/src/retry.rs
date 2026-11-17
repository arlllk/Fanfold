//! Retry back-off. Pure: jitter is an input.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Failed attempts (that consume budget) before a job is given up on.
    pub max_attempts: u32,
    /// Delay after the first failure; doubles per attempt.
    pub base: Duration,
    /// Hard upper bound for any single delay, jitter included.
    pub cap: Duration,
}

impl RetryPolicy {
    /// Delay before retrying after `attempt` consumed failures.
    ///
    /// `jitter` is a unit value in `[0, 1]` (out-of-range and NaN are tolerated);
    /// it scales the delay into `[0.5x, 1.5x]` before the cap is applied, so a
    /// herd of jobs failing together does not retry together.
    #[must_use]
    pub fn delay(&self, attempt: u32, jitter: f64) -> Duration {
        let jitter = if jitter.is_nan() {
            0.5
        } else {
            jitter.clamp(0.0, 1.0)
        };
        let exponent = i32::try_from(attempt.min(30)).unwrap_or(30);
        let raw = self.base.as_secs_f64() * 2f64.powi(exponent);
        let bounded = raw.min(self.cap.as_secs_f64());
        let jittered = bounded * (0.5 + jitter);
        Duration::try_from_secs_f64(jittered)
            .unwrap_or(self.cap)
            .min(self.cap)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const POLICY: RetryPolicy = RetryPolicy {
        max_attempts: 5,
        base: Duration::from_secs(60),
        cap: Duration::from_secs(3600),
    };

    #[test]
    fn doubles_until_cap() {
        let at = |attempt| POLICY.delay(attempt, 0.5);
        assert_eq!(at(0), Duration::from_secs(60));
        assert_eq!(at(1), Duration::from_secs(120));
        assert_eq!(at(2), Duration::from_secs(240));
        assert_eq!(at(10), Duration::from_secs(3600));
    }

    #[test]
    fn jitter_spans_half_to_one_and_a_half() {
        assert_eq!(POLICY.delay(0, 0.0), Duration::from_secs(30));
        assert_eq!(POLICY.delay(0, 1.0), Duration::from_secs(90));
    }

    #[test]
    fn hostile_jitter_is_tolerated() {
        for jitter in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -3.0, 7.0] {
            assert!(POLICY.delay(1, jitter) <= POLICY.cap);
        }
    }

    proptest! {
        #[test]
        fn never_exceeds_cap(attempt in any::<u32>(), jitter in any::<f64>()) {
            prop_assert!(POLICY.delay(attempt, jitter) <= POLICY.cap);
        }

        #[test]
        fn non_decreasing_in_attempt(attempt in 0u32..100, jitter in 0.0f64..=1.0) {
            prop_assert!(POLICY.delay(attempt, jitter) <= POLICY.delay(attempt + 1, jitter));
        }

        #[test]
        fn non_decreasing_in_jitter(attempt in 0u32..100, a in 0.0f64..=1.0, b in 0.0f64..=1.0) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(POLICY.delay(attempt, lo) <= POLICY.delay(attempt, hi));
        }
    }
}
