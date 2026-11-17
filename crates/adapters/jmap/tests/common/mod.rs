//! A fake JMAP server that is as strict as the real one where it matters:
//! it checks Basic auth, returns only the properties that were asked for (the very
//! leniency that once hid a missing `bodyValues`), resolves back-references, expires old
//! states, caps `Email/changes`, and serves a controllable event stream.

#![allow(
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]

use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

pub mod proxy;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
    task::JoinHandle,
};
use url::Url;

pub const ACCOUNT: &str = "acc1";
pub const INBOX: &str = "mb-inbox";
pub const SENT: &str = "mb-sent";

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct MailSpec {
    pub mailbox: String,
    pub received_at: String,
    pub from_name: Option<String>,
    pub from_email: String,
    pub subject: String,
    pub text: String,
    pub html: String,
}

impl MailSpec {
    /// An AO3 chapter notification as the real relay delivers it: the display name keeps
    /// the real domain, the address is rewritten.
    pub fn ao3_chapter(work: u64, chapter: u64, received_at: &str) -> Self {
        Self {
            mailbox: INBOX.to_owned(),
            received_at: received_at.to_owned(),
            from_name: Some("Archive of Our Own - do-not-reply at archiveofourown.org".to_owned()),
            from_email: "do-not-reply_at_archiveofourown_org_abcd@relay.example".to_owned(),
            subject: format!("[AO3] Someone posted Chapter {chapter} of A Story"),
            text: format!(
                "New chapter!\nhttps://archiveofourown.org/works/{work}/chapters/{}\nhttps://archiveofourown.org/works/{work}\nhttps://archiveofourown.org/users/Someone/pseuds/Someone\nhttps://archiveofourown.org/donate",
                chapter * 10
            ),
            html: format!(
                "<p><a href=\"https://archiveofourown.org/works/{work}/chapters/{}\">chapter</a></p>",
                chapter * 10
            ),
        }
    }

    pub fn other(subject: &str, received_at: &str) -> Self {
        Self {
            mailbox: INBOX.to_owned(),
            received_at: received_at.to_owned(),
            from_name: None,
            from_email: "no-reply@mail.proton.me".to_owned(),
            subject: subject.to_owned(),
            text: "Confirm: https://proton.example/confirm/1".to_owned(),
            html: String::new(),
        }
    }

    pub fn in_mailbox(mut self, mailbox: &str) -> Self {
        mailbox.clone_into(&mut self.mailbox);
        self
    }
}

#[derive(Debug, Clone)]
struct Mail {
    id: String,
    spec: MailSpec,
    created_state: u64,
}

#[derive(Debug)]
struct Server {
    mails: Vec<Mail>,
    state: u64,
    oldest_valid: u64,
    requests: Vec<Recorded>,
    expect_auth: String,
    base: String,
    fail_api_with: Option<u16>,
    /// Answer errors as RFC 7807/9457 `application/problem+json` documents, as Stalwart can.
    problem_json: bool,
    /// Make one method fail with a JMAP method-level error: (method name, error type).
    method_error: Option<(String, String)>,
}

#[derive(Debug, Clone, Copy)]
enum Signal {
    EmailState,
    OtherState,
    Close,
}

