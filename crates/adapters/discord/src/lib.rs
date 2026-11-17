//! [`Notifier`] that posts to a Discord webhook.
//!
//! Story titles and error messages come from a third-party site, so they are treated
//! as hostile: markdown is escaped, mentions are disabled (`allowed_mentions`),
//! link previews are suppressed, and length is capped.

use async_trait::async_trait;
use fanfold_app::{
    error::NotifyError,
    ports::{Notifier, NotifyEvent},
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

const MAX_CONTENT_CHARS: usize = 1900; // Discord's limit is 2000
const MAX_FIELD_CHARS: usize = 300;
/// Discord message flag: do not render link embeds.
const SUPPRESS_EMBEDS: u32 = 1 << 2;

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

/// The webhook payload for an event.
#[must_use]
pub fn render(event: &NotifyEvent) -> Value {
    let content = match event {
        NotifyEvent::Downloaded {
            work,
            title,
            updated,
        } => {
            let verb = if *updated {
                "🔄 Updated"
            } else {
                "✅ Downloaded"
            };
            format!(
                "{verb} **{}**\n<{}>",
                sanitize(title, MAX_FIELD_CHARS),
                work.canonical_url()
            )
        }
        NotifyEvent::GaveUp {
            work,
            kind,
            message,
        } => format!(
            "❌ Gave up on <{}> ({kind}): {}",
            work.canonical_url(),
            sanitize(message, MAX_FIELD_CHARS)
        ),
        NotifyEvent::SiteDegraded { kind } => {
            format!("⚠️ AO3 looks unhealthy ({kind}). Pausing downloads until it recovers.")
        }
        NotifyEvent::SiteRecovered => "✅ AO3 is responding again. Downloads resumed.".to_owned(),
    };
    let content: String = content.chars().take(MAX_CONTENT_CHARS).collect();
    json!({
        "content": content,
        "allowed_mentions": { "parse": [] },
        "flags": SUPPRESS_EMBEDS,
    })
}

pub struct DiscordNotifier {
    client: reqwest::Client,
    webhook: SecretString,
}

impl std::fmt::Debug for DiscordNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The webhook URL is a bearer credential.
        f.debug_struct("DiscordNotifier").finish_non_exhaustive()
    }
}

impl DiscordNotifier {
    #[must_use]
    pub const fn new(client: reqwest::Client, webhook: SecretString) -> Self {
        Self { client, webhook }
    }
}

#[async_trait]
impl Notifier for DiscordNotifier {
    async fn notify(&self, event: &NotifyEvent) -> Result<(), NotifyError> {
        let body = serde_json::to_vec(&render(event)).map_err(|e| NotifyError(e.to_string()))?;
        let response = self
            .client
            .post(self.webhook.expose_secret())
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

    fn content(event: &NotifyEvent) -> String {
        render(event)["content"].as_str().unwrap().to_owned()
    }

    #[test]
    fn messages() {
        let downloaded = NotifyEvent::Downloaded {
            work: work(),
            title: "Nice Story".into(),
            updated: false,
        };
        assert_eq!(
            content(&downloaded),
            "✅ Downloaded **Nice Story**\n<https://archiveofourown.org/works/42>"
        );
        let updated = NotifyEvent::Downloaded {
            work: work(),
            title: "Nice Story".into(),
            updated: true,
        };
        assert!(content(&updated).starts_with("🔄 Updated"));
        let gave_up = NotifyEvent::GaveUp {
            work: work(),
            kind: FailureKind::NotFound,
            message: "deleted".into(),
        };
        assert_eq!(
            content(&gave_up),
            "❌ Gave up on <https://archiveofourown.org/works/42> (not_found): deleted"
        );
        assert!(
            content(&NotifyEvent::SiteDegraded {
                kind: FailureKind::Blocked
            })
            .contains("(blocked)")
        );
        assert!(content(&NotifyEvent::SiteRecovered).contains("responding again"));
    }

    #[test]
    fn every_payload_disables_mentions_and_embeds() {
        let payload = render(&NotifyEvent::SiteRecovered);
        assert_eq!(payload["allowed_mentions"], json!({"parse": []}));
        assert_eq!(payload["flags"], 4);
    }

    #[test]
    fn hostile_titles_cannot_ping_or_format() {
        let title = "@everyone @here <@123> **bold** [x](http://evil) `code` ||spoiler||";
        let text = content(&NotifyEvent::Downloaded {
            work: work(),
            title: title.into(),
            updated: false,
        });
        assert!(!text.contains("@everyone") && !text.contains("@here") && !text.contains("<@123"));
        assert!(!text.contains("[x](http://evil)"));
        assert!(!text.contains("||spoiler||"));
    }

    #[test]
    fn long_content_is_capped() {
        let event = NotifyEvent::GaveUp {
            work: work(),
            kind: FailureKind::Transient,
            message: "x".repeat(10_000),
        };
        assert!(content(&event).chars().count() <= MAX_CONTENT_CHARS);
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
        notifier.notify(&NotifyEvent::SiteRecovered).await.unwrap();
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
        let err = notifier
            .notify(&NotifyEvent::SiteRecovered)
            .await
            .unwrap_err();
        assert!(err.0.contains("500"), "{}", err.0);
        assert!(!err.0.contains("SECRET-TOKEN"));
    }

    #[tokio::test]
    async fn connection_errors_do_not_leak_the_token_either() {
        let notifier = DiscordNotifier::new(
            reqwest::Client::new(),
            SecretString::from("http://127.0.0.1:1/api/webhooks/1/SECRET-TOKEN".to_owned()),
        );
        let err = notifier
            .notify(&NotifyEvent::SiteRecovered)
            .await
            .unwrap_err();
        assert!(!err.0.contains("SECRET-TOKEN"), "{}", err.0);
        assert!(!format!("{notifier:?}").contains("SECRET-TOKEN"));
    }
}
