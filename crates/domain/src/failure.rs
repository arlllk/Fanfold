//! Classification of download failures. The classification, not the raw error,
//! drives retry, circuit-breaker and notification behaviour.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The site is down, overloaded or unreachable (5xx, Cloudflare 52x, timeouts).
    SiteUnavailable,
    /// The site asked us to slow down (429, "retry later").
    RateLimited,
    /// The site refuses us (403, Cloudflare challenge/1010).
    Blocked,
    /// Credentials rejected, or the work needs a login we lack.
    AuthFailed,
    /// The work no longer exists or is hidden.
    NotFound,
    /// Unclassified, probably temporary.
    Transient,
    /// Retrying cannot help (bad URL, misconfiguration, unsupported site).
    Permanent,
}

impl FailureKind {
    pub const ALL: [Self; 7] = [
        Self::SiteUnavailable,
        Self::RateLimited,
        Self::Blocked,
        Self::AuthFailed,
        Self::NotFound,
        Self::Transient,
        Self::Permanent,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SiteUnavailable => "site_unavailable",
            Self::RateLimited => "rate_limited",
            Self::Blocked => "blocked",
            Self::AuthFailed => "auth_failed",
            Self::NotFound => "not_found",
            Self::Transient => "transient",
            Self::Permanent => "permanent",
        }
    }

    /// Whether trying again later can plausibly succeed.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::SiteUnavailable | Self::RateLimited | Self::Blocked | Self::Transient
        )
    }

    /// Failures that say something about the *site*, not about this one job.
    /// These feed the circuit breaker.
    #[must_use]
    pub const fn is_site_wide(self) -> bool {
        matches!(
            self,
            Self::SiteUnavailable | Self::RateLimited | Self::Blocked
        )
    }

    /// Site-wide trouble must not burn a job's retry budget: when AO3 has a bad
    /// day, every queued job would otherwise be marked failed.
    #[must_use]
    pub const fn consumes_attempt(self) -> bool {
        !matches!(self, Self::SiteUnavailable | Self::RateLimited)
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown failure kind: {0}")]
pub struct UnknownFailureKind(pub String);

impl FromStr for FailureKind {
    type Err = UnknownFailureKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| UnknownFailureKind(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_roundtrip_for_every_kind() {
        for kind in FailureKind::ALL {
            assert_eq!(kind.as_str().parse::<FailureKind>(), Ok(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        assert!("nope".parse::<FailureKind>().is_err());
    }

    #[test]
    fn policy_table() {
        use FailureKind::*;
        let table = [
            // kind, retryable, site_wide, consumes_attempt
            (SiteUnavailable, true, true, false),
            (RateLimited, true, true, false),
            (Blocked, true, true, true),
            (AuthFailed, false, false, true),
            (NotFound, false, false, true),
            (Transient, true, false, true),
            (Permanent, false, false, true),
        ];
        for (kind, retryable, site_wide, consumes) in table {
            assert_eq!(kind.is_retryable(), retryable, "{kind}");
            assert_eq!(kind.is_site_wide(), site_wide, "{kind}");
            assert_eq!(kind.consumes_attempt(), consumes, "{kind}");
        }
    }
}
