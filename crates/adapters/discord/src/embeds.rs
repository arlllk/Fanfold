//! Discord embeds for every [`NotifyEvent`].
//!
//! Everything from the site (titles, names, tags, summaries, chapter titles, error messages)
//! is untrusted: it is escaped (see [`crate::sanitize`]) and cut to size. Links are built from
//! numeric ids or from percent-encoded names only. Discord's limits (6000 characters per
//! embed, 4096 per description, 1024 per field, 25 fields) are kept with room to spare.

use fanfold_app::ports::{DownloadDetails, NotifyEvent};
use fanfold_domain::{
    Timestamp,
    failure::FailureKind,
    job::{JobId, Target},
    metadata::{Chapter, SeriesPart, Tag, TagKind, WorkFacts, WorkMetadata},
    work::{SeriesId, WorkId},
};
use serde_json::{Value, json};

use crate::sanitize;

const GREEN: u32 = 0x002E_9E6B;
const BLUE: u32 = 0x003B_82F6;
const GOLD: u32 = 0x00E8_A317;
const PURPLE: u32 = 0x008B_5CF6;
const RED: u32 = 0x00DC_2626;
const ORANGE: u32 = 0x00F9_7316;

const MAX_TITLE: usize = 240;
const MAX_DESCRIPTION: usize = 2000;
const MAX_FIELD: usize = 240;
const MAX_ERROR: usize = 700;
const MAX_SUMMARY: usize = 320;
const MAX_LISTED_CHAPTERS: usize = 10;
/// Room the list of new chapters may take in a description.
const MAX_CHAPTER_LIST: usize = 1100;
const MAX_LISTED_TAGS: usize = 8;
/// A comfortable reading speed, for the "time to read" estimates.
const WORDS_PER_MINUTE: u64 = 250;

/// Where links in notifications point, beyond AO3 itself.
#[derive(Debug, Clone, Default)]
pub struct Links {
    /// The Fanfold web interface (e.g. `https://fanfold.example.org`): adds "open the job"
    /// links. `None` leaves them out.
    pub ui: Option<String>,
}

impl Links {
    fn job(&self, job: JobId) -> Option<String> {
        self.ui
            .as_deref()
            .map(|base| format!("{}/jobs/{job}", base.trim_end_matches('/')))
    }
}

/// The webhook payload for an event: one embed, no mentions.
#[must_use]
pub fn payload(event: &NotifyEvent, links: &Links) -> Value {
    json!({
        "embeds": [embed(event, links)],
        "allowed_mentions": { "parse": [] },
    })
}

fn embed(event: &NotifyEvent, links: &Links) -> Value {
    match event {
        NotifyEvent::Downloaded {
            job,
            work,
            title,
            updated,
            details,
        } => match details.as_deref() {
            Some(details) => download(*job, *work, *updated, details, links),
            None => bare_download(*job, *work, title, *updated, links),
        },
        NotifyEvent::SeriesListed {
            job,
            series,
            name,
            works,
            queued,
        } => series_listed(*job, *series, name, *works, *queued, links),
        NotifyEvent::GaveUp {
            job,
            target,
            title,
            kind,
            message,
            attempts,
            max_attempts,
        } => gave_up(
            *job,
            *target,
            title.as_deref(),
            *kind,
            message,
            (*attempts, *max_attempts),
            links,
        ),
        NotifyEvent::SiteDegraded {
            kind,
            message,
            retry_at,
            waiting,
        } => degraded(*kind, message, *retry_at, *waiting),
        NotifyEvent::SiteRecovered { down_since, at } => recovered(*down_since, *at),
    }
}

fn series_listed(
    job: JobId,
    series: SeriesId,
    name: &str,
    works: usize,
    queued: usize,
    links: &Links,
) -> Value {
    let mut rows = vec![format!(
        "Queued **{queued}** of its **{works}** works; each goes into the series' folder."
    )];
    if queued < works {
        rows.push(format!("{} were already queued.", works - queued));
    }
    push_job_link(&mut rows, links, job);
    json!({
        "author": { "name": "📚 Following a series" },
        "title": sanitize(name, MAX_TITLE),
        "url": series.canonical_url(),
        "color": PURPLE,
        "description": cap(&rows.join("\n")),
    })
}

