use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::keymap::{self, Action, HelpItem};
use crate::ui::editor::TextEditor;

/// The `?` popup: sections and descriptions come from `keymap::HELP`; the
/// keys themselves are derived from the binding tables so they never drift.
/// Scrolls when it doesn't fit — `scroll` is clamped here, where the height
/// is known.
pub fn render_help(frame: &mut Frame, area: Rect, scroll: &mut u16) {
    let key_label = |item: &HelpItem| match item {
        HelpItem::Act(actions, _) => keymap::keys_label(keymap::REVIEW, actions),
        HelpItem::Raw(key, _) => (*key).to_string(),
    };
    let key_width = keymap::HELP
        .iter()
        .flat_map(|(_, items)| items.iter())
        .map(|item| key_label(item).chars().count())
        .max()
        .unwrap_or(0);

    let mut lines: Vec<Line> = Vec::new();
    for (si, (title, items)) in keymap::HELP.iter().enumerate() {
        if si > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(format!(" {title}").bold()));
        for item in *items {
            let desc = match item {
                HelpItem::Act(_, desc) | HelpItem::Raw(_, desc) => *desc,
            };
            lines.push(Line::from(vec![
                format!("  {:>key_width$}  ", key_label(item)).bold().cyan(),
                Span::raw(desc),
            ]));
        }
    }

    let width = 72.min(area.width.saturating_sub(4));
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let visible = height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(visible);
    *scroll = (*scroll).min(max_scroll);

    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(" help ".bold()));
    if max_scroll > 0 {
        let nav = keymap::keys_label(keymap::HELP_VIEW, &[Action::Down, Action::Up]);
        block = block.title_bottom(
            Line::from(format!(" {nav} scroll ({}/{max_scroll}) ", *scroll).dark_gray())
                .right_aligned(),
        );
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).scroll((*scroll, 0)).block(block),
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
