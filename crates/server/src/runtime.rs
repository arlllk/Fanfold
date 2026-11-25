//! Wiring and the worker loops.

use std::{sync::Arc, time::Duration};

use fanfold_adapter_discord::DiscordNotifier;
use fanfold_adapter_epub::EpubFinisher;
use fanfold_adapter_fanficfare::{
    BrowserService, FanFicFareConfig, FanFicFareDownloader, runner::ProcessRunner, sweep_stale,
};
use fanfold_adapter_fs::FsLibrary;
use fanfold_adapter_jmap::{JmapSettings, PushConfig, PushWatcher, ResilientMailSource};
use fanfold_adapter_postgres::{PgSettings, PgStore};
use fanfold_adapter_web::{WebDeps, ops_router, ui_router};
use fanfold_app::{
    admin::JobAdmin,
    error::RepoError,
    ingest::{IngestConfig, Ingestor},
    ports::{Clock, EventSource, Notifier},
    stats::MailStats,
    worker::{Ports, Tick, Worker, WorkerConfig},
};
use fanfold_domain::{breaker::BreakerConfig, retry::RetryPolicy};
use secrecy::{ExposeSecret, SecretString};
use sqlx::postgres::PgSslMode;
use thiserror::Error;
use tokio::{sync::watch, task::JoinSet};

use crate::{
    config::{Config, Secrets, SslMode},
    infra::{LogOnlyNotifier, SystemClock, SystemJitter},
    ini::{
        IniError, render_login_on_demand_ini, render_personal_ini, with_book_settings,
        write_private,
    },
};

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Ini(#[from] IniError),
    #[error("{0}")]
    Io(String),
}

const AO3_URL_TEMPLATE: &str = "https://archiveofourown.org/works/{id}";
const STALE_SCRATCH_AGE: Duration = Duration::from_secs(3600);
const MAX_ERROR_BACKOFF: Duration = Duration::from_secs(60);

const fn ssl_mode(mode: SslMode) -> PgSslMode {
    match mode {
        SslMode::Disable => PgSslMode::Disable,
        SslMode::Prefer => PgSslMode::Prefer,
        SslMode::Require => PgSslMode::Require,
        SslMode::VerifyCa => PgSslMode::VerifyCa,
        SslMode::VerifyFull => PgSslMode::VerifyFull,
    }
}

pub async fn connect_store(
    config: &Config,
    secrets: &Secrets,
) -> Result<Arc<PgStore>, StartupError> {
    let db = &config.database;
    let store = PgStore::connect(&PgSettings {
        host: db.host.clone(),
        port: db.port,
        database: db.name.clone(),
        user: db.user.clone(),
        password: secrets.database_password.expose_secret().to_owned(),
        ssl_mode: ssl_mode(db.ssl_mode),
        max_connections: db.max_connections,
    })
    .await?;
    store.migrate().await?;
    Ok(Arc::new(store))
}

fn copy_secret(secret: &SecretString) -> SecretString {
    SecretString::from(secret.expose_secret().to_owned())
}

/// Assemble the worker from real adapters, for AO3.
pub fn build_worker(
    config: &Config,
    secrets: &Secrets,
    store: &Arc<PgStore>,
    runner: Arc<dyn ProcessRunner>,
) -> Result<Worker, StartupError> {
    build_worker_with(config, secrets, store, runner, AO3_URL_TEMPLATE)
}

/// Discord when a webhook is configured (downloads and failures may go to different ones),
/// otherwise the log.
fn build_notifier(config: &Config, secrets: &Secrets) -> Result<Arc<dyn Notifier>, StartupError> {
    if secrets.discord_webhook.is_none() && secrets.discord_failures_webhook.is_none() {
        return Ok(Arc::new(LogOnlyNotifier));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| StartupError::Io(format!("cannot build HTTP client: {e}")))?;
    Ok(Arc::new(
        DiscordNotifier::routed(
            client,
            secrets.discord_webhook.as_ref().map(copy_secret),
            secrets.discord_failures_webhook.as_ref().map(copy_secret),
        )
        .with_ui(config.discord.as_ref().and_then(|d| d.ui_url.clone())),
    ))
}

