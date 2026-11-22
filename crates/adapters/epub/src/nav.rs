//! The tables of contents: EPUB 3's navigation document and the NCX that older readers use.
//!
//! Nothing may follow the story, or an update moves the reader's place: an older Fanfold
//! listed its "About this work" page here between marker comments, and FanFicFare lists its
//! update log. Both entries are taken out; nothing is added.

const OPEN: &str = "<!--fanfold-->";
const CLOSE: &str = "<!--/fanfold-->";

/// The file name of FanFicFare's update log, whatever folder it is in.
pub const FANFICFARE_LOG: &str = "log_page.xhtml";

/// `text` without the marked range starting at `start` (and its line break).
fn without_marked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start..).unwrap_or_default();
        rest = after
            .find(CLOSE)
            .and_then(|end| after.get(end + CLOSE.len()..))
            .map_or("", |r| r.strip_prefix('\n').unwrap_or(r));
    }
    out.push_str(rest);
    out
}

/// `text` without each `<{tag}` … `</{tag}>` element that links to the update log, with the
/// indentation before it and the line break after it.
fn without_log_entries(text: &str, tag: &str) -> String {
    let link = format!("{FANFICFARE_LOG}\"");
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut text = text.to_owned();
    while let Some(at) = text.find(&link) {
        let Some(start) = text.get(..at).and_then(|t| {
            // `<li>` or `<li …>`, not `<link`.
            t.match_indices(&open)
                .filter(|(i, _)| {
                    matches!(
                        t.as_bytes().get(i + open.len()),
                        Some(b'>' | b' ' | b'\n' | b'\t')
                    )
                })
                .last()
                .map(|(i, _)| i)
        }) else {
            break;
        };
        let Some(end) = text
            .get(at..)
            .and_then(|t| t.find(&close))
            .map(|e| at + e + close.len())
        else {
            break;
        };
        let line_start = text
            .get(..start)
            .map_or(start, |t| t.trim_end_matches([' ', '\t']).len());
        let line_start = if text.get(..line_start).is_some_and(|t| t.ends_with('\n')) {
            line_start
        } else {
            start
        };
        let end = if text.get(end..).is_some_and(|t| t.starts_with('\n')) && line_start < start {
            end + 1
        } else {
            end
        };
        text.replace_range(line_start..end, "");
    }
    text
}

/// The NCX's `playOrder`s numbered again from 1, in document order.
fn renumbered(ncx: &str) -> String {
    const KEY: &str = "playOrder=\"";
    let mut out = String::with_capacity(ncx.len());
    let mut rest = ncx;
    let mut order = 0;
    while let Some(at) = rest.find(KEY) {
        let value_start = at + KEY.len();
        let Some(len) = rest.get(value_start..).and_then(|t| t.find('"')) else {
            break;
        };
        order += 1;
        out.push_str(rest.get(..value_start).unwrap_or_default());
        out.push_str(&order.to_string());
        rest = rest.get(value_start + len..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// The navigation document without Fanfold's entries and the update log's; `None` when it
/// has neither (nothing to rewrite).
#[must_use]
pub fn clean_nav(nav: &str) -> Option<String> {
    let out = without_log_entries(&without_marked(nav), "li");
    (out != nav).then_some(out)
}

/// The NCX without Fanfold's entries and the update log's, its play order closed up; `None`
/// when it has neither.
#[must_use]
pub fn clean_ncx(ncx: &str) -> Option<String> {
    let out = without_log_entries(&without_marked(ncx), "navPoint");
    (out != ncx).then(|| renumbered(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As FanFicFare writes them, with the entries Fanfold's first finishing version added.
    const NAV: &str = "<nav epub:type=\"toc\" id=\"toc\">\n\t<ol>\n\t\t<li>\n\t\t\t<a href=\"OEBPS/title_page.xhtml\">Title Page</a>\n\t\t</li>\n\t\t<li>\n\t\t\t<a href=\"OEBPS/log_page.xhtml\">Update Log</a>\n\t\t</li>\n\t\t<li>\n\t\t\t<a href=\"OEBPS/file0001.xhtml\">1. One</a>\n\t\t</li>\n<!--fanfold--><li><a href=\"OEBPS/fanfold_about.xhtml\">About this work</a></li><!--/fanfold-->\n\t</ol>\n</nav>";
    const NCX: &str = "<navMap>\n\t\t<navPoint id=\"title_page\" playOrder=\"0\">\n\t\t\t<content src=\"OEBPS/title_page.xhtml\"/>\n\t\t</navPoint>\n\t\t<navPoint id=\"log_page\" playOrder=\"1\">\n\t\t\t<navLabel>\n\t\t\t\t<text>Update Log</text>\n\t\t\t</navLabel>\n\t\t\t<content src=\"OEBPS/log_page.xhtml\"/>\n\t\t</navPoint>\n\t\t<navPoint id=\"file0001\" playOrder=\"2\">\n\t\t\t<content src=\"OEBPS/file0001.xhtml\"/>\n\t\t</navPoint>\n<!--fanfold--><navPoint id=\"fanfold-about\" playOrder=\"3\"><navLabel><text>About this work</text></navLabel><content src=\"OEBPS/fanfold_about.xhtml\"/></navPoint><!--/fanfold-->\n</navMap>";

    #[test]
    fn the_update_log_and_an_older_about_entry_are_taken_out() {
        let nav = clean_nav(NAV).unwrap();
        assert_eq!(
            nav,
            "<nav epub:type=\"toc\" id=\"toc\">\n\t<ol>\n\t\t<li>\n\t\t\t<a href=\"OEBPS/title_page.xhtml\">Title Page</a>\n\t\t</li>\n\t\t<li>\n\t\t\t<a href=\"OEBPS/file0001.xhtml\">1. One</a>\n\t\t</li>\n\t</ol>\n</nav>"
        );
        assert_eq!(clean_nav(&nav), None, "a clean document is left alone");
    }

    #[test]
    fn the_ncx_play_order_is_closed_up() {
        let ncx = clean_ncx(NCX).unwrap();
        assert!(
            !ncx.contains("log_page") && !ncx.contains("fanfold"),
            "{ncx}"
        );
        assert!(
            ncx.contains("<navPoint id=\"title_page\" playOrder=\"1\">"),
            "{ncx}"
        );
        assert!(
            ncx.contains("<navPoint id=\"file0001\" playOrder=\"2\">"),
            "{ncx}"
        );
        assert!(
            ncx.contains("</navPoint>\n\t\t<navPoint id=\"file0001\""),
            "{ncx}"
        );
        assert_eq!(clean_ncx(&ncx), None);
    }

    #[test]
    fn a_link_element_is_not_a_list_item() {
        let nav = "<link href=\"x\"/><li><a href=\"log_page.xhtml\">Update Log</a></li>";
        assert_eq!(clean_nav(nav).unwrap(), "<link href=\"x\"/>");
    }
}
