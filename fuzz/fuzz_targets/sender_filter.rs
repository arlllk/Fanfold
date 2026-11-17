//! Any sender and allow-list entry: matching never panics, an empty allow-list admits
//! everyone, more entries never turn an allowed sender away, and a sender always matches
//! an allow-list containing itself.
#![no_main]

use fanfold_app::ingest::sender_allowed;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (from, entry) = text.split_once('\n').unwrap_or((&text, "x"));
    let allowed = vec![entry.to_owned()];
    let verdict = sender_allowed(from, &allowed);

    assert!(
        sender_allowed(from, &[]),
        "an empty allow-list admits everyone"
    );
    let mut more = allowed;
    more.push("zzz".to_owned());
    if verdict {
        assert!(
            sender_allowed(from, &more),
            "adding entries cannot reject an allowed sender"
        );
    }
    if from.chars().any(char::is_alphanumeric) {
        assert!(
            sender_allowed(from, &[from.to_owned()]),
            "a sender matches itself"
        );
    }
});
