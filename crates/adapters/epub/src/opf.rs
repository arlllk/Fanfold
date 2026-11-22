//! Rewriting the package document (the `.opf`).
//!
//! What the downloader wrote stays, except what Fanfold replaces; everything Fanfold adds
//! carries a `fanfold-` id or an `ao3:` property, so a second pass removes the first one's
//! additions before adding its own.

use quick_xml::{
    Reader, Writer, XmlVersion,
    events::{BytesStart, Event},
};

/// The vocabulary behind the `ao3:` properties.
pub const AO3_PREFIX: &str = "ao3";
pub const AO3_VOCABULARY: &str =
    "https://github.com/arlllk/Fanfold/blob/main/docs/epub-metadata.md#";

/// Prefix of every id Fanfold gives an element.
pub const ID_PREFIX: &str = "fanfold-";

/// What goes into each part of the package. Each string is a run of complete elements.
#[derive(Debug, Default)]
pub struct Additions {
    pub metadata: String,
    pub manifest: String,
    /// Spine entries read before the downloader's (the cover).
    pub spine_first: String,
    /// Spine entries read after the downloader's (the about page).
    pub spine_last: String,
    /// An EPUB 3 package: declare the `ao3:` prefix, replace the downloader's `dc:date`s.
    pub epub3: bool,
}

/// One manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub properties: String,
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.try_get_attribute(name).ok().flatten().and_then(|a| {
        a.normalized_value(XmlVersion::Implicit1_0)
            .ok()
            .map(std::borrow::Cow::into_owned)
    })
}

fn local(e: &BytesStart<'_>) -> String {
    e.local_name().as_ref().to_owned()
}

fn prefix(e: &BytesStart<'_>) -> Option<String> {
    e.name().prefix().map(|p| p.as_ref().to_owned())
}

/// The package's `version` attribute.
#[must_use]
pub fn version(opf: &str) -> Option<String> {
    let mut reader = Reader::from_str(opf);
    loop {
        match reader.read_event().ok()? {
            Event::Eof => return None,
            Event::Start(e) | Event::Empty(e) if local(&e) == "package" => {
                return attr(&e, "version");
            }
            _ => {}
        }
    }
}

/// The manifest's items, in document order.
pub fn manifest(opf: &str) -> Result<Vec<Item>, String> {
    let mut reader = Reader::from_str(opf);
    let mut items = Vec::new();
    loop {
        match reader.read_event().map_err(|e| format!("package: {e}"))? {
            Event::Eof => return Ok(items),
            Event::Start(e) | Event::Empty(e) if local(&e) == "item" => items.push(Item {
                id: attr(&e, "id").unwrap_or_default(),
                href: attr(&e, "href").unwrap_or_default(),
                media_type: attr(&e, "media-type").unwrap_or_default(),
                properties: attr(&e, "properties").unwrap_or_default(),
            }),
            _ => {}
        }
    }
}

/// Whether a child of `parent` is one Fanfold replaces.
fn replaced(parent: &str, e: &BytesStart<'_>, epub3: bool) -> bool {
    let ours = |name: &str| attr(e, name).is_some_and(|v| v.starts_with(ID_PREFIX));
    match (parent, local(e).as_str()) {
        ("metadata", name) => {
            let dc = prefix(e).as_deref() == Some("dc");
            if dc && (name == "subject" || (epub3 && name == "date")) {
                return true;
            }
            if ours("id") || attr(e, "refines").is_some_and(|r| r.starts_with("#fanfold-")) {
                return true;
            }
            if name != "meta" {
                return false;
            }
            let named = attr(e, "name");
            let property = attr(e, "property").unwrap_or_default();
            matches!(
                named.as_deref(),
                Some("calibre:series" | "calibre:series_index" | "cover")
            ) || named
                .as_deref()
                .is_some_and(|n| n.starts_with("fanfold:") || n.starts_with("ao3:"))
                || property.starts_with("ao3:")
                || property == "belongs-to-collection"
        }
        ("manifest", "item") => ours("id"),
        ("spine", "itemref") => ours("idref"),
        _ => false,
    }
}

