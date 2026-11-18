//! Where books live in the library folder.
//!
//! ```text
//! series/<series name> <series id>/<part> <title> <work id>.epub   a work in a followed series
//! single/<byline>/<title> <work id>.epub                           any other work
//! ```
//!
//! A book's path is chosen at its first download and never changes afterwards: e-readers
//! key reading progress on it. So is a series' folder name, once chosen.

use crate::work::{SeriesId, WorkId};

const MAX_TITLE_BYTES: usize = 150;
const MAX_BYLINE_BYTES: usize = 100;
const MAX_SERIES_NAME_BYTES: usize = 150;
/// Longest single path component most filesystems accept.
const MAX_COMPONENT_BYTES: usize = 255;
/// `series/<folder>/<file>` is the deepest layout.
const MAX_DEPTH: usize = 3;

pub const SERIES_DIR: &str = "series";
pub const SINGLE_DIR: &str = "single";

/// `text` made safe as one path component. May be empty.
///
/// No separators, control characters or characters that some filesystems reject, single
/// spaces, no leading or trailing dots, at most `max_bytes` bytes (cut on a character
/// boundary).
#[must_use]
pub fn clean_component(text: &str, max_bytes: usize) -> String {
    let mut cleaned = String::new();
    let mut last_was_space = true;
    for c in text.chars() {
        let c = if c.is_control() || "/\\:*?\"<>|".contains(c) {
            ' '
        } else {
            c
        };
        if c.is_whitespace() {
            if !last_was_space {
                cleaned.push(' ');
            }
            last_was_space = true;
        } else {
            cleaned.push(c);
            last_was_space = false;
        }
    }
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    let mut bounded = String::new();
    for c in trimmed.chars() {
        if bounded.len() + c.len_utf8() > max_bytes {
            break;
        }
        bounded.push(c);
    }
    bounded
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
        .to_owned()
}

fn or_default(cleaned: String, default: &str) -> String {
    if cleaned.is_empty() {
        default.to_owned()
    } else {
        cleaned
    }
}

/// The folder (inside `series/`) for a series: `<name> <id>`.
#[must_use]
pub fn series_folder(name: &str, series: SeriesId) -> String {
    let name = or_default(clean_component(name, MAX_SERIES_NAME_BYTES), "Series");
    format!("{name} {series}")
}

/// `series/<folder>/<part> <title> <work>.epub`; the part is zero-padded to two digits so
/// a file list sorts in reading order.
#[must_use]
pub fn series_book_path(folder: &str, part: Option<u32>, title: &str, work: WorkId) -> String {
    let title = or_default(clean_component(title, MAX_TITLE_BYTES), "Untitled");
    let folder = or_default(clean_component(folder, MAX_COMPONENT_BYTES - 1), "Series");
    match part {
        Some(part) => format!("{SERIES_DIR}/{folder}/{part:02} {title} {work}.epub"),
        None => format!("{SERIES_DIR}/{folder}/{title} {work}.epub"),
    }
}

/// `single/<byline>/<title> <work>.epub`.
#[must_use]
pub fn single_book_path(byline: &str, title: &str, work: WorkId) -> String {
    let byline = or_default(clean_component(byline, MAX_BYLINE_BYTES), "Unknown author");
    let title = or_default(clean_component(title, MAX_TITLE_BYTES), "Untitled");
    format!("{SINGLE_DIR}/{byline}/{title} {work}.epub")
}

