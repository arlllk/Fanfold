//! Any title, any id: the derived file name is always publishable by the
//! filesystem adapter. This ties two crates' invariants together.
#![no_main]

use fanfold_adapter_fs::is_valid_file_name;
use fanfold_domain::work::{WorkId, book_file_name};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (id_bytes, title_bytes) = data.split_at(data.len().min(8));
    let mut raw = [0u8; 8];
    for (slot, byte) in raw.iter_mut().zip(id_bytes) {
        *slot = *byte;
    }
    let Some(work) = WorkId::new(u64::from_le_bytes(raw) >> 1) else {
        return;
    };
    let name = book_file_name(&String::from_utf8_lossy(title_bytes), work);
    assert!(is_valid_file_name(&name), "{name:?}");
});
