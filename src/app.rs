use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use git2::Repository;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::event::{AppEvent, LoadRequest, spawn_loader};
use crate::git::diff::{self, DiffResult};
use crate::git::log::commit_log;
use crate::git::scope::{Scope, base_candidates, detect_base, file_count};
use crate::ui::picker::{BasePicker, LogPicker, ScopeAction, ScopeItem, ScopePicker};
use crate::ui::review::Stream;
use crate::ui::tree::FileTree;
use crate::ui::{bar, popups};
use crate::watch::{self, RepoWatcher};

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Tree,
    Stream,
}

struct ReviewState {
    scope: Scope,
    diff: DiffResult,
    stream: Stream,
    tree: FileTree,
    focus: Focus,
    show_tree: bool,
}

impl ReviewState {
    fn new(scope: Scope, diff: DiffResult) -> Self {
        let stream = Stream::new(&diff);
        let tree = FileTree::new(&diff.files);
        ReviewState {
            scope,
            diff,
            stream,
            tree,
            focus: Focus::Stream,
            show_tree: true,
        }
    }
}

enum Screen {
    Picker(ScopePicker),
    Log(LogPicker),
    Base(BasePicker),
    Review(Box<ReviewState>),
}

pub struct App {
    repo: Repository,
    screen: Screen,
    show_help: bool,
    error: Option<String>,
    quit: bool,
    events: Receiver<AppEvent>,
    loader: Sender<LoadRequest>,
    _watcher: Option<RepoWatcher>,
    /// Monotonic id pairing reload requests with responses.
    seq: u64,
    reloading: bool,
    reloaded_at: Option<Instant>,
    /// In-progress `/` query; Some while the user is typing it.
    search_input: Option<String>,
}

const TREE_WIDTH: u16 = 32;

