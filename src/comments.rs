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
    /// Set when the last re-anchor failed; the snippet preserves context.
    #[serde(default)]
    pub outdated: bool,
}

/// What a comment anchors to in the currently displayed diff.
pub enum Anchor {
    /// After this (hunk, line) of the file.
    Line { hunk: usize, line: usize },
    /// Re-anchoring failed: shown at the top of the file's section with the
    /// preserved snippet as context (GitHub's "outdated" state).
    Outdated,
}

pub struct Placed {
    pub comment: usize,
    pub file: usize,
    pub anchor: Anchor,
}

/// Strip the +/-/space sign a snippet line was stored with.
fn snippet_text(line: &str) -> &str {
    line.strip_prefix(['+', '-', ' ']).unwrap_or(line)
}

/// Anchor each comment against the diff with a GitHub-style cascade:
/// exact (anchor line still has the snippet content) → moved (same content
/// found elsewhere in the file's diff; stored lines updated) → outdated.
/// Comments whose file isn't in the diff at all are simply not placed (they
/// remain in the store and export). Returns placements and whether any
/// comment was mutated (moved or outdated-flag changed) and needs saving.
pub fn reanchor(diff: &DiffResult, comments: &mut [Comment]) -> (Vec<Placed>, bool) {
    let mut placed = Vec::new();
    let mut changed = false;
    for (ci, comment) in comments.iter_mut().enumerate() {
        let Some(fi) = diff.files.iter().position(|f| f.path == comment.path) else {
            continue;
        };
        let file = &diff.files[fi];
        // Only lines that exist on the comment's chosen side participate in
        // anchoring. A selection spanning a replacement can preserve both
        // +/- sides for display while still matching one coherent file view.
        let target: Vec<String> = comment
            .snippet
            .iter()
            .filter(|line| match line.chars().next() {
                Some('+') => comment.new_side,
                Some('-') => !comment.new_side,
                _ => true,
            })
            .map(|line| snippet_text(line).to_string())
            .collect();

        // Every diff line of this file on the comment's side.
        let mut candidates: Vec<(u32, usize, usize, &str)> = Vec::new(); // (lineno, hunk, line, content)
        for (hi, hunk) in file.hunks.iter().enumerate() {
            for (li, line) in hunk.lines.iter().enumerate() {
                let lineno = if comment.new_side {
                    line.new_lineno
                } else {
                    line.old_lineno
                };
                if let Some(n) = lineno {
                    candidates.push((n, hi, li, line.content.as_str()));
                }
            }
        }

        // Match the whole same-side snippet, not just its final line. Of the
        // matching sequences, an exact end line wins; otherwise the nearest
        // moved occurrence wins. All-blank snippets move too promiscuously.
        let mut exact: Option<(u32, usize, usize, &str)> = None;
        let mut moved: Option<(u32, usize, usize, &str)> = None;
        let movable = target.iter().any(|line| !line.trim().is_empty());
        if !target.is_empty() {
            for window in candidates.windows(target.len()) {
                let contiguous = window.windows(2).all(|pair| {
                    pair[0].1 == pair[1].1 && pair[0].0.checked_add(1) == Some(pair[1].0)
                });
                if !contiguous
                    || !window
                        .iter()
                        .zip(&target)
                        .all(|((_, _, _, content), expected)| *content == expected)
                {
                    continue;
                }
                let &(n, hi, li, content) = window.last().expect("non-empty target window");
                let candidate = (n, hi, li, content);
                if n == comment.lines.1 {
                    exact = Some(candidate);
                    break;
                }
                if movable
                    && moved.as_ref().is_none_or(|(best, _, _, _)| {
                        n.abs_diff(comment.lines.1) < best.abs_diff(comment.lines.1)
                    })
                {
                    moved = Some(candidate);
                }
            }
        }
        let hit = exact.or(moved);

        let anchor = match hit {
            Some((n, hi, li, _)) => {
                if n != comment.lines.1 {
                    let delta = i64::from(n) - i64::from(comment.lines.1);
                    comment.lines.0 = (i64::from(comment.lines.0) + delta).max(1) as u32;
                    comment.lines.1 = n;
                    changed = true;
                }
                if comment.outdated {
                    comment.outdated = false;
                    changed = true;
                }
                Anchor::Line { hunk: hi, line: li }
            }
            None => {
                if !comment.outdated {
                    comment.outdated = true;
                    changed = true;
                }
                Anchor::Outdated
            }
        };
        placed.push(Placed {
            comment: ci,
            file: fi,
            anchor,
        });
    }
    (placed, changed)
}

