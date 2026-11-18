//! Prometheus text exposition (format 0.0.4).

use std::fmt::Write as _;

use fanfold_app::{
    ports::{JobCounts, SiteStatus},
    stats::{MailSnapshot, StatsSnapshot},
};
use fanfold_domain::Timestamp;

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn metric(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Render every metric. Pure: the caller gathers the inputs.
// A flat list of series, one block each: splitting it up would only scatter them.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn render(
    stats: &StatsSnapshot,
    mail: &MailSnapshot,
    counts: &JobCounts,
    site: SiteStatus,
    version: &str,
) -> String {
    let mut out = String::new();

    metric(&mut out, "fanfold_jobs", "gauge", "Jobs by state.");
    for (state, n) in [
        ("queued", counts.queued),
        ("retrying", counts.retrying),
        ("running", counts.running),
        ("succeeded", counts.succeeded),
        ("failed", counts.failed),
        ("cancelled", counts.cancelled),
    ] {
        let _ = writeln!(out, "fanfold_jobs{{state=\"{state}\"}} {n}");
    }

    metric(
        &mut out,
        "fanfold_site_degraded",
        "gauge",
        "1 while downloads are paused because the site is unhealthy.",
    );
    let degraded = u8::from(!matches!(site, SiteStatus::Healthy));
    let _ = writeln!(out, "fanfold_site_degraded {degraded}");

    metric(
        &mut out,
        "fanfold_downloads_total",
        "counter",
        "Finished download attempts by outcome.",
    );
    for (outcome, n) in [
        ("downloaded", stats.downloaded),
        ("updated", stats.updated),
        ("unchanged", stats.unchanged),
        ("retry_scheduled", stats.retry_scheduled),
        ("gave_up", stats.gave_up),
    ] {
        let _ = writeln!(out, "fanfold_downloads_total{{outcome=\"{outcome}\"}} {n}");
    }

    metric(
        &mut out,
        "fanfold_download_failures_total",
        "counter",
        "Failed download attempts by classified kind.",
    );
    for (kind, n) in &stats.failures {
        let _ = writeln!(
            out,
            "fanfold_download_failures_total{{kind=\"{}\"}} {n}",
            escape_label(kind.as_str())
        );
    }

    metric(
        &mut out,
        "fanfold_download_duration_seconds",
        "histogram",
        "Time spent on one download attempt.",
    );
    let h = &stats.duration;
    for (bound, cumulative) in h.bounds.iter().zip(&h.cumulative) {
        let _ = writeln!(
            out,
            "fanfold_download_duration_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
        );
    }
    let _ = writeln!(
        out,
        "fanfold_download_duration_seconds_bucket{{le=\"+Inf\"}} {}",
        h.count
    );
    let _ = writeln!(out, "fanfold_download_duration_seconds_sum {}", h.sum_secs);
    let _ = writeln!(out, "fanfold_download_duration_seconds_count {}", h.count);

    metric(
        &mut out,
        "fanfold_breaker_trips_total",
        "counter",
        "Times the circuit breaker opened.",
    );
    let _ = writeln!(out, "fanfold_breaker_trips_total {}", stats.breaker_trips);

    metric(
        &mut out,
        "fanfold_worker_tick_errors_total",
        "counter",
        "Worker ticks that failed (database or storage errors).",
    );
    let _ = writeln!(
        out,
        "fanfold_worker_tick_errors_total {}",
        stats.tick_errors
    );

    metric(
        &mut out,
        "fanfold_last_success_timestamp_seconds",
        "gauge",
        "Unix time of the last successful download; 0 if none yet.",
    );
    let last = stats.last_success.map_or(0, Timestamp::as_second);
    let _ = writeln!(out, "fanfold_last_success_timestamp_seconds {last}");

    metric(
        &mut out,
        "fanfold_mail_enabled",
        "gauge",
        "1 when mail ingestion is configured.",
    );
    let _ = writeln!(out, "fanfold_mail_enabled {}", u8::from(mail.enabled));

    metric(
        &mut out,
        "fanfold_mail_polls_total",
        "counter",
        "Mailbox polls by result.",
    );
    let _ = writeln!(
        out,
        "fanfold_mail_polls_total{{result=\"ok\"}} {}",
        mail.polls_ok
    );
    let _ = writeln!(
        out,
        "fanfold_mail_polls_total{{result=\"error\"}} {}",
        mail.polls_failed
    );

    metric(
        &mut out,
        "fanfold_mail_messages_total",
        "counter",
        "Messages read from the mailbox, by what became of them.",
    );
    let _ = writeln!(
        out,
        "fanfold_mail_messages_total{{result=\"read\"}} {}",
        mail.messages
    );
    let _ = writeln!(
        out,
        "fanfold_mail_messages_total{{result=\"unlisted_sender\"}} {}",
        mail.unlisted_sender
    );
    let _ = writeln!(
        out,
        "fanfold_mail_messages_total{{result=\"no_work_links\"}} {}",
        mail.without_links
    );

    metric(
        &mut out,
        "fanfold_mail_works_total",
        "counter",
        "Works found in mail, by whether they started a new download.",
    );
    let _ = writeln!(
        out,
        "fanfold_mail_works_total{{result=\"queued\"}} {}",
        mail.enqueued
    );
    let _ = writeln!(
        out,
        "fanfold_mail_works_total{{result=\"already_active\"}} {}",
        mail.already_active
    );

    metric(
        &mut out,
        "fanfold_mail_last_success_timestamp_seconds",
        "gauge",
        "Unix time of the last successful mailbox poll; 0 if none yet.",
    );
    let _ = writeln!(
        out,
        "fanfold_mail_last_success_timestamp_seconds {}",
        mail.last_success.map_or(0, Timestamp::as_second)
    );

    metric(
        &mut out,
        "fanfold_build_info",
        "gauge",
        "Build information.",
    );
    let _ = writeln!(
        out,
        "fanfold_build_info{{version=\"{}\"}} 1",
        escape_label(version)
    );
    out
}