/// Whether `path` is acceptable as a library path. Every store checks it before any IO.
///
/// It must be relative, at most three components deep, each a plain visible name (no
/// `.`/`..`, no leading dot, no backslash or control character, at most 255 bytes), and
/// end in a `*.epub` file name.
#[must_use]
pub fn is_valid_library_path(path: &str) -> bool {
    let components: Vec<&str> = path.split('/').collect();
    let Some(file) = components.last() else {
        return false;
    };
    components.len() <= MAX_DEPTH
        && components.iter().all(|c| {
            !c.is_empty()
                && c.len() <= MAX_COMPONENT_BYTES
                && !c.starts_with('.')
                && !c.chars().any(|ch| ch == '\\' || ch.is_control())
        })
        && std::path::Path::new(file)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("epub"))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn work(n: u64) -> WorkId {
        WorkId::new(n).unwrap()
    }

    fn series(n: u64) -> SeriesId {
        SeriesId::new(n).unwrap()
    }

    #[test]
    fn the_two_layouts() {
        let folder = series_folder("The Harbor Saga", series(2_000_001));
        assert_eq!(folder, "The Harbor Saga 2000001");
        assert_eq!(
            series_book_path(
                &folder,
                Some(2),
                "The Keeper and the Logbook",
                work(10_000_004)
            ),
            "series/The Harbor Saga 2000001/02 The Keeper and the Logbook 10000004.epub"
        );
        assert_eq!(
            series_book_path(&folder, Some(123), "T", work(1)),
            "series/The Harbor Saga 2000001/123 T 1.epub"
        );
        assert_eq!(
            single_book_path("quillandink", "A Story: Part 1?", work(7)),
            "single/quillandink/A Story Part 1 7.epub"
        );
    }

    #[test]
    fn empty_and_hostile_names_get_a_placeholder_never_a_traversal() {
        assert_eq!(
            single_book_path("", "", work(1)),
            "single/Unknown author/Untitled 1.epub"
        );
        assert_eq!(
            single_book_path("../..", "../../etc/passwd", work(2)),
            "single/Unknown author/etc passwd 2.epub"
        );
        assert_eq!(series_folder("...", series(3)), "Series 3");
        assert_eq!(
            series_book_path("../x", None, ".hidden", work(4)),
            "series/x/hidden 4.epub"
        );
    }

    #[test]
    fn titles_are_cut_at_the_byte_limit_on_a_character_boundary() {
        let name = |title: &str| single_book_path("a", title, work(5));
        let exact = "a".repeat(150);
        assert_eq!(name(&exact), format!("single/a/{exact} 5.epub"));
        assert_eq!(name(&"a".repeat(151)), format!("single/a/{exact} 5.epub"));
        let wide = "é".repeat(75); // exactly 150 bytes
        assert_eq!(name(&"é".repeat(76)), format!("single/a/{wide} 5.epub"));
    }

    #[test]
    fn validation() {
        for ok in ["a.epub", "single/x/a 1.epub", "series/S 1/01 T 2.EPUB"] {
            assert!(is_valid_library_path(ok), "{ok}");
        }
        for bad in [
            "",
            "a.txt",
            "/abs/a.epub",
            "single//a.epub",
            "../a.epub",
            "single/../a.epub",
            "single/.hidden/a.epub",
            "a/b/c/d.epub",
            "single\\x/a.epub",
            "single/x\n/a.epub",
            ".epub",
        ] {
            assert!(!is_valid_library_path(bad), "{bad:?}");
        }
        assert!(!is_valid_library_path(&format!("{}.epub", "a".repeat(251))));
    }

    proptest! {
        #[test]
        fn every_generated_path_is_valid(
            byline in ".{0,300}", title in ".{0,300}", name in ".{0,300}",
            part in proptest::option::of(0u32..10_000), w in 1u64..i64::MAX as u64, s in 1u64..i64::MAX as u64,
        ) {
            let single = single_book_path(&byline, &title, work(w));
            prop_assert!(is_valid_library_path(&single), "{single:?}");
            let folder = series_folder(&name, series(s));
            let in_series = series_book_path(&folder, part, &title, work(w));
            prop_assert!(is_valid_library_path(&in_series), "{in_series:?}");
            prop_assert_eq!(in_series.split('/').nth(1), Some(folder.as_str()), "the folder is used as is");
        }
    }
}
