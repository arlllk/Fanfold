//! [`Downloader`] backed by FanFicFare, run as a short-lived Python process.
//!
//! One process per download gives crash isolation, a hard timeout, and no shared
//! interpreter state. The process boundary speaks JSON (see [`protocol`]); this crate
//! never parses FanFicFare's free-text output.

pub mod protocol;
pub mod runner;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use fanfold_app::ports::{
    Clock, DownloadError, DownloadRequest, DownloadedBook, Downloader, SessionStore,
};
use fanfold_domain::{
    failure::FailureKind,
    metadata::SeriesMetadata,
    session::{BrowserSession, parse_netscape, to_netscape},
    work::SeriesId,
};
use secrecy::{ExposeSecret, SecretString};

use crate::{
    protocol::{
        Clearance, WorkerRequest, WorkerResponse, into_listing, into_result, parse_response,
    },
    runner::{ProcessOutput, ProcessRunner, ProcessSpec, RunError},
};

/// The worker script, shipped inside the binary and fed to `python -c`.
const WORKER_SCRIPT: &str = include_str!("../worker/ffd_worker.py");

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// What to do when AO3 says a work needs a login, while downloads go through `FlareSolverr`.
///
/// Downloads are tried anonymously through `FlareSolverr` first (what
/// [`FanFicFareConfig::personal_ini`] sets up). The fallback then uses `FlareSolverr` only to pass
/// Cloudflare, and FanFicFare fetches over plain HTTP with that clearance, the stored login
/// cookie and, when there is no valid cookie, the account's credentials.
///
/// AO3 refuses a login submitted through `FlareSolverr`, which is why the login cannot simply
/// happen there.
#[derive(Debug, Clone)]
pub struct LoginFallback {
    /// FanFicFare settings for the fallback: plain requests, with the login if there is one.
    pub personal_ini: PathBuf,
    /// Origin of the `FlareSolverr` server.
    pub flaresolverr: url::Url,
    /// How long `FlareSolverr` may take to pass Cloudflare.
    pub flaresolverr_timeout: Duration,
}

pub struct FanFicFareConfig {
    /// Python interpreter that can `import fanficfare` (for example a venv's).
    pub python: String,
    /// Directory for per-download scratch space.
    pub scratch_dir: PathBuf,
    /// Rendered `personal.ini` (site logins). Written by the composition root.
    pub personal_ini: Option<PathBuf>,
    pub timeout: Duration,
    /// Story URL for a work id; `{id}` is substituted.
    pub url_template: String,
    /// Secret values scrubbed from everything this adapter reports.
    pub redactions: Vec<SecretString>,
    /// Set when downloads go through `FlareSolverr`; see [`LoginFallback`].
    pub login_fallback: Option<LoginFallback>,
}

impl std::fmt::Debug for FanFicFareConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FanFicFareConfig")
            .field("python", &self.python)
            .field("scratch_dir", &self.scratch_dir)
            .field("url_template", &self.url_template)
            .finish_non_exhaustive()
    }
}

/// Where the browser session lives and how to date changes to it.
struct SessionSupport {
    store: Arc<dyn SessionStore>,
    clock: Arc<dyn Clock>,
    /// The site the cookies belong to, taken from the story URL.
    site: String,
}

pub struct FanFicFareDownloader {
    runner: Arc<dyn ProcessRunner>,
    config: FanFicFareConfig,
    session: Option<SessionSupport>,
}

/// The session as handed to one worker run. `session` is `None` when nothing is stored yet
/// (the login fallback then starts from an empty cookie file and keeps what it earns).
struct PreparedSession {
    session: Option<BrowserSession>,
    cookies_file: PathBuf,
}

/// Which try of a download this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// The normal fetch. With a login fallback configured it goes through `FlareSolverr`,
    /// anonymously, so no cookies are sent along (they would mix with its own browser's);
    /// otherwise it is plain requests with the stored session.
    First,
    /// With a login fallback configured, every other try skips `FlareSolverr` altogether:
    /// plain requests, as `AutomatedFanfic` fetches, with the stored login cookie (none of
    /// Cloudflare's: those belong to `FlareSolverr`'s browser) and FanFicFare's own
    /// `User-Agent`, logging in with the account when AO3 asks. AO3's bot filter and
    /// Cloudflare judge the two routes apart, so when one is refused the next try may pass.
    Direct,
    /// The login fallback: Cloudflare passed by `FlareSolverr`, then plain requests.
    Fallback,
}

/// Cookies Cloudflare sets for the browser that passed its check. Sent from anywhere else
/// they do not help, and may mark the request as a replay.
fn is_cloudflare_cookie(name: &str) -> bool {
    name == "cf_clearance" || name.starts_with("__cf") || name == "_cfuvid"
}

/// Cookie values shorter than this are not scrubbed from logs: a value like `true`
/// would otherwise be blanked out of every sentence.
const MIN_REDACTED_COOKIE: usize = 16;

impl std::fmt::Debug for FanFicFareDownloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FanFicFareDownloader")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Replace every occurrence of every secret with `[REDACTED]`.
#[must_use]
pub fn redact(text: &str, secrets: &[SecretString]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        let value = secret.expose_secret();
        if !value.is_empty() {
            out = out.replace(value, "[REDACTED]");
        }
    }
    out
}

/// Remove scratch directories untouched for longer than `max_age` (leftovers from
/// crashes). Returns how many were removed. Call at startup.
pub fn sweep_stale(scratch_dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(scratch_dir) else {
        return 0;
    };
    let now = SystemTime::now();
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("work-"))
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= max_age)
        })
        .filter(|e| std::fs::remove_dir_all(e.path()).is_ok())
        .count()
}