#[cfg(test)]
mod tests {
    use fanfold_app::stats::{MailStats, WorkerStats};
    use fanfold_domain::failure::FailureKind;

    use super::*;

    fn sample() -> String {
        let stats = WorkerStats::new();
        stats.record_success(
            false,
            true,
            std::time::Duration::from_secs(3),
            Timestamp::from_second(1_700_000_000).unwrap(),
        );
        stats.record_failure(
            FailureKind::SiteUnavailable,
            false,
            std::time::Duration::from_secs(20),
        );
        stats.record_breaker_trip();
        let counts = JobCounts {
            queued: 2,
            running: 1,
            retrying: 3,
            succeeded: 40,
            failed: 4,
            cancelled: 0,
        };
        render(
            &stats.snapshot(),
            &MailStats::new().snapshot(),
            &counts,
            SiteStatus::Healthy,
            "1.2.3",
        )
    }

    #[test]
    fn exposes_the_documented_series() {
        let text = sample();
        for line in [
            "fanfold_jobs{state=\"queued\"} 2",
            "fanfold_jobs{state=\"retrying\"} 3",
            "fanfold_jobs{state=\"failed\"} 4",
            "fanfold_site_degraded 0",
            "fanfold_downloads_total{outcome=\"downloaded\"} 1",
            "fanfold_downloads_total{outcome=\"retry_scheduled\"} 1",
            "fanfold_download_failures_total{kind=\"site_unavailable\"} 1",
            "fanfold_download_failures_total{kind=\"not_found\"} 0",
            "fanfold_download_duration_seconds_bucket{le=\"5\"} 1",
            "fanfold_download_duration_seconds_bucket{le=\"30\"} 2",
            "fanfold_download_duration_seconds_bucket{le=\"+Inf\"} 2",
            "fanfold_download_duration_seconds_sum 23",
            "fanfold_download_duration_seconds_count 2",
            "fanfold_breaker_trips_total 1",
            "fanfold_last_success_timestamp_seconds 1700000000",
            "fanfold_build_info{version=\"1.2.3\"} 1",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in:\n{text}"
            );
        }
    }

    #[test]
    fn degraded_and_probing_both_count_as_degraded() {
        let stats = WorkerStats::new().snapshot();
        let counts = JobCounts::default();
        for site in [
            SiteStatus::Probing,
            SiteStatus::Degraded {
                retry_at: Timestamp::from_second(1).unwrap(),
            },
        ] {
            assert!(
                render(&stats, &MailStats::new().snapshot(), &counts, site, "v")
                    .contains("fanfold_site_degraded 1\n")
            );
        }
    }

    #[test]
    fn never_succeeded_reports_zero() {
        let text = render(
            &WorkerStats::new().snapshot(),
            &MailStats::new().snapshot(),
            &JobCounts::default(),
            SiteStatus::Healthy,
            "v",
        );
        assert!(text.contains("fanfold_last_success_timestamp_seconds 0\n"));
    }

    #[test]
    fn every_series_has_help_and_type_and_the_output_is_well_formed() {
        let text = sample();
        let mut typed = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                typed.insert(rest.split(' ').next().unwrap().to_owned());
            } else if !line.starts_with('#') {
                assert!(
                    line.split_whitespace().count() == 2,
                    "bad sample line {line:?}"
                );
                let name = line.split(['{', ' ']).next().unwrap();
                let base = name
                    .trim_end_matches("_bucket")
                    .trim_end_matches("_sum")
                    .trim_end_matches("_count");
                assert!(
                    typed.contains(base) || typed.contains(name),
                    "{name} has no TYPE"
                );
            }
        }
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn label_values_are_escaped() {
        let text = render(
            &WorkerStats::new().snapshot(),
            &MailStats::new().snapshot(),
            &JobCounts::default(),
            SiteStatus::Healthy,
            "a\"b\\c\nd",
        );
        assert!(
            text.contains(r#"fanfold_build_info{version="a\"b\\c\nd"} 1"#),
            "{text}"
        );
    }

    #[test]
    fn mail_metrics_distinguish_off_from_broken() {
        use fanfold_app::ingest::IngestReport;
        let render_mail = |mail: &MailStats| {
            render(
                &WorkerStats::new().snapshot(),
                &mail.snapshot(),
                &JobCounts::default(),
                SiteStatus::Healthy,
                "v",
            )
        };
        let off = render_mail(&MailStats::new());
        assert!(
            off.contains("fanfold_mail_enabled 0\n")
                && off.contains("fanfold_mail_last_success_timestamp_seconds 0\n")
        );

        let mail = MailStats::new();
        mail.enable();
        mail.record_poll(
            &IngestReport {
                messages: 8,
                from_unlisted_sender: 1,
                without_work_links: 0,
                enqueued: 2,
                series_followed: 0,
                already_active: 5,
                truncated_links: 0,
            },
            Timestamp::from_second(1_700_000_123).unwrap(),
        );
        mail.record_failure();
        let text = render_mail(&mail);
        for line in [
            "fanfold_mail_enabled 1",
            "fanfold_mail_polls_total{result=\"ok\"} 1",
            "fanfold_mail_polls_total{result=\"error\"} 1",
            "fanfold_mail_messages_total{result=\"read\"} 8",
            "fanfold_mail_messages_total{result=\"unlisted_sender\"} 1",
            "fanfold_mail_works_total{result=\"queued\"} 2",
            "fanfold_mail_works_total{result=\"already_active\"} 5",
            "fanfold_mail_last_success_timestamp_seconds 1700000123",
        ] {
            assert!(
                text.lines().any(|l| l == line),
                "missing {line:?} in:\n{text}"
            );
        }
    }
}
