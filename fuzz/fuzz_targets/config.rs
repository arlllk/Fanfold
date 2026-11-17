//! Any config text: parsing and validation never panic, and an accepted config
//! satisfies the invariants the rest of the program relies on.
#![no_main]

use fanfold_server::config::{Config, parse_duration};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = parse_duration(&text);
    if let Ok(config) = text.parse::<Config>() {
        assert!(config.retry.max_attempts >= 1);
        assert!(config.retry.cap.0 >= config.retry.base.0);
        assert!(config.breaker.failure_threshold >= 1);
        assert!(config.library.path.is_absolute());
        assert!((1..=16).contains(&config.worker.concurrency));
    }
});
