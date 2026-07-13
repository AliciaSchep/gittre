use std::cell::Cell;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VisualLine {
    start: usize,
    end: usize,
}

/// Small multiline editor used by comment drafts. The cursor is a UTF-8 byte
/// boundary; wrapping and vertical movement use terminal cell widths.
pub struct TextEditor {
    text: String,
    cursor: usize,
    preferred_column: Option<usize>,
    scroll: usize,
    reveal_cursor: bool,
    last_width: Cell<usize>,
    last_inner: Cell<Rect>,
}

impl TextEditor {
    pub fn new(text: String) -> Self {
        let cursor = text.len();
        Self {
            text,
            cursor,
            preferred_column: None,
            scroll: 0,
            reveal_cursor: true,
            last_width: Cell::new(70),
            last_inner: Cell::new(Rect::default()),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn insert_str(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn backspace(&mut self) {
        let Some(prev) = self.previous_boundary() else {
            return;
        };
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn delete(&mut self) {
        let Some(next) = self.next_boundary() else {
            return;
        };
        self.text.drain(self.cursor..next);
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn move_left(&mut self) {
        if let Some(prev) = self.previous_boundary() {
            self.cursor = prev;
        }
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn move_vertical(&mut self, delta: isize) {
        let width = self.last_width.get().max(1);
        let lines = visual_lines(&self.text, width);
        let (row, column) = self.cursor_position(&lines);
        let wanted = self.preferred_column.unwrap_or(column);
        let target = (row as isize + delta).clamp(0, lines.len() as isize - 1) as usize;
        self.cursor = byte_at_column(&self.text, lines[target], wanted);
        self.preferred_column = Some(wanted);
        self.reveal_cursor = true;
    }

    pub fn home(&mut self) {
        let lines = visual_lines(&self.text, self.last_width.get().max(1));
        let (row, _) = self.cursor_position(&lines);
        self.cursor = lines[row].start;
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn end(&mut self) {
        let lines = visual_lines(&self.text, self.last_width.get().max(1));
        let (row, _) = self.cursor_position(&lines);
        self.cursor = lines[row].end;
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn visual_line_count(&self, width: u16) -> usize {
        visual_lines(&self.text, width.max(1) as usize).len()
    }

    pub fn set_cursor_from_screen(&mut self, column: u16, row: u16) {
        let inner = self.last_inner.get();
        if !inner.contains(Position::new(column, row)) {
            return;
        }
        let lines = visual_lines(&self.text, inner.width.max(1) as usize);
        let visual_row = self.scroll + (row - inner.y) as usize;
        let Some(line) = lines.get(visual_row).copied() else {
            return;
        };
        self.cursor = byte_at_column(&self.text, line, (column - inner.x) as usize);
        self.preferred_column = None;
        self.reveal_cursor = true;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let lines = visual_lines(&self.text, self.last_width.get().max(1));
        let viewport = self.last_inner.get().height.max(1) as usize;
        let max = lines.len().saturating_sub(viewport) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
        self.reveal_cursor = false;
    }

    pub fn render(&mut self, frame: &mut Frame, inner: Rect) {
        let width = inner.width.max(1) as usize;
        let lines = visual_lines(&self.text, width);
        let (cursor_row, cursor_column) = self.cursor_position(&lines);
        let viewport = inner.height.max(1) as usize;
        if self.reveal_cursor || self.last_width.get() != width {
            if cursor_row < self.scroll {
                self.scroll = cursor_row;
            } else if cursor_row >= self.scroll + viewport {
                self.scroll = cursor_row + 1 - viewport;
            }
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(viewport));

        let visible: Vec<Line> = lines
            .iter()
            .skip(self.scroll)
            .take(viewport)
            .map(|line| Line::from(self.text[line.start..line.end].to_string()))
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);

        if cursor_row >= self.scroll && cursor_row < self.scroll + viewport {
            let x = inner.x + (cursor_column as u16).min(inner.width.saturating_sub(1));
            let y = inner.y + (cursor_row - self.scroll) as u16;
            frame.set_cursor_position(Position::new(x, y));
        }
        self.last_width.set(width);
        self.last_inner.set(inner);
        self.reveal_cursor = false;
    }

    fn cursor_position(&self, lines: &[VisualLine]) -> (usize, usize) {
        let row = lines
            .iter()
            .rposition(|line| line.start <= self.cursor)
            .unwrap_or(0);
        let column = UnicodeWidthStr::width(&self.text[lines[row].start..self.cursor]);
        (row, column)
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }
}

fn visual_lines(text: &str, width: usize) -> Vec<VisualLine> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut start = 0;
    let mut cells = 0;
    for (idx, c) in text.char_indices() {
        if c == '\n' {
            lines.push(VisualLine { start, end: idx });
            start = idx + c.len_utf8();
            cells = 0;
            continue;
        }
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if cells + char_width > width && idx > start {
            lines.push(VisualLine { start, end: idx });
            start = idx;
            cells = 0;
        }
        cells += char_width;
    }
    lines.push(VisualLine {
        start,
        end: text.len(),
    });
    lines
}

fn byte_at_column(text: &str, line: VisualLine, column: usize) -> usize {
    let mut cells = 0;
    for (offset, c) in text[line.start..line.end].char_indices() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if cells + char_width > column {
            return line.start + offset;
        }
        cells += char_width;
    }
    line.end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_at_cursor_without_breaking_utf8() {
        let mut editor = TextEditor::new("a界b".into());
        editor.move_left();
        editor.backspace();
        assert_eq!(editor.as_str(), "ab");
        editor.insert_char('é');
        assert_eq!(editor.as_str(), "aéb");
        editor.delete();
        assert_eq!(editor.as_str(), "aé");
    }

    #[test]
    fn wraps_and_moves_between_visual_rows() {
        let mut editor = TextEditor::new("abcdef".into());
        editor.last_width.set(3);
        editor.home();
        assert_eq!(editor.cursor, 3);
        editor.move_vertical(-1);
        assert_eq!(editor.cursor, 0);
        editor.end();
        assert_eq!(editor.cursor, 3);
        assert_eq!(editor.visual_line_count(3), 2);
    }

    #[test]
    fn newlines_make_empty_visual_rows() {
        let lines = visual_lines("one\n\n", 20);
        assert_eq!(lines.len(), 3);
        assert_eq!(&"one\n\n"[lines[1].start..lines[1].end], "");
        assert_eq!(&"one\n\n"[lines[2].start..lines[2].end], "");
    }
}
