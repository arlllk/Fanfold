//! The library's search language: what someone types into the search box, as data.
//!
//! A query is a list of tokens separated by spaces. Each one either searches text or filters:
//!
//! * `lighthouse`: a word, matched anywhere (title, author, series, tags, summary), allowing a
//!   typo or two; the word still being typed (the last one, with no space after it) also
//!   matches as a prefix. `"slow burn"` is a phrase.
//! * `author:quill`, `series:harbor`, `fandom:`, `ship:`, `char:`, `tag:`, `warn:`, `lang:`,
//!   `title:`, `summary:`: the same, in one field. A quoted value (`author:"quillandink"`) is
//!   that exact value instead (any case), which is what picking a suggestion inserts.
//! * `rating:e,m` (`g`, `t`, `m`, `e`, `nr`, or AO3's names); `words:>100k`, `words:10k..50k`
//!   (a bare number means "at least"); `is:complete`, `is:wip`, `is:restricted`.
//! * A leading `-` excludes: `-warn:"major character death"`, `-is:complete`, `-crossover`.
//!
//! Anything that is not a known `key:` is plain text: "Re:Zero" is a fandom, not a filter.
//! Parsing never fails; [`SearchQuery::render`] writes the canonical form back, and parsing that
//! gives the same query (the tests check it for any input), so chips and filter links can
//! edit a query as text.

/// Longest query read, in characters; the rest is ignored. Generous: the canonical form of
/// a query can be longer than what was typed (`warn:mcd` is `warn:"major character death"`).
pub const MAX_QUERY_CHARS: usize = 4_096;
/// Longest value of one token, in characters; the rest is ignored.
pub const MAX_VALUE_CHARS: usize = 100;
/// Most tokens a query keeps; the rest are ignored.
pub const MAX_TOKENS: usize = 24;

/// Where a text token searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TextField {
    /// Every text field, weighted (title first, summary last).
    Any,
    Title,
    Author,
    Series,
    Fandom,
    Relationship,
    Character,
    /// Any tag: fandoms, relationships, characters, warnings, categories and freeform tags.
    Tag,
    Warning,
    Language,
    Summary,
}

impl TextField {
    /// Every field with a key, in the order help text lists them.
    pub const KEYED: [Self; 10] = [
        Self::Title,
        Self::Author,
        Self::Series,
        Self::Fandom,
        Self::Relationship,
        Self::Character,
        Self::Tag,
        Self::Warning,
        Self::Language,
        Self::Summary,
    ];

    /// The key the canonical form uses (`None` for [`TextField::Any`]).
    #[must_use]
    pub const fn key(self) -> Option<&'static str> {
        Some(match self {
            Self::Any => return None,
            Self::Title => "title",
            Self::Author => "author",
            Self::Series => "series",
            Self::Fandom => "fandom",
            Self::Relationship => "ship",
            Self::Character => "char",
            Self::Tag => "tag",
            Self::Warning => "warn",
            Self::Language => "lang",
            Self::Summary => "summary",
        })
    }

    /// Whether a quoted value means one exact value (a name), rather than a phrase.
    #[must_use]
    pub const fn has_values(self) -> bool {
        !matches!(self, Self::Any | Self::Title | Self::Summary)
    }

    fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "title" | "t" => Self::Title,
            "author" | "by" | "a" => Self::Author,
            "series" | "s" => Self::Series,
            "fandom" | "f" => Self::Fandom,
            "ship" | "rel" | "relationship" | "pairing" => Self::Relationship,
            "char" | "character" | "c" => Self::Character,
            "tag" => Self::Tag,
            "warn" | "warning" | "w" => Self::Warning,
            "lang" | "language" | "l" => Self::Language,
            "summary" => Self::Summary,
            _ => return None,
        })
    }
}

/// AO3's ratings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rating {
    General,
    Teen,
    Mature,
    Explicit,
    NotRated,
}

impl Rating {
    pub const ALL: [Self; 5] = [
        Self::General,
        Self::Teen,
        Self::Mature,
        Self::Explicit,
        Self::NotRated,
    ];