/// The package element with the `ao3:` prefix declared.
fn with_prefix(e: &BytesStart<'_>) -> BytesStart<'static> {
    let name = e.name().as_ref().to_owned();
    let mut out = BytesStart::new(name);
    let mut declared = false;
    for a in e.attributes().flatten() {
        let key = a.key.as_ref().to_owned();
        let value = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_or_default(std::borrow::Cow::into_owned);
        if key == "prefix" {
            declared = true;
            let value = if value
                .split_whitespace()
                .any(|p| p == format!("{AO3_PREFIX}:"))
            {
                value
            } else {
                format!("{value} {AO3_PREFIX}: {AO3_VOCABULARY}")
                    .trim()
                    .to_owned()
            };
            out.push_attribute((key.as_str(), value.as_str()));
        } else {
            out.push_attribute((key.as_str(), value.as_str()));
        }
    }
    if !declared {
        let value = format!("{AO3_PREFIX}: {AO3_VOCABULARY}");
        out.push_attribute(("prefix", value.as_str()));
    }
    out
}

/// A manifest item other than Fanfold's, without the `cover-image` property: the book has
/// one cover, Fanfold's.
fn without_cover_property(e: &BytesStart<'_>) -> Option<BytesStart<'static>> {
    let properties = attr(e, "properties")?;
    if !properties.split_whitespace().any(|p| p == "cover-image") {
        return None;
    }
    let kept: Vec<&str> = properties
        .split_whitespace()
        .filter(|p| *p != "cover-image")
        .collect();
    let name = e.name().as_ref().to_owned();
    let mut out = BytesStart::new(name);
    for a in e.attributes().flatten() {
        let key = a.key.as_ref().to_owned();
        if key == "properties" {
            if !kept.is_empty() {
                out.push_attribute(("properties", kept.join(" ").as_str()));
            }
        } else {
            let value = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map_or_default(std::borrow::Cow::into_owned);
            out.push_attribute((key.as_str(), value.as_str()));
        }
    }
    Some(out)
}

/// The package document with Fanfold's additions in place of the previous ones.
pub fn rewrite(opf: &str, add: &Additions) -> Result<String, String> {
    let mut reader = Reader::from_str(opf);
    let mut writer = Writer::new(Vec::with_capacity(opf.len() + add.metadata.len() + 1024));
    let mut stack: Vec<String> = Vec::new();
    // While inside a replaced element: the depth it started at.
    let mut skipping: Option<usize> = None;
    // Whitespace before the next element: dropped with it when it is replaced, so replaced
    // elements leave no blank lines behind and a second pass changes nothing.
    let mut pending = String::new();
    let raw = |w: &mut Writer<Vec<u8>>, text: &str| w.get_mut().extend_from_slice(text.as_bytes());
    let flush = |w: &mut Writer<Vec<u8>>, pending: &mut String| {
        w.get_mut().extend_from_slice(pending.as_bytes());
        pending.clear();
    };
    let fail = |e: &dyn std::fmt::Display| format!("package: {e}");
    loop {
        let event = reader.read_event().map_err(|e| fail(&e))?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                let name = local(&e);
                if skipping.is_none() {
                    let parent = stack.last().map_or("", String::as_str);
                    if replaced(parent, &e, add.epub3) {
                        skipping = Some(stack.len());
                        pending.clear();
                    } else if name == "package" && add.epub3 {
                        flush(&mut writer, &mut pending);
                        writer
                            .write_event(Event::Start(with_prefix(&e)))
                            .map_err(|e| fail(&e))?;
                    } else {
                        flush(&mut writer, &mut pending);
                        writer
                            .write_event(Event::Start(e.borrow()))
                            .map_err(|e| fail(&e))?;
                        if name == "spine" {
                            raw(&mut writer, &add.spine_first);
                        }
                    }
                }
                stack.push(name);
            }
            Event::Empty(e) => {
                if skipping.is_some() {
                    continue;
                }
                let parent = stack.last().map_or("", String::as_str);
                if replaced(parent, &e, add.epub3) {
                    pending.clear();
                    continue;
                }
                flush(&mut writer, &mut pending);
                let rewritten = if local(&e) == "package" && add.epub3 {
                    Some(with_prefix(&e))
                } else if parent == "manifest" {
                    without_cover_property(&e)
                } else {
                    None
                };
                match rewritten {
                    Some(e) => writer.write_event(Event::Empty(e)),
                    None => writer.write_event(Event::Empty(e)),
                }
                .map_err(|e| fail(&e))?;
            }
            Event::End(e) => {
                let name = stack.pop().unwrap_or_default();
                if let Some(depth) = skipping {
                    if stack.len() == depth {
                        skipping = None;
                    }
                    continue;
                }
                let addition = match name.as_str() {
                    "metadata" => Some(&add.metadata),
                    "manifest" => Some(&add.manifest),
                    "spine" => Some(&add.spine_last),
                    _ => None,
                };
                match addition {
                    // The additions bring their own line breaks and indentation.
                    Some(text) => {
                        pending.clear();
                        raw(&mut writer, text);
                    }
                    None => flush(&mut writer, &mut pending),
                }
                writer.write_event(Event::End(e)).map_err(|e| fail(&e))?;
            }
            Event::Text(t) if skipping.is_none() && t.as_ref().trim().is_empty() => {
                pending.push_str(t.as_ref());
            }
            other => {
                if skipping.is_none() {
                    flush(&mut writer, &mut pending);
                    writer.write_event(other).map_err(|e| fail(&e))?;
                }
            }
        }
    }
    flush(&mut writer, &mut pending);
    String::from_utf8(writer.into_inner()).map_err(|e| fail(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const FANFICFARE: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<package version="3.0" prefix="calibre: https://calibre-ebook.com" xmlns="http://www.idpf.org/2007/opf" unique-identifier="fanficfare-uid">
	<metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
		<dc:identifier id="fanficfare-uid">fanficfare-uid:test1.com-u1-s1001</dc:identifier>
		<dc:title id="id">Test &amp; Story</dc:title>
		<dc:creator id="id-1">Test Author</dc:creator>
		<dc:language>en</dc:language>
		<meta property="dcterms:modified">2026-11-22T01:25:48Z</meta>
		<meta name="calibre:series" content="Old"/>
		<meta name="calibre:series_index" content="1.00"/>
		<dc:description>A summary.</dc:description>
		<dc:subject>Last Update: 1975/04/15</dc:subject>
		<dc:subject>In-Progress</dc:subject>
		<meta property="role" refines="#id-1" scheme="marc:relators">aut</meta>
	</metadata>
	<manifest>
		<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
		<item id="old-cover" href="OEBPS/images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>
		<item id="title_page" href="OEBPS/title_page.xhtml" media-type="application/xhtml+xml"/>
		<item id="file0001" href="OEBPS/file0001.xhtml" media-type="application/xhtml+xml"/>
		<item href="nav.xhtml" id="nav" media-type="application/xhtml+xml" properties="nav"/>
	</manifest>
	<spine toc="ncx">
		<itemref idref="title_page" linear="yes"/>
		<itemref idref="file0001" linear="yes"/>
	</spine>
</package>"##;

    fn additions() -> Additions {
        Additions {
            metadata: "<dc:subject>Fandom: X</dc:subject><meta property=\"ao3:words\">5</meta><meta property=\"belongs-to-collection\" id=\"fanfold-series-1\">S</meta><meta refines=\"#fanfold-series-1\" property=\"collection-type\">series</meta><meta name=\"cover\" content=\"fanfold-cover-image\"/>".into(),
            manifest: "<item id=\"fanfold-about\" href=\"OEBPS/fanfold_about.xhtml\" media-type=\"application/xhtml+xml\"/>".into(),
            spine_first: "<itemref idref=\"fanfold-cover\"/>".into(),
            spine_last: "<itemref idref=\"fanfold-about\"/>".into(),
            epub3: true,
        }
    }

    #[test]
    fn replaces_subjects_series_and_cover_and_keeps_the_rest() {
        let out = rewrite(FANFICFARE, &additions()).unwrap();
        assert!(!out.contains("Last Update"), "{out}");
        assert!(!out.contains("content=\"Old\""), "{out}");
        assert!(
            out.contains("<dc:title id=\"id\">Test &amp; Story</dc:title>"),
            "{out}"
        );
        assert!(out.contains("<dc:description>A summary.</dc:description>"));
        assert!(out.contains("refines=\"#id-1\""));
        assert!(out.contains("<dc:subject>Fandom: X</dc:subject>"));
        assert!(
            out.contains(&format!(
                "prefix=\"calibre: https://calibre-ebook.com ao3: {AO3_VOCABULARY}\""
            )),
            "{out}"
        );
        // The downloader's own cover image stays, but is no longer the cover.
        assert!(
            out.contains(
                "id=\"old-cover\" href=\"OEBPS/images/cover.jpg\" media-type=\"image/jpeg\"/>"
            ),
            "{out}"
        );
        let spine = out.split("<spine").nth(1).unwrap();
        let cover = spine.find("fanfold-cover").unwrap();
        let title = spine.find("title_page").unwrap();
        let about = spine.find("fanfold-about").unwrap();
        assert!(cover < title && title < about, "{spine}");
    }

    #[test]
    fn a_second_pass_replaces_the_first() {
        let once = rewrite(FANFICFARE, &additions()).unwrap();
        let twice = rewrite(&once, &additions()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(
            twice.matches("fanfold-about\"").count(),
            2,
            "one item, one itemref"
        );
        assert_eq!(twice.matches("ao3: ").count(), 1);
    }

    #[test]
    fn the_manifest_is_read_in_order() {
        let items = manifest(FANFICFARE).unwrap();
        let ids: Vec<_> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["ncx", "old-cover", "title_page", "file0001", "nav"]);
        let nav = items.iter().find(|i| i.properties == "nav").unwrap();
        assert_eq!(nav.href, "nav.xhtml");
    }

    #[test]
    fn broken_packages_are_errors() {
        assert!(rewrite("<package><metadata></package>", &additions()).is_err());
    }
}
