//! [`LibraryStore`] backed by a directory.
//!
//! Books are published with write-to-temp-then-rename, so a file-syncing reader
//! (Syncthing) never sees a half-written EPUB. The temp file lives in `<root>/.tmp`
//! on the same filesystem, which is what makes the rename atomic; add `.tmp` to the
//! folder's `.stignore` so staging files are not synced.
//!
//! Library paths are relative (`series/<folder>/<file>`, `single/<byline>/<file>`, see
//! `fanfold_domain::library`); their folders are created on first publish.

use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use fanfold_app::{error::StoreError, ports::LibraryStore};
use fanfold_domain::library::is_valid_library_path;

const STAGING_DIR: &str = ".tmp";

#[derive(Debug, Clone)]
pub struct FsLibrary {
    root: PathBuf,
}

impl FsLibrary {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    fn publish_blocking(root: &Path, source: &Path, file_name: &str) -> io::Result<()> {
        let staging = root.join(STAGING_DIR);
        fs::create_dir_all(&staging)?;
        let mut staged = tempfile::Builder::new()
            .prefix("publish-")
            .suffix(".part")
            .tempfile_in(&staging)?;
        io::copy(&mut File::open(source)?, &mut staged)?;
        staged.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Tempfiles are 0600; the reader/syncer may run as another user.
            staged
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        let target = root.join(file_name);
        if let Some(folder) = target.parent() {
            fs::create_dir_all(folder)?;
        }
        staged.persist(target).map_err(|e| e.error)?;
        if let Err(e) = fs::remove_file(source) {
            tracing::debug!(error = %e, source = %source.display(), "could not remove published source");
        }
        Ok(())
    }
}

#[async_trait]
impl LibraryStore for FsLibrary {
    fn path_of(&self, file_name: &str) -> PathBuf {
        self.root.join(file_name)
    }

    async fn exists(&self, file_name: &str) -> Result<bool, StoreError> {
        if !is_valid_library_path(file_name) {
            return Err(StoreError::InvalidName(file_name.to_owned()));
        }
        tokio::fs::try_exists(self.root.join(file_name))
            .await
            .map_err(|e| StoreError::Io(e.to_string()))
    }

