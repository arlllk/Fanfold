//! What Fanfold writes into a book's package: labelled subjects, every series, the `ao3:`
//! properties, the cover.

use std::fmt::Write as _;

use fanfold_app::ports::BookExtras;
use fanfold_domain::{
    Timestamp,
    metadata::{Author, SeriesPart, TagKind},
};

use crate::{
    opf::{Additions, ID_PREFIX},
    text::{complete, esc, tag_label},
};

/// What a book finished by this version carries, so books finished by an older one (or never)
/// are finished again at their next check. Bump it when what is added changes.
pub const FINISH_VERSION: u32 = 1;

/// The marker in the package, as written.
#[must_use]
pub fn finish_marker() -> String {
    format!("<meta name=\"fanfold:finish\" content=\"{FINISH_VERSION}\"/>")
}

/// Names of Fanfold's files, relative to the package document.
pub const COVER_IMAGE: &str = "OEBPS/fanfold_cover.png";
pub const COVER_PAGE: &str = "OEBPS/fanfold_cover.xhtml";
pub const ABOUT_PAGE: &str = "OEBPS/fanfold_about.xhtml";

/// `2026-11-21`.
#[must_use]
pub fn iso_day(at: Timestamp) -> String {
    at.strftime("%Y-%m-%d").to_string()
}

/// An AO3 profile page.
#[must_use]
pub fn author_url(author: &Author) -> String {
    fn part(s: &str) -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    }
    format!(
        "https://archiveofourown.org/users/{}/pseuds/{}",
        part(&author.user),
        part(&author.pseud)
    )
}

/// `pseud (user)`, or just the name when they are the same.
#[must_use]
pub fn author_name(author: &Author) -> String {
    if author.pseud.is_empty() || author.pseud == author.user {
        author.user.clone()
    } else {
        format!("{} ({})", author.pseud, author.user)
    }
}

/// The series a reader groups the book under: the requested one, else the first.
#[must_use]
pub fn main_series(extras: &BookExtras) -> Option<&SeriesPart> {
    let series = &extras.metadata.series;
    extras
        .main_series
        .and_then(|id| series.iter().find(|p| p.id == id))
        .or_else(|| series.first())
}

/// The subjects, labelled by kind so a reader's tag list groups them: rating, warnings,
/// categories, fandoms, relationships, characters, additional tags, status.
#[must_use]
pub fn subjects(extras: &BookExtras) -> Vec<String> {
    let facts = &extras.metadata.facts;
    let mut out = Vec::new();
    if !facts.rating.is_empty() {
        out.push(format!("Rating: {}", facts.rating));
    }
    for kind in [
        TagKind::Warning,
        TagKind::Category,
        TagKind::Fandom,
        TagKind::Relationship,
        TagKind::Character,
        TagKind::Freeform,
    ] {
        let (label, _) = tag_label(kind);
        for tag in extras.metadata.tags.iter().filter(|t| t.kind == kind) {
            let subject = format!("{label}: {}", tag.name);
            if !out.contains(&subject) {
                out.push(subject);
            }
        }
    }
    out.push(if complete(facts) {
        "Status: Complete".to_owned()
    } else {
        "Status: In progress".to_owned()
    });
    out
}

/// One `ao3:` fact: an EPUB 3 property, or EPUB 2's name and content.
fn meta(out: &mut String, epub3: bool, property: &str, value: &str) {
    let value = esc(value);
    let _ = if epub3 {
        writeln!(out, "\t\t<meta property=\"{property}\">{value}</meta>")
    } else {
        writeln!(out, "\t\t<meta name=\"{property}\" content=\"{value}\"/>")
    };
}

