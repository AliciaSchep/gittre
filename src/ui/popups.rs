use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::ui::editor::TextEditor;

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
    ("←↑↓→ / Home End", "move the caret while editing a comment"),
    ("⏎ / Alt+⏎", "save comment / insert newline"),
    ("} / {", "next / previous comment"),
    ("d / D", "delete the comment at the cursor / ALL comments"),
    ("e", "preview comment export (then copy or write markdown)"),
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

/// Modal editor positioned near the diff row it belongs to. It grows with
/// wrapped content, then becomes a scrolling viewport.
pub fn render_comment_editor(
    frame: &mut Frame,
    bounds: Rect,
    anchor: Option<Position>,
    label: &str,
    editor: &mut TextEditor,
) {
    let width = bounds.width.saturating_sub(4).clamp(3, 96);
    let content_width = width.saturating_sub(2).max(1);
    let wanted_height = editor.visual_line_count(content_width) as u16 + 2;
    let max_height = (bounds.height / 2)
        .max(5)
        .min(bounds.height.saturating_sub(1));
    let height = wanted_height.clamp(5.min(max_height), max_height);

    let fallback_x = bounds.x + bounds.width.saturating_sub(width) / 2;
    let desired_x = anchor.map(|p| p.x).unwrap_or(fallback_x);
    let max_x = bounds.right().saturating_sub(width);
    let x = desired_x.clamp(bounds.x, max_x);
    let centered_y = bounds.y + bounds.height.saturating_sub(height) / 2;
    let y = anchor
        .map(|p| {
            let below = p.y.saturating_add(1);
            if below.saturating_add(height) <= bounds.bottom() {
                below
            } else if p.y >= bounds.y.saturating_add(height) {
                p.y - height
            } else {
                centered_y
            }
        })
        .unwrap_or(centered_y)
        .clamp(bounds.y, bounds.bottom().saturating_sub(height));
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().cyan())
        .title(Line::from(format!(" ✎ {label} ").bold()));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    editor.render(frame, inner);
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
