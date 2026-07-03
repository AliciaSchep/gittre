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
        // The anchor line's expected content, from the preserved snippet.
        let target = comment
            .snippet
            .last()
            .map(|l| snippet_text(l))
            .unwrap_or("");

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

        // 1. Exact: stored line number still carries the snippet content.
        let exact = candidates
            .iter()
            .find(|(n, _, _, c)| *n == comment.lines.1 && *c == target);
        // 2. Moved: same content elsewhere; nearest occurrence wins.
        //    Blank targets match too promiscuously to be trusted.
        let hit = exact.or_else(|| {
            if target.trim().is_empty() {
                return None;
            }
            candidates
                .iter()
                .filter(|(_, _, _, c)| *c == target)
                .min_by_key(|(n, _, _, _)| n.abs_diff(comment.lines.1))
        });

        let anchor = match hit {
            Some(&(n, hi, li, _)) => {
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
            outdated: false,
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

    /// Re-anchor all comments against a diff, persisting any moves or
    /// outdated-state changes.
    pub fn reanchor(&mut self, diff: &DiffResult) -> Vec<Placed> {
        let (placed, changed) = reanchor(diff, &mut self.comments);
        if changed {
            let _ = self.save();
        }
        placed
    }

    pub fn count_for_path(&self, path: &str) -> usize {
        self.comments.iter().filter(|c| c.path == path).count()
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
