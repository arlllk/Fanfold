//! Declarative, strictly validated configuration.
//!
//! One TOML file (a Kubernetes ConfigMap), read once at startup. Unknown keys are
//! errors, every value is range-checked, and *all* problems are reported together so
//! a bad config fails the pod once, loudly, with the complete list. There is no
//! runtime editing: change the file and roll the pod. Secrets are references, see
//! [`crate::secrets`].

use std::{path::PathBuf, str::FromStr, time::Duration};

use fanfold_adapter_web::views::library::MoonReaderButton;
use secrecy::SecretString;
use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::secrets::{SecretError, SecretRef, SecretSource};

// ------------------------------------------------------------------- durations

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "invalid duration {0:?}: use a whole number with a unit, e.g. \"500ms\", \"30s\", \"5m\", \"2h\", \"1d\""
)]
pub struct DurationError(pub &'static str);

/// Parse `"90s"`, `"5m"`, `"2h"`, `"1d"`, `"250ms"`.
pub fn parse_duration(input: &str) -> Result<Duration, DurationError> {
    const BAD: DurationError = DurationError("");
    let input = input.trim();
    let split = input.find(|c: char| !c.is_ascii_digit()).ok_or(BAD)?;
    let (digits, unit) = input.split_at(split);
    let n: u64 = digits.parse().map_err(|_| BAD)?;
    let millis_per_unit: u64 = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(BAD),
    };
    n.checked_mul(millis_per_unit)
        .map(Duration::from_millis)
        .ok_or(BAD)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dur(pub Duration);

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_duration(&text).map(Dur).map_err(|_| {
            serde::de::Error::custom(format!(
                "invalid duration {text:?}: use e.g. \"30s\", \"5m\", \"2h\""
            ))
        })
    }
}

// -------------------------------------------------------------------- sections

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub host: String,
    #[serde(default = "default_pg_port")]
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: SecretRef,
    #[serde(default = "default_ssl_mode")]
    pub ssl_mode: SslMode,
    #[serde(default = "default_pool")]
    pub max_connections: u32,
}

