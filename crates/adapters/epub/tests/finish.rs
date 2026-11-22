//! The finisher against a real FanFicFare EPUB 3 (from its offline test site).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use fanfold_adapter_epub::EpubFinisher;
use fanfold_app::ports::{BookExtras, BookFinisher};
use fanfold_domain::{
    Timestamp,
    metadata::{Changes, Tag, TagKind, WorkVersionChanges},
};
use fanfold_testkit::contracts::observed_work;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/fanficfare-epub3.epub"
);
/// FanFicFare's default before Fanfold asked for EPUB 3: the books already in libraries.
const FIXTURE_EPUB2: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/fanficfare-epub2.epub"
);

fn t(secs: i64) -> Timestamp {
    Timestamp::from_second(secs).unwrap()
}

fn extras() -> BookExtras {
    let metadata = observed_work(10_000_001);
    let mut first = WorkVersionChanges {
        seen_at: t(1_791_000_000),
        facts: metadata.facts.clone(),
        authors: Changes {
            added: metadata.authors.clone(),
            removed: Vec::new(),
        },
        tags: Changes {
            added: metadata.tags.clone(),
            removed: Vec::new(),
        },
        series: Changes {
            added: metadata.series.clone(),
            removed: Vec::new(),
        },
        chapters: Changes {
            added: metadata.chapters.iter().take(1).cloned().collect(),
            removed: Vec::new(),
        },
    };
    first.facts.chapters_posted = 1;
    first.facts.words = 60_000;
    let fluff = Tag {
        kind: TagKind::Freeform,
        name: "Fluff & <Angst>".into(),
    };
    let second = WorkVersionChanges {
        seen_at: t(1_792_000_000),
        facts: metadata.facts.clone(),
        authors: Changes::default(),
        tags: Changes {
            added: vec![fluff.clone()],
            removed: Vec::new(),
        },
        series: Changes::default(),
        chapters: Changes {
            added: metadata.chapters.iter().skip(1).cloned().collect(),
            removed: Vec::new(),
        },
    };
    let mut metadata = metadata;
    metadata.tags.push(fluff);
    BookExtras {
        metadata,
        history: vec![first, second],
        main_series: None,
        fetched_at: t(1_792_000_000),
    }
}

fn copy_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("book.epub");
    std::fs::copy(FIXTURE, &path).unwrap();
    path
}

struct Book {
    names: Vec<String>,
    archive: zip::ZipArchive<std::fs::File>,
}

impl Book {
    fn open(path: &Path) -> Self {
        let archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let names = archive
            .file_names()
            .map(|n| n.unwrap().into_owned())
            .collect();
        Self { names, archive }
    }

    fn text(&mut self, name: &str) -> String {
        let mut out = String::new();
        self.archive
            .by_name(name)
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        out
    }

    fn bytes(&mut self, name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        self.archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }
}

/// Fails on anything that is not well-formed XML.
fn well_formed(name: &str, text: &str) {
    let mut reader = quick_xml_reader(text);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Eof) => return,
            Ok(_) => {}
            Err(e) => panic!("{name} is not well-formed: {e}\n{text}"),
        }
    }
}

fn quick_xml_reader(text: &str) -> quick_xml::Reader<&[u8]> {
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().check_end_names = true;
    reader
}

