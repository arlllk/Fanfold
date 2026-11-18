//! [`Notifier`] that posts to Discord webhooks: rich embeds (see [`embeds`]).
//!
//! Downloads and series go to the main webhook; failures (gave up, AO3 unhealthy, AO3 back)
//! to a separate one, which may point at a muted channel, or nowhere.
//!
//! Story titles and error messages come from a third-party site, so they are treated
//! as hostile: markdown is escaped, mentions are disabled (`allowed_mentions`), and length
//! is capped.

pub mod embeds;

use async_trait::async_trait;
use fanfold_app::{
    error::NotifyError,
    ports::{Notifier, NotifyEvent},
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

/// Escape Discord markdown and neutralize mentions in untrusted text.
#[must_use]
pub fn sanitize(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|c| !c.is_control() || *c == '\n') {
        if out.chars().count() >= max_chars {
            out.push('…');
            break;
        }
        match c {
            '\\' | '*' | '_' | '~' | '`' | '>' | '|' | '[' | ']' | '(' | ')' | '#' => {
                out.push('\\');
                out.push(c);
            }
            // Zero-width space after @ breaks @everyone / @here / <@id> even if
            // allowed_mentions were ever loosened.
            '@' => out.push_str("@\u{200b}"),
            _ => out.push(c),
        }
    }
    out
}

/// The webhook payload for an event, with no links beyond AO3.
#[must_use]
pub fn render(event: &NotifyEvent) -> Value {
    embeds::payload(event, &embeds::Links::default())
}

/// Which webhook an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Downloads and series.
    Downloads,
    /// Gave up, AO3 unhealthy, AO3 back.
    Failures,
}

#[must_use]
pub const fn channel(event: &NotifyEvent) -> Channel {
    match event {
        NotifyEvent::Downloaded { .. } | NotifyEvent::SeriesListed { .. } => Channel::Downloads,
        NotifyEvent::GaveUp { .. }
        | NotifyEvent::SiteDegraded { .. }
        | NotifyEvent::SiteRecovered { .. } => Channel::Failures,
    }
}

pub struct DiscordNotifier {
    client: reqwest::Client,
    downloads: Option<SecretString>,
    failures: Option<SecretString>,
    links: embeds::Links,
}

impl std::fmt::Debug for DiscordNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The webhook URL is a bearer credential.
        f.debug_struct("DiscordNotifier").finish_non_exhaustive()
    }
}

impl DiscordNotifier {
    /// Every event to one webhook.
    #[must_use]
    pub fn new(client: reqwest::Client, webhook: SecretString) -> Self {
        let failures = SecretString::from(webhook.expose_secret().to_owned());
        Self::routed(client, Some(webhook), Some(failures))
    }

    /// Downloads and series to `downloads`, failures to `failures`; an absent webhook means
    /// those events are not posted.
    #[must_use]
    pub fn routed(
        client: reqwest::Client,
        downloads: Option<SecretString>,
        failures: Option<SecretString>,
    ) -> Self {
        Self {
            client,
            downloads,
            failures,
            links: embeds::Links::default(),
        }
    }

    /// Link each notification to its job in the Fanfold interface at `ui`.
    #[must_use]
    pub fn with_ui(mut self, ui: Option<String>) -> Self {
        self.links.ui = ui;
        self
    }
}

