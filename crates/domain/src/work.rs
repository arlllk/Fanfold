//! AO3 work identity and URL handling.

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

/// Numeric AO3 work id. Always non-zero and representable as `i64`, so storage
/// adapters can use a signed 64-bit column without loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct WorkId(NonZeroU64);

/// A value that is not a valid [`WorkId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("work id must be between 1 and {max} (got {0})", max = i64::MAX)]
pub struct InvalidWorkId(pub u64);

impl WorkId {
    #[must_use]
    pub fn new(id: u64) -> Option<Self> {
        if i64::try_from(id).is_err() {
            return None;
        }
        NonZeroU64::new(id).map(Self)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Storage form; lossless by construction.
    #[must_use]
    pub fn to_i64(self) -> i64 {
        i64::try_from(self.get()).unwrap_or(i64::MAX)
    }

    #[must_use]
    pub fn from_i64(id: i64) -> Option<Self> {
        u64::try_from(id).ok().and_then(Self::new)
    }

    #[must_use]
    pub fn canonical_url(self) -> String {
        format!("https://archiveofourown.org/works/{self}")
    }
}

impl TryFrom<u64> for WorkId {
    type Error = InvalidWorkId;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(InvalidWorkId(value))
    }
}

impl From<WorkId> for u64 {
    fn from(id: WorkId) -> Self {
        id.get()
    }
}

impl fmt::Display for WorkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Why a string is not a downloadable AO3 work URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UrlError {
    #[error("not a valid URL")]
    Malformed,
    #[error("only http and https are supported")]
    Scheme,
    #[error("not an AO3 host")]
    Host,
    #[error("URL does not point at a work")]
    NotAWork,
    #[error("work id is not a valid number")]
    BadId,
}

const AO3_HOSTS: [&str; 4] = [
    "archiveofourown.org",
    "archiveofourown.com",
    "archiveofourown.net",
    "ao3.org",
];

/// Extract the work id from an AO3 URL.
///
/// Accepts `/works/N`, `/works/N/chapters/M`, `/collections/X/works/N`, with or
/// without `www.`, any query or fragment. Series and user URLs are rejected.
pub fn parse_work_url(input: &str) -> Result<WorkId, UrlError> {
    let url = Url::parse(input.trim()).map_err(|_| UrlError::Malformed)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlError::Scheme);
    }
    let host = url.host_str().ok_or(UrlError::Host)?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if !AO3_HOSTS.contains(&host) {
        return Err(UrlError::Host);
    }
    let segments: Vec<&str> = url.path_segments().ok_or(UrlError::NotAWork)?.collect();
    let id = segments
        .windows(2)
        .find_map(|pair| match pair {
            ["works", id] => Some(*id),
            _ => None,
        })
        .ok_or(UrlError::NotAWork)?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UrlError::BadId);
    }
    id.parse::<u64>()
        .ok()
        .and_then(WorkId::new)
        .ok_or(UrlError::BadId)
}

const MAX_TITLE_BYTES: usize = 150;

