//! Any title, byline, series name and part: every derived library path is publishable by
//! the filesystem adapter (it checks `is_valid_library_path` before any IO).
#![no_main]

use fanfold_domain::{
    library::{is_valid_library_path, series_book_path, series_folder, single_book_path},
    work::{SeriesId, WorkId},
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (id_bytes, rest) = data.split_at(data.len().min(8));
    let mut raw = [0u8; 8];
    for (slot, byte) in raw.iter_mut().zip(id_bytes) {
        *slot = *byte;
    }
    let id = u64::from_le_bytes(raw) >> 1;
    let (Some(work), Some(series)) = (WorkId::new(id), SeriesId::new(id)) else {
        return;
    };
    let text = String::from_utf8_lossy(rest);
    let mut parts = text.splitn(3, '\u{1}');
    let (title, byline, name) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let part = rest.first().map(|b| u32::from(*b) * 37);
    let single = single_book_path(byline, title, work);
    assert!(is_valid_library_path(&single), "{single:?}");
    let in_series = series_book_path(&series_folder(name, series), part, title, work);
    assert!(is_valid_library_path(&in_series), "{in_series:?}");
});
