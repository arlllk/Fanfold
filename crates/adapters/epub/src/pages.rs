//! The pages Fanfold adds: a cover page and an "About this work" page at the back.
//!
//! Plain XHTML with labelled paragraphs rather than tables or definition lists: e-readers
//! override styles freely, and plain paragraphs survive any of them.

use std::fmt::Write as _;

use fanfold_app::ports::BookExtras;
use fanfold_domain::metadata::{Chapter, TagKind, WorkFacts, WorkVersionChanges};

use crate::{
    book::{author_name, author_url},
    text::{chapter_count, complete, day, esc, paragraphs, tag_label, thousands},
};

const HEAD: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html>\n";

/// How many versions the history lists, newest first.
const HISTORY_LEN: usize = 40;
/// How many items one history line names before summarising the rest.
const LIST_LEN: usize = 6;

fn page(title: &str, lang: &str, style: &str, body: &str) -> String {
    format!(
        "{HEAD}<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"{lang}\" lang=\"{lang}\">\n\
         <head>\n<title>{}</title>\n<link href=\"stylesheet.css\" type=\"text/css\" rel=\"stylesheet\"/>\n<style type=\"text/css\">{style}</style>\n</head>\n\
         <body>\n{body}</body>\n</html>\n",
        esc(title)
    )
}

/// The cover, as the first page of the book.
#[must_use]
pub fn cover_page(title: &str, lang: &str) -> String {
    page(
        title,
        lang,
        "html, body { margin: 0; padding: 0; height: 100%; } \
         div.ff-cover { height: 100%; text-align: center; } \
         div.ff-cover img { max-width: 100%; max-height: 100%; }",
        "<section epub:type=\"cover\"><div class=\"ff-cover\"><img src=\"fanfold_cover.png\" alt=\"Cover\"/></div></section>\n",
    )
}

fn row(out: &mut String, label: &str, value: &str) {
    if !value.is_empty() {
        let _ = writeln!(
            out,
            "<p class=\"ff-row\"><b>{}:</b> {value}</p>",
            esc(label)
        );
    }
}

fn joined(items: impl IntoIterator<Item = String>) -> String {
    items.into_iter().collect::<Vec<_>>().join(", ")
}

/// `A, B, C and 4 more`.
fn some(items: &[String]) -> String {
    let shown: Vec<String> = items.iter().take(LIST_LEN).cloned().collect();
    let rest = items.len().saturating_sub(LIST_LEN);
    if rest == 0 {
        shown.join(", ")
    } else {
        format!("{} and {rest} more", shown.join(", "))
    }
}

fn chapter_name(chapter: &Chapter) -> String {
    let title = chapter.title.trim();
    let number = chapter.number.to_string();
    // FanFicFare's titles usually start with the number already: "3. The Storm".
    if title.is_empty() {
        format!("Chapter {number}")
    } else if title.starts_with(&format!("{number}.")) || title.starts_with(&format!("{number} ")) {
        title.to_owned()
    } else {
        format!("{number}. {title}")
    }
}

