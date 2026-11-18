//! In-process counters for the worker, rendered as Prometheus metrics by the web
//! adapter. Lock-free atomics, so recording never blocks a download.

use std::{
    sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    time::Duration,
};

use fanfold_domain::{Timestamp, failure::FailureKind};

/// Upper bounds (seconds) of the download duration histogram.
pub const DURATION_BUCKETS_SECS: [f64; 8] = [1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0];

#[derive(Debug, Default)]
pub struct WorkerStats {
    downloaded: AtomicU64,
    updated: AtomicU64,
    unchanged: AtomicU64,
    retry_scheduled: AtomicU64,
    gave_up: AtomicU64,
    failures: [AtomicU64; FailureKind::ALL.len()],
    breaker_trips: AtomicU64,
    tick_errors: AtomicU64,
    /// Unix seconds of the last successful download; 0 when there has been none.
    last_success_secs: AtomicI64,
    buckets: [AtomicU64; DURATION_BUCKETS_SECS.len()],
    duration_sum_ms: AtomicU64,
    duration_count: AtomicU64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DurationHistogram {
    pub bounds: Vec<f64>,
    /// Cumulative counts: `cumulative[i]` is observations `<= bounds[i]`.
    pub cumulative: Vec<u64>,
    pub sum_secs: f64,
    /// Total observations (the `+Inf` bucket).
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatsSnapshot {
    pub downloaded: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub retry_scheduled: u64,
    pub gave_up: u64,
    pub failures: Vec<(FailureKind, u64)>,
    pub breaker_trips: u64,
    pub tick_errors: u64,
    pub last_success: Option<Timestamp>,
    pub duration: DurationHistogram,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl WorkerStats {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn observe(&self, took: Duration) {
        let secs = took.as_secs_f64();
        if let Some(bucket) = DURATION_BUCKETS_SECS
            .iter()
            .position(|bound| secs <= *bound)
            .and_then(|i| self.buckets.get(i))
        {
            bump(bucket);
        }
        let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        self.duration_sum_ms.fetch_add(ms, Ordering::Relaxed);
        bump(&self.duration_count);
    }

    pub fn record_success(&self, updated: bool, changed: bool, took: Duration, at: Timestamp) {
        match (changed, updated) {
            (false, _) => bump(&self.unchanged),
            (true, true) => bump(&self.updated),
            (true, false) => bump(&self.downloaded),
        }
        self.last_success_secs
            .store(at.as_second(), Ordering::Relaxed);
        self.observe(took);
    }

    pub fn record_failure(&self, kind: FailureKind, gave_up: bool, took: Duration) {
        if let Some(counter) = FailureKind::ALL
            .iter()
            .position(|k| *k == kind)
            .and_then(|i| self.failures.get(i))
        {
            bump(counter);
        }
        bump(if gave_up {
            &self.gave_up
        } else {
            &self.retry_scheduled
        });
        self.observe(took);
    }

    pub fn record_breaker_trip(&self) {
        bump(&self.breaker_trips);
    }

    pub fn record_tick_error(&self) {
        bump(&self.tick_errors);
    }

    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut running = 0;
        let cumulative = self
            .buckets
            .iter()
            .map(|b| {
                running += load(b);
                running
            })
            .collect();
        let last = self.last_success_secs.load(Ordering::Relaxed);
        StatsSnapshot {
            downloaded: load(&self.downloaded),
            updated: load(&self.updated),
            unchanged: load(&self.unchanged),
            retry_scheduled: load(&self.retry_scheduled),
            gave_up: load(&self.gave_up),
            failures: FailureKind::ALL
                .iter()
                .copied()
                .zip(self.failures.iter().map(load))
                .collect(),
            breaker_trips: load(&self.breaker_trips),
            tick_errors: load(&self.tick_errors),
            last_success: (last > 0)
                .then(|| Timestamp::from_second(last).ok())
                .flatten(),
            duration: DurationHistogram {
                bounds: DURATION_BUCKETS_SECS.to_vec(),
                cumulative,
                sum_secs: Duration::from_millis(load(&self.duration_sum_ms)).as_secs_f64(),
                count: load(&self.duration_count),
            },
        }
    }
}

/// Counters for the mail trigger, rendered as Prometheus metrics next to [`WorkerStats`].
#[derive(Debug, Default)]
pub struct MailStats {
    enabled: AtomicBool,
    polls_ok: AtomicU64,
    polls_failed: AtomicU64,
    messages: AtomicU64,
    enqueued: AtomicU64,
    already_active: AtomicU64,
    unlisted_sender: AtomicU64,
    without_links: AtomicU64,
    /// Unix seconds of the last successful poll; 0 when there has been none.
    last_success_secs: AtomicI64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailSnapshot {
    pub enabled: bool,
    pub polls_ok: u64,
    pub polls_failed: u64,
    pub messages: u64,
    pub enqueued: u64,
    pub already_active: u64,
    pub unlisted_sender: u64,
    pub without_links: u64,
    pub last_success: Option<Timestamp>,
}

impl MailStats {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark mail ingestion as configured (an unconfigured service exports `enabled 0`,
    /// so a "mail went quiet" alert can tell "off" from "broken").
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    pub fn record_poll(&self, report: &crate::ingest::IngestReport, at: Timestamp) {
        let add = |counter: &AtomicU64, n: usize| {
            counter.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        };
        bump(&self.polls_ok);
        add(&self.messages, report.messages);
        add(&self.enqueued, report.enqueued);
        add(&self.already_active, report.already_active);
        add(&self.unlisted_sender, report.from_unlisted_sender);
        add(&self.without_links, report.without_work_links);
        self.last_success_secs
            .store(at.as_second(), Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        bump(&self.polls_failed);
    }

    #[must_use]
    pub fn snapshot(&self) -> MailSnapshot {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let last = self.last_success_secs.load(Ordering::Relaxed);
        MailSnapshot {
            enabled: self.enabled.load(Ordering::Relaxed),
            polls_ok: load(&self.polls_ok),
            polls_failed: load(&self.polls_failed),
            messages: load(&self.messages),
            enqueued: load(&self.enqueued),
            already_active: load(&self.already_active),
            unlisted_sender: load(&self.unlisted_sender),
            without_links: load(&self.without_links),
            last_success: (last > 0)
                .then(|| Timestamp::from_second(last).ok())
                .flatten(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    #[test]
    fn counts_outcomes_by_kind() {
        let stats = WorkerStats::new();
        stats.record_success(false, true, Duration::from_secs(2), t(100));
        stats.record_success(true, true, Duration::from_secs(2), t(200));
        stats.record_success(true, false, Duration::from_secs(2), t(300));
        stats.record_failure(FailureKind::SiteUnavailable, false, Duration::from_secs(1));
        stats.record_failure(FailureKind::NotFound, true, Duration::from_secs(1));
        stats.record_breaker_trip();
        stats.record_tick_error();
        let s = stats.snapshot();
        assert_eq!((s.downloaded, s.updated, s.unchanged), (1, 1, 1));
        assert_eq!((s.retry_scheduled, s.gave_up), (1, 1));
        assert_eq!((s.breaker_trips, s.tick_errors), (1, 1));
        assert_eq!(s.last_success, Some(t(300)));
        let count = |kind| s.failures.iter().find(|(k, _)| *k == kind).map(|(_, n)| *n);
        assert_eq!(count(FailureKind::SiteUnavailable), Some(1));
        assert_eq!(count(FailureKind::NotFound), Some(1));
        assert_eq!(count(FailureKind::Blocked), Some(0));
        assert_eq!(s.failures.len(), FailureKind::ALL.len());
    }

    #[test]
    fn no_success_means_no_timestamp() {
        assert_eq!(WorkerStats::new().snapshot().last_success, None);
    }

    #[test]
    fn histogram_is_cumulative_and_sums_durations() {
        let stats = WorkerStats::new();
        for secs in [0.5, 3.0, 3.0, 20.0, 1000.0] {
            stats.record_failure(FailureKind::Transient, false, Duration::from_secs_f64(secs));
        }
        let h = stats.snapshot().duration;
        // bounds: 1, 5, 15, 30, 60, 120, 300, 600
        assert_eq!(h.cumulative, [1, 3, 3, 4, 4, 4, 4, 4]);
        assert_eq!(h.count, 5, "the 1000 s observation is only in +Inf");
        assert!((h.sum_secs - 1026.5).abs() < 1e-6, "{}", h.sum_secs);
    }

    #[test]
    fn a_bucket_bound_is_inclusive() {
        let stats = WorkerStats::new();
        stats.record_failure(FailureKind::Transient, false, Duration::from_secs(5));
        assert_eq!(stats.snapshot().duration.cumulative[..3], [0, 1, 1]);
    }

    #[test]
    fn mail_stats_count_polls_and_messages() {
        use crate::ingest::IngestReport;
        let stats = MailStats::new();
        assert_eq!(stats.snapshot(), MailStats::new().snapshot());
        assert!(!stats.snapshot().enabled && stats.snapshot().last_success.is_none());
        stats.enable();
        let report = IngestReport {
            messages: 8,
            from_unlisted_sender: 1,
            without_work_links: 2,
            enqueued: 2,
            already_active: 5,
            truncated_links: 0,
            series_followed: 0,
        };
        stats.record_poll(&report, t(500));
        stats.record_poll(&report, t(900));
        stats.record_failure();
        let s = stats.snapshot();
        assert!(s.enabled);
        assert_eq!((s.polls_ok, s.polls_failed), (2, 1));
        assert_eq!(
            (
                s.messages,
                s.enqueued,
                s.already_active,
                s.unlisted_sender,
                s.without_links
            ),
            (16, 4, 10, 2, 4)
        );
        assert_eq!(s.last_success, Some(t(900)), "the latest successful poll");
    }
}
