//! Small text helpers shared by the package, the pages and the cover.

use fanfold_domain::metadata::{Date, TagKind, WorkFacts};

/// Escape text for XML content and attribute values.
#[must_use]
pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // Characters XML 1.0 cannot hold at all.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// AO3's summary HTML as plain paragraphs. The summary is the site's markup, which need not
/// be well-formed XHTML, so it is never copied into the book as markup.
#[must_use]
pub fn paragraphs(html: &str) -> Vec<String> {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        text.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start..).unwrap_or_default();
        let Some(end) = after.find('>') else {
            rest = "";
            break;
        };
        let tag = after
            .get(1..end)
            .unwrap_or_default()
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            tag.as_str(),
            "p" | "br" | "div" | "blockquote" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
        ) {
            text.push('\n');
        }
        rest = after.get(end + 1..).unwrap_or_default();
    }
    text.push_str(rest);
    decode_entities(&text)
        .split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect()
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start..).unwrap_or_default();
        let entity = after
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| after.get(1..end).map(|name| (name, end)));
        let decoded = entity.and_then(|(name, end)| {
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((c, end))
        });
        if let Some((c, end)) = decoded {
            out.push(c);
            rest = after.get(end + 1..).unwrap_or_default();
        } else {
            out.push('&');
            rest = after.get(1..).unwrap_or_default();
        }
    }
    out.push_str(rest);
    out
}

/// `12345` → `12,345`.
#[must_use]
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `21 Nov 2026`.
#[must_use]
pub fn day(date: Date) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = usize::try_from(date.month() - 1)
        .ok()
        .and_then(|m| MONTHS.get(m))
        .copied()
        .unwrap_or("?");
    format!("{} {month} {}", date.day(), date.year())
}

/// Whether every planned chapter is posted.
#[must_use]
pub fn complete(facts: &WorkFacts) -> bool {
    facts.chapters_total == Some(facts.chapters_posted)
}

/// `12/20` or `12/?`.
#[must_use]
pub fn chapter_count(facts: &WorkFacts) -> String {
    match facts.chapters_total {
        Some(total) => format!("{}/{total}", facts.chapters_posted),
        None => format!("{}/?", facts.chapters_posted),
    }
}

/// The label a tag of this kind carries in the book's subject list, and its heading.
#[must_use]
pub const fn tag_label(kind: TagKind) -> (&'static str, &'static str) {
    match kind {
        TagKind::Fandom => ("Fandom", "Fandoms"),
        TagKind::Category => ("Category", "Categories"),
        TagKind::Warning => ("Warning", "Archive warnings"),
        TagKind::Relationship => ("Relationship", "Relationships"),
        TagKind::Character => ("Character", "Characters"),
        TagKind::Freeform => ("Tag", "Additional tags"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_markup_and_drops_what_xml_cannot_hold() {
        assert_eq!(
            esc("a < b & \"c\" 'd' >\u{1}"),
            "a &lt; b &amp; &quot;c&quot; &#39;d&#39; &gt;"
        );
    }

    #[test]
    fn summaries_become_plain_paragraphs() {
        let html = "<p>First &amp; <em>best</em>.</p>\n<p>Second<br/>line&nbsp;two &#8212; &#x2014; &bogus; & done</p><p>  </p>";
        assert_eq!(
            paragraphs(html),
            [
                "First & best.",
                "Second",
                "line two \u{2014} \u{2014} &bogus; & done"
            ]
        );
        assert_eq!(paragraphs("plain text"), ["plain text"]);
        assert_eq!(paragraphs("<p>unclosed <b"), ["unclosed"]);
        assert_eq!(paragraphs(""), [] as [std::string::String; 0]);
    }

    #[test]
    fn numbers_and_dates_read_naturally() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(day(Date::constant(2026, 11, 21)), "21 Nov 2026");
    }
}