    async fn publish(&self, source: &Path, file_name: &str) -> Result<(), StoreError> {
        if !is_valid_library_path(file_name) {
            return Err(StoreError::InvalidName(file_name.to_owned()));
        }
        let (root, source, file_name) = (
            self.root.clone(),
            source.to_path_buf(),
            file_name.to_owned(),
        );
        tokio::task::spawn_blocking(move || Self::publish_blocking(&root, &source, &file_name))
            .await
            .map_err(|e| StoreError::Io(format!("publish task failed: {e}")))?
            .map_err(|e| StoreError::Io(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use fanfold_domain::{
        library::{series_book_path, series_folder, single_book_path},
        work::{SeriesId, WorkId},
    };
    use proptest::prelude::*;

    use super::*;

    fn library() -> (tempfile::TempDir, FsLibrary) {
        let dir = tempfile::tempdir().unwrap();
        let lib = FsLibrary::new(dir.path().join("library"));
        (dir, lib)
    }

    fn source(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn publishes_a_new_book_and_consumes_the_source() {
        let (dir, lib) = library();
        let src = source(dir.path(), "story.epub", "v1");
        lib.publish(&src, "Title [1].epub").await.unwrap();
        assert_eq!(
            fs::read_to_string(lib.path_of("Title [1].epub")).unwrap(),
            "v1"
        );
        assert!(!src.exists(), "source is consumed");
        assert!(lib.exists("Title [1].epub").await.unwrap());
        assert!(!lib.exists("Other [2].epub").await.unwrap());
    }

    #[tokio::test]
    async fn replaces_in_place_and_leaves_no_staging_files() {
        let (dir, lib) = library();
        lib.publish(&source(dir.path(), "a.epub", "v1"), "T [1].epub")
            .await
            .unwrap();
        lib.publish(&source(dir.path(), "b.epub", "version two"), "T [1].epub")
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(lib.path_of("T [1].epub")).unwrap(),
            "version two"
        );
        let leftovers: Vec<_> = fs::read_dir(lib.staging_dir()).unwrap().collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let books: Vec<_> = fs::read_dir(dir.path().join("library"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != STAGING_DIR)
            .collect();
        assert_eq!(books.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn published_files_are_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, lib) = library();
        lib.publish(&source(dir.path(), "a.epub", "x"), "T [1].epub")
            .await
            .unwrap();
        let mode = fs::metadata(lib.path_of("T [1].epub"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[tokio::test]
    async fn hostile_names_are_rejected_before_touching_the_disk() {
        let (dir, lib) = library();
        let src = source(dir.path(), "a.epub", "x");
        for name in [
            "../escape.epub",
            "single/../../escape.epub",
            "a/b/c/d.epub",
            "/abs.epub",
            "single/.hidden/x.epub",
            "..\\x.epub",
            ".hidden.epub",
            "",
            "no-extension",
            "nul\0.epub",
            "x.epub\n",
        ] {
            assert!(
                matches!(
                    lib.publish(&src, name).await,
                    Err(StoreError::InvalidName(_))
                ),
                "{name:?}"
            );
            assert!(
                matches!(lib.exists(name).await, Err(StoreError::InvalidName(_))),
                "{name:?}"
            );
        }
        assert!(
            src.exists(),
            "a rejected publish must not consume the source"
        );
        assert!(!dir.path().join("escape.epub").exists());
    }

    #[tokio::test]
    async fn a_missing_source_is_an_error_and_leaves_the_library_untouched() {
        let (dir, lib) = library();
        lib.publish(&source(dir.path(), "a.epub", "keep me"), "T [1].epub")
            .await
            .unwrap();
        let result = lib
            .publish(&dir.path().join("missing.epub"), "T [1].epub")
            .await;
        assert!(matches!(result, Err(StoreError::Io(_))));
        assert_eq!(
            fs::read_to_string(lib.path_of("T [1].epub")).unwrap(),
            "keep me"
        );
    }

    #[tokio::test]
    async fn books_are_filed_into_their_folders_which_are_created_on_demand() {
        let (dir, lib) = library();
        let folder = series_folder("The Harbor Saga", SeriesId::new(2_000_001).unwrap());
        let in_series = series_book_path(
            &folder,
            Some(2),
            "The Logbook",
            WorkId::new(10_000_004).unwrap(),
        );
        let single = single_book_path("quillandink", "Solo", WorkId::new(7).unwrap());
        lib.publish(&source(dir.path(), "a.epub", "series book"), &in_series)
            .await
            .unwrap();
        lib.publish(&source(dir.path(), "b.epub", "single book"), &single)
            .await
            .unwrap();
        let root = dir.path().join("library");
        assert_eq!(
            fs::read_to_string(
                root.join("series/The Harbor Saga 2000001/02 The Logbook 10000004.epub")
            )
            .unwrap(),
            "series book"
        );
        assert_eq!(
            fs::read_to_string(root.join("single/quillandink/Solo 7.epub")).unwrap(),
            "single book"
        );
        assert!(lib.exists(&in_series).await.unwrap());
        let leftovers: Vec<_> = fs::read_dir(lib.staging_dir()).unwrap().collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    proptest! {
        /// The domain's path generator and the adapter's validator must agree: whatever a
        /// site hands us, the derived paths are publishable.
        #[test]
        fn every_derived_path_is_publishable(
            title in "\\PC*", byline in "\\PC*", name in "\\PC*",
            id in 1u64..=i64::MAX as u64, part in proptest::option::of(0u32..1000),
        ) {
            let work = WorkId::new(id).unwrap();
            let single = single_book_path(&byline, &title, work);
            prop_assert!(is_valid_library_path(&single), "{single:?}");
            let folder = series_folder(&name, SeriesId::new(id).unwrap());
            let in_series = series_book_path(&folder, part, &title, work);
            prop_assert!(is_valid_library_path(&in_series), "{in_series:?}");
        }

        #[test]
        fn traversal_is_never_valid(prefix in "\\PC{0,20}", suffix in "\\PC{0,20}") {
            let up = format!("{prefix}/../{suffix}.epub");
            prop_assert!(!is_valid_library_path(&up));
            let back = format!("{prefix}\\{suffix}.epub");
            prop_assert!(!is_valid_library_path(&back));
        }
    }
}
