//! [`SearchIndexer`] on tantivy: the library's books in an in-memory index.
//!
//! Two indexes are built from the shelf, both in RAM (nothing is ever written to disk; a
//! library of thousands of books takes milliseconds to index):
//!
//! * **books**, one document per book. Every text field is tokenized the same way (words,
//!   lower-cased, accents folded: "Pokémon" is "pokemon"), with positions for phrases, and
//!   every value a filter can name exactly (an author, a series, a tag...) is also kept whole,
//!   lower-cased, in a `*_key` field;
//! * **names**, one document per distinct author, series, tag, warning, language and book,
//!   with how many books have it: what the search box suggests.
//!
//! A word matches a field three ways, best first: exactly (scored by BM25, times the field's
//! weight), as the start of a word while it is still being typed, and within one or two typos
//! (a constant, lower score). Every word must match somewhere (AND); a `-` token must not.
//! The query language itself (what a token means) is `fanfold_app::search`'s.

use std::{
    collections::{BTreeMap, HashMap},
    ops::Bound,
    sync::Arc,
};

use async_trait::async_trait;
use fanfold_app::{
    error::SearchError,
    ports::{Hit, LibraryEntry, SearchIndex, SearchIndexer, Suggestion, SuggestionKind},
    search::{Clause, Flag, Rating, SearchQuery, TextField, Token},
};
use fanfold_domain::{metadata::TagKind, work::WorkId};
use tantivy::{
    DocAddress, Index, IndexReader, ReloadPolicy, Score, Searcher, TantivyDocument, Term,
    collector::TopDocs,
    query::{
        AllQuery, BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhrasePrefixQuery, PhraseQuery,
        Query, RangeQuery, TermQuery,
    },
    schema::{
        FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing,
        TextOptions,
    },
    tokenizer::{
        AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
        TokenStream as _,
    },
};

/// The analyzer every text field and every query word goes through.
const FOLD: &str = "fold";
/// Words longer than this are dropped (nobody searches for one).
const LONGEST_WORD: usize = 40;
/// The indexing memory budget: tantivy's minimum for one thread, which is plenty.
const WRITER_BUDGET: usize = 15_000_000;
/// How many names a suggestion search looks at before ranking them.
const SUGGEST_POOL: usize = 64;

/// Weights of the text fields when a word is searched everywhere.
const WEIGHTS: [(TextField, Score); 8] = [
    (TextField::Title, 4.0),
    (TextField::Author, 3.0),
    (TextField::Series, 2.5),
    (TextField::Fandom, 2.0),
    (TextField::Relationship, 2.0),
    (TextField::Character, 1.5),
    (TextField::Tag, 1.0),
    (TextField::Summary, 0.5),
];
/// An exact word scores its BM25 times this; a prefix and a typo get these constants.
const EXACT: Score = 3.0;
const PREFIX: Score = 1.5;
const TYPO: Score = 1.0;

fn error(e: &impl std::fmt::Display) -> SearchError {
    SearchError(e.to_string())
}

fn fold_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(LONGEST_WORD))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

