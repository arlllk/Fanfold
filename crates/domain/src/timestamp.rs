//! Instants, backed by [`jiff::Timestamp`].
//!
//! Only saturating helpers live here, so the rest of the domain never has to
//! handle jiff's range errors.

use std::time::Duration;

use jiff::SignedDuration;
pub use jiff::Timestamp;

/// `ts + d`, saturating at the largest representable instant.
#[must_use]
pub fn add(ts: Timestamp, d: Duration) -> Timestamp {
    SignedDuration::try_from(d)
        .ok()
        .and_then(|sd| ts.checked_add(sd).ok())
        .unwrap_or(Timestamp::MAX)
}

/// `later - earlier`, clamped to zero when `later` is not after `earlier`.
#[must_use]
pub fn since(later: Timestamp, earlier: Timestamp) -> Duration {
    if later <= earlier {
        return Duration::ZERO;
    }
    Duration::try_from(later.duration_since(earlier)).unwrap_or(Duration::ZERO)
}

/// Unix epoch milliseconds, the storage representation used by adapters.
#[must_use]
pub fn to_millis(ts: Timestamp) -> i64 {
    ts.as_millisecond()
}

/// Inverse of [`to_millis`]; `None` when outside jiff's supported range.
#[must_use]
pub fn from_millis(ms: i64) -> Option<Timestamp> {
    Timestamp::from_millisecond(ms).ok()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ts(ms: i64) -> Timestamp {
        from_millis(ms).unwrap()
    }

    #[test]
    fn add_and_since_roundtrip() {
        let start = ts(1_000);
        let later = add(start, Duration::from_millis(2_500));
        assert_eq!(to_millis(later), 3_500);
        assert_eq!(since(later, start), Duration::from_millis(2_500));
    }

    #[test]
    fn since_clamps_to_zero() {
        assert_eq!(since(ts(10), ts(20)), Duration::ZERO);
        assert_eq!(since(ts(10), ts(10)), Duration::ZERO);
    }

    #[test]
    fn add_saturates() {
        assert_eq!(add(ts(0), Duration::MAX), Timestamp::MAX);
    }

    #[test]
    fn from_millis_rejects_out_of_range() {
        assert!(from_millis(i64::MAX).is_none());
    }

    proptest! {
        #[test]
        fn add_never_goes_backwards(ms in -1_000_000_000_000i64..1_000_000_000_000, d in 0u64..1_000_000_000) {
            let start = ts(ms);
            prop_assert!(add(start, Duration::from_millis(d)) >= start);
        }

        #[test]
        fn millis_roundtrip(ms in -250_000_000_000_000i64..250_000_000_000_000) {
            prop_assert_eq!(to_millis(ts(ms)), ms);
        }
    }
}
