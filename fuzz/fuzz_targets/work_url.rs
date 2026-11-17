//! Any string: parsing never panics, and an accepted URL names a work whose
//! canonical URL parses back to the same id.
#![no_main]

use fanfold_domain::work::parse_work_url;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    if let Ok(id) = parse_work_url(&input) {
        assert_eq!(parse_work_url(&id.canonical_url()), Ok(id));
        assert!(id.get() > 0 && i64::try_from(id.get()).is_ok());
    }
});