const fn default_pg_port() -> u16 {
    5432
}
const fn default_ssl_mode() -> SslMode {
    SslMode::Require
}
const fn default_pool() -> u32 {
    5
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryConfig {
    /// The folder Syncthing replicates.
    pub path: PathBuf,
    /// Per-download scratch space; need not be on the same filesystem.
    #[serde(default = "default_scratch")]
    pub scratch: PathBuf,
}

fn default_scratch() -> PathBuf {
    PathBuf::from("/tmp/fanfold")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ao3Config {
    pub username: SecretRef,
    pub password: SecretRef,
}

/// Send every request to AO3 through fanfold-browser (one browser that keeps the only cookies
/// and paces the requests).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    /// Origin of the service, e.g. `http://fanfold-browser:8191`.
    pub url: url::Url,
    /// How long one request may take there, waiting in its queue included. Must be shorter than
    /// `downloader.timeout`.
    #[serde(default = "default_browser_timeout")]
    pub timeout: Dur,
}

const fn default_browser_timeout() -> Dur {
    Dur(Duration::from_secs(180))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloaderConfig {
    pub python: String,
    pub timeout: Dur,
    /// Optional. See [`BrowserConfig`].
    pub browser: Option<BrowserConfig>,
    /// Where the rendered `personal.ini` is written. Point this at a memory-backed
    /// `emptyDir` so credentials never touch disk.
    pub personal_ini: PathBuf,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            python: "python3".into(),
            timeout: Dur(Duration::from_secs(600)),
            browser: None,
            personal_ini: PathBuf::from("/run/fanfold/personal.ini"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base: Dur,
    pub cap: Dur,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base: Dur(Duration::from_secs(60)),
            cap: Dur(Duration::from_secs(3600)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BreakerConfig {
    pub failure_threshold: u32,
    pub open_for: Dur,
    pub max_open_for: Dur,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            open_for: Dur(Duration::from_secs(120)),
            max_open_for: Dur(Duration::from_secs(3600)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    /// How long to sleep when there is nothing to do.
    pub poll_interval: Dur,
    /// Parallel download loops. AO3 is rate-limited: keep this at 1 unless you know better.
    pub concurrency: u32,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Dur(Duration::from_secs(5)),
            concurrency: 1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// The user interface. Put the gateway's authentication in front of it: it has no
    /// login of its own.
    pub listen: std::net::SocketAddr,
    /// Health probes and metrics, unauthenticated: keep this port internal.
    pub ops_listen: std::net::SocketAddr,
    /// Jobs per page on the history view.
    pub page_size: u32,
    /// When book pages offer "Open in Moon+ Reader" (Android, through fanfold-moonbridge).
    pub moon_reader_button: MoonReaderButton,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: ([0, 0, 0, 0], 8080).into(),
            ops_listen: ([0, 0, 0, 0], 9090).into(),
            page_size: 20,
            moon_reader_button: MoonReaderButton::default(),
        }
    }
}

/// Reading AO3 notification mail over JMAP. Optional: without it works are queued by hand
/// (the UI form, `fanfold enqueue`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    /// Origin the JMAP session is fetched from, e.g. `https://mail.example.org`. The
    /// server then advertises where its API lives; that address must be reachable from
    /// here (resolve the server's public name to it).
    pub jmap_url: url::Url,
    pub username: String,
    pub password: SecretRef,
    /// Case-insensitive substrings of the sender (`Name <address>`); punctuation is
    /// ignored, so relay-rewritten addresses still match. Empty allows any sender.
    #[serde(default = "default_senders")]
    pub allowed_senders: Vec<String>,
    /// How often to poll when push is off or silent. Push wakes the poll earlier.
    #[serde(default = "default_mail_poll")]
    pub poll_interval: Dur,
    /// On the first run, how far back to read mail.
    #[serde(default = "default_backfill")]
    pub backfill: Dur,
    #[serde(default = "default_batch")]
    pub batch_size: u32,
    /// Cap per message, so one runaway email cannot flood the queue.
    #[serde(default = "default_max_links")]
    pub max_links_per_message: usize,
    /// Wake the poll as soon as the server announces new mail.
    #[serde(default = "default_true")]
    pub push: bool,
}

fn default_senders() -> Vec<String> {
    vec!["archiveofourown.org".to_owned()]
}
const fn default_mail_poll() -> Dur {
    Dur(Duration::from_secs(60))
}
const fn default_backfill() -> Dur {
    Dur(Duration::from_hours(7 * 24))
}
const fn default_batch() -> u32 {
    200
}
const fn default_max_links() -> usize {
    20
}
const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    /// Downloads and series. Optional: leave it out to post failures only.
    #[serde(default)]
    pub webhook: Option<SecretRef>,
    /// Gave up, AO3 unhealthy, AO3 back: another channel, perhaps muted. Without it, failures
    /// are not posted.
    #[serde(default)]
    pub failures_webhook: Option<SecretRef>,
    /// The web interface's address as you open it (e.g. `https://fanfold.example.org`):
    /// notifications then link to their job.
    #[serde(default)]
    pub ui_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database: DatabaseConfig,
    pub library: LibraryConfig,
    /// AO3 login. Required unless `downloader.browser` is set; through the browser it is optional
    /// and used only when a work asks for a login (restricted works).
    pub ao3: Option<Ao3Config>,
    #[serde(default)]
    pub downloader: DownloaderConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub breaker: BreakerConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub web: WebConfig,
    pub mail: Option<MailConfig>,
    pub discord: Option<DiscordConfig>,
}

// ---------------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("config file is not valid: {0}")]
    Parse(String),
    #[error("invalid configuration:\n{}", format_list(.0))]
    Invalid(Vec<String>),
    #[error("cannot resolve secrets:\n{}", format_list(.0))]
    Secrets(Vec<String>),
}

fn format_list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("  - {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct Ao3Credentials {
    pub username: SecretString,
    pub password: SecretString,
}

impl std::fmt::Debug for Ao3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ao3Credentials").finish_non_exhaustive()
    }
}

/// Every secret, resolved.
pub struct Secrets {
    pub database_password: SecretString,
    /// `None` without an `[ao3]` table (through fanfold-browser, public works only).
    pub ao3: Option<Ao3Credentials>,
    pub discord_webhook: Option<SecretString>,
    pub discord_failures_webhook: Option<SecretString>,
    pub mail_password: Option<SecretString>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets").finish_non_exhaustive()
    }
}

impl FromStr for Config {
    type Err = ConfigError;

    fn from_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }
}