/// What changed in `now` compared with `before`, one sentence each (escaped).
#[allow(
    clippy::too_many_lines,
    reason = "one flat list of sentences, one per kind of change, in reading order"
)]
fn changes(before: Option<&WorkVersionChanges>, now: &WorkVersionChanges) -> Vec<String> {
    let f = &now.facts;
    let Some(before) = before else {
        return vec![format!(
            "First downloaded: {} chapters, {} words.",
            chapter_count(f),
            thousands(f.words)
        )];
    };
    let b: &WorkFacts = &before.facts;
    let mut out = Vec::new();
    if b.title != f.title {
        out.push(format!("Title changed from “{}”.", esc(&b.title)));
    }
    if b.chapters_posted != f.chapters_posted || b.chapters_total != f.chapters_total {
        out.push(format!(
            "Chapters {} → {}.",
            chapter_count(b),
            chapter_count(f)
        ));
    }
    if b.words != f.words {
        let delta = if f.words >= b.words {
            format!("+{}", thousands(f.words - b.words))
        } else {
            format!("−{}", thousands(b.words - f.words))
        };
        out.push(format!(
            "Words {} → {} ({delta}).",
            thousands(b.words),
            thousands(f.words)
        ));
    }
    match (complete(b), complete(f)) {
        (false, true) => out.push("Marked complete.".to_owned()),
        (true, false) => out.push("No longer complete.".to_owned()),
        _ => {}
    }
    if b.rating != f.rating {
        out.push(format!("Rating {} → {}.", esc(&b.rating), esc(&f.rating)));
    }
    if b.summary_html != f.summary_html {
        out.push("Summary edited.".to_owned());
    }

    let renamed: Vec<&Chapter> = now
        .chapters
        .added
        .iter()
        .filter(|c| now.chapters.removed.iter().any(|r| r.number == c.number))
        .collect();
    let new: Vec<String> = now
        .chapters
        .added
        .iter()
        .filter(|c| !renamed.iter().any(|r| r.number == c.number))
        .map(|c| esc(&chapter_name(c)))
        .collect();
    if !new.is_empty() {
        out.push(format!("New chapters: {}.", some(&new)));
    }
    if !renamed.is_empty() {
        let names: Vec<String> = renamed.iter().map(|c| esc(&chapter_name(c))).collect();
        out.push(format!("Chapters renamed: {}.", some(&names)));
    }
    let gone: Vec<String> = now
        .chapters
        .removed
        .iter()
        .filter(|c| !now.chapters.added.iter().any(|a| a.number == c.number))
        .map(|c| esc(&chapter_name(c)))
        .collect();
    if !gone.is_empty() {
        out.push(format!("Chapters removed: {}.", some(&gone)));
    }

    for kind in TagKind::ALL {
        let (label, _) = tag_label(kind);
        let label = label.to_lowercase();
        let names = |tags: &[fanfold_domain::metadata::Tag]| -> Vec<String> {
            tags.iter()
                .filter(|t| t.kind == kind)
                .map(|t| esc(&t.name))
                .collect()
        };
        let added = names(&now.tags.added);
        if !added.is_empty() {
            out.push(format!("Added {label}: {}.", some(&added)));
        }
        let removed = names(&now.tags.removed);
        if !removed.is_empty() {
            out.push(format!("Removed {label}: {}.", some(&removed)));
        }
    }

    for part in &now.series.added {
        out.push(match part.part {
            Some(n) => format!("Now part {n} of {}.", esc(&part.name)),
            None => format!("Now in the series {}.", esc(&part.name)),
        });
    }
    for part in &now.series.removed {
        if !now.series.added.iter().any(|p| p.id == part.id) {
            out.push(format!("No longer in the series {}.", esc(&part.name)));
        }
    }
    for author in &now.authors.added {
        out.push(format!("Creator added: {}.", esc(&author_name(author))));
    }
    for author in &now.authors.removed {
        out.push(format!("Creator removed: {}.", esc(&author_name(author))));
    }
    if out.is_empty() {
        out.push("Details on the work's page changed.".to_owned());
    }
    out
}

/// The work's facts as labelled lines.
fn details(body: &mut String, extras: &BookExtras) {
    let metadata = &extras.metadata;
    let facts = &metadata.facts;
    body.push_str("<h3>Details</h3>\n");
    row(body, "Rating", &esc(&facts.rating));
    for kind in [
        TagKind::Warning,
        TagKind::Category,
        TagKind::Fandom,
        TagKind::Relationship,
        TagKind::Character,
        TagKind::Freeform,
    ] {
        let (_, heading) = tag_label(kind);
        let tags = joined(
            metadata
                .tags
                .iter()
                .filter(|t| t.kind == kind)
                .map(|t| esc(&t.name)),
        );
        row(body, heading, &tags);
    }
    row(body, "Language", &esc(&facts.language));
    for part in &metadata.series {
        let name = format!(
            "<a href=\"{}\">{}</a>",
            esc(&part.id.canonical_url()),
            esc(&part.name)
        );
        let value = match part.part {
            Some(n) => format!("Part {n} of {name}"),
            None => name,
        };
        row(body, "Series", &value);
    }
    row(
        body,
        "Status",
        if complete(facts) {
            "Complete"
        } else {
            "In progress"
        },
    );
    row(body, "Chapters", &chapter_count(facts));
    row(body, "Words", &thousands(facts.words));
    if let Some(d) = facts.published_on {
        row(body, "Published", &day(d));
    }
    if let Some(d) = facts.updated_on {
        row(body, "Updated", &day(d));
    }
    if facts.restricted {
        row(body, "Visibility", "Logged-in AO3 users only");
    }
    let stats = metadata.stats;
    let counters: Vec<String> = [
        ("Kudos", stats.kudos),
        ("Hits", stats.hits),
        ("Bookmarks", stats.bookmarks),
        ("Comments", stats.comments),
    ]
    .into_iter()
    .filter_map(|(label, n)| n.map(|n| format!("{label} {}", thousands(n))))
    .collect();
    if !counters.is_empty() {
        let as_of = extras.fetched_at.strftime("%-d %b %Y").to_string();
        row(
            body,
            "Counters",
            &format!("{} (on {as_of})", counters.join(" · ")),
        );
    }
}

