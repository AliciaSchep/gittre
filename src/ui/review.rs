use std::cell::Cell;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::git::diff::{DiffResult, FileDiff, FileStatus};

/// One renderable row of the concatenated diff stream.
enum Row {
    Spacer,
    FileHeader(usize),
    Binary,
    HunkHeader(usize, usize),
    Line(usize, usize, usize),
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

/// The continuous multi-file diff view.
pub struct Stream {
    rows: Vec<Row>,
    /// Row index of each file's header, in file order.
    file_starts: Vec<usize>,
    /// Row index of each hunk header, in stream order.
    hunk_starts: Vec<usize>,
    pub scroll: usize,
    /// Height of the last rendered viewport, for paging and clamping.
    viewport: Cell<usize>,
    search: Option<SearchState>,
    selection: Option<Selection>,
}

impl Stream {
    pub fn new(diff: &DiffResult) -> Self {
        let mut rows = Vec::new();
        let mut file_starts = Vec::new();
        let mut hunk_starts = Vec::new();

        for (fi, file) in diff.files.iter().enumerate() {
            if fi > 0 {
                rows.push(Row::Spacer);
            }
            file_starts.push(rows.len());
            rows.push(Row::FileHeader(fi));
            if file.binary {
                rows.push(Row::Binary);
                continue;
            }
            for (hi, hunk) in file.hunks.iter().enumerate() {
                hunk_starts.push(rows.len());
                rows.push(Row::HunkHeader(fi, hi));
                for li in 0..hunk.lines.len() {
                    rows.push(Row::Line(fi, hi, li));
                }
            }
        }

        Stream {
            rows,
            file_starts,
            hunk_starts,
            scroll: 0,
            viewport: Cell::new(24),
            search: None,
            selection: None,
        }
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

    pub fn render(&self, frame: &mut Frame, area: Rect, files: &[FileDiff], focused: bool) {
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
                let mut line = self.render_row(row, files, inner.width);
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

    fn render_row(&self, row: &Row, files: &[FileDiff], width: u16) -> Line<'static> {
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
                spans.extend(self.content_spans(&line.content, base_style));
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
        let mut stream = Stream::new(&before);
        stream.set_viewport(4);
        stream.jump_to_file(1);
        stream.scroll_by(2); // two rows into b.txt
        let anchor = stream.anchor(&before.files).unwrap();
        assert_eq!(anchor.0, "b.txt");

        // a.txt grows by 10 lines; b.txt's rows all shift down.
        let after = diff(&[("a.txt", 13), ("b.txt", 5)]);
        let mut stream = Stream::new(&after);
        stream.set_viewport(4);
        stream.restore(&anchor, &after.files);
        assert_eq!(stream.current_file(), Some(1), "still reading b.txt");
    }

    #[test]
    fn anchor_of_vanished_file_clamps_safely() {
        let before = diff(&[("a.txt", 3), ("b.txt", 40)]);
        let mut stream = Stream::new(&before);
        stream.set_viewport(4);
        stream.scroll_to_bottom();
        let anchor = stream.anchor(&before.files).unwrap();

        let after = diff(&[("a.txt", 3)]);
        let mut stream = Stream::new(&after);
        stream.set_viewport(4);
        stream.restore(&anchor, &after.files);
        assert!(stream.scroll < 6, "scroll clamped into the smaller diff");
    }
}
