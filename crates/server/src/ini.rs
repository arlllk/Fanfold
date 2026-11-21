//! Rendering FanFicFare's `personal.ini` from secrets.
//!
//! Behaviour verified against FanFicFare 4.63 (Python `ConfigParser` with basic
//! interpolation): a lone `%` raises an error and must be written `%%`; `:`, `#` and
//! `;` inside a value are fine; leading and trailing whitespace is stripped, so a
//! password that has any cannot be represented and is rejected rather than silently
//! altered; and a newline would start a new line of configuration (injection).

use std::{fmt::Write as _, io, path::Path};

use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IniError {
    #[error("{0} contains a newline or control character, which an ini file cannot hold")]
    ControlCharacter(&'static str),
    #[error("{0} has leading or trailing whitespace, which FanFicFare's config parser would strip")]
    SurroundingWhitespace(&'static str),
    #[error("{0} is empty")]
    Empty(&'static str),
}

fn check(name: &'static str, value: &str) -> Result<String, IniError> {
    if value.is_empty() {
        return Err(IniError::Empty(name));
    }
    if value.chars().any(char::is_control) {
        return Err(IniError::ControlCharacter(name));
    }
    if value.trim() != value {
        return Err(IniError::SurroundingWhitespace(name));
    }
    Ok(value.replace('%', "%%"))
}

/// Where `FlareSolverr` listens and how long it may take over one request.
#[derive(Debug, Clone, Copy)]
pub struct FlareSolverr<'a> {
    pub url: &'a url::Url,
    pub timeout: std::time::Duration,
}

/// How much longer FanFicFare waits for `FlareSolverr` than `FlareSolverr` waits for a page.
const FLARESOLVERR_MARGIN_S: u64 = 30;

/// FanFicFare's `[defaults]` for fetching through `FlareSolverr`. It builds
/// `protocol://address:port/v1` itself, so only the origin's parts are given.
fn flaresolverr_section(fs: &FlareSolverr<'_>) -> Result<String, IniError> {
    let host = fs
        .url
        .host_str()
        .ok_or(IniError::Empty("FlareSolverr host"))?;
    let host = check("FlareSolverr host", host)?;
    let port = fs
        .url
        .port_or_known_default()
        .ok_or(IniError::Empty("FlareSolverr port"))?;
    let millis = fs.timeout.as_millis();
    // FanFicFare waits this long for any answer, FlareSolverr's included. It must outlast
    // FlareSolverr's own limit, or a slow page ends as "connection to flaresolverr failed"
    // instead of FlareSolverr's real answer (FanFicFare's docs say the same).
    let wait = fs.timeout.as_secs() + FLARESOLVERR_MARGIN_S;
    // A session keeps one browser (and its cookies) across the requests of a download.
    Ok(format!(
        "[defaults]\nuse_flaresolverr_proxy:true\nuse_flaresolverr_session:true\n\
         flaresolverr_proxy_protocol:{}\nflaresolverr_proxy_address:{host}\n\
         flaresolverr_proxy_port:{port}\nflaresolverr_proxy_timeout:{millis}\n\
         connect_timeout:{wait}\n\n",
        fs.url.scheme()
    ))
}

/// The `personal.ini`: the `[archiveofourown.org]` section, with a login when there are
/// credentials, and `FlareSolverr`'s settings when it is used.
///
/// Credentials and `FlareSolverr` are exclusive in practice: AO3 rejects the login form when
/// it goes through `FlareSolverr`, and `always_login` would force that attempt on every
/// download. Without credentials no login is attempted at all.
pub fn render_personal_ini(
    credentials: Option<(&SecretString, &SecretString)>,
    flaresolverr: Option<&FlareSolverr<'_>>,
) -> Result<SecretString, IniError> {
    let mut ini = String::new();
    if let Some(fs) = flaresolverr {
        ini.push_str(&flaresolverr_section(fs)?);
    }
    ini.push_str("[archiveofourown.org]\nis_adult:true\n");
    if let Some((username, password)) = credentials {
        let username = check("AO3 username", username.expose_secret())?;
        let password = check("AO3 password", password.expose_secret())?;
        let _ = write!(
            ini,
            "always_login:true\nusername:{username}\npassword:{password}\n"
        );
    }
    // Retries per request, on top of FanFicFare's own backoff.
    ini.push_str("max_request_retries:6\n");
    Ok(SecretString::from(ini))
}

/// FanFicFare's settings for the login fallback.
///
/// Plain requests (the caller has `FlareSolverr` pass Cloudflare first), with the account's login
/// when there is one. `always_login` is left off on purpose: FanFicFare then logs in only when a
/// page asks for it, so a still-valid login cookie is simply used, and a missing or expired one
/// is replaced by a real login (a fresh cookie).
pub fn render_login_fallback_ini(
    credentials: Option<(&SecretString, &SecretString)>,
) -> Result<SecretString, IniError> {
    let mut ini = String::from("[archiveofourown.org]\nis_adult:true\n");
    if let Some((username, password)) = credentials {
        let username = check("AO3 username", username.expose_secret())?;
        let password = check("AO3 password", password.expose_secret())?;
        let _ = write!(ini, "username:{username}\npassword:{password}\n");
    }
    ini.push_str("max_request_retries:6\n");
    Ok(SecretString::from(ini))
}

/// Write `content` readable by the owner only, replacing any previous file.
pub fn write_private(path: &Path, content: &SecretString) -> io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(content.expose_secret().as_bytes())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn secret(s: &str) -> SecretString {
        SecretString::from(s.to_owned())
    }

    #[test]
    fn renders_the_expected_section() {
        let ini = render_personal_ini(Some((&secret("alice"), &secret("hunter2"))), None).unwrap();
        assert_eq!(
            ini.expose_secret(),
            "[archiveofourown.org]\nis_adult:true\nalways_login:true\nusername:alice\npassword:hunter2\nmax_request_retries:6\n"
        );
    }

    #[test]
    fn percent_signs_are_escaped_and_other_punctuation_is_not() {
        let ini =
            render_personal_ini(Some((&secret("u"), &secret("100%sure: #x;y"))), None).unwrap();
        assert!(ini.expose_secret().contains("password:100%%sure: #x;y\n"));
    }

    #[test]
    fn unrepresentable_values_are_rejected() {
        let ok = secret("fine");
        assert_eq!(
            render_personal_ini(Some((&ok, &secret("a\nb"))), None).unwrap_err(),
            IniError::ControlCharacter("AO3 password")
        );
        assert_eq!(
            render_personal_ini(Some((&secret("a\rb"), &ok)), None).unwrap_err(),
            IniError::ControlCharacter("AO3 username")
        );
        assert_eq!(
            render_personal_ini(Some((&ok, &secret(" pad"))), None).unwrap_err(),
            IniError::SurroundingWhitespace("AO3 password")
        );
        assert_eq!(
            render_personal_ini(Some((&ok, &secret("pad "))), None).unwrap_err(),
            IniError::SurroundingWhitespace("AO3 password")
        );
        assert_eq!(
            render_personal_ini(Some((&ok, &secret(""))), None).unwrap_err(),
            IniError::Empty("AO3 password")
        );
    }

    #[test]
    fn an_injection_attempt_cannot_add_settings() {
        let evil = secret("x\n[defaults]\nbrowser_cache_path:/etc");
        assert!(render_personal_ini(Some((&secret("u"), &evil)), None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn written_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/personal.ini");
        write_private(&path, &secret("data")).unwrap();
        write_private(&path, &secret("data2")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "data2");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    proptest! {
        /// Whatever the password, a successful render has exactly the expected six
        /// lines: nothing a secret contains can add or remove a configuration line.
        #[test]
        fn rendering_never_changes_the_line_structure(user in "\\PC{1,20}", pass in "\\PC{1,20}") {
            if let Ok(ini) = render_personal_ini(Some((&secret(&user), &secret(&pass))), None) {
                prop_assert_eq!(ini.expose_secret().lines().count(), 6);
            }
        }
    }

    fn fs_url(url: &str) -> url::Url {
        url.parse().unwrap()
    }

    fn with_flaresolverr(url: &str, secs: u64) -> String {
        let url = fs_url(url);
        let fs = FlareSolverr {
            url: &url,
            timeout: std::time::Duration::from_secs(secs),
        };
        render_personal_ini(None, Some(&fs))
            .unwrap()
            .expose_secret()
            .to_owned()
    }

    #[test]
    fn flaresolverr_replaces_the_login_and_never_forces_one() {
        let ini = with_flaresolverr("http://flaresolverr:8191", 60);
        assert_eq!(
            ini,
            "[defaults]\nuse_flaresolverr_proxy:true\nuse_flaresolverr_session:true\n\
             flaresolverr_proxy_protocol:http\nflaresolverr_proxy_address:flaresolverr\n\
             flaresolverr_proxy_port:8191\nflaresolverr_proxy_timeout:60000\n\
             connect_timeout:90\n\n\
             [archiveofourown.org]\nis_adult:true\nmax_request_retries:6\n"
        );
        assert!(!ini.contains("always_login") && !ini.contains("username"));
    }

    /// Regression: both were 60 s, so `FanFicFare` gave up on `FlareSolverr` just before it
    /// answered, and every slow AO3 page became "connection to flaresolverr failed".
    #[test]
    fn fanficfare_outlasts_flaresolverr() {
        for secs in [1, 60, 180] {
            let ini = with_flaresolverr("http://flaresolverr:8191", secs);
            let value = |key: &str| -> u64 {
                ini.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .and_then(|v| v.parse().ok())
                    .unwrap()
            };
            let flaresolverr_s = value("flaresolverr_proxy_timeout:") / 1000;
            assert!(
                value("connect_timeout:") > flaresolverr_s,
                "FanFicFare must wait longer than FlareSolverr ({flaresolverr_s} s)"
            );
        }
    }

    #[test]
    fn flaresolverr_ports_and_schemes() {
        assert!(with_flaresolverr("https://fs.example.org", 5).contains("protocol:https\n"));
        assert!(with_flaresolverr("https://fs.example.org", 5).contains("port:443\n"));
        assert!(with_flaresolverr("http://fs.example.org", 5).contains("port:80\n"));
        assert!(with_flaresolverr("http://127.0.0.1:9000/", 1).contains("timeout:1000\n"));
        assert!(with_flaresolverr("http://[::1]:8191", 1).contains("address:[::1]\n"));
    }

    #[test]
    fn without_either_there_is_only_the_adult_flag() {
        let ini = render_personal_ini(None, None).unwrap();
        assert_eq!(
            ini.expose_secret(),
            "[archiveofourown.org]\nis_adult:true\nmax_request_retries:6\n"
        );
    }

    #[test]
    fn the_login_fallback_logs_in_only_when_asked_to() {
        let ini = render_login_fallback_ini(Some((&secret("alice"), &secret("hunter2")))).unwrap();
        let text = ini.expose_secret();
        assert!(text.contains("username:alice\n"));
        assert!(
            !text.contains("always_login"),
            "a valid login cookie must be enough (always_login is set)"
        );
        assert!(
            !text.contains("flaresolverr"),
            "the fallback fetches by itself"
        );
        let anonymous = render_login_fallback_ini(None).unwrap();
        assert!(!anonymous.expose_secret().contains("username"));
    }

    #[test]
    fn the_login_fallback_rejects_unrepresentable_credentials_too() {
        assert_eq!(
            render_login_fallback_ini(Some((&secret("a\nb"), &secret("x")))).unwrap_err(),
            IniError::ControlCharacter("AO3 username")
        );
    }
}
