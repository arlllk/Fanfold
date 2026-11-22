//! [`BookFinisher`] for EPUB: after each download, Fanfold writes what it knows about the
//! work into the book.
//!
//! * The package's subjects become labelled tags (`Rating: Explicit`, `Fandom: …`), so a
//!   reader's tag list groups them; every series is named, in calibre's form (the one most
//!   e-readers group by) and in EPUB 3's; every AO3 fact is an `ao3:` property
//!   (`docs/epub-metadata.md`).
//! * A generated cover, first in the book.
//!
//! Nothing follows the story, so an update never moves the reader's place: FanFicFare's
//! update log and the "About this work" page an older Fanfold added are taken out, with their
//! table-of-contents entries.
//! The downloader rebuilds the book on every update and drops all of this (it keeps only its
//! own chapter files), so it is added after every download; a second pass over a finished
//! book replaces the first one's additions.

use std::{
    io::{Read, Write as _},
    path::Path,
    sync::Arc,
};

use async_trait::async_trait;
use fanfold_app::ports::{BookExtras, BookFinisher, FinishError};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub mod book;
pub mod cover;
pub mod nav;
pub mod opf;
pub mod pages;
pub mod text;

use book::{COVER_IMAGE, COVER_PAGE, RETIRED_FILES};

/// Finishes EPUBs in place.
#[derive(Debug, Clone)]
pub struct EpubFinisher {
    renderer: Arc<cover::Renderer>,
}

impl EpubFinisher {
    /// Loads the built-in fonts.
    pub fn new() -> Result<Self, FinishError> {
        Ok(Self {
            renderer: Arc::new(cover::Renderer::new().map_err(FinishError)?),
        })
    }
}

#[async_trait]
impl BookFinisher for EpubFinisher {
    async fn finish(&self, book: &Path, extras: &BookExtras) -> Result<(), FinishError> {
        let renderer = Arc::clone(&self.renderer);
        let book = book.to_path_buf();
        let extras = extras.clone();
        tokio::task::spawn_blocking(move || finish(&renderer, &book, &extras))
            .await
            .map_err(|e| FinishError(format!("finishing task failed: {e}")))?
            .map_err(FinishError)
    }

    async fn is_current(&self, book: &Path) -> bool {
        let book = book.to_path_buf();
        tokio::task::spawn_blocking(move || is_current(&book))
            .await
            .unwrap_or(false)
    }
}

fn read_entry<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, String> {
    let mut entry = archive.by_name(name).map_err(|e| format!("{name}: {e}"))?;
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|e| format!("{name}: {e}"))?;
    Ok(text)
}

/// Whether the book's package carries this version's marker.
fn is_current(path: &Path) -> bool {
    let read = || -> Result<bool, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;
        let container = read_entry(&mut archive, "META-INF/container.xml")?;
        let opf_path = package_path(&container).ok_or_else(String::new)?;
        let opf = read_entry(&mut archive, &opf_path)?;
        Ok(opf.contains(&book::finish_marker()))
    };
    read().unwrap_or(false)
}

/// The package document's path, from `META-INF/container.xml`.
fn package_path(container: &str) -> Option<String> {
    let at = container.find("full-path=\"")? + "full-path=\"".len();
    let rest = container.get(at..)?;
    Some(rest.get(..rest.find('"')?)?.to_owned())
}

/// The book's language for the added pages: the package's `dc:language`, else English.
fn language(opf: &str) -> String {
    opf.find("<dc:language>")
        .and_then(|at| {
            let rest = opf.get(at + "<dc:language>".len()..)?;
            rest.get(..rest.find('<')?)
        })
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or("en")
        .to_owned()
}

