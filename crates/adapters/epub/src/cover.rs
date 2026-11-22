//! The generated cover: an SVG laid out here, rendered to PNG with the fonts built into the
//! binary (the image has no system fonts).
//!
//! Made to read at shelf-thumbnail size: a large title, a background colour that is the same
//! for every work of a fandom, and AO3's rating colour as a stripe and a badge.

// Layout arithmetic in pixels: small, positive numbers, where a lost bit of float precision
// or a fused multiply-add makes no visible difference.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::suboptimal_flops
)]

use std::{fmt::Write as _, sync::Arc};

use fanfold_app::ports::BookExtras;
use fanfold_domain::metadata::TagKind;
use resvg::{tiny_skia, usvg};

use crate::{
    book::main_series,
    text::{chapter_count, complete, day, esc, thousands},
};

const SERIF: &[u8] = include_bytes!("../fonts/NotoSerif-Bold.ttf");
const SANS: &[u8] = include_bytes!("../fonts/NotoSans-Regular.ttf");
const SERIF_FAMILY: &str = "Noto Serif";
const SANS_FAMILY: &str = "Noto Sans";

pub const WIDTH: u32 = 1000;
pub const HEIGHT: u32 = 1500;
const MARGIN: f32 = 80.0;
const TEXT_WIDTH: f32 = 840.0;

/// Deep tones that keep white text readable; a fandom always gets the same one.
const BACKGROUNDS: [&str; 12] = [
    "#1f3a5f", "#4a2545", "#2d4a3e", "#5b2a2a", "#2f2f4f", "#4a3d1f", "#173f4a", "#45305e",
    "#16404d", "#5a3418", "#2b4227", "#4a1f33",
];

/// AO3's rating colours, the letter on the badge, and whether the letter is dark.
fn rating_style(rating: &str) -> (&'static str, &'static str, bool) {
    match rating {
        "General Audiences" => ("#7ab317", "G", false),
        "Teen And Up Audiences" => ("#e9c117", "T", true),
        "Mature" => ("#ef8a1e", "M", false),
        "Explicit" => ("#c0172b", "E", false),
        _ => ("#9aa0a6", "?", true),
    }
}

/// FNV-1a: stable across builds and platforms, unlike the standard hasher.
fn stable_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Fonts and the means to measure text with them.
pub struct Renderer {
    fontdb: Arc<usvg::fontdb::Database>,
    serif: ttf_parser::Face<'static>,
    sans: ttf_parser::Face<'static>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
enum Font {
    Serif,
    Sans,
}

impl Renderer {
    pub fn new() -> Result<Self, String> {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(SERIF.to_vec());
        db.load_font_data(SANS.to_vec());
        db.set_serif_family(SERIF_FAMILY);
        db.set_sans_serif_family(SANS_FAMILY);
        Ok(Self {
            fontdb: Arc::new(db),
            serif: ttf_parser::Face::parse(SERIF, 0).map_err(|e| format!("serif font: {e}"))?,
            sans: ttf_parser::Face::parse(SANS, 0).map_err(|e| format!("sans font: {e}"))?,
        })
    }

    /// The width of `text` at `size` px. Characters the font lacks count as half an em.
    fn width(&self, font: Font, text: &str, size: f32) -> f32 {
        let face = match font {
            Font::Serif => &self.serif,
            Font::Sans => &self.sans,
        };
        let em = f32::from(face.units_per_em());
        let units: f32 = text
            .chars()
            .map(|c| {
                face.glyph_index(c)
                    .and_then(|g| face.glyph_hor_advance(g))
                    .map_or(em / 2.0, f32::from)
            })
            .sum();
        units / em * size
    }

    /// `text` broken into lines no wider than `width`; words longer than a line are split.
    fn wrap(&self, font: Font, text: &str, size: f32, width: f32) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if self.width(font, &candidate, size) <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            // A word wider than a whole line: split it where it overflows.
            let mut piece = String::new();
            for c in word.chars() {
                piece.push(c);
                if self.width(font, &piece, size) > width && piece.chars().count() > 1 {
                    piece.pop();
                    lines.push(std::mem::take(&mut piece));
                    piece.push(c);
                }
            }
            line = piece;
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
    }

