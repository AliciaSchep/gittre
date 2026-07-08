use anyhow::{Context, Result, anyhow};
use git2::{DiffOptions, Oid, Repository};

/// What is being reviewed. Each variant maps to one entry in the scope picker.
#[derive(Debug, Clone)]
pub enum Scope {
    /// Working tree + index vs HEAD, untracked included.
    Uncommitted,
    /// Index vs HEAD.
    Staged,
    /// Merge-base of HEAD and `base` vs HEAD (the branch's own commits).
    Branch { base: String },
    /// One commit vs its first parent.
    Commit { id: Oid, summary: String },
    /// Everything between two commits: tree(from) vs tree(to).
    Range { from: Oid, to: Oid },
}

impl Scope {
    /// Short description for the title bar.
    pub fn label(&self) -> String {
        match self {
            Scope::Uncommitted => "uncommitted changes".into(),
            Scope::Staged => "staged changes".into(),
            Scope::Branch { base } => format!("branch vs {base}"),
            Scope::Commit { id, summary } => {
                let mut s = summary.clone();
                if s.chars().count() > 40 {
                    s = format!("{}…", s.chars().take(39).collect::<String>());
                }
                format!("commit {id:.7} \u{201c}{s}\u{201d}")
            }
            Scope::Range { from, to } => format!("range {from:.7}..{to:.7}"),
        }
    }

    /// Message shown when the scope has no changes.
    pub fn empty_message(&self) -> &'static str {
        match self {
            Scope::Uncommitted => "no uncommitted changes — working tree clean",
            Scope::Staged => "no staged changes",
            Scope::Branch { .. } => "no changes vs base — branch has no commits of its own",
            Scope::Commit { .. } => "empty commit",
            Scope::Range { .. } => "no changes between these commits",
        }
    }
}

/// Pick a review base for the current branch: its upstream if set, else the
/// first of main/master/develop that exists and isn't the current branch.
pub fn detect_base(repo: &Repository) -> Option<String> {
    let head = repo.head().ok()?;
    let current = head.shorthand().ok().map(str::to_owned);

    if head.is_branch() {
        let branch = git2::Branch::wrap(head);
        if let Ok(upstream) = branch.upstream() {
            if let Ok(Some(name)) = upstream.name() {
                return Some(name.to_string());
            }
        }
    }

    ["main", "master", "develop"]
        .iter()
        .find(|&&name| {
            Some(name) != current.as_deref()
                && repo.find_branch(name, git2::BranchType::Local).is_ok()
        })
        .map(|&name| name.to_string())
}

