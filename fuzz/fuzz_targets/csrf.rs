//! Any combination of method and fetch-metadata/origin headers: the CSRF check never
//! panics, always lets safe methods through, and never lets a cross-site POST in.
#![no_main]

use fanfold_adapter_web::security::csrf_allows;
use http::{HeaderMap, HeaderValue, Method};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let method = match data.first().copied().unwrap_or(0) % 4 {
        0 => Method::GET,
        1 => Method::POST,
        2 => Method::DELETE,
        _ => Method::HEAD,
    };
    let text = String::from_utf8_lossy(data);
    let mut lines = text.split('\n');
    let mut headers = HeaderMap::new();
    for name in ["sec-fetch-site", "origin", "host"] {
        if let Some(value) = lines.next().and_then(|v| HeaderValue::from_str(v).ok()) {
            headers.insert(name, value);
        }
    }
    let allowed = csrf_allows(&method, &headers);
    if matches!(method, Method::GET | Method::HEAD) {
        assert!(allowed, "safe methods always pass");
    }
    if method == Method::POST
        && headers
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        assert!(!allowed, "a cross-site POST must never be allowed");
    }
});
