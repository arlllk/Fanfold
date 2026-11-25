//! The static files, compiled into the binary and served under content-hashed names.
//!
//! Each file is addressed as `/static/<stem>.<hash>.<ext>`, where the hash is taken from its
//! bytes when the server starts. Pages are never cached (`no-store`), so they always name the
//! current files; a name only ever means one content, so it is cached for a year as
//! `immutable` by browsers and by Cloudflare alike, and a release that changes a file changes
//! its name: nothing has to be purged. The hash is in the path, not a query string, so it holds
//! whatever the CDN does with query strings in its cache key.
//!
//! The stylesheet names fonts with `url()`; those are rewritten to the hashed names before the
//! stylesheet itself is hashed, so a new font gives a new stylesheet too.
//!
//! A plain name (`/static/app.js`) or a stale hash (a tab opened before a deploy, a request that
//! reached the other pod in a rolling update) still gets the current file, as `no-cache` with an
//! `ETag`: it is revalidated each time, never stored by the CDN, and never breaks a page.

use std::{collections::HashMap, fmt::Write as _, sync::LazyLock};

use sha2::{Digest, Sha256};

const JS: &str = "text/javascript; charset=utf-8";

/// Every static file: its plain name under `/static/`, its type and its bytes.
const FILES: [(&str, &str, &[u8]); 9] = [
    (
        "app.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/app.css"),
    ),
    ("app.js", JS, include_bytes!("../assets/app.js")),
    (
        "icon.svg",
        "image/svg+xml",
        include_bytes!("../assets/icon.svg"),
    ),
    // Self-hosted third-party files (assets/vendor, pinned by vendor.toml).
    (
        "vendor/htmx.min.js",
        JS,
        include_bytes!("../assets/vendor/htmx.min.js"),
    ),
    (
        "vendor/hx-sse.min.js",
        JS,
        include_bytes!("../assets/vendor/hx-sse.min.js"),
    ),
    (
        "vendor/hx-alpine-compat.min.js",
        JS,
        include_bytes!("../assets/vendor/hx-alpine-compat.min.js"),
    ),
    (
        "vendor/alpine-csp.min.js",
        JS,
        include_bytes!("../assets/vendor/alpine-csp.min.js"),
    ),
    (
        "vendor/literata-latin.woff2",
        "font/woff2",
        include_bytes!("../assets/vendor/literata-latin.woff2"),
    ),
    (
        "vendor/literata-latin-italic.woff2",
        "font/woff2",
        include_bytes!("../assets/vendor/literata-latin-italic.woff2"),
    ),
];

/// Hex digits of the content hash in a name: 64 bits, plenty to tell versions apart.
const HASH_LEN: usize = 16;

/// One static file, ready to serve.
#[derive(Debug, PartialEq, Eq)]
pub struct Asset {
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// The content hash, quoted, as an `ETag`.
    pub etag: String,
    /// Its address: `/static/<stem>.<hash>.<ext>`.
    pub url: String,
}

/// How a request named a file.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// The current hashed name: cache forever.
    Current(&'a Asset),
    /// A plain or stale name for a known file: serve it, but revalidate every time.
    Moved(&'a Asset),
}

#[derive(Debug)]
struct Assets {
    /// By plain name (`app.js`, `vendor/htmx.min.js`).
    by_name: HashMap<&'static str, Asset>,
}

static ASSETS: LazyLock<Assets> = LazyLock::new(Assets::build);

impl Assets {
    fn build() -> Self {
        let mut by_name = HashMap::new();
        // Everything but the stylesheet first: it names the fonts.
        for (name, content_type, bytes) in FILES.iter().filter(|(n, _, _)| !is_css(n)) {
            by_name.insert(*name, Asset::new(name, content_type, bytes.to_vec()));
        }
        for (name, content_type, bytes) in FILES.iter().filter(|(n, _, _)| is_css(n)) {
            let mut css = String::from_utf8_lossy(bytes).into_owned();
            for (other, asset) in &by_name {
                css = css.replace(&format!("/static/{other})"), &format!("{})", asset.url));
            }
            by_name.insert(*name, Asset::new(name, content_type, css.into_bytes()));
        }
        Self { by_name }
    }
}

impl Asset {
    fn new(name: &str, content_type: &'static str, body: Vec<u8>) -> Self {
        let mut hash = String::with_capacity(HASH_LEN);
        for byte in Sha256::digest(&body).iter().take(HASH_LEN / 2) {
            let _ = write!(hash, "{byte:02x}");
        }
        let url = format!("/static/{}", hashed_name(name, &hash));
        Self {
            content_type,
            body,
            etag: format!("\"{hash}\""),
            url,
        }
    }
}

/// `vendor/htmx.min.js` + `abcd` → `vendor/htmx.min.abcd.js`.
fn hashed_name(name: &str, hash: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.{hash}.{ext}"),
        None => format!("{name}.{hash}"),
    }
}

