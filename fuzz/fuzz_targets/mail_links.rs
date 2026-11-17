//! Any email body: extraction terminates, yields distinct ids, and each id's
//! canonical URL parses back.
#![no_main]

use std::collections::HashSet;

use fanfold_domain::{mail::extract_work_ids, work::parse_work_url};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let body = String::from_utf8_lossy(data);
    let ids = extract_work_ids(&body);
    let distinct: HashSet<_> = ids.iter().collect();
    assert_eq!(distinct.len(), ids.len(), "duplicate ids");
    for id in ids {
        assert_eq!(parse_work_url(&id.canonical_url()), Ok(id));
    }
});