impl FanFicFareDownloader {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>, config: FanFicFareConfig) -> Self {
        Self {
            runner,
            config,
            session: None,
        }
    }

    /// Present the stored browser session (cookies and `User-Agent`) on every download,
    /// and keep the cookies the site rotates.
    #[must_use]
    pub fn with_session(mut self, store: Arc<dyn SessionStore>, clock: Arc<dyn Clock>) -> Self {
        let site = url::Url::parse(&self.config.url_template.replace("{id}", "1"))
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        self.session = Some(SessionSupport { store, clock, site });
        self
    }

    /// Write the session's live cookies where the worker will read them. Trouble here
    /// never fails the download: it just runs without the session, as before.
    async fn prepare_session(&self, workdir: &Path, attempt: Attempt) -> Option<PreparedSession> {
        let support = self.session.as_ref()?;
        if attempt == Attempt::First && self.config.login_fallback.is_some() {
            return None;
        }
        let stored = match support.store.load().await {
            Ok(stored) => stored,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the browser session; downloading without it");
                if attempt == Attempt::First {
                    return None;
                }
                None
            }
        };
        let now = support.clock.now();
        let text = match &stored {
            Some(session) if attempt == Attempt::Direct => {
                let cookies: Vec<_> = session
                    .live_cookies(now)
                    .into_iter()
                    .filter(|c| !is_cloudflare_cookie(&c.name))
                    .collect();
                to_netscape(&cookies)
            }
            Some(session) => to_netscape(&session.live_cookies(now)),
            // Start from nothing, so a login FanFicFare does is kept for next time.
            None if attempt != Attempt::First => to_netscape(&[]),
            None => return None,
        };
        let cookies_file = workdir.join("cookies.txt");
        if let Err(e) = write_private(&cookies_file, &text).await {
            tracing::warn!(error = %e, "cannot stage the browser session; downloading without it");
            return None;
        }
        Some(PreparedSession {
            session: stored,
            cookies_file,
        })
    }

    /// Fold the cookies the run left behind into the stored session, unless someone
    /// imported a newer one meanwhile. With nothing stored yet (the first login the fallback
    /// ever did) they become the session, so the login cookie is there next time.
    async fn keep_rotated_cookies(&self, prepared: PreparedSession, workdir: &Path) {
        let Some(support) = &self.session else { return };
        let Ok(text) = tokio::fs::read_to_string(&prepared.cookies_file).await else {
            return;
        };
        let Ok(seen) = parse_netscape(&text, &support.site) else {
            return;
        };
        let now = support.clock.now();
        // The User-Agent the fallback's clearance was earned with, when it ran.
        let earned_with = tokio::fs::read_to_string(workdir.join("user_agent.txt"))
            .await
            .ok()
            .map(|ua| ua.trim().to_owned())
            .filter(|ua| fanfold_domain::session::valid_user_agent(ua));
        if let Some(mut session) = prepared.session {
            let based_on = session.updated_at;
            let absorbed = session.absorb(seen.cookies, now);
            let new_ua = earned_with.filter(|ua| *ua != session.user_agent);
            if !(absorbed || new_ua.is_some()) {
                return;
            }
            if let Some(ua) = new_ua {
                session.user_agent = ua;
            }
            session.updated_at = now;
            match support.store.refresh(&session, based_on).await {
                Ok(true) => tracing::info!("kept the cookies the site refreshed"),
                Ok(false) => {
                    tracing::info!("a newer browser session was imported; not overwriting it");
                }
                Err(e) => tracing::warn!(error = %e, "cannot save refreshed cookies"),
            }
            return;
        }
        let mut session = BrowserSession {
            user_agent: earned_with.unwrap_or_else(|| "fanfold".to_owned()),
            cookies: Vec::new(),
            updated_at: now,
        };
        session.absorb(seen.cookies, now);
        if session.cookies.is_empty() {
            return;
        }
        match support.store.load().await {
            Ok(None) => match support.store.save(&session).await {
                Ok(()) => tracing::info!("stored the login the fallback earned"),
                Err(e) => tracing::warn!(error = %e, "cannot save the new session"),
            },
            Ok(Some(_)) => {
                tracing::info!("a browser session was imported meanwhile; not overwriting it");
            }
            Err(e) => tracing::warn!(error = %e, "cannot save the new session"),
        }
    }

    /// How a job's next try fetches, after `tries` failed ones. With `FlareSolverr` configured
    /// the tries alternate: through it first (usually the better route), then directly, and so
    /// on, so that one route being refused never fails a job the other could fetch.
    const fn route(&self, tries: u32) -> Attempt {
        if self.config.login_fallback.is_some() && tries % 2 == 1 {
            Attempt::Direct
        } else {
            Attempt::First
        }
    }

    const fn route_name(&self, route: Attempt) -> &'static str {
        match (route, &self.config.login_fallback) {
            (Attempt::First, Some(_)) => "through FlareSolverr",
            (Attempt::Direct, _) => "directly, without FlareSolverr",
            (Attempt::Fallback, _) => "the login fallback",
            (Attempt::First, None) => "directly",
        }
    }

    /// The job-log line telling which route a try took.
    fn route_line(&self, route: Attempt, tries: u32) -> String {
        let other = match route {
            Attempt::Direct => "through FlareSolverr",
            _ => "directly",
        };
        format!(
            "Try {} fetched {}; the next try goes {other}.",
            u64::from(tries) + 1,
            self.route_name(route)
        )
    }

    /// Prefix the outcome's log with [`Self::route_line`] when routes alternate.
    fn note_route(
        &self,
        route: Attempt,
        tries: u32,
        result: Result<DownloadedBook, DownloadError>,
    ) -> Result<DownloadedBook, DownloadError> {
        if self.config.login_fallback.is_none() {
            return result;
        }
        let line = self.route_line(route, tries);
        match result {
            Ok(mut book) => {
                book.log = format!("{line}\n{}", book.log);
                Ok(book)
            }
            Err(mut failed) => {
                failed.log = format!("{line}\n{}", failed.log);
                Err(failed)
            }
        }
    }

    /// The page `FlareSolverr` opens to pass Cloudflare for the fallback: the site's login page.
    fn login_page(&self) -> String {
        url::Url::parse(&self.config.url_template.replace("{id}", "1"))
            .ok()
            .and_then(|u| u.join("/users/login").ok())
            .map_or_else(String::new, Into::into)
    }

    fn error(&self, kind: FailureKind, message: impl AsRef<str>, log: &str) -> DownloadError {
        DownloadError {
            kind,
            message: redact(message.as_ref(), &self.config.redactions),
            log: redact(log, &self.config.redactions),
        }
    }

    async fn make_workdir(&self, request: &DownloadRequest) -> Result<PathBuf, DownloadError> {
        self.make_dir(&format!("work-{}", request.work)).await
    }

    /// A fresh scratch directory named `<label>-<pid>-<n>`.
    async fn make_dir(&self, label: &str) -> Result<PathBuf, DownloadError> {
        let unique = format!(
            "{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.config.scratch_dir.join(unique);
        tokio::fs::create_dir_all(&dir).await.map_err(|e| {
            self.error(
                FailureKind::Transient,
                format!("cannot create scratch dir: {e}"),
                "",
            )
        })?;
        Ok(dir)
    }

    /// The child process: an isolated interpreter running the worker script, with a
    /// clean environment so none of the service's secrets leak into it.
    fn process_spec(&self, payload: Vec<u8>, workdir: &Path) -> ProcessSpec {
        let workdir_str = workdir.to_string_lossy().into_owned();
        ProcessSpec {
            program: self.config.python.clone(),
            // -I: isolated mode, ignores PYTHON* variables and the user site directory.
            args: vec!["-I".into(), "-c".into(), WORKER_SCRIPT.into()],
            stdin: payload,
            cwd: workdir.to_path_buf(),
            env: vec![
                ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
                ("HOME".into(), workdir_str.clone()),
                ("XDG_CONFIG_HOME".into(), workdir_str.clone()),
                // Nothing the worker writes may land outside its own scratch directory, so
                // the rest of the filesystem can be mounted read-only.
                ("TMPDIR".into(), workdir_str),
                ("LANG".into(), "C.UTF-8".into()),
                ("PYTHONIOENCODING".into(), "utf-8".into()),
                ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
            ],
            timeout: self.config.timeout,
        }
    }

    /// A series page's URL, from the work URL template (`.../works/{id}` -> `.../series/{id}`).
    fn series_url(&self, series: SeriesId) -> Option<String> {
        self.config.url_template.contains("/works/{id}").then(|| {
            self.config
                .url_template
                .replace("/works/{id}", "/series/{id}")
                .replace("{id}", &series.to_string())
        })
    }

    fn task_url(&self, task: Task<'_>) -> Result<String, DownloadError> {
        match task {
            Task::Download(request) => Ok(self
                .config
                .url_template
                .replace("{id}", &request.work.to_string())),
            Task::ListSeries(series) => self.series_url(series).ok_or_else(|| {
                self.error(
                    FailureKind::Permanent,
                    "series cannot be listed for this site",
                    "",
                )
            }),
        }
    }

    async fn run_worker(
        &self,
        request: &DownloadRequest,
        workdir: &Path,
        attempt: Attempt,
    ) -> Result<DownloadedBook, DownloadError> {
        let response = self
            .run_task(Task::Download(request), workdir, attempt)
            .await?;
        into_result(response, workdir, request.work)
    }

    async fn run_listing(
        &self,
        series: SeriesId,
        workdir: &Path,
        attempt: Attempt,
    ) -> Result<SeriesMetadata, DownloadError> {
        let response = self
            .run_task(Task::ListSeries(series), workdir, attempt)
            .await?;
        into_listing(response, series)
    }

    async fn run_task(
        &self,
        task: Task<'_>,
        workdir: &Path,
        attempt: Attempt,
    ) -> Result<WorkerResponse, DownloadError> {
        let existing = match task {
            Task::Download(request) => request.existing.as_ref(),
            Task::ListSeries(_) => None,
        };
        let existing_copy = match existing {
            Some(path) => {
                let copy = workdir.join("existing.epub");
                tokio::fs::copy(path, &copy).await.map_err(|e| {
                    self.error(
                        FailureKind::Transient,
                        format!("cannot stage existing book: {e}"),
                        "",
                    )
                })?;
                Some(copy)
            }
            None => None,
        };

        let url = self.task_url(task)?;
        let workdir_str = workdir.to_string_lossy();
        let existing_str = existing_copy
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let fallback = self.config.login_fallback.as_ref();
        let ini_path = match (attempt, fallback) {
            // Plain requests, logging in only when a page asks for it.
            (Attempt::Direct | Attempt::Fallback, Some(fallback)) => Some(&fallback.personal_ini),
            _ => self.config.personal_ini.as_ref(),
        };
        let ini_str = ini_path.map(|p| p.to_string_lossy().into_owned());
        let prepared = self.prepare_session(workdir, attempt).await;
        let cookies_str = prepared
            .as_ref()
            .map(|p| p.cookies_file.to_string_lossy().into_owned());
        let flaresolverr = fallback.map(|f| f.flaresolverr.as_str().to_owned());
        let login_page = self.login_page();
        let clearance = match (attempt, fallback, &flaresolverr) {
            (Attempt::Fallback, Some(fallback), Some(origin)) if prepared.is_some() => {
                Some(Clearance {
                    flaresolverr: origin,
                    timeout_ms: u64::try_from(fallback.flaresolverr_timeout.as_millis())
                        .unwrap_or(u64::MAX),
                    url: &login_page,
                })
            }
            _ => None,
        };
        // The fallback's User-Agent is the one FlareSolverr's browser earned its clearance with,
        // which the worker sets itself.
        let user_agent = match (attempt, &prepared) {
            (Attempt::First, Some(p)) => p.session.as_ref().map(|s| s.user_agent.as_str()),
            _ => None,
        };
        let payload = serde_json::to_vec(&WorkerRequest {
            op: task.op(),
            url: &url,
            existing: existing_str.as_deref(),
            workdir: &workdir_str,
            personal_ini: ini_str.as_deref(),
            cookies: cookies_str.as_deref(),
            user_agent,
            clearance,
        })
        .map_err(|e| {
            self.error(
                FailureKind::Permanent,
                format!("cannot encode request: {e}"),
                "",
            )
        })?;

        let spec = self.process_spec(payload, workdir);
        let ran = self.runner.run(&spec).await;
        // Redact what we handed the worker, so a cookie value echoed in its log (or a
        // traceback) never reaches the job log.
        let mut secrets = self.config.redactions.clone();
        if let Some(prepared) = prepared {
            if let Some(session) = &prepared.session {
                secrets.extend(
                    session
                        .cookies
                        .iter()
                        .filter(|c| c.value.len() >= MIN_REDACTED_COOKIE)
                        .map(|c| SecretString::from(c.value.clone())),
                );
            }
            self.keep_rotated_cookies(prepared, workdir).await;
        }
        let output = ran.map_err(|e| match e {
            // A missing interpreter is a deployment problem, not a flaky site.
            RunError::Spawn { .. } => self.error(FailureKind::Permanent, e.to_string(), ""),
            RunError::Timeout(_) | RunError::Io(_) => {
                self.error(FailureKind::Transient, e.to_string(), "")
            }
        })?;
        self.interpret_output(&output, &secrets)
    }

    /// Turn what the worker printed into an answer or a classified failure, scrubbing secrets.
    fn interpret_output(
        &self,
        output: &ProcessOutput,
        secrets: &[SecretString],
    ) -> Result<WorkerResponse, DownloadError> {
        let Some(response) = parse_response(&output.stdout) else {
            let tail: String = output
                .stderr
                .chars()
                .rev()
                .take(2000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(self.error(
                FailureKind::Transient,
                format!("worker produced no result (exit ok: {})", output.success),
                &tail,
            ));
        };
        match response {
            WorkerResponse::Failed { kind, message, log } => Err(DownloadError {
                kind,
                message: redact(&message, secrets),
                log: redact(&log, secrets),
            }),
            WorkerResponse::Done {
                path,
                title,
                changed,
                log,
                metadata,
            } => Ok(WorkerResponse::Done {
                path,
                title,
                changed,
                log: redact(&log, secrets),
                metadata,
            }),
            WorkerResponse::Listed {
                name,
                summary_html,
                complete,
                works,
                raw,
                log,
            } => Ok(WorkerResponse::Listed {
                name,
                summary_html,
                complete,
                works,
                raw,
                log: redact(&log, secrets),
            }),
        }
    }
}

/// What the worker is asked to do.
#[derive(Debug, Clone, Copy)]
enum Task<'a> {
    Download(&'a DownloadRequest),
    ListSeries(SeriesId),
}

impl Task<'_> {
    const fn op(self) -> &'static str {
        match self {
            Self::Download(_) => "download",
            Self::ListSeries(_) => "list_series",
        }
    }
}

/// Write `text` to a new file only its owner can read: it holds credentials.
async fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(text.as_bytes()).await?;
    file.flush().await
}

async fn remove_dir(dir: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(error = %e, dir = %dir.display(), "could not remove scratch dir");
    }
}

