//! A browser session: the cookies and `User-Agent` that let the downloader look like the
//! browser a human used to pass the site's bot protection (Cloudflare) and log in.
//!
//! Pure: parsing, validation, expiry arithmetic and rendering. Storage lives behind a
//! port, and the clock is always passed in.
//!
//! Two ways in, both pasted as text: a Netscape `cookies.txt` (what browser extensions
//! export, and what FanFicFare reads) or a HAR file saved from the browser's network
//! tab (which also carries the exact `User-Agent`). Cookie values are secrets: they are
//! never part of a `Debug` rendering, and everything that reaches the downloader is
//! validated so that a value cannot smuggle extra fields or lines into the cookie file.

use std::{fmt, fmt::Write as _, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::timestamp::{self, Timestamp};

/// Largest import accepted, in bytes (a real HAR for one page is about 10 kB).
pub const MAX_IMPORT_BYTES: usize = 512 * 1024;
/// Most cookies one session keeps.
pub const MAX_COOKIES: usize = 100;
const MAX_FIELD: usize = 8 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Domain as the browser reports it; a leading dot means "and subdomains".
    pub domain: String,
    pub path: String,
    /// `None` for a session cookie (the browser drops it on exit).
    pub expires: Option<Timestamp>,
    pub secure: bool,
    pub http_only: bool,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .field("domain", &self.domain)
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

/// Where a cookie stands against the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieStatus {
    /// No expiry: valid until the browser session ends, which for us is "until refused".
    Session,
    Valid,
    /// Expires within the warning window.
    ExpiringSoon,
    Expired,
}

impl Cookie {
    #[must_use]
    pub fn status(&self, now: Timestamp, warn_within: Duration) -> CookieStatus {
        match self.expires {
            None => CookieStatus::Session,
            Some(at) if at <= now => CookieStatus::Expired,
            Some(at) if at <= timestamp::add(now, warn_within) => CookieStatus::ExpiringSoon,
            Some(_) => CookieStatus::Valid,
        }
    }

    #[must_use]
    pub fn is_expired(&self, now: Timestamp) -> bool {
        self.expires.is_some_and(|at| at <= now)
    }

