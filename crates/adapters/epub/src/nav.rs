//! The about page in the tables of contents: EPUB 3's navigation document and the NCX that
//! older readers use. Fanfold's entries sit between marker comments, so a second pass
//! replaces them.

const OPEN: &str = "<!--fanfold-->";
const CLOSE: &str = "<!--/fanfold-->";
const LABEL: &str = "About this work";

fn without_ours(text: &str) -> String {
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

/// `to`, relative to the folder holding `from` (both relative to the package's folder).
#[must_use]
pub fn relative(from: &str, to: &str) -> String {
    let from_dirs: Vec<&str> = from.split('/').collect();
    let from_dirs = from_dirs.split_last().map_or(&[][..], |(_, d)| d);
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from_dirs
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_dirs.len() - common;
    let mut parts: Vec<&str> = std::iter::repeat_n("..", ups).collect();
    parts.extend(to_parts.iter().skip(common));
    parts.join("/")
}

/// The navigation document with an entry for the about page at the end of its table of
/// contents; `None` when it has no recognisable one.
#[must_use]
pub fn nav_with_about(nav: &str, href: &str) -> Option<String> {
    let nav = without_ours(nav);
    let toc = nav.find("epub:type=\"toc\"")?;
    let end = toc + nav.get(toc..)?.find("</nav>")?;
    let list_end = nav.get(..end)?.rfind("</ol>")?;
    if list_end < toc {
        return None;
    }
    let entry = format!("{OPEN}<li><a href=\"{href}\">{LABEL}</a></li>{CLOSE}\n");
    Some(format!(
        "{}{entry}{}",
        nav.get(..list_end)?,
        nav.get(list_end..)?
    ))
}

/// The NCX with a last navigation point for the about page; `None` without a `navMap`.
#[must_use]
pub fn ncx_with_about(ncx: &str, href: &str) -> Option<String> {
    let ncx = without_ours(ncx);
    let end = ncx.rfind("</navMap>")?;
    let order = ncx.matches("<navPoint").count() + 1;
    let entry = format!(
        "{OPEN}<navPoint id=\"fanfold-about\" playOrder=\"{order}\"><navLabel><text>{LABEL}</text></navLabel><content src=\"{href}\"/></navPoint>{CLOSE}\n"
    );
    Some(format!("{}{entry}{}", ncx.get(..end)?, ncx.get(end..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAV: &str = r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body>
<nav epub:type="toc"><ol>
<li><a href="OEBPS/title_page.xhtml">Title Page</a></li>
<li><a href="OEBPS/file0001.xhtml">1. One</a></li>
</ol></nav>
<nav epub:type="landmarks"><ol><li><a href="x">x</a></li></ol></nav>
</body></html>"#;

    const NCX: &str = r#"<ncx><navMap>
<navPoint id="a" playOrder="1"><navLabel><text>One</text></navLabel><content src="OEBPS/file0001.xhtml"/></navPoint>
<navPoint id="b" playOrder="2"><navLabel><text>Two</text></navLabel><content src="OEBPS/file0002.xhtml"/></navPoint>
</navMap></ncx>"#;

    #[test]
    fn the_about_page_ends_the_table_of_contents_once() {
        let once = nav_with_about(NAV, "OEBPS/fanfold_about.xhtml").unwrap();
        let toc = once.split("landmarks").next().unwrap();
        assert!(toc.contains("<li><a href=\"OEBPS/fanfold_about.xhtml\">About this work</a></li>"));
        assert!(toc.find("file0001").unwrap() < toc.find("fanfold_about").unwrap());
        assert_eq!(
            nav_with_about(&once, "OEBPS/fanfold_about.xhtml").unwrap(),
            once
        );
        assert!(nav_with_about("<html/>", "x").is_none());
    }

    #[test]
    fn the_ncx_gets_the_next_play_order_once() {
        let once = ncx_with_about(NCX, "OEBPS/fanfold_about.xhtml").unwrap();
        assert!(once.contains("playOrder=\"3\""), "{once}");
        let twice = ncx_with_about(&once, "OEBPS/fanfold_about.xhtml").unwrap();
        assert_eq!(once, twice);
        assert!(ncx_with_about("<ncx/>", "x").is_none());
    }

    #[test]
    fn paths_are_made_relative() {
        assert_eq!(relative("nav.xhtml", "OEBPS/a.xhtml"), "OEBPS/a.xhtml");
        assert_eq!(relative("OEBPS/nav.xhtml", "OEBPS/a.xhtml"), "a.xhtml");
        assert_eq!(
            relative("text/nav.xhtml", "OEBPS/a.xhtml"),
            "../OEBPS/a.xhtml"
        );
    }
}