fn gave_up(
    job: JobId,
    target: Target,
    title: Option<&str>,
    kind: FailureKind,
    message: &str,
    (attempts, max_attempts): (u32, u32),
    links: &Links,
) -> Value {
    let name = title.map_or_else(|| default_name(target), |t| sanitize(t, MAX_TITLE));
    let mut rows = vec![format!("**{}** — {}", kind_label(kind), kind_help(kind))];
    push_job_link(&mut rows, links, job);
    json!({
        "author": { "name": "❌ Gave up" },
        "title": name,
        "url": target.canonical_url(),
        "color": RED,
        "description": cap(&rows.join("\n")),
        "fields": [
            field("Attempts", &format!("{attempts} of {max_attempts}"), true),
            field("Kind", &format!("`{kind}`"), true),
            field("Error", &code_block(message), false),
        ],
    })
}

fn degraded(kind: FailureKind, message: &str, retry_at: Timestamp, waiting: Option<u64>) -> Value {
    let mut rows = vec![
        format!(
            "Downloads are paused: {}.",
            kind_help(kind).trim_end_matches('.')
        ),
        format!("The next probe runs {}.", discord_time(retry_at, 'R')),
    ];
    if let Some(waiting) = waiting {
        rows.push(format!(
            "**{waiting}** {} waiting meanwhile.",
            if waiting == 1 { "job is" } else { "jobs are" }
        ));
    }
    json!({
        "author": { "name": "⚠️ AO3 looks unhealthy" },
        "title": "Pausing downloads until it recovers",
        "color": ORANGE,
        "description": cap(&rows.join("\n")),
        "fields": [field("Last error", &code_block(message), false)],
    })
}

fn recovered(down_since: Option<Timestamp>, at: Timestamp) -> Value {
    let mut rows = vec!["Downloads resumed.".to_owned()];
    if let Some(since) = down_since {
        rows.push(format!(
            "The trouble lasted **{}** (since {}).",
            duration(at.as_second().saturating_sub(since.as_second())),
            discord_time(since, 't')
        ));
    }
    json!({
        "author": { "name": "✅ AO3 is responding again" },
        "title": "Back to normal",
        "color": GREEN,
        "description": cap(&rows.join("\n")),
    })
}

fn bare_download(job: JobId, work: WorkId, title: &str, updated: bool, links: &Links) -> Value {
    let mut rows = Vec::new();
    push_job_link(&mut rows, links, job);
    json!({
        "author": { "name": if updated { "🔄 Updated" } else { "📖 New book" } },
        "title": sanitize(title, MAX_TITLE),
        "url": work.canonical_url(),
        "color": if updated { BLUE } else { GREEN },
        "description": cap(&rows.join("\n")),
    })
}

fn download(
    job: JobId,
    work: WorkId,
    updated: bool,
    details: &DownloadDetails,
    links: &Links,
) -> Value {
    let m = &details.metadata;
    let f = &m.facts;
    let before = details.previous.as_ref().map(|p| &p.facts);
    let just_completed = before.is_some_and(|b| !is_complete(b)) && is_complete(f);

    let mut rows = vec![byline(m)];
    rows.extend(m.series.iter().take(3).map(series_line));
    rows.push(String::new());

    let (heading, color) = if updated {
        if details.previous.is_some() {
            rows.extend(new_chapter_lines(work, &details.new_chapters));
            rows.extend(changes(details));
        } else if let Some(last) = m.chapters.last() {
            rows.push(format!("Now up to {}.", chapter_label(work, last)));
        }
        let n = details.new_chapters.len();
        let heading = match (just_completed, details.previous.is_some(), n) {
            (true, _, 0) => "🎉 Completed".to_owned(),
            (true, _, n) => format!("🎉 Completed · {n} new {}", plural(n, "chapter")),
            (false, true, 0) | (false, false, _) => "🔄 Updated".to_owned(),
            (false, true, n) => format!("🆕 {n} new {}", plural(n, "chapter")),
        };
        (heading, if just_completed { GOLD } else { BLUE })
    } else {
        if let Some(last) = m.chapters.last() {
            rows.push(format!("Downloaded up to {}.", chapter_label(work, last)));
        }
        let summary = plain_text(&f.summary_html, MAX_SUMMARY);
        if !summary.is_empty() {
            rows.push(String::new());
            rows.extend(summary.lines().map(|l| format!("> {l}")));
        }
        let heading = if m.series.is_empty() {
            "📖 New book"
        } else {
            "📚 New book in a series"
        };
        (heading.to_owned(), GREEN)
    };
    push_job_link(&mut rows, links, job);

    let mut fields = vec![
        field("Chapters", &chapters_value(f), true),
        field("Words", &words_value(f.words), true),
        field("Status", &status_value(f), true),
        field("Rating", &rating_value(f), true),
    ];
    if let Some(published) = f.published_on {
        fields.push(field("Published", &published.to_string(), true));
    }
    if let Some(updated_on) = f.updated_on {
        fields.push(field("Updated", &updated_on.to_string(), true));
    }
    for (kind, label) in [
        (TagKind::Fandom, "Fandoms"),
        (TagKind::Relationship, "Relationships"),
        (TagKind::Character, "Characters"),
        (TagKind::Freeform, "Tags"),
    ] {
        if let Some(value) = tag_list(&m.tags, kind) {
            fields.push(field(label, &value, false));
        }
    }
    if let Some(value) = warnings(&m.tags) {
        fields.push(field("⚠️ Warnings", &value, false));
    }
    if let Some(value) = stats_value(m) {
        fields.push(field("Reception", &value, false));
    }

    json!({
        "author": { "name": heading },
        "title": sanitize(&f.title, MAX_TITLE),
        "url": work.canonical_url(),
        "color": color,
        "description": cap(&rows.join("\n")),
        "fields": fields,
        "footer": { "text": sanitize(&details.path, 300) },
    })
}

