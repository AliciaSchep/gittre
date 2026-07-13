use std::cell::Cell;

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState};

use crate::git::log::LogEntry;
use crate::git::scope::Scope;

/// What activating a scope-picker entry does.
pub enum ScopeAction {
    Open(Scope),
    PickCommit,
    /// Choose an explicit base from the branch list (merge-base semantics).
    PickBase,
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
    last_inner: Cell<Rect>,
}

pub fn count_label(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

impl ScopePicker {
    /// Fill in asynchronously computed file counts.
    pub fn set_counts(&mut self, uncommitted: usize, staged: usize, branch: Option<usize>) {
        for item in &mut self.items {
            match &item.action {
                ScopeAction::Open(Scope::Uncommitted) => item.detail = count_label(uncommitted),
                ScopeAction::Open(Scope::Staged) => item.detail = count_label(staged),
                ScopeAction::Open(Scope::BranchFork { .. }) => {
                    if let Some(n) = branch {
                        item.detail = count_label(n);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn new(items: Vec<ScopeItem>) -> Self {
        ScopePicker {
            items,
            selected: 0,
            last_inner: Cell::new(Rect::default()),
        }
    }

    /// Screen position -> item index (the menu never scrolls).
    pub fn hit(&self, column: u16, row: u16) -> Option<usize> {
        let inner = self.last_inner.get();
        if !inner.contains(Position::new(column, row)) {
            return None;
        }
        let idx = (row - inner.y) as usize;
        (idx < self.items.len()).then_some(idx)
    }

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

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(Line::from(" review what? ".bold()));
        self.last_inner.set(block.inner(popup));
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().reversed());
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, popup, &mut state);
    }
}

/// Branch list: pick a base to diff the current branch against.
pub struct BasePicker {
    pub names: Vec<String>,
    pub selected: usize,
    pub state: ListState,
    last_inner: Cell<Rect>,
}

impl BasePicker {
    pub fn new(names: Vec<String>) -> Self {
        BasePicker {
            names,
            selected: 0,
            state: ListState::default(),
            last_inner: Cell::new(Rect::default()),
        }
    }

    pub fn hit(&self, column: u16, row: u16) -> Option<usize> {
        list_hit(&self.last_inner, &self.state, self.names.len(), column, row)
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.names.len() as isize;
        if len > 0 {
            self.selected = (self.selected as isize + delta).clamp(0, len - 1) as usize;
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .names
            .iter()
            .map(|name| ListItem::new(Line::from(format!(" {name}"))))
            .collect();
        let block = Block::new().title(Line::from(" pick a base to compare against ".bold()));
        self.last_inner.set(block.inner(area));
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().reversed());
        self.state.select(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

/// The commit list: pick one commit to review.
pub struct LogPicker {
    pub entries: Vec<LogEntry>,
    pub selected: usize,
    /// Range base marked with Space; Enter then reviews marked..selected.
    pub marked: Option<git2::Oid>,
    pub state: ListState,
    last_inner: Cell<Rect>,
}

impl LogPicker {
    pub fn new(entries: Vec<LogEntry>) -> Self {
        LogPicker {
            entries,
            selected: 0,
            marked: None,
            state: ListState::default(),
            last_inner: Cell::new(Rect::default()),
        }
    }

    /// Space: mark the selected commit as range base (again to unmark).
    pub fn toggle_mark(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        self.marked = match self.marked {
            Some(m) if m == entry.id => None,
            _ => Some(entry.id),
        };
    }

    pub fn hit(&self, column: u16, row: u16) -> Option<usize> {
        list_hit(
            &self.last_inner,
            &self.state,
            self.entries.len(),
            column,
            row,
        )
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.entries.len() as isize;
        if len > 0 {
            self.selected = (self.selected as isize + delta).clamp(0, len - 1) as usize;
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| {
                let marker = if self.marked == Some(e.id) {
                    "●".cyan().bold()
                } else {
                    Span::raw(" ")
                };
                ListItem::new(Line::from(vec![
                    marker,
                    format!("{} ", e.short).yellow(),
                    format!("{:>3} ", e.age).dark_gray(),
                    format!("{:<12.12} ", e.author).cyan(),
                    Span::raw(e.summary.clone()),
                ]))
            })
            .collect();

        let block = Block::new().title(Line::from(" pick a commit ".bold()));
        self.last_inner.set(block.inner(area));
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().reversed());
        self.state.select(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

fn list_hit(
    last_inner: &Cell<Rect>,
    state: &ListState,
    len: usize,
    column: u16,
    row: u16,
) -> Option<usize> {
    let inner = last_inner.get();
    if !inner.contains(Position::new(column, row)) {
        return None;
    }
    let idx = state.offset() + (row - inner.y) as usize;
    (idx < len).then_some(idx)
}
