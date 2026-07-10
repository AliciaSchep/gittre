use std::cell::Cell;

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::UnicodeWidthChar;

/// Read-only preview of the exact Markdown produced by comment export.
pub struct ExportPreview {
    markdown: String,
    lines: Vec<String>,
    scroll: usize,
    viewport: Cell<usize>,
    line_width: usize,
}

impl ExportPreview {
    pub fn new(markdown: String) -> Self {
        let line_width = 100;
        let lines = wrapped_lines(&markdown, line_width);
        Self {
            markdown,
            lines,
            scroll: 0,
            viewport: Cell::new(20),
            line_width,
        }
    }

    pub fn markdown(&self) -> &str {
        &self.markdown
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(self.viewport.get()) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    pub fn page(&mut self, direction: isize) {
        self.scroll_by(direction * self.viewport.get().saturating_sub(1) as isize);
    }

    pub fn top(&mut self) {
        self.scroll = 0;
    }

    pub fn bottom(&mut self) {
        self.scroll = self.lines.len().saturating_sub(self.viewport.get());
    }

    pub fn render(&mut self, frame: &mut Frame, bounds: Rect, comment_count: usize) {
        let width = bounds.width.saturating_sub(4).min(120);
        let height = bounds.height.saturating_sub(2).max(3);
        let popup = Rect {
            x: bounds.x + bounds.width.saturating_sub(width) / 2,
            y: bounds.y + bounds.height.saturating_sub(height) / 2,
            width,
            height,
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().cyan())
            .title(Line::from(
                format!(
                    " export preview · {comment_count} comment{} ",
                    if comment_count == 1 { "" } else { "s" }
                )
                .bold(),
            ));
        let inner = block.inner(popup);
        let width = inner.width.max(1) as usize;
        if width != self.line_width {
            self.lines = wrapped_lines(&self.markdown, width);
            self.line_width = width;
        }
        self.viewport.set(inner.height.max(1) as usize);
        self.scroll = self
            .scroll
            .min(self.lines.len().saturating_sub(inner.height as usize));
        let visible: Vec<Line> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(inner.height as usize)
            .map(|line| Line::from(line.clone()))
            .collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(visible).block(block), popup);
    }
}

fn wrapped_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in text.lines() {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut start = 0;
        let mut cells = 0;
        for (idx, c) in logical.char_indices() {
            let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
            if cells + char_width > width && idx > start {
                out.push(logical[start..idx].to_string());
                start = idx;
                cells = 0;
            }
            cells += char_width;
        }
        out.push(logical[start..].to_string());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_stays_bounded() {
        let mut preview = ExportPreview::new((0..50).map(|n| format!("{n}\n")).collect());
        preview.viewport.set(10);
        preview.scroll_by(999);
        assert_eq!(preview.scroll, 40);
        preview.scroll_by(-999);
        assert_eq!(preview.scroll, 0);
    }

    #[test]
    fn long_comment_lines_wrap_without_changing_markdown() {
        let markdown = "> a deliberately long comment".to_string();
        let preview = ExportPreview::new(markdown.clone());
        let lines = wrapped_lines(preview.markdown(), 10);
        assert_eq!(lines, ["> a delibe", "rately lon", "g comment"]);
        assert_eq!(preview.markdown(), markdown);
    }
}