impl Config {
    // A flat list of independent checks: splitting it would only scatter them.
    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), ConfigError> {
        let mut problems = Vec::new();
        let mut require = |ok: bool, message: &str| {
            if !ok {
                problems.push(message.to_owned());
            }
        };
        let db = &self.database;
        require(
            !db.host.trim().is_empty(),
            "database.host must not be empty",
        );
        require(
            !db.name.trim().is_empty(),
            "database.name must not be empty",
        );
        require(
            !db.user.trim().is_empty(),
            "database.user must not be empty",
        );
        require(db.port != 0, "database.port must not be 0");
        require(
            (1..=100).contains(&db.max_connections),
            "database.max_connections must be 1-100",
        );
        require(
            self.library.path.is_absolute(),
            "library.path must be an absolute path",
        );
        require(
            self.library.scratch.is_absolute(),
            "library.scratch must be an absolute path",
        );
        require(
            !self.downloader.python.trim().is_empty(),
            "downloader.python must not be empty",
        );
        require(
            !self.downloader.timeout.0.is_zero(),
            "downloader.timeout must be greater than 0",
        );
        require(
            self.downloader.personal_ini.is_absolute(),
            "downloader.personal_ini must be an absolute path",
        );
        require(
            self.retry.max_attempts >= 1,
            "retry.max_attempts must be at least 1",
        );
        require(
            !self.retry.base.0.is_zero(),
            "retry.base must be greater than 0",
        );
        require(
            self.retry.cap.0 >= self.retry.base.0,
            "retry.cap must be at least retry.base",
        );
        if let Some(discord) = &self.discord {
            require(
                discord.webhook.is_some() || discord.failures_webhook.is_some(),
                "discord needs webhook, failures_webhook, or both",
            );
            require(
                discord.ui_url.as_deref().is_none_or(|u| {
                    url::Url::parse(u)
                        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.has_host())
                }),
                "discord.ui_url must be an http(s) URL with a host",
            );
        }
        require(
            self.breaker.failure_threshold >= 1,
            "breaker.failure_threshold must be at least 1",
        );
        require(
            !self.breaker.open_for.0.is_zero(),
            "breaker.open_for must be greater than 0",
        );
        require(
            self.breaker.max_open_for.0 >= self.breaker.open_for.0,
            "breaker.max_open_for must be at least breaker.open_for",
        );
        require(
            !self.worker.poll_interval.0.is_zero(),
            "worker.poll_interval must be greater than 0",
        );
        require(
            (1..=16).contains(&self.worker.concurrency),
            "worker.concurrency must be 1-16",
        );
        require(
            self.web.listen != self.web.ops_listen || self.web.listen.port() == 0,
            "web.listen and web.ops_listen must be different addresses",
        );
        require(
            (5..=100).contains(&self.web.page_size),
            "web.page_size must be 5-100",
        );
        require(
            self.ao3.is_some() || self.downloader.browser.is_some(),
            "[ao3] is required unless downloader.browser is set",
        );
        if let Some(browser) = &self.downloader.browser {
            let url = &browser.url;
            require(
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
                "downloader.browser.url must be an http(s) URL with a host",
            );
            require(
                matches!(url.path(), "" | "/") && url.query().is_none(),
                "downloader.browser.url must be just an origin (no path or query)",
            );
            require(
                url.username().is_empty() && url.password().is_none(),
                "downloader.browser.url cannot carry credentials",
            );
            require(
                !browser.timeout.0.is_zero() && browser.timeout.0 < self.downloader.timeout.0,
                "downloader.browser.timeout must be greater than 0 and shorter than downloader.timeout",
            );
        }
        if let Some(mail) = &self.mail {
            require(
                matches!(mail.jmap_url.scheme(), "http" | "https")
                    && mail.jmap_url.host_str().is_some(),
                "mail.jmap_url must be an http(s) URL with a host",
            );
            require(
                !mail.username.trim().is_empty(),
                "mail.username must not be empty",
            );
            require(
                !mail.poll_interval.0.is_zero(),
                "mail.poll_interval must be greater than 0",
            );
            require(
                !mail.backfill.0.is_zero(),
                "mail.backfill must be greater than 0",
            );
            require(
                (1..=500).contains(&mail.batch_size),
                "mail.batch_size must be 1-500",
            );
            require(
                (1..=200).contains(&mail.max_links_per_message),
                "mail.max_links_per_message must be 1-200",
            );
            require(
                mail.allowed_senders.iter().all(|s| !s.trim().is_empty()),
                "mail.allowed_senders must not contain empty entries",
            );
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }

    /// Fetch every referenced secret, reporting all failures at once.
    pub fn resolve_secrets(&self, source: &dyn SecretSource) -> Result<Secrets, ConfigError> {
        let mut problems = Vec::new();
        let mut get = |what: &str, secret: &SecretRef| match secret.resolve(source) {
            Ok(value) => Some(value),
            Err(e) => {
                problems.push(format!("{what} ({}): {e}", secret.describe()));
                None
            }
        };
        let database_password = get("database.password", &self.database.password);
        let ao3 = self.ao3.as_ref().map(|a| {
            (
                get("ao3.username", &a.username),
                get("ao3.password", &a.password),
            )
        });
        // `Some(None)`: no credentials configured (fine). `None`: configured but unresolved.
        let ao3 = match ao3 {
            None => Some(None),
            Some((Some(username), Some(password))) => {
                Some(Some(Ao3Credentials { username, password }))
            }
            Some(_) => None,
        };
        let discord_webhook = self
            .discord
            .as_ref()
            .and_then(|d| d.webhook.as_ref())
            .map(|w| get("discord.webhook", w));
        let discord_failures_webhook = self
            .discord
            .as_ref()
            .and_then(|d| d.failures_webhook.as_ref())
            .map(|w| get("discord.failures_webhook", w));
        let mail_password = self
            .mail
            .as_ref()
            .map(|m| get("mail.password", &m.password));
        match (database_password, ao3, discord_webhook, mail_password) {
            (Some(database_password), Some(ao3), webhook, mail) if problems.is_empty() => {
                Ok(Secrets {
                    database_password,
                    ao3,
                    discord_webhook: webhook.flatten(),
                    discord_failures_webhook: discord_failures_webhook.flatten(),
                    mail_password: mail.flatten(),
                })
            }
            _ => Err(ConfigError::Secrets(problems)),
        }
    }
}

