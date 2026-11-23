//! The index against a small library: what each kind of token finds, and how it ranks.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use fanfold_adapter_search::Library;
use fanfold_app::{
    ports::{BookRecord, LibraryEntry, SearchIndex, SuggestionKind},
    search::{SearchQuery, TextField},
};
use fanfold_domain::{
    Timestamp,
    metadata::{Author, SeriesPart, Tag, TagKind, WorkFacts, WorkStats},
    work::{SeriesId, WorkId},
};

struct Book {
    n: u64,
    title: &'static str,
    author: &'static str,
    tags: &'static [(TagKind, &'static str)],
    series: Option<&'static str>,
    words: u64,
    rating: &'static str,
    complete: bool,
    language: &'static str,
    summary: &'static str,
}

const NONE: Book = Book {
    n: 0,
    title: "",
    author: "someone",
    tags: &[],
    series: None,
    words: 1_000,
    rating: "General Audiences",
    complete: true,
    language: "English",
    summary: "",
};

fn entry(b: &Book) -> LibraryEntry {
    LibraryEntry {
        book: BookRecord {
            work: WorkId::new(b.n).unwrap(),
            file_name: format!("{}.epub", b.n),
            title: b.title.into(),
            updated_at: Timestamp::from_second(i64::try_from(b.n).unwrap()).unwrap(),
        },
        facts: Some(WorkFacts {
            title: b.title.into(),
            byline: b.author.into(),
            summary_html: format!("<p>{}</p>", b.summary),
            language: b.language.into(),
            rating: b.rating.into(),
            restricted: b.n == 5,
            status: String::new(),
            published_on: None,
            updated_on: None,
            chapters_posted: 3,
            chapters_total: b.complete.then_some(3),
            words: b.words,
        }),
        authors: vec![Author {
            user: b.author.into(),
            pseud: b.author.into(),
        }],
        tags: b
            .tags
            .iter()
            .map(|(kind, name)| Tag {
                kind: *kind,
                name: (*name).into(),
            })
            .collect(),
        series: b
            .series
            .map(|name| SeriesPart {
                id: SeriesId::new(77).unwrap(),
                name: name.into(),
                part: Some(1),
            })
            .into_iter()
            .collect(),
        stats: WorkStats::default(),
        last_checked: None,
    }
}

/// Built once: the tests only read it.
fn library() -> &'static Library {
    static LIBRARY: std::sync::LazyLock<Library> = std::sync::LazyLock::new(build);
    &LIBRARY
}

fn build() -> Library {
    use TagKind::{Character, Fandom, Freeform, Relationship, Warning};
    let books = [
        Book {
            n: 1,
            title: "The Lighthouse Keeper",
            author: "quillandink",
            tags: &[
                (Fandom, "Original Work"),
                (Relationship, "Ada Marsh/Ben Ward"),
                (Freeform, "Slow Burn"),
                (Warning, "No Archive Warnings Apply"),
            ],
            series: Some("The Harbor Saga"),
            words: 120_000,
            complete: false,
            summary: "Ada keeps the light.",
            ..NONE
        },
        Book {
            n: 2,
            title: "Burn Slow",
            author: "Amy",
            tags: &[
                (Fandom, "Pokémon"),
                (Character, "Ash Ketchum"),
                (Warning, "Major Character Death"),
            ],
            words: 5_000,
            rating: "Explicit",
            summary: "Nothing about lighthouses here.",
            ..NONE
        },
        Book {
            n: 3,
            title: "Quill",
            author: "zed",
            tags: &[(Fandom, "Original Work"), (Freeform, "Lighthouse")],
            words: 40_000,
            rating: "Mature",
            language: "Español",
            ..NONE
        },
        Book {
            n: 4,
            title: "Harbor Lights",
            author: "quillandink",
            tags: &[(Fandom, "Original Work")],
            series: Some("The Harbor Saga"),
            words: 300_000,
            rating: "Teen And Up Audiences",
            ..NONE
        },
        Book {
            n: 5,
            title: "Locked Away",
            author: "zed",
            rating: "Not Rated",
            ..NONE
        },
    ];
    Library::build(&books.iter().map(entry).collect::<Vec<_>>()).unwrap()
}

