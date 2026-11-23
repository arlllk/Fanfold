//! Browsing (`fanfold_app::browse`) over the in-memory store and the real index.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use fanfold_adapter_search::TantivyIndexer;
use fanfold_app::{
    browse::{GroupKind, LibraryBrowser, LibrarySort},
    error::SearchError,
    ports::{
        BookCatalog, BookRecord, LibraryEntry, MetadataStore, SearchIndex, SearchIndexer,
        SuggestionKind,
    },
    search::{Rating, SearchQuery},
};
use fanfold_domain::{
    Timestamp,
    metadata::{Author, FollowSource, SeriesFacts, SeriesMetadata, SeriesPart},
    work::{SeriesId, WorkId},
};
use fanfold_testkit::{contracts::observed_work, memory::InMemoryJobs};

fn t(s: i64) -> Timestamp {
    Timestamp::from_second(s).unwrap()
}

fn work(n: u64) -> WorkId {
    WorkId::new(n).unwrap()
}

/// Counts the indexes it builds.
#[derive(Default)]
struct Counting {
    builds: AtomicUsize,
}

#[async_trait]
impl SearchIndexer for Counting {
    async fn build(&self, books: &[LibraryEntry]) -> Result<Arc<dyn SearchIndex>, SearchError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        TantivyIndexer.build(books).await
    }
}

/// Books 1..=4 by two authors; 1 and 2 in series 77 (listed with a third, missing part);
/// 3 updated last; 4 never observed.
async fn library(store: &InMemoryJobs) {
    for (n, title, author, words, day, series) in [
        (
            1,
            "The Lighthouse Keeper",
            "quillandink",
            120_000,
            1,
            Some(1),
        ),
        (2, "Harbor Lights", "quillandink", 30_000, 5, Some(2)),
        (3, "Burn Slow", "amy", 5_000, 9, None),
    ] {
        let mut w = observed_work(n);
        w.facts.title = title.into();
        w.facts.byline = author.into();
        w.facts.words = words;
        w.facts.updated_on = Some(format!("2026-10-0{day}").parse().unwrap());
        w.facts.rating = if n == 3 { "Explicit" } else { "Mature" }.into();
        w.facts.chapters_total = (n != 1).then_some(w.facts.chapters_posted);
        w.authors = vec![Author {
            user: author.into(),
            pseud: author.into(),
        }];
        w.series = series
            .map(|part| SeriesPart {
                id: SeriesId::new(77).unwrap(),
                name: "The Harbor Saga".into(),
                part: Some(part),
            })
            .into_iter()
            .collect();
        store.record_work(&w, t(100), None).await.unwrap();
    }
    for n in 1..=4 {
        store
            .upsert(&BookRecord {
                work: work(n),
                file_name: format!("{n}.epub"),
                title: format!("Saved {n}"),
                updated_at: t(i64::try_from(n).unwrap()),
            })
            .await
            .unwrap();
    }
    store
        .follow_series(SeriesId::new(77).unwrap(), FollowSource::Manual, t(1))
        .await
        .unwrap();
    store
        .record_series(
            &SeriesMetadata {
                series: SeriesId::new(77).unwrap(),
                facts: SeriesFacts {
                    name: "The Harbor Saga".into(),
                    summary_html: String::new(),
                    complete: Some(false),
                    works: vec![work(1), work(2), work(9)],
                },
                raw: serde_json::json!({}),
            },
            t(2),
            None,
        )
        .await
        .unwrap();
}

async fn browser() -> (Arc<InMemoryJobs>, Arc<Counting>, LibraryBrowser) {
    let store = Arc::new(InMemoryJobs::new());
    library(&store).await;
    let indexer = Arc::new(Counting::default());
    let browser = LibraryBrowser::new(Arc::clone(&store) as _, Arc::clone(&indexer) as _);
    (store, indexer, browser)
}

fn ids(list: &[LibraryEntry]) -> Vec<u64> {
    list.iter().map(|e| e.book.work.get()).collect()
}

#[tokio::test]
async fn the_index_is_rebuilt_only_when_the_library_changes() {
    let (store, indexer, browser) = browser().await;
    let all = SearchQuery::default();
    browser.books(&all, None, 0, 10).await.unwrap();
    browser.books(&all, None, 0, 10).await.unwrap();
    browser.series(&all).await.unwrap();
    assert_eq!(indexer.builds.load(Ordering::SeqCst), 1);

    store.remove(work(4)).await.unwrap();
    let list = browser.books(&all, None, 0, 10).await.unwrap();
    assert_eq!(indexer.builds.load(Ordering::SeqCst), 2);
    assert_eq!(list.totals.books, 3, "the deleted book is gone at once");
}