/// Render and write FanFicFare's settings file.
fn write_fanficfare_settings(
    config: &Config,
    credentials: Option<(&SecretString, &SecretString)>,
) -> Result<(), StartupError> {
    // Through fanfold-browser FanFicFare logs in only when a page asks for it, and the browser
    // keeps the login (and every other cookie) between downloads. Without it, plain requests
    // log in on every download.
    let ini = if config.downloader.browser.is_some() {
        render_login_on_demand_ini(credentials)?
    } else {
        render_personal_ini(credentials)?
    };
    let ini = with_book_settings(&ini);
    write_private(&config.downloader.personal_ini, &ini).map_err(|e| {
        StartupError::Io(format!(
            "cannot write {}: {e}",
            config.downloader.personal_ini.display()
        ))
    })
}

/// As [`build_worker`], with the story URL template (`{id}` is the work id)
/// injectable so end-to-end tests can point at FanFicFare's offline test site.
pub fn build_worker_with(
    config: &Config,
    secrets: &Secrets,
    store: &Arc<PgStore>,
    runner: Arc<dyn ProcessRunner>,
    url_template: &str,
) -> Result<Worker, StartupError> {
    let credentials = secrets.ao3.as_ref().map(|a| (&a.username, &a.password));
    write_fanficfare_settings(config, credentials)?;

    let swept = sweep_stale(&config.library.scratch, STALE_SCRATCH_AGE);
    if swept > 0 {
        tracing::info!(count = swept, "removed stale scratch directories");
    }

    // Anything secret that could conceivably appear in downloader output is scrubbed.
    let mut redactions = vec![copy_secret(&secrets.database_password)];
    if let Some(ao3) = &secrets.ao3 {
        redactions.push(copy_secret(&ao3.password));
    }
    for webhook in [&secrets.discord_webhook, &secrets.discord_failures_webhook]
        .into_iter()
        .flatten()
    {
        redactions.push(copy_secret(webhook));
    }
    let downloader = FanFicFareDownloader::new(
        runner,
        FanFicFareConfig {
            python: config.downloader.python.clone(),
            scratch_dir: config.library.scratch.clone(),
            personal_ini: Some(config.downloader.personal_ini.clone()),
            timeout: config.downloader.timeout.0,
            url_template: url_template.to_owned(),
            redactions,
            browser: config.downloader.browser.as_ref().map(|b| BrowserService {
                url: b.url.clone(),
                timeout: b.timeout.0,
            }),
        },
    );

    let notifier = build_notifier(config, secrets)?;

    Ok(Worker::new(
        Ports {
            jobs: Arc::<PgStore>::clone(store),
            catalog: Arc::<PgStore>::clone(store),
            metadata: Arc::<PgStore>::clone(store),
            downloader: Arc::new(downloader),
            library: Arc::new(FsLibrary::new(config.library.path.clone())),
            finisher: Arc::new(EpubFinisher::new().map_err(|e| StartupError::Io(e.to_string()))?),
            notifier,
            clock: Arc::new(SystemClock),
            jitter: Arc::new(SystemJitter),
        },
        WorkerConfig {
            retry: RetryPolicy {
                max_attempts: config.retry.max_attempts,
                base: config.retry.base.0,
                cap: config.retry.cap.0,
            },
            breaker: BreakerConfig {
                failure_threshold: config.breaker.failure_threshold,
                open_for: config.breaker.open_for.0,
                max_open_for: config.breaker.max_open_for.0,
            },
        },
    ))
}

/// Sleep for `duration`, or return `true` early if shutdown is requested.
async fn sleep_or_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        () = tokio::time::sleep(duration) => *shutdown.borrow(),
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
    }
}

