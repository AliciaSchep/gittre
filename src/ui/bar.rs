use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

/// The `/` search prompt, rendered in place of the command bar while typing.
pub fn render_search_input(frame: &mut Frame, area: Rect, query: &str) {
    let line = Line::from(vec![
        "/".bold().cyan(),
        query.to_string().into(),
        "▏".cyan(),
        "   ".into(),
        "[⏎]".bold().cyan(),
        " search  ".dark_gray(),
        "[Esc]".bold().cyan(),
        " cancel".dark_gray(),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

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