/// The words of `text`, as the index stores them.
fn words(text: &str) -> Vec<String> {
    let mut analyzer = fold_analyzer();
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

/// How many typos a word may have: none for short words, where one typo is another word.
fn typos(word: &str) -> u8 {
    match word.chars().count() {
        0..=3 => 0,
        4..=6 => 1,
        _ => 2,
    }
}

/// Summary HTML as plain text: tags dropped, the common entities decoded.
fn html_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

// ------------------------------------------------------------------------------ the schema

/// One searchable field: its text (tokenized) and, for fields with values, the exact keys.
#[derive(Debug, Clone, Copy)]
struct Pair {
    text: Field,
    key: Option<Field>,
}

#[derive(Debug, Clone)]
struct BookFields {
    work: Field,
    fields: HashMap<TextField, Pair>,
    rating: Field,
    flags: Field,
    words: Field,
}

impl BookFields {
    fn schema() -> (Schema, Self) {
        let mut b = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(FOLD)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let work = b.add_u64_field("work", FAST | STORED);
        let mut fields = HashMap::new();
        for field in TextField::KEYED {
            let name = field.key().unwrap_or("any");
            let key = field
                .has_values()
                .then(|| b.add_text_field(&format!("{name}_key"), STRING));
            fields.insert(
                field,
                Pair {
                    text: b.add_text_field(name, text.clone()),
                    key,
                },
            );
        }
        let rating = b.add_text_field("rating", STRING);
        let flags = b.add_text_field("flags", STRING);
        let words = b.add_u64_field("words", INDEXED | FAST);
        let schema = b.build();
        (
            schema,
            Self {
                work,
                fields,
                rating,
                flags,
                words,
            },
        )
    }

    fn pair(&self, field: TextField) -> Result<Pair, SearchError> {
        self.fields
            .get(&field)
            .copied()
            .ok_or_else(|| SearchError(format!("no index field for {field:?}")))
    }

    fn document(&self, e: &LibraryEntry) -> Result<TantivyDocument, SearchError> {
        let mut doc = TantivyDocument::default();
        doc.add_u64(self.work, e.book.work.get());
        let mut put = |field: TextField, value: &str| -> Result<(), SearchError> {
            let pair = self.pair(field)?;
            doc.add_text(pair.text, value);
            if let Some(key) = pair.key {
                doc.add_text(key, value.to_lowercase());
            }
            Ok(())
        };
        put(TextField::Title, e.title())?;
        for a in &e.authors {
            put(TextField::Author, &a.user)?;
            if a.pseud != a.user {
                put(TextField::Author, &a.pseud)?;
            }
        }
        if e.authors.is_empty()
            && let Some(f) = &e.facts
            && !f.byline.is_empty()
        {
            put(TextField::Author, &f.byline)?;
        }
        for s in &e.series {
            put(TextField::Series, &s.name)?;
        }
        for t in &e.tags {
            let field = match t.kind {
                TagKind::Fandom => Some(TextField::Fandom),
                TagKind::Relationship => Some(TextField::Relationship),
                TagKind::Character => Some(TextField::Character),
                TagKind::Warning => Some(TextField::Warning),
                TagKind::Category | TagKind::Freeform => None,
            };
            if let Some(field) = field {
                put(field, &t.name)?;
            }
            put(TextField::Tag, &t.name)?;
        }
        if let Some(f) = &e.facts {
            if !f.language.is_empty() {
                put(TextField::Language, &f.language)?;
            }
            put(TextField::Summary, &html_text(&f.summary_html))?;
            doc.add_text(self.rating, Rating::from_ao3(&f.rating).code());
            doc.add_u64(self.words, f.words);
            if f.restricted {
                doc.add_text(self.flags, Flag::Restricted.code());
            }
        }
        match e.complete() {
            Some(true) => doc.add_text(self.flags, Flag::Complete.code()),
            Some(false) => doc.add_text(self.flags, Flag::InProgress.code()),
            None => {}
        }
        Ok(doc)
    }
}

#[derive(Debug, Clone, Copy)]
struct NameFields {
    kind: Field,
    label: Field,
    work: Field,
}

impl NameFields {
    fn schema() -> (Schema, Self) {
        let mut b = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(FOLD)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let kind = b.add_text_field("kind", STRING);
        let label = b.add_text_field("label", text);
        let work = b.add_u64_field("doc", FAST);
        let schema = b.build();
        (schema, Self { kind, label, work })
    }
}

const fn kind_code(kind: SuggestionKind) -> &'static str {
    match kind {
        SuggestionKind::Author => "author",
        SuggestionKind::Series => "series",
        SuggestionKind::Fandom => "fandom",
        SuggestionKind::Relationship => "ship",
        SuggestionKind::Character => "char",
        SuggestionKind::Tag => "tag",
        SuggestionKind::Warning => "warn",
        SuggestionKind::Language => "lang",
        SuggestionKind::Book => "book",
    }
}

/// The kinds of name a field suggests (every kind, for a word typed with no key).
const fn kinds_for(field: TextField) -> &'static [SuggestionKind] {
    use SuggestionKind as K;
    match field {
        TextField::Any => &[
            K::Author,
            K::Series,
            K::Fandom,
            K::Relationship,
            K::Character,
            K::Tag,
            K::Book,
        ],
        TextField::Title | TextField::Summary => &[K::Book],
        TextField::Author => &[K::Author],
        TextField::Series => &[K::Series],
        TextField::Fandom => &[K::Fandom],
        TextField::Relationship => &[K::Relationship],
        TextField::Character => &[K::Character],
        TextField::Tag => &[K::Tag, K::Fandom, K::Relationship, K::Character],
        TextField::Warning => &[K::Warning],
        TextField::Language => &[K::Language],
    }
}