async fn worker_loop(worker: Arc<Worker>, poll: Duration, mut shutdown: watch::Receiver<bool>) {
    let mut error_backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            return;
        }
        let pause = match worker.run_once().await {
            // Work was done: look for more straight away.
            Ok(Tick::Processed { .. }) => {
                error_backoff = Duration::from_secs(1);
                continue;
            }
            Ok(Tick::Idle | Tick::BreakerOpen) => {
                error_backoff = Duration::from_secs(1);
                poll
            }
            Err(e) => {
                worker.stats().record_tick_error();
                tracing::error!(error = %e, retry_in = ?error_backoff, "worker tick failed");
                let pause = error_backoff;
                error_backoff = (error_backoff * 2).min(MAX_ERROR_BACKOFF);
                pause
            }
        };
        if sleep_or_shutdown(pause, &mut shutdown).await {
            return;
        }
    }
}

/// Run `concurrency` worker loops until `shutdown` flips to `true`.
pub async fn run_workers(
    worker: Arc<Worker>,
    poll: Duration,
    concurrency: u32,
    shutdown: watch::Receiver<bool>,
) {
    let mut loops = JoinSet::new();
    for _ in 0..concurrency {
        loops.spawn(worker_loop(Arc::clone(&worker), poll, shutdown.clone()));
    }
    while loops.join_next().await.is_some() {}
}

// `Arc::clone(&x)` would block the `Arc<T>` -> `Arc<dyn Port>` coercion in the rig.

// ------------------------------------------------------------------------- mail

/// Mail ingestion, built from the `[mail]` section.
pub struct MailRuntime {
    pub ingestor: Arc<Ingestor>,
    pub push: Option<PushWatcher>,
    pub poll: Duration,
    pub stats: Arc<MailStats>,
}

impl std::fmt::Debug for MailRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailRuntime")
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

/// Wire mail ingestion if `[mail]` is configured. Nothing connects yet: the source
/// connects on its first poll, so a mail-server outage at start does not stop the service.
#[must_use]
pub fn build_mail(
    config: &Config,
    secrets: &Secrets,
    store: &Arc<PgStore>,
    stats: Arc<MailStats>,
    shutdown: watch::Receiver<bool>,
) -> Option<MailRuntime> {
    let mail = config.mail.as_ref()?;
    let password = secrets.mail_password.as_ref()?;
    let settings = Arc::new(JmapSettings {
        base_url: mail.jmap_url.clone(),
        username: mail.username.clone(),
        password: copy_secret(password),
        timeout: Duration::from_secs(30),
    });
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let source = Arc::new(ResilientMailSource::new(
        Arc::clone(&settings),
        Arc::<PgStore>::clone(store),
        Arc::clone(&clock),
        mail.backfill.0,
        mail.batch_size,
    ));
    let ingestor = Arc::new(Ingestor::new(
        source,
        Arc::<PgStore>::clone(store),
        Arc::<PgStore>::clone(store),
        clock,
        IngestConfig {
            allowed_senders: mail.allowed_senders.clone(),
            max_links_per_message: mail.max_links_per_message,
        },
    ));
    let push = mail
        .push
        .then(|| PushWatcher::start(settings, PushConfig::default(), shutdown));
    stats.enable();
    Some(MailRuntime {
        ingestor,
        push,
        poll: mail.poll_interval.0,
        stats,
    })
}

/// Poll the mailbox until `shutdown`: straight away while mail keeps arriving, then every
/// `poll`, or sooner when push announces new mail. Failures back off.
pub async fn run_ingest(mail: MailRuntime, mut shutdown: watch::Receiver<bool>) {
    let mut error_backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            return;
        }
        let pause = match mail.ingestor.run_once().await {
            Ok(report) => {
                error_backoff = Duration::from_secs(1);
                mail.stats
                    .record_poll(&report, fanfold_app::ports::Clock::now(&SystemClock));
                if report.messages > 0 {
                    tracing::info!(
                        messages = report.messages,
                        queued = report.enqueued,
                        already_queued = report.already_active,
                        other_senders = report.from_unlisted_sender,
                        "read new mail"
                    );
                    // There may be more waiting; ask again before sleeping.
                    continue;
                }
                mail.poll
            }
            Err(e) => {
                mail.stats.record_failure();
                tracing::error!(error = %e, retry_in = ?error_backoff, "mail poll failed");
                let pause = error_backoff;
                error_backoff = (error_backoff * 2).min(MAX_ERROR_BACKOFF);
                pause
            }
        };
        tokio::select! {
            // Push: new mail was announced (or the stream just reconnected).
            () = async {
                match &mail.push {
                    Some(push) => push.changed().await,
                    None => std::future::pending().await,
                }
            } => {}
            () = tokio::time::sleep(pause) => {}
            () = async { let _ = shutdown.wait_for(|stop| *stop).await; } => return,
        }
    }
}

