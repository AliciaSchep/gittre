use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

const HELP: &[(&str, &str)] = &[
    ("↑↓ / j k", "scroll diff (or move in file tree)"),
    ("PgUp PgDn / ⌃u ⌃d", "scroll a page"),
    ("g / G", "jump to top / bottom"),
    ("] / [", "next / previous file"),
    ("n / p", "next / previous hunk"),
    ("Tab", "switch focus between tree and diff"),
    ("⏎", "tree: open file in diff / toggle directory"),
    ("t", "show / hide the file tree"),
    ("?", "toggle this help"),
    ("q / Esc", "quit (or close this popup)"),
];

pub fn render_help(frame: &mut Frame, area: Rect) {
    let width = 64.min(area.width.saturating_sub(4));
    let height = (HELP.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let key_width = HELP
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::default()];
    for (key, desc) in HELP {
        lines.push(Line::from(vec![
            format!("  {key:>key_width$}  ").bold().cyan(),
            Span::raw(*desc),
        ]));
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(Line::from(" help ".bold())),
        ),
        popup,
    );
}
