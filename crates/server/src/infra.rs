//! Real-world implementations of the small ports: time, randomness, no-op notifier.

use std::hash::{BuildHasher, Hasher};

use async_trait::async_trait;
use fanfold_app::{
    error::NotifyError,
    ports::{Clock, Jitter, Notifier, NotifyEvent},
};
use fanfold_domain::Timestamp;

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// Retry jitter does not need cryptographic randomness. The standard library seeds
/// every `RandomState` from the OS, which is plenty to desynchronise retries.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemJitter;

impl Jitter for SystemJitter {
    fn unit(&self) -> f64 {
        let bits = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
            >> 32;
        f64::from(u32::try_from(bits).unwrap_or(0)) / f64::from(u32::MAX)
    }
}

/// Used when no notification channel is configured: events go to the log only.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogOnlyNotifier;

#[async_trait]
impl Notifier for LogOnlyNotifier {
    async fn notify(&self, event: &NotifyEvent) -> Result<(), NotifyError> {
        // A one-line summary: the full event carries the work's whole metadata.
        let summary = match event {
            NotifyEvent::Downloaded {
                work,
                title,
                updated,
                details,
                ..
            } => format!(
                "{} work {work} \"{title}\"{}",
                if *updated { "updated" } else { "downloaded" },
                details.as_ref().map_or_else(String::new, |d| format!(
                    " ({} new chapters, {})",
                    d.new_chapters.len(),
                    d.path
                ))
            ),
            NotifyEvent::SeriesListed {
                series,
                name,
                queued,
                ..
            } => format!("listed series {series} \"{name}\", queued {queued}"),
            NotifyEvent::GaveUp { target, kind, .. } => format!("gave up on {target} ({kind})"),
            NotifyEvent::SiteDegraded { kind, .. } => format!("site degraded ({kind})"),
            NotifyEvent::SiteRecovered { .. } => "site recovered".to_owned(),
        };
        tracing::info!(%summary, "notification (no channel configured)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_is_a_unit_value_and_actually_varies() {
        let samples: Vec<f64> = (0..200).map(|_| SystemJitter.unit()).collect();
        assert!(samples.iter().all(|s| (0.0..=1.0).contains(s)));
        let distinct = samples
            .iter()
            .map(|s| s.to_bits())
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(
            distinct > 100,
            "only {distinct} distinct values in 200 samples"
        );
    }

    #[test]
    fn the_clock_is_recent() {
        assert!(SystemClock.now().as_second() > 1_700_000_000);
    }
}
