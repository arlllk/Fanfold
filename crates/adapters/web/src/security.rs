//! Cross-site request protection and response hardening.
//!
//! The service sits behind a gateway that authenticates with a session cookie, so a
//! hostile page could make the user's browser POST here with their cookie attached
//! (CSRF). Unsafe methods are therefore only accepted from the same origin.

use std::fmt::Write as _;

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::Response,
};

use crate::problem::ProblemDetails;

/// Whether a request with these headers may change state.
///
/// 1. Modern browsers send `Sec-Fetch-Site`; only `same-origin` (or `none`, a direct
///    navigation) is accepted.
/// 2. Otherwise fall back to comparing `Origin` with `Host`.
/// 3. With neither header the client is not a browser (curl, a script), which cannot
///    be tricked into carrying someone's cookie.
#[must_use]
pub fn csrf_allows(method: &Method, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    if let Some(site) = headers.get("sec-fetch-site") {
        return matches!(site.to_str(), Ok("same-origin" | "none"));
    }
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => {
            let (Ok(origin), Some(Ok(host))) = (
                origin.to_str(),
                headers.get(header::HOST).map(HeaderValue::to_str),
            ) else {
                return false;
            };
            let authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
            !host.is_empty() && authority.eq_ignore_ascii_case(host)
        }
    }
}

pub async fn csrf_guard(request: Request, next: Next) -> Response {
    if csrf_allows(request.method(), request.headers()) {
        return next.run(request).await;
    }
    tracing::warn!(method = %request.method(), path = %request.uri().path(), "rejected cross-site request");
    ProblemDetails::about_blank(StatusCode::FORBIDDEN)
        .with_detail("Cross-site requests are not allowed.")
        .into_axum_response()
}

/// Content-Security-Policy: everything comes from this origin, nothing inline.
pub const CSP: &str = "default-src 'none'; style-src 'self'; script-src 'self'; img-src 'self'; font-src 'self'; \
                       connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// [`CSP`] with a one-time nonce in `script-src`, for HTML responses.
///
/// Fanfold has no inline script, so the nonce is never used by its own pages. It is there for
/// the CDN: Cloudflare's JavaScript Detections append an inline script to every page and copy
/// the nonce they find in this header onto it; without one, the policy blocks that script and
/// every page logs a violation. Everything else in the policy stays as strict.
#[must_use]
pub fn csp_with_nonce(nonce: &str) -> String {
    CSP.replacen(
        "script-src 'self'",
        &format!("script-src 'self' 'nonce-{nonce}'"),
        1,
    )
}

/// 128 random bits as hex (a valid CSP nonce); `None` if the OS has no randomness to give.
fn nonce() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    Some(hex)
}

fn is_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"))
}

pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // A fresh nonce per page; anything else (static files, events, problems) keeps the fixed
    // policy, so a cached file never carries a nonce. Without randomness, the fixed policy.
    let csp = is_html(headers)
        .then(nonce)
        .flatten()
        .and_then(|n| HeaderValue::from_str(&csp_with_nonce(&n)).ok())
        .unwrap_or(HeaderValue::from_static(CSP));
    headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nonce_only_widens_script_src() {
        let csp = csp_with_nonce("00ff");
        assert!(csp.contains("script-src 'self' 'nonce-00ff';"), "{csp}");
        assert_eq!(csp.replace(" 'nonce-00ff'", ""), CSP);
        let n = nonce().unwrap_or_default();
        assert_eq!(n.len(), 32);
        assert!(n.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(nonce(), nonce(), "one per response");
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn safe_methods_always_pass() {
        let hostile = headers(&[("sec-fetch-site", "cross-site")]);
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(csrf_allows(&m, &hostile), "{m}");
        }
    }

    #[test]
    fn fetch_metadata_decides_when_present() {
        let post = |pairs: &[(&str, &str)]| csrf_allows(&Method::POST, &headers(pairs));
        assert!(post(&[("sec-fetch-site", "same-origin")]));
        assert!(post(&[("sec-fetch-site", "none")]));
        assert!(!post(&[("sec-fetch-site", "cross-site")]));
        assert!(
            !post(&[("sec-fetch-site", "same-site")]),
            "a sibling subdomain is not us"
        );
        assert!(!post(&[("sec-fetch-site", "garbage")]));
        // The header wins even if Origin claims otherwise.
        assert!(!post(&[
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://a.example"),
            ("host", "a.example")
        ]));
    }

    #[test]
    fn origin_is_compared_with_host_as_a_fallback() {
        let post = |pairs: &[(&str, &str)]| csrf_allows(&Method::POST, &headers(pairs));
        assert!(post(&[
            ("origin", "https://fanfold.example"),
            ("host", "fanfold.example")
        ]));
        assert!(post(&[
            ("origin", "https://FANFOLD.example"),
            ("host", "fanfold.EXAMPLE")
        ]));
        assert!(post(&[
            ("origin", "http://localhost:8080"),
            ("host", "localhost:8080")
        ]));
        assert!(!post(&[
            ("origin", "https://evil.example"),
            ("host", "fanfold.example")
        ]));
        assert!(!post(&[
            ("origin", "https://fanfold.example.evil.com"),
            ("host", "fanfold.example")
        ]));
        assert!(!post(&[("origin", "null"), ("host", "fanfold.example")]));
        assert!(
            !post(&[("origin", "https://fanfold.example")]),
            "Origin without Host cannot be verified"
        );
    }

    #[test]
    fn non_browser_clients_without_either_header_are_allowed() {
        assert!(csrf_allows(&Method::POST, &HeaderMap::new()));
        assert!(csrf_allows(&Method::DELETE, &headers(&[("host", "x")])));
    }

    #[test]
    fn the_csp_forbids_inline_content() {
        assert!(!CSP.contains("unsafe-inline") && !CSP.contains("unsafe-eval"));
        assert!(CSP.contains("default-src 'none'") && CSP.contains("frame-ancestors 'none'"));
    }
}
