//! Parsing of `application/x-www-form-urlencoded` bodies and query strings.

use fanfold_domain::job::JobId;
use url::form_urlencoded;

/// Largest selection a bulk action accepts.
pub const MAX_IDS: usize = 500;

#[must_use]
pub fn pairs(raw: &[u8]) -> Vec<(String, String)> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[must_use]
pub fn first<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Every positive integer submitted under `key`, deduplicated in order, capped at
/// [`MAX_IDS`]. Junk values are ignored rather than failing the whole request.
#[must_use]
pub fn job_ids(pairs: &[(String, String)], key: &str) -> Vec<JobId> {
    let mut seen = std::collections::HashSet::new();
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .filter_map(|(_, v)| v.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
        .filter(|id| seen.insert(*id))
        .take(MAX_IDS)
        .map(JobId::new)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn decodes_pairs() {
        let p = pairs(b"url=https%3A%2F%2Fa.org%2Fworks%2F1&x=a+b&x=%C3%A9");
        assert_eq!(first(&p, "url"), Some("https://a.org/works/1"));
        assert_eq!(first(&p, "x"), Some("a b"), "first value wins");
        assert_eq!(first(&p, "nope"), None);
    }

    #[test]
    fn repeated_ids_are_collected_deduplicated_and_cleaned() {
        let p = pairs(b"id=3&id=1&id=3&id=abc&id=-5&id=0&id=+7&other=9&id=2.5&id=");
        let ids: Vec<i64> = job_ids(&p, "id").iter().map(|i| i.get()).collect();
        assert_eq!(ids, [3, 1, 7]);
    }

    #[test]
    fn selections_are_capped() {
        let body = (1..=MAX_IDS + 50).fold(String::new(), |mut body, n| {
            let _ = write!(body, "id={n}&");
            body
        });
        assert_eq!(job_ids(&pairs(body.as_bytes()), "id").len(), MAX_IDS);
    }

    proptest! {
        #[test]
        fn never_panics_and_only_yields_positive_ids(body in proptest::collection::vec(any::<u8>(), 0..300)) {
            let ids = job_ids(&pairs(&body), "id");
            prop_assert!(ids.iter().all(|i| i.get() > 0));
            prop_assert!(ids.len() <= MAX_IDS);
        }
    }
}
