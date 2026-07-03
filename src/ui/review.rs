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
        let title = match self.current_file() {
            Some(fi) => format!(" {}  ({}/{}) ", files[fi].path, fi + 1, files.len()),
            None => String::from(" diff "),
        };
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
            .skip(self.scroll)
            .take(inner.height as usize)
            .map(|row| self.render_row(row, files, inner.width))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), inner);
    }

    #[cfg(test)]
    fn set_viewport(&self, height: usize) {
        self.viewport.set(height);
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
                let body = format!("{}{}", line.origin, line.content);
                let styled_body = match line.origin {
                    '+' => body.green(),
                    '-' => body.red(),
                    _ => body.into(),
                };
                Line::from(vec![gutter.dark_gray(), styled_body])
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
