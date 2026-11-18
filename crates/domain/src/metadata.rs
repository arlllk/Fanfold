//! What AO3 says about a work or a series at one moment, and how that changes over time.
//!
//! The history is kept as *versions* plus *spans*:
//!
//! * a work version holds the work's scalar facts (title, rating, status, counts, dates, ...)
//!   and is written only when something about the work changed;
//! * every list item (author, tag, series membership, chapter) is a span that opens in the
//!   version where it first appeared and closes in the version where it disappeared, so
//!   "which update added this tag?" is a lookup, not a diff;
//! * statistics (hits, kudos, ...) change on every visit and are kept separately, so they
//!   never count as a change.
//!
//! [`plan_work_update`] is the one place that decides what a new observation changes.

use std::{collections::HashSet, fmt, hash::Hash, str::FromStr};

pub use jiff::civil::Date;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::work::{SeriesId, WorkId};

/// The kinds of AO3 tag, in AO3's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagKind {
    Fandom,
    Category,
    Warning,
    Relationship,
    Character,
    Freeform,
}

impl TagKind {
    pub const ALL: [Self; 6] = [
        Self::Fandom,
        Self::Category,
        Self::Warning,
        Self::Relationship,
        Self::Character,
        Self::Freeform,
    ];

    /// Stable storage name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fandom => "fandom",
            Self::Category => "category",
            Self::Warning => "warning",
            Self::Relationship => "relationship",
            Self::Character => "character",
            Self::Freeform => "freeform",
        }
    }
}

impl fmt::Display for TagKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown tag kind: {0}")]
pub struct UnknownTagKind(pub String);

impl FromStr for TagKind {
    type Err = UnknownTagKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownTagKind(s.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Tag {
    pub kind: TagKind,
    pub name: String,
}

/// One creator of a work: the AO3 account and the pseud it was posted under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author {
    pub user: String,
    pub pseud: String,
}

/// A work's place in a series, as the work's page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SeriesPart {
    pub id: SeriesId,
    pub name: String,
    pub part: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Chapter {
    pub number: u32,
    pub title: String,
    /// AO3's chapter id (from its URL), stable across renames.
    pub ao3_id: Option<u64>,
    pub posted_on: Option<Date>,
}

/// Counters that change on every visit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStats {
    pub hits: Option<u64>,
    pub kudos: Option<u64>,
    pub comments: Option<u64>,
    pub bookmarks: Option<u64>,
}

/// The scalar facts about a work; a change to any of them makes a new version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFacts {
    pub title: String,
    /// The creators as AO3 shows them in one line, e.g. "A, B".
    pub byline: String,
    pub summary_html: String,
    pub language: String,
    pub rating: String,
    /// Visible to logged-in users only.
    pub restricted: bool,
    pub status: String,
    pub published_on: Option<Date>,
    pub updated_on: Option<Date>,
    pub chapters_posted: u32,
    /// `None` when AO3 shows "?" (the author has not said how many).
    pub chapters_total: Option<u32>,
    pub words: u64,
}

impl WorkFacts {
    /// Whether AO3 shows its adult-content warning before the work: it does for Mature,
    /// Explicit and unrated works.
    #[must_use]
    pub fn adult(&self) -> bool {
        matches!(self.rating.as_str(), "Mature" | "Explicit" | "Not Rated")
    }
}

/// Everything one fetch of a work told us.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkMetadata {
    pub work: WorkId,
    pub facts: WorkFacts,
    pub authors: Vec<Author>,
    pub tags: Vec<Tag>,
    pub series: Vec<SeriesPart>,
    pub chapters: Vec<Chapter>,
    pub stats: WorkStats,
    /// The downloader's full answer, kept as is so it can be re-read later.
    pub raw: serde_json::Value,
}

/// What is currently recorded about a work: the latest version's facts and the open spans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkState {
    pub facts: WorkFacts,
    pub authors: Vec<Author>,
    pub tags: Vec<Tag>,
    pub series: Vec<SeriesPart>,
    pub chapters: Vec<Chapter>,
}

impl WorkMetadata {
    #[must_use]
    pub fn state(&self) -> WorkState {
        WorkState {
            facts: self.facts.clone(),
            authors: self.authors.clone(),
            tags: self.tags.clone(),
            series: self.series.clone(),
            chapters: self.chapters.clone(),
        }
    }
}

/// Spans to close and to open for one list. `opened` carries each item's position in the
/// new list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanChanges<T> {
    pub closed: Vec<T>,
    pub opened: Vec<(u32, T)>,
}