#[async_trait]
impl Downloader for FanFicFareDownloader {
    async fn download(&self, request: &DownloadRequest) -> Result<DownloadedBook, DownloadError> {
        let workdir = self.make_workdir(request).await?;
        let route = self.route(request.tries);
        let first = self.run_worker(request, &workdir, route).await;
        let result = match first {
            // AO3 says the work needs a login: try again the way that can log in.
            Err(refused)
                if refused.kind == FailureKind::AuthFailed
                    && self.config.login_fallback.is_some() =>
            {
                tracing::info!(work = %request.work, "AO3 asked for a login; trying the login fallback");
                match self.run_worker(request, &workdir, Attempt::Fallback).await {
                    Ok(mut book) => {
                        book.log = format!(
                            "AO3 asked for a login, so this was fetched through the login fallback.\n{}",
                            book.log
                        );
                        Ok(book)
                    }
                    Err(mut failed) => {
                        failed.log = format!(
                            "The first attempt ({}) failed: {}\n\n\
                             The login fallback then failed: {}\n{}",
                            self.route_name(route),
                            refused.message,
                            failed.message,
                            failed.log
                        );
                        Err(failed)
                    }
                }
            }
            other => other,
        };
        let result = self.note_route(route, request.tries, result);
        if result.is_err() {
            // No book will be handed out, so nobody would call `release`.
            remove_dir(&workdir).await;
        }
        result
    }

    async fn list_series(
        &self,
        series: SeriesId,
        tries: u32,
    ) -> Result<SeriesMetadata, DownloadError> {
        let workdir = self.make_dir(&format!("series-{series}")).await?;
        let route = self.route(tries);
        let result = match self.run_listing(series, &workdir, route).await {
            // A series of restricted works may need a login, like a work.
            Err(refused)
                if refused.kind == FailureKind::AuthFailed
                    && self.config.login_fallback.is_some() =>
            {
                tracing::info!(%series, "AO3 asked for a login; trying the login fallback");
                self.run_listing(series, &workdir, Attempt::Fallback)
                    .await
                    .map_err(|mut failed| {
                        failed.log = format!(
                            "The first attempt ({}) failed: {}\n\nThe login fallback then failed: {}\n{}",
                            self.route_name(route),
                            refused.message,
                            failed.message,
                            failed.log
                        );
                        failed
                    })
            }
            other => other,
        };
        let result = match self.config.login_fallback {
            Some(_) => result.map_err(|mut failed| {
                failed.log = format!("{}\n{}", self.route_line(route, tries), failed.log);
                failed
            }),
            None => result,
        };
        // A listing hands nothing out: its scratch space goes now.
        remove_dir(&workdir).await;
        result
    }

    async fn release(&self, book: &DownloadedBook) {
        // Only directories this adapter created are ever removed.
        if let Some(dir) = book.path.parent()
            && dir.starts_with(&self.config.scratch_dir)
            && dir
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("work-"))
        {
            remove_dir(dir).await;
        }
    }
}

// Tests define small one-off runner types next to the code that uses them.
#[cfg(test)]
#[allow(clippy::items_after_statements)]
mod tests {
    use std::sync::Mutex;

    use fanfold_domain::work::WorkId;

    use super::*;
    use crate::runner::{ProcessOutput, TokioProcessRunner};

    /// Replays canned outputs and records what it was asked to run.
    struct ScriptedRunner {
        outputs: Mutex<Vec<Result<ProcessOutput, RunError>>>,
        specs: Mutex<Vec<ProcessSpec>>,
    }

