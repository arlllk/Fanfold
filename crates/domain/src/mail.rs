//! Finding AO3 work and series links in an email body.

use std::{collections::HashSet, hash::Hash};

use crate::work::{SeriesId, WorkId, parse_series_url, parse_work_url};

/// Longest URL candidate considered; bounds work on hostile input.
const MAX_URL_BYTES: usize = 2048;

const fn is_url_end(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        )
}

/// All distinct AO3 work ids linked in `body` (plain text or HTML), in order of
/// first appearance. Other links (series, users, unsubscribe) are ignored.
#[must_use]
pub fn extract_work_ids(body: &str) -> Vec<WorkId> {
    extract(body, |url| parse_work_url(url).ok())
}

/// All distinct AO3 series ids linked in `body`, in order of first appearance. A series
/// link does not say which of the message's works belong to it; that comes from each
/// work's own metadata.
#[must_use]
pub fn extract_series_ids(body: &str) -> Vec<SeriesId> {
    extract(body, |url| parse_series_url(url).ok())
}

fn extract<T: Eq + Hash + Copy>(body: &str, parse: impl Fn(&str) -> Option<T>) -> Vec<T> {
    let body = body.replace("&amp;", "&");
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    // Candidates inside an already-scanned span are skipped, which keeps the scan
    // linear even for adversarial input such as "https://" repeated a million times.
    let mut resume_at = 0;
    for (start, _) in body.match_indices("http") {
        if start < resume_at {
            continue;
        }
        let Some(rest) = body.get(start..) else {
            continue;
        };
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        let end = rest
            .char_indices()
            .find(|&(i, c)| i >= MAX_URL_BYTES || is_url_end(c))
            .map_or(rest.len(), |(i, _)| i);
        resume_at = start.saturating_add(end);
        let candidate = rest
            .get(..end)
            .unwrap_or_default()
            .trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if let Some(id) = parse(candidate)
            && seen.insert(id)
        {
            found.push(id);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use proptest::prelude::*;

    use super::*;

    fn ids(body: &str) -> Vec<u64> {
        extract_work_ids(body)
            .into_iter()
            .map(WorkId::get)
            .collect()
    }

    #[test]
    fn series_links_are_found_separately() {
        let body = "Part 3 of <a href=\"https://archiveofourown.org/series/6704161\">The Malfoid Saga</a>: \
                    https://archiveofourown.org/works/95214386 and again \
                    https://archiveofourown.org/series/6704161?page=2 https://archiveofourown.org/series/7";
        let series: Vec<u64> = extract_series_ids(body)
            .into_iter()
            .map(SeriesId::get)
            .collect();
        assert_eq!(series, [6_704_161, 7]);
        assert_eq!(ids(body), [95_214_386], "series links are not works");
    }

    #[test]
    fn plain_text() {
        let body = "New chapter!\nhttps://archiveofourown.org/works/12345/chapters/99.\nBye";
        assert_eq!(ids(body), [12345]);
    }

    #[test]
    fn html_with_entities_and_tracking_params() {
        let body = r#"<a href="https://archiveofourown.org/works/222?view_full_work=true&amp;utm=x">w</a>"#;
        assert_eq!(ids(body), [222]);
    }

    #[test]
    fn dedupes_and_keeps_order() {
        let body = "https://archiveofourown.org/works/3 https://archiveofourown.org/works/1 \
                    https://archiveofourown.org/works/3/chapters/5 https://archiveofourown.org/works/2";
        assert_eq!(ids(body), [3, 1, 2]);
    }

    #[test]
    fn ignores_everything_else() {
        let body = "https://archiveofourown.org/series/9 https://archiveofourown.org/users/a \
                    https://example.com/works/5 https://archiveofourown.org/subscriptions/1";
        assert_eq!(ids(body), Vec::<u64>::new());
    }

    #[test]
    fn markdown_and_angle_brackets() {
        assert_eq!(
            ids("[x](https://archiveofourown.org/works/8) <https://archiveofourown.org/works/9>"),
            [8, 9]
        );
    }

    #[test]
    fn pathological_input_terminates() {
        for unit in ["http", "https://", "https://archiveofourown.org/works/"] {
            let body = unit.repeat(200_000);
            let _ = ids(&body);
        }
    }

    proptest! {
        #[test]
        fn never_panics(s in "\\PC*") {
            let _ = extract_work_ids(&s);
        }

        #[test]
        fn finds_every_embedded_link(nums in proptest::collection::vec(1u64..1_000_000, 1..8)) {
            let mut body = String::new();
            for n in &nums {
                writeln!(body, "see https://archiveofourown.org/works/{n} ok").unwrap();
            }
            let mut expected = Vec::new();
            for n in &nums { if !expected.contains(n) { expected.push(*n); } }
            prop_assert_eq!(ids(&body), expected);
        }
    }
}