    /// The short form used in queries and URLs.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::General => "g",
            Self::Teen => "t",
            Self::Mature => "m",
            Self::Explicit => "e",
            Self::NotRated => "nr",
        }
    }

    /// AO3's name for it.
    #[must_use]
    pub const fn ao3_name(self) -> &'static str {
        match self {
            Self::General => "General Audiences",
            Self::Teen => "Teen And Up Audiences",
            Self::Mature => "Mature",
            Self::Explicit => "Explicit",
            Self::NotRated => "Not Rated",
        }
    }

    /// A work's rating as AO3 wrote it; anything unknown (or blank) counts as not rated.
    #[must_use]
    pub fn from_ao3(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|r| r.ao3_name().eq_ignore_ascii_case(name.trim()))
            .unwrap_or(Self::NotRated)
    }

    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().to_ascii_lowercase();
        match raw.as_str() {
            "g" | "gen" | "general" => Some(Self::General),
            "t" | "teen" => Some(Self::Teen),
            "m" | "mature" => Some(Self::Mature),
            "e" | "explicit" => Some(Self::Explicit),
            "nr" | "notrated" | "not-rated" | "unrated" | "none" => Some(Self::NotRated),
            _ => Self::ALL
                .into_iter()
                .find(|r| r.ao3_name().eq_ignore_ascii_case(&raw)),
        }
    }
}

/// AO3's archive warnings, with short names for typing.
pub const WARNINGS: [(&str, &[&str]); 6] = [
    ("No Archive Warnings Apply", &["none", "nawa"]),
    (
        "Creator Chose Not To Use Archive Warnings",
        &["cntw", "choose", "chose"],
    ),
    ("Graphic Depictions Of Violence", &["violence", "gore"]),
    ("Major Character Death", &["mcd", "death"]),
    ("Rape/Non-Con", &["noncon", "non-con", "rape"]),
    ("Underage Sex", &["underage"]),
];

/// The AO3 warning a short name stands for.
#[must_use]
pub fn warning_alias(raw: &str) -> Option<&'static str> {
    let raw = raw.trim().to_ascii_lowercase();
    WARNINGS
        .iter()
        .find(|(_, aliases)| aliases.contains(&raw.as_str()))
        .map(|(name, _)| *name)
}

/// A yes-or-no property of a work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Flag {
    /// Every chapter the author planned is posted.
    Complete,
    /// Known and not complete (including a total of "?").
    InProgress,
    /// Visible to logged-in AO3 users only.
    Restricted,
}

impl Flag {
    pub const ALL: [Self; 3] = [Self::Complete, Self::InProgress, Self::Restricted];

    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::InProgress => "wip",
            Self::Restricted => "restricted",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "complete" | "completed" | "done" | "finished" => Some(Self::Complete),
            "wip" | "inprogress" | "in-progress" | "in_progress" | "ongoing" | "incomplete" => {
                Some(Self::InProgress)
            }
            "restricted" | "locked" => Some(Self::Restricted),
            _ => None,
        }
    }
}

/// An inclusive range of word counts; `None` is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WordRange {
    pub min: Option<u64>,
    pub max: Option<u64>,
}

impl WordRange {
    #[must_use]
    pub fn contains(self, words: u64) -> bool {
        self.min.is_none_or(|m| words >= m) && self.max.is_none_or(|m| words <= m)
    }

    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let range = if let Some((lo, hi)) = raw.split_once("..") {
            Self {
                min: if lo.is_empty() {
                    None
                } else {
                    Some(count(lo)?)
                },
                max: if hi.is_empty() {
                    None
                } else {
                    Some(count(hi)?)
                },
            }
        } else if let Some(n) = raw.strip_prefix(">=") {
            Self {
                min: Some(count(n)?),
                max: None,
            }
        } else if let Some(n) = raw.strip_prefix("<=") {
            Self {
                min: None,
                max: Some(count(n)?),
            }
        } else if let Some(n) = raw.strip_prefix('>') {
            Self {
                min: Some(count(n)?.checked_add(1)?),
                max: None,
            }
        } else if let Some(n) = raw.strip_prefix('<') {
            Self {
                min: None,
                max: Some(count(n)?.checked_sub(1)?),
            }
        } else {
            Self {
                min: Some(count(raw)?),
                max: None,
            }
        };
        (range.min.is_some() || range.max.is_some()).then_some(range)
    }

    fn render(self) -> String {
        match (self.min, self.max) {
            (Some(lo), None) => format!(">={}", short_count(lo)),
            (None, Some(hi)) => format!("<={}", short_count(hi)),
            (Some(lo), Some(hi)) => format!("{}..{}", short_count(lo), short_count(hi)),
            (None, None) => String::new(),
        }
    }
}

