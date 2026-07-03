use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use git2::Repository;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::comments::{CommentStore, place};
use crate::event::{AppEvent, LoadRequest, spawn_loader};
use crate::git::diff::{self, DiffResult};
use crate::git::log::commit_log;
use crate::git::scope::{Scope, base_candidates, detect_base, file_content, file_count};
use crate::ui::fileview::FileView;
use crate::ui::highlight::Highlighter;
use crate::ui::picker::{BasePicker, LogPicker, ScopeAction, ScopeItem, ScopePicker};
use crate::ui::review::{CommentTarget, Stream};
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
    /// Full-file pager overlay (`o`).
    file_view: Option<FileView>,
}

impl ReviewState {
    fn new(scope: Scope, diff: DiffResult, store: &CommentStore) -> Self {
        let placed = place(&diff, &store.comments);
        let stream = Stream::new(&diff, &placed, &store.comments);
        let tree = FileTree::new(&diff.files, &comment_counts(&diff, store));
        ReviewState {
            scope,
            diff,
            stream,
            tree,
            focus: Focus::Stream,
            show_tree: true,
            file_view: None,
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
    /// Transient confirmation shown in the title bar (e.g. "copied 12 lines").
    notice: Option<String>,
    /// $EDITOR launch requested by `E`; handled in the run loop where the
    /// terminal handle is available for suspend/resume.
    pending_editor: Option<(std::path::PathBuf, usize)>,
    highlighter: Highlighter,
    store: CommentStore,
    /// Comment being typed; Some while the editor popup is open.
    comment_draft: Option<CommentDraft>,
    /// `D` pressed: waiting for y/n to delete every comment.
    confirm_clear: bool,
}

struct CommentDraft {
    body: String,
    /// Some(id) when editing an existing comment.
    editing: Option<u64>,
    /// None when editing (anchor is kept from the original).
    target: Option<CommentTarget>,
    label: String,
}

const TREE_WIDTH: u16 = 32;

impl App {
    /// `initial`: a scope pre-resolved from CLI flags, already loaded so
    /// errors surface before the terminal is put into raw mode.
    pub fn new(repo: Repository, initial: Option<(Scope, DiffResult)>) -> Self {
        let store = CommentStore::load(&repo);
        let screen = match initial {
            Some((scope, diff)) => Screen::Review(Box::new(ReviewState::new(scope, diff, &store))),
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
            notice: None,
            pending_editor: None,
            highlighter: Highlighter::new(),
            store,
            comment_draft: None,
            confirm_clear: false,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.on_key(key.code, key.modifiers);
                    }
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    _ => {}
                }
            }
            while let Ok(app_event) = self.events.try_recv() {
                self.on_app_event(app_event);
            }
            if let Some((path, line)) = self.pending_editor.take() {
                self.run_editor(terminal, &path, line);
            }
        }
        Ok(())
    }

    /// Suspend the TUI, run $EDITOR at file:line, resume. Any edits the
    /// editor saves are picked up by the auto-reload watcher.
    fn run_editor(&mut self, terminal: &mut DefaultTerminal, path: &std::path::Path, line: usize) {
        use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        use ratatui::crossterm::execute;
        use ratatui::crossterm::terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        };

        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| "vi".into());
        let mut parts = editor.split_whitespace();
        let Some(program) = parts.next() else {
            return;
        };
        let args: Vec<&str> = parts.collect();

        let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        let _ = disable_raw_mode();
        let status = std::process::Command::new(program)
            .args(&args)
            .arg(format!("+{line}"))
            .arg(path)
            .status();
        let _ = enable_raw_mode();
        let _ = execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture);
        let _ = terminal.clear();

        match status {
            Ok(st) if st.success() => {}
            Ok(st) => self.error = Some(format!("{editor} exited with {st}")),
            Err(e) => self.error = Some(format!("failed to launch {editor}: {e}")),
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let (col, row) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollDown => self.mouse_scroll(1, col, row),
            MouseEventKind::ScrollUp => self.mouse_scroll(-1, col, row),
            MouseEventKind::Down(MouseButton::Left) => self.mouse_click(col, row),
            _ => {}
        }
    }

    fn mouse_scroll(&mut self, direction: isize, col: u16, row: u16) {
        match &mut self.screen {
            Screen::Review(review) => {
                // Wheel over an *active* tree moves its selection; everywhere
                // else it scrolls the diff (the tree is passive by default).
                if review.focus == Focus::Tree && review.tree.hit(col, row).is_some() {
                    review.tree.move_selection(direction);
                } else {
                    review.stream.scroll_by(direction * 3);
                    sync_tree(review);
                }
            }
            Screen::Picker(picker) => picker.move_selection(direction),
            Screen::Log(log) => log.move_selection(direction),
            Screen::Base(base) => base.move_selection(direction),
        }
    }

    fn mouse_click(&mut self, col: u16, row: u16) {
        if self.show_help {
            self.show_help = false;
            return;
        }
        match &mut self.screen {
            Screen::Review(review) => {
                if let Some(idx) = review.tree.hit(col, row) {
                    review.tree.selected = idx;
                    if let Some(file_idx) = review.tree.activate() {
                        review.stream.jump_to_file(file_idx);
                        review.focus = Focus::Stream;
                    }
                }
            }
            // The scope picker is a menu: click opens directly.
            Screen::Picker(picker) => {
                if let Some(idx) = picker.hit(col, row) {
                    picker.selected = idx;
                    self.activate_picker_item();
                }
            }
            // Long lists: first click selects, a second click on the
            // selected row opens.
            Screen::Log(log) => {
                if let Some(idx) = log.hit(col, row) {
                    if log.selected == idx {
                        self.open_selected_commit();
                    } else {
                        log.selected = idx;
                    }
                }
            }
            Screen::Base(base) => {
                if let Some(idx) = base.hit(col, row) {
                    if base.selected == idx {
                        self.open_selected_base();
                    } else {
                        base.selected = idx;
                    }
                }
            }
        }
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
                            apply_reload(review, diff, &self.store);
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
            Screen::Review(review) => draw_review(
                review,
                frame,
                main_area,
                &self.highlighter,
                &self.store.comments,
            ),
        }

        match &self.search_input {
            Some(query) => bar::render_search_input(frame, bar_area, query),
            None => bar::render(frame, bar_area, &self.hints()),
        }

        if let Some(draft) = &self.comment_draft {
            popups::render_comment_editor(frame, frame.area(), &draft.label, &draft.body);
        }
        if self.confirm_clear {
            let n = self.store.comments.len();
            let msg = if n == 1 {
                "delete the 1 comment? [y/n]".to_string()
            } else {
                format!("delete all {n} comments? [y/n]")
            };
            popups::render_confirm(frame, frame.area(), &msg);
        }
        if self.show_help {
            popups::render_help(frame, frame.area());
        }
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![" gittre ".bold().black().on_cyan()];
        if let Some(err) = &self.error {
            spans.push(format!(" {err}").red().bold());
        } else if let Some(notice) = &self.notice {
            spans.push(format!(" ✔ {notice}").green());
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
                    if !self.store.comments.is_empty() {
                        spans.push(format!("  ✎ {}", self.store.comments.len()).cyan());
                    }
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
        if self.comment_draft.is_some() {
            return vec![("⏎", "save"), ("Alt+⏎", "newline"), ("Esc", "cancel")];
        }
        if self.confirm_clear {
            return vec![("y", "delete all comments"), ("any key", "cancel")];
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
            Screen::Log(log) => {
                if log.marked.is_some() {
                    vec![
                        ("↑↓/jk", "select"),
                        ("⏎", "review marked ⇢ selected"),
                        ("Space", "unmark"),
                        ("q/Esc", "back"),
                    ]
                } else {
                    vec![
                        ("↑↓/jk", "select"),
                        ("⏎", "review commit"),
                        ("Space", "mark range start"),
                        ("q/Esc", "back"),
                        ("?", "help"),
                    ]
                }
            }
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
                if review.file_view.is_some() {
                    return vec![
                        ("↑↓/jk", "scroll"),
                        ("g/G", "top/bottom"),
                        ("E", "open in $EDITOR"),
                        ("q/Esc", "back to diff"),
                    ];
                }
                if review.stream.has_selection() {
                    return vec![
                        ("↑↓/jk", "extend"),
                        ("c", "comment"),
                        ("y", "copy code"),
                        ("Y", "copy patch"),
                        ("Esc/v", "cancel"),
                    ];
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
                hints.push(("c", "comment"));
                if review.stream.has_comments() {
                    hints.push(("}/{", "comment nav"));
                }
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
        self.notice = None;

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
        if self.comment_draft.is_some() {
            self.on_draft_key(code, modifiers);
            return;
        }
        if self.confirm_clear {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_clear = false;
                    if let Err(e) = self.store.delete_all() {
                        self.error = Some(format!("{e:#}"));
                    } else {
                        self.notice = Some("all comments deleted".into());
                    }
                    self.rebuild_review();
                }
                _ => self.confirm_clear = false,
            }
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

    fn on_draft_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let Some(draft) = &mut self.comment_draft else {
            return;
        };
        match code {
            KeyCode::Esc => self.comment_draft = None,
            KeyCode::Backspace => {
                draft.body.pop();
            }
            KeyCode::Enter if modifiers.contains(KeyModifiers::ALT) => draft.body.push('\n'),
            KeyCode::Enter => {
                let draft = self.comment_draft.take().unwrap();
                if draft.body.trim().is_empty() {
                    self.notice = Some("empty comment discarded".into());
                    return;
                }
                let scope_label = match &self.screen {
                    Screen::Review(r) => r.scope.label(),
                    _ => String::new(),
                };
                let result = match (draft.editing, draft.target) {
                    (Some(id), _) => self.store.edit(id, draft.body),
                    (None, Some(t)) => self.store.add(
                        t.path,
                        t.new_side,
                        t.lines,
                        t.snippet,
                        draft.body,
                        scope_label,
                    ),
                    (None, None) => Ok(()),
                };
                if let Err(e) = result {
                    self.error = Some(format!("{e:#}"));
                }
                self.rebuild_review();
            }
            KeyCode::Char(c) => draft.body.push(c),
            _ => {}
        }
    }

    /// Re-place comments and rebuild the stream, preserving position.
    fn rebuild_review(&mut self) {
        let Screen::Review(review) = &mut self.screen else {
            return;
        };
        let anchor = review.stream.anchor(&review.diff.files);
        let query = review.stream.search_query();
        let placed = place(&review.diff, &self.store.comments);
        review.stream = Stream::new(&review.diff, &placed, &self.store.comments);
        let collapsed = review.tree.collapsed_dirs();
        review.tree = FileTree::new(
            &review.diff.files,
            &comment_counts(&review.diff, &self.store),
        );
        review.tree.apply_collapsed(&collapsed);
        if let Some(q) = &query {
            review.stream.set_search(q, &review.diff.files);
        }
        if let Some(a) = &anchor {
            review.stream.restore(a, &review.diff.files);
        }
        sync_tree(review);
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
                    self.screen = Screen::Log(LogPicker::new(entries));
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            },
            ScopeAction::PickBase => {
                let names = base_candidates(&self.repo);
                if names.is_empty() {
                    self.error =
                        Some("only one branch and no upstream — nothing to compare".into());
                } else {
                    self.screen = Screen::Base(BasePicker::new(names));
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
            KeyCode::Enter => self.open_selected_base(),
            _ => {}
        }
    }

    fn open_selected_base(&mut self) {
        let Screen::Base(base) = &self.screen else {
            return;
        };
        if let Some(name) = base.names.get(base.selected) {
            let scope = Scope::Branch { base: name.clone() };
            self.open_scope(scope);
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
            KeyCode::Char(' ') => log.toggle_mark(),
            KeyCode::Enter => self.open_selected_commit(),
            _ => {}
        }
    }

    fn open_selected_commit(&mut self) {
        let Screen::Log(log) = &self.screen else {
            return;
        };
        let Some(entry) = log.entries.get(log.selected) else {
            return;
        };
        let scope = match log.marked {
            // A marked base + a different selection = review the range.
            Some(from) if from != entry.id => Scope::Range { from, to: entry.id },
            _ => Scope::Commit {
                id: entry.id,
                summary: entry.summary.clone(),
            },
        };
        self.open_scope(scope);
    }

    fn on_review_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let Screen::Review(review) = &mut self.screen else {
            return;
        };
        if let Some(view) = &mut review.file_view {
            match code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('o') => {
                    review.file_view = None;
                }
                KeyCode::Char('j') | KeyCode::Down => view.scroll_by(1),
                KeyCode::Char('k') | KeyCode::Up => view.scroll_by(-1),
                KeyCode::PageDown => view.page(1),
                KeyCode::PageUp => view.page(-1),
                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => view.page(1),
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => view.page(-1),
                KeyCode::Char('g') | KeyCode::Home => view.scroll_to_top(),
                KeyCode::Char('G') | KeyCode::End => view.scroll_to_bottom(),
                KeyCode::Char('E') => {
                    let line = view.top_line();
                    let path = view.path.clone();
                    self.request_editor(&path, line);
                }
                _ => {}
            }
            return;
        }
        if review.stream.has_selection() {
            match code {
                KeyCode::Esc | KeyCode::Char('v') => review.stream.cancel_selection(),
                KeyCode::Char('j') | KeyCode::Down => review.stream.move_cursor(1),
                KeyCode::Char('k') | KeyCode::Up => review.stream.move_cursor(-1),
                KeyCode::PageDown => review.stream.move_cursor(20),
                KeyCode::PageUp => review.stream.move_cursor(-20),
                KeyCode::Char('y') => self.copy_selection(false),
                KeyCode::Char('Y') => self.copy_selection(true),
                KeyCode::Char('c') => {
                    if let Some(t) = review.stream.selection_target(&review.diff.files) {
                        review.stream.cancel_selection();
                        self.comment_draft = Some(CommentDraft {
                            body: String::new(),
                            editing: None,
                            label: target_label(&t),
                            target: Some(t),
                        });
                    }
                }
                _ => {}
            }
            return;
        }
        match code {
            // Esc peels back one layer at a time: live search → tree pick → picker.
            KeyCode::Esc if review.stream.has_search() => review.stream.clear_search(),
            KeyCode::Esc if review.focus == Focus::Tree => {
                review.focus = Focus::Stream;
                sync_tree(review);
            }
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('x') => self.open_picker(),
            KeyCode::Char('/') => self.search_input = Some(String::new()),
            KeyCode::Char('v') if review.focus == Focus::Stream => {
                review.stream.start_selection();
            }
            KeyCode::Char('o') => self.open_file_view(),
            KeyCode::Char('c') => {
                if let Some(ci) = review.stream.comment_at_top() {
                    let c = &self.store.comments[ci];
                    self.comment_draft = Some(CommentDraft {
                        body: c.body.clone(),
                        editing: Some(c.id),
                        target: None,
                        label: format!("edit #{} on {}", c.id, c.path),
                    });
                } else if let Some(t) = review.stream.line_target(&review.diff.files) {
                    self.comment_draft = Some(CommentDraft {
                        body: String::new(),
                        editing: None,
                        label: target_label(&t),
                        target: Some(t),
                    });
                }
            }
            KeyCode::Char('d') if !modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(ci) = review.stream.comment_at_top() {
                    let id = self.store.comments[ci].id;
                    if let Err(e) = self.store.delete(id) {
                        self.error = Some(format!("{e:#}"));
                    } else {
                        self.notice = Some(format!("deleted comment #{id}"));
                    }
                    self.rebuild_review();
                }
            }
            KeyCode::Char('D') if !self.store.comments.is_empty() => {
                self.confirm_clear = true;
            }
            KeyCode::Char('}') => {
                review.stream.next_comment();
                sync_tree(review);
            }
            KeyCode::Char('{') => {
                review.stream.prev_comment();
                sync_tree(review);
            }
            KeyCode::Char('E') => {
                if let Some((fi, line)) = review.stream.current_position(&review.diff.files) {
                    let path = review.diff.files[fi].path.clone();
                    let line = line.unwrap_or(1) as usize;
                    self.request_editor(&path, line);
                }
            }
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

    fn open_file_view(&mut self) {
        let Screen::Review(review) = &mut self.screen else {
            return;
        };
        let Some((fi, line)) = review.stream.current_position(&review.diff.files) else {
            return;
        };
        let path = review.diff.files[fi].path.clone();
        match file_content(&self.repo, &review.scope, &path) {
            Ok((content, source)) => {
                let target = line.map(|l| l.saturating_sub(1) as usize);
                review.file_view = Some(FileView::new(
                    path,
                    source,
                    &content,
                    target,
                    &self.highlighter,
                ));
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Queue an $EDITOR launch if the file exists on disk.
    fn request_editor(&mut self, path: &str, line: usize) {
        let Some(workdir) = self.repo.workdir() else {
            self.error = Some("no working directory".into());
            return;
        };
        let full = workdir.join(path);
        if !full.exists() {
            self.error = Some(format!("{path} is not on disk (deleted or historical)"));
            return;
        }
        self.pending_editor = Some((full, line.max(1)));
    }

    fn copy_selection(&mut self, patch_style: bool) {
        let Screen::Review(review) = &mut self.screen else {
            return;
        };
        let Some(text) = review.stream.selected_text(&review.diff.files, patch_style) else {
            self.error = Some("selection has no copyable lines".into());
            return;
        };
        let lines = text.lines().count();
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => {
                let style = if patch_style { "as patch" } else { "as code" };
                self.notice = Some(format!(
                    "copied {} line{} {style}",
                    lines,
                    if lines == 1 { "" } else { "s" }
                ));
                review.stream.cancel_selection();
            }
            Err(e) => self.error = Some(format!("clipboard: {e}")),
        }
    }

    fn open_scope(&mut self, scope: Scope) {
        self.seq += 1; // invalidate any in-flight reload
        self.reloading = false;
        match diff::load(&self.repo, &scope) {
            Ok(diff) => {
                self.screen = Screen::Review(Box::new(ReviewState::new(scope, diff, &self.store)))
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn open_picker(&mut self) {
        self.screen = Screen::Picker(build_picker(&self.repo));
    }
}

fn comment_counts(diff: &DiffResult, store: &CommentStore) -> Vec<usize> {
    diff.files
        .iter()
        .map(|f| store.count_for_path(&f.path))
        .collect()
}

fn target_label(t: &CommentTarget) -> String {
    if t.lines.0 == t.lines.1 {
        format!("{}:{}", t.path, t.lines.0)
    } else {
        format!("{}:{}-{}", t.path, t.lines.0, t.lines.1)
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
    ScopePicker::new(items)
}

fn draw_review(
    review: &mut ReviewState,
    frame: &mut Frame,
    area: Rect,
    hl: &Highlighter,
    comments: &[crate::comments::Comment],
) {
    if let Some(view) = &review.file_view {
        view.render(frame, area);
        return;
    }
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
            comments,
            review.focus == Focus::Stream,
            hl,
        );
    } else {
        review
            .stream
            .render(frame, area, &review.diff.files, comments, true, hl);
    }
}

/// Swap in a freshly loaded diff, preserving the reader's position (by file
/// path + offset), the tree's fold state, and focus.
fn apply_reload(review: &mut ReviewState, diff: DiffResult, store: &CommentStore) {
    let anchor = review.stream.anchor(&review.diff.files);
    let collapsed = review.tree.collapsed_dirs();
    let query = review.stream.search_query();

    review.diff = diff;
    let placed = place(&review.diff, &store.comments);
    review.stream = Stream::new(&review.diff, &placed, &store.comments);
    review.tree = FileTree::new(&review.diff.files, &comment_counts(&review.diff, store));
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