impl<T> SpanChanges<T> {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.closed.is_empty() && self.opened.is_empty()
    }
}

/// What recording an observation changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkUpdate {
    /// Write a new version: the facts changed, or a span opens or closes in it.
    pub new_version: bool,
    pub authors: SpanChanges<Author>,
    pub tags: SpanChanges<Tag>,
    pub series: SpanChanges<SeriesPart>,
    pub chapters: SpanChanges<Chapter>,
}

fn diff<T: Eq + Hash + Clone>(old: &[T], new: &[T]) -> SpanChanges<T> {
    let before: HashSet<&T> = old.iter().collect();
    let mut seen = HashSet::new();
    let opened = new
        .iter()
        .enumerate()
        .filter(|(_, item)| seen.insert(*item) && !before.contains(item))
        .map(|(i, item)| (u32::try_from(i).unwrap_or(u32::MAX), item.clone()))
        .collect();
    let after: HashSet<&T> = new.iter().collect();
    let mut gone = HashSet::new();
    let closed = old
        .iter()
        .filter(|item| !after.contains(item) && gone.insert(*item))
        .cloned()
        .collect();
    SpanChanges { closed, opened }
}

/// Compare a new observation with what is recorded. With nothing recorded yet, everything
/// opens in a first version.
#[must_use]
pub fn plan_work_update(current: Option<&WorkState>, observed: &WorkState) -> WorkUpdate {
    let empty = WorkState {
        facts: observed.facts.clone(),
        authors: vec![],
        tags: vec![],
        series: vec![],
        chapters: vec![],
    };
    let base = current.unwrap_or(&empty);
    let update = WorkUpdate {
        new_version: false,
        authors: diff(&base.authors, &observed.authors),
        tags: diff(&base.tags, &observed.tags),
        series: diff(&base.series, &observed.series),
        chapters: diff(&base.chapters, &observed.chapters),
    };
    let lists_changed = !(update.authors.is_empty()
        && update.tags.is_empty()
        && update.series.is_empty()
        && update.chapters.is_empty());
    WorkUpdate {
        new_version: current.is_none() || base.facts != observed.facts || lists_changed,
        ..update
    }
}

/// Everything one listing of a series told us.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesMetadata {
    pub series: SeriesId,
    pub facts: SeriesFacts,
    pub raw: serde_json::Value,
}

/// The facts about a series; a change to any of them makes a new series version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesFacts {
    pub name: String,
    pub summary_html: String,
    /// `None` when the page did not say.
    pub complete: Option<bool>,
    /// The works, in series order.
    pub works: Vec<WorkId>,
}

/// How a series came to be followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowSource {
    /// Its link was added by hand: listed, and every work queued.
    Manual,
    /// It was linked in an AO3 email: only the works the emails mention are fetched.
    Mail,
}

impl FollowSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Mail => "mail",
        }
    }
}

