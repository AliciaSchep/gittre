use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

const HELP: &[(&str, &str)] = &[
    ("1-9", "picker: open that entry"),
    ("↑↓ / j k", "scroll diff (or move in a list)"),
    ("PgUp PgDn / ⌃u ⌃d", "scroll a page"),
    ("g / G", "jump to top / bottom"),
    ("] / [", "next / previous file"),
    ("n / p", "next / previous hunk"),
    ("/", "search the diff"),
    ("n / N", "next / previous match while a search is live"),
    ("v", "select lines; then y copies code, Y copies a patch"),
    ("o", "view the full file (read-only, at the current line)"),
    ("E", "open the file in $EDITOR at the current line"),
    ("c", "comment on the current line (or selection / edit)"),
    ("} / {", "next / previous comment"),
    ("d / D", "delete the comment at the cursor / ALL comments"),
    ("e", "export comments to markdown (also: gittre export)"),
    ("Tab", "pick a file from the tree (Esc cancels)"),
    ("⏎", "open the selected file / entry"),
    ("t", "show / hide the file tree"),
    (
        "r",
        "reload the diff now (always available; see --no-watch)",
    ),
    ("x", "switch scope (back to the picker)"),
    ("?", "toggle this help"),
    ("q / Esc", "back; quits from the picker"),
    ("⌃c", "quit from anywhere"),
    (
        "mouse",
        "wheel scrolls; click picks (shift+drag to select text)",
    ),
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

/// Modal editor for a comment body.
pub fn render_comment_editor(frame: &mut Frame, area: Rect, label: &str, body: &str) {
    let width = 72.min(area.width.saturating_sub(4));
    let body_lines = body.lines().count().max(1) as u16 + u16::from(body.ends_with('\n'));
    let height = (body_lines + 2).clamp(3, area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines: Vec<Line> = body.lines().map(|l| Line::from(l.to_string())).collect();
    if body.is_empty() || body.ends_with('\n') {
        lines.push(Line::default());
    }
    // Block cursor at the end of the last line.
    if let Some(last) = lines.last_mut() {
        last.push_span("▏".cyan());
    }

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().cyan())
                .title(Line::from(format!(" ✎ {label} ").bold())),
        ),
        popup,
    );
}

/// Small yes/no confirmation.
pub fn render_confirm(frame: &mut Frame, area: Rect, message: &str) {
    let width = (message.chars().count() as u16 + 6).min(area.width.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(3)) / 2,
        width,
        height: 3,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Line::from(message.to_string().bold()))
            .centered()
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().red()),
            ),
        popup,
    );
}
