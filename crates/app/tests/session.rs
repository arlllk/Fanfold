//! The browser-session use cases, against the in-memory store.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::clone_on_ref_ptr
)]

use std::{fmt::Write as _, sync::Arc, time::Duration};

use fanfold_app::{
    error::RepoError,
    ports::SessionStore,
    session::{SessionAdmin, SessionError},
};
use fanfold_domain::{Timestamp, session::CookieStatus};
use fanfold_testkit::memory::{FakeClock, InMemorySession};

const NOW: i64 = 1_800_000_000;
const SITE: &str = "archiveofourown.org";

fn admin() -> (SessionAdmin, Arc<InMemorySession>) {
    let store = Arc::new(InMemorySession::new());
    let admin = SessionAdmin::new(
        Arc::clone(&store) as Arc<dyn SessionStore>,
        Arc::new(FakeClock::at_secs(NOW)),
        SITE,
        Duration::from_hours(24),
    );
    (admin, store)
}

fn txt(lines: &[(&str, i64, &str, &str)]) -> String {
    lines.iter().fold(String::new(), |mut out, (d, exp, n, v)| {
        let exp = if *exp == 0 {
            String::new()
        } else {
            exp.to_string()
        };
        let _ = writeln!(out, "{d}\tTRUE\t/\tTRUE\t{exp}\t{n}\t{v}");
        out
    })
}

fn har(ua: &str) -> String {
    serde_json::json!({"log": {"entries": [{"request": {
        "url": "https://archiveofourown.org/works/1",
        "headers": [{"name": "User-Agent", "value": ua}],
        "cookies": [{"name": "cf_clearance", "value": "from-har-0123456789"}]},
        "response": {}}]}})
    .to_string()
}

#[tokio::test]
async fn a_cookies_file_needs_a_user_agent_from_somewhere() {
    let (admin, store) = admin();
    let text = txt(&[(".archiveofourown.org", NOW + 100, "a", "1")]);
    assert_eq!(
        admin.import(&text, None).await.unwrap_err(),
        SessionError::NeedUserAgent
    );
    assert_eq!(store.load().await.unwrap(), None, "nothing was stored");

    let report = admin.import(&text, Some("  UA/1  ")).await.unwrap();
    assert_eq!(report.view.user_agent, "UA/1");
    assert!(report.user_agent_changed);
}

#[tokio::test]
async fn a_har_brings_its_own_user_agent_and_a_typed_one_wins() {
    let (admin, _) = admin();
    let report = admin.import(&har("HarUA/1"), None).await.unwrap();
    assert_eq!(report.view.user_agent, "HarUA/1");
    let report = admin
        .import(&har("HarUA/1"), Some("Typed/2"))
        .await
        .unwrap();
    assert_eq!(report.view.user_agent, "Typed/2");
}

#[tokio::test]
async fn imports_merge_so_a_har_and_a_cookies_file_can_be_combined() {
    let (admin, store) = admin();
    admin.import(&har("HarUA/1"), None).await.unwrap();
    let text = txt(&[
        (".archiveofourown.org", NOW + 3_600, "__cfwaitingroom", "w"),
        (
            ".archiveofourown.org",
            NOW + 86_400,
            "remember_user_token",
            "r",
        ),
        (".other.example", NOW + 86_400, "elsewhere", "x"),
    ]);
    let report = admin.import(&text, None).await.unwrap();
    assert_eq!(report.cookies_in_import, 2, "other sites are not counted");
    assert!(!report.user_agent_changed, "the HAR's user agent is kept");
    let names: Vec<_> = report
        .view
        .cookies
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["__cfwaitingroom", "remember_user_token", "cf_clearance"]
    );
    let stored = store.load().await.unwrap().unwrap();
    assert_eq!(stored.user_agent, "HarUA/1");
    assert_eq!(stored.cookies.len(), 3);
}

#[tokio::test]
async fn the_view_reports_expiry_and_never_a_value() {
    let (admin, _) = admin();
    let text = txt(&[
        (
            ".archiveofourown.org",
            NOW + 600,
            "soon",
            "secret-value-aaaaaaaa",
        ),
        (
            ".archiveofourown.org",
            NOW + 30 * 86_400,
            "later",
            "secret-value-bbbbbbbb",
        ),
        (
            ".archiveofourown.org",
            0,
            "session",
            "secret-value-cccccccc",
        ),
    ]);
    admin.import(&text, Some("UA")).await.unwrap();
    let view = admin.view().await.unwrap().unwrap();
    let rows: Vec<_> = view
        .cookies
        .iter()
        .map(|c| (c.name.as_str(), c.status))
        .collect();
    assert_eq!(
        rows,
        [
            ("soon", CookieStatus::ExpiringSoon),
            ("later", CookieStatus::Valid),
            ("session", CookieStatus::Session)
        ]
    );
    assert_eq!(view.next_expiry, Timestamp::from_second(NOW + 600).ok());
    assert!(!format!("{view:?}").contains("secret-value"));
}

#[tokio::test]
async fn bad_imports_change_nothing() {
    let (admin, store) = admin();
    admin.import(&har("Good/1"), None).await.unwrap();
    let before = store.load().await.unwrap();
    for bad in ["", "{nope", "garbage line"] {
        assert!(admin.import(bad, Some("UA")).await.is_err(), "{bad:?}");
    }
    let hostile = har("x\r\nHost: evil");
    assert!(admin.import(&hostile, None).await.is_err());
    assert!(admin.import(&har("Good/1"), Some("a\nb")).await.is_err());
    assert_eq!(store.load().await.unwrap(), before);
}

#[tokio::test]
async fn clearing_forgets_the_session() {
    let (admin, _) = admin();
    assert!(!admin.clear().await.unwrap());
    admin.import(&har("UA/1"), None).await.unwrap();
    assert!(admin.clear().await.unwrap());
    assert_eq!(admin.view().await.unwrap(), None);
}

#[tokio::test]
async fn storage_failures_are_reported() {
    let (admin, store) = admin();
    store.break_with(Some(RepoError::Backend("down".into())));
    assert!(matches!(admin.view().await, Err(SessionError::Repo(_))));
    assert!(matches!(
        admin.import(&har("UA/1"), None).await,
        Err(SessionError::Repo(_))
    ));
}