/// Re-export so callers can name the error without importing `secrets`.
pub type SecretResolveError = SecretError;

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, io, path::Path};

    use proptest::prelude::*;
    use secrecy::ExposeSecret;

    use super::*;

    const MINIMAL: &str = r#"
[database]
host = "postgres-rw.postgres.svc"
name = "fanfold"
user = "fanfold"
password = { file = "/run/secrets/db/password" }

[library]
path = "/library"

[ao3]
username = { env = "AO3_USERNAME" }
password = { env = "AO3_PASSWORD" }
"#;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        text.parse()
    }

    fn invalid(text: &str) -> Vec<String> {
        match parse(text) {
            Err(ConfigError::Invalid(p)) => p,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// The documented example must stay valid, or the docs rot.
    #[test]
    fn the_example_config_parses_and_resolves() {
        let config: Config = include_str!("../../../config/config.example.toml")
            .parse()
            .unwrap();
        let mut fake = FakeSecrets::default();
        for name in [
            "AO3_USERNAME",
            "AO3_PASSWORD",
            "DISCORD_WEBHOOK_URL",
            "DISCORD_FAILURES_WEBHOOK_URL",
            "MAIL_PASSWORD",
        ] {
            fake.env.insert(name.into(), "value".into());
        }
        fake.files
            .insert("/run/secrets/postgres/password".into(), "pw\n".into());
        assert!(
            config
                .resolve_secrets(&fake)
                .unwrap()
                .discord_webhook
                .is_some()
        );
    }

    #[test]
    fn minimal_config_gets_sensible_defaults() {
        let c = parse(MINIMAL).unwrap();
        assert_eq!(
            (
                c.database.port,
                c.database.ssl_mode,
                c.database.max_connections
            ),
            (5432, SslMode::Require, 5)
        );
        assert_eq!(c.retry.max_attempts, 5);
        assert_eq!(c.breaker.failure_threshold, 3);
        assert_eq!(c.worker.concurrency, 1);
        assert_eq!(c.downloader.timeout.0, Duration::from_secs(600));
        assert!(c.discord.is_none());
    }

    fn via_browser(extra: &str) -> String {
        let without_ao3 = MINIMAL.split("[ao3]").next().unwrap();
        format!(
            "{without_ao3}\n[downloader.browser]\nurl = \"http://fanfold-browser:8191\"\n{extra}"
        )
    }

    #[test]
    fn the_browser_service_needs_no_login_and_defaults_its_timeout() {
        let c = parse(&via_browser("")).unwrap();
        let browser = c.downloader.browser.unwrap();
        assert_eq!(browser.url.as_str(), "http://fanfold-browser:8191/");
        assert_eq!(browser.timeout.0, Duration::from_secs(180));
        assert!(c.ao3.is_none());
    }

    #[test]
    fn the_browser_service_settings_are_checked() {
        for (text, problem) in [
            (
                via_browser("timeout = \"20m\""),
                "shorter than downloader.timeout",
            ),
            (
                via_browser("").replace("http://fanfold-browser:8191", "http://u:p@b:8191"),
                "credentials",
            ),
            (
                via_browser("").replace("http://fanfold-browser:8191", "http://b:8191/v1"),
                "just an origin",
            ),
        ] {
            let problems = invalid(&text);
            assert!(
                problems.iter().any(|p| p.contains(problem)),
                "{problem}: {problems:?}"
            );
        }
    }

    /// `FlareSolverr` and the proxy are gone: a config that still names them is refused loudly
    /// instead of being half-applied.
    #[test]
    fn the_removed_flaresolverr_and_proxy_settings_are_refused() {
        for old in [
            "[downloader.flaresolverr]\nurl = \"http://fs:8191\"\n",
            "[downloader]\nproxy = \"http://home:8080\"\n",
        ] {
            assert!(
                matches!(
                    parse(&format!("{MINIMAL}\n{old}")),
                    Err(ConfigError::Parse(_))
                ),
                "{old}"
            );
        }
    }

    #[test]
    fn without_the_browser_the_ao3_login_is_required() {
        let without_ao3 = MINIMAL.split("[ao3]").next().unwrap();
        let problems = invalid(without_ao3);
        assert_eq!(
            problems,
            ["[ao3] is required unless downloader.browser is set"]
        );
    }

    #[test]
    fn a_literal_secret_is_a_parse_error_not_an_accepted_value() {
        let leaked = MINIMAL.replace(r#"{ file = "/run/secrets/db/password" }"#, r#""hunter2""#);
        let err = parse(&leaked).unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "{err:?}");
        assert!(
            !err.to_string().contains("hunter2") || err.to_string().contains("expected"),
            "{err}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected_so_typos_fail_loudly() {
        for extra in [
            "[downloader]\ntimout = \"5m\"\n",
            "[retry]\nmax_attemps = 3\n",
            "[web]\nport = 1\n",
        ] {
            let err = parse(&format!("{MINIMAL}\n{extra}")).unwrap_err();
            assert!(matches!(err, ConfigError::Parse(_)), "{extra}: {err:?}");
        }
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let text = format!(
            "{MINIMAL}\n[retry]\nmax_attempts = 0\nbase = \"10m\"\ncap = \"1m\"\n[breaker]\nfailure_threshold = 0\n[worker]\nconcurrency = 99\n"
        )
        .replace(r#"path = "/library""#, r#"path = "relative/path""#);
        let problems = invalid(&text);
        assert_eq!(problems.len(), 5, "{problems:#?}");
        assert!(problems.iter().any(|p| p.contains("library.path")));
        assert!(problems.iter().any(|p| p.contains("retry.max_attempts")));
        assert!(problems.iter().any(|p| p.contains("retry.cap")));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("breaker.failure_threshold"))
        );
        assert!(problems.iter().any(|p| p.contains("worker.concurrency")));
    }

    #[test]
    fn durations() {
        for (text, expected) in [
            ("250ms", 250),
            ("30s", 30_000),
            ("5m", 300_000),
            ("2h", 7_200_000),
            ("1d", 86_400_000),
            ("0s", 0),
        ] {
            assert_eq!(
                parse_duration(text).unwrap(),
                Duration::from_millis(expected),
                "{text}"
            );
        }
        for bad in [
            "",
            "s",
            "5",
            "5x",
            "-5s",
            "1.5s",
            "5 m",
            "99999999999999999999s",
            "18446744073709551615d",
            "m5",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn web_defaults_and_validation() {
        let c = parse(MINIMAL).unwrap();
        assert_eq!(c.web.listen.port(), 8080);
        assert_eq!(c.web.ops_listen.port(), 9090);
        assert_eq!(c.web.page_size, 20);

        let custom = format!(
            "{MINIMAL}\n[web]\nlisten = \"127.0.0.1:3000\"\nops_listen = \"127.0.0.1:3001\"\npage_size = 50\n"
        );
        let c = parse(&custom).unwrap();
        assert_eq!((c.web.listen.port(), c.web.page_size), (3000, 50));

        let same =
            format!("{MINIMAL}\n[web]\nlisten = \"0.0.0.0:80\"\nops_listen = \"0.0.0.0:80\"\n");
        assert!(
            invalid(&same)
                .iter()
                .any(|p| p.contains("must be different"))
        );
        // Port 0 means "any free port", so two of them do not clash.
        let ephemeral =
            format!("{MINIMAL}\n[web]\nlisten = \"127.0.0.1:0\"\nops_listen = \"127.0.0.1:0\"\n");
        assert!(parse(&ephemeral).is_ok());
        for bad in ["page_size = 0", "page_size = 4", "page_size = 101"] {
            let problems = invalid(&format!("{MINIMAL}\n[web]\n{bad}\n"));
            assert!(
                problems.iter().any(|p| p.contains("web.page_size")),
                "{bad}: {problems:?}"
            );
        }
        for bad in [
            "listen = \"nonsense\"",
            "listen = 8080",
            "listen = \"localhost:80\"",
        ] {
            assert!(
                matches!(
                    parse(&format!("{MINIMAL}\n[web]\n{bad}\n")),
                    Err(ConfigError::Parse(_))
                ),
                "{bad}"
            );
        }
    }

    const MAIL: &str = r#"
[mail]
jmap_url = "https://mail.example.org"
username = "fanfold@example.org"
password = { env = "MAIL_PASSWORD" }
"#;

    #[test]
    fn mail_is_optional_and_has_sensible_defaults() {
        assert!(parse(MINIMAL).unwrap().mail.is_none());
        let mail = parse(&format!("{MINIMAL}{MAIL}")).unwrap().mail.unwrap();
        assert_eq!(mail.jmap_url.as_str(), "https://mail.example.org/");
        assert_eq!(mail.allowed_senders, ["archiveofourown.org"]);
        assert_eq!(mail.poll_interval.0, Duration::from_secs(60));
        assert_eq!(mail.backfill.0, Duration::from_hours(7 * 24));
        assert_eq!(
            (mail.batch_size, mail.max_links_per_message, mail.push),
            (200, 20, true)
        );
    }

    #[test]
    fn mail_values_are_validated_together() {
        let bad = format!(
            "{MINIMAL}{MAIL}\nbatch_size = 0\nmax_links_per_message = 500\npoll_interval = \"0s\"\nbackfill = \"0s\"\nallowed_senders = [\"ok\", \"  \"]\n"
        );
        let problems = invalid(&bad);
        for key in [
            "batch_size",
            "max_links_per_message",
            "poll_interval",
            "backfill",
            "allowed_senders",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(key)),
                "{key}: {problems:?}"
            );
        }
        for url in ["ftp://x", "mailto:a@b", "not a url"] {
            let text = format!("{MINIMAL}{}", MAIL.replace("https://mail.example.org", url));
            assert!(parse(&text).is_err(), "{url}");
        }
        assert!(
            parse(&format!(
                "{MINIMAL}{}",
                MAIL.replace("fanfold@example.org", " ")
            ))
            .is_err(),
            "blank username"
        );
    }

    #[test]
    fn a_literal_mail_password_is_rejected_like_every_other_secret() {
        let leaked = MAIL.replace(r#"{ env = "MAIL_PASSWORD" }"#, r#""hunter2""#);
        assert!(matches!(
            parse(&format!("{MINIMAL}{leaked}")),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn the_mail_password_is_resolved_with_the_other_secrets() {
        let mut fake = FakeSecrets::default();
        fake.env.insert("AO3_USERNAME".into(), "u".into());
        fake.env.insert("AO3_PASSWORD".into(), "p".into());
        fake.files
            .insert("/run/secrets/db/password".into(), "db".into());
        let config = parse(&format!("{MINIMAL}{MAIL}")).unwrap();
        // Missing: reported together with everything else that is missing.
        let ConfigError::Secrets(problems) = config.resolve_secrets(&fake).err().unwrap() else {
            panic!("wrong error")
        };
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains("mail.password") && problems[0].contains("MAIL_PASSWORD"),
            "{problems:?}"
        );
        fake.env.insert("MAIL_PASSWORD".into(), "mailpw".into());
        let secrets = config.resolve_secrets(&fake).unwrap();
        assert_eq!(secrets.mail_password.unwrap().expose_secret(), "mailpw");
        assert!(
            parse(MINIMAL)
                .unwrap()
                .resolve_secrets(&fake)
                .unwrap()
                .mail_password
                .is_none()
        );
    }

    #[test]
    fn ssl_modes_use_kebab_case() {
        let text = MINIMAL.replace("[library]", "ssl_mode = \"verify-full\"\n\n[library]");
        assert_eq!(parse(&text).unwrap().database.ssl_mode, SslMode::VerifyFull);
    }

    #[derive(Default)]
    struct FakeSecrets {
        env: HashMap<String, String>,
        files: HashMap<std::path::PathBuf, String>,
    }

    impl SecretSource for FakeSecrets {
        fn env(&self, name: &str) -> Option<String> {
            self.env.get(name).cloned()
        }

        fn read_file(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    #[test]
    fn secrets_resolve_from_env_and_files() {
        let mut fake = FakeSecrets::default();
        fake.env.insert("AO3_USERNAME".into(), "alice".into());
        fake.env.insert("AO3_PASSWORD".into(), "pw".into());
        fake.files
            .insert("/run/secrets/db/password".into(), "dbpw\n".into());
        let secrets = parse(MINIMAL).unwrap().resolve_secrets(&fake).unwrap();
        assert_eq!(secrets.database_password.expose_secret(), "dbpw");
        assert_eq!(
            secrets.ao3.as_ref().unwrap().username.expose_secret(),
            "alice"
        );
        assert!(secrets.discord_webhook.is_none());
        assert!(secrets.discord_failures_webhook.is_none());
        assert!(!format!("{secrets:?}").contains("dbpw"));
    }

    #[test]
    fn missing_secrets_are_all_listed_and_name_only_their_origin() {
        let text = format!("{MINIMAL}\n[discord]\nwebhook = {{ env = \"DISCORD_URL\" }}\n");
        let err = parse(&text)
            .unwrap()
            .resolve_secrets(&FakeSecrets::default())
            .unwrap_err();
        let ConfigError::Secrets(problems) = err else {
            panic!("wrong error")
        };
        assert_eq!(problems.len(), 4, "{problems:#?}");
        assert!(problems.iter().any(|p| p.contains("env DISCORD_URL")));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("file /run/secrets/db/password"))
        );
    }

    proptest! {
        #[test]
        fn parsing_arbitrary_text_never_panics(s in "\\PC*") {
            let _ = parse(&s);
        }

        #[test]
        fn duration_roundtrip(n in 0u64..1_000_000, unit in prop::sample::select(vec![("ms", 1u64), ("s", 1000), ("m", 60_000), ("h", 3_600_000)])) {
            let d = parse_duration(&format!("{n}{}", unit.0)).unwrap();
            prop_assert_eq!(d, Duration::from_millis(n * unit.1));
        }

        #[test]
        fn duration_parsing_never_panics(s in "\\PC*") {
            let _ = parse_duration(&s);
        }
    }

    #[test]
    fn failures_can_have_their_own_discord_channel_or_be_the_only_one() {
        let both = format!(
            "{MINIMAL}\n[discord]\nwebhook = {{ env = \"MAIN\" }}\n\
             failures_webhook = {{ env = \"MUTED\" }}\nui_url = \"https://fanfold.example.org\"\n"
        );
        let config = parse(&both).unwrap();
        assert_eq!(
            config.discord.as_ref().unwrap().ui_url.as_deref(),
            Some("https://fanfold.example.org")
        );
        let only_failures =
            format!("{MINIMAL}\n[discord]\nfailures_webhook = {{ env = \"MUTED\" }}\n");
        assert!(
            parse(&only_failures)
                .unwrap()
                .discord
                .unwrap()
                .webhook
                .is_none()
        );
        assert!(matches!(
            parse(&format!("{MINIMAL}\n[discord]\n")),
            Err(ConfigError::Invalid(p)) if p.iter().any(|m| m.contains("failures_webhook"))
        ));
        assert!(matches!(
            parse(&format!(
                "{MINIMAL}\n[discord]\nwebhook = {{ env = \"MAIN\" }}\nui_url = \"ftp://x\"\n"
            )),
            Err(ConfigError::Invalid(p)) if p.iter().any(|m| m.contains("ui_url"))
        ));
    }
}
