//! `jmap-client` emails to the application's [`MailMessage`]. Pure.

use fanfold_app::ports::MailMessage;
use jmap_client::email::{Email, EmailBodyPart};

/// `Name <address>`, so the sender filter can match on either half: relays rewrite the
/// address (`do-not-reply_at_archiveofourown_org_x`) but leave the name alone.
fn sender(email: &Email) -> String {
    let Some(addr) = email.from().and_then(<[_]>::first) else {
        return String::new();
    };
    match addr.name().map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("{name} <{}>", addr.email()),
        None => addr.email().to_owned(),
    }
}

fn body(email: &Email, parts: Option<&[EmailBodyPart]>) -> String {
    parts
        .into_iter()
        .flatten()
        .filter_map(|part| part.part_id())
        .filter_map(|id| email.body_value(id))
        .map(jmap_client::email::EmailBodyValue::value)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The message, if it has an id and sits in `inbox`.
#[must_use]
pub fn to_message(email: &Email, inbox: &str) -> Option<MailMessage> {
    let id = email.id().filter(|id| !id.is_empty())?;
    if !email.mailbox_ids().contains(&inbox) {
        return None;
    }
    Some(MailMessage {
        id: id.to_owned(),
        from: sender(email),
        subject: email.subject().unwrap_or_default().to_owned(),
        text: body(email, email.text_body()),
        html: body(email, email.html_body()),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn email(value: &Value) -> Email {
        serde_json::from_value(value.clone()).unwrap()
    }

    fn full(id: &str, mailbox: &str, from: &Value) -> Value {
        json!({
            "id": id,
            "from": from,
            "subject": "[AO3] someone posted Chapter 2 of A Story",
            "mailboxIds": { mailbox: true },
            "textBody": [{ "partId": "1" }],
            "htmlBody": [{ "partId": "2" }, { "partId": "3" }],
            "bodyValues": {
                "1": { "value": "text https://archiveofourown.org/works/5" },
                "2": { "value": "<p>one</p>" },
                "3": { "value": "<p>two</p>" },
            },
        })
    }

    #[test]
    fn maps_a_real_shaped_email() {
        let from = json!([{ "name": "Archive of Our Own - do-not-reply at archiveofourown.org", "email": "do-not-reply_at_archiveofourown_org_abcd@relay.example" }]);
        let m = to_message(&email(&full("m1", "inbox1", &from)), "inbox1").unwrap();
        assert_eq!(m.id, "m1");
        assert_eq!(
            m.from,
            "Archive of Our Own - do-not-reply at archiveofourown.org <do-not-reply_at_archiveofourown_org_abcd@relay.example>"
        );
        assert!(m.subject.starts_with("[AO3]"));
        assert_eq!(m.text, "text https://archiveofourown.org/works/5");
        assert_eq!(m.html, "<p>one</p>\n<p>two</p>", "parts are concatenated");
    }

    #[test]
    fn only_inbox_mail_is_returned() {
        let from = json!([{ "email": "a@b" }]);
        assert!(to_message(&email(&full("m1", "sent1", &from)), "inbox1").is_none());
        let multi = json!({ "id": "m2", "mailboxIds": { "x": true, "inbox1": true } });
        assert!(
            to_message(&email(&multi), "inbox1").is_some(),
            "in several mailboxes, one of them the inbox"
        );
    }

    #[test]
    fn senders_in_every_shape() {
        let sender_of = |from: Value| {
            let v = json!({ "id": "m", "mailboxIds": { "i": true }, "from": from });
            to_message(&email(&v), "i").unwrap().from
        };
        assert_eq!(
            sender_of(json!([{ "name": "N", "email": "a@b" }])),
            "N <a@b>"
        );
        assert_eq!(
            sender_of(json!([{ "name": "  ", "email": "a@b" }])),
            "a@b",
            "blank names are dropped"
        );
        assert_eq!(sender_of(json!([{ "email": "a@b" }])), "a@b");
        assert_eq!(sender_of(json!(null)), "");
        assert_eq!(sender_of(json!([])), "");
    }

    #[test]
    fn emails_missing_pieces_still_map() {
        let m = to_message(
            &email(&json!({ "id": "m", "mailboxIds": { "i": true } })),
            "i",
        )
        .unwrap();
        assert_eq!(
            (
                m.from.as_str(),
                m.subject.as_str(),
                m.text.as_str(),
                m.html.as_str()
            ),
            ("", "", "", "")
        );
        assert!(
            to_message(&email(&json!({ "mailboxIds": { "i": true } })), "i").is_none(),
            "no id"
        );
        assert!(
            to_message(
                &email(&json!({ "id": "", "mailboxIds": { "i": true } })),
                "i"
            )
            .is_none(),
            "empty id"
        );
    }

    #[test]
    fn a_part_without_a_value_contributes_nothing() {
        let v = json!({ "id": "m", "mailboxIds": { "i": true }, "textBody": [{ "partId": "1" }, { "partId": "gone" }], "bodyValues": { "1": { "value": "kept" } } });
        assert_eq!(to_message(&email(&v), "i").unwrap().text, "kept");
    }
}