#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    next_id: u64,
    comments: Vec<Comment>,
}

/// One comment pool per repo, persisted in the gitdir (never the worktree).
pub struct CommentStore {
    path: PathBuf,
    comments: Vec<Comment>,
    next_id: u64,
}

impl CommentStore {
    pub fn load(repo: &Repository) -> Result<Self> {
        let path = repo.path().join("gittre").join("comments.json");
        let data: StoreFile = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let next_id = data
            .comments
            .iter()
            .map(|comment| comment.id.saturating_add(1))
            .max()
            .unwrap_or(1)
            .max(data.next_id)
            .max(1);
        Ok(CommentStore {
            path,
            comments: data.comments,
            next_id,
        })
    }

    fn save_state(&self, comments: &[Comment], next_id: u64) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).context("creating comment dir")?;
        }
        let data = StoreFile {
            next_id,
            comments: comments.to_vec(),
        };
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&data)?).context("writing comments")?;
        std::fs::rename(&tmp, &self.path).context("committing comments file")?;
        Ok(())
    }

    /// Persist a proposed state before exposing it in memory, so a failed
    /// write leaves the live store exactly as it was.
    fn commit_state(&mut self, comments: Vec<Comment>, next_id: u64) -> Result<()> {
        self.save_state(&comments, next_id)?;
        self.comments = comments;
        self.next_id = next_id;
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
        let next_id = id.checked_add(1).context("comment id space exhausted")?;
        let mut comments = self.comments.clone();
        comments.push(Comment {
            id,
            path,
            new_side,
            lines,
            snippet,
            body,
            created_at: now(),
            scope,
            outdated: false,
        });
        self.commit_state(comments, next_id)
    }

    pub fn edit(&mut self, id: u64, body: String) -> Result<()> {
        let mut comments = self.comments.clone();
        let Some(c) = comments.iter_mut().find(|c| c.id == id) else {
            return Ok(());
        };
        c.body = body;
        self.commit_state(comments, self.next_id)
    }

    pub fn delete(&mut self, id: u64) -> Result<()> {
        let mut comments = self.comments.clone();
        let before = comments.len();
        comments.retain(|c| c.id != id);
        if comments.len() == before {
            return Ok(());
        }
        self.commit_state(comments, self.next_id)
    }

    pub fn delete_all(&mut self) -> Result<()> {
        if self.comments.is_empty() {
            return Ok(());
        }
        self.commit_state(Vec::new(), self.next_id)
    }

    /// Reinsert comments removed by a delete (the `u` undo). Ids were minted
    /// by this store and stay unique because next_id never decreases.
    pub fn restore(&mut self, mut comments: Vec<Comment>) -> Result<()> {
        if comments.is_empty() {
            return Ok(());
        }
        let mut restored = self.comments.clone();
        restored.append(&mut comments);
        self.commit_state(restored, self.next_id)
    }

    /// Re-anchor all comments against a diff, persisting any moves or
    /// outdated-state changes.
    pub fn reanchor(&mut self, diff: &DiffResult) -> Result<Vec<Placed>> {
        let mut comments = self.comments.clone();
        let (placed, changed) = reanchor(diff, &mut comments);
        if changed {
            self.commit_state(comments, self.next_id)?;
        }
        Ok(placed)
    }

    pub fn count_for_path(&self, path: &str) -> usize {
        self.comments.iter().filter(|c| c.path == path).count()
    }

    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }
}