/// Every distinct name on the shelf, with how many books have it, keyed by (kind, value).
fn names(shelf: &[LibraryEntry]) -> BTreeMap<(SuggestionKind, String), Suggestion> {
    let mut out: BTreeMap<(SuggestionKind, String), Suggestion> = BTreeMap::new();
    let mut add = |kind: SuggestionKind, label: &str, work: Option<WorkId>| {
        if label.trim().is_empty() {
            return;
        }
        let value = match work {
            Some(w) => w.to_string(),
            None => label.to_lowercase(),
        };
        out.entry((kind, value.clone()))
            .or_insert_with(|| Suggestion {
                kind,
                label: label.to_owned(),
                value: if work.is_some() { String::new() } else { value },
                books: 0,
                work,
            })
            .books += 1;
    };
    for e in shelf {
        add(SuggestionKind::Book, e.title(), Some(e.book.work));
        for a in &e.authors {
            add(SuggestionKind::Author, &a.user, None);
        }
        for s in &e.series {
            add(SuggestionKind::Series, &s.name, None);
        }
        for t in &e.tags {
            let kind = match t.kind {
                TagKind::Fandom => SuggestionKind::Fandom,
                TagKind::Relationship => SuggestionKind::Relationship,
                TagKind::Character => SuggestionKind::Character,
                TagKind::Warning => SuggestionKind::Warning,
                TagKind::Category | TagKind::Freeform => SuggestionKind::Tag,
            };
            add(kind, &t.name, None);
        }
        if let Some(f) = &e.facts {
            add(SuggestionKind::Language, &f.language, None);
        }
    }
    out
}

fn new_index(schema: Schema) -> Index {
    let index = Index::create_in_ram(schema);
    index.tokenizers().register(FOLD, fold_analyzer());
    index
}

fn fill(index: &Index, docs: Vec<TantivyDocument>) -> Result<IndexReader, SearchError> {
    let mut writer = index
        .writer_with_num_threads(1, WRITER_BUDGET)
        .map_err(|e| error(&e))?;
    for doc in docs {
        writer.add_document(doc).map_err(|e| error(&e))?;
    }
    writer.commit().map_err(|e| error(&e))?;
    writer.wait_merging_threads().map_err(|e| error(&e))?;
    index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(|e| error(&e))
}

// ------------------------------------------------------------------------------- the index

/// Builds a [`Library`] index from the shelf, on a blocking thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct TantivyIndexer;

#[async_trait]
impl SearchIndexer for TantivyIndexer {
    async fn build(&self, books: &[LibraryEntry]) -> Result<Arc<dyn SearchIndex>, SearchError> {
        let books = books.to_vec();
        let library = tokio::task::spawn_blocking(move || Library::build(&books))
            .await
            .map_err(|e| error(&e))??;
        Ok(Arc::new(library))
    }
}

/// The two indexes of one shelf.
pub struct Library {
    books: Searcher,
    book_fields: BookFields,
    names: Searcher,
    name_fields: NameFields,
    /// The names by document order, as built.
    suggestions: Vec<Suggestion>,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library")
            .field("books", &self.books.num_docs())
            .field("names", &self.suggestions.len())
            .finish_non_exhaustive()
    }
}

impl Library {
    /// Index a shelf (synchronously).
    pub fn build(shelf: &[LibraryEntry]) -> Result<Self, SearchError> {
        let (schema, book_fields) = BookFields::schema();
        let books = new_index(schema);
        let docs = shelf
            .iter()
            .map(|e| book_fields.document(e))
            .collect::<Result<Vec<_>, _>>()?;
        let books = fill(&books, docs)?.searcher();

        let (schema, name_fields) = NameFields::schema();
        let names_index = new_index(schema);
        let suggestions: Vec<Suggestion> = names(shelf).into_values().collect();
        let docs = suggestions
            .iter()
            .zip(0_u64..)
            .map(|(s, n)| {
                let mut doc = TantivyDocument::default();
                doc.add_text(name_fields.kind, kind_code(s.kind));
                doc.add_text(name_fields.label, &s.label);
                doc.add_u64(name_fields.work, n);
                doc
            })
            .collect();
        let names = fill(&names_index, docs)?.searcher();
        Ok(Self {
            books,
            book_fields,
            names,
            name_fields,
            suggestions,
        })
    }

    /// Every document `query` matches, best first, as `read` turns them into values.
    fn all<T>(
        searcher: &Searcher,
        query: &dyn Query,
        field: &str,
        read: impl Fn(u64, Score) -> Option<T>,
    ) -> Result<Vec<T>, SearchError> {
        let limit = usize::try_from(searcher.num_docs())
            .unwrap_or(usize::MAX)
            .max(1);
        Self::top(searcher, query, field, limit, read)
    }

