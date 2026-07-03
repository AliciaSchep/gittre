use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use git2::Repository;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};

use crate::event::AppEvent;

/// Keeps the filesystem watcher alive; dropping it stops watching.
pub struct RepoWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Watch the working tree and the git metadata that changes review scopes.
/// Worktree-aware: `repo.path()` (per-worktree HEAD/index) and
/// `repo.commondir()` (shared refs) can live outside the working directory.
pub fn spawn(repo: &Repository, events: Sender<AppEvent>) -> Option<RepoWatcher> {
    let workdir = repo.workdir()?.to_path_buf();
    let gitdir = repo.path().to_path_buf();
    let commondir = repo.commondir().to_path_buf();

    // A private handle for git-ignore checks on the debouncer thread, so
    // build artifacts (target/, node_modules/…) don't trigger reloads.
    let ignore_repo = Repository::discover(&workdir).ok();

    let filter = EventFilter {
        workdir: workdir.clone(),
        gitdir: gitdir.clone(),
        commondir: commondir.clone(),
        ignore_repo,
    };
    let mut debouncer = new_debouncer(DEBOUNCE, move |res: DebounceEventResult| {
        if let Ok(batch) = res {
            if batch.iter().any(|e| filter.is_relevant(&e.path)) {
                let _ = events.send(AppEvent::RepoChanged);
            }
        }
    })
    .ok()?;

    debouncer
        .watcher()
        .watch(&workdir, RecursiveMode::Recursive)
        .ok()?;
    // In a linked worktree the gitdir/commondir sit outside the workdir.
    for extra in [&gitdir, &commondir] {
        if !extra.starts_with(&workdir) {
            let _ = debouncer.watcher().watch(extra, RecursiveMode::Recursive);
        }
    }

    Some(RepoWatcher {
        _debouncer: debouncer,
    })
}

struct EventFilter {
    workdir: PathBuf,
    gitdir: PathBuf,
    commondir: PathBuf,
    ignore_repo: Option<Repository>,
}

impl EventFilter {
    fn is_relevant(&self, path: &Path) -> bool {
        if path.starts_with(&self.gitdir) || path.starts_with(&self.commondir) {
            // Inside git metadata: only ref/index/HEAD movement matters;
            // object and log churn would cause pointless reload storms.
            let s = path.to_string_lossy();
            return s.ends_with("HEAD")
                || s.ends_with("index")
                || s.ends_with("packed-refs")
                || s.contains("/refs/");
        }
        // Working tree: skip git-ignored paths (target/, node_modules/…).
        if let Some(repo) = &self.ignore_repo {
            if let Ok(rel) = path.strip_prefix(&self.workdir) {
                if repo.is_path_ignored(rel).unwrap_or(false) {
                    return false;
                }
            }
        }
        true
    }
}
