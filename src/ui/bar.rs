use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

/// A one-line prompt (search, export path…) in place of the command bar.
pub fn render_input(frame: &mut Frame, area: Rect, label: &str, value: &str, action: &str) {
    let line = Line::from(vec![
        label.to_string().bold().cyan(),
        value.to_string().into(),
        "▏".cyan(),
        "   ".into(),
        "[⏎]".bold().cyan(),
        format!(" {action}  ").dark_gray(),
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
