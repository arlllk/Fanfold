//! Any form body or query string: the selection parser never panics and only ever
//! yields positive, distinct ids, at most `MAX_IDS` of them.
#![no_main]

use std::collections::HashSet;

use fanfold_adapter_web::forms::{MAX_IDS, job_ids, pairs};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let ids = job_ids(&pairs(data), "id");
    assert!(ids.len() <= MAX_IDS);
    assert!(ids.iter().all(|id| id.get() > 0));
    let distinct: HashSet<_> = ids.iter().collect();
    assert_eq!(distinct.len(), ids.len(), "duplicate ids");
});
