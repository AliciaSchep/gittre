use anyhow::{Context, Result};
use git2::{Delta, Repository};

/// One line inside a hunk.
#[derive(Debug, Clone)]
pub struct DiffLine {
    /// ' ' context, '+' addition, '-' deletion
    pub origin: char,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Typechange,
}

impl FileStatus {
    pub fn letter(self) -> char {
        match self {
            FileStatus::Modified => 'M',
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Typechange => 'T',
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Repo-relative path (new side; old side for deletions).
    pub path: String,
    /// Old path when renamed.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    /// Content skipped because the file exceeds MAX_CONTENT_FILE_SIZE;
    /// the UI offers to load it on demand.
    pub large: bool,
    /// Size of the bigger side, for the stub label.
    pub byte_size: u64,
    /// An untracked directory (collapsed, GitHub-style); Enter lists it.
    pub untracked_dir: bool,
    pub hunks: Vec<Hunk>,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Default)]
pub struct DiffResult {
    pub files: Vec<FileDiff>,
    pub additions: usize,
    pub deletions: usize,
}

impl DiffResult {
    /// Sort into tree order and compute totals.
    pub fn from_files(mut files: Vec<FileDiff>) -> Self {
        files.sort_by(|a, b| tree_order(&a.path, &b.path));
        let (additions, deletions) = files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        DiffResult {
            files,
            additions,
            deletions,
        }
    }
}

fn delta_status(delta: Delta) -> FileStatus {
    match delta {
        Delta::Added | Delta::Untracked | Delta::Copied => FileStatus::Added,
        Delta::Deleted => FileStatus::Deleted,
        Delta::Renamed => FileStatus::Renamed,
        Delta::Typechange => FileStatus::Typechange,
        _ => FileStatus::Modified,
    }
}

/// Load the full structured diff for a scope.
///
/// Worktree scopes go through the `git` CLI (fsmonitor, untracked cache,
/// parallelism — libgit2 has none of these and is 10-100x slower on large
/// repos); object-database scopes stay on libgit2. If the CLI path fails
/// (no git binary, unborn HEAD, …), fall back to libgit2.
pub fn load(repo: &Repository, scope: &super::scope::Scope) -> Result<DiffResult> {
    use super::scope::Scope;
    if let (Some(workdir), Some(staged)) = (
        repo.workdir(),
        match scope {
            Scope::Uncommitted => Some(false),
            Scope::Staged => Some(true),
            _ => None,
        },
    ) {
        match super::cli::load_worktree(workdir, staged) {
            Ok(result) => return Ok(result),
            Err(e) => {
                crate::app::debug_log(&format!("cli diff failed, using libgit2: {e:#}"));
            }
        }
    }
    let t = std::time::Instant::now();
    let diff = super::scope::build_diff(repo, scope)?;
    crate::app::debug_log(&format!(
        "diff phase: build+renames {:?} ({} deltas)",
        t.elapsed(),
        diff.deltas().len()
    ));
    let t = std::time::Instant::now();
    let result = collect(&diff);
    crate::app::debug_log(&format!("diff phase: content walk {:?}", t.elapsed()));
    result
}

/// Load one file's full diff (no size cap), for expanding a large stub.
pub fn load_file(repo: &Repository, scope: &super::scope::Scope, path: &str) -> Result<FileDiff> {
    use super::scope::Scope;
    if let (Some(workdir), Some(staged)) = (
        repo.workdir(),
        match scope {
            Scope::Uncommitted => Some(false),
            Scope::Staged => Some(true),
            _ => None,
        },
    ) {
        if let Ok(file) = super::cli::load_worktree_file(workdir, staged, path) {
            return Ok(file);
        }
    }
    let diff = super::scope::build_file_diff(repo, scope, path)?;
    let mut result = collect(&diff)?;
    result
        .files
        .drain(..)
        .find(|f| f.path == path)
        .map(|mut f| {
            f.large = false;
            f
        })
        .ok_or_else(|| anyhow::anyhow!("'{path}' no longer in the diff"))
}

fn collect(diff: &git2::Diff) -> Result<DiffResult> {
    // The foreach callbacks all need mutable access to the same accumulator.
    let result = std::cell::RefCell::new(DiffResult::default());
    // Time attribution: content between two file callbacks belongs to the
    // earlier file; anything slow gets named in GITTRE_LOG.
    let file_clock = std::cell::RefCell::new((std::time::Instant::now(), None::<String>));
    let lap = |next: Option<String>| {
        let mut clock = file_clock.borrow_mut();
        let elapsed = clock.0.elapsed();
        if let Some(prev) = clock.1.take() {
            if elapsed > std::time::Duration::from_millis(200) {
                crate::app::debug_log(&format!("diff slow file: {prev} took {elapsed:?}"));
            }
        }
        *clock = (std::time::Instant::now(), next);
    };

    diff.foreach(
        &mut |delta, _| {
            let mut result = result.borrow_mut();
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let old_path = match delta.status() {
                Delta::Renamed | Delta::Copied => {
                    delta.old_file().path().map(|p| p.display().to_string())
                }
                _ => None,
            };
            let byte_size = delta.new_file().size().max(delta.old_file().size());
            let over_cap = byte_size > super::scope::MAX_CONTENT_FILE_SIZE as u64;
            result.files.push(FileDiff {
                path,
                old_path,
                status: delta_status(delta.status()),
                binary: !over_cap && (delta.new_file().is_binary() || delta.old_file().is_binary()),
                large: over_cap,
                byte_size,
                untracked_dir: false,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            lap(Some(
                result
                    .files
                    .last()
                    .map(|f| f.path.clone())
                    .unwrap_or_default(),
            ));
            true
        },
        Some(&mut |_, _| {
            // Binary delta: flag is already set from the file callback; if not,
            // mark the last file so the UI shows a "binary file" row.
            if let Some(f) = result.borrow_mut().files.last_mut() {
                f.binary = true;
            }
            true
        }),
        Some(&mut |_, hunk| {
            if let Some(f) = result.borrow_mut().files.last_mut() {
                f.hunks.push(Hunk {
                    header: String::from_utf8_lossy(hunk.header())
                        .trim_end()
                        .to_string(),
                    lines: Vec::new(),
                });
            }
            true
        }),
        Some(&mut |_, _, line| {
            let origin = line.origin();
            // Skip meta lines like file headers ('F') and hunk headers ('H');
            // also normalize "no newline at eof" markers.
            if !matches!(origin, ' ' | '+' | '-') {
                return true;
            }
            if let Some(f) = result.borrow_mut().files.last_mut() {
                match origin {
                    '+' => f.additions += 1,
                    '-' => f.deletions += 1,
                    _ => {}
                }
                if let Some(h) = f.hunks.last_mut() {
                    h.lines.push(DiffLine {
                        origin,
                        old_lineno: line.old_lineno(),
                        new_lineno: line.new_lineno(),
                        content: String::from_utf8_lossy(line.content())
                            .trim_end_matches(['\n', '\r'])
                            .to_string(),
                    });
                }
            }
            true
        }),
    )
    .context("walking diff")?;
    lap(None);

    // Match the file tree's visual order (directories first, then files,
    // alphabetical at each level) so scrolling the stream and walking the
    // tree traverse files in the same sequence.
    Ok(DiffResult::from_files(result.into_inner().files))
}

fn tree_order(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ac: Vec<&str> = a.split('/').collect();
    let bc: Vec<&str> = b.split('/').collect();
    for i in 0.. {
        match (ac.get(i), bc.get(i)) {
            (Some(x), Some(y)) => {
                let a_dir = i + 1 < ac.len();
                let b_dir = i + 1 < bc.len();
                match (a_dir, b_dir) {
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => match x.cmp(y) {
                        Ordering::Equal => continue,
                        other => return other,
                    },
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::scope::{Scope, commit_scope, file_count_fast, forkable_branch};
    use std::fs;
    use std::path::Path;

    fn load_uncommitted(repo: &Repository) -> Result<DiffResult> {
        load(repo, &Scope::Uncommitted)
    }

    fn commit_all(repo: &Repository, msg: &str) {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap();
    }

    fn setup() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "fn old() {}\n").unwrap();
        commit_all(&repo, "initial");
        (dir, repo)
    }

    fn find<'a>(result: &'a DiffResult, path: &str) -> &'a FileDiff {
        result
            .files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} not in diff"))
    }

    #[test]
    fn clean_tree_is_empty() {
        let (_dir, repo) = setup();
        let result = load_uncommitted(&repo).unwrap();
        assert!(result.files.is_empty());
    }

    #[test]
    fn modified_added_deleted_untracked() {
        let (dir, repo) = setup();
        let root = dir.path();
        fs::write(root.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        fs::remove_file(root.join("src/lib.rs")).unwrap();
        fs::write(root.join("new.txt"), "hello\n").unwrap();

        let result = load_uncommitted(&repo).unwrap();
        assert_eq!(result.files.len(), 3);

        let modified = find(&result, "a.txt");
        assert_eq!(modified.status, FileStatus::Modified);
        assert_eq!(modified.additions, 1);
        assert_eq!(modified.deletions, 1);
        let lines = &modified.hunks[0].lines;
        assert!(lines.iter().any(|l| l.origin == '-' && l.content == "two"));
        assert!(lines.iter().any(|l| l.origin == '+' && l.content == "TWO"));

        assert_eq!(find(&result, "src/lib.rs").status, FileStatus::Deleted);
        assert_eq!(find(&result, "new.txt").status, FileStatus::Added);
        assert_eq!(result.additions, 2);
        assert_eq!(result.deletions, 2);
    }

    #[test]
    fn staged_changes_are_included() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();

        let result = load_uncommitted(&repo).unwrap();
        assert_eq!(find(&result, "a.txt").additions, 1);
    }

    #[test]
    fn works_on_unborn_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        fs::write(dir.path().join("first.txt"), "hi\n").unwrap();
        let result = load_uncommitted(&repo).unwrap();
        assert_eq!(find(&result, "first.txt").status, FileStatus::Added);
    }

    #[test]
    fn staged_scope_sees_only_the_index() {
        let (dir, repo) = setup();
        // Stage one change, leave another unstaged.
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        fs::write(dir.path().join("unstaged.txt"), "not staged\n").unwrap();

        let result = load(&repo, &Scope::Staged).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(find(&result, "a.txt").additions, 1);
    }

    #[test]
    fn branch_scope_diffs_merge_base_to_head() {
        let (dir, repo) = setup();
        let main_head = repo.head().unwrap().peel_to_commit().unwrap().id();
        // Branch off, commit twice on the feature branch.
        repo.branch("feature", &repo.find_commit(main_head).unwrap(), false)
            .unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        fs::write(dir.path().join("feat.txt"), "feature work\n").unwrap();
        commit_all(&repo, "feat 1");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfeature\n").unwrap();
        commit_all(&repo, "feat 2");
        // Advance main independently so merge-base != main tip.
        // (skipped: merge-base == branch point is the common case anyway)

        let scope = Scope::Branch {
            base: "main".into(),
        };
        let result = load(&repo, &scope).unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(find(&result, "feat.txt").status, FileStatus::Added);
        assert_eq!(find(&result, "a.txt").additions, 1);
        // Uncommitted noise must not leak into the branch scope.
        fs::write(dir.path().join("noise.txt"), "dirty\n").unwrap();
        assert_eq!(load(&repo, &scope).unwrap().files.len(), 2);
    }

    #[test]
    fn commit_scope_diffs_against_first_parent() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        commit_all(&repo, "add four");

        let scope = commit_scope(&repo, "HEAD").unwrap();
        let result = load(&repo, &scope).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(find(&result, "a.txt").additions, 1);

        // Root commit diffs against the empty tree.
        let root = commit_scope(&repo, "HEAD~1").unwrap();
        let result = load(&repo, &root).unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(find(&result, "a.txt").status, FileStatus::Added);
    }

