use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState};

use crate::git::log::LogEntry;
use crate::git::scope::Scope;

/// What activating a scope-picker entry does.
pub enum ScopeAction {
    Open(Scope),
    PickCommit,
}

pub struct ScopeItem {
    pub title: String,
    /// e.g. "12 files"; empty for the commit entry.
    pub detail: String,
    pub action: ScopeAction,
}

/// The launch menu: "review what?"
pub struct ScopePicker {
    pub items: Vec<ScopeItem>,
    pub selected: usize,
}

impl ScopePicker {
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.items.len() as isize;
        if len > 0 {
            self.selected = (self.selected as isize + delta).clamp(0, len - 1) as usize;
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let width = 60.min(area.width.saturating_sub(2));
        let height = (self.items.len() as u16 + 2).min(area.height);
        let popup = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };

        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let detail_width = item.detail.chars().count();
                let title_width = item.title.chars().count();
                let pad = (width as usize)
                    .saturating_sub(4 + 3 + title_width + detail_width + 2)
                    .max(1);
                ListItem::new(Line::from(vec![
                    format!(" {}  ", i + 1).bold().cyan(),
                    Span::raw(item.title.clone()),
                    " ".repeat(pad).into(),
                    item.detail.clone().dark_gray(),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(Line::from(" review what? ".bold())),
            )
            .highlight_style(Style::new().reversed());
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, popup, &mut state);
    }
}

/// The commit list: pick one commit to review.
pub struct LogPicker {
    pub entries: Vec<LogEntry>,
    pub selected: usize,
}

impl LogPicker {
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.entries.len() as isize;
        if len > 0 {
            self.selected = (self.selected as isize + delta).clamp(0, len - 1) as usize;
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| {
                ListItem::new(Line::from(vec![
                    format!(" {} ", e.short).yellow(),
                    format!("{:>3} ", e.age).dark_gray(),
                    format!("{:<12.12} ", e.author).cyan(),
                    Span::raw(e.summary.clone()),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(Block::new().title(Line::from(" pick a commit ".bold())))
            .highlight_style(Style::new().reversed());
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }
}
