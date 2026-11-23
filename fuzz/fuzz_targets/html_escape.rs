//! Any text a hostile site could put in a title, error, file name or log line: the
//! rendered pages never gain markup. Compared with a benign render of the same
//! shape, the number of `<`, `>` and `"` characters must be identical, because every
//! such character that came from the input has to be escaped.
//!
//! `'` is deliberately not checked: maud does not escape it, and need not, since every
//! attribute it writes is double-quoted, so an apostrophe cannot end one. (The fuzzer
//! found this out the first time round; `apostrophes_cannot_break_out_of_attributes` in
//! the web adapter's tests pins the reasoning.)
#![no_main]

use fanfold_adapter_web::views::{
    DashboardModel, JobModel, JobRow, dashboard_content, job_content,
};
use fanfold_app::ports::{BookRecord, JobCounts, LogEntry, LogLevel, SiteStatus};
use fanfold_domain::{
    Timestamp,
    job::{Job, JobId, JobSource, JobState},
    work::WorkId,
};
use libfuzzer_sys::fuzz_target;

fn render(text: &str) -> Option<String> {
    let now = Timestamp::UNIX_EPOCH;
    let work = WorkId::new(1)?;
    let mut job = Job::new(JobId::new(1), work, JobSource::Mail, now);
    job.state = JobState::Failed;
    job.last_error = Some(text.to_owned());
    let row = JobRow {
        job,
        title: Some(text.to_owned()),
    };

    let dash = DashboardModel {
        now,
        site: SiteStatus::Healthy,
        counts: JobCounts::default(),
        attention: vec![row.clone()],
        in_progress: Vec::new(),
        recent: vec![row.clone()],
        shelf: vec![row.clone()],
    };
    let detail = JobModel {
        now,
        site: SiteStatus::Healthy,
        book: Some(BookRecord {
            work,
            file_name: text.to_owned(),
            title: text.to_owned(),
            updated_at: now,
        }),
        logs: vec![LogEntry {
            at: now,
            attempt: 0,
            level: LogLevel::Error,
            message: text.to_owned(),
        }],
        row,
        max_attempts: 5,
    };
    Some(format!(
        "{}{}",
        dashboard_content(&dash).into_string(),
        job_content(&detail).into_string()
    ))
}

fn counts(html: &str) -> [usize; 3] {
    ['<', '>', '"'].map(|c| html.matches(c).count())
}

fuzz_target!(|data: &[u8]| {
    let hostile = String::from_utf8_lossy(data);
    let (Some(hostile_html), Some(benign_html)) = (render(&hostile), render("x")) else {
        return;
    };
    assert_eq!(
        counts(&hostile_html),
        counts(&benign_html),
        "input leaked markup: {hostile:?}"
    );
});
