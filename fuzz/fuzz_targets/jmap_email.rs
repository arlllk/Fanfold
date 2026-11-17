//! Any JSON a (buggy or hostile) mail server could return for an email: reading it never
//! panics, mail outside the inbox is never returned, and a returned message has an id.
#![no_main]

use fanfold_adapter_jmap::convert::to_message;
use jmap_client::email::Email;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(email) = serde_json::from_slice::<Email>(data) else {
        return;
    };
    if let Some(message) = to_message(&email, "inbox") {
        assert!(!message.id.is_empty(), "a message always has an id");
        assert!(
            email.mailbox_ids().contains(&"inbox"),
            "only inbox mail is returned"
        );
    }
    if !email.mailbox_ids().contains(&"inbox") {
        assert!(to_message(&email, "inbox").is_none());
    }
});