/// Stable, filesystem-safe file name for a work: `Title [id].epub`.
///
/// The result never contains a path separator or control character, never starts
/// with a dot, and stays well below common 255-byte file name limits.
#[must_use]
pub fn book_file_name(title: &str, work: WorkId) -> String {
    let mut cleaned = String::new();
    let mut last_was_space = true;
    for c in title.chars() {
        let c = if c.is_control() || "/\\:*?\"<>|".contains(c) {
            ' '
        } else {
            c
        };
        if c.is_whitespace() {
            if !last_was_space {
                cleaned.push(' ');
            }
            last_was_space = true;
        } else {
            cleaned.push(c);
            last_was_space = false;
        }
    }
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let mut bounded = String::new();
    for c in trimmed.chars() {
        if bounded.len() + c.len_utf8() > MAX_TITLE_BYTES {
            break;
        }
        bounded.push(c);
    }
    let bounded = bounded.trim_end();
    let stem = if bounded.is_empty() {
        "Untitled"
    } else {
        bounded
    };
    format!("{stem} [{work}].epub")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn id(n: u64) -> WorkId {
        WorkId::new(n).unwrap()
    }

    #[test]
    fn parses_common_shapes() {
        for (input, expected) in [
            ("https://archiveofourown.org/works/123", 123),
            ("http://archiveofourown.org/works/123", 123),
            (
                "https://www.archiveofourown.org/works/123/chapters/456",
                123,
            ),
            (
                "https://archiveofourown.org/works/123?view_adult=true#comments",
                123,
            ),
            ("https://archiveofourown.org/collections/Foo/works/77", 77),
            ("  HTTPS://WWW.ArchiveOfOurOwn.org/works/1  ", 1),
            ("https://ao3.org/works/9", 9),
        ] {
            assert_eq!(parse_work_url(input), Ok(id(expected)), "{input}");
        }
    }

    #[test]
    fn rejects_non_work_inputs() {
        for (input, err) in [
            ("not a url", UrlError::Malformed),
            ("ftp://archiveofourown.org/works/1", UrlError::Scheme),
            ("https://example.com/works/1", UrlError::Host),
            ("https://archiveofourown.org/series/1", UrlError::NotAWork),
            ("https://archiveofourown.org/users/x", UrlError::NotAWork),
            ("https://archiveofourown.org/works/", UrlError::BadId),
            ("https://archiveofourown.org/works/abc", UrlError::BadId),
            ("https://archiveofourown.org/works/+5", UrlError::BadId),
            ("https://archiveofourown.org/works/0", UrlError::BadId),
            (
                "https://archiveofourown.org/works/99999999999999999999",
                UrlError::BadId,
            ),
            (
                "https://archiveofourown.org.evil.com/works/1",
                UrlError::Host,
            ),
        ] {
            assert_eq!(parse_work_url(input), Err(err), "{input}");
        }
    }

    #[test]
    fn work_id_rejects_zero_and_overflow() {
        assert!(WorkId::new(0).is_none());
        assert!(WorkId::new(u64::MAX).is_none());
        assert!(WorkId::new(i64::MAX as u64).is_some());
        assert!(WorkId::from_i64(-1).is_none());
    }

    #[test]
    fn storage_conversions_roundtrip() {
        assert_eq!(id(42).to_i64(), 42);
        assert_eq!(WorkId::from_i64(42), Some(id(42)));
        assert_eq!(WorkId::from_i64(0), None);
        assert_eq!(WorkId::from_i64(i64::MAX), WorkId::new(i64::MAX as u64));
    }

    #[test]
    fn titles_are_cut_at_the_byte_limit_on_a_character_boundary() {
        let name = |title: &str| book_file_name(title, id(5));
        let exact = "a".repeat(150);
        assert_eq!(
            name(&exact),
            format!("{exact} [5].epub"),
            "exactly at the limit is kept whole"
        );
        assert_eq!(
            name(&"a".repeat(151)),
            format!("{exact} [5].epub"),
            "one over is cut"
        );
        let wide = "é".repeat(75); // 2 bytes each: exactly 150 bytes
        assert_eq!(name(&wide), format!("{wide} [5].epub"));
        assert_eq!(
            name(&"é".repeat(76)),
            format!("{wide} [5].epub"),
            "never splits a character"
        );
    }

    #[test]
    fn serde_enforces_invariants() {
        assert_eq!(serde_json::from_str::<WorkId>("42").unwrap(), id(42));
        assert!(serde_json::from_str::<WorkId>("0").is_err());
        assert!(serde_json::from_str::<WorkId>("18446744073709551615").is_err());
        assert_eq!(serde_json::to_string(&id(42)).unwrap(), "42");
    }

    #[test]
    fn file_name_examples() {
        assert_eq!(book_file_name("A/B: C?", id(5)), "A B C [5].epub");
        assert_eq!(book_file_name("", id(5)), "Untitled [5].epub");
        assert_eq!(book_file_name("...", id(5)), "Untitled [5].epub");
        assert_eq!(
            book_file_name("  spaced   out  ", id(5)),
            "spaced out [5].epub"
        );
    }

    proptest! {
        #[test]
        fn canonical_url_roundtrips(n in 1u64..=i64::MAX as u64) {
            let work = id(n);
            prop_assert_eq!(parse_work_url(&work.canonical_url()), Ok(work));
        }

        #[test]
        fn parse_never_panics(s in "\\PC*") {
            let _ = parse_work_url(&s);
        }

        #[test]
        fn file_name_is_always_safe(title in "\\PC*", n in 1u64..=i64::MAX as u64) {
            let name = book_file_name(&title, id(n));
            prop_assert!(!name.contains('/') && !name.contains('\\') && !name.contains('\0'));
            prop_assert!(!name.starts_with('.'));
            let suffix = format!("[{n}].epub");
            prop_assert!(name.ends_with(&suffix));
            prop_assert!(name.len() < 255);
            prop_assert!(!name.chars().any(char::is_control));
        }
    }
}
