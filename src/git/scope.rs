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
        }
    }

    /// Message shown when the scope has no changes.
    pub fn empty_message(&self) -> &'static str {
        match self {
            Scope::Uncommitted => "no uncommitted changes — working tree clean",
            Scope::Staged => "no staged changes",
            Scope::Branch { .. } => "no changes vs base — branch has no commits of its own",
            Scope::Commit { .. } => "empty commit",
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

/// Build the raw git2 diff for a scope. Shared by the full loader (which walks
/// content) and the picker's file counts (which only look at deltas).
pub fn build_diff<'r>(repo: &'r Repository, scope: &Scope) -> Result<git2::Diff<'r>> {
    let mut opts = DiffOptions::new();
    opts.include_typechange(true);

    let mut diff = match scope {
        Scope::Uncommitted => {
            opts.include_untracked(true)
                .recurse_untracked_dirs(true)
                .show_untracked_content(true);
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
    };

    diff.find_similar(None).ok();
    Ok(diff)
}

/// Number of changed files in a scope, without walking file contents.
pub fn file_count(repo: &Repository, scope: &Scope) -> usize {
    build_diff(repo, scope).map_or(0, |d| d.deltas().len())
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

fn head_tree(repo: &Repository) -> Result<Option<git2::Tree<'_>>> {
    match repo.head() {
        Ok(head) => Ok(Some(head.peel_to_tree().context("resolving HEAD tree")?)),
        // Unborn branch (no commits yet): diff against the empty tree.
        Err(_) => Ok(None),
    }
}