// ------------------------------------------------------------------- pieces

fn byline(m: &WorkMetadata) -> String {
    if m.authors.is_empty() {
        return format!("by **{}**", sanitize(&m.facts.byline, 200));
    }
    let names: Vec<String> = m
        .authors
        .iter()
        .take(5)
        .map(|a| {
            let label = if a.pseud == a.user || a.user.is_empty() {
                a.pseud.clone()
            } else {
                format!("{} ({})", a.pseud, a.user)
            };
            link(&label, &author_url(&a.user, &a.pseud))
        })
        .collect();
    format!("by {}", names.join(", "))
}

fn series_line(part: &SeriesPart) -> String {
    let series = link(&part.name, &part.id.canonical_url());
    match part.part {
        Some(n) => format!("📚 Part **{n}** of {series}"),
        None => format!("📚 In {series}"),
    }
}

fn chapter_label(work: WorkId, chapter: &Chapter) -> String {
    let label = format!("chapter {}", chapter.number);
    let title = chapter.title.trim();
    let text = if title.is_empty() || title.eq_ignore_ascii_case(&label) {
        format!("**Chapter {}**", chapter.number)
    } else {
        format!("**Chapter {}** · {}", chapter.number, sanitize(title, 120))
    };
    match chapter.ao3_id {
        Some(id) => format!(
            "[{}](https://archiveofourown.org/works/{work}/chapters/{id})",
            text.replace('[', "(").replace(']', ")")
        ),
        None => text,
    }
}

fn new_chapter_lines(work: WorkId, chapters: &[Chapter]) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut used = 0;
    for c in chapters.iter().take(MAX_LISTED_CHAPTERS) {
        let posted = c
            .posted_on
            .map_or_else(String::new, |d| format!(" — posted {d}"));
        let line = format!("• {}{posted}", chapter_label(work, c));
        used += line.chars().count() + 1;
        if used > MAX_CHAPTER_LIST && !rows.is_empty() {
            break;
        }
        rows.push(line);
    }
    if chapters.len() > rows.len() {
        rows.push(format!("…and {} more", chapters.len() - rows.len()));
    }
    if !rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// What an update changed besides the chapters, one line each.
fn changes(details: &DownloadDetails) -> Vec<String> {
    let Some(before) = &details.previous else {
        return Vec::new();
    };
    let (old, new) = (&before.facts, &details.metadata.facts);
    let mut rows = Vec::new();
    if old.chapters_posted != new.chapters_posted || old.chapters_total != new.chapters_total {
        rows.push(format!(
            "📈 Chapters {} → {}",
            old.chapters_posted,
            chapters_value(new)
        ));
    }
    if new.words > old.words {
        let added = new.words - old.words;
        rows.push(format!(
            "✍️ +{} words ({} to read)",
            group(added),
            reading_time(added)
        ));
    } else if new.words < old.words {
        rows.push(format!("✍️ −{} words", group(old.words - new.words)));
    }
    if let (Some(then), Some(now)) = (old.updated_on, new.updated_on)
        && now > then
    {
        let days = (now - then).get_days();
        rows.push(format!(
            "⏱ First update in {days} {}",
            plural(usize::try_from(days).unwrap_or(0), "day")
        ));
    }
    if !is_complete(old) && is_complete(new) {
        rows.push("🎉 The work is now **complete**".to_owned());
    } else if old.status != new.status {
        rows.push(format!(
            "📌 Status: {} → {}",
            sanitize(&old.status, 40),
            sanitize(&new.status, 40)
        ));
    }
    if old.title != new.title {
        rows.push(format!("📝 Retitled from “{}”", sanitize(&old.title, 120)));
    }
    if old.rating != new.rating {
        rows.push(format!(
            "🔞 Rating: {} → {}",
            sanitize(&old.rating, 40),
            sanitize(&new.rating, 40)
        ));
    }
    if old.restricted != new.restricted {
        rows.push(
            if new.restricted {
                "🔒 Now visible to logged-in users only"
            } else {
                "🔓 Now public"
            }
            .to_owned(),
        );
    }
    let (added, removed) = tag_changes(&before.tags, &details.metadata.tags);
    if !added.is_empty() {
        rows.push(format!("🏷 New tags: {}", added.join(", ")));
    }
    if !removed.is_empty() {
        rows.push(format!("🗑 Tags removed: {}", removed.join(", ")));
    }
    for part in &details.metadata.series {
        if !before.series.iter().any(|p| p.id == part.id) {
            rows.push(format!(
                "➕ Added to {}",
                series_line(part).trim_start_matches("📚 ")
            ));
        }
    }
    rows
}