// -------------------------------------------------------------------------- web

/// The two HTTP listeners, once bound.
#[derive(Debug)]
pub struct WebServers {
    pub ui_addr: std::net::SocketAddr,
    pub ops_addr: std::net::SocketAddr,
    tasks: JoinSet<()>,
}

impl WebServers {
    /// Wait for both servers to finish (they stop when `shutdown` flips to `true`).
    pub async fn join(mut self) {
        while self.tasks.join_next().await.is_some() {}
    }
}

async fn serve(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    mut shutdown: watch::Receiver<bool>,
) {
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.wait_for(|stop| *stop).await;
        })
        .await;
    if let Err(e) = result {
        tracing::error!(error = %e, "http server failed");
    }
}

/// Bind the UI and ops listeners and start serving. Returns once both are bound, so
/// a port that is already taken fails startup instead of failing silently later.
pub async fn start_web(
    config: &Config,
    store: &Arc<PgStore>,
    worker: &Arc<Worker>,
    events: Arc<dyn EventSource>,
    mail_stats: Arc<MailStats>,
    shutdown: watch::Receiver<bool>,
) -> Result<WebServers, StartupError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let deps = Arc::new(WebDeps {
        jobs: Arc::<PgStore>::clone(store),
        catalog: Arc::<PgStore>::clone(store),
        metadata: Arc::<PgStore>::clone(store),
        listing: Arc::<PgStore>::clone(store),
        // The library's search index lives in memory, rebuilt when the library changes.
        browser: Arc::new(fanfold_app::browse::LibraryBrowser::new(
            Arc::<PgStore>::clone(store),
            Arc::new(fanfold_adapter_search::TantivyIndexer),
        )),
        admin: Arc::new(JobAdmin::new(
            Arc::<PgStore>::clone(store),
            Arc::clone(&clock),
        )),
        library: Arc::new(fanfold_app::library::LibraryAdmin::new(
            Arc::<PgStore>::clone(store),
            Arc::<PgStore>::clone(store),
            Arc::new(FsLibrary::new(config.library.path.clone())),
            Arc::<PgStore>::clone(store),
            Arc::clone(&clock),
        )),
        events,
        site: Arc::<Worker>::clone(worker),
        clock,
        stats: worker.stats(),
        mail: mail_stats,
        readiness: Arc::<PgStore>::clone(store),
        shutdown: shutdown.clone(),
        max_attempts: config.retry.max_attempts,
        page_size: config.web.page_size,
        moon_reader: config.web.moon_reader_button,
        // Releases are git tags (vX.Y.Z), not Cargo.toml bumps: the image build passes the
        // tag's version in, and a local build falls back to the manifest's.
        version: option_env!("FANFOLD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")),
    });

    let bind = |addr: std::net::SocketAddr| async move {
        tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| StartupError::Io(format!("cannot listen on {addr}: {e}")))
    };
    let ui = bind(config.web.listen).await?;
    let ops = bind(config.web.ops_listen).await?;
    let ui_addr = ui
        .local_addr()
        .map_err(|e| StartupError::Io(e.to_string()))?;
    let ops_addr = ops
        .local_addr()
        .map_err(|e| StartupError::Io(e.to_string()))?;

    let mut tasks = JoinSet::new();
    tasks.spawn(serve(ui, ui_router(Arc::clone(&deps)), shutdown.clone()));
    tasks.spawn(serve(ops, ops_router(deps), shutdown));
    tracing::info!(%ui_addr, %ops_addr, "web listening");
    Ok(WebServers {
        ui_addr,
        ops_addr,
        tasks,
    })
}

