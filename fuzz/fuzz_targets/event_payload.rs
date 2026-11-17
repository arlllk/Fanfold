//! Any NOTIFY payload: decoding never panics, and anything accepted survives a
//! round trip through the canonical encoding.
#![no_main]

use fanfold_adapter_postgres::events::parse_payload;
use fanfold_app::ports::Event;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(event) = parse_payload(&String::from_utf8_lossy(data)) else {
        return;
    };
    let canonical = match event {
        Event::JobChanged(id) => format!("j:{id}"),
        Event::LogAppended(id) => format!("l:{id}"),
        Event::Resync => return,
    };
    assert_eq!(parse_payload(&canonical), Some(event));
});
