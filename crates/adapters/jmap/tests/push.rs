//! The push watcher against the fake server's event stream.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[allow(dead_code)]
mod common;

use std::{sync::Arc, time::Duration};

use common::FakeJmap;
use fanfold_adapter_jmap::{JmapSettings, PushConfig, PushWatcher};
use secrecy::SecretString;
use tokio::{sync::watch, time::timeout};

const USER: &str = "ficbot@example.org";
const PASSWORD: &str = "pw";

const fn config() -> PushConfig {
    PushConfig {
        reconnect_min: Duration::from_millis(20),
        reconnect_max: Duration::from_millis(100),
        max_stream_age: Duration::from_secs(30),
    }
}

fn settings(fake: &FakeJmap, password: &str) -> Arc<JmapSettings> {
    Arc::new(JmapSettings {
        base_url: fake.base.clone(),
        username: USER.to_owned(),
        password: SecretString::from(password.to_owned()),
        timeout: Duration::from_secs(5),
    })
}

async fn start() -> (FakeJmap, PushWatcher, watch::Sender<bool>) {
    let fake = FakeJmap::start(USER, PASSWORD).await;
    let (stop, stopped) = watch::channel(false);
    let watcher = PushWatcher::start(settings(&fake, PASSWORD), config(), stopped);
    (fake, watcher, stop)
}

async fn woken(watcher: &PushWatcher) -> bool {
    timeout(Duration::from_millis(1500), watcher.changed())
        .await
        .is_ok()
}

async fn quiet(watcher: &PushWatcher) -> bool {
    timeout(Duration::from_millis(300), watcher.changed())
        .await
        .is_err()
}

async fn until(what: &str, check: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn connecting_wakes_the_poller_once_and_then_it_is_quiet() {
    let (_fake, watcher, _stop) = start().await;
    assert!(
        woken(&watcher).await,
        "a fresh connection triggers a poll, covering anything missed before it"
    );
    assert!(quiet(&watcher).await, "pings alone do not wake anyone");
}

#[tokio::test]
async fn new_mail_wakes_the_poller_but_other_changes_do_not() {
    let (fake, watcher, _stop) = start().await;
    assert!(woken(&watcher).await);
    until("the stream", || fake.stream_connections() == 1).await;

    fake.push_other_state();
    assert!(quiet(&watcher).await, "a Mailbox change is not new mail");
    fake.push_email_state();
    assert!(woken(&watcher).await, "an Email change is");
}

#[tokio::test]
async fn it_reconnects_after_the_stream_drops_and_polls_on_reconnect() {
    let (fake, watcher, _stop) = start().await;
    assert!(woken(&watcher).await);
    until("the first stream", || fake.stream_connections() == 1).await;

    fake.kill_streams();
    until("a second connection", || fake.stream_connections() == 2).await;
    assert!(
        woken(&watcher).await,
        "reconnecting itself wakes the poller: events may have been missed meanwhile"
    );
    fake.push_email_state();
    assert!(woken(&watcher).await, "and the new stream works");
}

#[tokio::test]
async fn shutdown_stops_it_for_good() {
    let (fake, watcher, stop) = start().await;
    assert!(woken(&watcher).await);
    until("the stream", || fake.stream_connections() == 1).await;
    stop.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    fake.kill_streams();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        fake.stream_connections(),
        1,
        "it does not reconnect after shutdown"
    );
}

#[tokio::test]
async fn bad_credentials_back_off_instead_of_hammering_the_server() {
    let fake = FakeJmap::start(USER, PASSWORD).await;
    let (_stop, stopped) = watch::channel(false);
    let _watcher = PushWatcher::start(settings(&fake, "wrong"), config(), stopped);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let attempts = fake.requests().len();
    assert!(attempts >= 2, "it keeps trying: {attempts}");
    assert!(
        attempts <= 12,
        "but backs off (20 ms doubling to 100 ms): {attempts} requests in 600 ms"
    );
}

#[tokio::test]
async fn a_long_lived_stream_is_rotated_quietly_at_its_maximum_age() {
    // jmap-client never times out an open stream, so a silently dead connection would go
    // unnoticed; we cap the age instead. Reaching it is a routine reconnect (which also
    // polls), not a failure.
    let fake = FakeJmap::start(USER, PASSWORD).await;
    let (_stop, stopped) = watch::channel(false);
    let rotating = PushConfig {
        max_stream_age: Duration::from_millis(700),
        ..config()
    };
    let watcher = PushWatcher::start(settings(&fake, PASSWORD), rotating, stopped);
    assert!(woken(&watcher).await);
    until("a second connection", || fake.stream_connections() >= 2).await;
    assert!(woken(&watcher).await, "rotating wakes the poller too");
}