/// `120k`, `1.5m`, `2,000` or `2000`.
fn count(raw: &str) -> Option<u64> {
    let raw = raw.trim().to_ascii_lowercase().replace(['_', ','], "");
    let (digits, scale) = match raw.strip_suffix('k') {
        Some(d) => (d, 1_000_u64),
        None => match raw.strip_suffix('m') {
            Some(d) => (d, 1_000_000),
            None => (raw.as_str(), 1),
        },
    };
    if digits.is_empty() || digits.len() > 12 {
        return None;
    }
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && frac.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || frac.len() > 3
        || (scale == 1 && !frac.is_empty())
    {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut value = whole.checked_mul(scale)?;
    if !frac.is_empty() {
        let frac_value: u64 = frac.parse().ok()?;
        let denominator = 10_u64.pow(u32::try_from(frac.len()).ok()?);
        value = value.checked_add(frac_value.checked_mul(scale)? / denominator)?;
    }
    Some(value)
}

/// The shortest form of a count that [`count`] reads back exactly.
fn short_count(n: u64) -> String {
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}m", n / 1_000_000)
    } else if n >= 1_000 && n.is_multiple_of(1_000) {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// What one token asks for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Clause {
    /// Words in a field, matched with typos allowed (and the word being typed as a prefix).
    Words {
        field: TextField,
        text: String,
    },
    /// A phrase in a field: these words, in this order.
    Phrase {
        field: TextField,
        text: String,
    },
    /// One exact value of a field (any case): an author, series, tag, warning or language.
    Exact {
        field: TextField,
        value: String,
    },
    Rating(Vec<Rating>),
    WordCount(WordRange),
    Is(Flag),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Token {
    pub clause: Clause,
    /// `-`: the books that do *not* match.
    pub negated: bool,
}

impl Token {
    #[must_use]
    pub const fn new(clause: Clause) -> Self {
        Self {
            clause,
            negated: false,
        }
    }

    /// Whether the token only filters (no text to rank by).
    #[must_use]
    pub const fn is_filter(&self) -> bool {
        self.negated || !matches!(self.clause, Clause::Words { .. } | Clause::Phrase { .. })
    }

    /// The canonical text of this token.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.negated {
            out.push('-');
        }
        let keyed = |out: &mut String, field: TextField| {
            if let Some(key) = field.key() {
                out.push_str(key);
                out.push(':');
            }
        };
        match &self.clause {
            Clause::Words { field, text } => {
                keyed(&mut out, *field);
                out.push_str(text);
            }
            Clause::Phrase { field, text } | Clause::Exact { field, value: text } => {
                keyed(&mut out, *field);
                quote(&mut out, text);
            }
            Clause::Rating(ratings) => {
                out.push_str("rating:");
                let codes: Vec<&str> = ratings.iter().map(|r| r.code()).collect();
                out.push_str(&codes.join(","));
            }
            Clause::WordCount(range) => {
                out.push_str("words:");
                out.push_str(&range.render());
            }
            Clause::Is(flag) => {
                out.push_str("is:");
                out.push_str(flag.code());
            }
        }
        out
    }
}

/// Quotes inside a quoted value cannot be escaped, so they are dropped (no AO3 name needs one).
fn quote(out: &mut String, text: &str) {
    out.push('"');
    out.extend(text.chars().filter(|c| *c != '"'));
    out.push('"');
}

/// A parsed search.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SearchQuery {
    pub tokens: Vec<Token>,
    /// The last token is still being typed: no space after it, and not a closed quote. Its
    /// last word then also matches as a prefix ("lightho" finds "lighthouse").
    pub typing: bool,
}

/// One raw token: its optional `-`, its key, its value, and how it was quoted.
struct Raw<'a> {
    negated: bool,
    key: Option<&'a str>,
    value: &'a str,
    quoted: bool,
    /// The quote was never closed: it runs to the end of the input.
    open: bool,
}