fn tag_changes(before: &[Tag], after: &[Tag]) -> (Vec<String>, Vec<String>) {
    let show = |tags: Vec<&Tag>| -> Vec<String> {
        let total = tags.len();
        let mut shown: Vec<String> = tags
            .into_iter()
            .take(MAX_LISTED_TAGS)
            .map(|t| sanitize(&t.name, 60))
            .collect();
        if total > MAX_LISTED_TAGS {
            shown.push(format!("+{} more", total - MAX_LISTED_TAGS));
        }
        shown
    };
    let added = after.iter().filter(|t| !before.contains(t)).collect();
    let removed = before.iter().filter(|t| !after.contains(t)).collect();
    (show(added), show(removed))
}

fn tag_list(tags: &[Tag], kind: TagKind) -> Option<String> {
    let names: Vec<&str> = tags
        .iter()
        .filter(|t| t.kind == kind)
        .map(|t| t.name.as_str())
        .collect();
    if names.is_empty() {
        return None;
    }
    let limit = if kind == TagKind::Freeform {
        MAX_LISTED_TAGS
    } else {
        4
    };
    let mut shown: Vec<String> = names.iter().take(limit).map(|n| sanitize(n, 60)).collect();
    if names.len() > limit {
        shown.push(format!("*+{} more*", names.len() - limit));
    }
    Some(shown.join(" · "))
}

/// Archive warnings worth a look: the generic "none apply" / "chose not to" are left out.
fn warnings(tags: &[Tag]) -> Option<String> {
    let real: Vec<String> = tags
        .iter()
        .filter(|t| t.kind == TagKind::Warning)
        .filter(|t| {
            !matches!(
                t.name.as_str(),
                "No Archive Warnings Apply" | "Creator Chose Not To Use Archive Warnings"
            )
        })
        .map(|t| sanitize(&t.name, 80))
        .collect();
    (!real.is_empty()).then(|| real.join(" · "))
}

fn stats_value(m: &WorkMetadata) -> Option<String> {
    let s = m.stats;
    let parts: Vec<String> = [
        (s.kudos, "❤️", "kudos"),
        (s.hits, "👁", "hits"),
        (s.comments, "💬", "comments"),
        (s.bookmarks, "🔖", "bookmarks"),
    ]
    .into_iter()
    .filter_map(|(n, icon, label)| n.map(|n| format!("{icon} {} {label}", group(n))))
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn is_complete(f: &WorkFacts) -> bool {
    f.status.eq_ignore_ascii_case("completed")
        || f.chapters_total
            .is_some_and(|total| total > 0 && total == f.chapters_posted)
}

fn chapters_value(f: &WorkFacts) -> String {
    let total = f
        .chapters_total
        .map_or_else(|| "?".to_owned(), |t| t.to_string());
    format!("{}/{total}", f.chapters_posted)
}

fn words_value(words: u64) -> String {
    format!("{} · {}", group(words), reading_time(words))
}

fn status_value(f: &WorkFacts) -> String {
    if is_complete(f) {
        "✅ Complete".to_owned()
    } else {
        format!(
            "✏️ {}",
            sanitize(&f.status, 40).replace("In-Progress", "In progress")
        )
    }
}

fn rating_value(f: &WorkFacts) -> String {
    let icon = match f.rating.as_str() {
        "General Audiences" => "🟢",
        "Teen And Up Audiences" => "🟡",
        "Mature" => "🟠",
        "Explicit" => "🔴",
        _ => "⚪",
    };
    let lock = if f.restricted {
        " · 🔒 logged-in only"
    } else {
        ""
    };
    format!("{icon} {}{lock}", sanitize(&f.rating, 40))
}

const fn kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::SiteUnavailable => "AO3 unavailable",
        FailureKind::RateLimited => "Rate limited",
        FailureKind::Blocked => "Blocked",
        FailureKind::AuthFailed => "Login needed",
        FailureKind::NotFound => "Not found",
        FailureKind::Transient => "Unexpected error",
        FailureKind::Permanent => "Cannot work",
    }
}

