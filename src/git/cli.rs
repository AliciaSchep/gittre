//! Worktree diffs via the `git` CLI.
//!
//! libgit2 has no fsmonitor, no untracked cache, and no parallelism, so on
//! large repos its worktree scans are 10-100x slower than git's. Like
//! lazygit/delta/tig, we shell out for the two scopes that touch the working
//! tree (uncommitted, staged) and parse unified diff output; object-database
//! scopes (commit/branch/range) stay on libgit2, where it's fast.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::git::diff::{DiffLine, DiffResult, FileDiff, FileStatus, Hunk};
use crate::git::scope::MAX_CONTENT_FILE_SIZE;

fn git(workdir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(workdir)
        // Emit non-ASCII paths raw instead of quoted/escaped.
        .args(["-c", "core.quotepath=false"]);
    cmd
}

fn run(mut cmd: Command, what: &str) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("running git for {what}"))?;
    if !out.status.success() {
        bail!(
            "git {what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Working tree + index vs HEAD (untracked included), or index vs HEAD.
pub fn load_worktree(workdir: &Path, staged: bool) -> Result<DiffResult> {
    let t = std::time::Instant::now();
    let mut cmd = git(workdir);
    cmd.args(["diff", "--no-color", "--no-ext-diff", "-M", "-p"]);
    if staged {
        cmd.arg("--cached");
    }
    cmd.arg("HEAD");
    let patch = run(cmd, "diff")?;
    crate::app::debug_log(&format!("cli phase: git diff {:?}", t.elapsed()));

    let t = std::time::Instant::now();
    let mut files = parse_patch(&patch);
    crate::app::debug_log(&format!(
        "cli phase: parse {:?} ({} files)",
        t.elapsed(),
        files.len()
    ));

    // Same stub rule as the libgit2 path: oversized content collapses to a
    // "press ⏎ to load" stub. The patch is already parsed, so this only
    // bounds what the stream has to hold and render.
    for file in &mut files {
        let content_bytes: usize = file
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.content.len() + 1)
            .sum();
        if content_bytes > MAX_CONTENT_FILE_SIZE as usize {
            file.large = true;
            file.byte_size = content_bytes as u64;
            file.hunks.clear();
            file.additions = 0;
            file.deletions = 0;
        }
    }

    if !staged {
        let t = std::time::Instant::now();
        files.extend(untracked_files(workdir)?);
        crate::app::debug_log(&format!("cli phase: untracked {:?}", t.elapsed()));
    }

    Ok(DiffResult::from_files(files))
}

/// One file's diff, for expanding a large stub.
pub fn load_worktree_file(workdir: &Path, staged: bool, path: &str) -> Result<FileDiff> {
    // Untracked files aren't in `git diff`; synthesize from disk.
    if !staged && is_untracked(workdir, path)? {
        return untracked_file(workdir, path, u64::MAX);
    }
    let mut cmd = git(workdir);
    cmd.args(["diff", "--no-color", "--no-ext-diff", "-M", "-p"]);
    if staged {
        cmd.arg("--cached");
    }
    cmd.args(["HEAD", "--"]).arg(path);
    let patch = run(cmd, "single-file diff")?;
    parse_patch(&patch)
        .into_iter()
        .find(|f| f.path == path)
        .ok_or_else(|| anyhow::anyhow!("'{path}' no longer in the diff"))
}

/// (uncommitted, staged) changed-file counts from one status scan.
pub fn worktree_counts(workdir: &Path) -> Result<(usize, usize)> {
    let out = run(
        {
            let mut cmd = git(workdir);
            cmd.args(["status", "--porcelain", "-z", "--untracked-files=all"]);
            cmd
        },
        "status",
    )?;
    let mut uncommitted = 0;
    let mut staged = 0;
    for entry in parse_status_z(&out) {
        let (x, y, _) = entry;
        if x != ' ' || y != ' ' {
            uncommitted += 1;
        }
        if x != ' ' && x != '?' {
            staged += 1;
        }
    }
    Ok((uncommitted, staged))
}

fn is_untracked(workdir: &Path, path: &str) -> Result<bool> {
    let out = run(
        {
            let mut cmd = git(workdir);
            cmd.args(["status", "--porcelain", "-z", "--untracked-files=all", "--"])
                .arg(path);
            cmd
        },
        "status",
    )?;
    Ok(parse_status_z(&out)
        .into_iter()
        .any(|(x, y, p)| x == '?' && y == '?' && p == path))
}

/// Inline untracked contents only this far; everything beyond becomes a
/// stub (Enter loads it), so a sea of untracked files can't dominate loads.
const UNTRACKED_INLINE_MAX_FILES: usize = 200;
const UNTRACKED_INLINE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Untracked files as "added" diffs, content inlined below the size cap.
fn untracked_files(workdir: &Path) -> Result<Vec<FileDiff>> {
    let t = std::time::Instant::now();
    let out = run(
        {
            let mut cmd = git(workdir);
            cmd.args(["status", "--porcelain", "-z", "--untracked-files=all"]);
            cmd
        },
        "status",
    )?;
    crate::app::debug_log(&format!("cli phase: untracked status {:?}", t.elapsed()));

    let t = std::time::Instant::now();
    let mut files = Vec::new();
    let mut inlined_bytes: u64 = 0;
    for (x, y, path) in parse_status_z(&out) {
        if x != '?' || y != '?' {
            continue;
        }
        let over_budget =
            files.len() >= UNTRACKED_INLINE_MAX_FILES || inlined_bytes > UNTRACKED_INLINE_MAX_BYTES;
        let cap = if over_budget {
            0 // stub: name and size only, no content read
        } else {
            MAX_CONTENT_FILE_SIZE as u64
        };
        let file = untracked_file(workdir, &path, cap)?;
        inlined_bytes += if file.large { 0 } else { file.byte_size };
        files.push(file);
    }
    crate::app::debug_log(&format!(
        "cli phase: untracked contents {:?} ({} files)",
        t.elapsed(),
        files.len()
    ));
    Ok(files)
}

fn untracked_file(workdir: &Path, path: &str, cap: u64) -> Result<FileDiff> {
    let full = workdir.join(path);
    let byte_size = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
    let mut file = FileDiff {
        path: path.to_string(),
        old_path: None,
        status: FileStatus::Added,
        binary: false,
        large: byte_size > cap,
        byte_size,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
    };
    if file.large {
        return Ok(file);
    }
    let bytes = std::fs::read(&full).unwrap_or_default();
    if bytes.contains(&0) {
        file.binary = true;
        return Ok(file);
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<DiffLine> = text
        .lines()
        .enumerate()
        .map(|(i, l)| DiffLine {
            origin: '+',
            old_lineno: None,
            new_lineno: Some(i as u32 + 1),
            content: l.to_string(),
        })
        .collect();
    file.additions = lines.len();
    if !lines.is_empty() {
        file.hunks.push(Hunk {
            header: format!("@@ -0,0 +1,{} @@", lines.len()),
            lines,
        });
    }
    Ok(file)
}

/// Parse `git status --porcelain -z` entries into (X, Y, path).
fn parse_status_z(out: &str) -> Vec<(char, char, String)> {
    let mut entries = Vec::new();
    let mut parts = out.split('\0').peekable();
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let mut chars = entry.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        let path = entry[3..].to_string();
        // Renames carry the original path as the next NUL-separated field.
        if x == 'R' || y == 'R' || x == 'C' || y == 'C' {
            parts.next();
        }
        entries.push((x, y, path));
    }
    entries
}

// ---- unified diff parsing --------------------------------------------------

/// Parse `git diff -p` output into FileDiffs (paths, statuses, hunks, lines).
pub fn parse_patch(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_ln: u32 = 0;
    let mut new_ln: u32 = 0;
    // Lines still expected in the current hunk; content is only consumed
    // while nonzero, so file contents can't be mistaken for markers.
    let mut old_left: u64 = 0;
    let mut new_left: u64 = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(FileDiff {
                path: guess_b_path(rest),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                large: false,
                byte_size: 0,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            old_left = 0;
            new_left = 0;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };

        if old_left > 0 || new_left > 0 {
            let Some(hunk) = file.hunks.last_mut() else {
                continue;
            };
            match line.chars().next() {
                Some(' ') => {
                    hunk.lines.push(DiffLine {
                        origin: ' ',
                        old_lineno: Some(old_ln),
                        new_lineno: Some(new_ln),
                        content: line[1..].to_string(),
                    });
                    old_ln += 1;
                    new_ln += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                Some('-') => {
                    hunk.lines.push(DiffLine {
                        origin: '-',
                        old_lineno: Some(old_ln),
                        new_lineno: None,
                        content: line[1..].to_string(),
                    });
                    old_ln += 1;
                    file.deletions += 1;
                    old_left = old_left.saturating_sub(1);
                }
                Some('+') => {
                    hunk.lines.push(DiffLine {
                        origin: '+',
                        old_lineno: None,
                        new_lineno: Some(new_ln),
                        content: line[1..].to_string(),
                    });
                    new_ln += 1;
                    file.additions += 1;
                    new_left = new_left.saturating_sub(1);
                }
                Some('\\') => {} // "\ No newline at end of file"
                _ => {}
            }
            continue;
        }

        if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("rename to ") {
            file.path = unquote(p);
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        } else if let Some(p) = line.strip_prefix("--- a/") {
            if file.status != FileStatus::Renamed {
                // Prefer header paths over the diff --git guess.
                if file.status != FileStatus::Added {
                    file.path = unquote(p);
                }
            }
        } else if let Some(p) = line.strip_prefix("+++ b/") {
            if file.status != FileStatus::Renamed {
                file.path = unquote(p);
            }
        } else if let Some((os, oc, ns, nc, header)) = parse_hunk_header(line) {
            old_ln = os;
            new_ln = ns;
            old_left = oc;
            new_left = nc;
            file.hunks.push(Hunk {
                header: header.to_string(),
                lines: Vec::new(),
            });
        }
    }
    files
}

/// "@@ -12,3 +14,4 @@ fn ctx" -> (12, 3, 14, 4, whole line)
fn parse_hunk_header(line: &str) -> Option<(u32, u64, u32, u64, &str)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    let parse_range = |s: &str| -> Option<(u32, u64)> {
        match s.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (os, oc) = parse_range(old_part)?;
    let (ns, nc) = parse_range(new_part)?;
    Some((os, oc, ns, nc, line))
}

/// Best-effort path from `diff --git a/x b/x` (overridden by ---/+++ lines).
fn guess_b_path(rest: &str) -> String {
    match rest.rfind(" b/") {
        Some(i) => unquote(&rest[i + 3..]),
        None => rest.to_string(),
    }
}

/// Undo git's C-style quoting for paths with special characters.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if !(s.starts_with('"') && s.ends_with('"') && s.len() >= 2) {
        return s.to_string();
    }
    let inner = &s[1..s.len() - 1];
    let mut out = Vec::new();
    let mut bytes = inner.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'"') => out.push(b'"'),
            Some(b'\\') => out.push(b'\\'),
            Some(d @ b'0'..=b'7') => {
                let mut v = (d - b'0') as u32;
                for _ in 0..2 {
                    if let Some(&d2 @ b'0'..=b'7') = bytes.peek() {
                        v = v * 8 + (d2 - b'0') as u32;
                        bytes.next();
                    }
                }
                out.push(v as u8);
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1234567..89abcde 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@ fn context()
 fn one() {}
-fn two() {}
+fn two() { updated(); }
+fn extra() {}
 fn three() {}
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index abc..000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-first
-second
diff --git a/old_name.rs b/new_name.rs
similarity index 90%
rename from old_name.rs
rename to new_name.rs
diff --git a/pic.png b/pic.png
index 111..222 100644
Binary files a/pic.png and b/pic.png differ
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
index 000..333
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1,2 @@
+hello
+--- not a header, content
";

    #[test]
    fn parses_statuses_hunks_and_line_numbers() {
        let files = parse_patch(SAMPLE);
        assert_eq!(files.len(), 5);

        let modified = &files[0];
        assert_eq!(modified.path, "src/lib.rs");
        assert_eq!(modified.status, FileStatus::Modified);
        assert_eq!((modified.additions, modified.deletions), (2, 1));
        let lines = &modified.hunks[0].lines;
        assert_eq!(lines[0].origin, ' ');
        assert_eq!(lines[0].old_lineno, Some(1));
        assert_eq!(lines[1].origin, '-');
        assert_eq!(lines[1].content, "fn two() {}");
        assert_eq!(lines[2].new_lineno, Some(2));
        assert_eq!(lines[4].origin, ' ');
        assert_eq!(lines[4].new_lineno, Some(4));

        assert_eq!(files[1].status, FileStatus::Deleted);
        assert_eq!(files[1].path, "gone.txt");

        assert_eq!(files[2].status, FileStatus::Renamed);
        assert_eq!(files[2].old_path.as_deref(), Some("old_name.rs"));
        assert_eq!(files[2].path, "new_name.rs");

        assert!(files[3].binary);

        let fresh = &files[4];
        assert_eq!(fresh.status, FileStatus::Added);
        assert_eq!(fresh.additions, 2);
        assert_eq!(
            fresh.hunks[0].lines[1].content, "--- not a header, content",
            "content lines can't be mistaken for headers"
        );
    }

    #[test]
    fn hunk_header_forms() {
        assert_eq!(
            parse_hunk_header("@@ -12,3 +14,4 @@ fn x"),
            Some((12, 3, 14, 4, "@@ -12,3 +14,4 @@ fn x"))
        );
        assert_eq!(
            parse_hunk_header("@@ -1 +1 @@"),
            Some((1, 1, 1, 1, "@@ -1 +1 @@"))
        );
        assert_eq!(parse_hunk_header("not a header"), None);
    }

    #[test]
    fn unquotes_special_paths() {
        assert_eq!(unquote("plain.txt"), "plain.txt");
        assert_eq!(unquote("\"with \\\"quote\\\".txt\""), "with \"quote\".txt");
        assert_eq!(unquote("\"tab\\there\""), "tab\there");
        assert_eq!(unquote("\"caf\\303\\251.txt\""), "café.txt");
    }
}
