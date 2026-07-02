use anyhow::{Context, Result};
use git2::{Oid, Repository};

/// Cap the commit picker; nobody scrolls a list further than this to review.
const LOG_LIMIT: usize = 1000;

pub struct LogEntry {
    pub id: Oid,
    pub short: String,
    pub summary: String,
    pub author: String,
    pub age: String,
}

/// Most recent commits reachable from HEAD, newest first.
pub fn commit_log(repo: &Repository) -> Result<Vec<LogEntry>> {
    let mut walk = repo.revwalk().context("starting revwalk")?;
    if walk.push_head().is_err() {
        return Ok(Vec::new()); // unborn branch: no commits to list
    }

    let now = git2::Time::new(chrono_now(), 0);
    let mut entries = Vec::new();
    for oid in walk.take(LOG_LIMIT) {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        entries.push(LogEntry {
            id: oid,
            short: format!("{oid:.7}"),
            summary: commit.summary().unwrap_or(None).unwrap_or("").to_string(),
            author: commit.author().name().unwrap_or("?").to_string(),
            age: humanize(now.seconds() - commit.time().seconds()),
        });
    }
    Ok(entries)
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn humanize(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        86_400..=604_799 => format!("{}d", s / 86_400),
        604_800..=31_535_999 => format!("{}w", s / 604_800),
        _ => format!("{}y", s / 31_536_000),
    }
}
