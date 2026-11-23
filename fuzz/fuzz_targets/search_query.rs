//! Any text typed into the library's search box: parsing never panics, keeps within its
//! limits, and the canonical form reads back as the same query (chips and filter links edit
//! queries as text, so they depend on it).
#![no_main]

use fanfold_app::search::{MAX_TOKENS, SearchQuery};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let query = SearchQuery::parse(text);
    assert!(query.tokens.len() <= MAX_TOKENS);
    let rendered = query.render();
    assert_eq!(SearchQuery::parse(&rendered), query, "{rendered:?}");
    let _ = query.partial();
});
