use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::comments::{Anchor, Comment, Placed};
use crate::git::diff::{DiffResult, FileDiff, FileStatus};
use crate::ui::highlight::Highlighter;

/// One renderable row of the concatenated diff stream.
enum Row {
    Spacer,
    FileHeader(usize),
    Binary,
    HunkHeader(usize, usize),
    Line(usize, usize, usize),
    /// Comment block header; the index is into the comment store slice.
    CommentHeader(usize),
    /// One body line of a comment: (comment index, body line index).
    CommentBody(usize, usize),
}

struct SearchState {
    query: String,
    case_sensitive: bool,
    /// Row indices of matching lines, ascending.
    matches: Vec<usize>,
    current: usize,
}

/// A row-range selection made in select mode (`v`).
struct Selection {
    anchor: usize,
    cursor: usize,
}

impl Selection {
    fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }
}

/// Everything needed to create a comment: captured at `c` time.
#[derive(Clone)]
pub struct CommentTarget {
    pub path: String,
    pub new_side: bool,
    pub lines: (u32, u32),
    pub snippet: Vec<String>,
}

/// The continuous multi-file diff view.
pub struct Stream {
    rows: Vec<Row>,
    /// Row index of each file's header, in file order.
    file_starts: Vec<usize>,
    /// Row index of each hunk header, in stream order.
    hunk_starts: Vec<usize>,
    /// Row index of each comment header, in stream order.
    comment_starts: Vec<usize>,
    pub scroll: usize,
    /// Height of the last rendered viewport, for paging and clamping.
    viewport: Cell<usize>,
    search: Option<SearchState>,
    selection: Option<Selection>,
    /// Cached syntax spans per (file, hunk, line); None caches a miss.
    #[allow(clippy::type_complexity)]
    syntax_cache: RefCell<HashMap<(usize, usize, usize), Option<Vec<Span<'static>>>>>,
}

impl Stream {
    pub fn new(diff: &DiffResult, placed: &[Placed], comments: &[Comment]) -> Self {
        let mut at_line: HashMap<(usize, usize, usize), Vec<usize>> = HashMap::new();
        let mut at_top: HashMap<usize, Vec<usize>> = HashMap::new();
        for p in placed {
            match p.anchor {
                Anchor::Line { hunk, line } => at_line
                    .entry((p.file, hunk, line))
                    .or_default()
                    .push(p.comment),
                Anchor::FileTop => at_top.entry(p.file).or_default().push(p.comment),
            }
        }

        let mut rows = Vec::new();
        let mut file_starts = Vec::new();
        let mut hunk_starts = Vec::new();
        let mut comment_starts = Vec::new();
        let push_comment = |rows: &mut Vec<Row>, starts: &mut Vec<usize>, ci: usize| {
            starts.push(rows.len());
            rows.push(Row::CommentHeader(ci));
            for bi in 0..comments[ci].body.lines().count().max(1) {
                rows.push(Row::CommentBody(ci, bi));
            }
        };

        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 {
                rows.push(Row::Spacer);
            }
            file_starts.push(rows.len());
            rows.push(Row::FileHeader(fi));
            if let Some(cs) = at_top.get(&fi) {
                for &ci in cs {
                    push_comment(&mut rows, &mut comment_starts, ci);
                }
            }
            if file.binary {
                rows.push(Row::Binary);
                continue;
            }
            for (hi, hunk) in file.hunks.iter().enumerate() {
                hunk_starts.push(rows.len());
                rows.push(Row::HunkHeader(fi, hi));
                for li in 0..hunk.lines.len() {
                    rows.push(Row::Line(fi, hi, li));
                    if let Some(cs) = at_line.get(&(fi, hi, li)) {
                        for &ci in cs {
                            push_comment(&mut rows, &mut comment_starts, ci);
                        }
                    }
                }
            }
        }

