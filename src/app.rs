use std::collections::HashMap;
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

use crate::clipboard;
use crate::comments::{Comment, CommentStore, export_markdown, placements_for_display};
use crate::event::{AppEvent, FileLoadKind, LoadRequest, ScopeCounts, spawn_loader};
use crate::git::diff::{self, DiffResult, FileDiff, FileStatus};
use crate::git::log::commit_log;
use crate::git::scope::{Scope, base_candidates, file_content, forkable_branch};
use crate::keymap::{self, Action, KeyPress, Lookup};
use crate::ui::fileview::FileView;
use crate::ui::highlight::Highlighter;
use crate::ui::picker::{BasePicker, LogPicker, ScopeAction, ScopeItem, ScopePicker, count_label};
use crate::ui::review::{CommentTarget, Stream, StreamAnchor};
use crate::ui::tree::FileTree;
use crate::ui::{bar, editor::TextEditor, export::ExportPreview, popups};

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Tree,
    Stream,
}

const TREE_RESIZE_STEP: u16 = 4;
const MIN_MANUAL_TREE_WIDTH: u16 = 16;
const MIN_DIFF_WIDTH: u16 = 60;

struct ReviewState {
    scope: Scope,
    diff: DiffResult,
    stream: Stream,
    tree: FileTree,
    focus: Focus,
    show_tree: bool,
    /// Explicit width selected with `</>`; None uses content-aware sizing.
    tree_width: Option<u16>,
    /// Full review width from the last frame, including a hidden tree.
    rendered_review_width: u16,
    /// Full-file pager overlay (`o`).
    file_view: Option<FileView>,
    /// Canonical file diffs hidden behind inline full-context display
    /// overrides. Presence means the user wants the file expanded; loading
    /// distinguishes an outstanding background request from an installed
    /// override.
    full_context: HashMap<String, FullContextState>,
}

struct FullContextState {
    base: FileDiff,
    loading: bool,
}

enum FullContextToggle {
    Load {
        path: String,
        old_path: Option<String>,
    },
    Collapsed(Option<String>),
    Unavailable,
}

enum FullContextInstall {
    Installed(Option<String>),
    Ignored,
}

#[derive(Default)]
struct ReviewViewState {
    anchor: Option<StreamAnchor>,
    collapsed: std::collections::HashSet<String>,
    query: Option<String>,
}

impl ReviewState {
    fn new(scope: Scope, diff: DiffResult, store: &mut CommentStore) -> (Self, Option<String>) {
        let (stream, tree, warning) =
            Self::build_views(&diff, &diff, store, &ReviewViewState::default());
        (
            ReviewState {
                scope,
                diff,
                stream,
                tree,
                focus: Focus::Stream,
                show_tree: true,
                tree_width: None,
                rendered_review_width: 0,
                file_view: None,
                full_context: HashMap::new(),
            },
            warning,
        )
    }

    /// Re-place comments and rebuild the views while preserving reader state.
    fn rebuild(&mut self, store: &mut CommentStore) -> Option<String> {
        let view = self.capture_view();
        let canonical = self.canonical_diff();
        let (stream, tree, warning) = Self::build_views(&canonical, &self.diff, store, &view);
        self.install_views(stream, tree);
        warning
    }

    /// Swap in a freshly loaded diff while preserving reader state. Comment
    /// persistence failures hide inline comments but never block review.
    fn replace_diff(
        &mut self,
        diff: DiffResult,
        store: &mut CommentStore,
    ) -> (Option<String>, Vec<(String, Option<String>)>) {
        let view = self.capture_view();
        let wanted: Vec<String> = self.full_context.keys().cloned().collect();
        self.full_context.clear();
        for path in wanted {
            if let Some(file) = diff
                .files
                .iter()
                .find(|file| file.path == path && full_context_eligible(file))
            {
                self.full_context.insert(
                    path,
                    FullContextState {
                        base: file.clone(),
                        loading: true,
                    },
                );
            }
        }
        let requests = self
            .full_context
            .iter()
            .map(|(path, state)| (path.clone(), state.base.old_path.clone()))
            .collect();
        (self.install_diff(diff, store, &view), requests)
    }

    /// Replace a lazy stub without cloning every other file in the review.
    fn expand_file(
        &mut self,
        pos: usize,
        loaded: Vec<crate::git::diff::FileDiff>,
        store: &mut CommentStore,
    ) -> Option<String> {
        let view = self.capture_view();
        let mut files = std::mem::take(&mut self.diff.files);
        files.splice(pos..=pos, loaded);
        self.install_diff(DiffResult::from_files(files), store, &view)
    }

    fn capture_view(&self) -> ReviewViewState {
        ReviewViewState {
            anchor: self.stream.anchor(&self.diff.files),
            collapsed: self.tree.collapsed_dirs(),
            query: self.stream.search_query(),
        }
    }

    fn install_diff(
        &mut self,
        diff: DiffResult,
        store: &mut CommentStore,
        view: &ReviewViewState,
    ) -> Option<String> {
        let canonical = self.canonical_diff_for(&diff);
        let (stream, tree, warning) = Self::build_views(&canonical, &diff, store, view);
        self.diff = diff;
        self.install_views(stream, tree);
        warning
    }

    fn build_views(
        canonical: &DiffResult,
        display: &DiffResult,
        store: &mut CommentStore,
        view: &ReviewViewState,
    ) -> (Stream, FileTree, Option<String>) {
        let (placed, counts, warning) = match store.reanchor(canonical) {
            Ok(canonical_placed) => (
                placements_for_display(canonical, display, store.comments(), &canonical_placed),
                comment_counts(display, store),
                None,
            ),
            Err(e) => (
                Vec::new(),
                vec![0; display.files.len()],
                Some(format!(
                    "inline comments hidden; anchor updates could not be saved: {e:#}"
                )),
            ),
        };
        let mut stream = Stream::new(display, &placed, store.comments());
        let mut tree = FileTree::new(&display.files, &counts);
        tree.apply_collapsed(&view.collapsed);
        if let Some(query) = &view.query {
            stream.set_search(query, &display.files);
        }
        if let Some(anchor) = &view.anchor {
            stream.restore(anchor, &display.files);
        }
        (stream, tree, warning)
    }

    fn canonical_diff(&self) -> DiffResult {
        self.canonical_diff_for(&self.diff)
    }

    fn canonical_diff_for(&self, display: &DiffResult) -> DiffResult {
        let files = display
            .files
            .iter()
            .map(|file| {
                self.full_context
                    .get(&file.path)
                    .map_or_else(|| file.clone(), |state| state.base.clone())
            })
            .collect();
        DiffResult::from_files(files)
    }

    fn full_context_status(&self) -> Option<(bool, bool)> {
        let fi = self.stream.current_file()?;
        let file = self.diff.files.get(fi)?;
        if let Some(state) = self.full_context.get(&file.path) {
            Some((true, state.loading))
        } else if full_context_eligible(file) {
            Some((false, false))
        } else {
            None
        }
    }

    fn toggle_full_context(&mut self, store: &mut CommentStore) -> FullContextToggle {
        let Some(fi) = self.stream.current_file() else {
            return FullContextToggle::Unavailable;
        };
        let path = self.diff.files[fi].path.clone();
        if let Some(state) = self.full_context.remove(&path) {
            let view = self.capture_view();
            if let Some(display) = self.diff.files.iter_mut().find(|file| file.path == path) {
                *display = state.base;
            }
            let warning = self.rebuild_with_view(store, &view);
            return FullContextToggle::Collapsed(warning);
        }
        let file = &self.diff.files[fi];
        if !full_context_eligible(file) {
            return FullContextToggle::Unavailable;
        }
        self.full_context.insert(
            path.clone(),
            FullContextState {
                base: file.clone(),
                loading: true,
            },
        );
        FullContextToggle::Load {
            path,
            old_path: file.old_path.clone(),
        }
    }

