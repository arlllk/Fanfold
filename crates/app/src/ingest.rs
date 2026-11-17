//! Use case: turn new mail into queued jobs.

use std::sync::Arc;

use fanfold_domain::{job::JobSource, mail::extract_work_ids};

use crate::{
    error::AppError,
    ports::{Clock, EnqueueOutcome, JobRepository, MailMessage, MailSource},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestConfig {
    /// Case-insensitive substrings of the `From` header. Empty allows any sender.
    pub allowed_senders: Vec<String>,
    /// Cap per message, so one hostile or runaway email cannot flood the queue.
    pub max_links_per_message: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestReport {
    pub messages: usize,
    pub from_unlisted_sender: usize,
    pub without_work_links: usize,
    pub enqueued: usize,
    pub already_active: usize,
    pub truncated_links: usize,
}

pub struct Ingestor {
    mail: Arc<dyn MailSource>,
    jobs: Arc<dyn JobRepository>,
    clock: Arc<dyn Clock>,
    config: IngestConfig,
}

impl std::fmt::Debug for Ingestor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ingestor")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Lower-case letters and digits only: `do-not-reply_at_archiveofourown_org_x` and
/// `do-not-reply@archiveofourown.org` both reduce to something containing
/// `archiveofourownorg`.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `from` (the sender as `Name <address>`) matches an allow-list entry.
///
/// An entry matches as a case-insensitive substring, or, because mail relays and
/// forwarders rewrite addresses (the real inbox this runs on delivers AO3's mail from
/// `do-not-reply_at_archiveofourown_org_<tag>`), as a substring once punctuation is
/// ignored on both sides. This is a convenience filter, not a security boundary: a
/// matching sender can only make us download public works.
#[must_use]
pub fn sender_allowed(from: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let plain = from.to_lowercase();
    let squashed = squash(from);
    allowed.iter().any(|entry| {
        let wanted = squash(entry);
        !wanted.is_empty() && (plain.contains(&entry.to_lowercase()) || squashed.contains(&wanted))
    })
}

impl Ingestor {
    #[must_use]
    pub fn new(
        mail: Arc<dyn MailSource>,
        jobs: Arc<dyn JobRepository>,
        clock: Arc<dyn Clock>,
        config: IngestConfig,
    ) -> Self {
        Self {
            mail,
            jobs,
            clock,
            config,
        }
    }

    /// Fetch one batch, enqueue its works, then commit the checkpoint.
    ///
    /// The checkpoint is committed only if every message was handled; on any error
    /// the batch is replayed next time, and per-work deduplication makes that safe.
    pub async fn run_once(&self) -> Result<IngestReport, AppError> {
        let batch = self.mail.fetch_new().await?;
        let mut report = IngestReport::default();
        for message in &batch.messages {
            report.messages += 1;
            self.handle(message, &mut report).await?;
        }
        self.mail.commit(&batch.checkpoint).await?;
        Ok(report)
    }

    async fn handle(
        &self,
        message: &MailMessage,
        report: &mut IngestReport,
    ) -> Result<(), AppError> {
        if !sender_allowed(&message.from, &self.config.allowed_senders) {
            report.from_unlisted_sender += 1;
            return Ok(());
        }
        let mut ids = extract_work_ids(&format!("{}\n{}", message.text, message.html));
        if ids.is_empty() {
            report.without_work_links += 1;
            return Ok(());
        }
        if ids.len() > self.config.max_links_per_message {
            report.truncated_links += ids.len() - self.config.max_links_per_message;
            ids.truncate(self.config.max_links_per_message);
        }
        for work in ids {
            match self
                .jobs
                .enqueue(work, JobSource::Mail, self.clock.now())
                .await?
            {
                EnqueueOutcome::Created(job) => {
                    tracing::info!(job = %job.id, %work, mail = %message.id, "queued from mail");
                    report.enqueued += 1;
                }
                EnqueueOutcome::AlreadyActive(_) => report.already_active += 1,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use fanfold_domain::{
        Timestamp,
        job::{Job, JobId},
    };

    use super::*;
    use crate::{
        error::{MailError, RepoError},
        ports::{Checkpoint, MailBatch, MockClock, MockJobRepository, MockMailSource},
    };

    fn message(from: &str, text: &str) -> MailMessage {
        MailMessage {
            id: "m1".into(),
            from: from.into(),
            subject: "s".into(),
            text: text.into(),
            html: String::new(),
        }
    }

    fn batch(messages: Vec<MailMessage>) -> MailBatch {
        MailBatch {
            messages,
            checkpoint: Checkpoint("cp".into()),
        }
    }

    fn config() -> IngestConfig {
        IngestConfig {
            allowed_senders: vec!["archiveofourown.org".into()],
            max_links_per_message: 2,
        }
    }

    fn clock() -> Arc<MockClock> {
        let mut clock = MockClock::new();
        clock
            .expect_now()
            .returning(|| Timestamp::from_second(100).unwrap());
        Arc::new(clock)
    }

    #[test]
    fn sender_matching() {
        let allowed = vec!["archiveofourown.org".to_owned()];
        assert!(
            sender_allowed("anyone <x@example.com>", &[]),
            "empty allow-list admits everyone"
        );
        assert!(sender_allowed("AO3 <x@archiveofourown.org>", &allowed));
        assert!(
            sender_allowed("AO3 <X@ArchiveOfOurOwn.org>", &allowed),
            "case-insensitive"
        );
        assert!(!sender_allowed("evil <x@example.com>", &allowed));
        assert!(
            !sender_allowed("anything", &[String::new()]),
            "an empty entry matches nothing"
        );
    }

    #[test]
    fn senders_rewritten_by_a_mail_relay_still_match() {
        let allowed = vec!["archiveofourown.org".to_owned()];
        // Exactly what the real forwarded mail looks like (name intact, address mangled),
        // and the mangled address alone, in case the name is ever dropped.
        for from in [
            "Archive of Our Own - do-not-reply at archiveofourown.org <do-not-reply_at_archiveofourown_org_cqcc@relay.example>",
            "do-not-reply_at_archiveofourown_org_cqcc@relay.example",
            "DO-NOT-REPLY@ARCHIVEOFOUROWN.ORG",
        ] {
            assert!(sender_allowed(from, &allowed), "{from}");
        }
        for from in [
            "no-reply@mail.proton.me",
            "Someone <someone@example.com>",
            "",
        ] {
            assert!(!sender_allowed(from, &allowed), "{from:?}");
        }
        // An entry made only of punctuation matches nothing rather than everything.
        assert!(!sender_allowed(
            "anything",
            &["...".to_owned(), "-_-".to_owned()]
        ));
    }

    #[tokio::test]
    async fn commits_only_after_everything_is_enqueued() {
        let mut mail = MockMailSource::new();
        mail.expect_fetch_new().returning(|| {
            Ok(batch(vec![message(
                "x@archiveofourown.org",
                "https://archiveofourown.org/works/5",
            )]))
        });
        let mut jobs = MockJobRepository::new();
        let mut seq = mockall::Sequence::new();
        jobs.expect_enqueue()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|work, source, now| {
                Ok(EnqueueOutcome::Created(Job::new(
                    JobId::new(1),
                    work,
                    source,
                    now,
                )))
            });
        mail.expect_commit()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_| Ok(()));

        let ingestor = Ingestor::new(Arc::new(mail), Arc::new(jobs), clock(), config());
        let report = ingestor.run_once().await.unwrap();
        assert_eq!((report.messages, report.enqueued), (1, 1));
    }

    #[tokio::test]
    async fn a_repository_failure_leaves_the_batch_uncommitted() {
        let mut mail = MockMailSource::new();
        mail.expect_fetch_new().returning(|| {
            Ok(batch(vec![message(
                "x@archiveofourown.org",
                "https://archiveofourown.org/works/5",
            )]))
        });
        mail.expect_commit().never();
        let mut jobs = MockJobRepository::new();
        jobs.expect_enqueue()
            .returning(|_, _, _| Err(RepoError::Backend("db down".into())));

        let ingestor = Ingestor::new(Arc::new(mail), Arc::new(jobs), clock(), config());
        assert!(matches!(ingestor.run_once().await, Err(AppError::Repo(_))));
    }

    #[tokio::test]
    async fn a_fetch_failure_commits_nothing() {
        let mut mail = MockMailSource::new();
        mail.expect_fetch_new()
            .returning(|| Err(MailError::Backend("down".into())));
        mail.expect_commit().never();
        let jobs = MockJobRepository::new();
        let ingestor = Ingestor::new(Arc::new(mail), Arc::new(jobs), clock(), config());
        assert!(matches!(ingestor.run_once().await, Err(AppError::Mail(_))));
    }

    #[tokio::test]
    async fn truncation_counts_exactly_the_excess() {
        let link = |n: u32| format!("https://archiveofourown.org/works/{n} ");
        let mut mail = MockMailSource::new();
        mail.expect_fetch_new().returning(move || {
            Ok(batch(vec![
                message(
                    "x@archiveofourown.org",
                    &(1..=5).map(link).collect::<String>(),
                ),
                message(
                    "x@archiveofourown.org",
                    &(10..=11).map(link).collect::<String>(),
                ),
            ]))
        });
        mail.expect_commit().returning(|_| Ok(()));
        let mut jobs = MockJobRepository::new();
        jobs.expect_enqueue()
            .times(4)
            .returning(|work, source, now| {
                Ok(EnqueueOutcome::Created(Job::new(
                    JobId::new(work.to_i64()),
                    work,
                    source,
                    now,
                )))
            });

        let ingestor = Ingestor::new(Arc::new(mail), Arc::new(jobs), clock(), config());
        let report = ingestor.run_once().await.unwrap();
        // First message: 5 links, cap 2 -> 3 dropped. Second: exactly at the cap -> none.
        assert_eq!((report.enqueued, report.truncated_links), (4, 3));
    }

    #[tokio::test]
    async fn filters_senders_and_caps_links() {
        let mut mail = MockMailSource::new();
        mail.expect_fetch_new().returning(|| {
            Ok(batch(vec![
                message("evil@example.com", "https://archiveofourown.org/works/1"),
                message("x@archiveofourown.org", "no links here"),
                message(
                    "x@archiveofourown.org",
                    "https://archiveofourown.org/works/10 https://archiveofourown.org/works/11 https://archiveofourown.org/works/12",
                ),
            ]))
        });
        mail.expect_commit().times(1).returning(|_| Ok(()));
        let mut jobs = MockJobRepository::new();
        jobs.expect_enqueue()
            .times(2)
            .returning(|work, source, now| {
                Ok(EnqueueOutcome::Created(Job::new(
                    JobId::new(work.to_i64()),
                    work,
                    source,
                    now,
                )))
            });

        let ingestor = Ingestor::new(Arc::new(mail), Arc::new(jobs), clock(), config());
        let report = ingestor.run_once().await.unwrap();
        assert_eq!(
            report,
            IngestReport {
                messages: 3,
                from_unlisted_sender: 1,
                without_work_links: 1,
                enqueued: 2,
                already_active: 0,
                truncated_links: 1,
            }
        );
    }
}
