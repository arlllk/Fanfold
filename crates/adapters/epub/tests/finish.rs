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
async fn a_downloaded_book_gets_metadata_a_cover_and_an_about_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_fixture(dir.path());
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
        "OEBPS/fanfold_about.xhtml",
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
    assert!(spine.find("file0001").unwrap() < spine.find("fanfold-about").unwrap());

    let about = book.text("OEBPS/fanfold_about.xhtml");
    well_formed("about", &about);
    for expected in [
        "About this work",
        "The Keeper and the Logbook",
        "Part 2 of <a href=\"https://archiveofourown.org/series/2000001\">The Harbor Saga</a>",
        "2. The Logbook <span class=\"ff-date\">· 9 Oct 2026</span> <b>· new</b>",
        "Added tag: Fluff &amp; &lt;Angst&gt;.",
        "Words 60,000 → 120,000 (+60,000).",
        "First downloaded: 1/? chapters, 60,000 words.",
        "Kudos 1,234",
    ] {
        assert!(about.contains(expected), "{expected}\n{about}");
    }
    well_formed("cover page", &book.text("OEBPS/fanfold_cover.xhtml"));
    let nav = book.text("nav.xhtml");
    well_formed("nav", &nav);
    assert!(nav.contains("<a href=\"OEBPS/fanfold_about.xhtml\">About this work</a>"));
    let ncx = book.text("toc.ncx");
    well_formed("ncx", &ncx);
    assert!(ncx.contains("<content src=\"OEBPS/fanfold_about.xhtml\"/>"));
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
    for name in [
        "content.opf",
        "nav.xhtml",
        "toc.ncx",
        "OEBPS/fanfold_about.xhtml",
    ] {
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
    assert!(book.text("toc.ncx").contains("fanfold_about.xhtml"));
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