    fn top<T>(
        searcher: &Searcher,
        query: &dyn Query,
        field: &str,
        limit: usize,
        read: impl Fn(u64, Score) -> Option<T>,
    ) -> Result<Vec<T>, SearchError> {
        let found: Vec<(Score, DocAddress)> = searcher
            .search(query, &TopDocs::with_limit(limit.max(1)).order_by_score())
            .map_err(|e| error(&e))?;
        let mut columns = HashMap::new();
        let mut out = Vec::with_capacity(found.len());
        for (score, address) in found {
            let column = match columns.entry(address.segment_ord) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => v.insert(
                    searcher
                        .segment_reader(address.segment_ord)
                        .fast_fields()
                        .u64(field)
                        .map_err(|e| error(&e))?,
                ),
            };
            if let Some(value) = column.first(address.doc_id)
                && let Some(item) = read(value, score)
            {
                out.push(item);
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------- query building

    /// One word in some fields: exactly (BM25), as a prefix while typed, or with typos.
    fn word(fields: &[(Field, Score)], word: &str, prefix: bool) -> Box<dyn Query> {
        let mut alternatives: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for &(field, weight) in fields {
            let term = Term::from_field_text(field, word);
            alternatives.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs)),
                    weight * EXACT,
                )),
            ));
            if prefix {
                alternatives.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(FuzzyTermQuery::new_prefix(term.clone(), 0, true)),
                        weight * PREFIX,
                    )),
                ));
            }
            let distance = typos(word);
            if distance > 0 {
                let fuzzy = if prefix {
                    FuzzyTermQuery::new_prefix(term, distance.min(1), true)
                } else {
                    FuzzyTermQuery::new(term, distance, true)
                };
                alternatives.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(fuzzy), weight * TYPO)),
                ));
            }
        }
        Box::new(BooleanQuery::new(alternatives))
    }

    /// A phrase in some fields; while typed, its last word is a prefix.
    fn phrase(fields: &[(Field, Score)], words: &[String], prefix: bool) -> Box<dyn Query> {
        let alternatives = fields
            .iter()
            .map(|&(field, weight)| {
                let terms: Vec<Term> = words
                    .iter()
                    .map(|w| Term::from_field_text(field, w))
                    .collect();
                let query: Box<dyn Query> = if prefix {
                    Box::new(PhrasePrefixQuery::new(terms))
                } else {
                    Box::new(PhraseQuery::new(terms))
                };
                (
                    Occur::Should,
                    Box::new(BoostQuery::new(query, weight * EXACT)) as Box<dyn Query>,
                )
            })
            .collect();
        Box::new(BooleanQuery::new(alternatives))
    }

    /// The text fields a token searches, with their weights.
    fn text_fields(&self, field: TextField) -> Result<Vec<(Field, Score)>, SearchError> {
        if field == TextField::Any {
            WEIGHTS
                .iter()
                .map(|(f, w)| Ok((self.book_fields.pair(*f)?.text, *w)))
                .collect()
        } else {
            Ok(vec![(self.book_fields.pair(field)?.text, 1.0)])
        }
    }

    /// What one token asks for; `None` when it can match nothing (only punctuation, say).
    fn token(&self, token: &Token, prefix: bool) -> Result<Option<Box<dyn Query>>, SearchError> {
        let term_query = |field: Field, value: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, value),
                IndexRecordOption::Basic,
            ))
        };
        Ok(Some(match &token.clause {
            Clause::Words { field, text } => {
                let fields = self.text_fields(*field)?;
                let ws = words(text);
                if ws.is_empty() {
                    return Ok(None);
                }
                let last = ws.len() - 1;
                let all = ws
                    .iter()
                    .enumerate()
                    .map(|(i, w)| (Occur::Must, Self::word(&fields, w, prefix && i == last)))
                    .collect();
                Box::new(BooleanQuery::new(all))
            }
            Clause::Phrase { field, text } => {
                let fields = self.text_fields(*field)?;
                let ws = words(text);
                match ws.as_slice() {
                    [] => return Ok(None),
                    [one] => Self::word(&fields, one, prefix),
                    _ => Self::phrase(&fields, &ws, prefix),
                }
            }
            Clause::Exact { field, value } => {
                if let Some(key) = self.book_fields.pair(*field)?.key {
                    term_query(key, value)
                } else {
                    // A field without values (title, summary): its exact words, as a phrase.
                    let fields = self.text_fields(*field)?;
                    Self::phrase(&fields, &words(value), false)
                }
            }
            Clause::Rating(ratings) => Box::new(BooleanQuery::union(
                ratings
                    .iter()
                    .map(|r| term_query(self.book_fields.rating, r.code()))
                    .collect(),
            )),
            Clause::WordCount(range) => {
                let bound = |b: Option<u64>| {
                    b.map_or(Bound::Unbounded, |n| {
                        Bound::Included(Term::from_field_u64(self.book_fields.words, n))
                    })
                };
                Box::new(RangeQuery::new(bound(range.min), bound(range.max)))
            }
            Clause::Is(flag) => term_query(self.book_fields.flags, flag.code()),
        }))
    }

    fn query(&self, query: &SearchQuery) -> Result<Box<dyn Query>, SearchError> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let last = query.tokens.len().saturating_sub(1);
        for (i, token) in query.tokens.iter().enumerate() {
            let prefix = query.typing && i == last;
            match self.token(token, prefix)? {
                Some(q) if token.negated => clauses.push((Occur::MustNot, q)),
                Some(q) => clauses.push((Occur::Must, q)),
                // Nothing to match, so nothing matches it: a `-` of it excludes nothing.
                None if token.negated => {}
                None => return Ok(Box::new(BooleanQuery::new(Vec::new()))),
            }
        }
        if !clauses.iter().any(|(o, _)| *o == Occur::Must) {
            clauses.push((Occur::Must, Box::new(AllQuery)));
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }
}

