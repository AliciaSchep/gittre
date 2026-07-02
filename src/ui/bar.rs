use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

/// gitui-style contextual command bar: always shows the keys valid right now.
pub fn render(frame: &mut Frame, area: Rect, hints: &[(&str, &str)]) {
    let mut spans: Vec<Span> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push("  ".into());
        }
        spans.push(format!("[{key}]").bold().cyan());
        spans.push(format!(" {label}").dark_gray());
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
        area,
    );
}
