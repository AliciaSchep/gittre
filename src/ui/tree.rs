use std::cell::Cell;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::git::diff::{FileDiff, FileStatus};

/// A directory or changed file in the sidebar tree.
struct Node {
    name: String,
    /// Index into the diff's file list; None for directories.
    file: Option<usize>,
    status: Option<FileStatus>,
    children: Vec<Node>,
    expanded: bool,
}

pub struct FileTree {
    root: Node,
    /// Selection index into the currently visible (flattened) rows.
    pub selected: usize,
    /// List scroll state, persisted so mouse clicks can be hit-tested.
    state: ListState,
    /// Inner list rect from the last render, for mouse hit-testing.
    last_inner: Cell<Rect>,
}

/// One visible row after flattening: (depth, node path through the tree).
struct VisibleRow<'a> {
    depth: usize,
    node: &'a Node,
}

impl FileTree {
    pub fn new(files: &[FileDiff]) -> Self {
        let mut root = Node {
            name: String::new(),
            file: None,
            status: None,
            children: Vec::new(),
            expanded: true,
        };
        for (idx, file) in files.iter().enumerate() {
            let mut node = &mut root;
            let parts: Vec<&str> = file.path.split('/').collect();
            for (i, part) in parts.iter().enumerate() {
                let is_leaf = i == parts.len() - 1;
                let pos = node
                    .children
                    .iter()
                    .position(|c| c.name == *part && (c.file.is_some() == is_leaf));
                let pos = match pos {
                    Some(p) => p,
                    None => {
                        node.children.push(Node {
                            name: part.to_string(),
                            file: is_leaf.then_some(idx),
                            status: is_leaf.then_some(file.status),
                            children: Vec::new(),
                            expanded: true,
                        });
                        node.children.len() - 1
                    }
                };
                node = &mut node.children[pos];
            }
        }
        sort_children(&mut root);
        FileTree {
            root,
            selected: 0,
            state: ListState::default(),
            last_inner: Cell::new(Rect::default()),
        }
    }

    /// Map a screen position to a visible row index, if it hits the list.
    pub fn hit(&self, column: u16, row: u16) -> Option<usize> {
        let inner = self.last_inner.get();
        if !inner.contains(Position::new(column, row)) {
            return None;
        }
        let idx = self.state.offset() + (row - inner.y) as usize;
        (idx < self.len()).then_some(idx)
    }

    fn visible(&self) -> Vec<VisibleRow<'_>> {
        let mut rows = Vec::new();
        fn walk<'a>(node: &'a Node, depth: usize, rows: &mut Vec<VisibleRow<'a>>) {
            for child in &node.children {
                rows.push(VisibleRow { depth, node: child });
                if child.expanded {
                    walk(child, depth + 1, rows);
                }
            }
        }
        walk(&self.root, 0, &mut rows);
        rows
    }

    pub fn len(&self) -> usize {
        self.visible().len()
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let new = self.selected as isize + delta;
        self.selected = new.clamp(0, len as isize - 1) as usize;
    }

    /// Enter on a directory toggles it; on a file returns its diff index.
    pub fn activate(&mut self) -> Option<usize> {
        let path = self.selected_path()?;
        let node = node_at_mut(&mut self.root, &path);
        match node.file {
            Some(idx) => Some(idx),
            None => {
                node.expanded = !node.expanded;
                self.selected = self.selected.min(self.len().saturating_sub(1));
                None
            }
        }
    }

    /// Move selection to the row for the given file index (stream -> tree sync).
    pub fn select_file(&mut self, file_idx: usize) {
        if let Some(pos) = self
            .visible()
            .iter()
            .position(|r| r.node.file == Some(file_idx))
        {
            self.selected = pos;
        }
    }

    /// Slash-joined paths of collapsed directories, for carrying fold state
    /// across a reload.
    pub fn collapsed_dirs(&self) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        fn walk(node: &Node, prefix: &str, out: &mut std::collections::HashSet<String>) {
            for child in &node.children {
                if child.file.is_none() {
                    let path = if prefix.is_empty() {
                        child.name.clone()
                    } else {
                        format!("{prefix}/{}", child.name)
                    };
                    if !child.expanded {
                        out.insert(path.clone());
                    }
                    walk(child, &path, out);
                }
            }
        }
        walk(&self.root, "", &mut out);
        out
    }

    pub fn apply_collapsed(&mut self, collapsed: &std::collections::HashSet<String>) {
        fn walk(node: &mut Node, prefix: &str, collapsed: &std::collections::HashSet<String>) {
            for child in &mut node.children {
                if child.file.is_none() {
                    let path = if prefix.is_empty() {
                        child.name.clone()
                    } else {
                        format!("{prefix}/{}", child.name)
                    };
                    if collapsed.contains(&path) {
                        child.expanded = false;
                    }
                    walk(child, &path, collapsed);
                }
            }
        }
        walk(&mut self.root, "", collapsed);
        self.selected = self.selected.min(self.len().saturating_sub(1));
    }

    /// Child-index path from the root to the selected visible node.
    fn selected_path(&self) -> Option<Vec<usize>> {
        let mut paths = Vec::new();
        fn walk(node: &Node, prefix: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
            for (i, child) in node.children.iter().enumerate() {
                prefix.push(i);
                out.push(prefix.clone());
                if child.expanded {
                    walk(child, prefix, out);
                }
                prefix.pop();
            }
        }
        walk(&self.root, &mut Vec::new(), &mut paths);
        paths.get(self.selected).cloned()
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let (border_style, title) = if focused {
            (
                Style::new().cyan(),
                Line::from(vec![" Files ".bold().black().on_cyan(), " ⏎ open ".cyan()]),
            )
        } else {
            (Style::new().dark_gray(), Line::from(" Files ".dark_gray()))
        };
        let block = Block::new()
            .borders(Borders::RIGHT)
            .border_style(border_style)
            .title(title);

        let items: Vec<ListItem> = self
            .visible()
            .iter()
            .map(|row| {
                let indent = "  ".repeat(row.depth);
                let mut spans = vec![Span::raw(indent)];
                if row.node.file.is_none() {
                    let arrow = if row.node.expanded { "▾ " } else { "▸ " };
                    spans.push(arrow.dark_gray());
                    spans.push(format!("{}/", row.node.name).bold());
                } else {
                    let status = row.node.status.map(|s| s.letter()).unwrap_or(' ');
                    let status_span = match row.node.status {
                        Some(FileStatus::Added) => format!("{status} ").green(),
                        Some(FileStatus::Deleted) => format!("{status} ").red(),
                        _ => format!("{status} ").yellow(),
                    };
                    spans.push(status_span);
                    spans.push(Span::raw(row.node.name.clone()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        // Active: bright selection bar for picking. Passive: a dim
        // "you are here" marker that follows the diff as it scrolls.
        let highlight = if focused {
            Style::new().black().on_cyan().bold()
        } else {
            Style::new().on_dark_gray()
        };
        self.last_inner.set(block.inner(area));
        let list = List::new(items).block(block).highlight_style(highlight);
        self.state.select(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

fn node_at_mut<'a>(root: &'a mut Node, path: &[usize]) -> &'a mut Node {
    let mut node = root;
    for &i in path {
        node = &mut node.children[i];
    }
    node
}

fn sort_children(node: &mut Node) {
    node.children
        .sort_by(|a, b| match (a.file.is_none(), b.file.is_none()) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
    for child in &mut node.children {
        sort_children(child);
    }
}