fn find(lib: &Library, query: &str) -> Vec<u64> {
    lib.find(&SearchQuery::parse(query))
        .unwrap()
        .iter()
        .map(|h| h.work.get())
        .collect()
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

#[test]
fn no_query_finds_everything() {
    assert_eq!(sorted(find(library(), "")), [1, 2, 3, 4, 5]);
}

#[test]
fn a_title_word_outranks_the_same_word_in_tags_or_summaries() {
    let lib = library();
    let found = find(lib, "lighthouse ");
    assert_eq!(
        found.first(),
        Some(&1),
        "title, then tag, then summary: {found:?}"
    );
    assert_eq!(
        sorted(found.clone()),
        [1, 2, 3],
        "the summary's \"lighthouses\" is one typo away"
    );
    assert_eq!(found.last(), Some(&2), "a summary match ranks last");
}

#[test]
fn typos_are_forgiven_on_long_words_only() {
    let lib = library();
    assert_eq!(
        find(lib, "lighthuose ").first(),
        Some(&1),
        "a transposition"
    );
    assert_eq!(find(lib, "ligthouse kepeer ").first(), Some(&1));
    assert_eq!(
        find(lib, "brn "),
        Vec::<u64>::new(),
        "three letters: exact only"
    );
}

#[test]
fn the_word_being_typed_is_a_prefix() {
    let lib = library();
    assert_eq!(find(lib, "lightho").first(), Some(&1));
    assert_eq!(
        sorted(find(lib, "harb")),
        [1, 4],
        "series and title start with it"
    );
    assert_eq!(
        find(lib, "harb "),
        Vec::<u64>::new(),
        "a finished word is not a prefix"
    );
}

#[test]
fn every_word_must_match_somewhere() {
    let lib = library();
    assert_eq!(find(lib, "harbor keeper "), [1]);
    assert_eq!(find(lib, "harbor ash "), Vec::<u64>::new());
}

#[test]
fn a_key_searches_one_field() {
    let lib = library();
    assert_eq!(
        sorted(find(lib, "author:quill")),
        [1, 4],
        "a prefix of the author"
    );
    assert_eq!(find(lib, "title:quill "), [3]);
    assert_eq!(find(lib, "fandom:pokemon "), [2], "accents fold");
    assert_eq!(find(lib, "char:ketchum "), [2]);
    assert_eq!(find(lib, "ship:marsh "), [1]);
    assert_eq!(
        find(lib, "tag:\"slow burn\""),
        [1],
        "any kind of tag, exactly"
    );
    assert_eq!(sorted(find(lib, "lang:espanol ")), [3]);
    assert_eq!(find(lib, "summary:light "), [1]);
}

#[test]
fn a_quoted_value_is_that_exact_value() {
    let lib = library();
    assert_eq!(sorted(find(lib, "author:\"QuillAndInk\"")), [1, 4]);
    assert_eq!(find(lib, "author:\"quill\""), Vec::<u64>::new());
    assert_eq!(sorted(find(lib, "series:\"the harbor saga\"")), [1, 4]);
    assert_eq!(sorted(find(lib, "fandom:\"original work\"")), [1, 3, 4]);
}

#[test]
fn phrases_keep_their_order() {
    let lib = library();
    assert_eq!(find(lib, "\"slow burn\""), [1]);
    assert_eq!(find(lib, "\"burn slow\""), [2]);
    assert_eq!(
        find(lib, "\"lighthouse kee"),
        [1],
        "a phrase still being typed"
    );
}

#[test]
fn filters_and_exclusions() {
    let lib = library();
    assert_eq!(sorted(find(lib, "rating:m,e")), [2, 3]);
    assert_eq!(find(lib, "rating:nr"), [5]);
    assert_eq!(sorted(find(lib, "words:>=100k")), [1, 4]);
    assert_eq!(sorted(find(lib, "words:10k..150k")), [1, 3]);
    assert_eq!(
        find(lib, "words:<5k"),
        [5],
        "below 5,000: 5,000 itself is out"
    );
    assert_eq!(find(lib, "is:wip"), [1]);
    assert_eq!(find(lib, "is:restricted"), [5]);
    assert_eq!(sorted(find(lib, "-is:complete")), [1]);
    assert_eq!(
        sorted(find(lib, "-warn:mcd")),
        [1, 3, 4, 5],
        "AO3's warning by its short name"
    );
    assert_eq!(
        sorted(find(lib, "-author:\"quillandink\" -rating:nr")),
        [2, 3]
    );
    assert_eq!(find(lib, "author:quillandink -is:wip"), [4]);
}

#[test]
fn punctuation_alone_matches_nothing_and_excludes_nothing() {
    let lib = library();
    assert_eq!(find(lib, "!!! "), Vec::<u64>::new());
    assert_eq!(sorted(find(lib, "-!!!")), [1, 2, 3, 4, 5]);
}

#[test]
fn filters_alone_do_not_rank() {
    let hits = library()
        .find(&SearchQuery::parse("rating:g,t,m,e,nr"))
        .unwrap();
    assert!(hits.iter().all(|h| h.score == 0.0), "{hits:?}");
}

#[test]
fn suggestions_start_like_the_typing_and_count_books() {
    let lib = library();
    let found = lib.suggest(TextField::Any, "quil", 8).unwrap();
    let first = &found[0];
    assert_eq!(
        (
            first.kind,
            first.label.as_str(),
            first.value.as_str(),
            first.books
        ),
        (SuggestionKind::Author, "quillandink", "quillandink", 2),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|s| s.kind == SuggestionKind::Book && s.work == WorkId::new(3)),
        "the book titled Quill: {found:?}"
    );

    let authors = lib.suggest(TextField::Author, "qui", 8).unwrap();
    assert!(authors.iter().all(|s| s.kind == SuggestionKind::Author));
    assert_eq!(authors.len(), 1);

    let fandoms = lib.suggest(TextField::Fandom, "pok", 8).unwrap();
    assert_eq!(fandoms[0].label, "Pokémon");
    assert_eq!(
        fandoms[0].value, "pokémon",
        "the exact value keeps its accent"
    );

    assert_eq!(lib.suggest(TextField::Any, "", 8).unwrap(), vec![]);
    assert_eq!(
        lib.suggest(TextField::Any, "orig", 1).unwrap().len(),
        1,
        "the limit"
    );
    assert_eq!(
        lib.suggest(TextField::Series, "harbr", 8).unwrap()[0].label,
        "The Harbor Saga",
        "a typo in a later word"
    );
}

mod properties {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn adding_a_token_never_finds_more(
            base in "((author|tag|title|series):)?[a-z]{0,8}( [a-z]{0,8})?",
            extra in "-?((rating:[gtmen]{1,2})|(words:[<>]=?[0-9]{1,3}k)|(is:(complete|wip))|[a-z]{2,8}|author:\"[a-z]{3,10}\")",
        ) {
            let lib = library();
            let narrow = |q: &str| {
                let mut q = SearchQuery::parse(q);
                q.typing = false;
                sorted(lib.find(&q).unwrap().iter().map(|h| h.work.get()).collect())
            };
            let before = narrow(&base);
            let after = narrow(&format!("{base} {extra}"));
            prop_assert!(after.iter().all(|w| before.contains(w)), "{after:?} not within {before:?}");
        }

        #[test]
        fn any_query_is_answered(query in "\\PC{0,60}") {
            let lib = library();
            let found = lib.find(&SearchQuery::parse(&query)).unwrap();
            prop_assert!(found.len() <= 5);
            prop_assert!(found.iter().all(|h| h.score.is_finite()));
            let _ = lib.suggest(TextField::Any, &query, 8).unwrap();
        }
    }
}