        Stream {
            rows,
            file_starts,
            hunk_starts,
            comment_starts,
            scroll: 0,
            viewport: Cell::new(24),
            search: None,
            selection: None,
            syntax_cache: RefCell::new(HashMap::new()),
        }
    }

    // ---- comments ----------------------------------------------------------

    pub fn next_comment(&mut self) {
        if let Some(&start) = self.comment_starts.iter().find(|&&s| s > self.scroll) {
            self.scroll = start.min(self.scroll_limit());
        }
    }

    pub fn prev_comment(&mut self) {
        if let Some(&start) = self.comment_starts.iter().rev().find(|&&s| s < self.scroll) {
            self.scroll = start;
        }
    }

    pub fn has_comments(&self) -> bool {
        !self.comment_starts.is_empty()
    }

    /// The comment whose block is at the top of the viewport, if any.
    pub fn comment_at_top(&self) -> Option<usize> {
        match self.rows.get(self.scroll) {
            Some(Row::CommentHeader(ci)) | Some(Row::CommentBody(ci, _)) => Some(*ci),
            _ => None,
        }
    }

    /// Build a comment target from the active selection (single file only).
    pub fn selection_target(&self, files: &[FileDiff]) -> Option<CommentTarget> {
        let (lo, hi) = self.selection.as_ref()?.range();
        let mut target_file: Option<usize> = None;
        let mut new_nums: Vec<u32> = Vec::new();
        let mut old_nums: Vec<u32> = Vec::new();
        let mut snippet = Vec::new();
        for row in &self.rows[lo..=hi.min(self.rows.len() - 1)] {
            if let Row::Line(fi, hi_, li) = *row {
                if *target_file.get_or_insert(fi) != fi {
                    break; // clamp a cross-file selection to the first file
                }
                let line = &files[fi].hunks[hi_].lines[li];
                if let Some(n) = line.new_lineno {
                    new_nums.push(n);
                }
                if let Some(n) = line.old_lineno {
                    old_nums.push(n);
                }
                snippet.push(format!("{}{}", line.origin, line.content));
            }
        }
        let fi = target_file?;
        let (new_side, nums) = if new_nums.is_empty() {
            (false, old_nums)
        } else {
            (true, new_nums)
        };
        let (&first, &last) = (nums.first()?, nums.last()?);
        Some(CommentTarget {
            path: files[fi].path.clone(),
            new_side,
            lines: (first, last),
            snippet,
        })
    }

    /// Target for a bare `c`: the first diff line at/below the viewport top.
    pub fn line_target(&self, files: &[FileDiff]) -> Option<CommentTarget> {
        self.rows.iter().skip(self.scroll).find_map(|row| {
            let Row::Line(fi, hi, li) = *row else {
                return None;
            };
            let line = &files[fi].hunks[hi].lines[li];
            let (new_side, n) = match (line.new_lineno, line.old_lineno) {
                (Some(n), _) => (true, n),
                (None, Some(n)) => (false, n),
                _ => return None,
            };
            Some(CommentTarget {
                path: files[fi].path.clone(),
                new_side,
                lines: (n, n),
                snippet: vec![format!("{}{}", line.origin, line.content)],
            })
        })
    }

    // ---- selection ---------------------------------------------------------

    /// Enter select mode with the cursor on the top visible row.
    pub fn start_selection(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let start = self.scroll.min(self.rows.len() - 1);
        self.selection = Some(Selection {
            anchor: start,
            cursor: start,
        });
    }

    pub fn cancel_selection(&mut self) {
        self.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Move the selection cursor and keep it in view.
    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.rows.len();
        let Some(sel) = &mut self.selection else {
            return;
        };
        let cursor =
            (sel.cursor as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        sel.cursor = cursor;
        let viewport = self.viewport.get().max(1);
        if cursor < self.scroll {
            self.scroll = cursor;
        } else if cursor >= self.scroll + viewport {
            self.scroll = cursor + 1 - viewport;
        }
    }

    /// Text of the selection. `patch_style` keeps +/- signs and hunk headers;
    /// otherwise returns clean new-side code (deletions skipped).
    pub fn selected_text(&self, files: &[FileDiff], patch_style: bool) -> Option<String> {
        let (lo, hi) = self.selection.as_ref()?.range();
        let mut out = String::new();
        for row in &self.rows[lo..=hi.min(self.rows.len() - 1)] {
            match *row {
                Row::Line(fi, hi_, li) => {
                    let line = &files[fi].hunks[hi_].lines[li];
                    if patch_style {
                        out.push(line.origin);
                        out.push_str(&line.content);
                        out.push('\n');
                    } else if line.origin != '-' {
                        out.push_str(&line.content);
                        out.push('\n');
                    }
                }
                Row::HunkHeader(fi, hi_) if patch_style => {
                    out.push_str(&files[fi].hunks[hi_].header);
                    out.push('\n');
                }
                _ => {}
            }
        }
        (!out.is_empty()).then_some(out)
    }

    // ---- search ------------------------------------------------------------

    /// Smart-case: case-sensitive only when the query has an uppercase char.
    /// Returns the number of matches and jumps to the first one.
    pub fn set_search(&mut self, query: &str, files: &[FileDiff]) -> usize {
        let case_sensitive = query.chars().any(|c| c.is_uppercase());
        let needle = if case_sensitive {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let matches: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                let Row::Line(fi, hi, li) = *row else {
                    return None;
                };
                let content = &files[fi].hunks[hi].lines[li].content;
                let hit = if case_sensitive {
                    content.contains(&needle)
                } else {
                    content.to_lowercase().contains(&needle)
                };
                hit.then_some(i)
            })
            .collect();
        let count = matches.len();
        if count == 0 {
            self.search = None;
        } else {
            self.search = Some(SearchState {
                query: query.to_string(),
                case_sensitive,
                matches,
                current: 0,
            });
            self.jump_to_current_match();
        }
        count
    }

    pub fn clear_search(&mut self) {
        self.search = None;
    }

    pub fn has_search(&self) -> bool {
        self.search.is_some()
    }

    pub fn search_query(&self) -> Option<String> {
        self.search.as_ref().map(|s| s.query.clone())
    }

    /// (current 1-based, total, query) for the status display.
    pub fn search_status(&self) -> Option<(usize, usize, &str)> {
        self.search
            .as_ref()
            .map(|s| (s.current + 1, s.matches.len(), s.query.as_str()))
    }

    pub fn next_match(&mut self) {
        if let Some(s) = &mut self.search {
            s.current = (s.current + 1) % s.matches.len();
            self.jump_to_current_match();
        }
    }

    pub fn prev_match(&mut self) {
        if let Some(s) = &mut self.search {
            s.current = (s.current + s.matches.len() - 1) % s.matches.len();
            self.jump_to_current_match();
        }
    }

    fn jump_to_current_match(&mut self) {
        if let Some(s) = &self.search {
            if let Some(&row) = s.matches.get(s.current) {
                // A few lines of context above the match.
                self.scroll = row.saturating_sub(3).min(self.scroll_limit());
            }
        }
    }

    /// Vim-style bound: scrolling stops when the last row reaches the top,
    /// so any file header can always be jumped to the top of the viewport.
    fn scroll_limit(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let new = self.scroll as isize + delta;
        self.scroll = new.clamp(0, self.scroll_limit() as isize) as usize;
    }

    pub fn page(&mut self, direction: isize) {
        self.scroll_by(direction * self.viewport.get().saturating_sub(1) as isize);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    /// Show the full last page rather than a lone final row.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.rows.len().saturating_sub(self.viewport.get());
    }

    pub fn jump_to_file(&mut self, file_idx: usize) {
        if let Some(&start) = self.file_starts.get(file_idx) {
            self.scroll = start;
        }
    }

    /// Where the reader is, expressed as (file path, rows below its header) —
    /// stable across reloads even when other files grow or shrink.
    pub fn anchor(&self, files: &[FileDiff]) -> Option<(String, usize)> {
        let fi = self.current_file()?;
        let rel = self.scroll - self.file_starts[fi];
        Some((files[fi].path.clone(), rel))
    }

    /// Re-apply an anchor after the diff was rebuilt. Falls back to clamping
    /// when the anchored file disappeared from the new diff.
    pub fn restore(&mut self, anchor: &(String, usize), files: &[FileDiff]) {
        let (path, rel) = anchor;
        if let Some(fi) = files.iter().position(|f| &f.path == path) {
            let start = self.file_starts[fi];
            let end = self
                .file_starts
                .get(fi + 1)
                .copied()
                .unwrap_or(self.rows.len());
            self.scroll = (start + rel).min(end.saturating_sub(1));
        }
        self.scroll = self.scroll.min(self.scroll_limit());
    }

    /// The first content row at or below the top of the viewport:
    /// (file index, 1-based line number on the new side when known).
    pub fn current_position(&self, files: &[FileDiff]) -> Option<(usize, Option<u32>)> {
        self.rows
            .iter()
            .skip(self.scroll)
            .find_map(|row| match *row {
                Row::Line(fi, hi, li) => {
                    let line = &files[fi].hunks[hi].lines[li];
                    Some((fi, line.new_lineno.or(line.old_lineno)))
                }
                Row::FileHeader(fi) => Some((fi, None)),
                _ => None,
            })
    }

    /// Index of the file whose content is at the top of the viewport.
    pub fn current_file(&self) -> Option<usize> {
        if self.file_starts.is_empty() {
            return None;
        }
        let pos = self
            .file_starts
            .partition_point(|&start| start <= self.scroll);
        Some(pos.saturating_sub(1))
    }

    pub fn next_file(&mut self) {
        if let Some(&start) = self.file_starts.iter().find(|&&s| s > self.scroll) {
            self.scroll = start;
        }
    }

    pub fn prev_file(&mut self) {
        if let Some(&start) = self.file_starts.iter().rev().find(|&&s| s < self.scroll) {
            self.scroll = start;
        }
    }

    pub fn next_hunk(&mut self) {
        if let Some(&start) = self.hunk_starts.iter().find(|&&s| s > self.scroll) {
            self.scroll = start;
        }
    }

    pub fn prev_hunk(&mut self) {
        if let Some(&start) = self.hunk_starts.iter().rev().find(|&&s| s < self.scroll) {
            self.scroll = start;
        }
    }

    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        files: &[FileDiff],
        comments: &[Comment],
        focused: bool,
        hl: &Highlighter,
    ) {
        let border_style = if focused {
            Style::new().cyan()
        } else {
            Style::new().dark_gray()
        };
        // Sticky header: the current file's path lives in the block title.
        let mut title = match self.current_file() {
            Some(fi) => format!(" {}  ({}/{}) ", files[fi].path, fi + 1, files.len()),
            None => String::from(" diff "),
        };
        if let Some((current, total, query)) = self.search_status() {
            title.push_str(&format!("─ /{query}  {current}/{total} "));
        }
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(border_style)
            .title(Line::from(title.bold()));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        self.viewport.set(inner.height as usize);

        let visible = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(inner.height as usize)
            .map(|(idx, row)| {
                let mut line = self.render_row(row, files, comments, inner.width, hl);
                if let Some(sel) = &self.selection {
                    let (lo, hi) = sel.range();
                    if idx >= lo && idx <= hi {
                        line = line.on_dark_gray();
                    }
                    if idx == sel.cursor {
                        line = line.bold().underlined();
                    }
                }
                line
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), inner);
    }

    #[cfg(test)]
    fn set_viewport(&self, height: usize) {
        self.viewport.set(height);
    }

    fn line_matches_search(&self, content: &str) -> bool {
        let Some(search) = &self.search else {
            return false;
        };
        if search.case_sensitive {
            content.contains(&search.query)
        } else {
            content
                .to_lowercase()
                .contains(&search.query.to_lowercase())
        }
    }

    /// Syntax-colored spans for a diff line, with a red/green background
    /// tint preserving the add/remove signal. Cached per row.
    fn syntax_spans(
        &self,
        hl: &Highlighter,
        key: (usize, usize, usize),
        path: &str,
        line: &crate::git::diff::DiffLine,
    ) -> Option<Vec<Span<'static>>> {
        if let Some(cached) = self.syntax_cache.borrow().get(&key) {
            return cached.clone();
        }
        let tint = match line.origin {
            '+' => Some(Color::Rgb(16, 48, 16)),
            '-' => Some(Color::Rgb(58, 20, 20)),
            _ => None,
        };
        let spans = hl.line_spans(path, &line.content).map(|sp| {
            sp.into_iter()
                .map(|span| match tint {
                    Some(bg) => span.bg(bg),
                    None => span,
                })
                .collect::<Vec<_>>()
        });
        self.syntax_cache.borrow_mut().insert(key, spans.clone());
        spans
    }

    /// Split a line's content into spans, painting search matches yellow.
    fn content_spans(&self, content: &str, base: Style) -> Vec<Span<'static>> {
        let Some(search) = &self.search else {
            return vec![Span::styled(content.to_string(), base)];
        };
        let needle = if search.case_sensitive {
            search.query.clone()
        } else {
            search.query.to_lowercase()
        };
        let hay = if search.case_sensitive {
            content.to_string()
        } else {
            content.to_lowercase()
        };
        // Lowercasing can change byte lengths (İ → i̇); offsets into `hay`
        // would then be invalid in `content`, so skip in-line highlighting.
        if hay.len() != content.len() {
            return vec![Span::styled(content.to_string(), base)];
        }
        let hit = Style::new().fg(Color::Black).bg(Color::Yellow);
        let mut spans = Vec::new();
        let mut pos = 0;
        while let Some(found) = hay[pos..].find(&needle) {
            let start = pos + found;
            let end = start + needle.len();
            if start > pos {
                spans.push(Span::styled(content[pos..start].to_string(), base));
            }
            spans.push(Span::styled(content[start..end].to_string(), hit));
            pos = end;
        }
        if pos < content.len() {
            spans.push(Span::styled(content[pos..].to_string(), base));
        }
        spans
    }

    fn render_row(
        &self,
        row: &Row,
        files: &[FileDiff],
        comments: &[Comment],
        width: u16,
        hl: &Highlighter,
    ) -> Line<'static> {
        match *row {
            Row::Spacer => Line::default(),
            Row::FileHeader(fi) => {
                let f = &files[fi];
                let mut spans: Vec<Span> = vec![" ".into()];
                spans.push(match f.status {
                    FileStatus::Added => "A ".green().bold(),
                    FileStatus::Deleted => "D ".red().bold(),
                    _ => format!("{} ", f.status.letter()).yellow().bold(),
                });
                if let Some(old) = &f.old_path {
                    spans.push(format!("{old} → ").into());
                }
                spans.push(f.path.clone().bold());
                spans.push(format!("  +{} ", f.additions).green());
                spans.push(format!("−{}", f.deletions).red());
                let text_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                let pad = (width as usize).saturating_sub(text_len);
                spans.push(" ".repeat(pad).into());
                Line::from(spans).on_dark_gray()
            }
            Row::Binary => Line::from("   (binary file changed)".dark_gray().italic()),
            Row::CommentHeader(ci) => {
                let c = &comments[ci];
                let range = if c.lines.0 == c.lines.1 {
                    format!("L{}", c.lines.0)
                } else {
                    format!("L{}-L{}", c.lines.0, c.lines.1)
                };
                let side = if c.new_side { "" } else { " (old side)" };
                Line::from(vec![
                    "▐ ".cyan(),
                    format!("✎ #{} · {}{}", c.id, range, side).bold().cyan(),
                ])
            }
            Row::CommentBody(ci, bi) => {
                let body = comments[ci].body.lines().nth(bi).unwrap_or("");
                Line::from(vec!["▐ ".cyan(), Span::raw(body.to_string())])
            }
            Row::HunkHeader(fi, hi) => Line::from(files[fi].hunks[hi].header.clone().cyan()),
            Row::Line(fi, hi, li) => {
                let line = &files[fi].hunks[hi].lines[li];
                let old = line
                    .old_lineno
                    .map(|n| format!("{n:>5}"))
                    .unwrap_or_else(|| " ".repeat(5));
                let new = line
                    .new_lineno
                    .map(|n| format!("{n:>5}"))
                    .unwrap_or_else(|| " ".repeat(5));
                let gutter = format!("{old} {new} ");
                let base_style = match line.origin {
                    '+' => Style::new().green(),
                    '-' => Style::new().red(),
                    _ => Style::new(),
                };
                let mut spans = vec![gutter.dark_gray()];
                spans.push(Span::styled(line.origin.to_string(), base_style));
                // Search hits keep the plain rendering so the yellow overlay
                // stays visible; everything else gets syntax colors.
                let searched = self.search.is_some() && self.line_matches_search(&line.content);
                let syntax = (!searched)
                    .then(|| self.syntax_spans(hl, (fi, hi, li), &files[fi].path, line))
                    .flatten();
                match syntax {
                    Some(sp) => spans.extend(sp),
                    None => spans.extend(self.content_spans(&line.content, base_style)),
                }
                Line::from(spans)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{DiffLine, DiffResult, FileDiff, FileStatus, Hunk};

    fn file(path: &str, lines: usize) -> FileDiff {
        FileDiff {
            path: path.into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![Hunk {
                header: "@@ @@".into(),
                lines: (0..lines)
                    .map(|i| DiffLine {
                        origin: '+',
                        old_lineno: None,
                        new_lineno: Some(i as u32 + 1),
                        content: format!("line {i}"),
                    })
                    .collect(),
            }],
            additions: lines,
            deletions: 0,
        }
    }

    fn diff(specs: &[(&str, usize)]) -> DiffResult {
        DiffResult {
            files: specs.iter().map(|(p, n)| file(p, *n)).collect(),
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn anchor_survives_growth_of_earlier_files() {
        let before = diff(&[("a.txt", 3), ("b.txt", 5)]);
        let mut stream = Stream::new(&before, &[], &[]);
        stream.set_viewport(4);
        stream.jump_to_file(1);
        stream.scroll_by(2); // two rows into b.txt
        let anchor = stream.anchor(&before.files).unwrap();
        assert_eq!(anchor.0, "b.txt");

        // a.txt grows by 10 lines; b.txt's rows all shift down.
        let after = diff(&[("a.txt", 13), ("b.txt", 5)]);
        let mut stream = Stream::new(&after, &[], &[]);
        stream.set_viewport(4);
        stream.restore(&anchor, &after.files);
        assert_eq!(stream.current_file(), Some(1), "still reading b.txt");
    }

    #[test]
    fn anchor_of_vanished_file_clamps_safely() {
        let before = diff(&[("a.txt", 3), ("b.txt", 40)]);
        let mut stream = Stream::new(&before, &[], &[]);
        stream.set_viewport(4);
        stream.scroll_to_bottom();
        let anchor = stream.anchor(&before.files).unwrap();

        let after = diff(&[("a.txt", 3)]);
        let mut stream = Stream::new(&after, &[], &[]);
        stream.set_viewport(4);
        stream.restore(&anchor, &after.files);
        assert!(stream.scroll < 6, "scroll clamped into the smaller diff");
    }
}