#[tokio::test]
async fn a_downloaded_book_gets_metadata_and_a_cover_and_nothing_after_the_story() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_fixture(dir.path());
    assert!(
        Book::open(&path)
            .names
            .iter()
            .any(|n| n == "OEBPS/log_page.xhtml"),
        "the fixture has FanFicFare's update log"
    );
    let finisher = EpubFinisher::new().unwrap();
    finisher.finish(&path, &extras()).await.unwrap();

    let mut book = Book::open(&path);
    assert_eq!(book.names.first().map(String::as_str), Some("mimetype"));
    assert_eq!(
        book.archive.by_index(0).unwrap().compression(),
        zip::CompressionMethod::Stored,
        "mimetype must be stored"
    );
    for name in [
        "OEBPS/fanfold_cover.png",
        "OEBPS/fanfold_cover.xhtml",
        "OEBPS/file0001.xhtml",
    ] {
        assert!(
            book.names.iter().any(|n| n == name),
            "{name} in {:?}",
            book.names
        );
    }
    assert!(
        book.bytes("OEBPS/fanfold_cover.png")
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );

    let opf = book.text("content.opf");
    well_formed("content.opf", &opf);
    for expected in [
        "<dc:subject>Rating: Teen And Up Audiences</dc:subject>",
        "<dc:subject>Fandom: Original Work</dc:subject>",
        "<dc:subject>Relationship: Ada Marsh/Ben Ward</dc:subject>",
        "<dc:subject>Tag: Fluff &amp; &lt;Angst&gt;</dc:subject>",
        "<dc:subject>Status: In progress</dc:subject>",
        "<meta name=\"calibre:series\" content=\"The Harbor Saga\"/>",
        "<meta name=\"calibre:series_index\" content=\"2.00\"/>",
        "<meta property=\"belongs-to-collection\" id=\"fanfold-series-1\">The Harbor Saga</meta>",
        "<meta refines=\"#fanfold-series-1\" property=\"group-position\">2</meta>",
        "<meta property=\"ao3:words\">120000</meta>",
        "<meta property=\"ao3:kudos\">1234</meta>",
        "<meta property=\"ao3:first-downloaded\">",
        "<meta name=\"cover\" content=\"fanfold-cover-image\"/>",
        "properties=\"cover-image\"",
        "<dc:date>2026-10-08</dc:date>",
        "ao3: https://github.com/arlllk/Fanfold",
    ] {
        assert!(opf.contains(expected), "{expected}\n{opf}");
    }
    assert!(
        !opf.contains("Last Update"),
        "FanFicFare's subjects are replaced"
    );
    let spine = opf.split("<spine").nth(1).unwrap();
    assert!(spine.find("fanfold-cover").unwrap() < spine.find("title_page").unwrap());
    assert!(!opf.contains("log_page"), "{opf}");
    let last = spine.rsplit("<itemref ").next().unwrap();
    assert!(
        last.starts_with("idref=\"file0001\""),
        "the story ends the book: {spine}"
    );
    assert!(
        !book
            .names
            .iter()
            .any(|n| n.contains("about") || n.contains("log_page")),
        "{:?}",
        book.names
    );
    well_formed("cover page", &book.text("OEBPS/fanfold_cover.xhtml"));
    let nav = book.text("nav.xhtml");
    well_formed("nav", &nav);
    assert!(
        !nav.contains("fanfold") && !nav.contains("log_page"),
        "{nav}"
    );
    let ncx = book.text("toc.ncx");
    well_formed("ncx", &ncx);
    assert!(
        !ncx.contains("fanfold") && !ncx.contains("log_page"),
        "{ncx}"
    );
}

#[tokio::test]
async fn finishing_twice_is_finishing_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_fixture(dir.path());
    let finisher = EpubFinisher::new().unwrap();
    assert!(
        !finisher.is_current(&path).await,
        "a downloaded book is not finished"
    );
    finisher.finish(&path, &extras()).await.unwrap();
    assert!(finisher.is_current(&path).await);
    assert!(!finisher.is_current(&dir.path().join("missing.epub")).await);
    let mut once = Book::open(&path);
    finisher.finish(&path, &extras()).await.unwrap();
    let mut twice = Book::open(&path);
    assert_eq!(once.names, twice.names);
    for name in ["content.opf", "nav.xhtml", "toc.ncx"] {
        assert_eq!(once.text(name), twice.text(name), "{name}");
    }
}

#[tokio::test]
async fn an_epub2_book_gets_only_what_epub2_allows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.epub");
    std::fs::copy(FIXTURE_EPUB2, &path).unwrap();
    let finisher = EpubFinisher::new().unwrap();
    finisher.finish(&path, &extras()).await.unwrap();
    let mut book = Book::open(&path);
    let opf = book.text("content.opf");
    well_formed("content.opf", &opf);
    for expected in [
        "<package version=\"2.0\" xmlns",
        "<dc:date opf:event=\"publication\">",
        "<dc:subject>Fandom: Original Work</dc:subject>",
        "<meta name=\"calibre:series\" content=\"The Harbor Saga\"/>",
        "<meta name=\"ao3:words\" content=\"120000\"/>",
        "<meta name=\"cover\" content=\"fanfold-cover-image\"/>",
        "href=\"OEBPS/fanfold_cover.png\" media-type=\"image/png\"/>",
    ] {
        assert!(opf.contains(expected), "{expected}\n{opf}");
    }
    for absent in [
        "property=",
        "prefix=",
        "belongs-to-collection",
        "properties=",
    ] {
        assert!(!opf.contains(absent), "{absent}\n{opf}");
    }
    assert!(!book.text("toc.ncx").contains("fanfold"));
    assert!(finisher.is_current(&path).await);
    finisher.finish(&path, &extras()).await.unwrap();
    assert_eq!(Book::open(&path).text("content.opf"), opf, "idempotent");
}

