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

use crate::comments::{Comment, CommentStore, export_markdown};
use crate::event::{AppEvent, LoadRequest, ScopeCounts, spawn_loader};
use crate::git::diff::DiffResult;
use crate::git::log::commit_log;
use crate::git::scope::{Scope, base_candidates, file_content, forkable_branch};
use crate::keymap::{self, Action, KeyPress, Lookup};
use crate::ui::fileview::FileView;
use crate::ui::highlight::Highlighter;
use crate::ui::picker::{BasePicker, LogPicker, ScopeAction, ScopeItem, ScopePicker, count_label};
use crate::ui::review::{CommentTarget, Stream};
use crate::ui::tree::FileTree;
use crate::ui::{bar, editor::TextEditor, export::ExportPreview, popups};

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
    /// None is content-aware automatic sizing. A future drag gesture can set
    /// a manual width without changing the pane-layout contract.
    tree_width: Option<u16>,
    /// Full-file pager overlay (`o`).
    file_view: Option<FileView>,
}

impl ReviewState {
    fn new(scope: Scope, diff: DiffResult, store: &mut CommentStore) -> Result<Self> {
        let placed = store.reanchor(&diff)?;
        let stream = Stream::new(&diff, &placed, &store.comments);
        let tree = FileTree::new(&diff.files, &comment_counts(&diff, store));
        Ok(ReviewState {
            scope,
            diff,
            stream,
            tree,
            focus: Focus::Stream,
            show_tree: true,
            tree_width: None,
            file_view: None,
        })
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
    /// Monotonic id pairing reload requests with responses.
    seq: u64,
    reloading: bool,
    reloaded_at: Option<Instant>,
    /// In-progress `/` query; Some while the user is typing it.
    search_input: Option<String>,
    /// Output path being typed for markdown export.
    export_input: Option<String>,
    /// Exact Markdown preview shown before an interactive export is written.
    export_preview: Option<ExportPreview>,
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
    /// What the last comment delete removed; `u` puts it back.
    undo_comments: Vec<Comment>,
    /// First key of a chord (`g`), held until the next key resolves it.
    pending_key: Option<KeyPress>,
    /// Scroll offset of the `?` help popup; clamped at render time.
    help_scroll: u16,
}

struct CommentDraft {
    editor: TextEditor,
    /// Some(id) when editing an existing comment.
    editing: Option<u64>,
    /// None when editing (anchor is kept from the original).
    target: Option<CommentTarget>,
    label: String,
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
            show_help: false,
            error: None,
            quit: false,
            events,
            loader,
            seq: 0,
            reloading: false,
            reloaded_at: None,
            search_input: None,
            export_input: None,
            export_preview: None,
            notice: None,
            pending_editor: None,
            highlighter: Highlighter::new(),
            store,
            comment_draft: None,
            confirm_clear: false,
            undo_comments: Vec::new(),
            pending_key: None,
            help_scroll: 0,
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
        // Match keyboard routing: the topmost overlay consumes the event so
        // mouse input can never mutate a hidden screen underneath it.
        if self.show_help {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.help_scroll = self.help_scroll.saturating_add(3),
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(3),
                MouseEventKind::Down(MouseButton::Left) => self.show_help = false,
                _ => {}
            }
            return;
        }
        if let Some(draft) = &mut self.comment_draft {
            match mouse.kind {
                MouseEventKind::ScrollDown => draft.editor.scroll_by(3),
                MouseEventKind::ScrollUp => draft.editor.scroll_by(-3),
                MouseEventKind::Down(MouseButton::Left) => {
                    draft.editor.set_cursor_from_screen(col, row)
                }
                _ => {}
            }
            return;
        }
        if self.confirm_clear || self.search_input.is_some() || self.export_input.is_some() {
            return;
        }
        if let Some(preview) = &mut self.export_preview {
            match mouse.kind {
                MouseEventKind::ScrollDown => preview.scroll_by(3),
                MouseEventKind::ScrollUp => preview.scroll_by(-3),
                MouseEventKind::Down(MouseButton::Left) => {}
                _ => {}
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
        if let Some(draft) = &mut self.comment_draft {
            draft.editor.insert_str(text);
        } else if let Some(path) = &mut self.export_input {
            path.push_str(&text.replace(['\r', '\n'], ""));
        } else if let Some(query) = &mut self.search_input {
            query.push_str(&text.replace(['\r', '\n'], ""));
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
                if let Some(idx) = review.tree.hit(col, row) {
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
                        match apply_reload(review, diff, &mut self.store) {
                            Ok(()) => self.reloaded_at = Some(Instant::now()),
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                    // Otherwise it's a scope being opened.
                    _ => match ReviewState::new(scope, diff, &mut self.store) {
                        Ok(review) => {
                            debug_log(&format!(
                                "review created: {} files",
                                review.diff.files.len()
                            ));
                            self.screen = Screen::Review(Box::new(review));
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    },
                }
            }
            AppEvent::File {
                seq,
                scope,
                path,
                files,
            } => {
                if seq != self.seq || !matches!(&self.screen, Screen::Review(r) if r.scope == scope)
                {
                    return;
                }
                let loaded = match files {
                    Ok(f) => f,
                    Err(e) => {
                        self.error = Some(format!("{e:#}"));
                        return;
                    }
                };
                if let Screen::Review(review) = &mut self.screen {
                    if let Some(pos) = review.diff.files.iter().position(|f| f.path == path) {
                        let mut files = review.diff.files.clone();
                        files.splice(pos..=pos, loaded);
                        // Treat expansion like a small reload: if comment
                        // persistence fails, the old diff/stream stay paired.
                        let diff = DiffResult::from_files(files);
                        if let Err(e) = apply_reload(review, diff, &mut self.store) {
                            self.error = Some(format!("{e:#}"));
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
                &self.store.comments,
            ),
        }

        if let Some(preview) = &mut self.export_preview {
            preview.render(frame, main_area, self.store.comments.len());
        }

        if let Some(query) = &self.search_input {
            bar::render_input(frame, bar_area, "/", query, "search");
        } else if let Some(path) = &self.export_input {
            bar::render_input(frame, bar_area, "export to ", path, "write");
        } else {
            bar::render(frame, bar_area, &self.hints());
        }

        let (comment_bounds, comment_anchor) = match &self.screen {
            Screen::Review(review) => (
                review.stream.viewport_rect(),
                review.stream.cursor_screen_position(),
            ),
            _ => (main_area, None),
        };
        if let Some(draft) = &mut self.comment_draft {
            popups::render_comment_editor(
                frame,
                comment_bounds,
                comment_anchor,
                &draft.label,
                &mut draft.editor,
            );
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
            popups::render_help(frame, frame.area(), &mut self.help_scroll);
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

    /// The table whose bindings apply to the next key press.
    fn active_table(&self) -> &'static [keymap::Binding] {
        if self.show_help {
            return keymap::HELP_VIEW;
        }
        if self.export_preview.is_some() {
            return keymap::EXPORT_PREVIEW;
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
        if self.show_help {
            return vec![
                h(keymap::HELP_VIEW, &[Down, Up], "scroll"),
                raw("q/Esc/?", "close help"),
            ];
        }
        if self.comment_draft.is_some() {
            return vec![
                raw("←↑↓→", "move"),
                raw("⏎", "save"),
                raw("Alt+⏎", "newline"),
                raw("Esc", "cancel"),
            ];
        }
        if self.confirm_clear {
            return vec![raw("y", "delete all comments"), raw("any key", "cancel")];
        }
        if self.search_input.is_some() {
            return vec![raw("⏎", "search"), raw("Esc", "cancel")];
        }
        if self.export_input.is_some() {
            return vec![raw("⏎", "write file"), raw("Esc", "cancel")];
        }
        if self.export_preview.is_some() {
            let t = keymap::EXPORT_PREVIEW;
            return vec![
                h(t, &[Down, Up], "scroll"),
                h(t, &[GotoTop, GotoBottom], "top/bottom"),
                h(t, &[CopyMarkdown], "copy markdown"),
                h(t, &[WriteFile], "write file"),
                raw("Esc", "close"),
            ];
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
                        h(t, &[SwitchScope], "scope"),
                        raw("?", "help"),
                    ];
                }
                let mut hints = vec![
                    h(t, &[Down, Up], "scroll"),
                    h(t, &[NextFile, PrevFile], "file"),
                ];
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

        if self.show_help {
            self.on_help_key(key, pending);
            return;
        }
        if self.comment_draft.is_some() {
            self.on_draft_key(code, modifiers);
            return;
        }
        if self.confirm_clear {
            self.confirm_clear = false;
            if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                let deleted = self.store.comments.clone();
                if let Err(e) = self.store.delete_all() {
                    self.error = Some(format!("{e:#}"));
                } else {
                    self.undo_comments = deleted;
                    self.notice = Some("all comments deleted (u restores)".into());
                }
                self.rebuild_review();
            }
            return;
        }
        if self.search_input.is_some() {
            self.on_search_input_key(code);
            return;
        }
        if self.export_input.is_some() {
            self.on_export_input_key(code);
            return;
        }
        if self.export_preview.is_some() {
            self.on_export_preview_key(key, pending);
            return;
        }
        if code == KeyCode::Char('?') {
            self.show_help = true;
            self.help_scroll = 0;
            return;
        }

        match &mut self.screen {
            Screen::Picker(_) => self.on_picker_key(key, pending),
            Screen::Log(_) => self.on_log_key(key, pending),
            Screen::Base(_) => self.on_base_key(key, pending),
            Screen::Review(_) => self.on_review_key(key, pending),
        }
    }

    fn on_help_key(&mut self, key: KeyPress, pending: Option<KeyPress>) {
        match keymap::lookup(keymap::HELP_VIEW, keymap::Ctx::default(), pending, key) {
            Lookup::Act(action) => match action {
                Action::Down => self.help_scroll = self.help_scroll.saturating_add(1),
                Action::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                Action::PageDown | Action::HalfPageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(10)
                }
                Action::PageUp | Action::HalfPageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10)
                }
                Action::Back => self.show_help = false,
                _ => {}
            },
            Lookup::Pending => self.pending_key = Some(key),
            Lookup::None => {}
        }
    }

    fn on_draft_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let Some(draft) = &mut self.comment_draft else {
            return;
        };
        match code {
            KeyCode::Esc => self.comment_draft = None,
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
            KeyCode::Enter if modifiers.contains(KeyModifiers::ALT) => {
                draft.editor.insert_char('\n')
            }
            KeyCode::Enter => {
                if draft.editor.as_str().trim().is_empty() {
                    self.comment_draft = None;
                    self.notice = Some("empty comment discarded".into());
                    return;
                }
                let body = draft.editor.as_str().to_string();
                let editing = draft.editing;
                let target = draft.target.clone();
                let scope_label = match &self.screen {
                    Screen::Review(r) => r.scope.label(),
                    _ => String::new(),
                };
                let result = match (editing, target) {
                    (Some(id), _) => self.store.edit(id, body),
                    (None, Some(t)) => {
                        self.store
                            .add(t.path, t.new_side, t.lines, t.snippet, body, scope_label)
                    }
                    (None, None) => Ok(()),
                };
                if let Err(e) = result {
                    self.error = Some(format!("{e:#}"));
                    return; // keep the draft open so the text can be retried
                }
                self.comment_draft = None;
                self.rebuild_review();
            }
            KeyCode::Char(c) => draft.editor.insert_char(c),
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
        let placed = match self.store.reanchor(&review.diff) {
            Ok(placed) => placed,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
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

    fn on_export_input_key(&mut self, code: KeyCode) {
        let Some(input) = &mut self.export_input else {
            return;
        };
        match code {
            KeyCode::Esc => self.export_input = None,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Enter => {
                let path = self.export_input.clone().unwrap_or_default();
                if path.trim().is_empty() {
                    return;
                }
                let md = self
                    .export_preview
                    .as_ref()
                    .map(|preview| preview.markdown().to_string())
                    .unwrap_or_else(|| {
                        export_markdown(
                            &self.store.comments,
                            &repo_title(&self.repo),
                            &today_string(),
                        )
                    });
                match std::fs::write(&path, md) {
                    Ok(()) => {
                        self.export_input = None;
                        self.export_preview = None;
                        self.notice = Some(format!(
                            "exported {} comment{} to {path}",
                            self.store.comments.len(),
                            if self.store.comments.len() == 1 {
                                ""
                            } else {
                                "s"
                            },
                        ))
                    }
                    Err(e) => self.error = Some(format!("export failed: {e}")),
                }
            }
            KeyCode::Char(c) => input.push(c),
            _ => {}
        }
    }

    fn on_export_preview_key(&mut self, key: KeyPress, pending: Option<KeyPress>) {
        let Some(preview) = &mut self.export_preview else {
            return;
        };
        match keymap::lookup(keymap::EXPORT_PREVIEW, keymap::Ctx::default(), pending, key) {
            Lookup::Act(action) => match action {
                Action::Back => self.export_preview = None,
                Action::Down => preview.scroll_by(1),
                Action::Up => preview.scroll_by(-1),
                Action::PageDown => preview.page(1),
                Action::PageUp => preview.page(-1),
                Action::HalfPageDown => preview.half_page(1),
                Action::HalfPageUp => preview.half_page(-1),
                Action::GotoTop => preview.top(),
                Action::GotoBottom => preview.bottom(),
                Action::WriteFile => {
                    self.export_input = Some(format!("review-{}.md", today_string()));
                }
                Action::CopyMarkdown => {
                    let markdown = preview.markdown().to_string();
                    match arboard::Clipboard::new()
                        .and_then(|mut clipboard| clipboard.set_text(markdown))
                    {
                        Ok(()) => self.notice = Some("copied export markdown".into()),
                        Err(e) => self.error = Some(format!("clipboard: {e}")),
                    }
                }
                _ => {}
            },
            Lookup::Pending => self.pending_key = Some(key),
            Lookup::None => {}
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
            Action::Search => self.search_input = Some(String::new()),
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
                if self.store.comments.is_empty() {
                    self.error = Some("no comments to export".into());
                } else {
                    let markdown = export_markdown(
                        &self.store.comments,
                        &repo_title(&self.repo),
                        &today_string(),
                    );
                    self.export_preview = Some(ExportPreview::new(markdown));
                }
            }
            Action::Comment => {
                if let Some(t) = review.stream.selection_target(&review.diff.files) {
                    review.stream.cancel_selection();
                    self.comment_draft = Some(CommentDraft {
                        editor: TextEditor::new(String::new()),
                        editing: None,
                        label: target_label(&t),
                        target: Some(t),
                    });
                } else if let Some(ci) = review.stream.comment_at_cursor() {
                    let c = &self.store.comments[ci];
                    self.comment_draft = Some(CommentDraft {
                        editor: TextEditor::new(c.body.clone()),
                        editing: Some(c.id),
                        target: None,
                        label: format!("edit #{} on {}", c.id, c.path),
                    });
                } else if let Some(t) = review.stream.line_target(&review.diff.files) {
                    self.comment_draft = Some(CommentDraft {
                        editor: TextEditor::new(String::new()),
                        editing: None,
                        label: target_label(&t),
                        target: Some(t),
                    });
                }
            }
            Action::DeleteComment => {
                if let Some(ci) = review.stream.comment_at_cursor() {
                    let comment = self.store.comments[ci].clone();
                    let id = comment.id;
                    if let Err(e) = self.store.delete(id) {
                        self.error = Some(format!("{e:#}"));
                    } else {
                        self.undo_comments = vec![comment];
                        self.notice = Some(format!("deleted comment #{id} (u restores)"));
                    }
                    self.rebuild_review();
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
                    }
                    self.rebuild_review();
                }
            }
            Action::DeleteAllComments => {
                if !self.store.comments.is_empty() {
                    self.confirm_clear = true;
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
                        untracked_dir,
                    });
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
    let fraction_cap = if available < 120 {
        available / 3
    } else {
        available.saturating_mul(2) / 5
    };
    let diff_cap = available.saturating_sub(60);
    let max_width = 72.min(fraction_cap).min(diff_cap.max(1));
    let desired = manual.unwrap_or(preferred).max(26);
    desired.min(max_width)
}

/// Swap in a freshly loaded diff, preserving the reader's position (by file
/// path + offset), the tree's fold state, and focus.
fn apply_reload(
    review: &mut ReviewState,
    diff: DiffResult,
    store: &mut CommentStore,
) -> Result<()> {
    let anchor = review.stream.anchor(&review.diff.files);
    let collapsed = review.tree.collapsed_dirs();
    let query = review.stream.search_query();

    let placed = store.reanchor(&diff)?;
    review.diff = diff;
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
    Ok(())
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
    use super::adaptive_tree_width;

    #[test]
    fn tree_grows_on_wide_screens_but_preserves_diff_space() {
        assert_eq!(adaptive_tree_width(80, None, 90), 30);
        assert_eq!(adaptive_tree_width(80, None, 160), 64);
        assert_eq!(adaptive_tree_width(100, None, 240), 72);
    }

    #[test]
    fn short_trees_do_not_waste_wide_screen_space() {
        assert_eq!(adaptive_tree_width(18, None, 200), 26);
        assert_eq!(adaptive_tree_width(18, Some(55), 200), 55);
    }
}
