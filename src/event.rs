use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use git2::Repository;

use crate::git::diff::{self, DiffResult};
use crate::git::scope::{self, Scope};

/// Work for the background loader. Everything git-expensive runs there; the
/// UI thread never computes a diff.
pub enum LoadRequest {
    /// Load (or reload) the diff for a scope.
    Diff { seq: u64, scope: Scope },
    /// Compute the scope picker's file counts.
    Counts { seq: u64 },
    /// Expand one stub: a large file (no size cap) or an untracked dir.
    File {
        scope: Scope,
        path: String,
        untracked_dir: bool,
    },
}

pub struct ScopeCounts {
    pub uncommitted: usize,
    pub staged: usize,
    /// (base name, count) when a base was auto-detected.
    pub branch: Option<(String, usize)>,
}

/// Everything the main loop can be woken up by besides key input.
pub enum AppEvent {
    /// The watcher saw a relevant change in the repo.
    RepoChanged,
    /// The loader finished a diff. `seq` pairs it with its request; stale
    /// responses (seq mismatch) are dropped.
    DiffLoaded {
        seq: u64,
        scope: Scope,
        diff: Result<DiffResult>,
        took: Duration,
    },
    CountsLoaded {
        seq: u64,
        counts: ScopeCounts,
    },
    /// Expansion result: one or more files replacing the stub at `path`.
    FileLoaded {
        scope_label: String,
        path: String,
        files: Result<Vec<crate::git::diff::FileDiff>>,
    },
}

/// Spawn the background loader. git2 handles can't be shared across threads,
/// so the worker discovers its own from the same path.
pub fn spawn_loader(repo_path: PathBuf, events: Sender<AppEvent>) -> Sender<LoadRequest> {
    let (req_tx, req_rx): (Sender<LoadRequest>, Receiver<LoadRequest>) = channel();
    thread::spawn(move || {
        let Ok(repo) = Repository::discover(&repo_path) else {
            return;
        };
        while let Ok(first) = req_rx.recv() {
            // Coalesce a burst down to the newest request of each kind
            // (file expansions are all kept — each is a distinct file).
            let mut diff_req = None;
            let mut counts_req = None;
            let mut file_reqs = Vec::new();
            let mut sort = |req: LoadRequest| match req {
                LoadRequest::Diff { .. } => diff_req = Some(req),
                LoadRequest::Counts { .. } => counts_req = Some(req),
                LoadRequest::File { .. } => file_reqs.push(req),
            };
            sort(first);
            while let Ok(req) = req_rx.try_recv() {
                sort(req);
            }

            for req in file_reqs {
                let LoadRequest::File {
                    scope,
                    path,
                    untracked_dir,
                } = req
                else {
                    continue;
                };
                let files = if untracked_dir {
                    repo.workdir()
                        .ok_or_else(|| anyhow::anyhow!("no working directory"))
                        .and_then(|wd| crate::git::cli::load_untracked_dir(wd, &path))
                } else {
                    diff::load_file(&repo, &scope, &path).map(|f| vec![f])
                };
                let event = AppEvent::FileLoaded {
                    scope_label: scope.label(),
                    path,
                    files,
                };
                if events.send(event).is_err() {
                    return;
                }
            }

            if let Some(LoadRequest::Diff { seq, scope }) = diff_req {
                let start = Instant::now();
                let diff = diff::load(&repo, &scope);
                let event = AppEvent::DiffLoaded {
                    seq,
                    scope,
                    diff,
                    took: start.elapsed(),
                };
                if events.send(event).is_err() {
                    return; // app is gone
                }
            }
            if let Some(LoadRequest::Counts { seq }) = counts_req {
                // One `git status` gives both worktree counts (fast even on
                // huge repos when fsmonitor is on); libgit2 as fallback.
                let (uncommitted, staged) = repo
                    .workdir()
                    .and_then(|wd| crate::git::cli::worktree_counts(wd).ok())
                    .unwrap_or_else(|| {
                        (
                            scope::file_count_fast(&repo, &Scope::Uncommitted),
                            scope::file_count_fast(&repo, &Scope::Staged),
                        )
                    });
                let counts = ScopeCounts {
                    uncommitted,
                    staged,
                    branch: scope::forkable_branch(&repo).map(|branch| {
                        let scope = Scope::BranchFork {
                            branch: branch.clone(),
                        };
                        (branch, scope::file_count_fast(&repo, &scope))
                    }),
                };
                if events.send(AppEvent::CountsLoaded { seq, counts }).is_err() {
                    return;
                }
            }
        }
    });
    req_tx
}