#[tokio::test]
async fn books_are_ordered_paged_and_counted() {
    let (_, _, browser) = browser().await;
    let all = SearchQuery::default();
    let list = browser.books(&all, None, 0, 10).await.unwrap();
    assert_eq!(list.sort, LibrarySort::Updated);
    assert_eq!(
        ids(&list.entries),
        [3, 2, 1, 4],
        "newest update first; never observed last"
    );
    assert_eq!((list.matching, list.totals.books), (4, 4));
    assert_eq!(list.totals.words, 155_000);
    assert_eq!(list.totals.complete, 2);

    let page = browser
        .books(&all, Some(LibrarySort::Words), 1, 2)
        .await
        .unwrap();
    assert_eq!(ids(&page.entries), [2, 3], "the second and third longest");
    assert_eq!(page.matching, 4);
    let title = browser
        .books(&all, Some(LibrarySort::Title), 0, 10)
        .await
        .unwrap();
    assert_eq!(
        ids(&title.entries),
        [3, 2, 4, 1],
        "Burn, Harbor, Saved 4, The Lighthouse"
    );

    let text = SearchQuery::parse("lighthouse ");
    let found = browser.books(&text, None, 0, 10).await.unwrap();
    assert_eq!(found.sort, LibrarySort::Relevance);
    assert_eq!(ids(&found.entries)[0], 1, "the title match first");

    let filtered = browser
        .books(&SearchQuery::parse("author:\"quillandink\""), None, 0, 10)
        .await
        .unwrap();
    assert_eq!(ids(&filtered.entries), [2, 1]);
    assert_eq!(filtered.totals.books, 4, "totals ignore the search");
    let f = &filtered.facets;
    assert_eq!((f.complete, f.in_progress), (1, 1));
    assert_eq!(
        f.ratings.iter().find(|(r, _)| *r == Rating::Mature),
        Some(&(Rating::Mature, 2))
    );
    assert_eq!(f.languages, vec![("English".to_owned(), 2)]);
}

#[tokio::test]
async fn series_show_every_part_in_order_with_the_missing_ones() {
    let (_, _, browser) = browser().await;
    let all = browser.series(&SearchQuery::default()).await.unwrap();
    assert_eq!(
        all.len(),
        1,
        "{:?}",
        all.iter()
            .map(|s| (s.id, &s.name, s.in_library))
            .collect::<Vec<_>>()
    );
    let saga = &all[0];
    assert_eq!(saga.name, "The Harbor Saga");
    assert!(saga.followed);
    assert_eq!(
        saga.parts
            .iter()
            .map(|p| (p.part, p.work.get(), p.entry.is_some()))
            .collect::<Vec<_>>(),
        [(Some(1), 1, true), (Some(2), 2, true), (Some(3), 9, false)]
    );
    assert_eq!(saga.missing().count(), 1);
    assert_eq!((saga.in_library, saga.words), (2, 150_000));

    let none = browser
        .series(&SearchQuery::parse("author:\"amy\""))
        .await
        .unwrap();
    assert_eq!(none.len(), 0, "no book of the series matches");
    // A finished word: "lights" while still typed is one edit from the start of "lighthouse".
    let one = browser
        .series(&SearchQuery::parse("harbor lights "))
        .await
        .unwrap();
    assert_eq!(one[0].matching, 1);
    assert!(
        browser
            .one_series(SeriesId::new(77).unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        browser
            .one_series(SeriesId::new(78).unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn authors_and_fandoms_group_the_matching_books() {
    let (_, _, browser) = browser().await;
    let authors = browser
        .groups(GroupKind::Author, &SearchQuery::default())
        .await
        .unwrap();
    assert_eq!(
        authors
            .iter()
            .map(|g| (g.label.as_str(), g.books, g.words))
            .collect::<Vec<_>>(),
        [("quillandink", 2, 150_000), ("amy", 1, 5_000)],
        "most books first; the unobserved book has no author"
    );
    assert_eq!(
        authors[0].titles,
        ["Harbor Lights", "The Lighthouse Keeper"]
    );
    let fandoms = browser
        .groups(GroupKind::Fandom, &SearchQuery::parse("rating:e"))
        .await
        .unwrap();
    assert_eq!(fandoms.len(), 1);
    assert_eq!(
        (fandoms[0].value.as_str(), fandoms[0].books),
        ("original work", 1)
    );
}

#[tokio::test]
async fn suggestions_leave_out_what_the_search_already_filters_on() {
    let (_, _, browser) = browser().await;
    let found = browser.suggest(&SearchQuery::parse("quil")).await.unwrap();
    assert_eq!(found[0].kind, SuggestionKind::Author);
    let token = LibraryBrowser::suggestion_token(&found[0]).unwrap();
    assert_eq!(
        SearchQuery::parse("quil").completed_with(token).render(),
        "author:\"quillandink\""
    );
    let again = browser
        .suggest(&SearchQuery::parse("author:\"quillandink\" author:qui"))
        .await
        .unwrap();
    assert_eq!(again, vec![], "already filtered on");
    assert_eq!(
        browser.suggest(&SearchQuery::parse("quil ")).await.unwrap(),
        vec![]
    );
}