/// Candidate review bases: every local and remote branch except the current
/// one, locals first. Used when no base could be auto-detected.
pub fn base_candidates(repo: &Repository) -> Vec<String> {
    let current = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().ok().map(str::to_owned));
    let mut names = Vec::new();
    for branch_type in [git2::BranchType::Local, git2::BranchType::Remote] {
        let Ok(branches) = repo.branches(Some(branch_type)) else {
            continue;
        };
        for branch in branches.flatten() {
            let (branch, _) = branch;
            if let Ok(Some(name)) = branch.name() {
                if Some(name) != current.as_deref() && !name.ends_with("/HEAD") {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// Build the raw git2 diff for a scope. Shared by the full loader (which walks
/// content) and the picker's file counts (which only look at deltas).
pub fn build_diff<'r>(repo: &'r Repository, scope: &Scope) -> Result<git2::Diff<'r>> {
    build_diff_with(repo, scope, |opts| {
        // Oversized files get no content here; collect() turns them into
        // stubs the UI expands on demand via build_file_diff.
        opts.max_size(MAX_CONTENT_FILE_SIZE);
    })
}

fn build_diff_with<'r>(
    repo: &'r Repository,
    scope: &Scope,
    configure: impl FnOnce(&mut DiffOptions),
) -> Result<git2::Diff<'r>> {
    let mut opts = DiffOptions::new();
    opts.include_typechange(true);
    configure(&mut opts);

    let mut diff = match scope {
        Scope::Uncommitted => {
            opts.include_untracked(true)
                .recurse_untracked_dirs(true)
                .show_untracked_content(true)
                // Refresh stale index stat-cache entries (like `git status`
                // does). Without this, libgit2 re-hashes every mistrusted
                // file on EVERY diff — the cost gets paid once instead.
                .update_index(true);
            let head_tree = head_tree(repo)?;
            repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))
                .context("diffing working tree vs HEAD")?
        }
        Scope::Staged => {
            let head_tree = head_tree(repo)?;
            repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
                .context("diffing index vs HEAD")?
        }
        Scope::Branch { base } => {
            let head = repo
                .head()
                .context("resolving HEAD")?
                .peel_to_commit()
                .context("resolving HEAD commit")?;
            let base_commit = repo
                .revparse_single(base)
                .with_context(|| format!("resolving base '{base}'"))?
                .peel_to_commit()
                .map_err(|_| anyhow!("base '{base}' is not a commit"))?;
            let mb = repo
                .merge_base(head.id(), base_commit.id())
                .with_context(|| format!("no merge base between HEAD and '{base}'"))?;
            let mb_tree = repo.find_commit(mb)?.tree()?;
            repo.diff_tree_to_tree(Some(&mb_tree), Some(&head.tree()?), Some(&mut opts))
                .context("diffing merge base vs HEAD")?
        }
        Scope::Commit { id, .. } => {
            let commit = repo.find_commit(*id).context("finding commit")?;
            let parent_tree = match commit.parent(0) {
                Ok(parent) => Some(parent.tree()?),
                Err(_) => None, // root commit: diff against the empty tree
            };
            repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&commit.tree()?), Some(&mut opts))
                .context("diffing commit vs parent")?
        }
        Scope::Range { from, to } => {
            let from_tree = repo
                .find_commit(*from)
                .context("finding range start")?
                .tree()?;
            let to_tree = repo.find_commit(*to).context("finding range end")?.tree()?;
            repo.diff_tree_to_tree(Some(&from_tree), Some(&to_tree), Some(&mut opts))
                .context("diffing range")?
        }
    };

    // Rename detection reads file contents pairwise; on huge diffs it can
    // dominate load time, so skip it past a threshold (statuses still show
    // as delete+add, which is honest if less pretty).
    if diff.deltas().len() <= RENAME_DETECTION_LIMIT {
        let t = std::time::Instant::now();
        diff.find_similar(None).ok();
        crate::app::debug_log(&format!("diff phase: rename detection {:?}", t.elapsed()));
    }
    Ok(diff)
}

/// Above this many changed files, skip rename detection.
const RENAME_DETECTION_LIMIT: usize = 2000;

/// Files above this size (bytes) get no content diff up front; the UI shows
/// a stub and loads the file's diff on demand (GitHub-style).
pub const MAX_CONTENT_FILE_SIZE: i64 = 1024 * 1024;

/// Build the diff for a single path within a scope, with no size cap —
/// used to expand a large-file stub on demand.
pub fn build_file_diff<'r>(
    repo: &'r Repository,
    scope: &Scope,
    path: &str,
) -> Result<git2::Diff<'r>> {
    let diff = build_diff_with(repo, scope, |opts| {
        opts.pathspec(path);
    })?;
    Ok(diff)
}

/// Number of changed files in a scope, built as cheaply as possible: no
/// untracked contents, no rename detection. Counts only — never reuse for
/// display.
pub fn file_count_fast(repo: &Repository, scope: &Scope) -> usize {
    let mut opts = DiffOptions::new();
    opts.include_typechange(true);
    let diff = match scope {
        Scope::Uncommitted => {
            opts.include_untracked(true).recurse_untracked_dirs(true);
            let Ok(head_tree) = head_tree(repo) else {
                return 0;
            };
            repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))
        }
        // The other scopes are cheap to build; reuse the real construction.
        _ => return build_diff(repo, scope).map_or(0, |d| d.deltas().len()),
    };
    diff.map_or(0, |d| d.deltas().len())
}

/// Resolve `a..b` / `a...b` (git-diff semantics: `...` diffs from the
/// merge base of a and b) or a single revision into a scope.
pub fn rev_scope(repo: &Repository, spec: &str) -> Result<Scope> {
    let (a, b, three_dot) = if let Some((a, b)) = spec.split_once("...") {
        (a, b, true)
    } else if let Some((a, b)) = spec.split_once("..") {
        (a, b, false)
    } else {
        return commit_scope(repo, spec);
    };
    let resolve = |rev: &str| -> Result<Oid> {
        let rev = if rev.is_empty() { "HEAD" } else { rev };
        Ok(repo
            .revparse_single(rev)
            .with_context(|| format!("unknown revision '{rev}'"))?
            .peel_to_commit()
            .map_err(|_| anyhow!("'{rev}' is not a commit"))?
            .id())
    };
    let mut from = resolve(a)?;
    let to = resolve(b)?;
    if three_dot {
        from = repo
            .merge_base(from, to)
            .context("no merge base for '...' range")?;
    }
    Ok(Scope::Range { from, to })
}