    /// Same cookie slot: a newer value replaces an older one.
    fn same_slot(&self, other: &Self) -> bool {
        self.name == other.name
            && self.path == other.path
            && self.domain.trim_start_matches('.') == other.domain.trim_start_matches('.')
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSession {
    /// Sent verbatim with every request: Cloudflare ties its clearance to it.
    pub user_agent: String,
    pub cookies: Vec<Cookie>,
    pub updated_at: Timestamp,
}

impl fmt::Debug for BrowserSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserSession")
            .field("user_agent", &self.user_agent)
            .field("cookies", &self.cookies)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl BrowserSession {
    /// The cookies still worth sending.
    #[must_use]
    pub fn live_cookies(&self, now: Timestamp) -> Vec<&Cookie> {
        self.cookies.iter().filter(|c| !c.is_expired(now)).collect()
    }

    /// The soonest expiry among cookies that have one and are still live.
    #[must_use]
    pub fn next_expiry(&self, now: Timestamp) -> Option<Timestamp> {
        self.cookies
            .iter()
            .filter(|c| !c.is_expired(now))
            .filter_map(|c| c.expires)
            .min()
    }

    /// Fold cookies the downloader saw back into this session (Cloudflare and the site
    /// rotate theirs). Cookies for a slot are replaced, new ones added, and nothing is
    /// removed: a cookie the run did not mention is still ours. Returns whether anything
    /// changed.
    pub fn absorb(&mut self, updated: Vec<Cookie>, now: Timestamp) -> bool {
        let before = self.cookies.clone();
        for new in updated {
            match self.cookies.iter_mut().find(|c| c.same_slot(&new)) {
                // The same value seen without its attributes (a HAR's request cookies)
                // is not news about the expiry.
                Some(slot) if new.expires.is_none() && new.value == slot.value => {}
                Some(slot) => *slot = new,
                None => self.cookies.push(new),
            }
        }
        self.cookies.retain(|c| !c.is_expired(now));
        self.cookies.truncate(MAX_COOKIES);
        self.cookies != before
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImportError {
    #[error("nothing to import")]
    Empty,
    #[error("the import is larger than {} kB", MAX_IMPORT_BYTES / 1024)]
    TooLarge,
    #[error("not a valid HAR file: {0}")]
    BadHar(String),
    #[error("line {0} is not a cookies.txt entry (expected 7 tab-separated fields)")]
    BadLine(usize),
    #[error("cookie {0:?} has characters that cannot be stored safely")]
    UnsafeCookie(String),
    #[error("no cookies for {0} were found in the import")]
    NoCookies(String),
    #[error("more than {MAX_COOKIES} cookies for the site")]
    TooMany,
    #[error("the User-Agent has characters that cannot be sent safely")]
    BadUserAgent,
}

/// What an import contained. `user_agent` is only known for HAR imports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub cookies: Vec<Cookie>,
    pub user_agent: Option<String>,
}

/// Is this header/cookie text safe to put on one line of a file or in a header?
fn clean(s: &str) -> bool {
    s.len() <= MAX_FIELD && !s.chars().any(char::is_control)
}

#[must_use]
pub fn valid_user_agent(s: &str) -> bool {
    !s.trim().is_empty() && clean(s)
}

/// A cookie we are willing to store: the fields cannot break the cookies.txt format.
fn valid_cookie(c: &Cookie) -> bool {
    let token = |s: &str| !s.is_empty() && clean(s) && !s.chars().any(char::is_whitespace);
    token(&c.name)
        && !c.name.contains('=')
        && !c.name.contains(';')
        && clean(&c.value)
        && !c.value.contains(';')
        && token(&c.domain)
        && c.path.starts_with('/')
        && clean(&c.path)
        && !c.path.chars().any(char::is_whitespace)
}

/// Does `domain` (as a cookie reports it) belong to `site`?
fn on_site(domain: &str, site: &str) -> bool {
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    let s = site.trim_start_matches('.').to_ascii_lowercase();
    d == s || d.ends_with(&format!(".{s}"))
}

fn finish(
    cookies: Vec<Cookie>,
    user_agent: Option<String>,
    site: &str,
) -> Result<Imported, ImportError> {
    let mut kept: Vec<Cookie> = Vec::new();
    for cookie in cookies.into_iter().filter(|c| on_site(&c.domain, site)) {
        if !valid_cookie(&cookie) {
            return Err(ImportError::UnsafeCookie(
                cookie.name.chars().take(40).collect(),
            ));
        }
        match kept.iter_mut().find(|c| c.same_slot(&cookie)) {
            Some(slot) => *slot = cookie,
            None => kept.push(cookie),
        }
    }
    if kept.is_empty() {
        return Err(ImportError::NoCookies(site.to_owned()));
    }
    if kept.len() > MAX_COOKIES {
        return Err(ImportError::TooMany);
    }
    if let Some(ua) = &user_agent
        && !valid_user_agent(ua)
    {
        return Err(ImportError::BadUserAgent);
    }
    Ok(Imported {
        cookies: kept,
        user_agent: user_agent.map(|ua| ua.trim().to_owned()),
    })
}

/// Parse pasted text: a HAR (starts with `{`) or a Netscape `cookies.txt`.
/// Cookies outside `site` are dropped, so an export of the whole browser is fine.
pub fn parse_import(text: &str, site: &str) -> Result<Imported, ImportError> {
    if text.len() > MAX_IMPORT_BYTES {
        return Err(ImportError::TooLarge);
    }
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Err(ImportError::Empty);
    }
    if text.starts_with('{') {
        parse_har(text, site)
    } else {
        parse_netscape(text, site)
    }
}

const HTTP_ONLY_PREFIX: &str = "#HttpOnly_";

pub fn parse_netscape(text: &str, site: &str) -> Result<Imported, ImportError> {
    let mut cookies = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let (line, http_only) = match line.strip_prefix(HTTP_ONLY_PREFIX) {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields: Vec<&str> = line.splitn(7, '\t').collect();
        if fields.len() != 7 {
            // Tolerate editors that turned tabs into spaces.
            fields = line.split_whitespace().collect();
        }
        let [domain, _subdomains, path, secure, expires, name, value] = fields[..] else {
            return Err(ImportError::BadLine(index + 1));
        };
        cookies.push(Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            domain: domain.to_owned(),
            path: path.to_owned(),
            expires: expires
                .parse::<i64>()
                .ok()
                .filter(|s| *s > 0)
                .and_then(|s| Timestamp::from_second(s).ok()),
            secure: secure.eq_ignore_ascii_case("true"),
            http_only,
        });
    }
    finish(cookies, None, site)
}

fn har_cookie(value: &serde_json::Value, host: &str) -> Option<Cookie> {
    let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
    let flag = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    Some(Cookie {
        name: text("name")?.to_owned(),
        value: text("value")?.to_owned(),
        domain: text("domain")
            .filter(|d| !d.is_empty())
            .unwrap_or(host)
            .to_owned(),
        path: text("path")
            .filter(|p| !p.is_empty())
            .unwrap_or("/")
            .to_owned(),
        expires: text("expires").and_then(|e| Timestamp::from_str(e).ok()),
        secure: flag("secure"),
        http_only: flag("httpOnly"),
    })
}

pub fn parse_har(text: &str, site: &str) -> Result<Imported, ImportError> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ImportError::BadHar(e.to_string()))?;
    let entries = json
        .pointer("/log/entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ImportError::BadHar("no log.entries".into()))?;

    let mut cookies: Vec<Cookie> = Vec::new();
    let mut user_agent: Option<String> = None;
    for entry in entries {
        let url = entry
            .pointer("/request/url")
            .and_then(serde_json::Value::as_str)
            .and_then(|u| url::Url::parse(u).ok());
        let Some(host) = url.as_ref().and_then(url::Url::host_str) else {
            continue;
        };
        if !on_site(host, site) {
            continue;
        }
        let default_domain = format!(".{}", site.trim_start_matches('.'));
        if user_agent.is_none() {
            user_agent = entry
                .pointer("/request/headers")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .find(|h| {
                    h.get("name")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|n| n.eq_ignore_ascii_case("user-agent"))
                })
                .and_then(|h| h.get("value").and_then(serde_json::Value::as_str))
                .map(str::to_owned);
        }
        // What was sent first, then what the server set in answer (newer, and the only
        // place a HAR records an expiry).
        for place in ["/request/cookies", "/response/cookies"] {
            let list = entry
                .pointer(place)
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten();
            for cookie in list.filter_map(|c| har_cookie(c, &default_domain)) {
                match cookies.iter_mut().find(|c| c.same_slot(&cookie)) {
                    // A cookie sent without attributes must not erase known ones.
                    Some(slot) if cookie.expires.is_none() && slot.expires.is_some() => {
                        slot.value = cookie.value;
                    }
                    Some(slot) => *slot = cookie,
                    None => cookies.push(cookie),
                }
            }
        }
    }
    finish(cookies, user_agent, site)
}