    fn rebuild_with_view(
        &mut self,
        store: &mut CommentStore,
        view: &ReviewViewState,
    ) -> Option<String> {
        let canonical = self.canonical_diff();
        let (stream, tree, warning) = Self::build_views(&canonical, &self.diff, store, view);
        self.install_views(stream, tree);
        warning
    }

    fn install_full_context(
        &mut self,
        path: &str,
        mut expanded: FileDiff,
        store: &mut CommentStore,
    ) -> Result<FullContextInstall, String> {
        let Some(state) = self.full_context.get_mut(path) else {
            return Ok(FullContextInstall::Ignored); // collapsed while in flight
        };
        if !diff::same_changes(&state.base, &expanded) {
            self.full_context.remove(path);
            return Err(format!(
                "{path} changed since this review snapshot; press r to reload"
            ));
        }
        diff::mark_expanded_context(&state.base, &mut expanded);
        state.loading = false;
        let view = self.capture_view();
        let Some(file) = self.diff.files.iter_mut().find(|file| file.path == path) else {
            self.full_context.remove(path);
            return Ok(FullContextInstall::Ignored);
        };
        *file = expanded;
        Ok(FullContextInstall::Installed(
            self.rebuild_with_view(store, &view),
        ))
    }

    fn cancel_full_context_load(&mut self, path: &str) {
        if self
            .full_context
            .get(path)
            .is_some_and(|state| state.loading)
        {
            self.full_context.remove(path);
        }
    }

    fn install_views(&mut self, stream: Stream, tree: FileTree) {
        self.stream = stream;
        self.tree = tree;
        if self.focus == Focus::Tree {
            if let Some(fi) = self.stream.current_file() {
                self.tree.select_file(fi);
            }
        } else {
            sync_tree(self);
        }
    }
}

enum Overlay {
    Help {
        scroll: u16,
    },
    Comment(CommentDraft),
    ConfirmClear,
    Search(String),
    ExportPreview(ExportPreview),
    ExportPath {
        path: String,
        preview: ExportPreview,
    },
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
    overlay: Option<Overlay>,
    error: Option<String>,
    quit: bool,
    events: Receiver<AppEvent>,
    loader: Sender<LoadRequest>,
    /// Monotonic id pairing reload requests with responses.
    seq: u64,
    reloading: bool,
    reloaded_at: Option<Instant>,
    /// Persistent until comment placement succeeds on a later rebuild.
    comment_warning: Option<String>,
    /// Transient confirmation shown in the title bar (e.g. "copied 12 lines").
    notice: Option<String>,
    /// $EDITOR launch requested by `E`; handled in the run loop where the
    /// terminal handle is available for suspend/resume.
    pending_editor: Option<(std::path::PathBuf, usize)>,
    highlighter: Highlighter,
    store: CommentStore,
    /// What the last comment delete removed; `u` puts it back.
    undo_comments: Vec<Comment>,
    /// First key of a chord (`g`), held until the next key resolves it.
    pending_key: Option<KeyPress>,
}

struct CommentDraft {
    editor: TextEditor,
    /// Some(id) when editing an existing comment.
    editing: Option<u64>,
    /// None when editing (anchor is kept from the original).
    target: Option<CommentTarget>,
    label: String,
}

enum SingleLineInput {
    Continue,
    Submit,
    Cancel,
}

fn edit_single_line(input: &mut String, code: KeyCode) -> SingleLineInput {
    match code {
        KeyCode::Esc => SingleLineInput::Cancel,
        KeyCode::Enter => SingleLineInput::Submit,
        KeyCode::Backspace => {
            input.pop();
            SingleLineInput::Continue
        }
        KeyCode::Char(c) => {
            input.push(c);
            SingleLineInput::Continue
        }
        _ => SingleLineInput::Continue,
    }
}

/// Append a line to $GITTRE_LOG if set — the TUI owns the terminal, so
/// debugging goes to a file.
pub fn debug_log(msg: &str) {
    if let Ok(path) = std::env::var("GITTRE_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "[{t}] {msg}");
        }
    }
}

impl App {
    /// `initial`: a scope pre-resolved (and validated) from CLI flags. Its
    /// diff loads on the background thread; the TUI appears immediately.
    pub fn new(repo: Repository, initial: Option<Scope>, store: CommentStore) -> Self {
        let (event_tx, events) = channel();
        let loader_path = repo.workdir().unwrap_or_else(|| repo.path()).to_path_buf();
        let loader = spawn_loader(loader_path, event_tx);
        let screen = Screen::Picker(build_picker_skeleton(&repo));
        let mut app = App {
            repo,
            screen,
            overlay: None,
            error: None,
            quit: false,
            events,
            loader,
            seq: 0,
            reloading: false,
            reloaded_at: None,
            comment_warning: None,
            notice: None,
            pending_editor: None,
            highlighter: Highlighter::new(),
            store,
            undo_comments: Vec::new(),
            pending_key: None,
        };
        match initial {
            Some(scope) => app.open_scope(scope),
            None => app.request_counts(),
        }
        app
    }

