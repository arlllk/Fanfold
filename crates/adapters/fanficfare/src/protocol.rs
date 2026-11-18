//! The JSON protocol between this crate and `worker/ffd_worker.py`.
//!
//! Pure functions only, so the whole parsing surface can be unit-tested and fuzzed
//! without spawning anything.

use std::{
    fmt::Write as _,
    path::{Component, Path, PathBuf},
};

use fanfold_app::ports::{DownloadError, DownloadedBook};
use fanfold_domain::{
    failure::FailureKind,
    metadata::{
        Author, Chapter, Date, SeriesFacts, SeriesMetadata, SeriesPart, Tag, TagKind, WorkFacts,
        WorkMetadata, WorkStats,
    },
    work::{SeriesId, WorkId},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct WorkerRequest<'a> {
    /// `"download"` or `"list_series"`.
    pub op: &'a str,
    pub url: &'a str,
    pub existing: Option<&'a str>,
    pub workdir: &'a str,
    pub personal_ini: Option<&'a str>,
    /// A Netscape `cookies.txt` the worker loads and writes back.
    pub cookies: Option<&'a str>,
    /// `User-Agent` to send; Cloudflare ties its clearance cookie to it.
    pub user_agent: Option<&'a str>,
    /// Have `FlareSolverr` pass Cloudflare first, then fetch without it (the login fallback).
    pub clearance: Option<Clearance<'a>>,
}

/// Where `FlareSolverr` is, and which page to ask it to open to earn the clearance.
#[derive(Debug, Serialize)]
pub struct Clearance<'a> {
    pub flaresolverr: &'a str,
    pub timeout_ms: u64,
    pub url: &'a str,
}

const fn default_changed() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "status")]
pub enum WorkerResponse {
    #[serde(rename = "ok")]
    Done {
        path: String,
        title: String,
        #[serde(default = "default_changed")]
        changed: bool,
        #[serde(default)]
        log: String,
        /// What the site said about the work; read leniently (see [`work_metadata`]).
        #[serde(default)]
        metadata: Option<serde_json::Value>,
    },
    /// A listed series.
    #[serde(rename = "listed")]
    Listed {
        name: String,
        #[serde(default)]
        summary_html: String,
        #[serde(default)]
        complete: Option<bool>,
        works: Vec<u64>,
        #[serde(default)]
        raw: serde_json::Value,
        #[serde(default)]
        log: String,
    },
    #[serde(rename = "error")]
    Failed {
        kind: FailureKind,
        message: String,
        #[serde(default)]
        log: String,
    },
}

/// The worker's answer is the last line of stdout that is a protocol object.
/// Anything printed before it (stray library chatter) is ignored.
#[must_use]
pub fn parse_response(stdout: &str) -> Option<WorkerResponse> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str(l).ok())
}

/// Lexical containment check: `path` lies under `dir` and never climbs out.
///
/// `publish` consumes the file it is given, so a buggy or compromised worker must
/// not be able to point us at something outside its scratch directory.
#[must_use]
pub fn path_within(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir) && !path.components().any(|c| matches!(c, Component::ParentDir))
}

fn failure(kind: FailureKind, message: impl Into<String>, log: String) -> DownloadError {
    DownloadError {
        kind,
        message: message.into(),
        log,
    }
}

