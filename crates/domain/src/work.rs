//! AO3 work identity and URL handling.

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

macro_rules! ao3_id {
    ($(#[$meta:meta])* $name:ident, $err:ident, $what:literal, $path:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "u64", into = "u64")]
        pub struct $name(NonZeroU64);

        #[doc = concat!("A value that is not a valid [`", stringify!($name), "`].")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
        #[error("{what} id must be between 1 and {max} (got {0})", what = $what, max = i64::MAX)]
        pub struct $err(pub u64);

        impl $name {
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
                format!(concat!("https://archiveofourown.org/", $path, "/{}"), self)
            }
        }

        impl TryFrom<u64> for $name {
            type Error = $err;

            fn try_from(value: u64) -> Result<Self, Self::Error> {
                Self::new(value).ok_or($err(value))
            }
        }

        impl From<$name> for u64 {
            fn from(id: $name) -> Self {
                id.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

ao3_id!(
    /// Numeric AO3 work id. Always non-zero and representable as `i64`, so storage
    /// adapters can use a signed 64-bit column without loss.
    WorkId,
    InvalidWorkId,
    "work",
    "works"
);

ao3_id!(
    /// Numeric AO3 series id, with the same guarantees as [`WorkId`].
    SeriesId,
    InvalidSeriesId,
    "series",
    "series"
);

/// What an AO3 link points at, among the things Fanfold can download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ao3Link {
    Work(WorkId),
    Series(SeriesId),
}

/// Why a string is not a downloadable AO3 work or series URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UrlError {
    #[error("not a valid URL")]
    Malformed,
    #[error("only http and https are supported")]
    Scheme,
    #[error("not an AO3 host")]
    Host,
    #[error("URL does not point at a work or a series")]
    NotAWork,
    #[error("the id is not a valid number")]
    BadId,
}

const AO3_HOSTS: [&str; 4] = [
    "archiveofourown.org",
    "archiveofourown.com",
    "archiveofourown.net",
    "ao3.org",
];

/// The path segments of an http(s) URL on an AO3 host.
fn ao3_segments(input: &str) -> Result<Vec<String>, UrlError> {
    let url = Url::parse(input.trim()).map_err(|_| UrlError::Malformed)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlError::Scheme);
    }
    let host = url.host_str().ok_or(UrlError::Host)?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    if !AO3_HOSTS.contains(&host) {
        return Err(UrlError::Host);
    }
    Ok(url
        .path_segments()
        .ok_or(UrlError::NotAWork)?
        .map(str::to_owned)
        .collect())
}

/// The number after the first `<kind>/` pair of segments, if there is such a pair.
fn id_after(segments: &[String], kind: &str) -> Option<Result<u64, UrlError>> {
    segments.windows(2).find_map(|pair| match pair {
        [k, id] if k == kind => Some(
            if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                id.parse::<u64>().map_err(|_| UrlError::BadId)
            } else {
                Err(UrlError::BadId)
            },
        ),
        _ => None,
    })
}

/// Accepts `/works/N`, `/works/N/chapters/M`, `/collections/X/works/N`, with or
/// without `www.`, any query or fragment. Series and user URLs are rejected.
pub fn parse_work_url(input: &str) -> Result<WorkId, UrlError> {
    let segments = ao3_segments(input)?;
    let id = id_after(&segments, "works").ok_or(UrlError::NotAWork)??;
    WorkId::new(id).ok_or(UrlError::BadId)
}

/// Accepts `/series/N`, with or without `www.`, any query or fragment.
pub fn parse_series_url(input: &str) -> Result<SeriesId, UrlError> {
    let segments = ao3_segments(input)?;
    let id = id_after(&segments, "series").ok_or(UrlError::NotAWork)??;
    SeriesId::new(id).ok_or(UrlError::BadId)
}

/// A work or a series link: what the "Add" form accepts.
pub fn parse_ao3_link(input: &str) -> Result<Ao3Link, UrlError> {
    match parse_work_url(input) {
        Ok(work) => Ok(Ao3Link::Work(work)),
        Err(UrlError::NotAWork) => parse_series_url(input).map(Ao3Link::Series),
        Err(e) => Err(e),
    }
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
    fn serde_enforces_invariants() {
        assert_eq!(serde_json::from_str::<WorkId>("42").unwrap(), id(42));
        assert!(serde_json::from_str::<WorkId>("0").is_err());
        assert!(serde_json::from_str::<WorkId>("18446744073709551615").is_err());
        assert_eq!(serde_json::to_string(&id(42)).unwrap(), "42");
    }

    #[test]
    fn series_links_and_the_combined_parser() {
        let series = |n: u64| SeriesId::new(n).unwrap();
        for (input, expected) in [
            ("https://archiveofourown.org/series/6704161", 6_704_161),
            ("https://www.archiveofourown.org/series/12?page=2#x", 12),
            ("http://ao3.org/series/9", 9),
        ] {
            assert_eq!(parse_series_url(input), Ok(series(expected)), "{input}");
            assert_eq!(parse_ao3_link(input), Ok(Ao3Link::Series(series(expected))));
        }
        assert_eq!(
            parse_ao3_link("https://archiveofourown.org/works/5/chapters/9"),
            Ok(Ao3Link::Work(id(5)))
        );
        for (input, err) in [
            ("https://archiveofourown.org/users/x", UrlError::NotAWork),
            ("https://archiveofourown.org/series/", UrlError::BadId),
            ("https://archiveofourown.org/series/0", UrlError::BadId),
            ("https://archiveofourown.org/series/x1", UrlError::BadId),
            ("https://example.com/series/1", UrlError::Host),
        ] {
            assert_eq!(parse_ao3_link(input), Err(err), "{input}");
        }
        assert_eq!(
            series(6_704_161).canonical_url(),
            "https://archiveofourown.org/series/6704161"
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
        fn series_url_roundtrips(n in 1u64..=i64::MAX as u64) {
            let series = SeriesId::new(n).unwrap();
            prop_assert_eq!(parse_ao3_link(&series.canonical_url()), Ok(Ao3Link::Series(series)));
        }
    }
}
