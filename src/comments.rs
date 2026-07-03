use std::path::PathBuf;

use anyhow::{Context, Result};
use git2::Repository;
use serde::{Deserialize, Serialize};

use crate::git::diff::DiffResult;

/// A review note, GitHub-style: it owns a snippet of the code it was made
/// on, so it can be re-anchored — or shown as "outdated" — but never lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub path: String,
    /// Line numbers refer to the new side (or old side for pure deletions).
    pub new_side: bool,
    /// Inclusive 1-based line range.
    pub lines: (u32, u32),
    /// The lines as displayed when commenting, +/- signs included.
    pub snippet: Vec<String>,
    pub body: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Scope label at creation time; metadata for export only.
    pub scope: String,
}

/// What a comment anchors to in the currently displayed diff.
pub enum Anchor {
    /// After this (hunk, line) of the file.
    Line { hunk: usize, line: usize },
    /// File is in the diff but the anchor lines are not: show at file top.
    FileTop,
}

pub struct Placed {
    pub comment: usize,
    pub file: usize,
    pub anchor: Anchor,
}

/// Anchor each comment against the diff. Comments whose file isn't in the
/// diff at all are simply not placed (they remain in the store and export).
pub fn place(diff: &DiffResult, comments: &[Comment]) -> Vec<Placed> {
    let mut placed = Vec::new();
    for (ci, comment) in comments.iter().enumerate() {
        let Some(fi) = diff.files.iter().position(|f| f.path == comment.path) else {
            continue;
        };
        let file = &diff.files[fi];
        let mut anchor = Anchor::FileTop;
        'hunks: for (hi, hunk) in file.hunks.iter().enumerate() {
            for (li, line) in hunk.lines.iter().enumerate() {
                let lineno = if comment.new_side {
                    line.new_lineno
                } else {
                    line.old_lineno
                };
                if lineno == Some(comment.lines.1) {
                    anchor = Anchor::Line { hunk: hi, line: li };
                    break 'hunks;
                }
            }
        }
        placed.push(Placed {
            comment: ci,
            file: fi,
            anchor,
        });
    }
    placed
}

#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    next_id: u64,
    comments: Vec<Comment>,
}

/// One comment pool per repo, persisted in the gitdir (never the worktree).
pub struct CommentStore {
    path: PathBuf,
    pub comments: Vec<Comment>,
    next_id: u64,
}

impl CommentStore {
    pub fn load(repo: &Repository) -> Self {
        let path = repo.path().join("gittre").join("comments.json");
        let data: StoreFile = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        CommentStore {
            path,
            comments: data.comments,
            next_id: data.next_id.max(1),
        }
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).context("creating comment dir")?;
        }
        let data = StoreFile {
            next_id: self.next_id,
            comments: self.comments.clone(),
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&data)?).context("writing comments")?;
        std::fs::rename(&tmp, &self.path).context("committing comments file")?;
        Ok(())
    }

    pub fn add(
        &mut self,
        path: String,
        new_side: bool,
        lines: (u32, u32),
        snippet: Vec<String>,
        body: String,
        scope: String,
    ) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        self.comments.push(Comment {
            id,
            path,
            new_side,
            lines,
            snippet,
            body,
            created_at: now(),
            scope,
        });
        self.save()
    }

    pub fn edit(&mut self, id: u64, body: String) -> Result<()> {
        if let Some(c) = self.comments.iter_mut().find(|c| c.id == id) {
            c.body = body;
        }
        self.save()
    }

    pub fn delete(&mut self, id: u64) -> Result<()> {
        self.comments.retain(|c| c.id != id);
        self.save()
    }

    pub fn delete_all(&mut self) -> Result<()> {
        self.comments.clear();
        self.save()
    }

    pub fn count_for_path(&self, path: &str) -> usize {
        self.comments.iter().filter(|c| c.path == path).count()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{DiffLine, FileDiff, FileStatus, Hunk};

    fn diff_with(path: &str, new_linenos: &[u32]) -> DiffResult {
        DiffResult {
            files: vec![FileDiff {
                path: path.into(),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                hunks: vec![Hunk {
                    header: "@@ @@".into(),
                    lines: new_linenos
                        .iter()
                        .map(|&n| DiffLine {
                            origin: '+',
                            old_lineno: None,
                            new_lineno: Some(n),
                            content: format!("line {n}"),
                        })
                        .collect(),
                }],
                additions: new_linenos.len(),
                deletions: 0,
            }],
            additions: 0,
            deletions: 0,
        }
    }

    fn comment(path: &str, end_line: u32) -> Comment {
        Comment {
            id: 1,
            path: path.into(),
            new_side: true,
            lines: (end_line, end_line),
            snippet: vec![format!("+line {end_line}")],
            body: "note".into(),
            created_at: 0,
            scope: "test".into(),
        }
    }

    #[test]
    fn places_on_matching_line() {
        let diff = diff_with("a.rs", &[10, 11, 12]);
        let placed = place(&diff, &[comment("a.rs", 11)]);
        assert_eq!(placed.len(), 1);
        assert!(matches!(
            placed[0].anchor,
            Anchor::Line { hunk: 0, line: 1 }
        ));
    }

    #[test]
    fn falls_back_to_file_top_when_line_missing() {
        let diff = diff_with("a.rs", &[10, 11]);
        let placed = place(&diff, &[comment("a.rs", 99)]);
        assert!(matches!(placed[0].anchor, Anchor::FileTop));
    }

    #[test]
    fn skips_files_not_in_diff() {
        let diff = diff_with("a.rs", &[10]);
        assert!(place(&diff, &[comment("other.rs", 10)]).is_empty());
    }

    #[test]
    fn store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut store = CommentStore::load(&repo);
        store
            .add(
                "a.rs".into(),
                true,
                (3, 5),
                vec!["+x".into()],
                "hello".into(),
                "uncommitted changes".into(),
            )
            .unwrap();
        let reloaded = CommentStore::load(&repo);
        assert_eq!(reloaded.comments.len(), 1);
        assert_eq!(reloaded.comments[0].body, "hello");
        assert_eq!(reloaded.comments[0].id, 1);

        let mut store = reloaded;
        store.delete(1).unwrap();
        assert!(CommentStore::load(&repo).comments.is_empty());
    }
}
