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

/// The `personal.ini` for plain requests (no fanfold-browser).
///
/// The `[archiveofourown.org]` section, with a login on every download when there are
/// credentials (`always_login`). Without credentials no login is attempted at all.
pub fn render_personal_ini(
    credentials: Option<(&SecretString, &SecretString)>,
) -> Result<SecretString, IniError> {
    let mut ini = String::from("[archiveofourown.org]\nis_adult:true\n");
    if let Some((username, password)) = credentials {
        let username = check("AO3 username", username.expose_secret())?;
        let password = check("AO3 password", password.expose_secret())?;
        let _ = write!(
            ini,
            "always_login:true\nusername:{username}\npassword:{password}\n"
        );
    }
    // Re-sends of one page (2, 4, 8 s apart). Few on purpose: a refused try is retried by
    // the job, with a back-off that grows to an hour, and a burst of re-sends is exactly
    // what keeps AO3's bot filter refusing.
    ini.push_str("max_request_retries:3\n");
    Ok(SecretString::from(ini))
}

/// FanFicFare's settings through fanfold-browser.
///
/// The account's login when there is one, but no `always_login`: FanFicFare then logs in only
/// when a page asks for it, through the browser, which keeps the login between downloads, so
/// AO3 sees a login only when one is needed.
pub fn render_login_on_demand_ini(
    credentials: Option<(&SecretString, &SecretString)>,
) -> Result<SecretString, IniError> {
    let mut ini = String::from("[archiveofourown.org]\nis_adult:true\n");
    if let Some((username, password)) = credentials {
        let username = check("AO3 username", username.expose_secret())?;
        let password = check("AO3 password", password.expose_secret())?;
        let _ = write!(ini, "username:{username}\npassword:{password}\n");
    }
    ini.push_str("max_request_retries:3\n");
    Ok(SecretString::from(ini))
}

/// How FanFicFare builds the book. Appended to an `[archiveofourown.org]` section, so the
/// first lines belong to it.
///
/// * The title page also lists collections and AO3's counters (as of the download).
/// * EPUB 3, with the series in calibre's `calibre:series` form, which e-readers group by
///   (Fanfold adds the EPUB 3 form, and every series, afterwards).
/// * A log page that records what changed with each update (works in progress only), at the
///   back of the book with Fanfold's about page, so the book opens on its cover and title.
const BOOK_SETTINGS: &str = "\
extra_titlepage_entries:collectionsHTML,kudos,hits,bookmarks,comments

[epub]
epub_version:3.0
calibre_series_meta:true
include_logpage:smart
logpage_at_end:true
";

/// `ini` (one of the renderings above) followed by [`BOOK_SETTINGS`].
#[must_use]
pub fn with_book_settings(ini: &SecretString) -> SecretString {
    SecretString::from(format!("{}{BOOK_SETTINGS}", ini.expose_secret()))
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
        let ini = render_personal_ini(Some((&secret("alice"), &secret("hunter2")))).unwrap();
        assert_eq!(
            ini.expose_secret(),
            "[archiveofourown.org]\nis_adult:true\nalways_login:true\nusername:alice\npassword:hunter2\nmax_request_retries:3\n"
        );
    }

    #[test]
    fn percent_signs_are_escaped_and_other_punctuation_is_not() {
        let ini = render_personal_ini(Some((&secret("u"), &secret("100%sure: #x;y")))).unwrap();
        assert!(ini.expose_secret().contains("password:100%%sure: #x;y\n"));
    }

    #[test]
    fn unrepresentable_values_are_rejected() {
        let ok = secret("fine");
        assert_eq!(
            render_personal_ini(Some((&ok, &secret("a\nb")))).unwrap_err(),
            IniError::ControlCharacter("AO3 password")
        );
        assert_eq!(
            render_personal_ini(Some((&secret("a\rb"), &ok))).unwrap_err(),
            IniError::ControlCharacter("AO3 username")
        );
        assert_eq!(
            render_personal_ini(Some((&ok, &secret(" pad")))).unwrap_err(),
            IniError::SurroundingWhitespace("AO3 password")
        );
        assert_eq!(
            render_personal_ini(Some((&ok, &secret("pad ")))).unwrap_err(),
            IniError::SurroundingWhitespace("AO3 password")
        );
        assert_eq!(
            render_personal_ini(Some((&ok, &secret("")))).unwrap_err(),
            IniError::Empty("AO3 password")
        );
    }

    #[test]
    fn an_injection_attempt_cannot_add_settings() {
        let evil = secret("x\n[defaults]\nbrowser_cache_path:/etc");
        assert!(render_personal_ini(Some((&secret("u"), &evil))).is_err());
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
            if let Ok(ini) = render_personal_ini(Some((&secret(&user), &secret(&pass)))) {
                prop_assert_eq!(ini.expose_secret().lines().count(), 6);
            }
        }
    }

    #[test]
    fn without_a_login_there_is_only_the_adult_flag() {
        let ini = render_personal_ini(None).unwrap();
        assert_eq!(
            ini.expose_secret(),
            "[archiveofourown.org]\nis_adult:true\nmax_request_retries:3\n"
        );
    }

    #[test]
    fn through_the_browser_the_login_happens_only_when_asked_for() {
        let ini = render_login_on_demand_ini(Some((&secret("alice"), &secret("hunter2")))).unwrap();
        let text = ini.expose_secret();
        assert!(text.contains("username:alice\n"));
        assert!(
            !text.contains("always_login"),
            "the browser's own login must be enough (always_login is not set)"
        );
        let anonymous = render_login_on_demand_ini(None).unwrap();
        assert!(!anonymous.expose_secret().contains("username"));
    }

    #[test]
    fn book_settings_follow_the_site_section() {
        let ini = with_book_settings(&render_personal_ini(None).unwrap());
        let text = ini.expose_secret();
        assert!(text.starts_with("[archiveofourown.org]\nis_adult:true\n"));
        let site = text.split("\n[epub]\n").next().unwrap();
        assert!(site.contains("extra_titlepage_entries:"), "{site}");
        let epub = text.split("\n[epub]\n").nth(1).unwrap();
        for line in [
            "epub_version:3.0",
            "calibre_series_meta:true",
            "include_logpage:smart",
        ] {
            assert!(epub.lines().any(|l| l == line), "{line}");
        }
    }

    #[test]
    fn the_on_demand_login_rejects_unrepresentable_credentials_too() {
        assert_eq!(
            render_login_on_demand_ini(Some((&secret("a\nb"), &secret("x")))).unwrap_err(),
            IniError::ControlCharacter("AO3 username")
        );
    }
}