/// Split into raw tokens. Quotes group spaces; a quote left open runs to the end.
fn lex(input: &str) -> Vec<Raw<'_>> {
    let mut tokens = Vec::new();
    let mut rest = input.trim_start();
    while !rest.is_empty() {
        let (negated, body) = match rest.strip_prefix('-') {
            Some(after) if !after.is_empty() && !after.starts_with(char::is_whitespace) => {
                (true, after)
            }
            _ => (false, rest),
        };
        // `key:` directly followed by its value; only our keys count ("Re:Zero" is text).
        let (key, value_start) = match body.find(':') {
            Some(i) => match body.get(..i) {
                Some(k) if is_key(k) => (Some(k), body.get(i + 1..).unwrap_or("")),
                _ => (None, body),
            },
            None => (None, body),
        };
        let (value, quoted, open, after) = if let Some(inner) = value_start.strip_prefix('"') {
            match inner.find('"') {
                Some(end) => (
                    inner.get(..end).unwrap_or(""),
                    true,
                    false,
                    inner.get(end + 1..).unwrap_or(""),
                ),
                None => (inner, true, true, ""),
            }
        } else {
            let end = value_start
                .find(char::is_whitespace)
                .unwrap_or(value_start.len());
            (
                value_start.get(..end).unwrap_or(""),
                false,
                false,
                value_start.get(end..).unwrap_or(""),
            )
        };
        tokens.push(Raw {
            negated,
            key,
            value,
            quoted,
            open,
        });
        rest = after.trim_start();
    }
    tokens
}

fn is_key(key: &str) -> bool {
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let key = key.to_ascii_lowercase();
    TextField::from_key(&key).is_some() || matches!(key.as_str(), "rating" | "r" | "words" | "is")
}

/// Text with no quotes and single spaces, trimmed.
fn clean(text: &str) -> String {
    text.split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`clean`], at most [`MAX_VALUE_CHARS`] long. Cleaning its own output changes nothing, so
/// a rendered query reads back the same.
fn capped(text: &str) -> String {
    clean(
        &clean(text)
            .chars()
            .take(MAX_VALUE_CHARS)
            .collect::<String>(),
    )
}

fn clause_of(raw: &Raw<'_>) -> Option<Clause> {
    let value = capped(raw.value);
    if value.is_empty() {
        return None;
    }
    let Some(key) = raw.key.map(str::to_ascii_lowercase) else {
        let field = TextField::Any;
        // Text that would read back as a filter once its quotes are gone (`R":` is `R:`) stays
        // quoted, so it stays text.
        let looks_keyed = value.split_once(':').is_some_and(|(k, _)| is_key(k));
        return Some(if raw.quoted || looks_keyed {
            Clause::Phrase { field, text: value }
        } else {
            Clause::Words { field, text: value }
        });
    };
    match key.as_str() {
        "rating" | "r" => {
            let mut ratings: Vec<Rating> = value.split(',').filter_map(Rating::parse).collect();
            ratings.sort();
            ratings.dedup();
            (!ratings.is_empty()).then_some(Clause::Rating(ratings))
        }
        "words" => WordRange::parse(&value).map(Clause::WordCount),
        "is" => Flag::parse(&value).map(Clause::Is),
        _ => {
            let field = TextField::from_key(&key)?;
            // A quote still open is still being typed: a phrase (its last word a prefix)
            // until it is closed and becomes the exact value. (An exclusion is never "being
            // typed", so it is the exact value at once.)
            if raw.quoted && (!raw.open || raw.negated) && field.has_values() {
                Some(Clause::Exact {
                    field,
                    value: capped(&value.to_lowercase()),
                })
            } else if raw.quoted {
                Some(Clause::Phrase { field, text: value })
            } else if let Some(name) = warning_alias(&value).filter(|_| field == TextField::Warning)
            {
                Some(Clause::Exact {
                    field,
                    value: name.to_lowercase(),
                })
            } else {
                Some(Clause::Words { field, text: value })
            }
        }
    }
}

impl SearchQuery {
    /// Read a query. Never fails: what cannot be understood is ignored or taken as text.
    #[must_use]
    pub fn parse(input: &str) -> Self {
        let input: String = input.chars().take(MAX_QUERY_CHARS).collect();
        let raws = lex(&input);
        let mut tokens: Vec<Token> = Vec::new();
        for raw in &raws {
            let Some(clause) = clause_of(raw) else {
                continue;
            };
            let token = Token {
                clause,
                negated: raw.negated,
            };
            if tokens.len() < MAX_TOKENS && !tokens.contains(&token) {
                tokens.push(token);
            }
        }
        // Still typing: the input ends inside the last token (no space after it, or inside an
        // open quote), and that token searches text.
        let typing = raws.last().is_some_and(|last| {
            (last.open || !last.quoted && !input.ends_with(char::is_whitespace))
                && clause_of(last).is_some()
        }) && tokens.last().is_some_and(|t| {
            !t.negated && matches!(t.clause, Clause::Words { .. } | Clause::Phrase { .. })
        });
        Self { tokens, typing }
    }

