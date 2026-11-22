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
    if job.resolved_by.is_some() {
        return Status {
            label: "Resolved",
            class: "resolved",
        };
    }
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

// ----------------------------------------------------------------------- library

/// `84,213`.
#[must_use]
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `950`, `8.4k`, `84k`, `1.2M`: one decimal below ten of a unit, none above.
#[must_use]
pub fn short(n: u64) -> String {
    // In u128: `value * 10` must not overflow for any u64.
    let unit = |value: u64, size: u128, suffix: &str| {
        let value = u128::from(value);
        let tenths = (value * 10 + size / 2) / size;
        match tenths {
            0..100 if !tenths.is_multiple_of(10) => {
                format!("{}.{}{suffix}", tenths / 10, tenths % 10)
            }
            0..100 => format!("{}{suffix}", tenths / 10),
            _ => format!("{}{suffix}", (value + size / 2) / size),
        }
    };
    match n {
        0..1_000 => n.to_string(),
        1_000..999_500 => unit(n, 1_000, "k"),
        _ => unit(n, 1_000_000, "M"),
    }
}

/// AO3's rating as a one-letter badge: (CSS class, letter, the full name for people).
#[must_use]
pub fn rating_badge(rating: &str) -> (&'static str, &'static str) {
    match rating {
        "General Audiences" => ("g", "G"),
        "Teen And Up Audiences" => ("t", "T"),
        "Mature" => ("m", "M"),
        "Explicit" => ("e", "E"),
        _ => ("nr", "?"),
    }
}

/// Number of cover colours in the stylesheet (`.hue-0` … `.hue-11`).
pub const HUES: u32 = 12;

/// A stable colour for a name (its fandom, usually): FNV-1a, so it never changes between
/// releases or machines.
#[must_use]
pub fn hue(seed: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in seed.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h % HUES
}

/// `20 Oct 2026`.
#[must_use]
pub fn day(date: fanfold_domain::metadata::Date) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month =
        usize::try_from(date.month() - 1).map_or("?", |m| MONTHS.get(m).copied().unwrap_or("?"));
    format!("{} {month} {}", date.day(), date.year())
}

/// AO3 summary HTML as plain paragraphs, safe to print escaped.
///
/// Tags are dropped (block ends and `<br>` break paragraphs), the common entities are
/// decoded, whitespace is collapsed. Nothing of the markup survives, so a summary can never
/// inject anything into the page.
#[must_use]
pub fn summary_paragraphs(html: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let mut rest = html;
    let flush = |current: &mut String, paragraphs: &mut Vec<String>| {
        let text = decode_entities(current)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !text.is_empty() {
            paragraphs.push(text);
        }
        current.clear();
    };
    while let Some(open) = rest.find('<') {
        current.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            // An unclosed `<`: the rest is text.
            current.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close]
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            tag.as_str(),
            "p" | "br"
                | "div"
                | "blockquote"
                | "li"
                | "ul"
                | "ol"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "hr"
        ) {
            flush(&mut current, &mut paragraphs);
        } else {
            current.push(' ');
        }
        rest = &rest[open + close + 1..];
    }
    current.push_str(rest);
    flush(&mut current, &mut paragraphs);
    paragraphs
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after.find(';').filter(|&end| end <= 10).and_then(|end| {
            let name = &after[..end];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((c, end))
        });
        if let Some((c, end)) = decoded {
            out.push(c);
            rest = &after[end + 1..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod library_tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn numbers_read_naturally() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(84_213), "84,213");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(short(950), "950");
        assert_eq!(short(1_000), "1k");
        assert_eq!(short(8_449), "8.4k");
        assert_eq!(short(84_213), "84k");
        assert_eq!(short(999_499), "999k");
        assert_eq!(short(999_500), "1M");
        assert_eq!(short(1_250_000), "1.3M");
        assert_eq!(short(12_000_000), "12M");
    }

    #[test]
    fn ratings_become_badges() {
        assert_eq!(rating_badge("Teen And Up Audiences"), ("t", "T"));
        assert_eq!(rating_badge("Explicit"), ("e", "E"));
        assert_eq!(rating_badge("Not Rated"), ("nr", "?"));
    }

    #[test]
    fn hues_are_stable_and_in_range() {
        assert_eq!(hue("Original Work"), hue("Original Work"));
        assert!((0..HUES).contains(&hue("")));
    }

    #[test]
    fn days_are_short() {
        let d = fanfold_domain::metadata::Date::new(2026, 10, 20).unwrap();
        assert_eq!(day(d), "20 Oct 2026");
    }

    #[test]
    fn summaries_lose_all_markup() {
        assert_eq!(
            summary_paragraphs("<p>It&#39;s now <b>Ben</b>&#x27;s year.</p><p>Two &amp; three</p>"),
            ["It's now Ben 's year.", "Two & three"]
        );
        assert_eq!(
            summary_paragraphs("line one<br/>line two<br>\n  "),
            ["line one", "line two"]
        );
        assert_eq!(
            summary_paragraphs(r#"<script>alert(1)</script><img src=x onerror="y">hi"#),
            ["alert(1) hi"],
            "tags vanish, their text is just text (and is escaped on output)"
        );
        assert_eq!(summary_paragraphs("a < b & c"), ["a < b & c"]);
        assert_eq!(summary_paragraphs("&bogus; &#xZZ; &"), ["&bogus; &#xZZ; &"]);
        assert_eq!(summary_paragraphs("  <p> </p> "), Vec::<String>::new());
    }

    proptest! {
        #[test]
        fn summaries_never_panic_or_keep_a_tag(s in ".{0,300}") {
            for p in summary_paragraphs(&s) {
                prop_assert!(!p.contains('\n'));
                prop_assert_eq!(p.trim(), p.as_str());
            }
        }

        #[test]
        fn short_never_panics(n in any::<u64>()) {
            let _ = short(n);
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

        let mut resolved = job(JobState::Failed, Some("x"));
        resolved.resolved_by = Some(fanfold_domain::job::JobId::new(9));
        let status = status_of(&resolved);
        assert_eq!((status.label, status.class), ("Resolved", "resolved"));
        assert!(!classes.contains(status.class));
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