/// Render all comments as markdown, grouped by file, ordered by line.
/// Outdated comments are flagged; the preserved snippet gives context either
/// way.
pub fn export_markdown(comments: &[Comment], title: &str, date: &str) -> String {
    let mut sorted: Vec<&Comment> = comments.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path).then(a.lines.0.cmp(&b.lines.0)));

    let mut out = format!("# Review comments — {title} ({date})\n");
    let mut current_path = "";
    for c in sorted {
        if c.path != current_path {
            current_path = &c.path;
            out.push_str(&format!("\n## {}\n", c.path));
        }
        let range = if c.lines.0 == c.lines.1 {
            format!("L{}", c.lines.0)
        } else {
            format!("L{}–L{}", c.lines.0, c.lines.1)
        };
        let side = if c.new_side { "" } else { ", old side" };
        let outdated = if c.outdated { " · ⚠ outdated" } else { "" };
        out.push_str(&format!(
            "\n**{range}**{side} · _{}_{outdated}\n\n",
            c.scope
        ));
        if !c.snippet.is_empty() {
            out.push_str("```diff\n");
            for line in &c.snippet {
                out.push_str(line);
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
        for line in c.body.lines() {
            out.push_str(&format!("> {line}\n"));
        }
    }
    out
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
    use std::fs;

    fn diff_with(path: &str, new_linenos: &[u32]) -> DiffResult {
        DiffResult {
            files: vec![FileDiff {
                path: path.into(),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                large: false,
                byte_size: 0,
                untracked_dir: false,
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
            outdated: false,
        }
    }

    #[test]
    fn exact_anchor_stays_put() {
        let diff = diff_with("a.rs", &[10, 11, 12]);
        let mut comments = [comment("a.rs", 11)];
        let (placed, changed) = reanchor(&diff, &mut comments);
        assert!(matches!(
            placed[0].anchor,
            Anchor::Line { hunk: 0, line: 1 }
        ));
        assert!(!changed);
        assert_eq!(comments[0].lines, (11, 11));
    }

    #[test]
    fn moved_content_updates_stored_lines() {
        // "line 11" now lives at line 14 (content is keyed by the snippet).
        let diff = DiffResult {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                large: false,
                byte_size: 0,
                untracked_dir: false,
                hunks: vec![Hunk {
                    header: "@@ @@".into(),
                    lines: [(13, "line 10"), (14, "line 11"), (15, "line 12")]
                        .iter()
                        .map(|&(n, c)| DiffLine {
                            origin: '+',
                            old_lineno: None,
                            new_lineno: Some(n),
                            content: c.into(),
                        })
                        .collect(),
                }],
                additions: 3,
                deletions: 0,
            }],
            additions: 0,
            deletions: 0,
        };
        let mut comments = [comment("a.rs", 11)];
        let (placed, changed) = reanchor(&diff, &mut comments);
        assert!(matches!(
            placed[0].anchor,
            Anchor::Line { hunk: 0, line: 1 }
        ));
        assert!(changed);
        assert_eq!(comments[0].lines, (14, 14), "range shifted with the move");
        assert!(!comments[0].outdated);
    }

    #[test]
    fn multi_line_anchor_matches_the_whole_same_side_snippet() {
        let diff = DiffResult {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                large: false,
                byte_size: 0,
                untracked_dir: false,
                hunks: vec![Hunk {
                    header: "@@ @@".into(),
                    lines: [(20, "other"), (21, "}"), (30, "unique"), (31, "}")]
                        .into_iter()
                        .map(|(n, content)| DiffLine {
                            origin: '+',
                            old_lineno: None,
                            new_lineno: Some(n),
                            content: content.into(),
                        })
                        .collect(),
                }],
                additions: 4,
                deletions: 0,
            }],
            additions: 4,
            deletions: 0,
        };
        let mut c = comment("a.rs", 21);
        c.lines = (20, 21);
        c.snippet = vec!["+unique".into(), "+}".into()];
        let mut comments = [c];

        let (placed, changed) = reanchor(&diff, &mut comments);

        assert!(matches!(
            placed[0].anchor,
            Anchor::Line { hunk: 0, line: 3 }
        ));
        assert!(changed);
        assert_eq!(comments[0].lines, (30, 31));
    }

    #[test]
    fn multi_line_anchor_never_spans_hunks_or_line_gaps() {
        let diff = DiffResult {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                large: false,
                byte_size: 0,
                untracked_dir: false,
                hunks: vec![
                    Hunk {
                        header: "@@ -40 +40 @@".into(),
                        lines: vec![DiffLine {
                            origin: '+',
                            old_lineno: None,
                            new_lineno: Some(40),
                            content: "foo".into(),
                        }],
                    },
                    Hunk {
                        header: "@@ -400 +400 @@".into(),
                        lines: vec![DiffLine {
                            origin: '+',
                            old_lineno: None,
                            new_lineno: Some(400),
                            content: "bar".into(),
                        }],
                    },
                ],
                additions: 2,
                deletions: 0,
            }],
            additions: 2,
            deletions: 0,
        };
        let mut c = comment("a.rs", 11);
        c.lines = (10, 11);
        c.snippet = vec!["+foo".into(), "+bar".into()];
        let mut comments = [c];

        let (placed, changed) = reanchor(&diff, &mut comments);

        assert!(matches!(placed[0].anchor, Anchor::Outdated));
        assert!(changed);
        assert_eq!(comments[0].lines, (10, 11));
    }

    #[test]
    fn vanished_content_goes_outdated_and_can_recover() {
        let diff = diff_with("a.rs", &[10, 11]);
        let mut comments = [comment("a.rs", 99)];
        let (placed, changed) = reanchor(&diff, &mut comments);
        assert!(matches!(placed[0].anchor, Anchor::Outdated));
        assert!(changed);
        assert!(comments[0].outdated);

        // The content comes back: comment recovers to live.
        let diff = diff_with("a.rs", &[97, 98, 99]);
        let (placed, changed) = reanchor(&diff, &mut comments);
        assert!(matches!(placed[0].anchor, Anchor::Line { .. }));
        assert!(changed, "outdated flag cleared");
        assert!(!comments[0].outdated);
    }

    #[test]
    fn blank_snippet_never_matches_by_content() {
        let mut c = comment("a.rs", 99);
        c.snippet = vec!["+".into()];
        let diff = DiffResult {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                large: false,
                byte_size: 0,
                untracked_dir: false,
                hunks: vec![Hunk {
                    header: "@@ @@".into(),
                    lines: vec![DiffLine {
                        origin: '+',
                        old_lineno: None,
                        new_lineno: Some(5),
                        content: String::new(),
                    }],
                }],
                additions: 1,
                deletions: 0,
            }],
            additions: 0,
            deletions: 0,
        };
        let mut comments = [c];
        let (placed, _) = reanchor(&diff, &mut comments);
        assert!(matches!(placed[0].anchor, Anchor::Outdated));
    }

    #[test]
    fn skips_files_not_in_diff() {
        let diff = diff_with("a.rs", &[10]);
        let mut comments = [comment("other.rs", 10)];
        let (placed, _) = reanchor(&diff, &mut comments);
        assert!(placed.is_empty());
        assert!(!comments[0].outdated, "untouched when file absent");
    }

    #[test]
    fn export_groups_and_flags() {
        let mut a = comment("b.rs", 5);
        a.body = "second file".into();
        let mut b = comment("a.rs", 9);
        b.id = 2;
        b.body = "line one\nline two".into();
        b.outdated = true;
        let md = export_markdown(&[a, b], "demo", "2026-07-03");
        let a_pos = md.find("## a.rs").unwrap();
        let b_pos = md.find("## b.rs").unwrap();
        assert!(a_pos < b_pos, "files sorted");
        assert!(md.contains("⚠ outdated"));
        assert!(md.contains("```diff\n+line 9\n```"));
        assert!(md.contains("> line one\n> line two"));
    }

    #[test]
    fn store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut store = CommentStore::load(&repo).unwrap();
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
        let reloaded = CommentStore::load(&repo).unwrap();
        assert_eq!(reloaded.comments.len(), 1);
        assert_eq!(reloaded.comments[0].body, "hello");
        assert_eq!(reloaded.comments[0].id, 1);

        let mut store = reloaded;
        store.delete(1).unwrap();
        assert!(CommentStore::load(&repo).unwrap().comments.is_empty());
    }

    #[test]
    fn malformed_store_is_reported_instead_of_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let store_dir = repo.path().join("gittre");
        fs::create_dir(&store_dir).unwrap();
        fs::write(store_dir.join("comments.json"), b"{not json").unwrap();

        let err = CommentStore::load(&repo)
            .err()
            .expect("malformed store fails");
        assert!(format!("{err:#}").contains("parsing"));
    }

    #[test]
    fn exhausted_comment_ids_fail_without_creating_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let store_dir = repo.path().join("gittre");
        fs::create_dir(&store_dir).unwrap();
        let existing = Comment {
            id: u64::MAX,
            path: "a.rs".into(),
            new_side: true,
            lines: (1, 1),
            snippet: vec!["+x".into()],
            body: "existing".into(),
            created_at: 0,
            scope: "test".into(),
            outdated: false,
        };
        fs::write(
            store_dir.join("comments.json"),
            serde_json::to_vec(&StoreFile {
                next_id: u64::MAX,
                comments: vec![existing],
            })
            .unwrap(),
        )
        .unwrap();
        let mut store = CommentStore::load(&repo).unwrap();

        let err = store
            .add(
                "b.rs".into(),
                true,
                (1, 1),
                vec!["+y".into()],
                "new".into(),
                "test".into(),
            )
            .unwrap_err();

        assert!(format!("{err:#}").contains("comment id space exhausted"));
        assert_eq!(store.comments.len(), 1);
    }

    #[test]
    fn failed_write_leaves_mutations_out_of_memory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut store = CommentStore::load(&repo).unwrap();
        store
            .add(
                "a.rs".into(),
                true,
                (1, 1),
                vec!["+old".into()],
                "original".into(),
                "test".into(),
            )
            .unwrap();
        fs::create_dir(store.path.with_extension("json.tmp")).unwrap();

        assert!(store.edit(1, "changed".into()).is_err());
        assert_eq!(store.comments[0].body, "original");
        assert!(store.delete(1).is_err());
        assert_eq!(store.comments.len(), 1);
    }

    #[test]
    fn failed_reanchor_write_keeps_stored_position() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut store = CommentStore::load(&repo).unwrap();
        store
            .add(
                "a.rs".into(),
                true,
                (11, 11),
                vec!["+line 11".into()],
                "note".into(),
                "test".into(),
            )
            .unwrap();
        fs::create_dir(store.path.with_extension("json.tmp")).unwrap();
        let moved = DiffResult {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                large: false,
                byte_size: 0,
                untracked_dir: false,
                hunks: vec![Hunk {
                    header: "@@ @@".into(),
                    lines: vec![DiffLine {
                        origin: '+',
                        old_lineno: None,
                        new_lineno: Some(14),
                        content: "line 11".into(),
                    }],
                }],
                additions: 1,
                deletions: 0,
            }],
            additions: 1,
            deletions: 0,
        };

        assert!(store.reanchor(&moved).is_err());
        assert_eq!(store.comments[0].lines, (11, 11));
    }

    #[test]
    fn restore_after_delete_keeps_id_and_uniqueness() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut store = CommentStore::load(&repo).unwrap();
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
        let deleted = store.comments[0].clone();
        store.delete(deleted.id).unwrap();
        store.restore(vec![deleted]).unwrap();

        let reloaded = CommentStore::load(&repo).unwrap();
        assert_eq!(reloaded.comments.len(), 1);
        assert_eq!(reloaded.comments[0].id, 1);
        assert_eq!(reloaded.comments[0].body, "hello");

        // Ids minted after a restore never collide with the restored one.
        let mut store = reloaded;
        store
            .add(
                "b.rs".into(),
                true,
                (1, 1),
                vec!["+y".into()],
                "later".into(),
                "uncommitted changes".into(),
            )
            .unwrap();
        assert_ne!(store.comments[0].id, store.comments[1].id);
    }
}
