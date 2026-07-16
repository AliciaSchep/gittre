use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use git2::Repository;

use crate::git::diff::{self, DiffResult};
use crate::git::scope::{self, Scope};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileLoadKind {
    Stub { untracked_dir: bool },
    FullContext,
}

/// Work for the background loader. Everything git-expensive runs there; the
/// UI thread never computes a diff.
pub enum LoadRequest {
    /// Load (or reload) the diff for a scope.
    Diff { seq: u64, scope: Scope },
    /// Compute the scope picker's file counts.
    Counts { seq: u64 },
    /// Expand one stub: a large file (no size cap) or an untracked dir.
    File {
        seq: u64,
        scope: Scope,
        path: String,
        old_path: Option<String>,
        kind: FileLoadKind,
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
    /// The loader finished a diff. `seq` pairs it with its request; stale
    /// responses (seq mismatch) are dropped.
    Diff {
        seq: u64,
        scope: Scope,
        diff: Result<DiffResult>,
        took: Duration,
    },
    Counts {
        seq: u64,
        counts: ScopeCounts,
    },
    /// Expansion result: one or more files replacing the stub at `path`.
    File {
        seq: u64,
        scope: Scope,
        path: String,
        kind: FileLoadKind,
        files: Result<Vec<crate::git::diff::FileDiff>>,
    },
}

fn request_seq(request: &LoadRequest) -> u64 {
    match request {
        LoadRequest::Diff { seq, .. }
        | LoadRequest::Counts { seq }
        | LoadRequest::File { seq, .. } => *seq,
    }
}

fn push_file_request(requests: &mut Vec<LoadRequest>, request: LoadRequest) {
    let LoadRequest::File {
        seq, scope, path, ..
    } = &request
    else {
        return;
    };
    if let Some(existing) = requests.iter_mut().find(|existing| {
        matches!(
            existing,
            LoadRequest::File {
                seq: existing_seq,
                scope: existing_scope,
                path: existing_path,
                ..
            } if existing_seq == seq && existing_scope == scope && existing_path == path
        )
    }) {
        *existing = request;
    } else {
        requests.push(request);
    }
}

fn retain_newest_generation(
    diff: &mut Option<LoadRequest>,
    counts: &mut Option<LoadRequest>,
    files: &mut Vec<LoadRequest>,
) {
    let generation = diff
        .iter()
        .chain(counts.iter())
        .chain(files.iter())
        .map(request_seq)
        .max();
    let Some(generation) = generation else {
        return;
    };
    if diff.as_ref().is_some_and(|r| request_seq(r) != generation) {
        *diff = None;
    }
    if counts
        .as_ref()
        .is_some_and(|r| request_seq(r) != generation)
    {
        *counts = None;
    }
    files.retain(|r| request_seq(r) == generation);
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
            // Coalesce a burst down to the newest request of each kind and
            // one expansion per path in a scope generation.
            let mut diff_req = None;
            let mut counts_req = None;
            let mut file_reqs = Vec::new();
            let mut sort = |req: LoadRequest| match req {
                LoadRequest::Diff { .. } => diff_req = Some(req),
                LoadRequest::Counts { .. } => counts_req = Some(req),
                LoadRequest::File { .. } => push_file_request(&mut file_reqs, req),
            };
            sort(first);
            while let Ok(req) = req_rx.try_recv() {
                sort(req);
            }

            // A later screen/scope generation supersedes every older kind of
            // work before it consumes git time. A reload and expansions may
            // share a generation, in which case the reload runs first.
            retain_newest_generation(&mut diff_req, &mut counts_req, &mut file_reqs);

            if let Some(LoadRequest::Diff { seq, scope }) = diff_req {
                let start = Instant::now();
                let diff = diff::load(&repo, &scope);
                let event = AppEvent::Diff {
                    seq,
                    scope,
                    diff,
                    took: start.elapsed(),
                };
                if events.send(event).is_err() {
                    return; // app is gone
                }
            }

            for req in file_reqs {
                let LoadRequest::File {
                    seq,
                    scope,
                    path,
                    old_path,
                    kind,
                } = req
                else {
                    continue;
                };
                let files = match kind {
                    FileLoadKind::Stub {
                        untracked_dir: true,
                    } => repo
                        .workdir()
                        .ok_or_else(|| anyhow::anyhow!("no working directory"))
                        .and_then(|wd| crate::git::cli::load_untracked_dir(wd, &path)),
                    FileLoadKind::Stub {
                        untracked_dir: false,
                    } => diff::load_file(&repo, &scope, &path).map(|f| vec![f]),
                    FileLoadKind::FullContext => {
                        diff::load_file_full_context(&repo, &scope, &path, old_path.as_deref())
                            .map(|f| vec![f])
                    }
                };
                let event = AppEvent::File {
                    seq,
                    scope,
                    path,
                    kind,
                    files,
                };
                if events.send(event).is_err() {
                    return;
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
                if events.send(AppEvent::Counts { seq, counts }).is_err() {
                    return;
                }
            }
        }
    });
    req_tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_file_requests_coalesce_by_generation_scope_and_path() {
        let mut requests = Vec::new();
        push_file_request(
            &mut requests,
            LoadRequest::File {
                seq: 4,
                scope: Scope::Uncommitted,
                path: "src/lib.rs".into(),
                old_path: None,
                kind: FileLoadKind::Stub {
                    untracked_dir: false,
                },
            },
        );
        push_file_request(
            &mut requests,
            LoadRequest::File {
                seq: 4,
                scope: Scope::Uncommitted,
                path: "src/lib.rs".into(),
                old_path: None,
                kind: FileLoadKind::Stub {
                    untracked_dir: true,
                },
            },
        );
        push_file_request(
            &mut requests,
            LoadRequest::File {
                seq: 5,
                scope: Scope::Uncommitted,
                path: "src/lib.rs".into(),
                old_path: Some("src/old.rs".into()),
                kind: FileLoadKind::FullContext,
            },
        );

        assert_eq!(requests.len(), 2);
        assert!(matches!(
            requests[0],
            LoadRequest::File {
                seq: 4,
                kind: FileLoadKind::Stub {
                    untracked_dir: true
                },
                ..
            }
        ));
    }

    #[test]
    fn newest_generation_discards_stale_work_of_every_kind() {
        let mut diff = Some(LoadRequest::Diff {
            seq: 3,
            scope: Scope::Uncommitted,
        });
        let mut counts = Some(LoadRequest::Counts { seq: 2 });
        let mut files = vec![
            LoadRequest::File {
                seq: 2,
                scope: Scope::Uncommitted,
                path: "old.rs".into(),
                old_path: None,
                kind: FileLoadKind::Stub {
                    untracked_dir: false,
                },
            },
            LoadRequest::File {
                seq: 3,
                scope: Scope::Uncommitted,
                path: "current.rs".into(),
                old_path: None,
                kind: FileLoadKind::FullContext,
            },
        ];

        retain_newest_generation(&mut diff, &mut counts, &mut files);

        assert!(diff.is_some());
        assert!(counts.is_none());
        assert_eq!(files.len(), 1);
        assert_eq!(request_seq(&files[0]), 3);
    }
}