/// The plain name behind a requested one: drops a `.<hash>` before the extension, if any.
fn plain_name(requested: &str) -> String {
    let Some((rest, ext)) = requested.rsplit_once('.') else {
        return requested.to_owned();
    };
    match rest.rsplit_once('.') {
        Some((stem, hash))
            if hash.len() == HASH_LEN && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            format!("{stem}.{ext}")
        }
        _ => requested.to_owned(),
    }
}

/// The address of a static file, by its plain name; empty for a name that is not one of
/// [`FILES`] (the UI tests check every page names only hashed files).
#[must_use]
pub fn url(name: &str) -> &'static str {
    ASSETS
        .by_name
        .get(name)
        .map_or("", |asset| asset.url.as_str())
}

fn is_css(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("css"))
}

/// The file a request under `/static/` names (`path` is what follows `/static/`).
#[must_use]
pub fn lookup(path: &str) -> Option<Lookup<'static>> {
    let asset = ASSETS.by_name.get(plain_name(path).as_str())?;
    Some(if asset.url.strip_prefix("/static/") == Some(path) {
        Lookup::Current(asset)
    } else {
        Lookup::Moved(asset)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_the_hash_before_the_extension() {
        assert_eq!(
            hashed_name("vendor/htmx.min.js", "ab12"),
            "vendor/htmx.min.ab12.js"
        );
        let js = url("app.js");
        assert!(
            js.starts_with("/static/app.")
                && std::path::Path::new(js)
                    .extension()
                    .is_some_and(|e| e == "js"),
            "{js}"
        );
        assert_eq!(js.len(), "/static/app..js".len() + HASH_LEN);
    }

    #[test]
    fn current_plain_and_stale_names_all_find_the_file() {
        let current = url("vendor/htmx.min.js").trim_start_matches("/static/");
        assert!(matches!(lookup(current), Some(Lookup::Current(_))));
        assert!(matches!(
            lookup("vendor/htmx.min.js"),
            Some(Lookup::Moved(_))
        ));
        assert!(matches!(
            lookup("vendor/htmx.min.0123456789abcdef.js"),
            Some(Lookup::Moved(_))
        ));
        assert_eq!(lookup("vendor/nope.js"), None);
        assert_eq!(
            lookup("vendor/htmx.min.zz23456789abcdef.js"),
            None,
            "not a hash"
        );
        assert_eq!(lookup("../Cargo.toml"), None);
    }

    #[test]
    fn the_stylesheet_names_the_hashed_fonts() {
        let css = std::str::from_utf8(&ASSETS.by_name["app.css"].body).unwrap_or_default();
        for font in [
            "vendor/literata-latin.woff2",
            "vendor/literata-latin-italic.woff2",
        ] {
            assert!(css.contains(&format!("url({})", url(font))), "{font}");
            assert!(
                !css.contains(&format!("/static/{font})")),
                "{font} left plain"
            );
        }
    }

    #[test]
    fn a_name_changes_with_the_content() {
        let a = Asset::new("x.js", JS, b"one".to_vec());
        let b = Asset::new("x.js", JS, b"two".to_vec());
        assert_ne!(a.url, b.url);
        assert_eq!(a.etag.len(), HASH_LEN + 2);
    }
}
