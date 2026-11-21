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
use fanfold_app::ports::{DownloadError, DownloadRequest, DownloadedBook, Downloader};
use fanfold_domain::{failure::FailureKind, metadata::SeriesMetadata, work::SeriesId};
use secrecy::{ExposeSecret, SecretString};

use crate::{
    protocol::{
        BrowserRef, WorkerRequest, WorkerResponse, into_listing, into_result, parse_response,
    },
    runner::{ProcessOutput, ProcessRunner, ProcessSpec, RunError},
};

/// The worker script, shipped inside the binary and fed to `python -c`.
const WORKER_SCRIPT: &str = include_str!("../worker/ffd_worker.py");

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// fanfold-browser: one browser that makes every request, logins included, and keeps the only
/// cookies (Cloudflare's clearance and the site's login): FanFicFare keeps none.
#[derive(Debug, Clone)]
pub struct BrowserService {
    /// Origin of the service.
    pub url: url::Url,
    /// How long one request may take there, waiting in its queue included.
    pub timeout: Duration,
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
    /// Set when every request goes through fanfold-browser; see [`BrowserService`].
    pub browser: Option<BrowserService>,
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

pub struct FanFicFareDownloader {
    runner: Arc<dyn ProcessRunner>,
    config: FanFicFareConfig,
}

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
        Self { runner, config }
    }

    /// What the worker is told about fanfold-browser, when every request goes through it.
    fn browser_ref<'a>(&self, url: Option<&'a str>) -> Option<BrowserRef<'a>> {
        let timeout = self.config.browser.as_ref()?.timeout;
        Some(BrowserRef {
            url: url?,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        })
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
    ) -> Result<DownloadedBook, DownloadError> {
        let response = self.run_task(Task::Download(request), workdir).await?;
        into_result(response, workdir, request.work)
    }

    async fn run_listing(
        &self,
        series: SeriesId,
        workdir: &Path,
    ) -> Result<SeriesMetadata, DownloadError> {
        let response = self.run_task(Task::ListSeries(series), workdir).await?;
        into_listing(response, series)
    }

    async fn run_task(
        &self,
        task: Task<'_>,
        workdir: &Path,
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
        let ini_str = self
            .config
            .personal_ini
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let browser_url = self.config.browser.as_ref().map(|b| b.url.to_string());
        let browser = self.browser_ref(browser_url.as_deref());
        let payload = serde_json::to_vec(&WorkerRequest {
            op: task.op(),
            url: &url,
            existing: existing_str.as_deref(),
            workdir: &workdir_str,
            personal_ini: ini_str.as_deref(),
            browser,
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
        // Redact the secrets we know of from what the worker printed.
        let secrets = self.config.redactions.clone();
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
        let result = self.run_worker(request, &workdir).await;
        if result.is_err() {
            // No book will be handed out, so nobody would call `release`.
            remove_dir(&workdir).await;
        }
        result
    }

    async fn list_series(&self, series: SeriesId) -> Result<SeriesMetadata, DownloadError> {
        let workdir = self.make_dir(&format!("series-{series}")).await?;
        let result = self.run_listing(series, &workdir).await;
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
                browser: None,
            },
        )
    }

    fn request(existing: Option<PathBuf>) -> DownloadRequest {
        DownloadRequest {
            work: WorkId::new(42).unwrap(),
            existing,
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

    /// A stand-in for fanfold-browser, answering every page with `page` and HTTP status 200 (what
    /// a browser shows when an error page comes with a 200, or after a challenge), recording the
    /// requests it was sent.
    async fn fake_browser_serving(page: &'static str) -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
        fake_browser_routing(&[], page).await
    }

    /// [`fake_browser_serving`], answering a URL containing a route's needle with that
    /// route's page, and any other URL with `default`.
    async fn fake_browser_routing(
        routes: &'static [(&'static str, &'static str)],
        default: &'static str,
    ) -> (u16, Arc<Mutex<Vec<serde_json::Value>>>) {
        fake_browser_flaky(0, routes, default).await
    }

    /// [`fake_browser_routing`], answering the first `failures` requests with Cloudflare's
    /// 525 page, as AO3 does during a partial outage.
    async fn fake_browser_flaky(
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
                        "status": 200, "url": request["url"], "content_type": "text/html",
                        "body_b64": base64(page.as_bytes()),
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
                    browser: None,
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

    /// A downloader for one story, with `ini` as the personal.ini, and through the fake browser
    /// when [`browser_ini`] set one up in `scratch`.
    fn real_downloader(scratch: &Path, ini: &Path, url_template: &str) -> FanFicFareDownloader {
        let browser = std::fs::read_to_string(scratch.join("browser-port"))
            .ok()
            .map(|port| BrowserService {
                url: format!("http://127.0.0.1:{}", port.trim()).parse().unwrap(),
                timeout: Duration::from_secs(10),
            });
        FanFicFareDownloader::new(
            Arc::new(TokioProcessRunner),
            FanFicFareConfig {
                python: std::env::var("FFD_PYTHON").expect("set FFD_PYTHON"),
                scratch_dir: scratch.to_path_buf(),
                personal_ini: Some(ini.to_path_buf()),
                timeout: Duration::from_secs(60),
                url_template: url_template.into(),
                redactions: vec![],
                browser,
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

    /// Cloudflare's error page for AO3 (trimmed), as a browser shows it.
    const CLOUDFLARE_525: &str = "<html><head><title>archiveofourown.org | 525: SSL handshake \
        failed</title></head><body><div id=\"cf-wrapper\"><div id=\"cf-error-details\" \
        class=\"cf-error-details-wrapper\"><h1>SSL handshake failed</h1>\
        <span class=\"cf-error-type\">Error code 525</span></div></div></body></html>";

    /// The root of the bug above: Cloudflare's error page came as a 200 (through `FlareSolverr`
    /// then), so FanFicFare parsed "525: SSL handshake failed" as the AO3 page it had asked for.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_cloudflare_error_page_through_the_browser_is_an_unavailable_site() {
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_browser_serving(CLOUDFLARE_525).await;
        let ini = browser_ini(scratch.path(), port);
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
            "it went through the browser"
        );
    }

    /// What AO3's bot filter answered from a flagged address (November 2026, HTTP 429 as plain
    /// text; through a browser it can arrive as a 200 page).
    const AO3_BOT_FILTER: &str = "The bot servers are currently overloaded, please try again \
        later. You are seeing this page because our systems think you may be a robot. If you are \
        not a robot, this could be due to using a VPN or privacy-focused browser.";

    async fn download_through_the_browser_serving(page: &'static str) -> DownloadError {
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_browser_serving(page).await;
        let ini = browser_ini(scratch.path(), port);
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
            "it went through the browser"
        );
        err
    }

    /// Regression (work 10000001 in production, jobs failed after five tries): AO3's bot filter
    /// was parsed as the work and crashed FanFicFare (`'NoneType' object has no attribute
    /// 'get_text'`), a transient failure retried within minutes. It is a rate limit: the queue
    /// waits for the site, and the job's tries are not used up.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn ao3_s_bot_filter_through_the_browser_is_a_rate_limit() {
        let err = download_through_the_browser_serving(AO3_BOT_FILTER).await;
        assert_eq!(err.kind, FailureKind::RateLimited, "{}", err.message);
        assert!(err.message.contains("bot protection"), "{}", err.message);
        assert!(err.message.contains("may be a robot"), "{}", err.message);
    }

    /// Whatever else answers in AO3's place is an outage, and the job log says what it was.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_page_that_is_not_ao3_s_is_an_outage_that_shows_what_it_was() {
        let err = download_through_the_browser_serving(
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
        let err = download_through_the_browser_serving(
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
        let (port, _seen) = fake_browser_serving(AO3_BOT_FILTER).await;
        let ini = browser_ini(scratch.path(), port);
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(fanfold_domain::work::SeriesId::new(77).unwrap())
        .await
        .expect_err("an error page lists nothing");
        assert_eq!(err.kind, FailureKind::RateLimited, "{}", err.message);
    }

    /// A synthetic AO3 series page (the markup of a real one, every text made up).
    const AO3_SERIES_PAGE: &str = include_str!("../testdata/ao3_series.html");

    /// A plain personal.ini, and the fake fanfold-browser on `port` remembered for
    /// [`real_downloader`] (which then sends every request through it).
    fn browser_ini(dir: &Path, port: u16) -> PathBuf {
        let ini = dir.join("personal.ini");
        std::fs::write(&ini, "[archiveofourown.org]\nis_adult:true\n").unwrap();
        std::fs::write(dir.join("browser-port"), port.to_string()).unwrap();
        ini
    }

    /// Standard base64, for the fake browser's page bodies.
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// A series page lists its works in series order, through the browser like any page.
    /// `FFD_PYTHON=/path/to/venv/bin/python cargo test -p fanfold-adapter-fanficfare -- --ignored`
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_series_is_listed_in_series_order() {
        let scratch = tempfile::tempdir().unwrap();
        let (port, seen) = fake_browser_serving(AO3_SERIES_PAGE).await;
        let ini = browser_ini(scratch.path(), port);
        let series = fanfold_domain::work::SeriesId::new(77).unwrap();
        let listing = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(series)
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
        let (port, _seen) = fake_browser_serving(CLOUDFLARE_525).await;
        let ini = browser_ini(scratch.path(), port);
        let err = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(fanfold_domain::work::SeriesId::new(77).unwrap())
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

    /// Regression (work 10000001 in production): the login token request hit AO3's 525;
    /// `FanFicFare` reports that as "Failed getting login token", which was taken for a
    /// transient error. The site was down: it is an outage, retried at no cost.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_login_token_lost_to_an_outage_is_an_outage() {
        static ROUTES: [(&str, &str); 1] = [("token_dispenser", CLOUDFLARE_525)];
        let scratch = tempfile::tempdir().unwrap();
        let (port, _seen) = fake_browser_routing(&ROUTES, AO3_LOGIN_REQUIRED).await;
        let ini = browser_ini(scratch.path(), port);
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

    /// When the site does answer but the login cannot be done, it is a login problem.
    #[tokio::test]
    #[ignore = "needs FFD_PYTHON pointing at a Python with fanficfare installed"]
    async fn a_login_token_that_cannot_be_read_is_a_login_failure() {
        static ROUTES: [(&str, &str); 1] =
            [("token_dispenser", "<html><body>not the token</body></html>")];
        let scratch = tempfile::tempdir().unwrap();
        let (port, _seen) = fake_browser_routing(&ROUTES, AO3_LOGIN_REQUIRED).await;
        let ini = browser_ini(scratch.path(), port);
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
        let (port, seen) = fake_browser_flaky(2, &[], AO3_SERIES_PAGE).await;
        let ini = browser_ini(scratch.path(), port);
        let listing = real_downloader(
            scratch.path(),
            &ini,
            "https://archiveofourown.org/works/{id}",
        )
        .list_series(fanfold_domain::work::SeriesId::new(77).unwrap())
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
                browser: None,
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
                browser: None,
            },
        );
        assert_eq!(
            bad.download(&request(None)).await.unwrap_err().kind,
            FailureKind::Permanent
        );
    }
}
