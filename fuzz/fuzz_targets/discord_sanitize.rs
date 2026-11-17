//! Any text from the site: sanitising never panics and never lets a mention or
//! control character through.
#![no_main]

use fanfold_adapter_discord::sanitize;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let clean = sanitize(&String::from_utf8_lossy(data), 300);
    assert!(!clean.contains("@everyone") && !clean.contains("@here"));
    assert!(!clean.chars().any(|c| c.is_control() && c != '\n'));
});
