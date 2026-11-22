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
    fn numbers_and_dates_read_naturally() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(day(Date::constant(2026, 11, 21)), "21 Nov 2026");
    }
}