/// The "About this work" page.
#[must_use]
pub fn about_page(extras: &BookExtras, lang: &str) -> String {
    let metadata = &extras.metadata;
    let facts = &metadata.facts;
    let mut body = String::from("<section epub:type=\"backmatter\">\n<h1>About this work</h1>\n");

    let _ = writeln!(body, "<h2>{}</h2>", esc(&facts.title));
    let authors = joined(metadata.authors.iter().map(|a| {
        format!(
            "<a href=\"{}\">{}</a>",
            esc(&author_url(a)),
            esc(&author_name(a))
        )
    }));
    if !authors.is_empty() {
        let _ = writeln!(body, "<p>by {authors}</p>");
    } else if !facts.byline.is_empty() {
        let _ = writeln!(body, "<p>by {}</p>", esc(&facts.byline));
    }
    let url = metadata.work.canonical_url();
    let _ = writeln!(body, "<p><a href=\"{0}\">{0}</a></p>", esc(&url));

    let summary = paragraphs(&facts.summary_html);
    if !summary.is_empty() {
        body.push_str("<h3>Summary</h3>\n");
        for p in summary {
            let _ = writeln!(body, "<p class=\"ff-summary\">{}</p>", esc(&p));
        }
    }

    details(&mut body, extras);

    if !metadata.chapters.is_empty() {
        let latest_added: Vec<&Chapter> = extras
            .history
            .last()
            .filter(|_| extras.history.len() > 1)
            .map_or_default(|v| v.chapters.added.iter().collect());
        body.push_str("<h3>Chapters</h3>\n");
        for chapter in &metadata.chapters {
            let mut line = esc(&chapter_name(chapter));
            if let Some(d) = chapter.posted_on {
                let _ = write!(line, " <span class=\"ff-date\">· {}</span>", day(d));
            }
            if latest_added.iter().any(|c| c.number == chapter.number) {
                line.push_str(" <b>· new</b>");
            }
            let _ = writeln!(body, "<p class=\"ff-chapter\">{line}</p>");
        }
    }

    if !extras.history.is_empty() {
        body.push_str("<h3>History</h3>\n");
        let versions = extras
            .history
            .iter()
            .enumerate()
            .map(|(i, v)| (i.checked_sub(1).and_then(|p| extras.history.get(p)), v));
        for (before, now) in versions.rev().take(HISTORY_LEN) {
            let seen = now.seen_at.strftime("%-d %b %Y").to_string();
            let _ = writeln!(body, "<h4>{seen}</h4>");
            for line in changes(before, now) {
                let _ = writeln!(body, "<p class=\"ff-change\">{line}</p>");
            }
        }
    }

    body.push_str("</section>\n");
    page(
        "About this work",
        lang,
        "p.ff-row, p.ff-chapter, p.ff-change { margin: 0.25em 0; text-indent: 0; } \
         p.ff-summary { text-indent: 0; } span.ff-date { opacity: 0.7; } \
         h4 { margin: 1em 0 0.25em; }",
        &body,
    )
}