#[derive(Debug, Deserialize)]
struct WireTag {
    kind: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct WireSeries {
    id: u64,
    name: String,
    part: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct WireChapter {
    number: u32,
    title: String,
    ao3_id: Option<u64>,
    posted_on: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireWork {
    title: String,
    #[serde(default)]
    byline: String,
    #[serde(default)]
    summary_html: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    rating: String,
    #[serde(default)]
    restricted: bool,
    #[serde(default)]
    status: String,
    published_on: Option<String>,
    updated_on: Option<String>,
    #[serde(default)]
    chapters_posted: u32,
    chapters_total: Option<u32>,
    #[serde(default)]
    words: u64,
    #[serde(default)]
    authors: Vec<Author>,
    #[serde(default)]
    tags: Vec<WireTag>,
    #[serde(default)]
    series: Vec<WireSeries>,
    #[serde(default)]
    chapters: Vec<WireChapter>,
    #[serde(default)]
    stats: WorkStats,
    #[serde(default)]
    raw: serde_json::Value,
}

fn date(text: Option<&str>) -> Option<Date> {
    text.and_then(|t| t.parse::<Date>().ok())
}

/// The worker's description of a work, as domain metadata. Unknown tag kinds, unusable
/// series ids and unreadable dates are dropped rather than failing the whole record.
pub fn work_metadata(value: serde_json::Value, work: WorkId) -> Result<WorkMetadata, String> {
    let wire: WireWork = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(WorkMetadata {
        work,
        facts: WorkFacts {
            title: wire.title,
            byline: wire.byline,
            summary_html: wire.summary_html,
            language: wire.language,
            rating: wire.rating,
            restricted: wire.restricted,
            status: wire.status,
            published_on: date(wire.published_on.as_deref()),
            updated_on: date(wire.updated_on.as_deref()),
            chapters_posted: wire.chapters_posted,
            chapters_total: wire.chapters_total,
            words: wire.words,
        },
        authors: wire.authors,
        tags: wire
            .tags
            .into_iter()
            .filter_map(|t| {
                Some(Tag {
                    kind: t.kind.parse::<TagKind>().ok()?,
                    name: t.name,
                })
            })
            .collect(),
        series: wire
            .series
            .into_iter()
            .filter_map(|s| {
                Some(SeriesPart {
                    id: SeriesId::new(s.id)?,
                    name: s.name,
                    part: s.part,
                })
            })
            .collect(),
        chapters: wire
            .chapters
            .into_iter()
            .map(|c| Chapter {
                number: c.number,
                title: c.title,
                ao3_id: c.ao3_id,
                posted_on: date(c.posted_on.as_deref()),
            })
            .collect(),
        stats: wire.stats,
        raw: wire.raw,
    })
}

/// Interpret a parsed response to a download.
pub fn into_result(
    response: WorkerResponse,
    workdir: &Path,
    work: WorkId,
) -> Result<DownloadedBook, DownloadError> {
    match response {
        WorkerResponse::Done {
            path,
            title,
            changed,
            mut log,
            metadata,
        } => {
            let path = PathBuf::from(path);
            if path_within(&path, workdir) {
                let metadata = match metadata.map(|m| work_metadata(m, work)) {
                    None => None,
                    Some(Ok(m)) => Some(m),
                    Some(Err(e)) => {
                        let _ = write!(log, "\ncould not read the work's details: {e}");
                        None
                    }
                };
                Ok(DownloadedBook {
                    path,
                    title,
                    changed,
                    log,
                    metadata,
                })
            } else {
                Err(failure(
                    FailureKind::Permanent,
                    "worker reported a file outside its scratch directory",
                    log,
                ))
            }
        }
        WorkerResponse::Failed { kind, message, log } => Err(failure(kind, message, log)),
        WorkerResponse::Listed { log, .. } => Err(failure(
            FailureKind::Permanent,
            "worker answered a download with a series listing",
            log,
        )),
    }
}

/// Interpret a parsed response to a series listing.
pub fn into_listing(
    response: WorkerResponse,
    series: SeriesId,
) -> Result<SeriesMetadata, DownloadError> {
    match response {
        WorkerResponse::Listed {
            name,
            summary_html,
            complete,
            works,
            raw,
            ..
        } => {
            let mut ordered: Vec<WorkId> = Vec::new();
            for work in works.into_iter().filter_map(WorkId::new) {
                if !ordered.contains(&work) {
                    ordered.push(work);
                }
            }
            Ok(SeriesMetadata {
                series,
                facts: SeriesFacts {
                    name,
                    summary_html,
                    complete,
                    works: ordered,
                },
                raw,
            })
        }
        WorkerResponse::Failed { kind, message, log } => Err(failure(kind, message, log)),
        WorkerResponse::Done { log, .. } => Err(failure(
            FailureKind::Permanent,
            "worker answered a series listing with a download",
            log,
        )),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const OK: &str =
        r#"{"status": "ok", "path": "/s/w/story.epub", "title": "T", "changed": true, "log": "l"}"#;

    #[test]
    fn parses_success() {
        assert_eq!(
            parse_response(OK),
            Some(WorkerResponse::Done {
                path: "/s/w/story.epub".into(),
                title: "T".into(),
                changed: true,
                log: "l".into(),
                metadata: None,
            })
        );
    }

    #[test]
    fn changed_defaults_to_true() {
        let line = r#"{"status":"ok","path":"/p","title":"T"}"#;
        assert!(matches!(
            parse_response(line),
            Some(WorkerResponse::Done { changed: true, .. })
        ));
    }

    #[test]
    fn parses_every_failure_kind() {
        for kind in FailureKind::ALL {
            let line = format!(r#"{{"status":"error","kind":"{kind}","message":"m"}}"#);
            assert_eq!(
                parse_response(&line),
                Some(WorkerResponse::Failed {
                    kind,
                    message: "m".into(),
                    log: String::new()
                })
            );
        }
    }

    #[test]
    fn ignores_noise_around_the_answer() {
        let stdout = format!("Updating story...\n{{not json\n\n{OK}\n\ntrailing chatter\n");
        assert!(matches!(
            parse_response(&stdout),
            Some(WorkerResponse::Done { .. })
        ));
    }

    #[test]
    fn the_last_answer_wins() {
        let stdout = format!(
            "{OK}\n{}",
            r#"{"status":"error","kind":"transient","message":"later"}"#
        );
        assert!(matches!(
            parse_response(&stdout),
            Some(WorkerResponse::Failed { .. })
        ));
    }

    #[test]
    fn rejects_unknown_kinds_and_statuses() {
        assert_eq!(
            parse_response(r#"{"status":"error","kind":"exploded","message":"m"}"#),
            None
        );
        assert_eq!(parse_response(r#"{"status":"maybe"}"#), None);
        assert_eq!(parse_response(""), None);
        assert_eq!(parse_response("   \n\n"), None);
    }

    #[test]
    fn containment() {
        let dir = Path::new("/scratch/w1");
        assert!(path_within(Path::new("/scratch/w1/story.epub"), dir));
        assert!(path_within(Path::new("/scratch/w1/sub/x.epub"), dir));
        assert!(!path_within(Path::new("/scratch/w1/../w2/story.epub"), dir));
        assert!(
            !path_within(Path::new("/scratch/w10/story.epub"), dir),
            "prefix of a sibling is not containment"
        );
        assert!(!path_within(Path::new("/etc/passwd"), dir));
        assert!(!path_within(Path::new("story.epub"), dir));
    }

    #[test]
    fn an_escaping_path_becomes_a_permanent_failure() {
        let response = WorkerResponse::Done {
            path: "/etc/passwd".into(),
            title: "T".into(),
            changed: true,
            log: String::new(),
            metadata: None,
        };
        let err = into_result(response, Path::new("/scratch/w1"), work(1)).unwrap_err();
        assert_eq!(err.kind, FailureKind::Permanent);
    }

    fn work(n: u64) -> WorkId {
        WorkId::new(n).unwrap()
    }

    #[test]
    fn metadata_is_read_and_cleaned() {
        let line = r#"{"status":"ok","path":"/s/w/story.epub","title":"T","metadata":{
            "title":"T","byline":"A, B","rating":"Mature","restricted":true,
            "published_on":"2026-10-08","updated_on":"not a date",
            "chapters_posted":4,"chapters_total":null,"words":37766,
            "authors":[{"user":"a","pseud":"A"}],
            "tags":[{"kind":"freeform","name":"Yandere"},{"kind":"mystery","name":"x"}],
            "series":[{"id":6704161,"name":"The Malfoid Saga","part":4},{"id":0,"name":"bad","part":1}],
            "chapters":[{"number":1,"title":"One","ao3_id":11,"posted_on":"2026-11-15"}],
            "stats":{"hits":10,"kudos":null,"comments":1,"bookmarks":2},
            "raw":{"numWords":"37,766"}}}"#
            .replace('\n', " ");
        let book = into_result(parse_response(&line).unwrap(), Path::new("/s/w"), work(9)).unwrap();
        let m = book.metadata.unwrap();
        assert_eq!(m.work, work(9));
        assert_eq!(m.facts.published_on, Some(Date::new(2026, 10, 8).unwrap()));
        assert_eq!(m.facts.updated_on, None, "an unreadable date is dropped");
        assert_eq!(m.facts.chapters_total, None);
        assert!(m.facts.adult() && m.facts.restricted);
        assert_eq!(m.tags.len(), 1, "unknown tag kinds are dropped");
        assert_eq!(m.series.len(), 1, "unusable series ids are dropped");
        assert_eq!(m.series[0].part, Some(4));
        assert_eq!(m.stats.kudos, None);
        assert_eq!(m.raw["numWords"], "37,766");
    }

    #[test]
    fn unreadable_metadata_does_not_lose_the_book() {
        let line = r#"{"status":"ok","path":"/s/w/story.epub","title":"T","log":"l","metadata":{"words":"lots"}}"#;
        let book = into_result(parse_response(line).unwrap(), Path::new("/s/w"), work(9)).unwrap();
        assert_eq!(book.metadata, None);
        assert!(
            book.log.contains("could not read the work's details"),
            "{}",
            book.log
        );
    }

    #[test]
    fn a_listing_keeps_series_order_and_drops_duplicates_and_zero() {
        let line = r#"{"status":"listed","name":"The Malfoid Saga","complete":false,"works":[93774621,94243666,0,93774621,95214386],"raw":{"name":"The Malfoid Saga"}}"#;
        let series = SeriesId::new(6_704_161).unwrap();
        let listing = into_listing(parse_response(line).unwrap(), series).unwrap();
        assert_eq!(listing.series, series);
        assert_eq!(listing.facts.complete, Some(false));
        let works: Vec<u64> = listing.facts.works.iter().map(|w| w.get()).collect();
        assert_eq!(works, [93_774_621, 94_243_666, 95_214_386]);
        assert_eq!(
            into_listing(parse_response(OK).unwrap(), series)
                .unwrap_err()
                .kind,
            FailureKind::Permanent,
            "a download is not a listing"
        );
        assert_eq!(
            into_result(parse_response(line).unwrap(), Path::new("/s/w"), work(1))
                .unwrap_err()
                .kind,
            FailureKind::Permanent,
            "nor the other way round"
        );
    }

    proptest! {
        #[test]
        fn parsing_never_panics(s in "\\PC*") {
            let _ = parse_response(&s);
        }

        #[test]
        fn path_within_never_accepts_parent_components(a in "[a-z]{1,5}", b in "[a-z]{1,5}") {
            let p = PathBuf::from(format!("/scratch/w/{a}/../{b}"));
            prop_assert!(!path_within(&p, Path::new("/scratch/w")));
        }
    }
}