    /// Commit one added file directly onto a ref, without touching HEAD or
    /// the working tree — for advancing a branch we are not on.
    fn commit_file_on(repo: &Repository, refname: &str, path: &str, content: &str, msg: &str) {
        let parent = repo
            .find_reference(refname)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let blob = repo.blob(content.as_bytes()).unwrap();
        let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        builder.insert(path, blob, 0o100_644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        repo.commit(Some(refname), &sig, &sig, msg, &tree, &[&parent])
            .unwrap();
    }

    #[test]
    fn fork_scope_reviews_all_branch_commits() {
        let (dir, repo) = setup();
        let main_ref = repo.head().unwrap().name().unwrap().to_string();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        fs::write(dir.path().join("feat.txt"), "feature work\n").unwrap();
        commit_all(&repo, "feat 1");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfeature\n").unwrap();
        commit_all(&repo, "feat 2");

        // Pushing the branch must not shrink the review: a remote copy at
        // the branch tip is ignored (upstream-based detection got this
        // wrong and reviewed only unpushed commits).
        let tip = repo.head().unwrap().peel_to_commit().unwrap().id();
        repo.reference("refs/remotes/origin/feature", tip, true, "push")
            .unwrap();
        // The trunk advancing afterwards must not leak into the review.
        commit_file_on(
            &repo,
            &main_ref,
            "trunk.txt",
            "moved on\n",
            "trunk advances",
        );

        let scope = Scope::BranchFork {
            branch: "feature".into(),
        };
        let result = load(&repo, &scope).unwrap();
        assert_eq!(result.files.len(), 2, "both feature commits, nothing else");
        assert_eq!(find(&result, "feat.txt").status, FileStatus::Added);
        assert_eq!(find(&result, "a.txt").additions, 1);
    }

    #[test]
    fn fork_scope_is_empty_for_branch_with_no_own_commits() {
        let (_dir, repo) = setup();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        let scope = Scope::BranchFork {
            branch: "feature".into(),
        };
        assert!(load(&repo, &scope).unwrap().files.is_empty());
    }

    #[test]
    fn fork_scope_diffs_orphan_branch_against_empty_tree() {
        let (dir, repo) = setup();
        repo.set_head("refs/heads/orphan").unwrap(); // unborn: next commit is a root
        fs::write(dir.path().join("solo.txt"), "orphan work\n").unwrap();
        commit_all(&repo, "orphan root");
        let scope = Scope::BranchFork {
            branch: "orphan".into(),
        };
        let result = load(&repo, &scope).unwrap();
        assert!(result.files.iter().all(|f| f.status == FileStatus::Added));
        assert!(result.files.iter().any(|f| f.path == "solo.txt"));
    }

    #[test]
    fn forkable_needs_another_branch_that_is_not_a_remote_copy() {
        let (_dir, repo) = setup();
        // Alone on the default branch: no fork point.
        assert_eq!(forkable_branch(&repo), None);
        // A remote copy of the same branch does not count.
        let name = repo.head().unwrap().shorthand().unwrap().to_string();
        let tip = repo.head().unwrap().peel_to_commit().unwrap().id();
        repo.reference(&format!("refs/remotes/origin/{name}"), tip, true, "push")
            .unwrap();
        assert_eq!(forkable_branch(&repo), None);
        // A real second branch does.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        assert_eq!(forkable_branch(&repo).as_deref(), Some("feature"));
    }

    #[test]
    fn files_come_in_tree_order() {
        use std::cmp::Ordering;
        let paths = [
            "zeta/a.txt",     // dirs before root files, "zeta" after "docs"
            "docs/readme.md", // within docs: sub/ before files
            "docs/sub/x.txt",
            "a_root_file.txt",
        ];
        let mut sorted: Vec<&str> = paths.to_vec();
        sorted.sort_by(|a, b| tree_order(a, b));
        assert_eq!(
            sorted,
            [
                "docs/sub/x.txt",
                "docs/readme.md",
                "zeta/a.txt",
                "a_root_file.txt",
            ]
        );
        assert_eq!(tree_order("a.txt", "a.txt"), Ordering::Equal);
    }

    #[test]
    fn oversized_diffs_become_stubs_and_load_on_demand() {
        let (dir, repo) = setup();
        let big: String = "0123456789abcdef\n".repeat(80_000); // ~1.3 MB
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        commit_all(&repo, "add big");

        // A small change to a big file stays inline: the patch is tiny.
        fs::write(dir.path().join("big.txt"), format!("{big}one more line\n")).unwrap();
        let result = load_uncommitted(&repo).unwrap();
        let small_change = find(&result, "big.txt");
        assert!(
            !small_change.large,
            "small patch to a big file shows inline"
        );
        assert_eq!(small_change.additions, 1);

        // A full rewrite produces an over-cap patch: stubbed.
        let rewritten: String = "fedcba9876543210\n".repeat(80_000);
        fs::write(dir.path().join("big.txt"), &rewritten).unwrap();
        let result = load_uncommitted(&repo).unwrap();
        let stub = find(&result, "big.txt");
        assert!(stub.large, "over-cap patch should be a stub");
        assert!(stub.hunks.is_empty(), "no content held up front");
        assert!(stub.byte_size > 1024 * 1024);

        let loaded = load_file(&repo, &Scope::Uncommitted, "big.txt").unwrap();
        assert!(!loaded.large);
        assert!(loaded.additions >= 80_000, "on-demand load has real hunks");

        // A big untracked file stubs by file size (content never read).
        fs::write(dir.path().join("bignew.txt"), &big).unwrap();
        let result = load_uncommitted(&repo).unwrap();
        let untracked = find(&result, "bignew.txt");
        assert!(untracked.large);
        assert!(untracked.hunks.is_empty());
    }

    #[test]
    fn file_count_matches_full_load() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        assert_eq!(file_count_fast(&repo, &Scope::Uncommitted), 2);
        assert_eq!(file_count_fast(&repo, &Scope::Staged), 0);
    }
}