    /// At most `max` lines; the last one ends in an ellipsis when text was left out.
    fn clamp(&self, font: Font, mut lines: Vec<String>, max: usize, size: f32) -> Vec<String> {
        if lines.len() <= max {
            return lines;
        }
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            while !last.is_empty() && self.width(font, &format!("{last}…"), size) > TEXT_WIDTH {
                last.pop();
            }
            *last = format!("{}…", last.trim_end());
        }
        lines
    }

    /// The cover as PNG.
    pub fn render(&self, extras: &BookExtras) -> Result<Vec<u8>, String> {
        let svg = self.svg(extras);
        let options = usvg::Options {
            fontdb: Arc::clone(&self.fontdb),
            font_family: SANS_FAMILY.to_owned(),
            ..usvg::Options::default()
        };
        let tree = usvg::Tree::from_str(&svg, &options).map_err(|e| format!("cover: {e}"))?;
        let mut pixmap =
            tiny_skia::Pixmap::new(WIDTH, HEIGHT).ok_or_else(|| "cover: no pixmap".to_owned())?;
        resvg::render(
            &tree,
            tiny_skia::Transform::identity(),
            &mut pixmap.as_mut(),
        );
        pixmap.encode_png().map_err(|e| format!("cover: {e}"))
    }

    #[allow(clippy::too_many_lines)]
    pub fn svg(&self, extras: &BookExtras) -> String {
        let metadata = &extras.metadata;
        let facts = &metadata.facts;
        let fandoms: Vec<&str> = metadata
            .tags
            .iter()
            .filter(|t| t.kind == TagKind::Fandom)
            .map(|t| t.name.as_str())
            .collect();
        let background = fandoms.first().map_or(BACKGROUNDS[0], |f| {
            let i = usize::try_from(stable_hash(f) % BACKGROUNDS.len() as u64).unwrap_or(0);
            BACKGROUNDS.get(i).copied().unwrap_or(BACKGROUNDS[0])
        });
        let (rating_colour, letter, dark_letter) = rating_style(&facts.rating);

        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{WIDTH}\" height=\"{HEIGHT}\" viewBox=\"0 0 {WIDTH} {HEIGHT}\">\n\
             <rect width=\"{WIDTH}\" height=\"{HEIGHT}\" fill=\"{background}\"/>\n\
             <rect width=\"{WIDTH}\" height=\"1500\" fill=\"url(#shade)\"/>\n\
             <defs><linearGradient id=\"shade\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
             <stop offset=\"0\" stop-color=\"#ffffff\" stop-opacity=\"0.08\"/>\
             <stop offset=\"1\" stop-color=\"#000000\" stop-opacity=\"0.25\"/></linearGradient></defs>\n\
             <rect width=\"{WIDTH}\" height=\"18\" fill=\"{rating_colour}\"/>\n"
        );
        let text = |svg: &mut String, font: Font, size: f32, y: f32, opacity: f32, line: &str| {
            let (family, weight) = match font {
                Font::Serif => (SERIF_FAMILY, 700),
                Font::Sans => (SANS_FAMILY, 400),
            };
            let _ = writeln!(
                svg,
                "<text x=\"{MARGIN}\" y=\"{y:.0}\" font-family=\"{family}\" font-weight=\"{weight}\" font-size=\"{size:.0}\" fill=\"#ffffff\" fill-opacity=\"{opacity}\">{}</text>",
                esc(line)
            );
        };

        // Fandoms, small, at the top.
        let mut y = 150.0;
        if !fandoms.is_empty() {
            let size = 36.0;
            let lines = self.wrap(Font::Sans, &fandoms.join(" · "), size, TEXT_WIDTH);
            for line in self.clamp(Font::Sans, lines, 2, size) {
                text(&mut svg, Font::Sans, size, y, 0.8, &line);
                y += size * 1.35;
            }
        }
        let rule = y - 6.0;
        let _ = writeln!(
            svg,
            "<rect x=\"{MARGIN}\" y=\"{rule:.0}\" width=\"140\" height=\"6\" fill=\"{rating_colour}\"/>"
        );

        // The title (as large as fits), the creators and the series, centred between the
        // rule and the band.
        let title = if facts.title.trim().is_empty() {
            format!("Work {}", metadata.work.get())
        } else {
            facts.title.clone()
        };
        let creators = if metadata.authors.is_empty() {
            facts.byline.clone()
        } else {
            metadata
                .authors
                .iter()
                .map(|a| {
                    if a.pseud.is_empty() {
                        a.user.clone()
                    } else {
                        a.pseud.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        // (font, size, line height, opacity, gap before, lines)
        let mut below: Vec<(Font, f32, f32, f32, f32, Vec<String>)> = Vec::new();
        if !creators.is_empty() {
            let size = 46.0;
            let lines = self.wrap(Font::Sans, &format!("by {creators}"), size, TEXT_WIDTH);
            below.push((
                Font::Sans,
                size,
                size * 1.3,
                0.92,
                50.0,
                self.clamp(Font::Sans, lines, 2, size),
            ));
        }
        if let Some(series) = main_series(extras) {
            let size = 38.0;
            let label = match series.part {
                Some(n) => format!("{} · Part {n}", series.name),
                None => series.name.clone(),
            };
            let lines = self.wrap(Font::Sans, &label, size, TEXT_WIDTH);
            below.push((
                Font::Sans,
                size,
                size * 1.3,
                0.78,
                16.0,
                self.clamp(Font::Sans, lines, 2, size),
            ));
        }
        let below_height: f32 = below
            .iter()
            .map(|(_, _, height, _, gap, lines)| gap + height * lines.len() as f32)
            .sum();
        let (area_top, area_bottom) = (rule + 70.0, 1270.0);
        let room = area_bottom - area_top - below_height;
        let mut size = 124.0_f32;
        let mut lines = self.wrap(Font::Serif, &title, size, TEXT_WIDTH);
        while size > 60.0 && (lines.len() > 7 || lines.len() as f32 * size * 1.15 > room) {
            size -= 4.0;
            lines = self.wrap(Font::Serif, &title, size, TEXT_WIDTH);
        }
        let max = ((room / (size * 1.15)).floor().max(1.0)) as usize;
        let lines = self.clamp(Font::Serif, lines, max, size);
        let total = size * 1.15 * lines.len() as f32 + below_height;
        // The top of the next line; a line's baseline sits 0.8 of its size below it.
        let mut top = area_top + ((area_bottom - area_top - total) / 2.0).max(0.0);
        for line in &lines {
            text(&mut svg, Font::Serif, size, top + size * 0.85, 1.0, line);
            top += size * 1.15;
        }
        for (font, size, height, opacity, gap, lines) in &below {
            top += gap;
            for line in lines {
                text(&mut svg, *font, *size, top + size * 0.85, *opacity, line);
                top += height;
            }
        }

        // The band: rating badge, progress, size.
        let _ = writeln!(
            svg,
            "<rect y=\"1320\" width=\"{WIDTH}\" height=\"180\" fill=\"#000000\" fill-opacity=\"0.3\"/>\n\
             <rect x=\"{MARGIN}\" y=\"1360\" width=\"100\" height=\"100\" rx=\"16\" fill=\"{rating_colour}\"/>\n\
             <text x=\"{:.0}\" y=\"1432\" text-anchor=\"middle\" font-family=\"{SERIF_FAMILY}\" font-weight=\"700\" font-size=\"64\" fill=\"{}\">{letter}</text>",
            MARGIN + 50.0,
            if dark_letter { "#1a1a1a" } else { "#ffffff" }
        );
        let progress = if complete(facts) {
            format!("Complete · {} words", thousands(facts.words))
        } else {
            format!(
                "{} chapters · {} words",
                chapter_count(facts),
                thousands(facts.words)
            )
        };
        let detail = facts
            .updated_on
            .or(facts.published_on)
            .map_or_else(String::new, |d| format!("Updated {}", day(d)));
        let x = MARGIN + 130.0;
        let _ = writeln!(
            svg,
            "<text x=\"{x:.0}\" y=\"1402\" font-family=\"{SANS_FAMILY}\" font-size=\"38\" fill=\"#ffffff\">{}</text>\n\
             <text x=\"{x:.0}\" y=\"1450\" font-family=\"{SANS_FAMILY}\" font-size=\"30\" fill=\"#ffffff\" fill-opacity=\"0.7\">{}</text>",
            esc(&progress),
            esc(&detail)
        );
        svg.push_str("</svg>\n");
        svg
    }
}