impl App {
    /// `initial`: a scope pre-resolved from CLI flags, already loaded so
    /// errors surface before the terminal is put into raw mode.
    pub fn new(repo: Repository, initial: Option<(Scope, DiffResult)>) -> Self {
        let screen = match initial {
            Some((scope, diff)) => Screen::Review(Box::new(ReviewState::new(scope, diff))),
            None => Screen::Picker(build_picker(&repo)),
        };
        let (event_tx, events) = channel();
        let loader_path = repo.workdir().unwrap_or_else(|| repo.path()).to_path_buf();
        let loader = spawn_loader(loader_path, event_tx.clone());
        let watcher = watch::spawn(&repo, event_tx);
        App {
            repo,
            screen,
            show_help: false,
            error: None,
            quit: false,
            events,
            loader,
            _watcher: watcher,
            seq: 0,
            reloading: false,
            reloaded_at: None,
            search_input: None,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.on_key(key.code, key.modifiers);
                    }
                }
            }
            while let Ok(app_event) = self.events.try_recv() {
                self.on_app_event(app_event);
            }
        }
        Ok(())
    }

    fn on_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::RepoChanged => match &self.screen {
                Screen::Review(review) => {
                    self.seq += 1;
                    self.reloading = true;
                    let _ = self.loader.send((self.seq, review.scope.clone()));
                }
                // Keep the picker's counts fresh too.
                Screen::Picker(picker) => {
                    let selected = picker.selected;
                    let mut rebuilt = build_picker(&self.repo);
                    rebuilt.selected = selected.min(rebuilt.items.len().saturating_sub(1));
                    self.screen = Screen::Picker(rebuilt);
                }
                _ => {}
            },
            AppEvent::DiffLoaded { seq, diff } => {
                if seq != self.seq {
                    return; // superseded by a newer request or scope switch
                }
                self.reloading = false;
                if let Screen::Review(review) = &mut self.screen {
                    match diff {
                        Ok(diff) => {
                            apply_reload(review, diff);
                            self.reloaded_at = Some(Instant::now());
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
            }
        }
    }

    // ---- drawing ----------------------------------------------------------

    fn draw(&mut self, frame: &mut Frame) {
        let [title_area, main_area, bar_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .areas(frame.area());

        self.draw_title(frame, title_area);

        match &mut self.screen {
            Screen::Picker(picker) => picker.render(frame, main_area),
            Screen::Log(log) => log.render(frame, main_area),
            Screen::Base(base) => base.render(frame, main_area),
            Screen::Review(review) => draw_review(review, frame, main_area),
        }

        match &self.search_input {
            Some(query) => bar::render_search_input(frame, bar_area, query),
            None => bar::render(frame, bar_area, &self.hints()),
        }

        if self.show_help {
            popups::render_help(frame, frame.area());
        }
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![" gittre ".bold().black().on_cyan()];
        if let Some(err) = &self.error {
            spans.push(format!(" {err}").red().bold());
        } else {
            match &self.screen {
                Screen::Picker(_) => spans.push(" select what to review".dark_gray()),
                Screen::Log(_) => spans.push(" pick a commit to review".dark_gray()),
                Screen::Base(_) => spans.push(" pick a base branch".dark_gray()),
                Screen::Review(r) => {
                    spans.push(format!(" {}", r.scope.label()).bold());
                    spans.push(format!("  {} ", count_label(r.diff.files.len())).into());
                    spans.push(format!("+{} ", r.diff.additions).green());
                    spans.push(format!("−{}", r.diff.deletions).red());
                    if self.reloading {
                        spans.push("  ↻ reloading…".dark_gray());
                    } else if self
                        .reloaded_at
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
                    {
                        spans.push("  ↻ reloaded".dark_gray());
                    }
                }
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.show_help {
            return vec![("q/Esc/?", "close help")];
        }
        if self.search_input.is_some() {
            return vec![("⏎", "search"), ("Esc", "cancel")];
        }
        match &self.screen {
            Screen::Picker(picker) => vec![
                ("1-9", "open"),
                ("↑↓/jk", "select"),
                ("⏎", "open"),
                ("?", "help"),
                ("q", "quit"),
            ]
            .into_iter()
            .take(if picker.items.is_empty() { 2 } else { 5 })
            .collect(),
            Screen::Log(_) => vec![
                ("↑↓/jk", "select"),
                ("⏎", "review commit"),
                ("q/Esc", "back"),
                ("?", "help"),
            ],
            Screen::Base(_) => vec![
                ("↑↓/jk", "select"),
                ("⏎", "compare against"),
                ("q/Esc", "back"),
                ("?", "help"),
            ],
            Screen::Review(review) => {
                if review.diff.files.is_empty() {
                    return vec![("x/q", "switch scope"), ("?", "help")];
                }
                if review.focus == Focus::Tree {
                    return vec![
                        ("↑↓/jk", "select"),
                        ("⏎", "open"),
                        ("Esc/Tab", "back to diff"),
                        ("x/q", "scope"),
                        ("?", "help"),
                    ];
                }
                let mut hints: Vec<(&str, &str)> = vec![("↑↓/jk", "scroll"), ("]/[", "file")];
                if review.stream.has_search() {
                    hints.push(("n/N", "match"));
                    hints.push(("Esc", "clear search"));
                } else {
                    hints.push(("n/p", "hunk"));
                }
                hints.push(("/", "search"));
                hints.push(("Tab", "pick file"));
                hints.push(if review.show_tree {
                    ("t", "hide tree")
                } else {
                    ("t", "show tree")
                });
                hints.extend([("x/q", "scope"), ("?", "help")]);
                hints
            }
        }
    }

    // ---- input ------------------------------------------------------------

    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.error = None;

        if self.show_help {
            if matches!(code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('?')) {
                self.show_help = false;
            }
            return;
        }
        // Raw mode swallows the usual SIGINT, so honor Ctrl-C ourselves.
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.search_input.is_some() {
            self.on_search_input_key(code);
            return;
        }
        if code == KeyCode::Char('?') {
            self.show_help = true;
            return;
        }

        match &mut self.screen {
            Screen::Picker(_) => self.on_picker_key(code),
            Screen::Log(_) => self.on_log_key(code),
            Screen::Base(_) => self.on_base_key(code),
            Screen::Review(_) => self.on_review_key(code, modifiers),
        }
    }

    fn on_search_input_key(&mut self, code: KeyCode) {
        let Some(input) = &mut self.search_input else {
            return;
        };
        match code {
            KeyCode::Esc => self.search_input = None,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => {
                let query = self.search_input.take().unwrap_or_default();
                if query.is_empty() {
                    return;
                }
                if let Screen::Review(review) = &mut self.screen {
                    let count = review.stream.set_search(&query, &review.diff.files);
                    if count == 0 {
                        self.error = Some(format!("no matches for \u{2018}{query}\u{2019}"));
                    } else {
                        review.focus = Focus::Stream;
                        sync_tree(review);
                    }
                }
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
    }

    fn on_picker_key(&mut self, code: KeyCode) {
        let Screen::Picker(picker) = &mut self.screen else {
            return;
        };
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => picker.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => picker.move_selection(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                if idx < picker.items.len() {
                    picker.selected = idx;
                    self.activate_picker_item();
                }
            }
            KeyCode::Enter => self.activate_picker_item(),
            _ => {}
        }
    }

    fn activate_picker_item(&mut self) {
        let action = {
            let Screen::Picker(picker) = &self.screen else {
                return;
            };
            let Some(item) = picker.items.get(picker.selected) else {
                return;
            };
            match &item.action {
                ScopeAction::Open(scope) => ScopeAction::Open(scope.clone()),
                ScopeAction::PickCommit => ScopeAction::PickCommit,
                ScopeAction::PickBase => ScopeAction::PickBase,
            }
        };
        match action {
            ScopeAction::Open(scope) => self.open_scope(scope),
            ScopeAction::PickCommit => match commit_log(&self.repo) {
                Ok(entries) if entries.is_empty() => {
                    self.error = Some("no commits yet".into());
                }
                Ok(entries) => {
                    self.screen = Screen::Log(LogPicker {
                        entries,
                        selected: 0,
                    });
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            },
            ScopeAction::PickBase => {
                let names = base_candidates(&self.repo);
                if names.is_empty() {
                    self.error =
                        Some("only one branch and no upstream — nothing to compare".into());
                } else {
                    self.screen = Screen::Base(BasePicker { names, selected: 0 });
                }
            }
        }
    }

    fn on_base_key(&mut self, code: KeyCode) {
        let Screen::Base(base) = &mut self.screen else {
            return;
        };
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.open_picker(),
            KeyCode::Char('j') | KeyCode::Down => base.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => base.move_selection(-1),
            KeyCode::PageDown => base.move_selection(20),
            KeyCode::PageUp => base.move_selection(-20),
            KeyCode::Char('g') | KeyCode::Home => base.selected = 0,
            KeyCode::Char('G') | KeyCode::End => base.selected = base.names.len().saturating_sub(1),
            KeyCode::Enter => {
                if let Some(name) = base.names.get(base.selected) {
                    let scope = Scope::Branch { base: name.clone() };
                    self.open_scope(scope);
                }
            }
            _ => {}
        }
    }

    fn on_log_key(&mut self, code: KeyCode) {
        let Screen::Log(log) = &mut self.screen else {
            return;
        };
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.open_picker(),
            KeyCode::Char('j') | KeyCode::Down => log.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => log.move_selection(-1),
            KeyCode::PageDown => log.move_selection(20),
            KeyCode::PageUp => log.move_selection(-20),
            KeyCode::Char('g') | KeyCode::Home => log.selected = 0,
            KeyCode::Char('G') | KeyCode::End => log.selected = log.entries.len().saturating_sub(1),
            KeyCode::Enter => {
                if let Some(entry) = log.entries.get(log.selected) {
                    let scope = Scope::Commit {
                        id: entry.id,
                        summary: entry.summary.clone(),
                    };
                    self.open_scope(scope);
                }
            }
            _ => {}
        }
    }

    fn on_review_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let Screen::Review(review) = &mut self.screen else {
            return;
        };
        match code {
            // Esc peels back one layer at a time: live search → tree pick → picker.
            KeyCode::Esc if review.stream.has_search() => review.stream.clear_search(),
            KeyCode::Esc if review.focus == Focus::Tree => {
                review.focus = Focus::Stream;
                sync_tree(review);
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('x') => self.open_picker(),
            KeyCode::Char('/') => self.search_input = Some(String::new()),
            KeyCode::Char('n') if review.stream.has_search() => {
                review.stream.next_match();
                sync_tree(review);
            }
            KeyCode::Char('N') if review.stream.has_search() => {
                review.stream.prev_match();
                sync_tree(review);
            }
            KeyCode::Char('t') => {
                review.show_tree = !review.show_tree;
                if !review.show_tree {
                    review.focus = Focus::Stream;
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if !review.show_tree {
                    review.show_tree = true;
                }
                review.focus = match review.focus {
                    Focus::Tree => Focus::Stream,
                    Focus::Stream => Focus::Tree,
                };
                // Start the pick from the file being read, not row zero.
                if review.focus == Focus::Tree {
                    if let Some(fi) = review.stream.current_file() {
                        review.tree.select_file(fi);
                    }
                } else {
                    sync_tree(review);
                }
            }
            KeyCode::Char('j') | KeyCode::Down => match review.focus {
                Focus::Tree => review.tree.move_selection(1),
                Focus::Stream => {
                    review.stream.scroll_by(1);
                    sync_tree(review);
                }
            },
            KeyCode::Char('k') | KeyCode::Up => match review.focus {
                Focus::Tree => review.tree.move_selection(-1),
                Focus::Stream => {
                    review.stream.scroll_by(-1);
                    sync_tree(review);
                }
            },
            KeyCode::PageDown => review.stream.page(1),
            KeyCode::PageUp => review.stream.page(-1),
            KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                review.stream.page(1)
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                review.stream.page(-1)
            }
            KeyCode::Char('g') | KeyCode::Home => {
                review.stream.scroll_to_top();
                sync_tree(review);
            }
            KeyCode::Char('G') | KeyCode::End => {
                review.stream.scroll_to_bottom();
                sync_tree(review);
            }
            KeyCode::Char(']') => {
                review.stream.next_file();
                sync_tree(review);
            }
            KeyCode::Char('[') => {
                review.stream.prev_file();
                sync_tree(review);
            }
            KeyCode::Char('n') => {
                review.stream.next_hunk();
                sync_tree(review);
            }
            KeyCode::Char('p') => {
                review.stream.prev_hunk();
                sync_tree(review);
            }
            KeyCode::Enter => {
                if review.focus == Focus::Tree {
                    if let Some(file_idx) = review.tree.activate() {
                        review.stream.jump_to_file(file_idx);
                        review.focus = Focus::Stream;
                    }
                }
            }
            _ => {}
        }
    }

    // ---- transitions ------------------------------------------------------

    fn open_scope(&mut self, scope: Scope) {
        self.seq += 1; // invalidate any in-flight reload
        self.reloading = false;
        match diff::load(&self.repo, &scope) {
            Ok(diff) => self.screen = Screen::Review(Box::new(ReviewState::new(scope, diff))),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn open_picker(&mut self) {
        self.screen = Screen::Picker(build_picker(&self.repo));
    }
}

fn count_label(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

fn build_picker(repo: &Repository) -> ScopePicker {
    let mut items = vec![
        ScopeItem {
            title: "Uncommitted changes".into(),
            detail: count_label(file_count(repo, &Scope::Uncommitted)),
            action: ScopeAction::Open(Scope::Uncommitted),
        },
        ScopeItem {
            title: "Staged changes".into(),
            detail: count_label(file_count(repo, &Scope::Staged)),
            action: ScopeAction::Open(Scope::Staged),
        },
    ];
    match detect_base(repo) {
        Some(base) => {
            let scope = Scope::Branch { base: base.clone() };
            items.push(ScopeItem {
                title: format!("Branch vs {base}"),
                detail: count_label(file_count(repo, &scope)),
                action: ScopeAction::Open(scope),
            });
        }
        // Always show the entry; explain instead of silently hiding it.
        None => items.push(ScopeItem {
            title: "Branch vs a base you pick…".into(),
            detail: "no base auto-detected".into(),
            action: ScopeAction::PickBase,
        }),
    }
    items.push(ScopeItem {
        title: "A specific commit…".into(),
        detail: String::new(),
        action: ScopeAction::PickCommit,
    });
    ScopePicker { items, selected: 0 }
}

fn draw_review(review: &ReviewState, frame: &mut Frame, area: Rect) {
    if review.diff.files.is_empty() {
        let msg = Paragraph::new(Line::from(
            format!("✔ {}", review.scope.empty_message()).green(),
        ))
        .centered();
        let [_, center, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(area);
        frame.render_widget(msg, center);
        return;
    }

    if review.show_tree {
        let [tree_area, stream_area] = Layout::horizontal([
            Constraint::Length(TREE_WIDTH.min(area.width / 3)),
            Constraint::Min(0),
        ])
        .areas(area);
        review
            .tree
            .render(frame, tree_area, review.focus == Focus::Tree);
        review.stream.render(
            frame,
            stream_area,
            &review.diff.files,
            review.focus == Focus::Stream,
        );
    } else {
        review.stream.render(frame, area, &review.diff.files, true);
    }
}

/// Swap in a freshly loaded diff, preserving the reader's position (by file
/// path + offset), the tree's fold state, and focus.
fn apply_reload(review: &mut ReviewState, diff: DiffResult) {
    let anchor = review.stream.anchor(&review.diff.files);
    let collapsed = review.tree.collapsed_dirs();
    let query = review.stream.search_query();

    review.diff = diff;
    review.stream = Stream::new(&review.diff);
    review.tree = FileTree::new(&review.diff.files);
    review.tree.apply_collapsed(&collapsed);
    // Re-run a live search against the new content, but let the anchor
    // (applied below) win over the jump-to-first-match.
    if let Some(query) = query {
        review.stream.set_search(&query, &review.diff.files);
    }
    if let Some(anchor) = &anchor {
        review.stream.restore(anchor, &review.diff.files);
    }
    if review.focus == Focus::Tree {
        // Re-seed the pick from wherever the reader is.
        if let Some(fi) = review.stream.current_file() {
            review.tree.select_file(fi);
        }
    } else {
        sync_tree(review);
    }
}

/// Keep the tree highlight in sync with the file at the top of the stream.
/// Passive-mode only: never fights the user while they're picking in the tree.
fn sync_tree(review: &mut ReviewState) {
    if review.focus == Focus::Tree {
        return;
    }
    if let Some(fi) = review.stream.current_file() {
        review.tree.select_file(fi);
    }
}