impl SearchIndex for Library {
    fn find(&self, query: &SearchQuery) -> Result<Vec<Hit>, SearchError> {
        let q = self.query(query)?;
        // Without text to rank by, every match scores the same.
        let ranked = query.has_text();
        Self::all(&self.books, q.as_ref(), "work", |work, score| {
            WorkId::new(work).map(|work| Hit {
                work,
                score: if ranked { score } else { 0.0 },
            })
        })
    }

    fn suggest(
        &self,
        field: TextField,
        partial: &str,
        limit: usize,
    ) -> Result<Vec<Suggestion>, SearchError> {
        let ws = words(partial);
        let Some(last) = ws.len().checked_sub(1) else {
            return Ok(Vec::new());
        };
        let label = [(self.name_fields.label, 1.0)];
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = ws
            .iter()
            .enumerate()
            .map(|(i, w)| (Occur::Must, Self::word(&label, w, i == last)))
            .collect();
        clauses.push((
            Occur::Must,
            Box::new(BooleanQuery::union(
                kinds_for(field)
                    .iter()
                    .map(|k| {
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.name_fields.kind, kind_code(*k)),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>
                    })
                    .collect(),
            )),
        ));
        let mut found = Self::top(
            &self.names,
            &BooleanQuery::new(clauses),
            "doc",
            SUGGEST_POOL,
            |n, score| {
                usize::try_from(n)
                    .ok()
                    .and_then(|n| self.suggestions.get(n))
                    .map(|s| (s, score))
            },
        )?;
        // Names that start the way the typing does first, then the most books, then the
        // best match, then by name.
        let typed = ws.join(" ");
        let starts = |s: &Suggestion| words(&s.label).join(" ").starts_with(&typed);
        found.sort_by(|(a, sa), (b, sb)| {
            starts(b)
                .cmp(&starts(a))
                .then(b.books.cmp(&a.books))
                .then(sb.total_cmp(sa))
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(found
            .into_iter()
            .take(limit)
            .map(|(s, _)| s.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_lower_cased_and_folded() {
        assert_eq!(
            words("Pokémon: The Ñandú-Saga"),
            ["pokemon", "the", "nandu", "saga"]
        );
        assert_eq!(words("Ada Marsh/Ben Ward"), ["ada", "marsh", "ben", "ward"]);
        assert_eq!(words(&"x".repeat(LONGEST_WORD + 1)), Vec::<String>::new());
    }

    #[test]
    fn short_words_allow_no_typos() {
        assert_eq!(
            (
                typos("the"),
                typos("tide"),
                typos("keeper"),
                typos("lighthouse")
            ),
            (0, 1, 1, 2)
        );
    }

    #[test]
    fn summaries_lose_their_markup() {
        assert_eq!(
            html_text("<p>Ben &amp; Ada&#39;s <em>war</em></p>")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["Ben", "&", "Ada's", "war"]
        );
    }
}
