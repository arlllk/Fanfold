//! The page Fanfold adds: a cover page, first in the book.
//!
//! Plain XHTML with labelled paragraphs rather than tables or definition lists: e-readers
//! override styles freely, and plain paragraphs survive any of them.

use crate::text::esc;

const HEAD: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html>\n";

fn page(title: &str, lang: &str, style: &str, body: &str) -> String {
    format!(
        "{HEAD}<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"{lang}\" lang=\"{lang}\">\n\
         <head>\n<title>{}</title>\n<link href=\"stylesheet.css\" type=\"text/css\" rel=\"stylesheet\"/>\n<style type=\"text/css\">{style}</style>\n</head>\n\
         <body>\n{body}</body>\n</html>\n",
        esc(title)
    )
}

/// The cover, as the first page of the book.
#[must_use]
pub fn cover_page(title: &str, lang: &str) -> String {
    page(
        title,
        lang,
        "html, body { margin: 0; padding: 0; height: 100%; } \
         div.ff-cover { height: 100%; text-align: center; } \
         div.ff-cover img { max-width: 100%; max-height: 100%; }",
        "<section epub:type=\"cover\"><div class=\"ff-cover\"><img src=\"fanfold_cover.png\" alt=\"Cover\"/></div></section>\n",
    )
}