#[cfg(test)]
#[allow(clippy::clone_on_ref_ptr)]
mod tests {
    use fanfold_app::{
        error::RepoError,
        ingest::{IngestConfig, Ingestor},
        ports::{Checkpoint, JobRepository, MailBatch, MailMessage, NotifyEvent},
        stats::MailStats,
        worker::{Ports, Worker, WorkerConfig},
    };
    use fanfold_domain::{
        failure::FailureKind,
        job::{JobSource, JobState},
        work::WorkId,
    };
    use fanfold_testkit::memory::{
        FakeClock, FixedJitter, InMemoryJobs, InMemoryLibrary, InMemoryMail, RecordingNotifier,
        ScriptedDownloader,
    };

    use super::*;

    struct Rig {
        jobs: Arc<InMemoryJobs>,
        downloader: Arc<ScriptedDownloader>,
        notifier: Arc<RecordingNotifier>,
        worker: Arc<Worker>,
    }

    fn rig() -> Rig {
        let jobs = Arc::new(InMemoryJobs::new());
        let downloader = Arc::new(ScriptedDownloader::new());
        let notifier = Arc::new(RecordingNotifier::new());
        let worker = Arc::new(Worker::new(
            Ports {
                jobs: jobs.clone(),
                catalog: jobs.clone(),
                metadata: jobs.clone(),
                downloader: downloader.clone(),
                library: Arc::new(InMemoryLibrary::new()),
                finisher: Arc::new(fanfold_testkit::memory::RecordingFinisher::new()),
                notifier: notifier.clone(),
                clock: Arc::new(SystemClock),
                jitter: Arc::new(FixedJitter(0.5)),
            },
            WorkerConfig {
                retry: RetryPolicy {
                    max_attempts: 3,
                    base: Duration::from_secs(60),
                    cap: Duration::from_secs(3600),
                },
                breaker: BreakerConfig {
                    failure_threshold: 3,
                    open_for: Duration::from_secs(60),
                    max_open_for: Duration::from_secs(600),
                },
            },
        ));
        let _ = FakeClock::at_secs(0);
        Rig {
            jobs,
            downloader,
            notifier,
            worker,
        }
    }

    async fn enqueue(rig: &Rig, work: u64) {
        rig.jobs
            .enqueue(
                WorkId::new(work).unwrap().into(),
                JobSource::Manual,
                SystemClock.now_for_tests(),
            )
            .await
            .unwrap();
    }

    trait NowForTests {
        fn now_for_tests(&self) -> fanfold_domain::Timestamp;
    }
    impl NowForTests for SystemClock {
        fn now_for_tests(&self) -> fanfold_domain::Timestamp {
            use fanfold_app::ports::Clock as _;
            self.now()
        }
    }

    fn mail_runtime(
        mail: &Arc<InMemoryMail>,
        jobs: &Arc<InMemoryJobs>,
        stats: &Arc<MailStats>,
    ) -> MailRuntime {
        MailRuntime {
            ingestor: Arc::new(Ingestor::new(
                mail.clone(),
                jobs.clone(),
                jobs.clone(),
                Arc::new(FakeClock::at_secs(1_000)),
                IngestConfig {
                    allowed_senders: vec!["archiveofourown.org".into()],
                    max_links_per_message: 20,
                },
            )),
            push: None,
            poll: Duration::from_secs(60),
            stats: stats.clone(),
        }
    }