impl FromStr for FollowSource {
    type Err = UnknownTagKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "manual" => Ok(Self::Manual),
            "mail" => Ok(Self::Mail),
            other => Err(UnknownTagKind(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn facts() -> WorkFacts {
        WorkFacts {
            title: "The Keeper and the Logbook".into(),
            byline: "quillandink".into(),
            summary_html: "<p>Second year.</p>".into(),
            language: "English".into(),
            rating: "Teen And Up Audiences".into(),
            restricted: false,
            status: "Completed".into(),
            published_on: Some(jiff::civil::date(2026, 10, 8)),
            updated_on: Some(jiff::civil::date(2026, 10, 20)),
            chapters_posted: 19,
            chapters_total: Some(19),
            words: 120_000,
        }
    }

    fn tag(kind: TagKind, name: &str) -> Tag {
        Tag {
            kind,
            name: name.into(),
        }
    }

    fn state(tags: Vec<Tag>) -> WorkState {
        WorkState {
            facts: facts(),
            authors: vec![Author {
                user: "quillandink".into(),
                pseud: "quillandink".into(),
            }],
            tags,
            series: vec![SeriesPart {
                id: SeriesId::new(2_000_001).unwrap(),
                name: "The Harbor Saga".into(),
                part: Some(2),
            }],
            chapters: vec![],
        }
    }

    #[test]
    fn the_first_observation_opens_everything_in_a_first_version() {
        let now = state(vec![tag(TagKind::Freeform, "Fluff")]);
        let update = plan_work_update(None, &now);
        assert!(update.new_version);
        assert_eq!(update.tags.opened, [(0, tag(TagKind::Freeform, "Fluff"))]);
        assert_eq!(update.series.opened.len(), 1);
        assert_eq!(update.tags.closed, []);
    }

    #[test]
    fn an_identical_observation_changes_nothing() {
        let now = state(vec![tag(TagKind::Freeform, "Fluff")]);
        let update = plan_work_update(Some(&now), &now.clone());
        assert!(!update.new_version);
        assert!(update.tags.is_empty() && update.authors.is_empty());
    }

    #[test]
    fn a_new_tag_alone_is_a_new_version_with_one_span_opened() {
        let before = state(vec![tag(TagKind::Freeform, "Fluff")]);
        let after = state(vec![
            tag(TagKind::Freeform, "Fluff"),
            tag(TagKind::Freeform, "Found Family"),
        ]);
        let update = plan_work_update(Some(&before), &after);
        assert!(
            update.new_version,
            "AO3 does not bump the date for tag edits"
        );
        assert_eq!(
            update.tags.opened,
            [(1, tag(TagKind::Freeform, "Found Family"))]
        );
        assert_eq!(update.tags.closed, []);
    }

    #[test]
    fn a_removed_tag_closes_its_span_and_a_reorder_is_not_a_change() {
        let before = state(vec![tag(TagKind::Fandom, "A"), tag(TagKind::Fandom, "B")]);
        let reordered = state(vec![tag(TagKind::Fandom, "B"), tag(TagKind::Fandom, "A")]);
        assert!(!plan_work_update(Some(&before), &reordered).new_version);
        let removed = state(vec![tag(TagKind::Fandom, "B")]);
        let update = plan_work_update(Some(&before), &removed);
        assert_eq!(update.tags.closed, [tag(TagKind::Fandom, "A")]);
        assert!(update.new_version);
    }

    #[test]
    fn the_same_name_under_another_kind_is_another_tag() {
        let before = state(vec![tag(TagKind::Character, "Harbor")]);
        let after = state(vec![tag(TagKind::Freeform, "Harbor")]);
        let update = plan_work_update(Some(&before), &after);
        assert_eq!(update.tags.closed.len(), 1);
        assert_eq!(update.tags.opened.len(), 1);
    }

    #[test]
    fn a_fact_change_is_a_new_version_without_span_changes() {
        let before = state(vec![]);
        let mut after = before.clone();
        after.facts.restricted = true;
        let update = plan_work_update(Some(&before), &after);
        assert!(update.new_version && update.tags.is_empty());
    }

    #[test]
    fn a_moved_part_closes_the_old_membership_and_opens_the_new_one() {
        let before = state(vec![]);
        let mut after = before.clone();
        after.series[0].part = Some(3);
        let update = plan_work_update(Some(&before), &after);
        assert_eq!(update.series.closed[0].part, Some(2));
        assert_eq!(update.series.opened[0].1.part, Some(3));
    }

    #[test]
    fn adult_follows_the_rating() {
        let mut f = facts();
        assert!(!f.adult());
        for rating in ["Mature", "Explicit", "Not Rated"] {
            f.rating = rating.into();
            assert!(f.adult(), "{rating}");
        }
    }

    #[test]
    fn storage_names_roundtrip() {
        for kind in TagKind::ALL {
            assert_eq!(kind.as_str().parse::<TagKind>(), Ok(kind));
        }
        for source in [FollowSource::Manual, FollowSource::Mail] {
            assert_eq!(source.as_str().parse::<FollowSource>(), Ok(source));
        }
    }

    proptest! {
        /// Applying the planned changes to the recorded spans gives exactly the observed set.
        #[test]
        fn applying_the_plan_reproduces_the_observation(
            old in proptest::collection::vec(0u8..12, 0..10),
            new in proptest::collection::vec(0u8..12, 0..10),
        ) {
            let to_tags = |v: &[u8]| v.iter().map(|n| tag(TagKind::Freeform, &n.to_string())).collect::<Vec<_>>();
            let (before, after) = (state(to_tags(&old)), state(to_tags(&new)));
            let update = plan_work_update(Some(&before), &after);
            let mut open: HashSet<Tag> = before.tags.iter().cloned().collect();
            for gone in &update.tags.closed {
                prop_assert!(open.remove(gone), "closes only what is open");
            }
            for (_, added) in &update.tags.opened {
                prop_assert!(open.insert(added.clone()), "opens only what is not open");
            }
            let expected: HashSet<Tag> = after.tags.iter().cloned().collect();
            prop_assert_eq!(open, expected);
            let same: HashSet<_> = old.iter().collect::<HashSet<_>>();
            let same_new: HashSet<_> = new.iter().collect::<HashSet<_>>();
            prop_assert_eq!(update.new_version, same != same_new);
        }
    }
}