/// Rewrite `path`. Everything is built in a new file next to it, which then replaces it, so
/// on any error the book is untouched.
fn finish(renderer: &cover::Renderer, path: &Path, extras: &BookExtras) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("not an EPUB: {e}"))?;
    let container = read_entry(&mut archive, "META-INF/container.xml")?;
    let opf_path =
        package_path(&container).ok_or_else(|| "no package in container.xml".to_owned())?;
    let opf = read_entry(&mut archive, &opf_path)?;
    let base = opf_path
        .rfind('/')
        .and_then(|i| opf_path.get(..=i))
        .unwrap_or_default()
        .to_owned();
    let lang = language(&opf);

    let items = opf::manifest(&opf)?;
    let mut replaced: Vec<(String, Vec<u8>)> = Vec::new();
    let epub3 = opf::version(&opf).is_some_and(|v| v.starts_with('3'));
    replaced.push((
        opf_path.clone(),
        opf::rewrite(&opf, &book::additions(extras, epub3))?.into_bytes(),
    ));
    if let Some(nav) = items.iter().find(|i| {
        i.properties.split_whitespace().any(|p| p == "nav") && !i.id.starts_with(opf::ID_PREFIX)
    }) {
        let name = format!("{base}{}", nav.href);
        let text = read_entry(&mut archive, &name)?;
        if let Some(text) = nav::clean_nav(&text) {
            replaced.push((name, text.into_bytes()));
        }
    }
    if let Some(ncx) = items
        .iter()
        .find(|i| i.media_type == "application/x-dtbncx+xml")
    {
        let name = format!("{base}{}", ncx.href);
        let text = read_entry(&mut archive, &name)?;
        if let Some(text) = nav::clean_ncx(&text) {
            replaced.push((name, text.into_bytes()));
        }
    }
    let title = &extras.metadata.facts.title;
    // (name, content, compress): the PNG is compressed already.
    let added: Vec<(String, Vec<u8>, bool)> = vec![
        (
            format!("{base}{COVER_IMAGE}"),
            renderer.render(extras)?,
            false,
        ),
        (
            format!("{base}{COVER_PAGE}"),
            pages::cover_page(title, &lang).into_bytes(),
            true,
        ),
    ];
    // Fanfold's old page, and FanFicFare's update log wherever its manifest puts it.
    let retired: Vec<String> = RETIRED_FILES
        .iter()
        .map(|f| format!("{base}{f}"))
        .chain(
            items
                .iter()
                .filter(|i| opf::is_fanficfare_log(&i.id))
                .map(|i| format!("{base}{}", i.href)),
        )
        .collect();
    write_book(&mut archive, path, &replaced, &added, &retired)
}

/// The book with `replaced` entries swapped, `retired` ones dropped and `added` ones appended,
/// written next to `path` and then moved over it.
fn write_book(
    archive: &mut ZipArchive<std::fs::File>,
    path: &Path,
    replaced: &[(String, Vec<u8>)],
    added: &[(String, Vec<u8>, bool)],
    retired: &[String],
) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let out = tempfile::Builder::new()
        .prefix(".finishing-")
        .suffix(".epub")
        .tempfile_in(dir)
        .map_err(|e| format!("scratch file: {e}"))?;
    let mut writer = ZipWriter::new(out.as_file());
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|e| format!("entry {index}: {e}"))?;
        let name = entry
            .name()
            .map_err(|e| format!("entry {index}: {e}"))?
            .into_owned();
        if added.iter().any(|(n, _, _)| *n == name) || retired.contains(&name) {
            continue;
        }
        if let Some((_, data)) = replaced.iter().find(|(n, _)| *n == name) {
            drop(entry);
            writer
                .start_file(name.as_str(), deflated)
                .and_then(|()| writer.write_all(data).map_err(Into::into))
                .map_err(|e| format!("{name}: {e}"))?;
        } else {
            // Unchanged entries are copied as stored, which keeps `mimetype` first and
            // uncompressed as EPUB requires.
            writer
                .raw_copy_file(entry)
                .map_err(|e| format!("{name}: {e}"))?;
        }
    }
    for (name, data, compress) in added {
        let options = if *compress {
            deflated
        } else {
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
        };
        writer
            .start_file(name.as_str(), options)
            .and_then(|()| writer.write_all(data).map_err(Into::into))
            .map_err(|e| format!("{name}: {e}"))?;
    }
    writer.finish().map_err(|e| format!("write: {e}"))?;
    // The scratch file is private; the book keeps the permissions it had.
    if let Ok(meta) = std::fs::metadata(path) {
        out.as_file()
            .set_permissions(meta.permissions())
            .map_err(|e| format!("permissions: {e}"))?;
    }
    out.as_file()
        .sync_all()
        .map_err(|e| format!("write: {e}"))?;
    out.persist(path)
        .map_err(|e| format!("replace the book: {}", e.error))?;
    Ok(())
}
