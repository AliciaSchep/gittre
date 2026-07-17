use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

/// A one-line prompt (search, export path…) in place of the command bar.
pub fn render_input(frame: &mut Frame, area: Rect, label: &str, value: &str, action: &str) {
    let line = Line::from(vec![
        label.to_string().bold().cyan(),
        value.to_string().into(),
        "▏".cyan(),
        "   ".into(),
        "⏎".bold().cyan(),
        format!(" {action}  ").dark_gray(),
        "Esc".bold().cyan(),
        " cancel".dark_gray(),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Gitui-style contextual command bar for the primary keys valid right now.
pub fn render(frame: &mut Frame, area: Rect, hints: &[(String, &str)]) {
    let rows = pack_rows(hints, area.width, area.height);
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| {
            let mut spans = Vec::new();
            for (position, hint) in row.into_iter().enumerate() {
                if position > 0 {
                    spans.push("  ".into());
                }
                let (key, label) = &hints[hint];
                spans.push(display_key(key).bold().cyan());
                spans.push(format!(" {label}").dark_gray());
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn display_key(key: &str) -> String {
    key.to_string()
}

fn hint_width(key: &str, label: &str) -> usize {
    UnicodeWidthStr::width(display_key(key).as_str()) + 1 + UnicodeWidthStr::width(label)
}

/// Pack only complete hint groups into the available rows. Ratatui's text
/// wrapping can clip the final group midway through its label; keeping layout
/// at item boundaries makes narrow bars degrade by omission instead.
fn pack_rows(hints: &[(String, &str)], width: u16, height: u16) -> Vec<Vec<usize>> {
    let width = usize::from(width);
    let height = usize::from(height);
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = 0;
    for (index, (key, label)) in hints.iter().enumerate() {
        let item_width = hint_width(key, label);
        if item_width > width {
            continue;
        }
        if rows.is_empty() {
            rows.push(Vec::new());
        }
        let separator = usize::from(!rows.last().expect("row exists").is_empty()) * 2;
        if used + separator + item_width > width {
            if rows.len() == height {
                break;
            }
            rows.push(Vec::new());
            used = 0;
        }
        let separator = usize::from(!rows.last().expect("row exists").is_empty()) * 2;
        rows.last_mut().expect("row exists").push(index);
        used += separator + item_width;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_render_without_decorative_brackets() {
        assert_eq!(display_key("]/["), "]/[");
        assert_eq!(display_key("j/k"), "j/k");
        assert_eq!(display_key("/"), "/");
    }

    #[test]
    fn packing_never_returns_a_partial_hint() {
        let hints = vec![
            ("a".into(), "one"),
            ("b".into(), "two"),
            ("c".into(), "three"),
        ];
        assert_eq!(pack_rows(&hints, 10, 2), vec![vec![0], vec![1]]);
    }
}
