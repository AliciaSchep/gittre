use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::comments::{Anchor, Comment, Placed};
use crate::git::diff::{DiffResult, FileDiff, FileStatus};
use crate::ui::highlight::Highlighter;

/// One renderable row of the concatenated diff stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Spacer,
    FileHeader(usize),
    Binary,
    /// Oversized file whose diff wasn't loaded; Enter expands it.
    LargeStub(usize),
    HunkHeader(usize, usize),
    Line(usize, usize, usize),
    /// Comment block header; the index is into the comment store slice.
    CommentHeader(usize),
    /// One preserved-snippet line of an outdated comment.
    CommentSnippet(usize, usize),
    /// A wrapped segment of one comment body line:
    /// (comment index, logical line, start byte, end byte).
    CommentBody(usize, usize, usize, usize),
}

#[derive(Clone, Copy)]
struct RowOrigin {
    template: usize,
    body_offset: usize,
}

struct SearchState {
    query: String,
    case_sensitive: bool,
    /// Row indices of matching lines, ascending.
    matches: Vec<usize>,
    current: usize,
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
    /// Width-independent rows. Comment body rows are expanded from this
    /// template whenever the diff pane changes width.
    template_rows: Vec<Row>,
    rows: Vec<Row>,
    row_origins: Vec<RowOrigin>,
    comment_wrap_width: usize,
    /// Row index of each file's header, in file order.
    file_starts: Vec<usize>,
    /// Row index of each hunk header, in stream order.
    hunk_starts: Vec<usize>,
    /// Row index of each comment header, in stream order.
    comment_starts: Vec<usize>,
    pub scroll: usize,
    /// The highlighted row all position-dependent actions operate on.
    pub cursor: usize,
    /// Selection anchor (`v`); the selection is anchor..=cursor.
    selection_anchor: Option<usize>,
    /// Height of the last rendered viewport, for paging and clamping.
    viewport: Cell<usize>,
    search: Option<SearchState>,
    /// Inner rect from the last render, for mouse hit-testing.
    last_inner: Cell<Rect>,
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
                Anchor::Outdated => at_top.entry(p.file).or_default().push(p.comment),
            }
        }

        let mut rows = Vec::new();
        let mut file_starts = Vec::new();
        let mut hunk_starts = Vec::new();
        let mut comment_starts = Vec::new();
        let push_comment =
            |rows: &mut Vec<Row>, starts: &mut Vec<usize>, ci: usize, with_snippet: bool| {
                starts.push(rows.len());
                rows.push(Row::CommentHeader(ci));
                if with_snippet {
                    // Outdated: the preserved snippet is the only context left.
                    for si in 0..comments[ci].snippet.len() {
                        rows.push(Row::CommentSnippet(ci, si));
                    }
                }
                for bi in 0..comments[ci].body.lines().count().max(1) {
                    let body = comments[ci].body.lines().nth(bi).unwrap_or("");
                    rows.push(Row::CommentBody(ci, bi, 0, body.len()));
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
                    push_comment(&mut rows, &mut comment_starts, ci, true);
                }
            }
            if file.untracked_dir || (file.large && file.hunks.is_empty()) {
                rows.push(Row::LargeStub(fi));
                continue;
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
                            push_comment(&mut rows, &mut comment_starts, ci, false);
                        }
                    }
                }
            }
        }

        let template_rows = rows;
        let rows = template_rows.clone();
        let row_origins = (0..rows.len())
            .map(|template| RowOrigin {
                template,
                body_offset: 0,
            })
            .collect();
        Stream {
            template_rows,
            rows,
            row_origins,
            comment_wrap_width: 0,
            file_starts,
            hunk_starts,
            comment_starts,
            scroll: 0,
            cursor: 0,
            selection_anchor: None,
            viewport: Cell::new(24),
            search: None,
            last_inner: Cell::new(Rect::default()),
            syntax_cache: RefCell::new(HashMap::new()),
        }
    }

    // ---- cursor ------------------------------------------------------------

    fn clamp_row(&self, row: isize) -> usize {
        row.clamp(0, self.rows.len().saturating_sub(1) as isize) as usize
    }

    /// Scroll just enough to keep the cursor on screen.
    fn ensure_cursor_visible(&mut self) {
        let viewport = self.viewport.get().max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + viewport {
            self.scroll = self.cursor + 1 - viewport;
        }
    }

    /// Vim-style: j/k move the cursor; the view follows.
    pub fn move_cursor(&mut self, delta: isize) {
        self.cursor = self.clamp_row(self.cursor as isize + delta);
        self.ensure_cursor_visible();
    }

    /// Put a jump target at the top of the viewport with the cursor on it.
    fn jump_to(&mut self, row: usize) {
        self.cursor = self.clamp_row(row as isize);
        self.scroll = self.cursor.min(self.scroll_limit());
    }

    /// Mouse: map a screen position to a stream row.
    pub fn hit(&self, column: u16, row: u16) -> Option<usize> {
        let inner = self.last_inner.get();
        if !inner.contains(Position::new(column, row)) {
            return None;
        }
        let idx = self.scroll + (row - inner.y) as usize;
        (idx < self.rows.len()).then_some(idx)
    }

    pub fn set_cursor(&mut self, row: usize) {
        self.cursor = self.clamp_row(row as isize);
        self.ensure_cursor_visible();
    }

    /// Screen position of the active row after the latest render. Popups use
    /// this to stay visually attached to the line or comment being edited.
    pub fn cursor_screen_position(&self) -> Option<Position> {
        let inner = self.last_inner.get();
        let offset = self.cursor.checked_sub(self.scroll)? as u16;
        (offset < inner.height).then(|| Position::new(inner.x.saturating_add(2), inner.y + offset))
    }

    pub fn viewport_rect(&self) -> Rect {
        self.last_inner.get()
    }

    // ---- comments ----------------------------------------------------------

    pub fn next_comment(&mut self) {
        if let Some(&start) = self.comment_starts.iter().find(|&&s| s > self.cursor) {
            self.jump_to(start);
        }
    }

    pub fn prev_comment(&mut self) {
        if let Some(&start) = self.comment_starts.iter().rev().find(|&&s| s < self.cursor) {
            self.jump_to(start);
        }
    }

    pub fn has_comments(&self) -> bool {
        !self.comment_starts.is_empty()
    }

    /// File index if the cursor is on a large-file stub row.
    pub fn large_stub_at_cursor(&self) -> Option<usize> {
        match self.rows.get(self.cursor) {
            Some(Row::LargeStub(fi)) => Some(*fi),
            _ => None,
        }
    }

    /// The comment whose block the cursor is on, if any.
    pub fn comment_at_cursor(&self) -> Option<usize> {
        match self.rows.get(self.cursor) {
            Some(Row::CommentHeader(ci))
            | Some(Row::CommentSnippet(ci, _))
            | Some(Row::CommentBody(ci, _, _, _)) => Some(*ci),
            _ => None,
        }
    }

    /// Build a comment target from the active selection (single file only).
    pub fn selection_target(&self, files: &[FileDiff]) -> Option<CommentTarget> {
        let (lo, hi) = self.selection_range()?;
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

    /// Target for a bare `c`: the first diff line at/below the cursor.
    pub fn line_target(&self, files: &[FileDiff]) -> Option<CommentTarget> {
        self.rows.iter().skip(self.cursor).find_map(|row| {
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

    /// `v`: anchor a selection at the cursor; it extends as the cursor moves.
    pub fn start_selection(&mut self) {
        if !self.rows.is_empty() {
            self.selection_anchor = Some(self.cursor);
        }
    }

    pub fn cancel_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection_anchor.is_some()
    }

    fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// Text of the selection. `patch_style` keeps +/- signs and hunk headers;
    /// otherwise returns clean new-side code (deletions skipped).
    pub fn selected_text(&self, files: &[FileDiff], patch_style: bool) -> Option<String> {
        let (lo, hi) = self.selection_range()?;
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
                self.cursor = row.min(self.rows.len().saturating_sub(1));
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

    /// Wheel scroll: moves the view, dragging the cursor along if it would
    /// leave the window.
    pub fn scroll_by(&mut self, delta: isize) {
        let new = self.scroll as isize + delta;
        self.scroll = new.clamp(0, self.scroll_limit() as isize) as usize;
        let viewport = self.viewport.get().max(1);
        let last_visible = (self.scroll + viewport - 1).min(self.rows.len().saturating_sub(1));
        self.cursor = self.cursor.clamp(self.scroll, last_visible);
    }

    pub fn page(&mut self, direction: isize) {
        self.move_cursor(direction * self.viewport.get().saturating_sub(1) as isize);
    }

    pub fn scroll_to_top(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Cursor to the last row; show the full last page.
    pub fn scroll_to_bottom(&mut self) {
        self.cursor = self.rows.len().saturating_sub(1);
        self.scroll = self.rows.len().saturating_sub(self.viewport.get());
    }

    pub fn jump_to_file(&mut self, file_idx: usize) {
        if let Some(&start) = self.file_starts.get(file_idx) {
            self.jump_to(start);
        }
    }

    /// Where the reader is: (file path, cursor rows below its header, cursor
    /// rows below the viewport top) — stable across reloads even when other
    /// files grow or shrink.
    pub fn anchor(&self, files: &[FileDiff]) -> Option<(String, usize, usize)> {
        let fi = self.current_file()?;
        let rel = self.cursor - self.file_starts[fi];
        let screen_offset = self.cursor.saturating_sub(self.scroll);
        Some((files[fi].path.clone(), rel, screen_offset))
    }

    /// Re-apply an anchor after the diff was rebuilt. Falls back to clamping
    /// when the anchored file disappeared from the new diff.
    pub fn restore(&mut self, anchor: &(String, usize, usize), files: &[FileDiff]) {
        let (path, rel, screen_offset) = anchor;
        if let Some(fi) = files.iter().position(|f| &f.path == path) {
            let start = self.file_starts[fi];
            let end = self
                .file_starts
                .get(fi + 1)
                .copied()
                .unwrap_or(self.rows.len());
            self.cursor = (start + rel).min(end.saturating_sub(1));
        }
        self.cursor = self.clamp_row(self.cursor as isize);
        self.scroll = self
            .cursor
            .saturating_sub(*screen_offset)
            .min(self.scroll_limit());
        self.ensure_cursor_visible();
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

    /// Index of the file the cursor is in.
    pub fn current_file(&self) -> Option<usize> {
        if self.file_starts.is_empty() {
            return None;
        }
        let pos = self
            .file_starts
            .partition_point(|&start| start <= self.cursor);
        Some(pos.saturating_sub(1))
    }

    pub fn next_file(&mut self) {
        if let Some(&start) = self.file_starts.iter().find(|&&s| s > self.cursor) {
            self.jump_to(start);
        }
    }

    pub fn prev_file(&mut self) {
        if let Some(&start) = self.file_starts.iter().rev().find(|&&s| s < self.cursor) {
            self.jump_to(start);
        }
    }

    pub fn next_hunk(&mut self) {
        if let Some(&start) = self.hunk_starts.iter().find(|&&s| s > self.cursor) {
            self.jump_to(start);
        }
    }

    pub fn prev_hunk(&mut self) {
        if let Some(&start) = self.hunk_starts.iter().rev().find(|&&s| s < self.cursor) {
            self.jump_to(start);
        }
    }

    fn reflow_comments(&mut self, width: u16, comments: &[Comment]) {
        let width = width.max(1) as usize;
        if width == self.comment_wrap_width {
            return;
        }

        let screen_offset = self.cursor.saturating_sub(self.scroll);
        let cursor_origin = self.row_origins.get(self.cursor).copied();
        let selection_origin = self
            .selection_anchor
            .and_then(|row| self.row_origins.get(row).copied());
        let search_origins: Option<Vec<RowOrigin>> = self.search.as_ref().map(|search| {
            search
                .matches
                .iter()
                .filter_map(|&row| self.row_origins.get(row).copied())
                .collect()
        });

        let mut rows = Vec::new();
        let mut origins = Vec::new();
        for (template, row) in self.template_rows.iter().copied().enumerate() {
            match row {
                Row::CommentBody(ci, bi, _, _) => {
                    let body = comments[ci].body.lines().nth(bi).unwrap_or("");
                    for (start, end) in wrap_ranges(body, width) {
                        rows.push(Row::CommentBody(ci, bi, start, end));
                        origins.push(RowOrigin {
                            template,
                            body_offset: start,
                        });
                    }
                }
                _ => {
                    rows.push(row);
                    origins.push(RowOrigin {
                        template,
                        body_offset: 0,
                    });
                }
            }
        }

        let locate = |origin: RowOrigin| {
            origins
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate.template == origin.template
                        && candidate.body_offset <= origin.body_offset
                })
                .map(|(row, _)| row)
                .next_back()
        };

        let cursor_row = cursor_origin.and_then(locate);
        let selection_row = selection_origin.and_then(locate);
        let search_rows = search_origins
            .map(|origins| origins.into_iter().filter_map(locate).collect::<Vec<_>>());

        self.rows = rows;
        self.row_origins = origins;
        self.comment_wrap_width = width;
        if let Some(row) = cursor_row {
            self.cursor = row;
        }
        self.selection_anchor = selection_row;
        if let (Some(search), Some(search_rows)) = (&mut self.search, search_rows) {
            search.matches = search_rows;
            search.current = search.current.min(search.matches.len().saturating_sub(1));
        }

        self.file_starts.clear();
        self.hunk_starts.clear();
        self.comment_starts.clear();
        for (row, item) in self.rows.iter().enumerate() {
            match item {
                Row::FileHeader(_) => self.file_starts.push(row),
                Row::HunkHeader(_, _) => self.hunk_starts.push(row),
                Row::CommentHeader(_) => self.comment_starts.push(row),
                _ => {}
            }
        }
        self.cursor = self.clamp_row(self.cursor as isize);
        self.scroll = self
            .cursor
            .saturating_sub(screen_offset)
            .min(self.scroll_limit());
        self.ensure_cursor_visible();
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        files: &[FileDiff],
        comments: &[Comment],
        focused: bool,
        hl: &Highlighter,
    ) {
        self.reflow_comments(area.width.saturating_sub(2), comments);
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
        self.last_inner.set(inner);

        let visible = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(inner.height as usize)
            .map(|(idx, row)| {
                let mut line = self.render_row(row, files, comments, inner.width, hl);
                if let Some((lo, hi)) = self.selection_range() {
                    if idx >= lo && idx <= hi {
                        line = line.on_dark_gray();
                    }
                }
                if idx == self.cursor && focused {
                    line = line.bold().underlined();
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
            Row::LargeStub(fi) => {
                let f = &files[fi];
                let label = if f.untracked_dir {
                    "untracked directory — contents not listed".to_string()
                } else {
                    format!("large file ({}) — diff not loaded", human_size(f.byte_size))
                };
                Line::from(vec![
                    "   ▶ ".cyan(),
                    label.italic(),
                    if f.untracked_dir {
                        "  [⏎ list]".bold().cyan()
                    } else {
                        "  [⏎ load]".bold().cyan()
                    },
                ])
            }
            Row::CommentHeader(ci) => {
                let c = &comments[ci];
                let range = if c.lines.0 == c.lines.1 {
                    format!("L{}", c.lines.0)
                } else {
                    format!("L{}-L{}", c.lines.0, c.lines.1)
                };
                let side = if c.new_side { "" } else { " (old side)" };
                if c.outdated {
                    Line::from(vec![
                        "▐ ".yellow(),
                        format!("⚠ #{} · outdated (was {}{})", c.id, range, side)
                            .bold()
                            .yellow(),
                    ])
                } else {
                    Line::from(vec![
                        "▐ ".cyan(),
                        format!("✎ #{} · {}{}", c.id, range, side).bold().cyan(),
                    ])
                }
            }
            Row::CommentSnippet(ci, si) => {
                let text = comments[ci].snippet.get(si).cloned().unwrap_or_default();
                Line::from(vec!["▐ ".yellow(), text.dark_gray().italic()])
            }
            Row::CommentBody(ci, bi, start, end) => {
                let body = comments[ci].body.lines().nth(bi).unwrap_or("");
                let text = body.get(start..end).unwrap_or("");
                Line::from(vec!["▐ ".cyan(), Span::raw(text.to_string())])
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

fn wrap_ranges(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![(0, 0)];
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut cells = 0;
    for (idx, c) in text.char_indices() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if cells + char_width > width && idx > start {
            ranges.push((start, idx));
            start = idx;
            cells = 0;
        }
        cells += char_width;
    }
    ranges.push((start, text.len()));
    ranges
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::Placed;
    use crate::git::diff::{DiffLine, DiffResult, FileDiff, FileStatus, Hunk};

    fn file(path: &str, lines: usize) -> FileDiff {
        FileDiff {
            path: path.into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            large: false,
            byte_size: 0,
            untracked_dir: false,
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

    #[test]
    fn saved_comments_wrap_and_keep_cursor_on_resize() {
        let diff = diff(&[("a.txt", 1)]);
        let comments = [Comment {
            id: 1,
            path: "a.txt".into(),
            new_side: true,
            lines: (1, 1),
            snippet: vec!["+line 0".into()],
            body: "a deliberately long saved comment".into(),
            created_at: 0,
            scope: "test".into(),
            outdated: false,
        }];
        let placed = [Placed {
            comment: 0,
            file: 0,
            anchor: Anchor::Line { hunk: 0, line: 0 },
        }];
        let mut stream = Stream::new(&diff, &placed, &comments);

        stream.reflow_comments(10, &comments);
        assert_eq!(stream.comment_starts, [3]);
        assert_eq!(stream.rows.len(), 8, "body expands to four visual rows");
        stream.cursor = 5;
        assert_eq!(stream.comment_at_cursor(), Some(0));

        stream.reflow_comments(20, &comments);
        assert_eq!(stream.comment_at_cursor(), Some(0));
        assert_eq!(stream.rows.len(), 6, "body reflows to two visual rows");
    }
}