#[tokio::test]
async fn a_broken_book_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.epub");
    std::fs::write(&path, b"not a zip").unwrap();
    let finisher = EpubFinisher::new().unwrap();
    assert!(finisher.finish(&path, &extras()).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"not a zip");
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no scratch file left"
    );
}

/// `FANFOLD_PREVIEW=<dir> cargo test -p fanfold-adapter-epub preview` writes a finished book
/// and its cover there, to look at.
#[tokio::test]
async fn preview() {
    let Some(out) = std::env::var_os("FANFOLD_PREVIEW") else {
        return;
    };
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap();
    let path = copy_fixture(&out);
    let mut extras = extras();
    if let Ok(title) = std::env::var("FANFOLD_PREVIEW_TITLE") {
        extras.metadata.facts.title = title;
    }
    EpubFinisher::new()
        .unwrap()
        .finish(&path, &extras)
        .await
        .unwrap();
    let mut book = Book::open(&path);
    std::fs::write(out.join("cover.png"), book.bytes("OEBPS/fanfold_cover.png")).unwrap();
}

/// The package, navigation document and NCX entries and the page an older Fanfold (finishing
/// version 1) added for "About this work", put into a book finished by this one.
fn as_finished_by_version_1(path: &Path) {
    use std::io::Write as _;
    let mut book = Book::open(path);
    let swap = |name: &str, text: String| -> (String, String) { (name.to_owned(), text) };
    let opf = book
        .text("content.opf")
        .replace("content=\"2\"/>", "content=\"1\"/>")
        .replace(
            "</manifest>",
            "\t<item id=\"fanfold-about\" href=\"OEBPS/fanfold_about.xhtml\" media-type=\"application/xhtml+xml\"/>\n\t</manifest>",
        )
        .replace(
            "</spine>",
            "\t<itemref idref=\"fanfold-about\" linear=\"yes\"/>\n\t</spine>",
        );
    let nav = book.text("nav.xhtml").replacen(
        "</ol>",
        "<!--fanfold--><li><a href=\"OEBPS/fanfold_about.xhtml\">About this work</a></li><!--/fanfold-->\n</ol>",
        1,
    );
    let ncx = book.text("toc.ncx").replace(
        "</navMap>",
        "<!--fanfold--><navPoint id=\"fanfold-about\" playOrder=\"9\"><navLabel><text>About this work</text></navLabel><content src=\"OEBPS/fanfold_about.xhtml\"/></navPoint><!--/fanfold-->\n</navMap>",
    );
    assert!(
        opf.contains("fanfold-about") && opf.contains("content=\"1\"/>"),
        "{opf}"
    );
    let swapped = [
        swap("content.opf", opf),
        swap("nav.xhtml", nav),
        swap("toc.ncx", ncx),
        swap("OEBPS/fanfold_about.xhtml", "<html/>".to_owned()),
    ];
    let out = path.with_extension("v1");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
    for index in 0..book.archive.len() {
        let entry = book.archive.by_index_raw(index).unwrap();
        let name = entry.name().unwrap().into_owned();
        if swapped.iter().any(|(n, _)| *n == name) {
            continue;
        }
        writer.raw_copy_file(entry).unwrap();
    }
    for (name, text) in &swapped {
        writer
            .start_file(name.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
    std::fs::rename(out, path).unwrap();
}

#[tokio::test]
async fn a_book_finished_with_an_about_page_loses_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_fixture(dir.path());
    let finisher = EpubFinisher::new().unwrap();
    finisher.finish(&path, &extras()).await.unwrap();
    let mut clean = Book::open(&path);
    as_finished_by_version_1(&path);
    assert!(
        !finisher.is_current(&path).await,
        "a book with the about page is finished again"
    );

    finisher.finish(&path, &extras()).await.unwrap();
    let mut book = Book::open(&path);
    assert!(finisher.is_current(&path).await);
    assert!(
        !book.names.iter().any(|n| n.contains("about")),
        "{:?}",
        book.names
    );
    for name in ["content.opf", "nav.xhtml", "toc.ncx"] {
        let text = book.text(name);
        assert!(!text.contains("about"), "{name}: {text}");
        assert_eq!(text, clean.text(name), "{name}");
    }
}