    fn batch(checkpoint: &str, works: &[u64]) -> MailBatch {
        MailBatch {
            messages: works
                .iter()
                .map(|w| MailMessage {
                    id: format!("m{w}"),
                    from: "Archive of Our Own <do-not-reply_at_archiveofourown_org_x@relay>".into(),
                    subject: "[AO3] posted".into(),
                    text: format!("https://archiveofourown.org/works/{w}"),
                    html: String::new(),
                })
                .collect(),
            checkpoint: Checkpoint(checkpoint.into()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_mail_loop_reads_new_mail_promptly_then_idles_and_stops() {
        let (mail, jobs, stats) = (
            Arc::new(InMemoryMail::new()),
            Arc::new(InMemoryJobs::new()),
            Arc::new(MailStats::new()),
        );
        mail.push(batch("cp1", &[10, 20]));
        mail.push(batch("cp2", &[30]));
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run_ingest(mail_runtime(&mail, &jobs, &stats), stopped));

        // Both batches are drained at once (mail was waiting), without sleeping between them.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(jobs.all_jobs().len(), 3);
        assert_eq!(mail.pending_batches(), 0);
        let s = stats.snapshot();
        assert_eq!((s.messages, s.enqueued, s.polls_failed), (3, 3, 0));

        // Later mail is found on the next poll interval.
        mail.push(batch("cp3", &[40]));
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(jobs.all_jobs().len(), 4);
        assert!(stats.snapshot().last_success.is_some());

        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("stops promptly")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_mail_loop_backs_off_on_errors_and_recovers() {
        let (mail, jobs, stats) = (
            Arc::new(InMemoryMail::new()),
            Arc::new(InMemoryJobs::new()),
            Arc::new(MailStats::new()),
        );
        mail.break_fetch(true);
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run_ingest(mail_runtime(&mail, &jobs, &stats), stopped));

        tokio::time::sleep(Duration::from_secs(30)).await;
        let failures = stats.snapshot().polls_failed;
        assert!(
            (3..=7).contains(&failures),
            "backoff 1, 2, 4, 8, 16 s: {failures} failures in 30 s"
        );

        mail.break_fetch(false);
        mail.push(batch("cp1", &[50]));
        tokio::time::sleep(Duration::from_secs(70)).await;
        assert_eq!(jobs.all_jobs().len(), 1, "it recovers by itself");
        assert!(stats.snapshot().polls_ok >= 1);

        stop.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_drains_the_queue_then_idles_and_stops_on_shutdown() {
        let rig = rig();
        for n in 1..=3 {
            enqueue(&rig, n).await;
            rig.downloader.push_ok(&format!("Story {n}"));
        }
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(run_workers(
            Arc::clone(&rig.worker),
            Duration::from_secs(5),
            2,
            rx,
        ));

        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(
            rig.jobs
                .all_jobs()
                .iter()
                .all(|j| j.state == JobState::Succeeded)
        );
        assert_eq!(rig.notifier.events().len(), 3);

        // A job arriving later is picked up on the next poll.
        enqueue(&rig, 4).await;
        rig.downloader.push_ok("Story 4");
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(rig.notifier.events().len(), 4);

        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("loops must stop promptly")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn repository_errors_back_off_instead_of_spinning() {
        let rig = rig();
        rig.jobs
            .break_with(Some(RepoError::Backend("db down".into())));
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(run_workers(
            Arc::clone(&rig.worker),
            Duration::from_secs(5),
            1,
            rx,
        ));
        tokio::time::sleep(Duration::from_secs(10)).await;

        rig.jobs.break_with(None);
        enqueue(&rig, 1).await;
        rig.downloader.push_ok("Recovered");
        tokio::time::sleep(Duration::from_secs(90)).await;
        assert_eq!(
            rig.jobs.all_jobs()[0].state,
            JobState::Succeeded,
            "the loop survives an outage and resumes"
        );
        tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_open_breaker_pauses_without_burning_the_poll_loop() {
        let rig = rig();
        for n in 1..=4 {
            enqueue(&rig, n).await;
            rig.downloader.push_err(FailureKind::SiteUnavailable, "525");
        }
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(run_workers(
            Arc::clone(&rig.worker),
            Duration::from_secs(5),
            1,
            rx,
        ));
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(
            rig.downloader.requests().len(),
            3,
            "the breaker stopped further requests"
        );
        assert!(rig.notifier.events().iter().any(|e| matches!(
            e,
            NotifyEvent::SiteDegraded {
                kind: FailureKind::SiteUnavailable,
                ..
            }
        )));
        tx.send(true).unwrap();
        task.await.unwrap();
    }
}
