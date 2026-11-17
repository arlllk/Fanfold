//! Use cases behind the browser-session page: import cookies, look at them, forget them.

use std::{sync::Arc, time::Duration};

use fanfold_domain::{
    Timestamp,
    session::{BrowserSession, CookieStatus, ImportError, parse_import},
};
use thiserror::Error;

use crate::{
    error::RepoError,
    ports::{Clock, SessionStore},
};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionError {
    #[error(transparent)]
    Import(#[from] ImportError),
    #[error(
        "no User-Agent: import a HAR file (it carries one) or fill in the User-Agent field; \
         Cloudflare ties its clearance cookie to it"
    )]
    NeedUserAgent,
    #[error(transparent)]
    Repo(RepoError),
}

/// A cookie as the page shows it. The value is deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieView {
    pub name: String,
    pub domain: String,
    pub expires: Option<Timestamp>,
    pub status: CookieStatus,
    pub http_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub user_agent: String,
    pub updated_at: Timestamp,
    /// Soonest expiry among the cookies still alive.
    pub next_expiry: Option<Timestamp>,
    /// Soonest expiry first; session cookies last.
    pub cookies: Vec<CookieView>,
}

impl SessionView {
    #[must_use]
    pub fn of(session: &BrowserSession, now: Timestamp, warn_within: Duration) -> Self {
        let mut cookies: Vec<CookieView> = session
            .cookies
            .iter()
            .map(|c| CookieView {
                name: c.name.clone(),
                domain: c.domain.clone(),
                expires: c.expires,
                status: c.status(now, warn_within),
                http_only: c.http_only,
            })
            .collect();
        cookies.sort_by_key(|c| (c.expires.is_none(), c.expires, c.name.clone()));
        Self {
            user_agent: session.user_agent.clone(),
            updated_at: session.updated_at,
            next_expiry: session.next_expiry(now),
            cookies,
        }
    }
}

/// What an import did, for the confirmation message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub cookies_in_import: usize,
    pub user_agent_changed: bool,
    pub view: SessionView,
}

pub struct SessionAdmin {
    store: Arc<dyn SessionStore>,
    clock: Arc<dyn Clock>,
    /// The site cookies are kept for (`archiveofourown.org`); everything else is dropped.
    site: String,
    warn_within: Duration,
}

impl std::fmt::Debug for SessionAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAdmin")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}

impl SessionAdmin {
    #[must_use]
    pub fn new(
        store: Arc<dyn SessionStore>,
        clock: Arc<dyn Clock>,
        site: impl Into<String>,
        warn_within: Duration,
    ) -> Self {
        Self {
            store,
            clock,
            site: site.into(),
            warn_within,
        }
    }

    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    pub async fn view(&self) -> Result<Option<SessionView>, SessionError> {
        let now = self.clock.now();
        Ok(self
            .store
            .load()
            .await
            .map_err(SessionError::Repo)?
            .map(|s| SessionView::of(&s, now, self.warn_within)))
    }

    /// Merge pasted cookies (a `cookies.txt` or a HAR) into the stored session. Cookies in
    /// the import replace same-named ones; others are kept, so a HAR and a `cookies.txt`
    /// can be imported one after the other. The `User-Agent` comes from `user_agent` if
    /// given, else from the import (a HAR has one), else from the stored session.
    pub async fn import(
        &self,
        text: &str,
        user_agent: Option<&str>,
    ) -> Result<ImportReport, SessionError> {
        let now = self.clock.now();
        let imported = parse_import(text, &self.site)?;
        let existing = self.store.load().await.map_err(SessionError::Repo)?;

        let given = user_agent.map(str::trim).filter(|u| !u.is_empty());
        let user_agent = given
            .map(str::to_owned)
            .or(imported.user_agent)
            .or_else(|| existing.as_ref().map(|s| s.user_agent.clone()))
            .ok_or(SessionError::NeedUserAgent)?;
        if !fanfold_domain::session::valid_user_agent(&user_agent) {
            return Err(ImportError::BadUserAgent.into());
        }

        let cookies_in_import = imported.cookies.len();
        let user_agent_changed = existing.as_ref().is_none_or(|s| s.user_agent != user_agent);
        let mut session = existing.unwrap_or_else(|| BrowserSession {
            user_agent: user_agent.clone(),
            cookies: Vec::new(),
            updated_at: now,
        });
        session.user_agent = user_agent;
        session.absorb(imported.cookies, now);
        session.updated_at = now;
        self.store
            .save(&session)
            .await
            .map_err(SessionError::Repo)?;
        Ok(ImportReport {
            cookies_in_import,
            user_agent_changed,
            view: SessionView::of(&session, now, self.warn_within),
        })
    }

    /// Forget everything. Returns whether there was a session.
    pub async fn clear(&self) -> Result<bool, SessionError> {
        self.store.clear().await.map_err(SessionError::Repo)
    }
}
