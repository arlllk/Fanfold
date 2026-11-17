//! The JSON protocol between this crate and `worker/ffd_worker.py`.
//!
//! Pure functions only, so the whole parsing surface can be unit-tested and fuzzed
//! without spawning anything.

use std::path::{Component, Path, PathBuf};

use fanfold_app::ports::{DownloadError, DownloadedBook};
use fanfold_domain::failure::FailureKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct WorkerRequest<'a> {
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

/// Interpret a parsed response.
pub fn into_result(
    response: WorkerResponse,
    workdir: &Path,
) -> Result<DownloadedBook, DownloadError> {
    match response {
        WorkerResponse::Done {
            path,
            title,
            changed,
            log,
        } => {
            let path = PathBuf::from(path);
            if path_within(&path, workdir) {
                Ok(DownloadedBook {
                    path,
                    title,
                    changed,
                    log,
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
                log: "l".into()
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
        };
        let err = into_result(response, Path::new("/scratch/w1")).unwrap_err();
        assert_eq!(err.kind, FailureKind::Permanent);
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
