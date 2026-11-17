//! Per-site circuit breaker.
//!
//! When the site is having a bad day, hammering it helps nobody. After enough
//! consecutive site-wide failures the breaker opens and the worker stops claiming
//! jobs; after a cool-down a single probe job is let through. Probe fails: stay
//! open, with a longer cool-down. Probe succeeds: close.
//!
//! Pure and single-threaded; the caller owns locking and supplies the time.

use std::time::Duration;

use crate::{Timestamp, timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive site-wide failures that open the breaker (minimum 1).
    pub failure_threshold: u32,
    /// First cool-down; doubles each time a probe fails.
    pub open_for: Duration,
    pub max_open_for: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed {
        consecutive_failures: u32,
    },
    /// Rejecting work until `until`. `trips` counts consecutive openings.
    Open {
        until: Timestamp,
        trips: u32,
    },
    /// One probe is in flight; everything else is rejected until it reports.
    HalfOpen {
        trips: u32,
    },
}

/// A state change callers may want to announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerEvent {
    Tripped,
    Recovered,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    config: BreakerConfig,
    state: BreakerState,
}

impl CircuitBreaker {
    #[must_use]
    pub const fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            state: BreakerState::Closed {
                consecutive_failures: 0,
            },
        }
    }

    #[must_use]
    pub const fn state(&self) -> BreakerState {
        self.state
    }

    /// Whether a job may be started now. Does not change state.
    #[must_use]
    pub fn permits(&self, now: Timestamp) -> bool {
        match self.state {
            BreakerState::Closed { .. } => true,
            BreakerState::Open { until, .. } => now >= until,
            BreakerState::HalfOpen { .. } => false,
        }
    }

    /// Call when a job is actually started: an expired `Open` becomes `HalfOpen`,
    /// so exactly one probe runs.
    pub fn begin(&mut self, now: Timestamp) {
        if let BreakerState::Open { until, trips } = self.state
            && now >= until
        {
            self.state = BreakerState::HalfOpen { trips };
        }
    }

    /// The site answered sensibly (including "that work does not exist").
    pub const fn record_success(&mut self) -> BreakerEvent {
        let was_closed = matches!(self.state, BreakerState::Closed { .. });
        self.state = BreakerState::Closed {
            consecutive_failures: 0,
        };
        if was_closed {
            BreakerEvent::Unchanged
        } else {
            BreakerEvent::Recovered
        }
    }

    /// A site-wide failure (outage, rate limit, block).
    pub fn record_failure(&mut self, now: Timestamp) -> BreakerEvent {
        match self.state {
            BreakerState::Closed {
                consecutive_failures,
            } => {
                let failures = consecutive_failures.saturating_add(1);
                if failures >= self.config.failure_threshold.max(1) {
                    self.state = BreakerState::Open {
                        until: timestamp::add(now, self.cool_down(0)),
                        trips: 1,
                    };
                    BreakerEvent::Tripped
                } else {
                    self.state = BreakerState::Closed {
                        consecutive_failures: failures,
                    };
                    BreakerEvent::Unchanged
                }
            }
            BreakerState::HalfOpen { trips } => {
                self.state = BreakerState::Open {
                    until: timestamp::add(now, self.cool_down(trips)),
                    trips: trips.saturating_add(1),
                };
                BreakerEvent::Unchanged
            }
            // A straggler reporting after the breaker already opened.
            BreakerState::Open { .. } => BreakerEvent::Unchanged,
        }
    }

    fn cool_down(&self, failed_probes: u32) -> Duration {
        let factor = 1u32 << failed_probes.min(20);
        self.config
            .open_for
            .checked_mul(factor)
            .unwrap_or(self.config.max_open_for)
            .min(self.config.max_open_for)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const CONFIG: BreakerConfig = BreakerConfig {
        failure_threshold: 3,
        open_for: Duration::from_secs(60),
        max_open_for: Duration::from_secs(600),
    };

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn tripped(at: i64) -> CircuitBreaker {
        let mut b = CircuitBreaker::new(CONFIG);
        for _ in 0..2 {
            assert_eq!(b.record_failure(t(at)), BreakerEvent::Unchanged);
        }
        assert_eq!(b.record_failure(t(at)), BreakerEvent::Tripped);
        b
    }

    #[test]
    fn opens_after_threshold_consecutive_failures() {
        let b = tripped(100);
        assert_eq!(
            b.state(),
            BreakerState::Open {
                until: t(160),
                trips: 1
            }
        );
        assert!(!b.permits(t(159)));
        assert!(b.permits(t(160)));
    }

    #[test]
    fn success_resets_the_failure_count() {
        let mut b = CircuitBreaker::new(CONFIG);
        b.record_failure(t(0));
        b.record_failure(t(0));
        assert_eq!(b.record_success(), BreakerEvent::Unchanged);
        b.record_failure(t(0));
        b.record_failure(t(0));
        assert_eq!(
            b.state(),
            BreakerState::Closed {
                consecutive_failures: 2
            }
        );
    }

    #[test]
    fn exactly_one_probe_after_cool_down() {
        let mut b = tripped(0);
        assert!(!b.permits(t(10)));
        b.begin(t(10)); // too early: no change
        assert!(matches!(b.state(), BreakerState::Open { .. }));
        b.begin(t(60));
        assert_eq!(b.state(), BreakerState::HalfOpen { trips: 1 });
        assert!(!b.permits(t(61)), "second job must wait for the probe");
    }

    #[test]
    fn failed_probe_doubles_cool_down_up_to_the_cap() {
        let mut b = tripped(0);
        let mut now = 60;
        let mut expected = [120, 240, 480, 600, 600].into_iter();
        for _ in 0..5 {
            b.begin(t(now));
            assert_eq!(b.record_failure(t(now)), BreakerEvent::Unchanged);
            let BreakerState::Open { until, .. } = b.state() else {
                panic!("must be open")
            };
            let wait = expected.next().unwrap();
            assert_eq!(until, t(now + wait));
            now += wait;
        }
    }

    #[test]
    fn successful_probe_closes_and_announces_recovery() {
        let mut b = tripped(0);
        b.begin(t(60));
        assert_eq!(b.record_success(), BreakerEvent::Recovered);
        assert_eq!(
            b.state(),
            BreakerState::Closed {
                consecutive_failures: 0
            }
        );
        assert!(b.permits(t(61)));
    }

    #[test]
    fn straggler_failure_while_open_changes_nothing() {
        let mut b = tripped(0);
        let before = b.state();
        assert_eq!(b.record_failure(t(5)), BreakerEvent::Unchanged);
        assert_eq!(b.state(), before);
    }

    #[test]
    fn zero_threshold_behaves_like_one() {
        let mut b = CircuitBreaker::new(BreakerConfig {
            failure_threshold: 0,
            ..CONFIG
        });
        assert_eq!(b.record_failure(t(0)), BreakerEvent::Tripped);
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Success,
        Failure,
        Begin,
        Tick(i64),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::Success),
            Just(Op::Failure),
            Just(Op::Begin),
            (0i64..400).prop_map(Op::Tick),
        ]
    }

    proptest! {
        #[test]
        fn random_histories_keep_invariants(ops in proptest::collection::vec(op(), 0..200)) {
            let mut b = CircuitBreaker::new(CONFIG);
            let mut now = 0i64;
            for op in ops {
                match op {
                    Op::Success => { b.record_success(); }
                    Op::Failure => { b.record_failure(t(now)); }
                    Op::Begin => b.begin(t(now)),
                    Op::Tick(s) => now += s,
                }
                match b.state() {
                    BreakerState::HalfOpen { .. } => prop_assert!(!b.permits(t(now))),
                    BreakerState::Open { until, .. } => {
                        prop_assert_eq!(b.permits(t(now)), t(now) >= until);
                        // Cool-down never exceeds the cap, measured from now at worst.
                        prop_assert!(timestamp::since(until, t(0)) <= Duration::from_secs(now.unsigned_abs() + 600));
                    }
                    BreakerState::Closed { .. } => prop_assert!(b.permits(t(now))),
                }
            }
        }
    }
}