const fn kind_help(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::SiteUnavailable => {
            "AO3, or Cloudflare in front of it, did not answer properly."
        }
        FailureKind::RateLimited => "AO3 asked us to slow down.",
        FailureKind::Blocked => {
            "Cloudflare refused the request; a fresh browser session on the Session page may help."
        }
        FailureKind::AuthFailed => {
            "the work needs a login, and logging in did not work. Check the AO3 credentials."
        }
        FailureKind::NotFound => "the work is gone or hidden (deleted, or back to a draft).",
        FailureKind::Transient => "something unexpected went wrong; the job log has the details.",
        FailureKind::Permanent => "this cannot work as things are; the job log has the details.",
    }
}

fn default_name(target: Target) -> String {
    match target {
        Target::Work(w) => format!("Work #{w}"),
        Target::Series(s) => format!("Series #{s}"),
    }
}

fn push_job_link(rows: &mut Vec<String>, links: &Links, job: JobId) {
    if let Some(url) = links.job(job) {
        if rows.last().is_some_and(|l| !l.is_empty()) {
            rows.push(String::new());
        }
        rows.push(format!("[Open the job in Fanfold]({})", url_safe(&url)));
    }
}

fn field(name: &str, value: &str, inline: bool) -> Value {
    let value = if value.chars().count() > MAX_FIELD && !value.starts_with("```") {
        let cut: String = value.chars().take(MAX_FIELD).collect();
        format!("{cut}…")
    } else {
        value.to_owned()
    };
    json!({ "name": name, "value": if value.is_empty() { "—".to_owned() } else { value }, "inline": inline })
}

/// Untrusted text in a code block: backticks cannot close it early.
fn code_block(text: &str) -> String {
    let clean: String = text
        .replace('`', "ʼ")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(MAX_ERROR)
        .collect();
    format!("```\n{}\n```", if clean.is_empty() { "—" } else { &clean })
}

fn cap(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.chars().count() <= MAX_DESCRIPTION {
        return trimmed.to_owned();
    }
    let cut: String = trimmed.chars().take(MAX_DESCRIPTION).collect();
    format!("{cut}…")
}

/// A markdown link with an escaped label.
fn link(label: &str, url: &str) -> String {
    format!(
        "[{}]({})",
        sanitize(label, 100).replace('[', "(").replace(']', ")"),
        url_safe(url)
    )
}

/// Parentheses and spaces would end a markdown link early.
fn url_safe(url: &str) -> String {
    url.replace('(', "%28")
        .replace(')', "%29")
        .replace(' ', "%20")
        .replace('@', "%40")
}

fn author_url(user: &str, pseud: &str) -> String {
    let Ok(mut url) = url::Url::parse("https://archiveofourown.org/") else {
        return "https://archiveofourown.org/".to_owned();
    };
    if let Ok(mut path) = url.path_segments_mut() {
        path.clear();
        path.push("users");
        path.push(if user.is_empty() { pseud } else { user });
        path.push("pseuds");
        path.push(pseud);
    }
    url.into()
}

/// HTML from the site as plain text, at most `max` characters, escaped for markdown.
fn plain_text(html: &str, max: usize) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match (in_tag, c) {
            (false, '<') => {
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                let name = tag.trim_start_matches('/').to_ascii_lowercase();
                if name.starts_with("br") || name.starts_with('p') || name.starts_with("div") {
                    text.push('\n');
                }
            }
            (true, c) => tag.push(c),
            (false, c) => text.push(c),
        }
    }
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&");
    let paragraphs: Vec<String> = decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    sanitize(&paragraphs.join("\n"), max)
}

/// `<t:...>`: Discord shows it in each reader's own time zone (`R`: "in 5 minutes").
fn discord_time(at: Timestamp, style: char) -> String {
    format!("<t:{}:{style}>", at.as_second())
}

fn duration(seconds: i64) -> String {
    let minutes = (seconds.max(0) + 59) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

fn reading_time(words: u64) -> String {
    let minutes = words.div_ceil(WORDS_PER_MINUTE);
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("~{} min", m.max(1)),
        (h, 0) => format!("~{h} h"),
        (h, m) => format!("~{h} h {m} min"),
    }
}

/// 179890 → "179,890".
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}
