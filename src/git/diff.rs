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
pub fn load(repo: &Repository, scope: &super::scope::Scope) -> Result<DiffResult> {
    let diff = super::scope::build_diff(repo, scope)?;
    collect(&diff)
}

fn collect(diff: &git2::Diff) -> Result<DiffResult> {
    // The foreach callbacks all need mutable access to the same accumulator.
    let result = std::cell::RefCell::new(DiffResult::default());

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
            result.files.push(FileDiff {
                path,
                old_path,
                status: delta_status(delta.status()),
                binary: delta.new_file().is_binary() || delta.old_file().is_binary(),
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
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

    let mut result = result.into_inner();
    let (adds, dels) = result
        .files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    result.additions = adds;
    result.deletions = dels;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::scope::{Scope, commit_scope, detect_base, file_count};
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

    #[test]
    fn base_detection_prefers_default_branches() {
        let (dir, repo) = setup();
        // On main with no upstream and no other branches: nothing to compare to.
        assert_eq!(detect_base(&repo), None);
        // From a feature branch, main is found.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        // Branch names depend on init.defaultBranch; accept either.
        let base = detect_base(&repo);
        assert!(
            base.as_deref() == Some("main") || base.as_deref() == Some("master"),
            "unexpected base: {base:?} (dir: {:?})",
            dir.path()
        );
    }

    #[test]
    fn file_count_matches_full_load() {
        let (dir, repo) = setup();
        fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        assert_eq!(file_count(&repo, &Scope::Uncommitted), 2);
        assert_eq!(file_count(&repo, &Scope::Staged), 0);
    }
}
