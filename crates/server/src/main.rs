//! `fanfold-server`: wires config, secrets and adapters, then runs the worker.

use std::{io::Write as _, process::ExitCode, sync::Arc, time::Duration};

use fanfold_adapter_fanficfare::runner::TokioProcessRunner;
use fanfold_adapter_postgres::events::PgEvents;
use fanfold_app::{
    ports::{Clock, JobRepository},
    stats::MailStats,
};
use fanfold_domain::{job::JobSource, work::parse_work_url};
use fanfold_server::{
    cli::{Command, USAGE, parse_args},
    config::Config,
    infra::SystemClock,
    runtime::{
        StartupError, build_mail, build_worker, connect_store, run_ingest, run_workers, start_web,
    },
    secrets::SystemSecrets,
};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if std::env::var("FANFOLD_LOG_FORMAT").is_ok_and(|f| f == "json") {
        builder.json().init();
    } else {
        builder.init();
    }
}

fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn run() -> Result<(), StartupError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse_args(&args, std::env::var("FANFOLD_CONFIG").ok().as_deref())
        .map_err(|e| StartupError::Usage(format!("{e}\n{USAGE}")))?;

    let text = std::fs::read_to_string(&cli.config_path)
        .map_err(|e| StartupError::Io(format!("cannot read {}: {e}", cli.config_path.display())))?;
    let config: Config = text.parse()?;
    let secrets = config.resolve_secrets(&SystemSecrets)?;

    match cli.command {
        Command::CheckConfig => {
            say("configuration is valid and every secret resolves");
            Ok(())
        }
        Command::Enqueue(url) => {
            let work =
                parse_work_url(&url).map_err(|e| StartupError::Usage(format!("{url}: {e}")))?;
            let store = connect_store(&config, &secrets).await?;
            let outcome = store
                .enqueue(work, JobSource::Manual, SystemClock.now())
                .await?;
            say(&format!("{outcome:?}"));
            Ok(())
        }
        Command::Run => {
            let store = connect_store(&config, &secrets).await?;
            let worker = Arc::new(build_worker(
                &config,
                &secrets,
                &store,
                Arc::new(TokioProcessRunner),
            )?);
            worker
                .recover()
                .await
                .map_err(|e| StartupError::Io(e.to_string()))?;
            tracing::info!(concurrency = config.worker.concurrency, "worker started");

            let (stop, stopped) = watch::channel(false);
            let events = PgEvents::start(store.pool().clone(), stopped.clone()).await?;
            let mail_stats = Arc::new(MailStats::new());
            let mail = build_mail(
                &config,
                &secrets,
                &store,
                Arc::clone(&mail_stats),
                stopped.clone(),
            );
            if mail.is_none() {
                tracing::info!("no [mail] section: works are queued by hand");
            }
            let web = start_web(
                &config,
                &store,
                &worker,
                Arc::new(events),
                mail_stats,
                stopped.clone(),
            )
            .await?;
            let ingest = mail.map(|mail| tokio::spawn(run_ingest(mail, stopped.clone())));
            let workers = tokio::spawn(run_workers(
                Arc::clone(&worker),
                config.worker.poll_interval.0,
                config.worker.concurrency,
                stopped,
            ));
            shutdown_signal().await;
            tracing::info!("shutting down");
            let _ = stop.send(true);
            // Servers drain quickly; a download in flight is re-queued on the next start
            // if it does not finish within the grace period.
            let _ = tokio::time::timeout(Duration::from_secs(15), async {
                web.join().await;
                let _ = workers.await;
                if let Some(ingest) = ingest {
                    let _ = ingest.await;
                }
            })
            .await;
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