    impl ScriptedRunner {
        fn new(outputs: Vec<Result<ProcessOutput, RunError>>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(outputs),
                specs: Mutex::new(Vec::new()),
            })
        }

        fn specs(&self) -> Vec<ProcessSpec> {
            self.specs.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProcessRunner for ScriptedRunner {
        async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutput, RunError> {
            self.specs.lock().unwrap().push(spec.clone());
            self.outputs.lock().unwrap().remove(0)
        }
    }

    /// Always `Ok`: shaped like `ProcessRunner::run` so tests can return it directly.
    #[allow(clippy::unnecessary_wraps)]
    fn out(stdout: &str) -> Result<ProcessOutput, RunError> {
        Ok(ProcessOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            success: true,
        })
    }

    fn downloader(runner: Arc<ScriptedRunner>, scratch: &Path) -> FanFicFareDownloader {
        FanFicFareDownloader::new(
            runner,
            FanFicFareConfig {
                python: "python3".into(),
                scratch_dir: scratch.to_path_buf(),
                personal_ini: Some(PathBuf::from("/run/secrets/personal.ini")),
                timeout: Duration::from_secs(30),
                url_template: "https://archiveofourown.org/works/{id}".into(),
                redactions: vec![SecretString::from("hunter2")],
                login_fallback: None,
            },
        )
    }

    fn request(existing: Option<PathBuf>) -> DownloadRequest {
        DownloadRequest {
            work: WorkId::new(42).unwrap(),
            existing,
            tries: 0,
        }
    }

    #[tokio::test]
    async fn success_is_mapped_and_the_workdir_is_kept_for_publishing() {
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new(vec![]);
        let dl = downloader(Arc::clone(&runner), scratch.path());
        // The script answers with a path inside whatever workdir was chosen, so
        // compute the answer from the request the runner receives.
        struct Echo(Arc<ScriptedRunner>);
        #[async_trait]
        impl ProcessRunner for Echo {
            async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutput, RunError> {
                let _ = &self.0;
                let req: serde_json::Value = serde_json::from_slice(&spec.stdin).unwrap();
                let dir = req["workdir"].as_str().unwrap();
                std::fs::write(format!("{dir}/story.epub"), "epub").unwrap();
                out(&format!(
                    r#"noise
{{"status":"ok","path":"{dir}/story.epub","title":"A Title","changed":true,"log":"password hunter2 leaked"}}"#
                ))
            }
        }
        let dl = FanFicFareDownloader::new(Arc::new(Echo(runner)), dl.config);
        let book = dl.download(&request(None)).await.unwrap();
        assert_eq!(book.title, "A Title");
        assert!(book.changed);
        assert!(
            book.path.exists(),
            "the file must survive until it is published"
        );
        assert!(!book.log.contains("hunter2"), "{}", book.log);
        assert!(book.log.contains("[REDACTED]"));
        dl.release(&book).await;
        assert!(!book.path.exists(), "release removes the scratch space");
    }

    #[tokio::test]
    async fn the_request_carries_the_url_and_a_clean_environment() {
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new(vec![out(
            r#"{"status":"error","kind":"not_found","message":"gone"}"#,
        )]);
        let dl = downloader(Arc::clone(&runner), scratch.path());
        let err = dl.download(&request(None)).await.unwrap_err();
        assert_eq!(
            (err.kind, err.message.as_str()),
            (FailureKind::NotFound, "gone")
        );

        let spec = &runner.specs()[0];
        let req: serde_json::Value = serde_json::from_slice(&spec.stdin).unwrap();
        assert_eq!(req["url"], "https://archiveofourown.org/works/42");
        assert_eq!(req["personal_ini"], "/run/secrets/personal.ini");
        assert_eq!(req["existing"], serde_json::Value::Null);
        assert_eq!(&spec.args[..2], ["-I", "-c"], "isolated interpreter");
        assert!(
            spec.env
                .iter()
                .all(|(k, _)| !k.contains("PASSWORD") && !k.contains("SECRET"))
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "HOME" && Some(v.as_str()) == req["workdir"].as_str())
        );
        assert_eq!(
            std::fs::read_dir(scratch.path()).unwrap().count(),
            0,
            "failed downloads clean up after themselves"
        );
    }

    // ------------------------------------------------------------ browser session

    use fanfold_domain::{
        Timestamp,
        session::{BrowserSession, Cookie},
    };
    use fanfold_testkit::memory::{FakeClock, InMemorySession};

    const NOW: i64 = 1_800_000_000;
    const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:160.0) Gecko/20100101 Firefox/160.0";
    const CF_VALUE: &str = "cf-clearance-value-1234567890";

    fn cookie(name: &str, value: &str, expires: Option<i64>) -> Cookie {
        Cookie {
            name: name.into(),
            value: value.into(),
            domain: ".archiveofourown.org".into(),
            path: "/".into(),
            expires: expires.map(|s| Timestamp::from_second(s).unwrap()),
            secure: true,
            http_only: false,
        }
    }

    fn stored_session(updated: i64) -> BrowserSession {
        BrowserSession {
            user_agent: UA.into(),
            cookies: vec![
                cookie("cf_clearance", CF_VALUE, Some(NOW + 86_400)),
                cookie("view_adult", "true", None),
                cookie("expired", "old", Some(NOW - 10)),
            ],
            updated_at: Timestamp::from_second(updated).unwrap(),
        }
    }

    /// A runner that plays the worker: records the cookie file it was given, optionally
    /// lets a hook change the world mid-run, rewrites the file like a rotating site, and
    /// fails with `message`.
    struct CookieRunner {
        seen: Mutex<Option<(String, u32)>>,
        request: Mutex<Option<serde_json::Value>>,
        rewrite: Option<String>,
        during: Option<Box<dyn Fn() + Send + Sync>>,
        message: String,
    }

    impl CookieRunner {
        fn new(rewrite: Option<&str>, message: &str) -> Self {
            Self {
                seen: Mutex::new(None),
                request: Mutex::new(None),
                rewrite: rewrite.map(str::to_owned),
                during: None,
                message: message.into(),
            }
        }
    }

    #[async_trait]
    impl ProcessRunner for CookieRunner {
        async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutput, RunError> {
            use std::os::unix::fs::PermissionsExt as _;
            let req: serde_json::Value = serde_json::from_slice(&spec.stdin).unwrap();
            *self.request.lock().unwrap() = Some(req.clone());
            if let Some(path) = req["cookies"].as_str() {
                let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
                *self.seen.lock().unwrap() = Some((std::fs::read_to_string(path).unwrap(), mode));
                if let Some(text) = &self.rewrite {
                    std::fs::write(path, text).unwrap();
                }
            }
            if let Some(hook) = &self.during {
                hook();
            }
            out(&format!(
                r#"{{"status":"error","kind":"blocked","message":{},"log":{}}}"#,
                serde_json::to_string(&self.message).unwrap(),
                serde_json::to_string(&self.message).unwrap()
            ))
        }
    }

    fn with_session(
        runner: Arc<CookieRunner>,
        scratch: &Path,
        store: &Arc<InMemorySession>,
    ) -> FanFicFareDownloader {
        let clock = Arc::new(FakeClock::at_secs(NOW));
        FanFicFareDownloader::new(
            runner,
            FanFicFareConfig {
                python: "python3".into(),
                scratch_dir: scratch.to_path_buf(),
                personal_ini: None,
                timeout: Duration::from_secs(30),
                url_template: "https://archiveofourown.org/works/{id}".into(),
                redactions: vec![],
                login_fallback: None,
            },
        )
        .with_session(Arc::clone(store) as Arc<dyn SessionStore>, clock)
    }

    #[tokio::test]
    async fn the_stored_session_reaches_the_worker_privately_and_without_expired_cookies() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        store.save(&stored_session(NOW - 100)).await.unwrap();
        let runner = Arc::new(CookieRunner::new(None, "refused"));
        let dl = with_session(Arc::clone(&runner), scratch.path(), &store);
        let _ = dl.download(&request(None)).await;

        let req = runner.request.lock().unwrap().clone().unwrap();
        assert_eq!(req["user_agent"], UA);
        let (file, mode) = runner.seen.lock().unwrap().clone().unwrap();
        assert_eq!(mode, 0o600, "the cookie file is private to us");
        assert!(
            file.contains("cf_clearance") && file.contains(CF_VALUE),
            "{file}"
        );
        assert!(file.contains("view_adult"));
        assert!(!file.contains("expired"), "an expired cookie is not sent");
    }

    // ------------------------------------------------------------------ login fallback

    const FIRST_INI: &str = "/run/fanfold/personal.ini";
    const LOGIN_INI: &str = "/run/fanfold/personal-login.ini";

    /// Plays the worker for the two tries of a download and records what it was asked.
    struct TwoTryRunner {
        requests: Mutex<Vec<serde_json::Value>>,
        /// How the anonymous try ends (a `FailureKind` name).
        first_fails_with: &'static str,
        /// `None`: the fallback also asks for a login. `Some(cookies)`: it works, leaving this
        /// cookies.txt behind, and `ua` as the User-Agent its clearance was earned with.
        fallback_succeeds_with: Option<(String, String)>,
    }

    #[async_trait]
    impl ProcessRunner for TwoTryRunner {
        async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutput, RunError> {
            let req: serde_json::Value = serde_json::from_slice(&spec.stdin).unwrap();
            let call = {
                let mut seen = self.requests.lock().unwrap();
                seen.push(req.clone());
                seen.len()
            };
            let workdir = req["workdir"].as_str().unwrap().to_owned();
            if call == 1 || self.fallback_succeeds_with.is_none() {
                let kind = if call == 1 {
                    self.first_fails_with
                } else {
                    "auth_failed"
                };
                return out(&format!(
                    r#"{{"status":"error","kind":"{kind}","message":"try {call} said {kind}","log":"log of try {call}"}}"#
                ));
            }
            let (cookies, ua) = self.fallback_succeeds_with.clone().unwrap();
            std::fs::write(req["cookies"].as_str().unwrap(), cookies).unwrap();
            std::fs::write(format!("{workdir}/user_agent.txt"), ua).unwrap();
            std::fs::write(format!("{workdir}/story.epub"), "epub").unwrap();
            out(&format!(
                r#"{{"status":"ok","path":"{workdir}/story.epub","title":"T","changed":true,"log":"worker log"}}"#
            ))
        }
    }

    fn fallback_downloader(
        runner: Arc<TwoTryRunner>,
        scratch: &Path,
        store: &Arc<InMemorySession>,
    ) -> FanFicFareDownloader {
        FanFicFareDownloader::new(
            runner,
            FanFicFareConfig {
                python: "python3".into(),
                scratch_dir: scratch.to_path_buf(),
                personal_ini: Some(PathBuf::from(FIRST_INI)),
                timeout: Duration::from_secs(30),
                url_template: "https://archiveofourown.org/works/{id}".into(),
                redactions: vec![],
                login_fallback: Some(LoginFallback {
                    personal_ini: PathBuf::from(LOGIN_INI),
                    flaresolverr: "http://flaresolverr:8191".parse().unwrap(),
                    flaresolverr_timeout: Duration::from_secs(45),
                }),
            },
        )
        .with_session(
            Arc::clone(store) as Arc<dyn SessionStore>,
            Arc::new(FakeClock::at_secs(NOW)),
        )
    }

    fn two_try_runner(
        first: &'static str,
        fallback: Option<(String, String)>,
    ) -> Arc<TwoTryRunner> {
        Arc::new(TwoTryRunner {
            requests: Mutex::new(Vec::new()),
            first_fails_with: first,
            fallback_succeeds_with: fallback,
        })
    }

    #[tokio::test]
    async fn a_login_demand_through_flaresolverr_falls_back_to_the_login_path() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        store.save(&stored_session(NOW - 100)).await.unwrap();
        let runner = two_try_runner(
            "auth_failed",
            Some(("# Netscape HTTP Cookie File\n".into(), "EarnedUA/1".into())),
        );
        let dl = fallback_downloader(Arc::clone(&runner), scratch.path(), &store);
        let book = dl.download(&request(None)).await.unwrap();
        assert!(book.log.contains("login fallback"), "{}", book.log);
        assert!(book.log.contains("worker log"));

        let seen = runner.requests.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        let (first, second) = (&seen[0], &seen[1]);
        assert_eq!(first["personal_ini"], FIRST_INI);
        assert_eq!(
            (&first["cookies"], &first["user_agent"], &first["clearance"]),
            (
                &serde_json::Value::Null,
                &serde_json::Value::Null,
                &serde_json::Value::Null
            ),
            "the anonymous try sends nothing of the stored session through FlareSolverr"
        );
        assert_eq!(second["personal_ini"], LOGIN_INI);
        assert!(
            second["cookies"].is_string(),
            "the stored session goes to the fallback"
        );
        assert_eq!(
            second["clearance"]["flaresolverr"],
            "http://flaresolverr:8191/"
        );
        assert_eq!(second["clearance"]["timeout_ms"], 45_000);
        assert_eq!(
            second["clearance"]["url"],
            "https://archiveofourown.org/users/login"
        );
        assert_eq!(
            second["user_agent"],
            serde_json::Value::Null,
            "the worker sets it"
        );
    }

    #[tokio::test]
    async fn only_a_login_demand_triggers_the_fallback() {
        for kind in [
            "site_unavailable",
            "rate_limited",
            "blocked",
            "not_found",
            "transient",
            "permanent",
        ] {
            let scratch = tempfile::tempdir().unwrap();
            let store = Arc::new(InMemorySession::new());
            let runner = two_try_runner(kind, Some((String::new(), "ua".into())));
            let dl = fallback_downloader(Arc::clone(&runner), scratch.path(), &store);
            let err = dl.download(&request(None)).await.unwrap_err();
            assert_eq!(err.kind.as_str(), kind);
            assert_eq!(
                runner.requests.lock().unwrap().len(),
                1,
                "{kind} must not retry"
            );
        }
    }

    #[tokio::test]
    async fn without_a_fallback_a_login_demand_is_final() {
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new(vec![out(
            r#"{"status":"error","kind":"auth_failed","message":"needs login"}"#,
        )]);
        let dl = downloader(Arc::clone(&runner), scratch.path());
        let err = dl.download(&request(None)).await.unwrap_err();
        assert_eq!(err.kind, FailureKind::AuthFailed);
        assert_eq!(runner.specs().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_fallback_reports_both_tries() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        let runner = two_try_runner("auth_failed", None);
        let dl = fallback_downloader(Arc::clone(&runner), scratch.path(), &store);
        let err = dl.download(&request(None)).await.unwrap_err();
        assert_eq!(
            err.kind,
            FailureKind::AuthFailed,
            "still a login problem: not retried"
        );
        assert!(err.log.contains("try 1 said auth_failed"), "{}", err.log);
        assert!(
            err.log.contains("login fallback then failed"),
            "{}",
            err.log
        );
        assert_eq!(runner.requests.lock().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_dir(scratch.path()).unwrap().count(),
            0,
            "a failed download cleans up"
        );
    }

    /// [`fallback_downloader`]'s configuration, around any runner.
    fn alternating_downloader(
        runner: Arc<dyn ProcessRunner>,
        scratch: &Path,
        store: &Arc<InMemorySession>,
    ) -> FanFicFareDownloader {
        FanFicFareDownloader::new(
            runner,
            FanFicFareConfig {
                python: "python3".into(),
                scratch_dir: scratch.to_path_buf(),
                personal_ini: Some(PathBuf::from(FIRST_INI)),
                timeout: Duration::from_secs(30),
                url_template: "https://archiveofourown.org/works/{id}".into(),
                redactions: vec![],
                login_fallback: Some(LoginFallback {
                    personal_ini: PathBuf::from(LOGIN_INI),
                    flaresolverr: "http://flaresolverr:8191".parse().unwrap(),
                    flaresolverr_timeout: Duration::from_secs(45),
                }),
            },
        )
        .with_session(
            Arc::clone(store) as Arc<dyn SessionStore>,
            Arc::new(FakeClock::at_secs(NOW)),
        )
    }

    /// One route refused (AO3's bot filter flags `FlareSolverr`, Cloudflare flags plain
    /// requests) must not fail a job the other could fetch: the tries take turns.
    #[tokio::test]
    async fn tries_alternate_between_flaresolverr_and_fetching_directly() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        let mut session = stored_session(NOW - 100);
        session.cookies.push(cookie(
            "remember_user_token",
            "login-token-0123456789",
            Some(NOW + 30 * 86_400),
        ));
        session.cookies.push(cookie(
            "__cf_bm",
            "bot-management-0123456789",
            Some(NOW + 600),
        ));
        store.save(&session).await.unwrap();

        for tries in 0..4 {
            let runner = Arc::new(CookieRunner::new(None, "refused"));
            let dl = alternating_downloader(
                Arc::clone(&runner) as Arc<dyn ProcessRunner>,
                scratch.path(),
                &store,
            );
            let err = dl
                .download(&DownloadRequest {
                    tries,
                    ..request(None)
                })
                .await
                .unwrap_err();
            let req = runner.request.lock().unwrap().clone().unwrap();
            assert_eq!(req["clearance"], serde_json::Value::Null, "try {tries}");
            assert_eq!(req["user_agent"], serde_json::Value::Null, "try {tries}");
            if tries % 2 == 0 {
                assert_eq!(req["personal_ini"], FIRST_INI, "try {tries}");
                assert_eq!(req["cookies"], serde_json::Value::Null);
                assert!(
                    err.log
                        .contains("through FlareSolverr; the next try goes directly")
                );
            } else {
                assert_eq!(req["personal_ini"], LOGIN_INI, "try {tries}: can log in");
                let (file, _) = runner.seen.lock().unwrap().clone().unwrap();
                assert!(file.contains("login-token-0123456789"), "{file}");
                assert!(file.contains("view_adult"), "{file}");
                assert!(
                    !file.contains("cf_clearance") && !file.contains("__cf_bm"),
                    "Cloudflare's cookies belong to FlareSolverr's browser: {file}"
                );
                assert!(
                    err.log
                        .contains("without FlareSolverr; the next try goes through"),
                    "{}",
                    err.log
                );
            }
            assert!(
                err.log.contains(&format!("Try {} fetched", tries + 1)),
                "{}",
                err.log
            );
        }
        let kept = store.load().await.unwrap().unwrap();
        assert!(
            kept.cookies.iter().any(|c| c.name == "cf_clearance"),
            "a direct try leaves the stored clearance alone"
        );
    }

    #[tokio::test]
    async fn without_flaresolverr_every_try_fetches_the_same_way() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        store.save(&stored_session(NOW - 100)).await.unwrap();
        for tries in 0..2 {
            let runner = Arc::new(CookieRunner::new(None, "refused"));
            let dl = with_session(Arc::clone(&runner), scratch.path(), &store);
            let err = dl
                .download(&DownloadRequest {
                    tries,
                    ..request(None)
                })
                .await
                .unwrap_err();
            let req = runner.request.lock().unwrap().clone().unwrap();
            assert_eq!(req["user_agent"], UA, "try {tries}");
            assert!(!err.log.contains("next try"), "{}", err.log);
        }
    }

    #[tokio::test]
    async fn a_login_demand_on_a_direct_try_also_gets_the_login_fallback() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        let runner = two_try_runner("auth_failed", Some((String::new(), "EarnedUA/1".into())));
        let dl = fallback_downloader(Arc::clone(&runner), scratch.path(), &store);
        let book = dl
            .download(&DownloadRequest {
                tries: 1,
                ..request(None)
            })
            .await
            .unwrap();
        assert!(book.log.contains("login fallback"), "{}", book.log);
        let seen = runner.requests.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert!(seen[0]["clearance"].is_null() && seen[1]["clearance"].is_object());
    }

    #[tokio::test]
    async fn the_login_the_fallback_earns_is_kept_when_nothing_was_stored() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        let cookies = format!(
            "# Netscape HTTP Cookie File\n\
             .archiveofourown.org\tTRUE\t/\tTRUE\t{}\tremember_user_token\tlogin-token-0123456789\n\
             .archiveofourown.org\tTRUE\t/\tTRUE\t{}\tcf_clearance\tclearance-0123456789abcdef\n",
            NOW + 92 * 86_400,
            NOW + 365 * 86_400
        );
        let runner = two_try_runner("auth_failed", Some((cookies, "FlareSolverrUA/9".into())));
        let dl = fallback_downloader(Arc::clone(&runner), scratch.path(), &store);
        dl.download(&request(None)).await.unwrap();

        let kept = store
            .load()
            .await
            .unwrap()
            .expect("the earned login is stored");
        assert_eq!(kept.user_agent, "FlareSolverrUA/9");
        let remember = kept
            .cookies
            .iter()
            .find(|c| c.name == "remember_user_token")
            .unwrap();
        assert_eq!(
            remember.expires,
            Some(Timestamp::from_second(NOW + 92 * 86_400).unwrap()),
            "its expiry is known, so the Session page can warn before it lapses"
        );
        assert!(kept.cookies.iter().any(|c| c.name == "cf_clearance"));
    }

    #[tokio::test]
    async fn the_fallback_starts_from_the_stored_login_and_updates_it() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        let mut session = stored_session(NOW - 100);
        session.cookies.push(cookie(
            "remember_user_token",
            "old-login-token-0123456789",
            Some(NOW + 30 * 86_400),
        ));
        store.save(&session).await.unwrap();
        let renewed = format!(
            ".archiveofourown.org\tTRUE\t/\tTRUE\t{}\tremember_user_token\tnew-login-token-0123456789\n",
            NOW + 92 * 86_400
        );
        let runner = two_try_runner("auth_failed", Some((renewed, "FlareSolverrUA/9".into())));
        let dl = fallback_downloader(Arc::clone(&runner), scratch.path(), &store);
        dl.download(&request(None)).await.unwrap();

        let after = store.load().await.unwrap().unwrap();
        let remember = after
            .cookies
            .iter()
            .find(|c| c.name == "remember_user_token")
            .unwrap();
        assert_eq!(
            remember.value, "new-login-token-0123456789",
            "the renewed token replaces the old"
        );
        assert!(
            after.cookies.iter().any(|c| c.name == "view_adult"),
            "cookies the run did not mention are kept"
        );
        assert_eq!(after.user_agent, "FlareSolverrUA/9");
    }

    #[tokio::test]
    async fn without_a_session_nothing_changes() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        let runner = Arc::new(CookieRunner::new(None, "refused"));
        let dl = with_session(Arc::clone(&runner), scratch.path(), &store);
        let _ = dl.download(&request(None)).await;
        let req = runner.request.lock().unwrap().clone().unwrap();
        assert_eq!(req["cookies"], serde_json::Value::Null);
        assert_eq!(req["user_agent"], serde_json::Value::Null);
        assert_eq!(store.load().await.unwrap(), None);
    }

    #[tokio::test]
    async fn cookies_the_site_rotated_are_kept_even_when_the_download_failed() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        store.save(&stored_session(NOW - 100)).await.unwrap();
        let rotated = format!(
            "# Netscape HTTP Cookie File\n\
             .archiveofourown.org\tTRUE\t/\tTRUE\t{}\tcf_clearance\trotated-value\n\
             .archiveofourown.org\tTRUE\t/\tTRUE\t{}\t__cf_bm\tnew-cookie\n",
            NOW + 2 * 86_400,
            NOW + 1_800
        );
        let runner = Arc::new(CookieRunner::new(Some(&rotated), "refused"));
        let dl = with_session(runner, scratch.path(), &store);
        let _ = dl.download(&request(None)).await;

        let session = store.load().await.unwrap().unwrap();
        let value = |n: &str| {
            session
                .cookies
                .iter()
                .find(|c| c.name == n)
                .map(|c| c.value.clone())
        };
        assert_eq!(value("cf_clearance").as_deref(), Some("rotated-value"));
        assert_eq!(value("__cf_bm").as_deref(), Some("new-cookie"));
        assert_eq!(
            value("view_adult").as_deref(),
            Some("true"),
            "unmentioned cookies stay"
        );
        assert_eq!(value("expired"), None, "expired cookies are dropped");
        assert_eq!(session.user_agent, UA);
        assert_eq!(session.updated_at, Timestamp::from_second(NOW).unwrap());
    }

    #[tokio::test]
    async fn an_import_made_during_a_run_is_not_overwritten() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        store.save(&stored_session(NOW - 100)).await.unwrap();
        let rotated = format!(
            ".archiveofourown.org\tTRUE\t/\tTRUE\t{}\tcf_clearance\tfrom-the-run\n",
            NOW + 86_400
        );
        let mut runner = CookieRunner::new(Some(&rotated), "refused");
        let imported = BrowserSession {
            user_agent: "imported".into(),
            cookies: vec![cookie(
                "cf_clearance",
                "fresh-import",
                Some(NOW + 9_999_999),
            )],
            updated_at: Timestamp::from_second(NOW - 1).unwrap(),
        };
        let store_for_hook = Arc::clone(&store);
        let handle = tokio::runtime::Handle::current();
        runner.during = Some(Box::new(move || {
            let (store, session) = (Arc::clone(&store_for_hook), imported.clone());
            std::thread::spawn({
                let handle = handle.clone();
                move || handle.block_on(async move { store.save(&session).await.unwrap() })
            })
            .join()
            .unwrap();
        }));
        let dl = with_session(Arc::new(runner), scratch.path(), &store);
        let _ = dl.download(&request(None)).await;

        let session = store.load().await.unwrap().unwrap();
        assert_eq!(session.user_agent, "imported");
        assert_eq!(session.cookies[0].value, "fresh-import");
    }

    #[tokio::test]
    async fn cookie_values_never_reach_the_job_log() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        store.save(&stored_session(NOW - 100)).await.unwrap();
        let echo = format!("403 with Cookie: cf_clearance={CF_VALUE}; view_adult=true");
        let runner = Arc::new(CookieRunner::new(None, &echo));
        let dl = with_session(runner, scratch.path(), &store);
        let err = dl.download(&request(None)).await.unwrap_err();
        for text in [&err.message, &err.log] {
            assert!(!text.contains(CF_VALUE), "{text}");
            assert!(text.contains("[REDACTED]"));
            assert!(
                text.contains("view_adult=true"),
                "short values are not blanked: {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_broken_session_store_does_not_stop_downloads() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemorySession::new());
        store.break_with(Some(fanfold_app::error::RepoError::Backend("down".into())));
        let runner = Arc::new(CookieRunner::new(None, "refused"));
        let dl = with_session(Arc::clone(&runner), scratch.path(), &store);
        let err = dl.download(&request(None)).await.unwrap_err();
        assert_eq!(err.kind, FailureKind::Blocked, "the worker still ran");
        assert!(runner.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn an_existing_book_is_staged_as_a_copy_never_edited_in_place() {
        let scratch = tempfile::tempdir().unwrap();
        let library = tempfile::tempdir().unwrap();
        let book = library.path().join("A [42].epub");
        std::fs::write(&book, "original").unwrap();
        let runner = ScriptedRunner::new(vec![out(
            r#"{"status":"error","kind":"transient","message":"x"}"#,
        )]);
        let dl = downloader(Arc::clone(&runner), scratch.path());
        let _ = dl.download(&request(Some(book.clone()))).await;

        let req: serde_json::Value = serde_json::from_slice(&runner.specs()[0].stdin).unwrap();
        let staged = req["existing"].as_str().unwrap();
        assert_ne!(Path::new(staged), book.as_path());
        assert!(staged.starts_with(req["workdir"].as_str().unwrap()));
        assert_eq!(std::fs::read_to_string(&book).unwrap(), "original");
    }

    #[tokio::test]
    async fn garbage_output_is_a_transient_failure_with_stderr_for_diagnosis() {
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new(vec![Ok(ProcessOutput {
            stdout: "Traceback...".into(),
            stderr: "ImportError: no module named fanficfare (password hunter2)".into(),
            success: false,
        })]);
        let err = downloader(runner, scratch.path())
            .download(&request(None))
            .await
            .unwrap_err();
        assert_eq!(err.kind, FailureKind::Transient);
        assert!(
            err.log.contains("ImportError") && !err.log.contains("hunter2"),
            "{}",
            err.log
        );
    }

    #[tokio::test]
    async fn a_missing_interpreter_is_permanent_and_a_timeout_is_transient() {
        let scratch = tempfile::tempdir().unwrap();
        let runner = ScriptedRunner::new(vec![
            Err(RunError::Spawn {
                program: "python3".into(),
                reason: "not found".into(),
            }),
            Err(RunError::Timeout(Duration::from_secs(30))),
        ]);
        let dl = downloader(runner, scratch.path());
        assert_eq!(
            dl.download(&request(None)).await.unwrap_err().kind,
            FailureKind::Permanent
        );
        assert_eq!(
            dl.download(&request(None)).await.unwrap_err().kind,
            FailureKind::Transient
        );
    }

    #[tokio::test]
    async fn release_removes_the_workdir_of_an_unchanged_update() {
        let scratch = tempfile::tempdir().unwrap();
        let library = tempfile::tempdir().unwrap();
        let book = library.path().join("A [42].epub");
        std::fs::write(&book, "x").unwrap();
        struct Unchanged;
        #[async_trait]
        impl ProcessRunner for Unchanged {
            async fn run(&self, spec: &ProcessSpec) -> Result<ProcessOutput, RunError> {
                let req: serde_json::Value = serde_json::from_slice(&spec.stdin).unwrap();
                let existing = req["existing"].as_str().unwrap();
                out(&format!(
                    r#"{{"status":"ok","path":"{existing}","title":"T","changed":false}}"#
                ))
            }
        }
        let dl = FanFicFareDownloader::new(
            Arc::new(Unchanged),
            downloader(ScriptedRunner::new(vec![]), scratch.path()).config,
        );
        let result = dl.download(&request(Some(book))).await.unwrap();
        assert!(!result.changed);
        assert_eq!(
            std::fs::read_dir(scratch.path()).unwrap().count(),
            1,
            "kept until released"
        );
        dl.release(&result).await;
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn release_only_ever_removes_directories_this_adapter_created() {
        let scratch = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ours = scratch.path().join("work-1-2-3");
        let foreign = scratch.path().join("somebody-elses");
        std::fs::create_dir(&ours).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        let dl = downloader(ScriptedRunner::new(vec![]), scratch.path());
        let book = |path: PathBuf| DownloadedBook {
            path,
            title: "T".into(),
            changed: true,
            log: String::new(),
            metadata: None,
        };

        dl.release(&book(foreign.join("story.epub"))).await;
        dl.release(&book(outside.path().join("story.epub"))).await;
        assert!(
            foreign.exists() && outside.path().exists(),
            "not ours: must survive"
        );

        dl.release(&book(ours.join("story.epub"))).await;
        assert!(!ours.exists());
        dl.release(&book(ours.join("story.epub"))).await; // idempotent
    }

    #[test]
    fn redaction() {
        let secrets = [SecretString::from("pw"), SecretString::from("")];
        assert_eq!(redact("a pw b pw", &secrets), "a [REDACTED] b [REDACTED]");
        assert_eq!(
            redact("nothing", &secrets),
            "nothing",
            "an empty secret must not match everywhere"
        );
    }

    #[test]
    fn sweeping_removes_only_our_stale_directories() {
        let scratch = tempfile::tempdir().unwrap();
        std::fs::create_dir(scratch.path().join("work-1-2-3")).unwrap();
        std::fs::create_dir(scratch.path().join("unrelated")).unwrap();
        assert_eq!(
            sweep_stale(scratch.path(), Duration::from_secs(3600)),
            0,
            "fresh dirs are kept"
        );
        assert_eq!(sweep_stale(scratch.path(), Duration::ZERO), 1);
        assert!(scratch.path().join("unrelated").exists());
        assert_eq!(
            sweep_stale(Path::new("/nonexistent/dir"), Duration::ZERO),
            0
        );
    }

    /// A stand-in for `FlareSolverr`: answers every command with a fresh Cloudflare clearance and
    /// session cookie, and records the request bodies.
    async fn fake_flaresolverr() -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
        fake_flaresolverr_serving(
            "<html><head><title>fake</title></head><body><div id=\"main\"></div></body></html>",
        )
        .await
    }

    /// [`fake_flaresolverr`], answering every page with `page` and HTTP status 200, as the real
    /// one does even for Cloudflare's own error pages.
    async fn fake_flaresolverr_serving(
        page: &'static str,
    ) -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
        fake_flaresolverr_routing(&[], page).await
    }

    /// [`fake_flaresolverr_serving`], answering a URL containing a route's needle with that
    /// route's page, and any other URL with `default`.
    async fn fake_flaresolverr_routing(
        routes: &'static [(&'static str, &'static str)],
        default: &'static str,
    ) -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
        fake_flaresolverr_flaky(0, routes, default).await
    }

    /// [`fake_flaresolverr_routing`], answering the first `failures` requests with Cloudflare's
    /// 525 page, as AO3 does during a partial outage.
    async fn fake_flaresolverr_flaky(
        failures: usize,
        routes: &'static [(&'static str, &'static str)],
        default: &'static str,
    ) -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        use tokio::{
            io::{AsyncReadExt as _, AsyncWriteExt as _},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let log = Arc::clone(&log);
                let served = Arc::clone(&served);
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let body = loop {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        raw.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&raw).into_owned();
                        if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                            let length = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if rest.len() >= length || n == 0 {
                                break rest.to_owned();
                            }
                        } else if n == 0 {
                            return;
                        }
                    };
                    let request: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or_default();
                    let url = request["url"].as_str().unwrap_or_default();
                    let n = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let page = if n < failures {
                        CLOUDFLARE_525
                    } else {
                        routes
                            .iter()
                            .find(|(needle, _)| url.contains(needle))
                            .map_or(default, |(_, page)| *page)
                    };
                    let reply = serde_json::json!({
                        "status": "ok", "message": "", "session": "fake",
                        "solution": {
                            "url": request["url"], "status": 200, "headers": {},
                            "response": page,
                            "userAgent": "FakeClearanceUA/1.0",
                            "cookies": [
                                {"name": "_otwarchive_session", "value": "fresh-session-value", "domain": ".archiveofourown.org",
                                 "path": "/", "secure": true, "expires": -1},
                                {"name": "cf_clearance", "value": "fresh-clearance-value", "domain": ".archiveofourown.org",
                                 "path": "/", "secure": true, "expires": 1_900_000_000}
                            ]
                        }
                    })
                    .to_string();
                    log.lock().unwrap().push(request);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (port, seen)
    }

    /// The clearance step against the real worker: `FlareSolverr`'s cookies replace the stored
    /// ones of the same name, the login cookie is kept, and the User-Agent is left for the caller.
    /// `FFD_PYTHON=/path/to/venv/bin/python cargo test -p fanfold-adapter-fanficfare -- --ignored`
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn the_worker_merges_flaresolverrs_clearance_with_the_stored_login() {
        let python = std::env::var("FFD_PYTHON").expect("set FFD_PYTHON");
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_flaresolverr().await;
        let login_ini = scratch.path().join("personal-login.ini");
        std::fs::write(&login_ini, "[defaults]\nmax_request_retries:0\n").unwrap();
        let store = Arc::new(InMemorySession::new());
        let mut session = stored_session(NOW - 100);
        session.cookies.push(cookie(
            "_otwarchive_session",
            "stale-session-value",
            Some(NOW + 86_400),
        ));
        session.cookies.push(cookie(
            "remember_user_token",
            "login-token-0123456789",
            Some(NOW + 86_400),
        ));
        store.save(&session).await.unwrap();
        let dl = FanFicFareDownloader::new(
            Arc::new(TokioProcessRunner),
            FanFicFareConfig {
                python,
                scratch_dir: scratch.path().to_path_buf(),
                personal_ini: None,
                timeout: Duration::from_secs(60),
                // FanFicFare's offline site, so the download itself needs no network.
                url_template: "http://test1.com?sid={id}".into(),
                redactions: vec![],
                login_fallback: Some(LoginFallback {
                    personal_ini: login_ini,
                    flaresolverr: format!("http://127.0.0.1:{port}").parse().unwrap(),
                    flaresolverr_timeout: Duration::from_secs(10),
                }),
            },
        )
        .with_session(
            Arc::clone(&store) as Arc<dyn SessionStore>,
            Arc::new(FakeClock::at_secs(NOW)),
        );
        let workdir = dl.make_workdir(&request(None)).await.unwrap();
        let book = dl
            .run_worker(&request(None), &workdir, Attempt::Fallback)
            .await
            .unwrap();
        assert_eq!(book.title, "Test Story Title 42");

        let asked = seen.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "one clearance request: {asked:?}");
        assert_eq!(asked[0]["cmd"], "request.get");
        assert_eq!(asked[0]["url"], "http://test1.com/users/login");

        let saved = std::fs::read_to_string(workdir.join("cookies.txt")).unwrap();
        let value_of = |name: &str| {
            saved
                .lines()
                .map(|l| {
                    l.trim_start_matches("#HttpOnly_")
                        .split('\t')
                        .collect::<Vec<_>>()
                })
                .find(|f| f.get(5) == Some(&name))
                .and_then(|f| f.get(6).map(|v| (*v).to_owned()))
        };
        assert_eq!(
            value_of("_otwarchive_session").as_deref(),
            Some("fresh-session-value")
        );
        assert_eq!(
            value_of("cf_clearance").as_deref(),
            Some("fresh-clearance-value")
        );
        assert_eq!(
            value_of("remember_user_token").as_deref(),
            Some("login-token-0123456789"),
            "only a real login can issue the login cookie, so the stored one stays"
        );
        assert_eq!(value_of("view_adult").as_deref(), Some("true"));
        assert_eq!(
            std::fs::read_to_string(workdir.join("user_agent.txt")).unwrap(),
            "FakeClearanceUA/1.0"
        );
        dl.release(&book).await;
    }

    /// Regression: a failed *update* left the old book on disk, which the worker took for a
    /// success ("unchanged"), so a login failure was reported as an up-to-date book.
    /// Story 668 of FanFicFare's offline site demands the username `Me`.
    /// `FFD_PYTHON=/path/to/venv/bin/python cargo test -p fanfold-adapter-fanficfare -- --ignored`
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_failed_update_is_a_failure_not_an_unchanged_book() {
        let python = std::env::var("FFD_PYTHON").expect("set FFD_PYTHON");
        let scratch = tempfile::tempdir().unwrap();
        let with_login = scratch.path().join("login.ini");
        std::fs::write(&with_login, "[test1.com]\nusername:Me\npassword:x\n").unwrap();
        let without_login = scratch.path().join("anonymous.ini");
        std::fs::write(&without_login, "[test1.com]\nis_adult:true\n").unwrap();
        let downloader = |ini: &Path| {
            FanFicFareDownloader::new(
                Arc::new(TokioProcessRunner),
                FanFicFareConfig {
                    python: python.clone(),
                    scratch_dir: scratch.path().to_path_buf(),
                    personal_ini: Some(ini.to_path_buf()),
                    timeout: Duration::from_secs(60),
                    url_template: "http://test1.com?sid=668".into(),
                    redactions: vec![],
                    login_fallback: None,
                },
            )
        };

        let first = downloader(&with_login)
            .download(&request(None))
            .await
            .unwrap();
        let library = tempfile::tempdir().unwrap();
        let stored = library.path().join("Login story [668].epub");
        std::fs::copy(&first.path, &stored).unwrap();

        let err = downloader(&without_login)
            .download(&request(Some(stored.clone())))
            .await
            .expect_err("without the login the update cannot work, whatever is on disk");
        assert_eq!(err.kind, FailureKind::AuthFailed, "{}", err.message);
        assert!(stored.exists(), "the stored book is left untouched");
    }

    /// A downloader for one story, with `ini` as the personal.ini and no login fallback.
    fn real_downloader(scratch: &Path, ini: &Path, url_template: &str) -> FanFicFareDownloader {
        FanFicFareDownloader::new(
            Arc::new(TokioProcessRunner),
            FanFicFareConfig {
                python: std::env::var("FFD_PYTHON").expect("set FFD_PYTHON"),
                scratch_dir: scratch.to_path_buf(),
                personal_ini: Some(ini.to_path_buf()),
                timeout: Duration::from_secs(60),
                url_template: url_template.into(),
                redactions: vec![],
                login_fallback: None,
            },
        )
    }

    /// Story 1001 of FanFicFare's offline site: its stats claim 3 chapters, it has `chapters`.
    /// That is what an AO3 work looks like when its chapter list page was an error page.
    fn story_1001_ini(dir: &Path, name: &str, chapters: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "[teststory:1001]\nvalid_entries:title,author,authorId,numChapters\n\
                 title:Story 1001\nauthor:Nobody\nauthorId:1\nnumChapters:3\n\
                 chaptertitles:{chapters}\n"
            ),
        )
        .unwrap();
        path
    }

    /// The chapters FanFicFare itself finds in an EPUB.
    fn chapters_in(book: &Path) -> usize {
        let out = std::process::Command::new(std::env::var("FFD_PYTHON").unwrap())
            .args([
                "-I",
                "-c",
                "import sys; from fanficfare.epubutils import get_dcsource_chaptercount as c; \
                 print(c(sys.argv[1])[1])",
            ])
            .arg(book)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    /// Regression (work 10000003 in production): a Cloudflare error page in place of AO3's
    /// chapter list made FanFicFare write a book with only a title page and report success,
    /// because it checks the chapter count the work's stats claim, not the chapters it found.
    /// `FFD_PYTHON=/path/to/venv/bin/python cargo test -p fanfold-adapter-fanficfare -- --ignored`
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_book_without_chapters_is_a_failure_not_a_download() {
        let scratch = tempfile::tempdir().unwrap();
        let empty = story_1001_ini(scratch.path(), "empty.ini", "");
        let full = story_1001_ini(scratch.path(), "full.ini", "Prologue,Chapter 1");
        let url = "http://test1.com?sid=1001";

        let err = real_downloader(scratch.path(), &empty, url)
            .download(&request(None))
            .await
            .expect_err("a title page alone is not a book");
        assert_eq!(err.kind, FailureKind::SiteUnavailable, "{}", err.message);

        // Nor may an update swap a good book for an empty one.
        let good = real_downloader(scratch.path(), &full, url)
            .download(&request(None))
            .await
            .unwrap();
        let library = tempfile::tempdir().unwrap();
        let stored = library.path().join("Story 1001 [1001].epub");
        std::fs::copy(&good.path, &stored).unwrap();
        let before = std::fs::read(&stored).unwrap();
        let err = real_downloader(scratch.path(), &empty, url)
            .download(&request(Some(stored.clone())))
            .await
            .expect_err("an update that finds no chapters is a failure");
        assert_eq!(err.kind, FailureKind::SiteUnavailable, "{}", err.message);
        assert_eq!(
            std::fs::read(&stored).unwrap(),
            before,
            "the good book stays"
        );
    }

    /// The books the bug above already wrote must heal: FanFicFare refuses to update a book with
    /// no chapters ("no recognizable chapters") and reports it unchanged, forever.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_stored_book_without_chapters_is_downloaded_again() {
        let scratch = tempfile::tempdir().unwrap();
        let empty = story_1001_ini(scratch.path(), "empty.ini", "");
        let full = story_1001_ini(scratch.path(), "full.ini", "Prologue,Chapter 1");
        // Such a book, written the way the bug wrote it: FanFicFare's CLI, no checks.
        let library = tempfile::tempdir().unwrap();
        let stored = library.path().join("Story 1001 [1001].epub");
        let made = std::process::Command::new(std::env::var("FFD_PYTHON").expect("set FFD_PYTHON"))
            .args([
                "-I",
                "-m",
                "fanficfare.cli",
                "--non-interactive",
                "-f",
                "epub",
                "-c",
            ])
            .arg(&empty)
            .arg("-o")
            .arg(format!("output_filename={}", stored.display()))
            .arg("http://test1.com?sid=1001")
            .current_dir(scratch.path())
            .output()
            .unwrap();
        assert!(stored.exists(), "{}", String::from_utf8_lossy(&made.stderr));
        assert_eq!(chapters_in(&stored), 0);

        let book = real_downloader(scratch.path(), &full, "http://test1.com?sid=1001")
            .download(&request(Some(stored.clone())))
            .await
            .unwrap();
        assert!(book.changed, "{}", book.log);
        assert_eq!(chapters_in(&book.path), 2, "{}", book.log);
    }

    /// Cloudflare's error page as `FlareSolverr` returned it for AO3 (trimmed): HTTP 200.
    const CLOUDFLARE_525: &str = "<html><head><title>archiveofourown.org | 525: SSL handshake \
        failed</title></head><body><div id=\"cf-wrapper\"><div id=\"cf-error-details\" \
        class=\"cf-error-details-wrapper\"><h1>SSL handshake failed</h1>\
        <span class=\"cf-error-type\">Error code 525</span></div></div></body></html>";

    /// The root of the bug above: `FlareSolverr` reports 200 for Cloudflare's error pages, so
    /// FanFicFare parsed "525: SSL handshake failed" as the AO3 page it had asked for.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_cloudflare_error_page_through_flaresolverr_is_an_unavailable_site() {
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_flaresolverr_serving(CLOUDFLARE_525).await;
        let ini = scratch.path().join("personal.ini");
        std::fs::write(
            &ini,
            format!(
                "[defaults]\nuse_flaresolverr_proxy:true\nflaresolverr_proxy_protocol:http\n\
                 flaresolverr_proxy_address:127.0.0.1\nflaresolverr_proxy_port:{port}\n\
                 flaresolverr_proxy_timeout:10000\n\n[archiveofourown.org]\nis_adult:true\n"
            ),
        )
        .unwrap();
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .download(&request(None))
        .await
        .expect_err("an error page is not AO3's page");
        assert_eq!(err.kind, FailureKind::SiteUnavailable, "{}", err.message);
        assert!(err.message.contains("525"), "{}", err.message);
        assert!(
            !seen.lock().unwrap().is_empty(),
            "it went through FlareSolverr"
        );
    }

    /// What AO3's bot filter answered from a flagged address (November 2026, HTTP 429 as plain
    /// text, which `FlareSolverr` reports as a 200).
    const AO3_BOT_FILTER: &str = "The bot servers are currently overloaded, please try again \
        later. You are seeing this page because our systems think you may be a robot. If you are \
        not a robot, this could be due to using a VPN or privacy-focused browser.";

    async fn download_through_flaresolverr_serving(page: &'static str) -> DownloadError {
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_flaresolverr_serving(page).await;
        let ini = flaresolverr_ini(scratch.path(), port);
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .download(&request(None))
        .await
        .expect_err("that is not the work");
        assert!(
            !seen.lock().unwrap().is_empty(),
            "it went through FlareSolverr"
        );
        err
    }

    /// Regression (work 10000001 in production, jobs failed after five tries): AO3's bot filter
    /// was parsed as the work and crashed FanFicFare (`'NoneType' object has no attribute
    /// 'get_text'`), a transient failure retried within minutes. It is a rate limit: the queue
    /// waits for the site, and the job's tries are not used up.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn ao3_s_bot_filter_through_flaresolverr_is_a_rate_limit() {
        let err = download_through_flaresolverr_serving(AO3_BOT_FILTER).await;
        assert_eq!(err.kind, FailureKind::RateLimited, "{}", err.message);
        assert!(err.message.contains("bot protection"), "{}", err.message);
        assert!(err.message.contains("may be a robot"), "{}", err.message);
    }

    /// Whatever else answers in AO3's place is an outage, and the job log says what it was.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_page_that_is_not_ao3_s_is_an_outage_that_shows_what_it_was() {
        let err = download_through_flaresolverr_serving(
            "<html><body><h1>502 Bad Gateway</h1><hr>nginx</body></html>",
        )
        .await;
        assert_eq!(err.kind, FailureKind::SiteUnavailable, "{}", err.message);
        assert!(err.message.contains("502 Bad Gateway"), "{}", err.message);
    }

    /// An AO3 page FanFicFare cannot read (AO3 changed its markup) is named, not a bare
    /// `AttributeError`.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn an_ao3_page_fanficfare_cannot_read_is_named_in_the_failure() {
        let err = download_through_flaresolverr_serving(
            "<html><body><div id=\"main\" class=\"works-show region\">\
             <p>A layout nobody has seen before</p></div></body></html>",
        )
        .await;
        assert_eq!(err.kind, FailureKind::Transient, "{}", err.message);
        assert!(err.message.contains("could not read"), "{}", err.message);
        assert!(
            err.message.contains("archiveofourown.org/works/"),
            "{}",
            err.message
        );
        assert!(err.message.contains("A layout nobody"), "{}", err.message);
    }

    /// The bot filter is no empty series either.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn ao3_s_bot_filter_is_not_an_empty_series() {
        let scratch = tempfile::tempdir().unwrap();
        let (port, _seen) = fake_flaresolverr_serving(AO3_BOT_FILTER).await;
        let ini = flaresolverr_ini(scratch.path(), port);
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(fanfold_domain::work::SeriesId::new(77).unwrap(), 0)
        .await
        .expect_err("an error page lists nothing");
        assert_eq!(err.kind, FailureKind::RateLimited, "{}", err.message);
    }

    /// A synthetic AO3 series page (the markup of a real one, every text made up).
    const AO3_SERIES_PAGE: &str = include_str!("../testdata/ao3_series.html");

    fn flaresolverr_ini(dir: &Path, port: u16) -> PathBuf {
        let ini = dir.join("personal.ini");
        std::fs::write(
            &ini,
            format!(
                "[defaults]\nuse_flaresolverr_proxy:true\nflaresolverr_proxy_protocol:http\n\
                 flaresolverr_proxy_address:127.0.0.1\nflaresolverr_proxy_port:{port}\n\
                 flaresolverr_proxy_timeout:10000\n\n[archiveofourown.org]\nis_adult:true\n"
            ),
        )
        .unwrap();
        ini
    }

    /// A series page lists its works in series order, through `FlareSolverr` like any page.
    /// `FFD_PYTHON=/path/to/venv/bin/python cargo test -p fanfold-adapter-fanficfare -- --ignored`
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_series_is_listed_in_series_order() {
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_flaresolverr_serving(AO3_SERIES_PAGE).await;
        let ini = flaresolverr_ini(scratch.path(), port);
        let series = fanfold_domain::work::SeriesId::new(77).unwrap();
        let listing = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(series, 0)
        .await
        .unwrap();
        assert_eq!(listing.series, series);
        assert_eq!(listing.facts.name, "A Test Saga");
        assert_eq!(listing.facts.complete, Some(false));
        let works: Vec<u64> = listing.facts.works.iter().map(|w| w.get()).collect();
        assert_eq!(works, [1001, 1002]);
        assert_eq!(listing.raw["name"], "A Test Saga");
        let asked = seen.lock().unwrap().clone();
        assert!(
            asked
                .iter()
                .any(|r| r["url"].as_str().is_some_and(|u| u.contains("/series/77"))),
            "{asked:?}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(scratch.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("series-"))
            .collect();
        assert!(leftovers.is_empty(), "a listing leaves no scratch space");
    }

    /// The same error page that once became an empty book must not become an empty series.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_cloudflare_error_page_is_not_an_empty_series() {
        let scratch = tempfile::tempdir().unwrap();
        let (port, _seen) = fake_flaresolverr_serving(CLOUDFLARE_525).await;
        let ini = flaresolverr_ini(scratch.path(), port);
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(fanfold_domain::work::SeriesId::new(77).unwrap(), 0)
        .await
        .expect_err("an error page lists nothing");
        assert_eq!(err.kind, FailureKind::SiteUnavailable, "{}", err.message);
    }

    /// A download reports what the site said about the work, lists as lists.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_download_reports_the_works_metadata() {
        let scratch = tempfile::tempdir().unwrap();
        let ini = scratch.path().join("story.ini");
        std::fs::write(
            &ini,
            "[test1.com]\n\
             extra_valid_entries:fandoms,freeformtags,hits,byline,restricted,chapterslashtotal,\
             series00,series01,series00Url,series01Url\n\n\
             [teststory:1002]\n\
             valid_entries:title,author,authorId,authorUrl,byline,series00,series00Url,series01,\
             series01Url,freeformtags_list,fandoms_list,ships_list,rating,status,numWords,hits,\
             restricted,chapterslashtotal\n\
             title:Two Series Story\nauthor:Nobody\nauthorId:nobody\n\
             authorUrl:https://archiveofourown.org/users/nobody/pseuds/Nobody\nbyline:Nobody\n\
             series00:First Saga [3]\nseries00Url:https://archiveofourown.org/series/77\n\
             series01:Second Saga [1]\nseries01Url:https://archiveofourown.org/series/88\n\
             freeformtags_list:Fluff,Slow Burn\nfandoms_list:Test Fandom\n\
             ships_list:A &amp; B\nrating:Mature\nstatus:In-Progress\nnumWords:12,345\n\
             hits:1,234\nrestricted:Restricted\nchapterslashtotal:2/?\n\
             chaptertitles:Prologue,Chapter 1\n",
        )
        .unwrap();
        let mut req = request(None);
        req.work = WorkId::new(1002).unwrap();
        let book = real_downloader(scratch.path(), &ini, "http://test1.com?sid={id}")
            .download(&req)
            .await
            .unwrap();
        let m = book.metadata.clone().expect("metadata");
        assert_eq!(m.work, req.work);
        assert_eq!(m.facts.title, "Two Series Story");
        assert_eq!(m.facts.byline, "Nobody");
        assert!(m.facts.restricted && m.facts.adult());
        assert_eq!((m.facts.chapters_posted, m.facts.chapters_total), (2, None));
        assert_eq!(m.facts.words, 12_345);
        assert_eq!(m.stats.hits, Some(1234));
        let series: Vec<(u64, &str, Option<u32>)> = m
            .series
            .iter()
            .map(|s| (s.id.get(), s.name.as_str(), s.part))
            .collect();
        assert_eq!(
            series,
            [(77, "First Saga", Some(3)), (88, "Second Saga", Some(1))]
        );
        assert!(m.tags.iter().any(|t| t.name == "A & B"), "{:?}", m.tags);
        assert!(m.tags.iter().any(|t| t.name == "Fluff"));
        assert_eq!(m.authors[0].user, "nobody");
        assert_eq!(m.chapters.len(), 2);
        assert_eq!(m.raw["title"], "Two Series Story");
        dl_release(&book).await;
    }

    async fn dl_release(book: &DownloadedBook) {
        if let Some(dir) = book.path.parent() {
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
    }

    /// An AO3 work that only logged-in users may read.
    const AO3_LOGIN_REQUIRED: &str = "<html><body><div id=\"main\">\
        <p class=\"notice\">This work is only available to registered users of the Archive.</p>\
        </div></body></html>";

    /// Regression (work 10000001 in production): the login token request hit AO3's 525 through
    /// `FlareSolverr`; `FanFicFare` reports that as "Failed getting login token", which was taken
    /// for a transient error. The site was down: it is an outage, retried at no cost.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_login_token_lost_to_an_outage_is_an_outage() {
        static ROUTES: [(&str, &str); 1] = [("token_dispenser", CLOUDFLARE_525)];
        let scratch = tempfile::tempdir().unwrap();
        let (port, _seen) = fake_flaresolverr_routing(&ROUTES, AO3_LOGIN_REQUIRED).await;
        let ini = flaresolverr_ini(scratch.path(), port);
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .download(&request(None))
        .await
        .expect_err("no work behind a 525");
        assert_eq!(
            err.kind,
            FailureKind::SiteUnavailable,
            "{}\n{}",
            err.message,
            err.log
        );
    }

    /// When the site does answer but the login cannot be done this way, it is a login problem:
    /// the login fallback (plain HTTP after a `FlareSolverr` clearance) must get its turn.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_login_token_that_cannot_be_read_asks_for_the_login_fallback() {
        static ROUTES: [(&str, &str); 1] =
            [("token_dispenser", "<html><body>not the token</body></html>")];
        let scratch = tempfile::tempdir().unwrap();
        let (port, _seen) = fake_flaresolverr_routing(&ROUTES, AO3_LOGIN_REQUIRED).await;
        let ini = flaresolverr_ini(scratch.path(), port);
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .download(&request(None))
        .await
        .expect_err("no login, no work");
        assert_eq!(
            err.kind,
            FailureKind::AuthFailed,
            "{}\n{}",
            err.message,
            err.log
        );
    }

    /// AO3's partial outage: a few 525s in a row, then the page. The worker retries through
    /// them instead of failing the whole download (and waiting minutes for the next try).
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_few_cloudflare_525s_are_retried_through() {
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_flaresolverr_flaky(2, &[], AO3_SERIES_PAGE).await;
        let ini = flaresolverr_ini(scratch.path(), port);
        let listing = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(fanfold_domain::work::SeriesId::new(77).unwrap(), 0)
        .await
        .expect("two 525s are retried through");
        assert_eq!(listing.facts.works.len(), 2);
        assert_eq!(seen.lock().unwrap().len(), 3, "two retries, then the page");
    }

    /// Opt-in: drives the *real* worker against FanFicFare's offline `test1.com` site.
    /// `FFD_PYTHON=/path/to/venv/bin/python cargo test -p fanfold-adapter-fanficfare -- --ignored`
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn real_worker_roundtrip_against_the_offline_test_site() {
        let python = std::env::var("FFD_PYTHON").expect("set FFD_PYTHON");
        let scratch = tempfile::tempdir().unwrap();
        let dl = FanFicFareDownloader::new(
            Arc::new(TokioProcessRunner),
            FanFicFareConfig {
                python,
                scratch_dir: scratch.path().to_path_buf(),
                personal_ini: None,
                timeout: Duration::from_secs(120),
                url_template: "http://test1.com?sid={id}".into(),
                redactions: vec![],
                login_fallback: None,
            },
        );

        // 1. fresh download
        let first = dl.download(&request(None)).await.unwrap();
        assert!(first.changed);
        assert_eq!(first.title, "Test Story Title 42");
        assert!(std::fs::metadata(&first.path).unwrap().len() > 1000);

        // 2. update with nothing new
        let library = tempfile::tempdir().unwrap();
        let stored = library.path().join("Testing [42].epub");
        std::fs::copy(&first.path, &stored).unwrap();
        let second = dl.download(&request(Some(stored.clone()))).await.unwrap();
        assert!(!second.changed, "no new chapters: {}", second.log);
        assert_eq!(second.title, "Test Story Title 42");

        // 3. unsupported site is permanent
        let bad = FanFicFareDownloader::new(
            Arc::new(TokioProcessRunner),
            FanFicFareConfig {
                python: std::env::var("FFD_PYTHON").unwrap(),
                scratch_dir: scratch.path().to_path_buf(),
                personal_ini: None,
                timeout: Duration::from_secs(120),
                url_template: "http://nosuchsite.invalid/{id}".into(),
                redactions: vec![],
                login_fallback: None,
            },
        );
        assert_eq!(
            bad.download(&request(None)).await.unwrap_err().kind,
            FailureKind::Permanent
        );
    }
}