/// Resolve a user-supplied revision string into a commit scope.
pub fn commit_scope(repo: &Repository, rev: &str) -> Result<Scope> {
    let commit = repo
        .revparse_single(rev)
        .with_context(|| format!("unknown revision '{rev}'"))?
        .peel_to_commit()
        .map_err(|_| anyhow!("'{rev}' is not a commit"))?;
    Ok(Scope::Commit {
        id: commit.id(),
        summary: commit.summary().unwrap_or(None).unwrap_or("").to_string(),
    })
}

/// Full content of a file as it exists on the *new* side of a scope's diff,
/// plus a short label describing where it came from. Falls back to the old
/// side for deleted files.
pub fn file_content(repo: &Repository, scope: &Scope, path: &str) -> Result<(String, String)> {
    let from_disk = || -> Option<(String, String)> {
        let full = repo.workdir()?.join(path);
        std::fs::read(&full).ok().map(|bytes| {
            (
                String::from_utf8_lossy(&bytes).into_owned(),
                "working tree".into(),
            )
        })
    };
    let from_tree = |tree: &git2::Tree, label: String| -> Option<(String, String)> {
        let entry = tree.get_path(std::path::Path::new(path)).ok()?;
        let blob = repo.find_blob(entry.id()).ok()?;
        Some((String::from_utf8_lossy(blob.content()).into_owned(), label))
    };
    let head_commit_tree = || repo.head().ok().and_then(|h| h.peel_to_tree().ok());

    let content = match scope {
        Scope::Uncommitted => from_disk().or_else(|| {
            // Deleted from the working tree: show what HEAD had.
            head_commit_tree().and_then(|t| from_tree(&t, "HEAD".into()))
        }),
        Scope::Staged => {
            // The staged (index) version, else HEAD's for deletions.
            let from_index = || -> Option<(String, String)> {
                let index = repo.index().ok()?;
                let entry = index.get_path(std::path::Path::new(path), 0)?;
                let blob = repo.find_blob(entry.id).ok()?;
                Some((
                    String::from_utf8_lossy(blob.content()).into_owned(),
                    "index".into(),
                ))
            };
            from_index().or_else(|| head_commit_tree().and_then(|t| from_tree(&t, "HEAD".into())))
        }
        Scope::Branch { base } => head_commit_tree()
            .and_then(|t| from_tree(&t, "HEAD".into()))
            .or_else(|| {
                // Deleted on the branch: show the base's version.
                let tree = repo
                    .revparse_single(base)
                    .ok()?
                    .peel_to_commit()
                    .ok()?
                    .tree()
                    .ok()?;
                from_tree(&tree, base.clone())
            }),
        Scope::Commit { id, .. } => {
            let commit = repo.find_commit(*id).context("finding commit")?;
            from_tree(&commit.tree()?, format!("{id:.7}")).or_else(|| {
                // Deleted by this commit: show the parent's version.
                let parent = commit.parent(0).ok()?;
                from_tree(&parent.tree().ok()?, format!("{:.7}", parent.id()))
            })
        }
        Scope::Range { from, to } => {
            let to_tree = repo.find_commit(*to).context("finding range end")?.tree()?;
            from_tree(&to_tree, format!("{to:.7}")).or_else(|| {
                // Deleted within the range: show the starting version.
                let tree = repo.find_commit(*from).ok()?.tree().ok()?;
                from_tree(&tree, format!("{from:.7}"))
            })
        }
    };
    content.ok_or_else(|| anyhow!("no content found for '{path}' in this scope"))
}

fn head_tree(repo: &Repository) -> Result<Option<git2::Tree<'_>>> {
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_tree().context("resolving HEAD tree")?)),
        // Unborn branch (no commits yet): diff against the empty tree.
        Err(_) => Ok(None),
    }
}