pub struct FakeJmap {
    pub base: Url,
    state: Arc<Mutex<Server>>,
    signals: broadcast::Sender<Signal>,
    connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for FakeJmap {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl FakeJmap {
    pub async fn start(user: &str, password: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: Url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let state = Arc::new(Mutex::new(Server {
            mails: Vec::new(),
            state: 1,
            oldest_valid: 0,
            requests: Vec::new(),
            expect_auth: format!("Basic {}", base64(format!("{user}:{password}").as_bytes())),
            base: base.as_str().trim_end_matches('/').to_owned(),
            fail_api_with: None,
            problem_json: false,
            method_error: None,
        }));
        let (signals, _) = broadcast::channel(16);
        let connections = Arc::new(AtomicUsize::new(0));
        let task = {
            let (state, signals, connections) = (
                Arc::clone(&state),
                signals.clone(),
                Arc::clone(&connections),
            );
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(handle(
                        stream,
                        Arc::clone(&state),
                        signals.clone(),
                        Arc::clone(&connections),
                    ));
                }
            })
        };
        Self {
            base,
            state,
            signals,
            connections,
            task,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Server> {
        self.state.lock().unwrap()
    }

    /// Deliver a message; returns its id.
    pub fn deliver(&self, spec: MailSpec) -> String {
        let mut s = self.lock();
        s.state += 1;
        let id = format!("m{}", s.mails.len() + 1);
        let created_state = s.state;
        s.mails.push(Mail {
            id: id.clone(),
            spec,
            created_state,
        });
        id
    }

    pub fn state(&self) -> String {
        format!("s{}", self.lock().state)
    }

    /// Forget how to calculate changes from any state before now.
    pub fn expire_states(&self) {
        let mut s = self.lock();
        s.state += 1;
        s.oldest_valid = s.state;
    }

    /// From now on, HTTP errors carry a `application/problem+json` body.
    pub fn problem_json_errors(&self, on: bool) {
        self.lock().problem_json = on;
    }

    /// Make `method` answer with a method-level error of `error_type`, or heal with `None`.
    pub fn fail_method(&self, failure: Option<(&str, &str)>) {
        self.lock().method_error = failure.map(|(m, t)| (m.to_owned(), t.to_owned()));
    }

    pub fn fail_api(&self, status: Option<u16>) {
        self.lock().fail_api_with = status;
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.lock().requests.clone()
    }

    /// The method names of every API call so far, one entry per POST.
    pub fn api_calls(&self) -> Vec<Vec<String>> {
        self.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .map(|r| {
                let v: Value = serde_json::from_str(&r.body).unwrap();
                v["methodCalls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c[0].as_str().unwrap().to_owned())
                    .collect()
            })
            .collect()
    }

    /// Every API method called so far, in order, across all requests.
    pub fn api_methods(&self) -> Vec<String> {
        self.api_calls().into_iter().flatten().collect()
    }

    pub fn api_call_count(&self) -> usize {
        self.api_calls().len()
    }

    pub fn push_email_state(&self) {
        let _ = self.signals.send(Signal::EmailState);
    }

    pub fn push_other_state(&self) {
        let _ = self.signals.send(Signal::OtherState);
    }

    pub fn kill_streams(&self) {
        let _ = self.signals.send(Signal::Close);
    }

    /// Event-stream connections opened so far.
    pub fn stream_connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

// ----------------------------------------------------------------------- server

pub async fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Recorded {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

pub async fn respond(stream: &mut TcpStream, status: u16, extra: &[(&str, &str)], body: &str) {
    let reason = match status {
        200 => "OK",
        307 => "Temporary Redirect",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
}

/// An HTTP error: plain, or as a problem document.
async fn respond_error(stream: &mut TcpStream, status: u16, problem: bool) {
    if problem {
        let body = json!({ "type": "about:blank", "title": "error", "status": status }).to_string();
        respond(
            stream,
            status,
            &[("content-type", "application/problem+json")],
            &body,
        )
        .await;
    } else {
        respond(stream, status, &[], "").await;
    }
}

async fn handle(
    mut stream: TcpStream,
    state: Arc<Mutex<Server>>,
    signals: broadcast::Sender<Signal>,
    connections: Arc<AtomicUsize>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let (authorized, fail, problem) = {
        let mut s = state.lock().unwrap();
        s.requests.push(request.clone());
        (
            request
                .headers
                .get("authorization")
                .is_some_and(|a| *a == s.expect_auth),
            s.fail_api_with,
            s.problem_json,
        )
    };
    if !authorized {
        respond_error(&mut stream, 401, problem).await;
        return;
    }
    let path = request.path.split('?').next().unwrap_or("").to_owned();
    match (request.method.as_str(), path.as_str()) {
        ("GET", "/.well-known/jmap") => {
            respond(&mut stream, 307, &[("location", "/jmap/session")], "").await;
        }
        ("GET", "/jmap/session") => {
            let base = state.lock().unwrap().base.clone();
            let doc = json!({
                "capabilities": {
                    "urn:ietf:params:jmap:core": {
                        "maxSizeUpload": 50_000_000, "maxConcurrentUpload": 4, "maxSizeRequest": 10_000_000,
                        "maxConcurrentRequests": 4, "maxCallsInRequest": 16, "maxObjectsInGet": 500,
                        "maxObjectsInSet": 500, "collationAlgorithms": ["i;ascii-numeric", "i;ascii-casemap"],
                    },
                    "urn:ietf:params:jmap:mail": {},
                },
                "accounts": { ACCOUNT: {
                    "name": "ficbot", "isPersonal": true, "isReadOnly": false,
                    "accountCapabilities": { "urn:ietf:params:jmap:mail": {} },
                }},
                "primaryAccounts": { "urn:ietf:params:jmap:mail": ACCOUNT },
                "username": "ficbot",
                "apiUrl": format!("{base}/jmap/"),
                "downloadUrl": format!("{base}/jmap/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}"),
                "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
                "eventSourceUrl": format!("{base}/jmap/eventsource/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
                "state": "sess1",
            });
            respond(
                &mut stream,
                200,
                &[("content-type", "application/json")],
                &doc.to_string(),
            )
            .await;
        }
        ("POST", "/jmap/") => {
            if let Some(status) = fail {
                respond_error(&mut stream, status, problem).await;
                return;
            }
            let calls: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let out = run_calls(&state.lock().unwrap(), &calls);
            respond(
                &mut stream,
                200,
                &[("content-type", "application/json")],
                &out.to_string(),
            )
            .await;
        }
        ("GET", "/jmap/eventsource/") => {
            connections.fetch_add(1, Ordering::SeqCst);
            let mut rx = signals.subscribe();
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n";
            if stream.write_all(head.as_bytes()).await.is_err()
                || stream
                    .write_all(b"event: ping\ndata: {\"interval\": 30}\n\n")
                    .await
                    .is_err()
            {
                return;
            }
            while let Ok(signal) = rx.recv().await {
                let frame = match signal {
                    Signal::Close => return,
                    Signal::EmailState => format!(
                        "event: state\ndata: {{\"@type\":\"StateChange\",\"changed\":{{\"{ACCOUNT}\":{{\"Email\":\"s{}\"}}}}}}\n\n",
                        state.lock().unwrap().state
                    ),
                    Signal::OtherState => {
                        format!(
                            "event: state\ndata: {{\"@type\":\"StateChange\",\"changed\":{{\"{ACCOUNT}\":{{\"Mailbox\":\"s1\"}}}}}}\n\n"
                        )
                    }
                };
                if stream.write_all(frame.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
        _ => respond(&mut stream, 404, &[], "").await,
    }
}

// ----------------------------------------------------------------------- methods

fn error(kind: &str, description: &str) -> (String, Value) {
    (
        "error".to_owned(),
        json!({ "type": kind, "description": description }),
    )
}

fn parse_state(s: &str) -> Option<u64> {
    s.strip_prefix('s')?.parse().ok()
}

fn run_calls(server: &Server, request: &Value) -> Value {
    let mut results: HashMap<String, (String, Value)> = HashMap::new();
    let mut responses = Vec::new();
    for call in request["methodCalls"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let (name, args, id) = (
            call[0].as_str().unwrap_or(""),
            &call[1],
            call[2].as_str().unwrap_or("").to_owned(),
        );
        let outcome = execute(server, name, args, &results);
        responses.push(json!([outcome.0, outcome.1, id]));
        results.insert(id, (name.to_owned(), outcome.1));
    }
    json!({ "methodResponses": responses, "sessionState": "sess1" })
}

fn resolve_ids(
    args: &Value,
    results: &HashMap<String, (String, Value)>,
) -> Result<Option<Vec<String>>, (String, Value)> {
    if let Some(reference) = args.get("#ids") {
        let (of, name, path) = (
            reference["resultOf"].as_str().unwrap_or(""),
            reference["name"].as_str().unwrap_or(""),
            reference["path"].as_str().unwrap_or(""),
        );
        let Some((called, value)) = results.get(of) else {
            return Err(error("invalidResultReference", "unknown call"));
        };
        if called != name {
            return Err(error("invalidResultReference", "wrong method"));
        }
        let Some(list) = value.pointer(path).and_then(Value::as_array) else {
            return Err(error(
                "invalidResultReference",
                "no such path (the earlier call failed?)",
            ));
        };
        return Ok(Some(
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ));
    }
    Ok(args.get("ids").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }))
}

fn execute(
    server: &Server,
    name: &str,
    args: &Value,
    results: &HashMap<String, (String, Value)>,
) -> (String, Value) {
    if let Some((method, kind)) = &server.method_error
        && method == name
    {
        return error(kind, "forced by the test");
    }
    if args["accountId"] != ACCOUNT {
        return error("accountNotFound", "no such account");
    }
    match name {
        "Mailbox/get" => (
            name.to_owned(),
            json!({ "accountId": ACCOUNT, "state": "mbs", "list": [
                { "id": INBOX, "role": "inbox", "name": "Inbox" },
                { "id": SENT, "role": "sent", "name": "Sent Items" },
            ]}),
        ),
        "Mailbox/query" => {
            let ids: Vec<&str> = match args["filter"]["role"].as_str() {
                Some("inbox") => vec![INBOX],
                Some("sent") => vec![SENT],
                Some(_) => vec![],
                None => vec![INBOX, SENT],
            };
            (
                name.to_owned(),
                json!({ "accountId": ACCOUNT, "queryState": "mq1", "canCalculateChanges": false, "position": 0, "ids": ids }),
            )
        }
        "Email/get" => {
            let ids = match resolve_ids(args, results) {
                Ok(ids) => ids.unwrap_or_default(),
                Err(e) => return e,
            };
            let props: Vec<&str> = args["properties"]
                .as_array()
                .map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).collect());
            let (want_text, want_html) = (
                args["fetchTextBodyValues"] == true,
                args["fetchHTMLBodyValues"] == true,
            );
            let mut list = Vec::new();
            let mut not_found = Vec::new();
            for id in &ids {
                let Some(mail) = server.mails.iter().find(|m| &m.id == id) else {
                    not_found.push(id.clone());
                    continue;
                };
                let mut obj = serde_json::Map::new();
                obj.insert("id".into(), json!(mail.id));
                let spec = &mail.spec;
                if props.contains(&"receivedAt") {
                    obj.insert("receivedAt".into(), json!(spec.received_at));
                }
                if props.contains(&"from") {
                    obj.insert(
                        "from".into(),
                        json!([{ "name": spec.from_name, "email": spec.from_email }]),
                    );
                }
                if props.contains(&"subject") {
                    obj.insert("subject".into(), json!(spec.subject));
                }
                if props.contains(&"mailboxIds") {
                    obj.insert("mailboxIds".into(), json!({ spec.mailbox.clone(): true }));
                }
                if props.contains(&"textBody") {
                    obj.insert(
                        "textBody".into(),
                        json!([{ "partId": format!("t{}", mail.id) }]),
                    );
                }
                if props.contains(&"htmlBody") {
                    obj.insert(
                        "htmlBody".into(),
                        json!([{ "partId": format!("h{}", mail.id) }]),
                    );
                }
                // The point of the strictness: bodyValues only when listed in properties.
                if props.contains(&"bodyValues") {
                    let mut values = serde_json::Map::new();
                    if want_text {
                        values.insert(
                            format!("t{}", mail.id),
                            json!({ "value": spec.text, "isTruncated": false }),
                        );
                    }
                    if want_html && !spec.html.is_empty() {
                        values.insert(
                            format!("h{}", mail.id),
                            json!({ "value": spec.html, "isTruncated": false }),
                        );
                    }
                    obj.insert("bodyValues".into(), Value::Object(values));
                }
                list.push(Value::Object(obj));
            }
            (
                name.to_owned(),
                json!({ "accountId": ACCOUNT, "state": format!("s{}", server.state), "list": list, "notFound": not_found }),
            )
        }
        "Email/query" => {
            let mut hits: Vec<&Mail> = server
                .mails
                .iter()
                .filter(|m| matches_filter(m, &args["filter"]))
                .collect();
            hits.sort_by(|a, b| a.spec.received_at.cmp(&b.spec.received_at));
            if args["sort"][0]["isAscending"] == false {
                hits.reverse();
            }
            let limit = args["limit"]
                .as_u64()
                .map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
            let ids: Vec<&str> = hits.iter().take(limit).map(|m| m.id.as_str()).collect();
            (
                name.to_owned(),
                json!({ "accountId": ACCOUNT, "queryState": format!("s{}", server.state), "canCalculateChanges": false, "position": 0, "ids": ids }),
            )
        }
        "Email/changes" => {
            let Some(since) = args["sinceState"].as_str().and_then(parse_state) else {
                return error("invalidArguments", "invalid JMAP State");
            };
            if since < server.oldest_valid {
                return error("cannotCalculateChanges", "the state is too old");
            }
            let max = args["maxChanges"]
                .as_u64()
                .map_or(usize::MAX, |m| usize::try_from(m).unwrap_or(usize::MAX));
            let mut created: Vec<&Mail> = server
                .mails
                .iter()
                .filter(|m| m.created_state > since)
                .collect();
            created.sort_by_key(|m| m.created_state);
            let has_more = created.len() > max;
            created.truncate(max);
            let new_state = if has_more {
                created.last().map_or(since, |m| m.created_state)
            } else {
                server.state
            };
            (
                name.to_owned(),
                json!({
                    "accountId": ACCOUNT, "oldState": format!("s{since}"), "newState": format!("s{new_state}"),
                    "hasMoreChanges": has_more, "created": created.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                    "updated": [], "destroyed": [],
                }),
            )
        }
        other => error("unknownMethod", other),
    }
}

/// An `Email/query` filter: a condition, or `{"operator": "AND"|"OR"|"NOT", "conditions": [...]}`.
fn matches_filter(mail: &Mail, filter: &Value) -> bool {
    if filter.is_null() {
        return true;
    }
    if let Some(conditions) = filter["conditions"].as_array() {
        let mut results = conditions.iter().map(|c| matches_filter(mail, c));
        return match filter["operator"].as_str() {
            Some("OR") => results.any(|r| r),
            Some("NOT") => !results.any(|r| r),
            _ => results.all(|r| r),
        };
    }
    let inbox_ok = filter["inMailbox"]
        .as_str()
        .is_none_or(|i| mail.spec.mailbox == i);
    let after_ok = filter["after"]
        .as_str()
        .is_none_or(|a| mail.spec.received_at.as_str() > a);
    inbox_ok && after_ok
}