    fn request_counts(&mut self) {
        self.seq += 1;
        let _ = self.loader.send(LoadRequest::Counts { seq: self.seq });
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
                    Event::Paste(text) => self.on_paste(&text),
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

    /// Suspend the TUI, run $EDITOR at file:line, resume, and refresh the
    /// review after a successful editor exit.
    fn run_editor(&mut self, terminal: &mut DefaultTerminal, path: &std::path::Path, line: usize) {
        use ratatui::crossterm::event::{
            DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        };
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

        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        let _ = disable_raw_mode();
        let status = std::process::Command::new(program)
            .args(&args)
            .arg(format!("+{line}"))
            .arg(path)
            .status();
        let _ = enable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        );
        let _ = terminal.clear();

        match status {
            Ok(st) if st.success() => self.request_reload(),
            Ok(st) => self.error = Some(format!("{editor} exited with {st}")),
            Err(e) => self.error = Some(format!("failed to launch {editor}: {e}")),
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let (col, row) = (mouse.column, mouse.row);
        if let Some(overlay) = &mut self.overlay {
            let mut close = false;
            match overlay {
                Overlay::Help { scroll } => match mouse.kind {
                    MouseEventKind::ScrollDown => *scroll = scroll.saturating_add(3),
                    MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(3),
                    MouseEventKind::Down(MouseButton::Left) => close = true,
                    _ => {}
                },
                Overlay::Comment(draft) => match mouse.kind {
                    MouseEventKind::ScrollDown => draft.editor.scroll_by(3),
                    MouseEventKind::ScrollUp => draft.editor.scroll_by(-3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        draft.editor.set_cursor_from_screen(col, row)
                    }
                    _ => {}
                },
                Overlay::ExportPreview(preview) | Overlay::ExportPath { preview, .. } => {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => preview.scroll_by(3),
                        MouseEventKind::ScrollUp => preview.scroll_by(-3),
                        _ => {}
                    }
                }
                Overlay::ConfirmClear | Overlay::Search(_) => {}
            }
            if close {
                self.overlay = None;
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.mouse_scroll(1, col, row),
            MouseEventKind::ScrollUp => self.mouse_scroll(-1, col, row),
            MouseEventKind::Down(MouseButton::Left) => self.mouse_click(col, row),
            _ => {}
        }
    }

    fn on_paste(&mut self, text: &str) {
        match &mut self.overlay {
            Some(Overlay::Comment(draft)) => draft.editor.insert_str(text),
            Some(Overlay::ExportPath { path, .. }) | Some(Overlay::Search(path)) => {
                path.push_str(&text.replace(['\r', '\n'], ""));
            }
            _ => {}
        }
    }

    fn mouse_scroll(&mut self, direction: isize, col: u16, row: u16) {
        match &mut self.screen {
            Screen::Review(review) => {
                if let Some(view) = &mut review.file_view {
                    view.scroll_by(direction * 3);
                    return;
                }
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
        match &mut self.screen {
            Screen::Review(review) => {
                if review.file_view.is_some() {
                    return;
                }
                if review.show_tree
                    && let Some(idx) = review.tree.hit(col, row)
                {
                    review.tree.selected = idx;
                    if let Some(file_idx) = review.tree.activate() {
                        review.stream.jump_to_file(file_idx);
                        review.focus = Focus::Stream;
                    }
                } else if let Some(idx) = review.stream.hit(col, row) {
                    review.stream.set_cursor(idx);
                    review.focus = Focus::Stream;
                    sync_tree(review);
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
        match &event {
            AppEvent::Diff { seq, took, .. } => debug_log(&format!(
                "event: Diff seq={seq} (cur={}) took={took:?}",
                self.seq
            )),
            AppEvent::Counts { seq, .. } => {
                debug_log(&format!("event: Counts seq={seq} (cur={})", self.seq))
            }
            AppEvent::File {
                seq, scope, path, ..
            } => debug_log(&format!("event: File seq={seq} {path} ({})", scope.label())),
        }
        match event {
            AppEvent::Diff {
                seq,
                scope,
                diff,
                took: _,
            } => {
                if seq != self.seq {
                    return; // superseded by a newer request or scope switch
                }
                self.reloading = false;
                let diff = match diff {
                    Ok(diff) => diff,
                    Err(e) => {
                        self.error = Some(format!("{e:#}"));
                        return;
                    }
                };
                match &mut self.screen {
                    // Same scope already open: this is a reload.
                    Screen::Review(review) if review.scope == scope => {
                        let (warning, full_context_paths) =
                            review.replace_diff(diff, &mut self.store);
                        self.comment_warning = warning;
                        for (path, old_path) in full_context_paths {
                            let _ = self.loader.send(LoadRequest::File {
                                seq: self.seq,
                                scope: scope.clone(),
                                path,
                                old_path,
                                kind: FileLoadKind::FullContext,
                            });
                        }
                        self.reloaded_at = Some(Instant::now());
                    }
                    // Otherwise it's a scope being opened.
                    _ => {
                        let (review, warning) = ReviewState::new(scope, diff, &mut self.store);
                        debug_log(&format!(
                            "review created: {} files",
                            review.diff.files.len()
                        ));
                        self.screen = Screen::Review(Box::new(review));
                        self.comment_warning = warning;
                    }
                }
            }
            AppEvent::File {
                seq,
                scope,
                path,
                kind,
                files,
            } => {
                if seq != self.seq || !matches!(&self.screen, Screen::Review(r) if r.scope == scope)
                {
                    return;
                }
                let loaded = match files {
                    Ok(f) => f,
                    Err(e) => {
                        if kind == FileLoadKind::FullContext
                            && let Screen::Review(review) = &mut self.screen
                        {
                            review.cancel_full_context_load(&path);
                        }
                        self.error = Some(format!("{e:#}"));
                        return;
                    }
                };
                if let Screen::Review(review) = &mut self.screen {
                    match kind {
                        FileLoadKind::Stub { .. } => {
                            if let Some(pos) = review.diff.files.iter().position(|f| f.path == path)
                            {
                                self.comment_warning =
                                    review.expand_file(pos, loaded, &mut self.store);
                            }
                        }
                        FileLoadKind::FullContext => {
                            let Some(expanded) = loaded.into_iter().next() else {
                                review.cancel_full_context_load(&path);
                                self.error = Some(format!("no diff returned for {path}"));
                                return;
                            };
                            match review.install_full_context(&path, expanded, &mut self.store) {
                                Ok(FullContextInstall::Installed(warning)) => {
                                    self.comment_warning = warning;
                                    self.notice = Some(format!("showing full diff for {path}"));
                                }
                                Ok(FullContextInstall::Ignored) => {}
                                Err(message) => self.error = Some(message),
                            }
                        }
                    }
                }
            }
            AppEvent::Counts { seq, counts } => {
                if seq != self.seq {
                    return;
                }
                if let Screen::Picker(picker) = &mut self.screen {
                    let ScopeCounts {
                        uncommitted,
                        staged,
                        branch,
                    } = counts;
                    picker.set_counts(uncommitted, staged, branch.map(|(_, n)| n));
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
                self.store.comments(),
            ),
        }

        match &mut self.overlay {
            Some(Overlay::ExportPreview(preview)) | Some(Overlay::ExportPath { preview, .. }) => {
                preview.render(frame, main_area, self.store.comments().len());
            }
            _ => {}
        }

        match &self.overlay {
            Some(Overlay::Search(query)) => {
                bar::render_input(frame, bar_area, "/", query, "search")
            }
            Some(Overlay::ExportPath { path, .. }) => {
                bar::render_input(frame, bar_area, "export to ", path, "write")
            }
            _ => bar::render(frame, bar_area, &self.hints()),
        }

        let (comment_bounds, comment_anchor) = match &self.screen {
            Screen::Review(review) => (
                review.stream.viewport_rect(),
                review.stream.cursor_screen_position(),
            ),
            _ => (main_area, None),
        };
        if let Some(Overlay::Comment(draft)) = &mut self.overlay {
            popups::render_comment_editor(
                frame,
                comment_bounds,
                comment_anchor,
                &draft.label,
                &mut draft.editor,
            );
        }
        if matches!(self.overlay, Some(Overlay::ConfirmClear)) {
            let n = self.store.comments().len();
            let msg = if n == 1 {
                "delete the 1 comment? [y/n]".to_string()
            } else {
                format!("delete all {n} comments? [y/n]")
            };
            popups::render_confirm(frame, frame.area(), &msg);
        }
        if let Some(Overlay::Help { scroll }) = &mut self.overlay {
            popups::render_help(frame, frame.area(), scroll);
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
                Screen::Picker(_) if self.reloading => {
                    spans.push(" loading diff…".dark_gray().italic())
                }
                Screen::Picker(_) => spans.push(" select what to review".dark_gray()),
                Screen::Log(_) => spans.push(" pick a commit to review".dark_gray()),
                Screen::Base(_) => spans.push(" pick a base branch".dark_gray()),
                Screen::Review(r) => {
                    spans.push(format!(" {}", r.scope.label()).bold());
                    spans.push(format!("  {} ", count_label(r.diff.files.len())).into());
                    spans.push(format!("+{} ", r.diff.additions).green());
                    spans.push(format!("−{}", r.diff.deletions).red());
                    if !self.store.comments().is_empty() {
                        spans.push(format!("  ✎ {}", self.store.comments().len()).cyan());
                    }
                    if let Some(warning) = &self.comment_warning {
                        spans.push(format!("  ⚠ {warning}").yellow().bold());
                    } else if self.reloading {
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

    /// The table whose bindings apply to the next key press.
    fn active_table(&self) -> &'static [keymap::Binding] {
        match &self.overlay {
            Some(Overlay::Help { .. }) => return keymap::HELP_VIEW,
            Some(Overlay::ExportPreview(_)) => return keymap::EXPORT_PREVIEW,
            _ => {}
        }
        match &self.screen {
            Screen::Picker(_) => keymap::PICKER,
            Screen::Log(_) => keymap::LOG,
            Screen::Base(_) => keymap::BASE,
            Screen::Review(r) if r.file_view.is_some() => keymap::FILE_VIEW,
            Screen::Review(_) => keymap::REVIEW,
        }
    }

    /// Key labels come from the binding tables; only descriptions live here.
    fn hints(&self) -> Vec<(String, &'static str)> {
        use Action::*;
        fn h(
            table: &[keymap::Binding],
            actions: &[Action],
            desc: &'static str,
        ) -> (String, &'static str) {
            (keymap::keys_label(table, actions), desc)
        }
        fn raw(key: &str, desc: &'static str) -> (String, &'static str) {
            (key.to_string(), desc)
        }

        // Mid-chord: show what the held prefix can still complete.
        if let Some(prefix) = self.pending_key {
            let mut hints = keymap::chords_from(self.active_table(), prefix);
            hints.push(raw("other", "cancel"));
            return hints;
        }
        match &self.overlay {
            Some(Overlay::Help { .. }) => {
                return vec![
                    h(keymap::HELP_VIEW, &[Down, Up], "scroll"),
                    raw("q/Esc/?", "close help"),
                ];
            }
            Some(Overlay::Comment(_)) => {
                return vec![
                    raw("←↑↓→", "move"),
                    raw("⏎", "save"),
                    raw("Alt+⏎", "newline"),
                    raw("Esc", "cancel"),
                ];
            }
            Some(Overlay::ConfirmClear) => {
                return vec![raw("y", "delete all comments"), raw("any key", "cancel")];
            }
            Some(Overlay::Search(_)) => {
                return vec![raw("⏎", "search"), raw("Esc", "cancel")];
            }
            Some(Overlay::ExportPath { .. }) => {
                return vec![raw("⏎", "write file"), raw("Esc", "cancel")];
            }
            Some(Overlay::ExportPreview(_)) => {
                let t = keymap::EXPORT_PREVIEW;
                return vec![
                    h(t, &[Down, Up], "scroll"),
                    h(t, &[GotoTop, GotoBottom], "top/bottom"),
                    h(t, &[CopyMarkdown], "copy markdown"),
                    h(t, &[WriteFile], "write file"),
                    raw("Esc", "close"),
                ];
            }
            None => {}
        }
        match &self.screen {
            Screen::Picker(picker) => {
                let t = keymap::PICKER;
                vec![
                    raw("1-9", "open"),
                    h(t, &[Down, Up], "select"),
                    h(t, &[Activate], "open"),
                    raw("?", "help"),
                    h(t, &[Quit], "quit"),
                ]
                .into_iter()
                .take(if picker.items.is_empty() { 2 } else { 5 })
                .collect()
            }
            Screen::Log(log) => {
                let t = keymap::LOG;
                if log.marked.is_some() {
                    vec![
                        h(t, &[Down, Up], "select"),
                        h(t, &[Activate], "review marked ⇢ selected"),
                        h(t, &[MarkRange], "unmark"),
                        h(t, &[Back], "back"),
                    ]
                } else {
                    vec![
                        h(t, &[Down, Up], "select"),
                        h(t, &[Activate], "review commit"),
                        h(t, &[MarkRange], "mark range start"),
                        h(t, &[Back], "back"),
                        raw("?", "help"),
                    ]
                }
            }
            Screen::Base(_) => {
                let t = keymap::BASE;
                vec![
                    h(t, &[Down, Up], "select"),
                    h(t, &[Activate], "compare against"),
                    h(t, &[Back], "back"),
                    raw("?", "help"),
                ]
            }
            Screen::Review(review) => {
                let t = keymap::REVIEW;
                if review.diff.files.is_empty() {
                    return vec![
                        h(t, &[Reload], "reload"),
                        h(t, &[SwitchScope], "switch scope"),
                        raw("?", "help"),
                    ];
                }
                if review.file_view.is_some() {
                    let ft = keymap::FILE_VIEW;
                    return vec![
                        h(ft, &[Down, Up], "scroll"),
                        h(ft, &[GotoTop, GotoBottom], "top/bottom"),
                        h(ft, &[OpenEditor], "open in $EDITOR"),
                        h(ft, &[Back], "back to diff"),
                    ];
                }
                if review.stream.large_stub_at_cursor().is_some() {
                    return vec![
                        h(t, &[Activate], "expand"),
                        h(t, &[Down, Up], "scroll"),
                        h(t, &[NextFile, PrevFile], "file"),
                        h(t, &[SwitchScope], "scope"),
                        raw("?", "help"),
                    ];
                }
                if review.stream.has_selection() {
                    return vec![
                        h(t, &[Down, Up], "extend"),
                        h(t, &[Comment], "comment"),
                        h(t, &[CopyCode], "copy code"),
                        h(t, &[CopyPatch], "copy patch"),
                        raw("Esc/v", "cancel"),
                    ];
                }
                if review.focus == Focus::Tree {
                    return vec![
                        h(t, &[Down, Up], "select"),
                        h(t, &[Activate], "open"),
                        raw("Esc/Tab", "back to diff"),
                        h(t, &[NarrowTree, WidenTree, AutoTreeWidth], "tree width"),
                        h(t, &[SwitchScope], "scope"),
                        raw("?", "help"),
                    ];
                }
                let mut hints = vec![
                    h(t, &[Down, Up], "scroll"),
                    h(t, &[NextFile, PrevFile], "file"),
                ];
                if let Some((expanded, loading)) = review.full_context_status() {
                    hints.push(h(
                        t,
                        &[Activate],
                        if loading {
                            "cancel full diff"
                        } else if expanded {
                            "collapse full diff"
                        } else {
                            "show full diff"
                        },
                    ));
                }
                if review.stream.has_search() {
                    hints.push(h(t, &[NextMatch, PrevMatch], "match"));
                    hints.push(raw("Esc", "clear search"));
                } else {
                    hints.push(h(t, &[NextHunk, PrevHunk], "hunk"));
                }
                hints.push(h(t, &[Search], "search"));
                hints.push(h(t, &[Reload], "reload"));
                hints.push(h(t, &[Comment], "comment"));
                if review.stream.has_comments() {
                    hints.push(h(t, &[NextComment, PrevComment], "comment nav"));
                }
                hints.push(h(t, &[FocusTree], "pick file"));
                hints.push(h(
                    t,
                    &[ToggleTree],
                    if review.show_tree {
                        "hide tree"
                    } else {
                        "show tree"
                    },
                ));
                hints.push(h(t, &[NarrowTree, WidenTree, AutoTreeWidth], "tree width"));
                hints.push(h(t, &[SwitchScope], "scope"));
                hints.push(raw("?", "help"));
                hints
            }
        }
    }

    // ---- input ------------------------------------------------------------

    fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.error = None;
        self.notice = None;

        // Raw mode swallows the usual SIGINT, so honor Ctrl-C ourselves.
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        let key = KeyPress::new(code, modifiers);
        let pending = self.pending_key.take();

        if self.overlay.is_some() {
            self.on_overlay_key(key, pending);
            return;
        }
        if code == KeyCode::Char('?') {
            self.overlay = Some(Overlay::Help { scroll: 0 });
            return;
        }

        match &mut self.screen {
            Screen::Picker(_) => self.on_picker_key(key, pending),
            Screen::Log(_) => self.on_log_key(key, pending),
            Screen::Base(_) => self.on_base_key(key, pending),
            Screen::Review(_) => self.on_review_key(key, pending),
        }
    }

    fn on_overlay_key(&mut self, key: KeyPress, pending: Option<KeyPress>) {
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        self.overlay = match overlay {
            Overlay::Help { mut scroll } => {
                let mut close = false;
                match keymap::lookup(keymap::HELP_VIEW, keymap::Ctx::default(), pending, key) {
                    Lookup::Act(action) => match action {
                        Action::Down => scroll = scroll.saturating_add(1),
                        Action::Up => scroll = scroll.saturating_sub(1),
                        Action::PageDown | Action::HalfPageDown => {
                            scroll = scroll.saturating_add(10)
                        }
                        Action::PageUp | Action::HalfPageUp => scroll = scroll.saturating_sub(10),
                        Action::Back => close = true,
                        _ => {}
                    },
                    Lookup::Pending => self.pending_key = Some(key),
                    Lookup::None => {}
                }
                (!close).then_some(Overlay::Help { scroll })
            }
            Overlay::Comment(mut draft) => {
                let mut close = false;
                let mut saved = false;
                match key.code {
                    KeyCode::Esc => close = true,
                    KeyCode::Backspace => draft.editor.backspace(),
                    KeyCode::Delete => draft.editor.delete(),
                    KeyCode::Left => draft.editor.move_left(),
                    KeyCode::Right => draft.editor.move_right(),
                    KeyCode::Up => draft.editor.move_vertical(-1),
                    KeyCode::Down => draft.editor.move_vertical(1),
                    KeyCode::Home => draft.editor.home(),
                    KeyCode::End => draft.editor.end(),
                    KeyCode::PageUp => draft.editor.move_vertical(-10),
                    KeyCode::PageDown => draft.editor.move_vertical(10),
                    KeyCode::Enter if key.mods.contains(KeyModifiers::ALT) => {
                        draft.editor.insert_char('\n')
                    }
                    KeyCode::Enter => {
                        if draft.editor.as_str().trim().is_empty() {
                            self.notice = Some("empty comment discarded".into());
                            close = true;
                        } else {
                            let body = draft.editor.as_str().to_string();
                            let scope_label = match &self.screen {
                                Screen::Review(r) => r.scope.label(),
                                _ => String::new(),
                            };
                            let result = match (draft.editing, draft.target.clone()) {
                                (Some(id), _) => self.store.edit(id, body),
                                (None, Some(t)) => self.store.add(
                                    t.path,
                                    t.new_side,
                                    t.lines,
                                    t.snippet,
                                    body,
                                    scope_label,
                                ),
                                (None, None) => Ok(()),
                            };
                            match result {
                                Ok(()) => {
                                    close = true;
                                    saved = true;
                                }
                                Err(e) => self.error = Some(format!("{e:#}")),
                            }
                        }
                    }
                    KeyCode::Char(c) => draft.editor.insert_char(c),
                    _ => {}
                }
                if saved {
                    self.rebuild_review();
                }
                (!close).then_some(Overlay::Comment(draft))
            }
            Overlay::ConfirmClear => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    let deleted = self.store.comments().to_vec();
                    if let Err(e) = self.store.delete_all() {
                        self.error = Some(format!("{e:#}"));
                    } else {
                        self.undo_comments = deleted;
                        self.notice = Some("all comments deleted (u restores)".into());
                        self.rebuild_review();
                    }
                }
                None
            }
            Overlay::Search(mut input) => match edit_single_line(&mut input, key.code) {
                SingleLineInput::Cancel => None,
                SingleLineInput::Submit => {
                    if !input.is_empty()
                        && let Screen::Review(review) = &mut self.screen
                    {
                        let count = review.stream.set_search(&input, &review.diff.files);
                        if count == 0 {
                            self.error = Some(format!("no matches for \u{2018}{input}\u{2019}"));
                        } else {
                            review.focus = Focus::Stream;
                            sync_tree(review);
                        }
                    }
                    None
                }
                SingleLineInput::Continue => Some(Overlay::Search(input)),
            },
            Overlay::ExportPath { mut path, preview } => {
                match edit_single_line(&mut path, key.code) {
                    SingleLineInput::Cancel => Some(Overlay::ExportPreview(preview)),
                    SingleLineInput::Submit if path.trim().is_empty() => {
                        Some(Overlay::ExportPath { path, preview })
                    }
                    SingleLineInput::Submit => match std::fs::write(&path, preview.markdown()) {
                        Ok(()) => {
                            self.notice = Some(format!(
                                "exported {} comment{} to {path}",
                                self.store.comments().len(),
                                if self.store.comments().len() == 1 {
                                    ""
                                } else {
                                    "s"
                                },
                            ));
                            None
                        }
                        Err(e) => {
                            self.error = Some(format!("export failed: {e}"));
                            Some(Overlay::ExportPath { path, preview })
                        }
                    },
                    SingleLineInput::Continue => Some(Overlay::ExportPath { path, preview }),
                }
            }
            Overlay::ExportPreview(mut preview) => {
                let mut close = false;
                let mut write = false;
                match keymap::lookup(keymap::EXPORT_PREVIEW, keymap::Ctx::default(), pending, key) {
                    Lookup::Act(action) => match action {
                        Action::Back => close = true,
                        Action::Down => preview.scroll_by(1),
                        Action::Up => preview.scroll_by(-1),
                        Action::PageDown => preview.page(1),
                        Action::PageUp => preview.page(-1),
                        Action::HalfPageDown => preview.half_page(1),
                        Action::HalfPageUp => preview.half_page(-1),
                        Action::GotoTop => preview.top(),
                        Action::GotoBottom => preview.bottom(),
                        Action::WriteFile => write = true,
                        Action::CopyMarkdown => {
                            let markdown = preview.markdown().to_string();
                            match clipboard::copy(&markdown) {
                                Ok(()) => self.notice = Some("copied export markdown".into()),
                                Err(e) => self.error = Some(format!("clipboard: {e}")),
                            }
                        }
                        _ => {}
                    },
                    Lookup::Pending => self.pending_key = Some(key),
                    Lookup::None => {}
                }
                if close {
                    None
                } else if write {
                    Some(Overlay::ExportPath {
                        path: format!("review-{}.md", today_string()),
                        preview,
                    })
                } else {
                    Some(Overlay::ExportPreview(preview))
                }
            }
        };
    }

    fn rebuild_review(&mut self) {
        let Screen::Review(review) = &mut self.screen else {
            return;
        };
        self.comment_warning = review.rebuild(&mut self.store);
    }

    fn on_picker_key(&mut self, key: KeyPress, pending: Option<KeyPress>) {
        let Screen::Picker(picker) = &mut self.screen else {
            return;
        };
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            if key.mods.is_empty() {
                let idx = c as usize - '1' as usize;
                if idx < picker.items.len() {
                    picker.selected = idx;
                    self.activate_picker_item();
                }
                return;
            }
        }
        match keymap::lookup(keymap::PICKER, keymap::Ctx::default(), pending, key) {
            Lookup::Act(action) => match action {
                Action::Quit => self.quit = true,
                Action::Down => picker.move_selection(1),
                Action::Up => picker.move_selection(-1),
                Action::Activate => self.activate_picker_item(),
                _ => {}
            },
            Lookup::Pending => self.pending_key = Some(key),
            Lookup::None => {}
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

    fn on_base_key(&mut self, key: KeyPress, pending: Option<KeyPress>) {
        let Screen::Base(base) = &mut self.screen else {
            return;
        };
        match keymap::lookup(keymap::BASE, keymap::Ctx::default(), pending, key) {
            Lookup::Act(action) => match action {
                Action::Back => self.open_picker(),
                Action::Down => base.move_selection(1),
                Action::Up => base.move_selection(-1),
                Action::PageDown => base.move_selection(20),
                Action::PageUp => base.move_selection(-20),
                Action::HalfPageDown => base.move_selection(10),
                Action::HalfPageUp => base.move_selection(-10),
                Action::GotoTop => base.selected = 0,
                Action::GotoBottom => base.selected = base.names.len().saturating_sub(1),
                Action::Activate => self.open_selected_base(),
                _ => {}
            },
            Lookup::Pending => self.pending_key = Some(key),
            Lookup::None => {}
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

    fn on_log_key(&mut self, key: KeyPress, pending: Option<KeyPress>) {
        let Screen::Log(log) = &mut self.screen else {
            return;
        };
        match keymap::lookup(keymap::LOG, keymap::Ctx::default(), pending, key) {
            Lookup::Act(action) => match action {
                Action::Back => self.open_picker(),
                Action::Down => log.move_selection(1),
                Action::Up => log.move_selection(-1),
                Action::PageDown => log.move_selection(20),
                Action::PageUp => log.move_selection(-20),
                Action::HalfPageDown => log.move_selection(10),
                Action::HalfPageUp => log.move_selection(-10),
                Action::GotoTop => log.selected = 0,
                Action::GotoBottom => log.selected = log.entries.len().saturating_sub(1),
                Action::MarkRange => log.toggle_mark(),
                Action::Activate => self.open_selected_commit(),
                _ => {}
            },
            Lookup::Pending => self.pending_key = Some(key),
            Lookup::None => {}
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

    fn on_review_key(&mut self, key: KeyPress, pending: Option<KeyPress>) {
        let Screen::Review(review) = &mut self.screen else {
            return;
        };
        if let Some(view) = &mut review.file_view {
            let action =
                match keymap::lookup(keymap::FILE_VIEW, keymap::Ctx::default(), pending, key) {
                    Lookup::Act(action) => action,
                    Lookup::Pending => {
                        self.pending_key = Some(key);
                        return;
                    }
                    Lookup::None => return,
                };
            match action {
                Action::Back => review.file_view = None,
                Action::Down => view.scroll_by(1),
                Action::Up => view.scroll_by(-1),
                Action::PageDown => view.page(1),
                Action::PageUp => view.page(-1),
                Action::HalfPageDown => view.half_page(1),
                Action::HalfPageUp => view.half_page(-1),
                Action::GotoTop => view.scroll_to_top(),
                Action::GotoBottom => view.scroll_to_bottom(),
                Action::OpenEditor => {
                    let line = view.top_line();
                    let path = view.path.clone();
                    self.request_editor(&path, line);
                }
                _ => {}
            }
            return;
        }
        let ctx = keymap::Ctx {
            search_active: review.stream.has_search(),
        };
        let action = match keymap::lookup(keymap::REVIEW, ctx, pending, key) {
            Lookup::Act(action) => action,
            Lookup::Pending => {
                self.pending_key = Some(key);
                return;
            }
            Lookup::None => return,
        };
        match action {
            // Esc peels back one layer at a time:
            // selection → live search → tree pick → picker.
            Action::Back => {
                if review.stream.has_selection() {
                    review.stream.cancel_selection();
                } else if review.stream.has_search() {
                    review.stream.clear_search();
                } else if review.focus == Focus::Tree {
                    review.focus = Focus::Stream;
                    sync_tree(review);
                } else {
                    self.open_picker();
                }
            }
            Action::SwitchScope => self.open_picker(),
            Action::Search => self.overlay = Some(Overlay::Search(String::new())),
            Action::ToggleSelection => {
                if review.stream.has_selection() {
                    review.stream.cancel_selection();
                } else if review.focus == Focus::Stream {
                    review.stream.start_selection();
                }
            }
            // helix's x: select the current line, extend line-wise on repeat.
            Action::SelectLine => {
                if review.stream.has_selection() {
                    review.stream.move_cursor(1);
                    sync_tree(review);
                } else if review.focus == Focus::Stream {
                    review.stream.start_selection();
                }
            }
            Action::CopyCode if review.stream.has_selection() => self.copy_selection(false),
            Action::CopyPatch if review.stream.has_selection() => self.copy_selection(true),
            Action::CopyCode | Action::CopyPatch => {}
            Action::FileView => self.open_file_view(),
            Action::Reload => self.request_reload(),
            Action::ExportPreview => {
                if self.store.comments().is_empty() {
                    self.error = Some("no comments to export".into());
                } else {
                    let markdown = export_markdown(
                        self.store.comments(),
                        &repo_title(&self.repo),
                        &today_string(),
                    );
                    self.overlay = Some(Overlay::ExportPreview(ExportPreview::new(markdown)));
                }
            }
            Action::Comment => {
                let selection_target = match review.stream.selection_target(&review.diff.files) {
                    Ok(target) => target,
                    Err(message) => {
                        self.error = Some(message.into());
                        return;
                    }
                };
                if let Some(t) = selection_target {
                    review.stream.cancel_selection();
                    self.overlay = Some(Overlay::Comment(CommentDraft {
                        editor: TextEditor::new(String::new()),
                        editing: None,
                        label: target_label(&t),
                        target: Some(t),
                    }));
                } else if let Some(ci) = review.stream.comment_at_cursor() {
                    let c = &self.store.comments()[ci];
                    self.overlay = Some(Overlay::Comment(CommentDraft {
                        editor: TextEditor::new(c.body.clone()),
                        editing: Some(c.id),
                        target: None,
                        label: format!("edit #{} on {}", c.id, c.path),
                    }));
                } else if review.stream.expanded_context_at_cursor(&review.diff.files) {
                    self.error = Some("comments require a line in the original diff".into());
                } else if let Some(t) = review.stream.line_target(&review.diff.files) {
                    self.overlay = Some(Overlay::Comment(CommentDraft {
                        editor: TextEditor::new(String::new()),
                        editing: None,
                        label: target_label(&t),
                        target: Some(t),
                    }));
                }
            }
            Action::DeleteComment => {
                if let Some(ci) = review.stream.comment_at_cursor() {
                    let comment = self.store.comments()[ci].clone();
                    let id = comment.id;
                    if let Err(e) = self.store.delete(id) {
                        self.error = Some(format!("{e:#}"));
                    } else {
                        self.undo_comments = vec![comment];
                        self.notice = Some(format!("deleted comment #{id} (u restores)"));
                        self.rebuild_review();
                    }
                }
            }
            Action::RestoreComments => {
                if !self.undo_comments.is_empty() {
                    let comments = self.undo_comments.clone();
                    let n = comments.len();
                    if let Err(e) = self.store.restore(comments) {
                        self.error = Some(format!("{e:#}"));
                    } else {
                        self.undo_comments.clear();
                        self.notice = Some(format!(
                            "restored {n} comment{}",
                            if n == 1 { "" } else { "s" }
                        ));
                        self.rebuild_review();
                    }
                }
            }
            Action::DeleteAllComments => {
                if !self.store.comments().is_empty() {
                    self.overlay = Some(Overlay::ConfirmClear);
                }
            }
            Action::NextComment => {
                review.stream.next_comment();
                sync_tree(review);
            }
            Action::PrevComment => {
                review.stream.prev_comment();
                sync_tree(review);
            }
            Action::OpenEditor => {
                if let Some((fi, line)) = review.stream.current_position(&review.diff.files) {
                    let path = review.diff.files[fi].path.clone();
                    let line = line.unwrap_or(1) as usize;
                    self.request_editor(&path, line);
                }
            }
            Action::NextMatch => {
                review.stream.next_match();
                sync_tree(review);
            }
            Action::PrevMatch => {
                review.stream.prev_match();
                sync_tree(review);
            }
            Action::ToggleTree => {
                review.show_tree = !review.show_tree;
                if !review.show_tree {
                    review.focus = Focus::Stream;
                }
            }
            Action::NarrowTree => {
                if let Some(message) = resize_notice(resize_tree(review, false)) {
                    self.notice = Some(message.into());
                }
            }
            Action::WidenTree => {
                if let Some(message) = resize_notice(resize_tree(review, true)) {
                    self.notice = Some(message.into());
                }
            }
            Action::AutoTreeWidth => {
                review.tree_width = None;
                review.show_tree = true;
            }
            Action::FocusTree => {
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
            Action::Down => match review.focus {
                Focus::Tree => review.tree.move_selection(1),
                Focus::Stream => {
                    review.stream.move_cursor(1);
                    sync_tree(review);
                }
            },
            Action::Up => match review.focus {
                Focus::Tree => review.tree.move_selection(-1),
                Focus::Stream => {
                    review.stream.move_cursor(-1);
                    sync_tree(review);
                }
            },
            Action::PageDown => {
                review.stream.page(1);
                sync_tree(review);
            }
            Action::PageUp => {
                review.stream.page(-1);
                sync_tree(review);
            }
            Action::HalfPageDown => {
                review.stream.half_page(1);
                sync_tree(review);
            }
            Action::HalfPageUp => {
                review.stream.half_page(-1);
                sync_tree(review);
            }
            Action::GotoTop => {
                review.stream.scroll_to_top();
                sync_tree(review);
            }
            Action::GotoBottom => {
                review.stream.scroll_to_bottom();
                sync_tree(review);
            }
            Action::NextFile => {
                review.stream.next_file();
                sync_tree(review);
            }
            Action::PrevFile => {
                review.stream.prev_file();
                sync_tree(review);
            }
            Action::NextHunk => {
                review.stream.next_hunk();
                sync_tree(review);
            }
            Action::PrevHunk => {
                review.stream.prev_hunk();
                sync_tree(review);
            }
            Action::Activate => {
                if review.focus == Focus::Tree {
                    if let Some(file_idx) = review.tree.activate() {
                        review.stream.jump_to_file(file_idx);
                        review.focus = Focus::Stream;
                    }
                } else if let Some(fi) = review.stream.large_stub_at_cursor() {
                    let path = review.diff.files[fi].path.clone();
                    let untracked_dir = review.diff.files[fi].untracked_dir;
                    self.notice = Some(if untracked_dir {
                        format!("listing {path}/…")
                    } else {
                        format!("loading diff for {path}…")
                    });
                    let _ = self.loader.send(LoadRequest::File {
                        seq: self.seq,
                        scope: review.scope.clone(),
                        path,
                        old_path: None,
                        kind: FileLoadKind::Stub { untracked_dir },
                    });
                } else {
                    match review.toggle_full_context(&mut self.store) {
                        FullContextToggle::Load { path, old_path } => {
                            self.notice = Some(format!("loading full diff for {path}…"));
                            let _ = self.loader.send(LoadRequest::File {
                                seq: self.seq,
                                scope: review.scope.clone(),
                                path,
                                old_path,
                                kind: FileLoadKind::FullContext,
                            });
                        }
                        FullContextToggle::Collapsed(warning) => {
                            self.comment_warning = warning;
                            self.notice = Some("collapsed full diff context".into());
                        }
                        FullContextToggle::Unavailable => {}
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
        match clipboard::copy(&text) {
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

    /// Ask the loader for a scope's diff; the screen switches when it lands.
    fn open_scope(&mut self, scope: Scope) {
        self.seq += 1; // invalidate any in-flight response
        self.reloading = true;
        let _ = self.loader.send(LoadRequest::Diff {
            seq: self.seq,
            scope,
        });
    }

    /// Reload the current review through the background worker. This is used
    /// by the explicit `r` action and after returning from `$EDITOR`.
    fn request_reload(&mut self) {
        let Screen::Review(review) = &self.screen else {
            return;
        };
        let scope = review.scope.clone();
        self.seq += 1;
        self.reloading = true;
        let _ = self.loader.send(LoadRequest::Diff {
            seq: self.seq,
            scope,
        });
    }

    fn open_picker(&mut self) {
        self.screen = Screen::Picker(build_picker_skeleton(&self.repo));
        self.reloading = false;
        // request_counts bumps seq, which also drops any in-flight diff
        // for the scope we just left.
        self.request_counts();
    }
}

fn repo_title(repo: &Repository) -> String {
    repo.workdir()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repository".into())
}

pub fn today_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn comment_counts(diff: &DiffResult, store: &CommentStore) -> Vec<usize> {
    diff.files
        .iter()
        .map(|f| store.count_for_path(&f.path))
        .collect()
}

fn full_context_eligible(file: &FileDiff) -> bool {
    matches!(file.status, FileStatus::Modified | FileStatus::Renamed)
        && !file.binary
        && !file.large
        && !file.untracked_dir
        && !file.hunks.is_empty()
}

fn target_label(t: &CommentTarget) -> String {
    if t.lines.0 == t.lines.1 {
        format!("{}:{}", t.path, t.lines.0)
    } else {
        format!("{}:{}-{}", t.path, t.lines.0, t.lines.1)
    }
}

/// Picker with instant structure; the counts arrive asynchronously.
fn build_picker_skeleton(repo: &Repository) -> ScopePicker {
    let mut items = vec![
        ScopeItem {
            title: "Uncommitted changes".into(),
            detail: "…".into(),
            action: ScopeAction::Open(Scope::Uncommitted),
        },
        ScopeItem {
            title: "Staged changes".into(),
            detail: "…".into(),
            action: ScopeAction::Open(Scope::Staged),
        },
    ];
    let forkable = forkable_branch(repo);
    if let Some(branch) = forkable.clone() {
        items.push(ScopeItem {
            title: "Branch vs fork point".into(),
            detail: "…".into(),
            action: ScopeAction::Open(Scope::BranchFork { branch }),
        });
    }
    // Explicit-base compare is always on the menu; when no fork point is
    // definable it explains why the entry above is missing.
    items.push(ScopeItem {
        title: "Branch vs a base you pick…".into(),
        detail: if forkable.is_some() {
            String::new()
        } else {
            "no fork point detected".into()
        },
        action: ScopeAction::PickBase,
    });
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
    review.rendered_review_width = area.width;
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
        let tree_width = review_tree_width(review, area.width);
        let [tree_area, stream_area] =
            Layout::horizontal([Constraint::Length(tree_width), Constraint::Min(0)]).areas(area);
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

fn review_tree_width(review: &ReviewState, available: u16) -> u16 {
    adaptive_tree_width(review.tree.preferred_width(), review.tree_width, available)
}

fn adaptive_tree_width(preferred: u16, manual: Option<u16>, available: u16) -> u16 {
    let diff_cap = available.saturating_sub(MIN_DIFF_WIDTH).max(1);
    if let Some(width) = manual {
        return width.max(MIN_MANUAL_TREE_WIDTH).min(diff_cap);
    }
    let fraction_cap = if available < 120 {
        available / 3
    } else {
        available.saturating_mul(2) / 5
    };
    let max_width = 72.min(fraction_cap).min(diff_cap);
    let desired = preferred.max(26);
    desired.min(max_width)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeResizeResult {
    Changed(u16),
    AtMinimum,
    AtMaximum,
    TerminalTooNarrow,
}

fn resized_tree_width(
    preferred: u16,
    manual: Option<u16>,
    available: u16,
    wider: bool,
) -> TreeResizeResult {
    let diff_cap = available.saturating_sub(MIN_DIFF_WIDTH).max(1);
    if diff_cap < MIN_MANUAL_TREE_WIDTH {
        return TreeResizeResult::TerminalTooNarrow;
    }
    let current = adaptive_tree_width(preferred, manual, available);
    if wider && current >= diff_cap {
        return TreeResizeResult::AtMaximum;
    }
    if !wider && current <= MIN_MANUAL_TREE_WIDTH {
        return TreeResizeResult::AtMinimum;
    }
    let requested = if wider {
        current.saturating_add(TREE_RESIZE_STEP)
    } else {
        current.saturating_sub(TREE_RESIZE_STEP)
    };
    TreeResizeResult::Changed(adaptive_tree_width(preferred, Some(requested), available))
}

fn resize_tree(review: &mut ReviewState, wider: bool) -> TreeResizeResult {
    review.show_tree = true;
    let result = resized_tree_width(
        review.tree.preferred_width(),
        review.tree_width,
        review.rendered_review_width,
        wider,
    );
    if let TreeResizeResult::Changed(width) = result {
        review.tree_width = Some(width);
    }
    result
}

fn resize_notice(result: TreeResizeResult) -> Option<&'static str> {
    match result {
        TreeResizeResult::Changed(_) => None,
        TreeResizeResult::AtMinimum => Some("tree is already at minimum width"),
        TreeResizeResult::AtMaximum => Some("tree is already at maximum width"),
        TreeResizeResult::TerminalTooNarrow => Some("terminal too narrow to resize panes"),
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

#[cfg(test)]
mod layout_tests {
    use super::{
        ReviewState, SingleLineInput, TreeResizeResult, adaptive_tree_width, edit_single_line,
        resize_notice, resized_tree_width,
    };
    use crate::comments::CommentStore;
    use crate::git::diff::{DiffLine, DiffResult, FileDiff, FileStatus, Hunk};
    use crate::git::scope::Scope;
    use git2::Repository;
    use ratatui::crossterm::event::KeyCode;

    #[test]
    fn tree_grows_on_wide_screens_but_preserves_diff_space() {
        assert_eq!(adaptive_tree_width(80, None, 90), 30);
        assert_eq!(adaptive_tree_width(80, None, 160), 64);
        assert_eq!(adaptive_tree_width(100, None, 240), 72);
    }

    #[test]
    fn short_trees_do_not_waste_wide_screen_space() {
        assert_eq!(adaptive_tree_width(18, None, 200), 26);
    }

    #[test]
    fn manual_width_can_exceed_auto_caps_but_preserves_diff_space() {
        assert_eq!(adaptive_tree_width(18, Some(100), 200), 100);
        assert_eq!(adaptive_tree_width(18, Some(180), 200), 140);
        assert_eq!(adaptive_tree_width(18, Some(8), 200), 16);
        assert_eq!(adaptive_tree_width(18, Some(40), 70), 10);
    }

    #[test]
    fn resize_reports_each_width_limit_precisely() {
        assert_eq!(
            resized_tree_width(18, Some(40), 70, false),
            TreeResizeResult::TerminalTooNarrow
        );
        assert_eq!(
            resized_tree_width(18, Some(16), 200, false),
            TreeResizeResult::AtMinimum
        );
        assert_eq!(
            resized_tree_width(18, Some(140), 200, true),
            TreeResizeResult::AtMaximum
        );
        assert_eq!(
            resized_tree_width(18, None, 160, true),
            TreeResizeResult::Changed(30)
        );
        assert_eq!(
            resize_notice(TreeResizeResult::AtMinimum),
            Some("tree is already at minimum width")
        );
        assert_eq!(
            resize_notice(TreeResizeResult::AtMaximum),
            Some("tree is already at maximum width")
        );
        assert_eq!(
            resize_notice(TreeResizeResult::TerminalTooNarrow),
            Some("terminal too narrow to resize panes")
        );
    }

    #[test]
    fn single_line_input_shares_edit_submit_and_cancel_behavior() {
        let mut input = "ab".to_string();
        assert!(matches!(
            edit_single_line(&mut input, KeyCode::Backspace),
            SingleLineInput::Continue
        ));
        assert_eq!(input, "a");
        edit_single_line(&mut input, KeyCode::Char('z'));
        assert_eq!(input, "az");
        assert!(matches!(
            edit_single_line(&mut input, KeyCode::Enter),
            SingleLineInput::Submit
        ));
        assert!(matches!(
            edit_single_line(&mut input, KeyCode::Esc),
            SingleLineInput::Cancel
        ));
    }

    #[test]
    fn comment_persistence_failure_does_not_block_a_fresh_review() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut store = CommentStore::load(&repo).unwrap();
        store
            .add(
                "a.rs".into(),
                true,
                (11, 11),
                vec!["+line 11".into()],
                "note".into(),
                "test".into(),
            )
            .unwrap();
        std::fs::create_dir(repo.path().join("gittre/comments.json.tmp")).unwrap();
        let diff = DiffResult::from_files(vec![FileDiff {
            path: "a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            large: false,
            byte_size: 0,
            untracked_dir: false,
            full_context: false,
            hunks: vec![Hunk {
                header: "@@ @@".into(),
                lines: vec![DiffLine {
                    origin: '+',
                    old_lineno: None,
                    new_lineno: Some(14),
                    content: "line 11".into(),
                    expanded_context: false,
                }],
            }],
            additions: 1,
            deletions: 0,
        }]);

        let (review, warning) = ReviewState::new(Scope::Uncommitted, diff, &mut store);

        assert_eq!(review.diff.files.len(), 1);
        assert!(!review.stream.has_comments());
        assert!(warning.unwrap().contains("inline comments hidden"));
        assert_eq!(store.comments()[0].lines, (11, 11));
    }

    #[test]
    fn failed_rebuild_never_keeps_rows_for_an_old_comment_vector() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let mut store = CommentStore::load(&repo).unwrap();
        for (line, body) in [(11, "first"), (12, "second")] {
            store
                .add(
                    "a.rs".into(),
                    true,
                    (line, line),
                    vec![format!("+line {line}")],
                    body.into(),
                    "test".into(),
                )
                .unwrap();
        }
        let diff = DiffResult::from_files(vec![FileDiff {
            path: "a.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            large: false,
            byte_size: 0,
            untracked_dir: false,
            full_context: false,
            hunks: vec![Hunk {
                header: "@@ @@".into(),
                lines: [11, 12]
                    .into_iter()
                    .map(|line| DiffLine {
                        origin: '+',
                        old_lineno: None,
                        new_lineno: Some(line),
                        content: format!("line {line}"),
                        expanded_context: false,
                    })
                    .collect(),
            }],
            additions: 2,
            deletions: 0,
        }]);
        let (mut review, warning) = ReviewState::new(Scope::Uncommitted, diff, &mut store);
        assert!(warning.is_none());
        assert!(review.stream.has_comments());

        store.delete(1).unwrap();
        review.diff.files[0].hunks[0].lines[1].new_lineno = Some(15);
        std::fs::create_dir(repo.path().join("gittre/comments.json.tmp")).unwrap();

        let warning = review.rebuild(&mut store);

        assert!(warning.unwrap().contains("inline comments hidden"));
        assert!(!review.stream.has_comments());
        assert_eq!(store.comments().len(), 1);
    }
}
