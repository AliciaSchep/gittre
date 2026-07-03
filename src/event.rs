use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use anyhow::Result;
use git2::Repository;

use crate::git::diff::{self, DiffResult};
use crate::git::scope::Scope;

/// Everything the main loop can be woken up by besides key input.
pub enum AppEvent {
    /// The watcher saw a relevant change in the repo.
    RepoChanged,
    /// The loader finished re-diffing. `seq` pairs it with its request;
    /// stale responses (seq mismatch) are dropped.
    DiffLoaded { seq: u64, diff: Result<DiffResult> },
}

/// Spawn the background diff loader. git2 handles can't be shared across
/// threads, so the worker discovers its own from the same path.
/// Returns the request sender; requests are (seq, scope).
pub fn spawn_loader(repo_path: PathBuf, events: Sender<AppEvent>) -> Sender<(u64, Scope)> {
    let (req_tx, req_rx): (Sender<(u64, Scope)>, Receiver<(u64, Scope)>) = channel();
    thread::spawn(move || {
        let Ok(repo) = Repository::discover(&repo_path) else {
            return;
        };
        while let Ok((mut seq, mut scope)) = req_rx.recv() {
            // Coalesce a burst of requests down to the newest one.
            while let Ok((s, sc)) = req_rx.try_recv() {
                seq = s;
                scope = sc;
            }
            let diff = diff::load(&repo, &scope);
            if events.send(AppEvent::DiffLoaded { seq, diff }).is_err() {
                return; // app is gone
            }
        }
    });
    req_tx
}