/// Render cookies as a Netscape `cookies.txt`, the file FanFicFare loads.
///
/// Session cookies get an empty expiry, which Python's cookie jar reads as "session" and
/// sends as normal; a literal 0 would count as already expired. Callers pass only the
/// cookies worth sending (see [`BrowserSession::live_cookies`]).
#[must_use]
pub fn to_netscape(cookies: &[&Cookie]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        let prefix = if c.http_only { HTTP_ONLY_PREFIX } else { "" };
        let subdomains = if c.domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let secure = if c.secure { "TRUE" } else { "FALSE" };
        let expires = c
            .expires
            .map_or_else(String::new, |t| t.as_second().to_string());
        let _ = writeln!(
            out,
            "{prefix}{}\t{subdomains}\t{}\t{secure}\t{expires}\t{}\t{}",
            c.domain, c.path, c.name, c.value
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const SITE: &str = "archiveofourown.org";

    fn at(s: i64) -> Timestamp {
        Timestamp::from_second(s).unwrap()
    }

    fn cookie(name: &str, expires: Option<i64>) -> Cookie {
        Cookie {
            name: name.into(),
            value: "v".into(),
            domain: ".archiveofourown.org".into(),
            path: "/".into(),
            expires: expires.map(at),
            secure: true,
            http_only: false,
        }
    }

    #[test]
    fn status_follows_the_clock() {
        let now = at(1_000);
        let week = Duration::from_hours(7 * 24);
        assert_eq!(cookie("a", None).status(now, week), CookieStatus::Session);
        assert_eq!(
            cookie("a", Some(500)).status(now, week),
            CookieStatus::Expired
        );
        assert_eq!(
            cookie("a", Some(1_000)).status(now, week),
            CookieStatus::Expired,
            "expiring exactly now is expired"
        );
        assert_eq!(
            cookie("a", Some(1_001)).status(now, week),
            CookieStatus::ExpiringSoon
        );
        assert_eq!(
            cookie("a", Some(1_000 + 7 * 86_400)).status(now, week),
            CookieStatus::ExpiringSoon,
            "the warning window is inclusive"
        );
        assert_eq!(
            cookie("a", Some(1_001 + 7 * 86_400)).status(now, week),
            CookieStatus::Valid
        );
    }

    #[test]
    fn netscape_roundtrip_keeps_every_attribute() {
        let mut http_only = cookie("cf_clearance", Some(2_000_000_000));
        http_only.http_only = true;
        let session = cookie("_otwarchive_session", None);
        let text = to_netscape(&[&http_only, &session]);
        let back = parse_netscape(&text, SITE).unwrap();
        assert_eq!(back.cookies, vec![http_only, session]);
        assert_eq!(back.user_agent, None);
    }

    #[test]
    fn session_cookies_are_written_with_an_empty_expiry() {
        let text = to_netscape(&[&cookie("s", None)]);
        let line = text.lines().nth(1).unwrap();
        assert_eq!(line.split('\t').nth(4), Some(""), "{line:?}");
    }

    #[test]
    fn netscape_ignores_comments_other_sites_and_zero_expiry() {
        let text = "# Netscape HTTP Cookie File\n\n\
            .example.com\tTRUE\t/\tTRUE\t9999999999\tother\tx\n\
            archiveofourown.org\tFALSE\t/\tFALSE\t0\tview_adult\ttrue\n\
            #HttpOnly_.archiveofourown.org\tTRUE\t/\tTRUE\t2000000000\tcf_clearance\tabc\r\n";
        let got = parse_netscape(text, SITE).unwrap();
        let names: Vec<_> = got.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["view_adult", "cf_clearance"]);
        assert_eq!(got.cookies[0].expires, None, "0 means a session cookie");
        assert!(got.cookies[1].http_only);
        assert_eq!(got.cookies[1].expires, Some(at(2_000_000_000)));
    }

    #[test]
    fn netscape_tolerates_spaces_instead_of_tabs() {
        let text = ".archiveofourown.org TRUE / TRUE 2000000000 a b";
        assert_eq!(parse_netscape(text, SITE).unwrap().cookies.len(), 1);
    }

    #[test]
    fn netscape_reports_the_bad_line() {
        let text = "# c\n.archiveofourown.org\tTRUE\t/\n";
        assert_eq!(
            parse_netscape(text, SITE).unwrap_err(),
            ImportError::BadLine(2)
        );
    }

    #[test]
    fn an_import_without_site_cookies_is_refused() {
        let text = ".example.com\tTRUE\t/\tTRUE\t9999999999\tother\tx\n";
        assert_eq!(
            parse_import(text, SITE).unwrap_err(),
            ImportError::NoCookies(SITE.into())
        );
        assert_eq!(parse_import("  \n", SITE).unwrap_err(), ImportError::Empty);
    }

    #[test]
    fn domains_must_really_belong_to_the_site() {
        assert!(on_site(".archiveofourown.org", SITE));
        assert!(on_site("www.archiveofourown.org", SITE));
        assert!(!on_site("evilarchiveofourown.org", SITE));
        assert!(!on_site("archiveofourown.org.evil.test", SITE));
    }

    #[test]
    fn values_cannot_smuggle_lines_or_fields() {
        for bad in ["a\nb", "a\tb", "a;b", "a\rb"] {
            let c = Cookie {
                value: bad.into(),
                ..cookie("n", None)
            };
            assert!(!valid_cookie(&c), "{bad:?}");
        }
        for bad in ["a b", "a=b", "", "a\tb"] {
            let c = cookie(bad, None);
            assert!(!valid_cookie(&c), "{bad:?}");
        }
        assert!(valid_cookie(&cookie("_cfuvid", None)));
    }

    fn har() -> String {
        serde_json::json!({"log": {"entries": [
            {"request": {
                "url": "https://archiveofourown.org/works/1",
                "headers": [{"name": "User-Agent", "value": "Mozilla/5.0 Test"}],
                "cookies": [
                    {"name": "cf_clearance", "value": "old"},
                    {"name": "view_adult", "value": "true"}
                ]},
             "response": {"cookies": [
                {"name": "cf_clearance", "value": "new", "expires": "2027-01-01T00:00:00Z",
                 "httpOnly": true, "secure": true}
             ]}},
            {"request": {"url": "https://other.example/x",
                "cookies": [{"name": "nope", "value": "1"}]},
             "response": {}}
        ]}})
        .to_string()
    }

    #[test]
    fn har_gives_cookies_user_agent_and_expiry() {
        let got = parse_import(&har(), SITE).unwrap();
        assert_eq!(got.user_agent.as_deref(), Some("Mozilla/5.0 Test"));
        let names: Vec<_> = got.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["cf_clearance", "view_adult"],
            "other sites are dropped"
        );
        let cf = &got.cookies[0];
        assert_eq!(cf.value, "new", "what the server set wins");
        assert!(cf.http_only && cf.secure);
        assert_eq!(cf.domain, ".archiveofourown.org");
        assert_eq!(cf.expires, Some("2027-01-01T00:00:00Z".parse().unwrap()));
    }

    #[test]
    fn a_sent_cookie_without_attributes_keeps_the_known_expiry() {
        let text = serde_json::json!({"log": {"entries": [
            {"request": {"url": "https://archiveofourown.org/",
                "cookies": [{"name": "a", "value": "2"}]},
             "response": {"cookies": [{"name": "a", "value": "1",
                "expires": "2027-01-01T00:00:00Z"}]}},
            {"request": {"url": "https://archiveofourown.org/",
                "cookies": [{"name": "a", "value": "3"}]},
             "response": {}}
        ]}})
        .to_string();
        let c = &parse_har(&text, SITE).unwrap().cookies[0];
        assert_eq!(c.value, "3");
        assert!(c.expires.is_some());
    }

    #[test]
    fn broken_har_is_reported_not_panicked_on() {
        assert!(matches!(
            parse_import("{not json", SITE),
            Err(ImportError::BadHar(_))
        ));
        assert!(matches!(
            parse_import("{\"log\": {}}", SITE),
            Err(ImportError::BadHar(_))
        ));
        assert_eq!(
            parse_import(&" ".repeat(MAX_IMPORT_BYTES + 1), SITE).unwrap_err(),
            ImportError::TooLarge
        );
    }

    #[test]
    fn a_hostile_user_agent_is_refused() {
        let text = serde_json::json!({"log": {"entries": [
            {"request": {"url": "https://archiveofourown.org/",
                "headers": [{"name": "user-agent", "value": "x\r\nHost: evil"}],
                "cookies": [{"name": "a", "value": "1"}]},
             "response": {}}
        ]}})
        .to_string();
        assert_eq!(
            parse_har(&text, SITE).unwrap_err(),
            ImportError::BadUserAgent
        );
    }

    #[test]
    fn absorbing_replaces_adds_and_drops_the_expired() {
        let now = at(1_000);
        let mut session = BrowserSession {
            user_agent: "ua".into(),
            cookies: vec![cookie("keep", Some(5_000)), cookie("rotate", Some(5_000))],
            updated_at: now,
        };
        let mut rotated = cookie("rotate", Some(9_000));
        rotated.value = "fresh".into();
        let changed = session.absorb(vec![rotated, cookie("added", None)], now);
        assert!(changed);
        let by_name = |n: &str| session.cookies.iter().find(|c| c.name == n).unwrap();
        assert_eq!(by_name("rotate").value, "fresh");
        assert_eq!(by_name("rotate").expires, Some(at(9_000)));
        assert_eq!(session.cookies.len(), 3, "unmentioned cookies stay");

        assert!(
            !session.absorb(vec![by_name("keep").clone()], now),
            "an identical cookie changes nothing"
        );
        assert!(session.absorb(vec![], at(6_000)), "expiry is a change");
        assert!(session.cookies.iter().all(|c| c.name != "keep"));
    }

    #[test]
    fn the_same_value_without_attributes_keeps_the_known_expiry() {
        let now = at(1_000);
        let mut session = BrowserSession {
            user_agent: "ua".into(),
            cookies: vec![cookie("a", Some(9_000))],
            updated_at: now,
        };
        assert!(!session.absorb(vec![cookie("a", None)], now));
        assert_eq!(session.cookies[0].expires, Some(at(9_000)));
        let mut changed = cookie("a", None);
        changed.value = "other".into();
        assert!(session.absorb(vec![changed], now));
        assert_eq!(
            session.cookies[0].expires, None,
            "a new value is a new cookie"
        );
    }

    #[test]
    fn next_expiry_ignores_session_and_expired_cookies() {
        let session = BrowserSession {
            user_agent: "ua".into(),
            cookies: vec![
                cookie("gone", Some(10)),
                cookie("session", None),
                cookie("late", Some(9_000)),
                cookie("soon", Some(3_000)),
            ],
            updated_at: at(0),
        };
        assert_eq!(session.next_expiry(at(1_000)), Some(at(3_000)));
        assert_eq!(session.live_cookies(at(1_000)).len(), 3);
    }

    #[test]
    fn debug_never_shows_a_value() {
        let mut c = cookie("cf_clearance", None);
        c.value = "super-secret-token".into();
        let session = BrowserSession {
            user_agent: "ua".into(),
            cookies: vec![c.clone()],
            updated_at: at(0),
        };
        assert!(!format!("{c:?}{session:?}").contains("super-secret"));
    }

    proptest! {
        #[test]
        fn parsing_never_panics(s in "\\PC*") {
            let _ = parse_import(&s, SITE);
        }

        #[test]
        fn whatever_is_accepted_is_safe_to_write(s in "[ -~\\t\\n]{0,300}") {
            if let Ok(got) = parse_import(&s, SITE) {
                let refs: Vec<&Cookie> = got.cookies.iter().collect();
                let out = to_netscape(&refs);
                // One header line plus exactly one line per cookie.
                prop_assert_eq!(out.lines().count(), got.cookies.len() + 1);
                for line in out.lines().skip(1) {
                    let fields = line.trim_start_matches(HTTP_ONLY_PREFIX).split('\t').count();
                    prop_assert_eq!(fields, 7, "{:?}", line);
                }
            }
        }
    }
}
