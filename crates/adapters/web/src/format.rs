//! Small pure helpers for presenting jobs, times and text.

use fanfold_app::ports::SiteStatus;
use fanfold_domain::{
    Timestamp,
    job::{Job, JobState},
    timestamp,
};

/// How a job is presented: a label for people and a CSS class for the ribbon and
/// chip. Colour is never the only signal: the label always accompanies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub label: &'static str,
    pub class: &'static str,
}

#[must_use]
pub const fn status_of(job: &Job) -> Status {
    match (job.state, job.last_error.is_some()) {
        (JobState::Queued, false) => Status {
            label: "Queued",
            class: "queued",
        },
        (JobState::Queued, true) => Status {
            label: "Retrying",
            class: "retrying",
        },
        (JobState::Running, _) => Status {
            label: "Downloading",
            class: "running",
        },
        (JobState::Succeeded, _) => Status {
            label: "Done",
            class: "done",
        },
        (JobState::Failed, _) => Status {
            label: "Failed",
            class: "failed",
        },
        (JobState::Cancelled, _) => Status {
            label: "Cancelled",
            class: "cancelled",
        },
    }
}

/// `5 min ago`, `in 3 h`, `just now`.
#[must_use]
pub fn relative(now: Timestamp, then: Timestamp) -> String {
    let (secs, future) = if then > now {
        (timestamp::since(then, now).as_secs(), true)
    } else {
        (timestamp::since(now, then).as_secs(), false)
    };
    if secs < 10 {
        return if future {
            "any moment".to_owned()
        } else {
            "just now".to_owned()
        };
    }
    let amount = match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3_600 => format!("{} min", s / 60),
        s if s < 86_400 => format!("{} h", s / 3_600),
        s => format!("{} d", s / 86_400),
    };
    if future {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

/// RFC 3339 UTC, for `<time datetime>` and tooltips.
#[must_use]
pub fn iso(ts: Timestamp) -> String {
    ts.to_string()
}

/// `14:03:09` (UTC), for log lines.
#[must_use]
pub fn clock(ts: Timestamp) -> String {
    ts.strftime("%H:%M:%S").to_string()
}

/// At most `max` characters, with an ellipsis when cut.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{}…", head.trim_end())
    } else {
        head
    }
}

/// First line only, for list previews of multi-line errors.
#[must_use]
pub fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// Headline for the site-health pill: (css class, text).
#[must_use]
pub fn site_summary(status: SiteStatus, now: Timestamp) -> (&'static str, String) {
    match status {
        SiteStatus::Healthy => ("ok", "AO3 reachable".to_owned()),
        SiteStatus::Probing => ("warn", "Checking AO3…".to_owned()),
        SiteStatus::Degraded { retry_at } if retry_at > now => (
            "bad",
            format!(
                "AO3 is having trouble. Trying again {}",
                relative(now, retry_at)
            ),
        ),
        SiteStatus::Degraded { .. } => {
            ("bad", "AO3 is having trouble. Trying again now".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use fanfold_domain::{
        job::{JobId, JobSource},
        work::WorkId,
    };
    use proptest::prelude::*;

    use super::*;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn job(state: JobState, error: Option<&str>) -> Job {
        let mut job = Job::new(
            JobId::new(1),
            WorkId::new(1).unwrap(),
            JobSource::Mail,
            t(0),
        );
        job.state = state;
        job.last_error = error.map(str::to_owned);
        job
    }

    #[test]
    fn statuses_have_labels_and_distinct_classes() {
        let all = [
            (JobState::Queued, None, "Queued", "queued"),
            (JobState::Queued, Some("x"), "Retrying", "retrying"),
            (JobState::Running, None, "Downloading", "running"),
            (JobState::Succeeded, None, "Done", "done"),
            (JobState::Failed, Some("x"), "Failed", "failed"),
            (JobState::Cancelled, None, "Cancelled", "cancelled"),
        ];
        for (state, error, label, class) in all {
            assert_eq!(status_of(&job(state, error)), Status { label, class });
        }
        let classes: std::collections::HashSet<_> = all.iter().map(|a| a.3).collect();
        assert_eq!(classes.len(), all.len());
    }

    #[test]
    fn relative_times() {
        let now = t(100_000);
        for (then, expected) in [
            (t(100_000), "just now"),
            (t(99_995), "just now"),
            (t(99_990), "10 s ago"),
            (t(99_941), "59 s ago"),
            (t(99_940), "1 min ago"),
            (t(96_400), "1 h ago"),
            (t(100_000 - 86_400 * 3), "3 d ago"),
            (t(100_005), "any moment"),
            (t(100_090), "in 1 min"),
            (t(100_000 + 7_200), "in 2 h"),
        ] {
            assert_eq!(relative(now, then), expected, "{then}");
        }
    }

    #[test]
    fn unit_boundaries_are_exact() {
        let now = t(1_000_000);
        for (secs_ago, expected) in [
            (59, "59 s ago"),
            (60, "1 min ago"),
            (3_599, "59 min ago"),
            (3_600, "1 h ago"),
            (86_399, "23 h ago"),
            (86_400, "1 d ago"),
        ] {
            assert_eq!(
                relative(now, t(1_000_000 - secs_ago)),
                expected,
                "{secs_ago} s"
            );
        }
    }

    #[test]
    fn a_retry_that_is_due_exactly_now_says_so() {
        let now = t(1_000);
        let (_, text) = site_summary(SiteStatus::Degraded { retry_at: now }, now);
        assert_eq!(text, "AO3 is having trouble. Trying again now");
        let (_, text) = site_summary(SiteStatus::Degraded { retry_at: t(1_001) }, now);
        assert!(text.ends_with("any moment"), "{text}");
    }

    #[test]
    fn times_render_in_utc() {
        assert_eq!(iso(t(0)), "1970-01-01T00:00:00Z");
        assert_eq!(clock(t(3_661)), "01:01:01");
    }

    #[test]
    fn truncation_respects_characters() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("hello world", 5), "hello…");
        assert_eq!(truncate("héllo wörld ✓✓✓", 8), "héllo wö…");
        assert_eq!(truncate("", 3), "");
    }

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(first_line("\n  \n  boom \nmore"), "boom");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn site_summaries() {
        let now = t(1_000);
        assert_eq!(
            site_summary(SiteStatus::Healthy, now),
            ("ok", "AO3 reachable".into())
        );
        assert_eq!(site_summary(SiteStatus::Probing, now).0, "warn");
        let (class, text) = site_summary(SiteStatus::Degraded { retry_at: t(1_240) }, now);
        assert_eq!(
            (class, text.as_str()),
            ("bad", "AO3 is having trouble. Trying again in 4 min")
        );
        assert!(
            site_summary(SiteStatus::Degraded { retry_at: t(900) }, now)
                .1
                .ends_with("now")
        );
    }

    proptest! {
        #[test]
        fn truncate_never_exceeds_the_limit_plus_ellipsis(s in "\\PC*", max in 0usize..50) {
            prop_assert!(truncate(&s, max).chars().count() <= max + 1);
        }

        #[test]
        fn relative_never_panics(a in -1_000_000_000i64..1_000_000_000, b in -1_000_000_000i64..1_000_000_000) {
            let _ = relative(t(a), t(b));
        }
    }
}