/// The package additions for a book; `epub3` for an EPUB 3 package (otherwise only what
/// EPUB 2 allows).
#[allow(
    clippy::too_many_lines,
    reason = "one flat list of properties, in output order"
)]
#[must_use]
pub fn additions(extras: &BookExtras, epub3: bool) -> Additions {
    let metadata = &extras.metadata;
    let facts = &metadata.facts;
    let mut m = String::from("\n");

    // EPUB 2 books keep the downloader's dated `dc:date`s; its EPUB 3 books have none.
    if let Some(published) = facts.published_on.filter(|_| epub3) {
        let _ = writeln!(m, "\t\t<dc:date>{published}</dc:date>");
    }
    for subject in subjects(extras) {
        let _ = writeln!(m, "\t\t<dc:subject>{}</dc:subject>", esc(&subject));
    }

    // Series: calibre's form (one series, the one e-readers group by) and EPUB 3's (all).
    if let Some(series) = main_series(extras) {
        let _ = writeln!(
            m,
            "\t\t<meta name=\"calibre:series\" content=\"{}\"/>",
            esc(&series.name)
        );
        if let Some(part) = series.part {
            let _ = writeln!(
                m,
                "\t\t<meta name=\"calibre:series_index\" content=\"{part}.00\"/>"
            );
        }
    }
    let ordered = main_series(extras).into_iter().chain(
        metadata
            .series
            .iter()
            .filter(|p| Some(p.id) != main_series(extras).map(|m| m.id)),
    );
    for (n, series) in ordered.enumerate().filter(|_| epub3) {
        let id = format!("{ID_PREFIX}series-{}", n + 1);
        let _ = writeln!(
            m,
            "\t\t<meta property=\"belongs-to-collection\" id=\"{id}\">{}</meta>",
            esc(&series.name)
        );
        let _ = writeln!(
            m,
            "\t\t<meta refines=\"#{id}\" property=\"collection-type\">series</meta>"
        );
        if let Some(part) = series.part {
            let _ = writeln!(
                m,
                "\t\t<meta refines=\"#{id}\" property=\"group-position\">{part}</meta>"
            );
        }
        let _ = writeln!(
            m,
            "\t\t<meta refines=\"#{id}\" property=\"dcterms:identifier\">{}</meta>",
            esc(&series.id.canonical_url())
        );
    }

    // The same facts, one property each, for software.
    meta(
        &mut m,
        epub3,
        "ao3:work-id",
        &metadata.work.get().to_string(),
    );
    meta(&mut m, epub3, "ao3:rating", &facts.rating);
    for kind in [
        TagKind::Warning,
        TagKind::Category,
        TagKind::Fandom,
        TagKind::Relationship,
        TagKind::Character,
        TagKind::Freeform,
    ] {
        for tag in metadata.tags.iter().filter(|t| t.kind == kind) {
            meta(&mut m, epub3, &format!("ao3:{}", kind.as_str()), &tag.name);
        }
    }
    for author in &metadata.authors {
        meta(&mut m, epub3, "ao3:author", &author_name(author));
    }
    meta(
        &mut m,
        epub3,
        "ao3:status",
        if complete(facts) {
            "complete"
        } else {
            "in-progress"
        },
    );
    meta(
        &mut m,
        epub3,
        "ao3:chapters-posted",
        &facts.chapters_posted.to_string(),
    );
    if let Some(total) = facts.chapters_total {
        meta(&mut m, epub3, "ao3:chapters-total", &total.to_string());
    }
    meta(&mut m, epub3, "ao3:words", &facts.words.to_string());
    if let Some(published) = facts.published_on {
        meta(&mut m, epub3, "ao3:published", &published.to_string());
    }
    if let Some(updated) = facts.updated_on {
        meta(&mut m, epub3, "ao3:updated", &updated.to_string());
    }
    meta(
        &mut m,
        epub3,
        "ao3:restricted",
        if facts.restricted { "true" } else { "false" },
    );
    let stats = metadata.stats;
    for (name, value) in [
        ("kudos", stats.kudos),
        ("hits", stats.hits),
        ("bookmarks", stats.bookmarks),
        ("comments", stats.comments),
    ] {
        if let Some(value) = value {
            meta(&mut m, epub3, &format!("ao3:{name}"), &value.to_string());
        }
    }
    meta(&mut m, epub3, "ao3:stats-date", &iso_day(extras.fetched_at));
    if let Some(first) = extras.history.first() {
        meta(
            &mut m,
            epub3,
            "ao3:first-downloaded",
            &iso_day(first.seen_at),
        );
    }

    let _ = writeln!(
        m,
        "\t\t<meta name=\"cover\" content=\"{ID_PREFIX}cover-image\"/>"
    );
    let _ = writeln!(m, "\t\t{}", finish_marker());
    m.push('\t');

    Additions {
        metadata: m,
        manifest: format!(
            "\n\t\t<item id=\"{ID_PREFIX}cover-image\" href=\"{COVER_IMAGE}\" media-type=\"image/png\"{}/>\n\
             \t\t<item id=\"{ID_PREFIX}cover\" href=\"{COVER_PAGE}\" media-type=\"application/xhtml+xml\"/>\n\
             \t\t<item id=\"{ID_PREFIX}about\" href=\"{ABOUT_PAGE}\" media-type=\"application/xhtml+xml\"/>\n\t",
            if epub3 {
                " properties=\"cover-image\""
            } else {
                ""
            }
        ),
        epub3,
        spine_first: format!("\n\t\t<itemref idref=\"{ID_PREFIX}cover\" linear=\"yes\"/>"),
        spine_last: format!("\n\t\t<itemref idref=\"{ID_PREFIX}about\" linear=\"yes\"/>\n\t"),
    }
}