#[async_trait]
impl Notifier for DiscordNotifier {
    async fn notify(&self, event: &NotifyEvent) -> Result<(), NotifyError> {
        let webhook = match channel(event) {
            Channel::Downloads => &self.downloads,
            Channel::Failures => &self.failures,
        };
        let Some(webhook) = webhook else {
            return Ok(());
        };
        let body = serde_json::to_vec(&embeds::payload(event, &self.links))
            .map_err(|e| NotifyError(e.to_string()))?;
        let response = self
            .client
            .post(webhook.expose_secret())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            // reqwest errors embed the URL, which here contains the webhook token.
            .map_err(|e| NotifyError(e.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(NotifyError(format!("discord answered {status}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use fanfold_domain::{failure::FailureKind, work::WorkId};
    use proptest::prelude::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    fn work() -> WorkId {
        WorkId::new(42).unwrap()
    }

    use fanfold_app::ports::DownloadDetails;
    use fanfold_domain::{
        Timestamp,
        job::JobId,
        metadata::{
            Author, Chapter, Date, SeriesPart, Tag, TagKind, WorkFacts, WorkMetadata, WorkStats,
        },
        work::SeriesId,
    };

    use serde_json::json;

    fn recovered() -> NotifyEvent {
        NotifyEvent::SiteRecovered {
            down_since: None,
            at: Timestamp::UNIX_EPOCH,
        }
    }

    fn embed(event: &NotifyEvent) -> Value {
        render(event)["embeds"][0].clone()
    }

    fn text_of(embed: &Value) -> String {
        embed.to_string()
    }

    fn chapter(n: u32, title: &str, posted: Option<(i16, i8, i8)>) -> Chapter {
        Chapter {
            number: n,
            title: title.into(),
            ao3_id: Some(u64::from(n) + 1000),
            posted_on: posted.map(|(y, m, d)| Date::new(y, m, d).unwrap()),
        }
    }

    fn metadata() -> WorkMetadata {
        WorkMetadata {
            work: work(),
            facts: WorkFacts {
                title: "Harry Potter and the Girl with the Diary".into(),
                byline: "TheBrokenPokeman".into(),
                summary_html:
                    "<p>It&#39;s now Harry&#x27;s <b>second</b> year.</p><p>Another &amp; more.</p>"
                        .into(),
                language: "English".into(),
                rating: "Teen And Up Audiences".into(),
                restricted: false,
                status: "In-Progress".into(),
                published_on: Some(Date::new(2026, 10, 8).unwrap()),
                updated_on: Some(Date::new(2026, 10, 20).unwrap()),
                chapters_posted: 2,
                chapters_total: None,
                words: 20_000,
            },
            authors: vec![Author {
                user: "TheBrokenPokeman".into(),
                pseud: "TheBrokenPokeman".into(),
            }],
            tags: vec![
                Tag {
                    kind: TagKind::Fandom,
                    name: "Harry Potter - J. K. Rowling".into(),
                },
                Tag {
                    kind: TagKind::Warning,
                    name: "Creator Chose Not To Use Archive Warnings".into(),
                },
                Tag {
                    kind: TagKind::Relationship,
                    name: "Draco Malfoy/Harry Potter".into(),
                },
                Tag {
                    kind: TagKind::Freeform,
                    name: "Enemies to Lovers".into(),
                },
            ],
            series: vec![SeriesPart {
                id: SeriesId::new(6_704_161).unwrap(),
                name: "The Malfoid Saga".into(),
                part: Some(2),
            }],
            chapters: vec![
                chapter(1, "1. Making No Noise", Some((2026, 10, 8))),
                chapter(2, "2. The Diary", Some((2026, 10, 9))),
            ],
            stats: WorkStats {
                hits: Some(117_707),
                kudos: Some(2874),
                comments: Some(1907),
                bookmarks: Some(235),
            },
            raw: serde_json::Value::Null,
        }
    }

    fn downloaded(updated: bool, details: DownloadDetails) -> NotifyEvent {
        NotifyEvent::Downloaded {
            job: JobId::new(7),
            work: work(),
            title: details.metadata.facts.title.clone(),
            updated,
            details: Some(Box::new(details)),
        }
    }

    #[test]
    fn a_first_download_says_how_far_it_goes_and_who_wrote_it() {
        let event = downloaded(
            false,
            DownloadDetails {
                metadata: metadata(),
                previous: None,
                new_chapters: vec![],
                path: "series/The Malfoid Saga 6704161/02 Harry Potter and the Girl with the Diary 42.epub".into(),
            },
        );
        let e = embed(&event);
        assert_eq!(e["author"]["name"], "📚 New book in a series");
        assert_eq!(e["title"], "Harry Potter and the Girl with the Diary");
        assert_eq!(e["url"], "https://archiveofourown.org/works/42");
        let description = e["description"].as_str().unwrap();
        assert!(description.contains(
            "by [TheBrokenPokeman](https://archiveofourown.org/users/TheBrokenPokeman/pseuds/TheBrokenPokeman)"
        ), "{description}");
        assert!(description.contains(
            "Part **2** of [The Malfoid Saga](https://archiveofourown.org/series/6704161)"
        ));
        assert!(description.contains(
            "Downloaded up to [**Chapter 2** · 2. The Diary](https://archiveofourown.org/works/42/chapters/1002)"
        ), "{description}");
        assert!(
            description.contains("> It's now Harry's second year."),
            "the summary is plain text: {description}"
        );
        let all = text_of(&e);
        assert!(
            all.contains("2/?") && all.contains("20,000 · ~1 h 20 min"),
            "{all}"
        );
        assert!(all.contains("🟡 Teen And Up Audiences"));
        assert!(all.contains("❤️ 2,874 kudos"));
        assert!(
            !all.contains("Warnings"),
            "the generic warning is not shown"
        );
        assert_eq!(
            e["footer"]["text"].as_str().unwrap().split('/').next(),
            Some("series")
        );
    }

    #[test]
    fn an_update_lists_its_new_chapters_and_what_else_changed() {
        let mut before = metadata();
        before.chapters.truncate(1);
        before.facts.chapters_posted = 1;
        before.facts.words = 8000;
        before.facts.updated_on = Some(Date::new(2026, 10, 8).unwrap());
        let mut now = metadata();
        now.facts.status = "Completed".into();
        now.facts.chapters_total = Some(2);
        now.tags.push(Tag {
            kind: TagKind::Freeform,
            name: "Yandere".into(),
        });
        let event = downloaded(
            true,
            DownloadDetails {
                new_chapters: vec![now.chapters[1].clone()],
                previous: Some(before.state()),
                metadata: now,
                path: "p.epub".into(),
            },
        );
        let e = embed(&event);
        assert_eq!(e["author"]["name"], "🎉 Completed · 1 new chapter");
        let description = e["description"].as_str().unwrap();
        for line in [
            "• [**Chapter 2** · 2. The Diary](https://archiveofourown.org/works/42/chapters/1002) — posted 2026-10-09",
            "📈 Chapters 1 → 2/2",
            "✍️ +12,000 words (~48 min to read)",
            "⏱ First update in 12 days",
            "🎉 The work is now **complete**",
            "🏷 New tags: Yandere",
        ] {
            assert!(
                description.contains(line),
                "missing {line:?} in {description}"
            );
        }
    }

    #[test]
    fn a_long_update_is_capped_and_still_fits_discord() {
        let mut before = metadata();
        before.chapters.clear();
        let mut now = metadata();
        now.chapters = (1..=400)
            .map(|n| chapter(n, &"long title ".repeat(30), None))
            .collect();
        now.tags.extend((0..300).map(|n| Tag {
            kind: TagKind::Freeform,
            name: format!("tag {n} {}", "x".repeat(50)),
        }));
        now.facts.summary_html = "y".repeat(10_000);
        let event = downloaded(
            true,
            DownloadDetails {
                new_chapters: now.chapters.clone(),
                previous: Some(before.state()),
                metadata: now,
                path: "p".repeat(5000),
            },
        );
        let e = embed(&event);
        let description = e["description"].as_str().unwrap();
        assert!(description.contains(" more"), "{description}");
        assert!(
            description.contains("…and 39"),
            "the count of the rest is shown"
        );
        assert!(description.chars().count() <= 2001);
        let total: usize = ["title", "description"]
            .iter()
            .map(|k| e[*k].as_str().unwrap_or("").chars().count())
            .sum::<usize>()
            + e["footer"]["text"].as_str().unwrap().chars().count()
            + e["author"]["name"].as_str().unwrap().chars().count()
            + e["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    f["name"].as_str().unwrap().chars().count()
                        + f["value"].as_str().unwrap().chars().count()
                })
                .sum::<usize>();
        assert!(total <= 6000, "{total} characters");
        for f in e["fields"].as_array().unwrap() {
            assert!(f["value"].as_str().unwrap().chars().count() <= 1024);
        }
    }

    #[test]
    fn hostile_text_cannot_ping_format_or_break_links() {
        let mut m = metadata();
        m.facts.title = "@everyone **bold** [x](http://evil)".into();
        m.authors = vec![Author {
            user: "a b(c)".into(),
            pseud: "@here".into(),
        }];
        m.chapters = vec![chapter(1, "](http://evil) `code`", None)];
        let e = embed(&downloaded(
            false,
            DownloadDetails {
                metadata: m,
                previous: None,
                new_chapters: vec![],
                path: "p".into(),
            },
        ));
        let all = text_of(&e);
        assert!(!all.contains("@everyone") && !all.contains("@here"));
        assert!(!all.contains("[x](http://evil)") && !all.contains("](http://evil)"));
        assert!(all.contains("users/a%20b%28c%29/pseuds/"), "{all}");
        assert_eq!(
            render(&NotifyEvent::SiteRecovered {
                down_since: None,
                at: Timestamp::UNIX_EPOCH
            })["allowed_mentions"],
            json!({"parse": []})
        );
    }

    #[test]
    fn failures_explain_themselves() {
        let gave_up = embed(&NotifyEvent::GaveUp {
            job: JobId::new(3),
            target: work().into(),
            title: Some("A Deleted Fic".into()),
            kind: FailureKind::NotFound,
            message: "StoryDoesNotExist: ```gone```".into(),
            attempts: 1,
            max_attempts: 5,
        });
        assert_eq!(gave_up["author"]["name"], "❌ Gave up");
        assert_eq!(gave_up["title"], "A Deleted Fic");
        let all = text_of(&gave_up);
        assert!(all.contains("Not found") && all.contains("deleted, or back to a draft"));
        assert!(all.contains("1 of 5") && all.contains("`not_found`"));
        assert_eq!(
            all.matches("```").count(),
            2,
            "the message cannot close the code block: {all}"
        );

        let since = Timestamp::from_second(1_000_000).unwrap();
        let degraded = text_of(&embed(&NotifyEvent::SiteDegraded {
            kind: FailureKind::SiteUnavailable,
            message: "HTTP 525".into(),
            retry_at: Timestamp::from_second(1_000_240).unwrap(),
            waiting: Some(12),
        }));
        assert!(
            degraded.contains("<t:1000240:R>") && degraded.contains("**12** jobs are waiting"),
            "{degraded}"
        );
        let recovered = text_of(&embed(&NotifyEvent::SiteRecovered {
            down_since: Some(since),
            at: Timestamp::from_second(1_004_380).unwrap(),
        }));
        assert!(
            recovered.contains("responding again") && recovered.contains("**1 h 13 min**"),
            "{recovered}"
        );
    }

    #[test]
    fn job_links_point_at_the_interface_when_it_is_known() {
        let links = embeds::Links {
            ui: Some("https://fanfold.example.org/".into()),
        };
        let event = NotifyEvent::SeriesListed {
            job: JobId::new(9),
            series: SeriesId::new(77).unwrap(),
            name: "A Saga".into(),
            works: 4,
            queued: 3,
        };
        let with = embeds::payload(&event, &links).to_string();
        assert!(
            with.contains("(https://fanfold.example.org/jobs/9)"),
            "{with}"
        );
        assert!(with.contains("Queued **3** of its **4** works"));
        assert!(!render(&event).to_string().contains("/jobs/9"));
    }

    #[test]
    fn events_go_to_their_channel() {
        assert_eq!(
            channel(&downloaded(
                false,
                DownloadDetails {
                    metadata: metadata(),
                    previous: None,
                    new_chapters: vec![],
                    path: String::new()
                }
            )),
            Channel::Downloads
        );
        assert_eq!(
            channel(&NotifyEvent::SiteRecovered {
                down_since: None,
                at: Timestamp::UNIX_EPOCH
            }),
            Channel::Failures
        );
    }

    proptest! {
        #[test]
        fn sanitized_text_never_contains_a_raw_mention_or_control_chars(s in "\\PC*") {
            let clean = sanitize(&s, 300);
            prop_assert!(!clean.contains("@everyone") && !clean.contains("@here"));
            prop_assert!(!clean.chars().any(|c| c.is_control() && c != '\n'));
            prop_assert!(clean.chars().count() <= 302);
        }
    }

    /// One-shot HTTP server: answers `status` and returns the request it saw.
    async fn serve_once(status: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/api/webhooks/1/SECRET-TOKEN",
            listener.local_addr().unwrap()
        );
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                seen.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&seen).into_owned();
                if let Some(split) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if seen.len() >= split + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            stream
                .write_all(
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            String::from_utf8_lossy(&seen).into_owned()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn posts_the_payload_as_json() {
        let (url, server) = serve_once("204 No Content").await;
        let notifier = DiscordNotifier::new(reqwest::Client::new(), SecretString::from(url));
        notifier.notify(&recovered()).await.unwrap();
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/webhooks/1/SECRET-TOKEN "),
            "{request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("content-type: application/json")
        );
        assert!(request.contains("responding again") && request.contains("allowed_mentions"));
    }

    #[tokio::test]
    async fn http_errors_are_reported_without_the_webhook_token() {
        let (url, _server) = serve_once("500 Internal Server Error").await;
        let notifier = DiscordNotifier::new(reqwest::Client::new(), SecretString::from(url));
        let err = notifier.notify(&recovered()).await.unwrap_err();
        assert!(err.0.contains("500"), "{}", err.0);
        assert!(!err.0.contains("SECRET-TOKEN"));
    }

    #[tokio::test]
    async fn connection_errors_do_not_leak_the_token_either() {
        let notifier = DiscordNotifier::new(
            reqwest::Client::new(),
            SecretString::from("http://127.0.0.1:1/api/webhooks/1/SECRET-TOKEN".to_owned()),
        );
        let err = notifier.notify(&recovered()).await.unwrap_err();
        assert!(!err.0.contains("SECRET-TOKEN"), "{}", err.0);
        assert!(!format!("{notifier:?}").contains("SECRET-TOKEN"));
    }

    #[tokio::test]
    async fn failures_go_to_their_own_webhook_or_nowhere() {
        let (downloads, downloads_seen) = serve_once("204 No Content").await;
        let (failures, failures_seen) = serve_once("204 No Content").await;
        let notifier = DiscordNotifier::routed(
            reqwest::Client::new(),
            Some(SecretString::from(downloads)),
            Some(SecretString::from(failures)),
        );
        notifier.notify(&recovered()).await.unwrap();
        assert!(failures_seen.await.unwrap().contains("responding again"));
        let book = downloaded(
            false,
            DownloadDetails {
                metadata: metadata(),
                previous: None,
                new_chapters: vec![],
                path: "p.epub".into(),
            },
        );
        notifier.notify(&book).await.unwrap();
        assert!(
            downloads_seen
                .await
                .unwrap()
                .contains("New book in a series")
        );

        // No failures webhook: failures are simply not posted.
        let quiet = DiscordNotifier::routed(reqwest::Client::new(), None, None);
        quiet.notify(&recovered()).await.unwrap();
        quiet.notify(&book).await.unwrap();
    }
}
