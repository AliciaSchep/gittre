use std::time::Duration;

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::git::diff::DiffResult;
use crate::ui::review::Stream;
use crate::ui::tree::FileTree;
use crate::ui::{bar, popups};

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Tree,
    Stream,
}

pub struct App {
    diff: DiffResult,
    stream: Stream,
    tree: FileTree,
    focus: Focus,
    show_tree: bool,
    show_help: bool,
    quit: bool,
}

const TREE_WIDTH: u16 = 32;

impl App {
    pub fn new(diff: DiffResult) -> Self {
        let stream = Stream::new(&diff);
        let tree = FileTree::new(&diff.files);
        App {
            diff,
            stream,
            tree,
            focus: Focus::Stream,
            show_tree: true,
            show_help: false,
            quit: false,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key.code, key.modifiers);
                    }
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [title_area, main_area, bar_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .areas(frame.area());

        self.draw_title(frame, title_area);

        if self.diff.files.is_empty() {
            let msg = Paragraph::new(Line::from(
                "✔ no uncommitted changes — working tree clean".green(),
            ))
            .centered();
            let [_, center, _] = Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(main_area);
            frame.render_widget(msg, center);
        } else if self.show_tree {
            let [tree_area, stream_area] = Layout::horizontal([
                Constraint::Length(TREE_WIDTH.min(frame.area().width / 3)),
                Constraint::Min(0),
            ])
            .areas(main_area);
            self.tree
                .render(frame, tree_area, self.focus == Focus::Tree);
            self.stream.render(
                frame,
                stream_area,
                &self.diff.files,
                self.focus == Focus::Stream,
            );
        } else {
            self.stream.render(frame, main_area, &self.diff.files, true);
        }

        bar::render(frame, bar_area, &self.hints());

        if self.show_help {
            popups::render_help(frame, frame.area());
        }
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            " gittre ".bold().black().on_cyan(),
            " uncommitted changes".bold(),
            format!("  {} files ", self.diff.files.len()).into(),
            format!("+{} ", self.diff.additions).green(),
            format!("−{}", self.diff.deletions).red(),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.show_help {
            return vec![("q/Esc/?", "close help")];
        }
        if self.diff.files.is_empty() {
            return vec![("?", "help"), ("q", "quit")];
        }
        let mut hints: Vec<(&str, &str)> = vec![("↑↓/jk", "scroll")];
        if self.focus == Focus::Tree {
            hints.push(("⏎", "open"));
        }
        hints.extend([("]/[", "file"), ("n/p", "hunk")]);
        if self.show_tree {
            hints.push(("Tab", "focus"));
            hints.push(("t", "hide tree"));
        } else {
            hints.push(("t", "show tree"));
        }
        hints.extend([("?", "help"), ("q", "quit")]);
        hints
    }

    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if self.show_help {
            if matches!(code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('?')) {
                self.show_help = false;
            }
            return;
        }

        match code {
            // Raw mode swallows the usual SIGINT, so honor Ctrl-C ourselves.
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('t') => {
                self.show_tree = !self.show_tree;
                if !self.show_tree {
                    self.focus = Focus::Stream;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if !self.show_tree {
                    self.show_tree = true;
                }
                self.focus = match self.focus {
                    Focus::Tree => Focus::Stream,
                    Focus::Stream => Focus::Tree,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_vertical(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_vertical(-1),
            KeyCode::PageDown => self.stream.page(1),
            KeyCode::PageUp => self.stream.page(-1),
            KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => self.stream.page(1),
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => self.stream.page(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.stream.scroll_to_top();
                self.sync_tree();
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.stream.scroll_to_bottom();
                self.sync_tree();
            }
            KeyCode::Char(']') => {
                self.stream.next_file();
                self.sync_tree();
            }
            KeyCode::Char('[') => {
                self.stream.prev_file();
                self.sync_tree();
            }
            KeyCode::Char('n') => {
                self.stream.next_hunk();
                self.sync_tree();
            }
            KeyCode::Char('p') => {
                self.stream.prev_hunk();
                self.sync_tree();
            }
            KeyCode::Enter => {
                if self.focus == Focus::Tree {
                    if let Some(file_idx) = self.tree.activate() {
                        self.stream.jump_to_file(file_idx);
                        self.focus = Focus::Stream;
                    }
                }
            }
            _ => {}
        }
    }

    fn move_vertical(&mut self, delta: isize) {
        match self.focus {
            Focus::Tree => self.tree.move_selection(delta),
            Focus::Stream => {
                self.stream.scroll_by(delta);
                self.sync_tree();
            }
        }
    }

    /// Keep the tree highlight in sync with the file at the top of the stream.
    fn sync_tree(&mut self) {
        if let Some(fi) = self.stream.current_file() {
            self.tree.select_file(fi);
        }
    }
}