    /// The canonical text. Parsing it gives this query back, `typing` included: a phrase
    /// still being typed keeps its quote open, and a finished last word gets a space.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, token) in self.tokens.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(&token.render());
        }
        let last_is_phrase = self
            .tokens
            .last()
            .is_some_and(|t| matches!(t.clause, Clause::Phrase { .. }));
        if self.typing && last_is_phrase {
            out.pop(); // the closing quote
        } else if !self.typing && self.ends_like_typing() {
            out.push(' ');
        }
        out
    }

    /// Whether the rendered text alone would read as "still typing", so `render` must add the
    /// space that says it is not.
    fn ends_like_typing(&self) -> bool {
        self.tokens
            .last()
            .is_some_and(|t| !t.negated && matches!(t.clause, Clause::Words { .. }))
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Whether anything in it ranks by text (so "best match" is the natural order).
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.tokens.iter().any(|t| !t.is_filter())
    }

    /// This query with `token` added, or removed if it is already there.
    #[must_use]
    pub fn toggled(&self, token: &Token) -> Self {
        let mut tokens = self.tokens.clone();
        if let Some(i) = tokens.iter().position(|t| t == token) {
            tokens.remove(i);
        } else {
            tokens.push(token.clone());
        }
        Self {
            tokens,
            typing: false,
        }
    }

    /// This query without the tokens `drop` matches.
    #[must_use]
    pub fn without(&self, drop: impl Fn(&Token) -> bool) -> Self {
        Self {
            tokens: self.tokens.iter().filter(|t| !drop(t)).cloned().collect(),
            typing: false,
        }
    }

    /// This query with `token` instead of whatever `replaces` matches.
    #[must_use]
    pub fn with(&self, token: Token, replaces: impl Fn(&Token) -> bool) -> Self {
        let mut next = self.without(replaces);
        next.tokens.push(token);
        next
    }

    /// What the user is in the middle of typing, for suggestions: the field it is in (if it
    /// has a key) and the text so far. `None` once a space ends the last token.
    #[must_use]
    pub fn partial(&self) -> Option<(TextField, &str)> {
        if !self.typing {
            return None;
        }
        match &self.tokens.last()?.clause {
            Clause::Words { field, text } | Clause::Phrase { field, text } => {
                Some((*field, text.as_str()))
            }
            _ => None,
        }
    }

    /// This query with the token being typed replaced by `token` (a picked suggestion).
    #[must_use]
    pub fn completed_with(&self, token: Token) -> Self {
        let mut tokens = self.tokens.clone();
        if self.typing {
            tokens.pop();
        }
        if !tokens.contains(&token) {
            tokens.push(token);
        }
        Self {
            tokens,
            typing: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(field: TextField, text: &str) -> Clause {
        Clause::Words {
            field,
            text: text.into(),
        }
    }

    fn one(input: &str) -> Token {
        let q = SearchQuery::parse(input);
        assert_eq!(q.tokens.len(), 1, "{input:?} -> {q:?}");
        q.tokens.into_iter().next().unwrap()
    }

    #[test]
    fn plain_words_search_everywhere() {
        let q = SearchQuery::parse("lighthouse keeper");
        assert_eq!(
            q.tokens,
            vec![
                Token::new(words(TextField::Any, "lighthouse")),
                Token::new(words(TextField::Any, "keeper"))
            ]
        );
        assert!(q.typing, "no space after the last word");
        assert!(!SearchQuery::parse("lighthouse ").typing);
        assert_eq!(SearchQuery::parse("   "), SearchQuery::default());
    }

    #[test]
    fn quotes_make_phrases_and_exact_values() {
        assert_eq!(
            one("\"slow burn\"").clause,
            Clause::Phrase {
                field: TextField::Any,
                text: "slow burn".into()
            }
        );
        assert_eq!(
            one("author:\"QuillAndInk\"").clause,
            Clause::Exact {
                field: TextField::Author,
                value: "quillandink".into()
            }
        );
        assert_eq!(
            one("title:\"The Keeper\"").clause,
            Clause::Phrase {
                field: TextField::Title,
                text: "The Keeper".into()
            },
            "titles have no list of values: quoted is a phrase"
        );
        let open = SearchQuery::parse("series:\"the harb");
        assert!(open.typing, "a quote still open is still being typed");
        assert_eq!(open.partial(), Some((TextField::Series, "the harb")));
        assert_eq!(open.render(), "series:\"the harb", "and stays open");
        assert!(
            SearchQuery::parse("\"slow ").typing,
            "inside an open quote, even after a space"
        );
    }

    #[test]
    fn keys_and_their_short_forms() {
        for (input, field) in [
            ("author:quill", TextField::Author),
            ("by:quill", TextField::Author),
            ("series:harbor", TextField::Series),
            ("fandom:x", TextField::Fandom),
            ("ship:x", TextField::Relationship),
            ("rel:x", TextField::Relationship),
            ("char:x", TextField::Character),
            ("tag:x", TextField::Tag),
            ("warn:x", TextField::Warning),
            ("lang:english", TextField::Language),
            ("title:x", TextField::Title),
            ("summary:x", TextField::Summary),
            ("AUTHOR:x", TextField::Author),
        ] {
            match one(input).clause {
                Clause::Words { field: f, .. } => assert_eq!(f, field, "{input}"),
                other => panic!("{input}: {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_keys_are_text() {
        assert_eq!(one("Re:Zero").clause, words(TextField::Any, "Re:Zero"));
        assert_eq!(one("http://x").clause, words(TextField::Any, "http://x"));
        assert_eq!(one("12:30").clause, words(TextField::Any, "12:30"));
        assert_eq!(one(":x").clause, words(TextField::Any, ":x"));
    }

    #[test]
    fn ratings_words_and_flags() {
        assert_eq!(
            one("rating:e,m,E").clause,
            Clause::Rating(vec![Rating::Mature, Rating::Explicit])
        );
        assert_eq!(
            one("rating:Explicit").clause,
            Clause::Rating(vec![Rating::Explicit])
        );
        assert!(
            SearchQuery::parse("rating:zzz").is_empty(),
            "nothing valid: ignored"
        );
        for (input, min, max) in [
            ("words:>100k", Some(100_001), None),
            ("words:>=100k", Some(100_000), None),
            ("words:<5000", None, Some(4_999)),
            ("words:<=5k", None, Some(5_000)),
            ("words:10k..50k", Some(10_000), Some(50_000)),
            ("words:..50k", None, Some(50_000)),
            ("words:1.5m", Some(1_500_000), None),
            ("words:2,000", Some(2_000), None),
        ] {
            assert_eq!(
                one(input).clause,
                Clause::WordCount(WordRange { min, max }),
                "{input}"
            );
        }
        for bad in [
            "words:lots",
            "words:<0",
            "words:..",
            "words:1.5",
            "words:k",
            "words:99999999999999m",
        ] {
            assert!(SearchQuery::parse(bad).is_empty(), "{bad}");
        }
        assert_eq!(one("is:complete").clause, Clause::Is(Flag::Complete));
        assert_eq!(one("is:wip").clause, Clause::Is(Flag::InProgress));
        assert_eq!(one("is:restricted").clause, Clause::Is(Flag::Restricted));
    }

    #[test]
    fn a_minus_excludes() {
        let t = one("-warn:mcd");
        assert!(t.negated);
        assert_eq!(
            t.clause,
            Clause::Exact {
                field: TextField::Warning,
                value: "major character death".into()
            },
            "a short name stands for AO3's warning"
        );
        assert!(one("-is:complete").negated);
        assert!(one("-crossover").negated);
        assert_eq!(
            one("well-known").clause,
            words(TextField::Any, "well-known"),
            "only a leading minus"
        );
        assert_eq!(
            SearchQuery::parse("- x").tokens,
            vec![
                Token::new(words(TextField::Any, "-")),
                Token::new(words(TextField::Any, "x"))
            ],
            "a lone minus is text"
        );
        assert_eq!(one("Re:\"Zero").clause, words(TextField::Any, "Re:Zero"));
    }

    #[test]
    fn a_key_without_a_value_is_ignored_but_still_being_typed() {
        let q = SearchQuery::parse("author:");
        assert!(q.is_empty());
        assert_eq!(q.partial(), None);
    }

    #[test]
    fn the_word_being_typed_is_the_partial() {
        let q = SearchQuery::parse("is:complete author:qui");
        assert_eq!(q.partial(), Some((TextField::Author, "qui")));
        assert_eq!(SearchQuery::parse("author:qui ").partial(), None);
        assert_eq!(
            SearchQuery::parse("author:\"quillandink\"").partial(),
            None,
            "a closed quote is finished"
        );
        assert_eq!(
            SearchQuery::parse("lightho").partial(),
            Some((TextField::Any, "lightho"))
        );
    }

    #[test]
    fn a_picked_suggestion_replaces_the_partial() {
        let q = SearchQuery::parse("is:complete quil");
        let picked = Token::new(Clause::Exact {
            field: TextField::Author,
            value: "quillandink".into(),
        });
        assert_eq!(
            q.completed_with(picked).render(),
            "is:complete author:\"quillandink\""
        );
    }

    #[test]
    fn toggling_adds_and_removes() {
        let q = SearchQuery::parse("keeper is:complete");
        let complete = Token::new(Clause::Is(Flag::Complete));
        assert_eq!(q.toggled(&complete).render(), "keeper ");
        assert_eq!(
            SearchQuery::parse("keeper ").toggled(&complete).render(),
            "keeper is:complete"
        );
    }

    #[test]
    fn duplicates_are_dropped_and_length_is_capped() {
        assert_eq!(SearchQuery::parse("a a a").tokens.len(), 1);
        let many = (0..100)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(SearchQuery::parse(&many).tokens.len(), MAX_TOKENS);
        let long = "x".repeat(MAX_VALUE_CHARS * 3);
        match &one(&long).clause {
            Clause::Words { text, .. } => assert_eq!(text.chars().count(), MAX_VALUE_CHARS),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_canonical_form_reads_back() {
        for input in [
            "a:quill -by:\"Ada\" rating:explicit,g words:>100k -is:complete \"slow burn\" lightho",
            "series:\"The Harbor Saga\" ",
            "warn:violence lang:english",
            "words:<=1m",
        ] {
            let q = SearchQuery::parse(input);
            assert_eq!(
                SearchQuery::parse(&q.render()),
                q,
                "{input} -> {}",
                q.render()
            );
        }
        assert_eq!(
            SearchQuery::parse("a:quill rating:explicit,g").render(),
            "author:quill rating:g,e"
        );
    }

    #[test]
    fn ao3_ratings_and_counts() {
        assert_eq!(Rating::from_ao3("Teen And Up Audiences"), Rating::Teen);
        assert_eq!(Rating::from_ao3(""), Rating::NotRated);
        assert_eq!(count("12k"), Some(12_000));
        assert_eq!(count("0.5k"), Some(500));
        assert_eq!(count(".5k"), Some(500));
        assert_eq!(short_count(1_500_000), "1500k");
        assert_eq!(short_count(2_000_000), "2m");
        assert_eq!(short_count(1_234), "1234");
        assert!(
            WordRange {
                min: Some(5),
                max: Some(9)
            }
            .contains(5)
        );
        assert!(
            !WordRange {
                min: Some(5),
                max: Some(9)
            }
            .contains(10)
        );
    }

    mod properties {
        use proptest::prelude::*;

        use super::super::*;

        proptest! {
            #[test]
            fn parsing_never_panics_and_the_canonical_form_is_a_fixed_point(input in "\\PC{0,80}") {
                let q = SearchQuery::parse(&input);
                let again = SearchQuery::parse(&q.render());
                prop_assert_eq!(&again, &q, "rendered {:?}", q.render());
            }

            #[test]
            fn queries_built_from_the_alphabet_read_back(
                input in "(-?((author|series|tag|warn|rating|words|is|title|re):)?(\"[a-z ]{0,6}\"?|[a-z0-9.,<>=]{0,6}) ?){0,6}"
            ) {
                let q = SearchQuery::parse(&input);
                prop_assert_eq!(SearchQuery::parse(&q.render()), q);
            }
        }
    }
}
