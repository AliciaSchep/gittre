use std::cell::Cell;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::ui::highlight::Highlighter;

/// Read-only full-file pager, overlaid on the review screen via `o`.
pub struct FileView {
    pub path: String,
    /// Where the content came from: "working tree", "index", a sha, …
    pub source: String,
    lines: Vec<String>,
    /// Stateful syntax highlighting, precomputed at open; None = plain.
    styled: Option<Vec<Vec<Span<'static>>>>,
    pub scroll: usize,
    /// 0-based line the reader came from; kept highlighted.
    target: Option<usize>,
    viewport: Cell<usize>,
}

impl FileView {
    pub fn new(
        path: String,
        source: String,
        content: &str,
        target: Option<usize>,
        hl: &Highlighter,
    ) -> Self {
        let lines: Vec<String> = content.lines().map(str::to_string).collect();
        let styled = hl.file_lines(&path, content);
        let scroll = target
            .map(|t| t.saturating_sub(3))
            .unwrap_or(0)
            .min(lines.len().saturating_sub(1));
        FileView {
            path,
            source,
            lines,
            styled,
            scroll,
            target,
            viewport: Cell::new(24),
        }
    }

    /// 1-based line number at the top of the viewport (for $EDITOR handoff).
    pub fn top_line(&self) -> usize {
        self.scroll + 1
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    pub fn page(&mut self, direction: isize) {
        self.scroll_by(direction * self.viewport.get().saturating_sub(1) as isize);
    }

    pub fn half_page(&mut self, direction: isize) {
        self.scroll_by(direction * (self.viewport.get() / 2).max(1) as isize);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.lines.len().saturating_sub(self.viewport.get());
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let title = format!(
            " {}  full file · {} · {} lines ",
            self.path,
            self.source,
            self.lines.len()
        );
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().cyan())
            .title(Line::from(title.bold()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.viewport.set(inner.height as usize);

        let width = self.lines.len().to_string().len().max(4);
        let visible: Vec<Line> = self
            .lines
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(inner.height as usize)
            .map(|(i, text)| {
                let mut spans = vec![format!("{:>width$} ", i + 1).dark_gray()];
                match self.styled.as_ref().and_then(|s| s.get(i)) {
                    Some(sp) => spans.extend(sp.iter().cloned()),
                    None => spans.push(Span::raw(text.clone())),
                }
                let mut line = Line::from(spans);
                if self.target == Some(i) {
                    line = line.on_dark_gray().bold();
                }
                line
            })
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);
    }
}
