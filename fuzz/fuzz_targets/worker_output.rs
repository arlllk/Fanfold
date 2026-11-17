//! Any bytes a (buggy or compromised) worker could print: parsing never panics,
//! and an accepted book never points outside its scratch directory.
#![no_main]

use std::path::Path;

use fanfold_adapter_fanficfare::protocol::{into_result, parse_response, path_within};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let stdout = String::from_utf8_lossy(data);
    let workdir = Path::new("/scratch/work-1");
    if let Some(response) = parse_response(&stdout)
        && let Ok(book) = into_result(response, workdir)
    {
        assert!(path_within(&book.path, workdir), "{}", book.path.display());
    }
});
