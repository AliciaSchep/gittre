//! Every keybinding lives here. Each screen/overlay has a table mapping key
//! sequences to [`Action`]s; the dispatchers in app.rs, the command-bar
//! hints, and the `?` help popup all read the same tables, so a binding is
//! changed in exactly one place. Two-key chords (helix-style `gg`/`ge`) are
//! first-class: a chord's first key reports [`Lookup::Pending`] and the app
//! holds it until the next key.

use ratatui::crossterm::event::{KeyCode, KeyModifiers};

/// Everything a key can do, across all screens and overlays. A table decides
/// which subset applies where; the same action may have different keys in
/// different contexts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    // navigation, shared by every scrollable context
    Down,
    Up,
    PageDown,
    PageUp,
    HalfPageDown,
    HalfPageUp,
    GotoTop,
    GotoBottom,
    // review stream
    NextFile,
    PrevFile,
    NextHunk,
    PrevHunk,
    NextMatch,
    PrevMatch,
    Search,
    ToggleSelection,
    SelectLine,
    CopyCode,
    CopyPatch,
    Comment,
    DeleteComment,
    RestoreComments,
    DeleteAllComments,
    NextComment,
    PrevComment,
    ExportPreview,
    FileView,
    OpenEditor,
    Reload,
    ToggleTree,
    FocusTree,
    NarrowTree,
    WidenTree,
    AutoTreeWidth,
    Activate,
    Back,
    SwitchScope,
    Quit,
    // commit log
    MarkRange,
    // export preview
    CopyMarkdown,
    WriteFile,
}

/// A key with the modifiers that distinguish bindings. Shift never does:
/// it is already encoded in the char (`G` vs `g`) and terminals disagree
/// about reporting it on other keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyPress {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeyPress {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        KeyPress {
            code,
            mods: mods.intersection(KeyModifiers::CONTROL.union(KeyModifiers::ALT)),
        }
    }
}

const fn k(code: KeyCode) -> KeyPress {
    KeyPress {
        code,
        mods: KeyModifiers::NONE,
    }
}
const fn c(ch: char) -> KeyPress {
    k(KeyCode::Char(ch))
}
const fn ctrl(ch: char) -> KeyPress {
    KeyPress {
        code: KeyCode::Char(ch),
        mods: KeyModifiers::CONTROL,
    }
}

/// When a binding applies. Keys may be overloaded across guards (`n` is
/// next-match during a live search, next-hunk otherwise).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Guard {
    Always,
    SearchActive,
    NoSearch,
}

pub struct Binding {
    /// One key, or a two-key chord (`gg`).
    pub seq: &'static [KeyPress],
    pub guard: Guard,
    pub action: Action,
}

const fn bind(seq: &'static [KeyPress], action: Action) -> Binding {
    Binding {
        seq,
        guard: Guard::Always,
        action,
    }
}
const fn bind_if(guard: Guard, seq: &'static [KeyPress], action: Action) -> Binding {
    Binding { seq, guard, action }
}

use Action::*;

/// The main diff stream. The first binding listed for an action is the one
/// hints and help display, so letter keys go before their arrow/function-key
/// aliases.
pub static REVIEW: &[Binding] = &[
    bind(&[c('j')], Down),
    bind(&[k(KeyCode::Down)], Down),
    bind(&[c('k')], Up),
    bind(&[k(KeyCode::Up)], Up),
    bind(&[ctrl('d')], HalfPageDown),
    bind(&[ctrl('u')], HalfPageUp),
    bind(&[ctrl('f')], PageDown),
    bind(&[k(KeyCode::PageDown)], PageDown),
    bind(&[ctrl('b')], PageUp),
    bind(&[k(KeyCode::PageUp)], PageUp),
    bind(&[c('g'), c('g')], GotoTop),
    bind(&[k(KeyCode::Home)], GotoTop),
    bind(&[c('g'), c('e')], GotoBottom),
    bind(&[c('G')], GotoBottom),
    bind(&[k(KeyCode::End)], GotoBottom),
    bind(&[c(']')], NextFile),
    bind(&[c('[')], PrevFile),
    bind_if(Guard::NoSearch, &[c('n')], NextHunk),
    bind(&[c('p')], PrevHunk),
    bind_if(Guard::SearchActive, &[c('n')], NextMatch),
    bind_if(Guard::SearchActive, &[c('N')], PrevMatch),
    bind(&[c('/')], Search),
    bind(&[c('v')], ToggleSelection),
    bind(&[c('x')], SelectLine),
    bind(&[c('y')], CopyCode),
    bind(&[c('Y')], CopyPatch),
    bind(&[c('c')], Comment),
    bind(&[c('d')], DeleteComment),
    bind(&[c('u')], RestoreComments),
    bind(&[c('D')], DeleteAllComments),
    bind(&[c('}')], NextComment),
    bind(&[c('{')], PrevComment),
    bind(&[c('e')], ExportPreview),
    bind(&[c('o')], FileView),
    bind(&[c('E')], OpenEditor),
    bind(&[c('r')], Reload),
    bind(&[c('t')], ToggleTree),
    bind(&[c('<')], NarrowTree),
    bind(&[c('>')], WidenTree),
    bind(&[c('=')], AutoTreeWidth),
    bind(&[k(KeyCode::Tab)], FocusTree),
    bind(&[k(KeyCode::BackTab)], FocusTree),
    bind(&[k(KeyCode::Enter)], Activate),
    bind(&[k(KeyCode::Esc)], Back),
    bind(&[c('q')], SwitchScope),
];

/// The scope picker (digits 1-9 are handled separately in app.rs).
pub static PICKER: &[Binding] = &[
    bind(&[c('j')], Down),
    bind(&[k(KeyCode::Down)], Down),
    bind(&[c('k')], Up),
    bind(&[k(KeyCode::Up)], Up),
    bind(&[k(KeyCode::Enter)], Activate),
    bind(&[c('q')], Quit),
    bind(&[k(KeyCode::Esc)], Quit),
];

/// The commit log list.
pub static LOG: &[Binding] = &[
    bind(&[c('j')], Down),
    bind(&[k(KeyCode::Down)], Down),
    bind(&[c('k')], Up),
    bind(&[k(KeyCode::Up)], Up),
    bind(&[ctrl('d')], HalfPageDown),
    bind(&[ctrl('u')], HalfPageUp),
    bind(&[ctrl('f')], PageDown),
    bind(&[k(KeyCode::PageDown)], PageDown),
    bind(&[ctrl('b')], PageUp),
    bind(&[k(KeyCode::PageUp)], PageUp),
    bind(&[c('g'), c('g')], GotoTop),
    bind(&[k(KeyCode::Home)], GotoTop),
    bind(&[c('g'), c('e')], GotoBottom),
    bind(&[c('G')], GotoBottom),
    bind(&[k(KeyCode::End)], GotoBottom),
    bind(&[c(' ')], MarkRange),
    bind(&[k(KeyCode::Enter)], Activate),
    bind(&[c('q')], Back),
    bind(&[k(KeyCode::Esc)], Back),
];

/// The base-branch list.
pub static BASE: &[Binding] = &[
    bind(&[c('j')], Down),
    bind(&[k(KeyCode::Down)], Down),
    bind(&[c('k')], Up),
    bind(&[k(KeyCode::Up)], Up),
    bind(&[ctrl('d')], HalfPageDown),
    bind(&[ctrl('u')], HalfPageUp),
    bind(&[ctrl('f')], PageDown),
    bind(&[k(KeyCode::PageDown)], PageDown),
    bind(&[ctrl('b')], PageUp),
    bind(&[k(KeyCode::PageUp)], PageUp),
    bind(&[c('g'), c('g')], GotoTop),
    bind(&[k(KeyCode::Home)], GotoTop),
    bind(&[c('g'), c('e')], GotoBottom),
    bind(&[c('G')], GotoBottom),
    bind(&[k(KeyCode::End)], GotoBottom),
    bind(&[k(KeyCode::Enter)], Activate),
    bind(&[c('q')], Back),
    bind(&[k(KeyCode::Esc)], Back),
];

/// The full-file pager (`o`).
pub static FILE_VIEW: &[Binding] = &[
    bind(&[c('j')], Down),
    bind(&[k(KeyCode::Down)], Down),
    bind(&[c('k')], Up),
    bind(&[k(KeyCode::Up)], Up),
    bind(&[ctrl('d')], HalfPageDown),
    bind(&[ctrl('u')], HalfPageUp),
    bind(&[ctrl('f')], PageDown),
    bind(&[k(KeyCode::PageDown)], PageDown),
    bind(&[ctrl('b')], PageUp),
    bind(&[k(KeyCode::PageUp)], PageUp),
    bind(&[c('g'), c('g')], GotoTop),
    bind(&[k(KeyCode::Home)], GotoTop),
    bind(&[c('g'), c('e')], GotoBottom),
    bind(&[c('G')], GotoBottom),
    bind(&[k(KeyCode::End)], GotoBottom),
    bind(&[c('E')], OpenEditor),
    bind(&[c('q')], Back),
    bind(&[k(KeyCode::Esc)], Back),
    bind(&[c('o')], Back),
];

/// The comment-export markdown preview (`e`).
pub static EXPORT_PREVIEW: &[Binding] = &[
    bind(&[c('j')], Down),
    bind(&[k(KeyCode::Down)], Down),
    bind(&[c('k')], Up),
    bind(&[k(KeyCode::Up)], Up),
    bind(&[ctrl('d')], HalfPageDown),
    bind(&[ctrl('u')], HalfPageUp),
    bind(&[ctrl('f')], PageDown),
    bind(&[k(KeyCode::PageDown)], PageDown),
    bind(&[ctrl('b')], PageUp),
    bind(&[k(KeyCode::PageUp)], PageUp),
    bind(&[c('g'), c('g')], GotoTop),
    bind(&[k(KeyCode::Home)], GotoTop),
    bind(&[c('g'), c('e')], GotoBottom),
    bind(&[c('G')], GotoBottom),
    bind(&[k(KeyCode::End)], GotoBottom),
    bind(&[c('y')], CopyMarkdown),
    bind(&[c('w')], WriteFile),
    bind(&[k(KeyCode::Enter)], WriteFile),
    bind(&[c('q')], Back),
    bind(&[k(KeyCode::Esc)], Back),
    bind(&[c('e')], Back),
];

/// The `?` help popup (scrollable).
pub static HELP_VIEW: &[Binding] = &[
    bind(&[c('j')], Down),
    bind(&[k(KeyCode::Down)], Down),
    bind(&[c('k')], Up),
    bind(&[k(KeyCode::Up)], Up),
    bind(&[ctrl('d')], HalfPageDown),
    bind(&[ctrl('u')], HalfPageUp),
    bind(&[ctrl('f')], PageDown),
    bind(&[k(KeyCode::PageDown)], PageDown),
    bind(&[ctrl('b')], PageUp),
    bind(&[k(KeyCode::PageUp)], PageUp),
    bind(&[c('q')], Back),
    bind(&[k(KeyCode::Esc)], Back),
    bind(&[c('?')], Back),
];

/// What a binding table resolves against.
#[derive(Clone, Copy, Default)]
pub struct Ctx {
    pub search_active: bool,
}

pub enum Lookup {
    Act(Action),
    /// The key starts a chord; hold it and resolve on the next key.
    Pending,
    None,
}

pub fn lookup(table: &[Binding], ctx: Ctx, pending: Option<KeyPress>, key: KeyPress) -> Lookup {
    let live = |b: &Binding| match b.guard {
        Guard::Always => true,
        Guard::SearchActive => ctx.search_active,
        Guard::NoSearch => !ctx.search_active,
    };
    if let Some(first) = pending {
        for b in table.iter().filter(|b| live(b)) {
            if b.seq.len() == 2 && b.seq[0] == first && b.seq[1] == key {
                return Lookup::Act(b.action);
            }
        }
        // A broken chord swallows the key rather than firing something else.
        return Lookup::None;
    }
    // A chord prefix shadows any single binding on the same key.
    if table
        .iter()
        .any(|b| live(b) && b.seq.len() == 2 && b.seq[0] == key)
    {
        return Lookup::Pending;
    }
    for b in table.iter().filter(|b| live(b)) {
        if b.seq.len() == 1 && b.seq[0] == key {
            return Lookup::Act(b.action);
        }
    }
    Lookup::None
}

// ---- display ---------------------------------------------------------

pub fn key_display(key: KeyPress) -> String {
    let base = match key.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "⏎".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "Shift+Tab".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        other => format!("{other:?}"),
    };
    if key.mods.contains(KeyModifiers::CONTROL) {
        format!("⌃{base}")
    } else if key.mods.contains(KeyModifiers::ALT) {
        format!("Alt+{base}")
    } else {
        base
    }
}

pub fn seq_display(seq: &[KeyPress]) -> String {
    seq.iter().map(|k| key_display(*k)).collect()
}

/// Display of the primary (first-listed) binding for an action.
pub fn key_for(table: &[Binding], action: Action) -> String {
    table
        .iter()
        .find(|b| b.action == action)
        .map(|b| seq_display(b.seq))
        .unwrap_or_default()
}

/// "j/k"-style label built from the primary keys of several actions.
pub fn keys_label(table: &[Binding], actions: &[Action]) -> String {
    actions
        .iter()
        .map(|a| key_for(table, *a))
        .collect::<Vec<_>>()
        .join("/")
}

/// The chords a held prefix key could still complete, for the hint bar.
pub fn chords_from(table: &[Binding], prefix: KeyPress) -> Vec<(String, &'static str)> {
    table
        .iter()
        .filter(|b| b.seq.len() == 2 && b.seq[0] == prefix)
        .map(|b| (seq_display(b.seq), hint_desc(b.action)))
        .collect()
}

/// Terse action names for the hint bar.
pub fn hint_desc(action: Action) -> &'static str {
    match action {
        GotoTop => "top",
        GotoBottom => "bottom",
        _ => "",
    }
}

// ---- help popup content ------------------------------------------------

pub enum HelpItem {
    /// Keys derived from the REVIEW table so they can never drift.
    Act(&'static [Action], &'static str),
    /// Keys that live outside the tables (digits, editor keys, mouse).
    Raw(&'static str, &'static str),
}

pub static HELP: &[(&str, &[HelpItem])] = &[
    (
        "Navigate",
        &[
            HelpItem::Act(&[Down, Up], "move the cursor (or move in a list)"),
            HelpItem::Act(&[HalfPageDown, HalfPageUp], "half page down / up"),
            HelpItem::Act(&[PageDown, PageUp], "page down / up"),
            HelpItem::Act(&[GotoTop, GotoBottom], "jump to top / bottom"),
            HelpItem::Act(&[NextFile, PrevFile], "next / previous file"),
            HelpItem::Act(&[NextHunk, PrevHunk], "next / previous hunk"),
            HelpItem::Raw("↑↓ PgUp PgDn", "arrows and paging keys work too"),
        ],
    ),
    (
        "Search",
        &[
            HelpItem::Act(&[Search], "search the diff"),
            HelpItem::Act(
                &[NextMatch, PrevMatch],
                "next / previous match while a search is live",
            ),
        ],
    ),
    (
        "Select & copy",
        &[
            HelpItem::Act(&[ToggleSelection], "select lines (again or Esc cancels)"),
            HelpItem::Act(&[SelectLine], "select the current line; extends line-wise"),
            HelpItem::Act(&[CopyCode, CopyPatch], "copy selection as code / as patch"),
        ],
    ),
    (
        "Comments",
        &[
            HelpItem::Act(
                &[Comment],
                "comment on the current line (or selection / edit)",
            ),
            HelpItem::Act(&[NextComment, PrevComment], "next / previous comment"),
            HelpItem::Act(&[DeleteComment], "delete the comment at the cursor"),
            HelpItem::Act(&[RestoreComments], "restore what the last delete removed"),
            HelpItem::Act(&[DeleteAllComments], "delete ALL comments (asks first)"),
            HelpItem::Act(
                &[ExportPreview],
                "preview comment export (then copy or write markdown)",
            ),
            HelpItem::Raw("⏎ / Alt+⏎", "in the comment editor: save / newline"),
            HelpItem::Raw("←↑↓→ Home End", "move the caret while editing a comment"),
        ],
    ),
    (
        "Views & scope",
        &[
            HelpItem::Act(
                &[Activate],
                "show / collapse the full file inline (or load a stub)",
            ),
            HelpItem::Act(
                &[FileView],
                "view the full file (read-only, at the current line)",
            ),
            HelpItem::Act(
                &[OpenEditor],
                "open the file in $EDITOR at the current line",
            ),
            HelpItem::Act(&[FocusTree], "pick a file from the tree (Esc cancels)"),
            HelpItem::Act(&[ToggleTree], "show / hide the file tree"),
            HelpItem::Act(
                &[NarrowTree, WidenTree, AutoTreeWidth],
                "narrow / widen / auto-size the file tree",
            ),
            HelpItem::Act(&[Reload], "reload the diff from the repository"),
            HelpItem::Act(&[SwitchScope], "switch scope (back to the picker)"),
        ],
    ),
    (
        "General",
        &[
            HelpItem::Raw("1-9", "picker: open that entry"),
            HelpItem::Raw("Space", "commit log: mark an inclusive range start"),
            HelpItem::Raw("?", "toggle this help"),
            HelpItem::Raw("q / Esc", "back; quits from the picker"),
            HelpItem::Raw("⌃c", "quit from anywhere"),
            HelpItem::Raw(
                "mouse",
                "wheel scrolls; click picks (shift+drag selects text)",
            ),
        ],
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyPress {
        KeyPress::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn chords_resolve_in_two_steps() {
        let g = press(KeyCode::Char('g'));
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), None, g),
            Lookup::Pending
        ));
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), Some(g), g),
            Lookup::Act(Action::GotoTop)
        ));
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), Some(g), press(KeyCode::Char('e'))),
            Lookup::Act(Action::GotoBottom)
        ));
        // A broken chord swallows the key: gj must not scroll or fire g-alone.
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), Some(g), press(KeyCode::Char('j'))),
            Lookup::None
        ));
    }

    #[test]
    fn control_distinguishes_bindings() {
        let d = press(KeyCode::Char('d'));
        let ctrl_d = KeyPress::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), None, d),
            Lookup::Act(Action::DeleteComment)
        ));
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), None, ctrl_d),
            Lookup::Act(Action::HalfPageDown)
        ));
    }

    #[test]
    fn shift_is_normalized_away() {
        // Terminals report G as Char('G') + SHIFT; the char alone decides.
        let big_g = KeyPress::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), None, big_g),
            Lookup::Act(Action::GotoBottom)
        ));
    }

    #[test]
    fn search_guard_overloads_n() {
        let n = press(KeyCode::Char('n'));
        assert!(matches!(
            lookup(REVIEW, Ctx::default(), None, n),
            Lookup::Act(Action::NextHunk)
        ));
        let searching = Ctx {
            search_active: true,
        };
        assert!(matches!(
            lookup(REVIEW, searching, None, n),
            Lookup::Act(Action::NextMatch)
        ));
    }

    #[test]
    fn no_table_has_conflicting_bindings() {
        let overlap = |a: Guard, b: Guard| a == Guard::Always || b == Guard::Always || a == b;
        for (name, table) in [
            ("REVIEW", REVIEW),
            ("PICKER", PICKER),
            ("LOG", LOG),
            ("BASE", BASE),
            ("FILE_VIEW", FILE_VIEW),
            ("EXPORT_PREVIEW", EXPORT_PREVIEW),
            ("HELP_VIEW", HELP_VIEW),
        ] {
            for (i, a) in table.iter().enumerate() {
                for b in &table[i + 1..] {
                    assert!(
                        !(a.seq == b.seq && overlap(a.guard, b.guard)),
                        "{name}: {} bound twice",
                        seq_display(a.seq)
                    );
                    // A single binding on a chord's prefix key is unreachable.
                    if a.seq.len() != b.seq.len() {
                        let (short, long) = if a.seq.len() == 1 { (a, b) } else { (b, a) };
                        assert!(
                            !(short.seq[0] == long.seq[0] && overlap(a.guard, b.guard)),
                            "{name}: {} shadowed by chord {}",
                            seq_display(short.seq),
                            seq_display(long.seq)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn labels_derive_from_tables() {
        assert_eq!(keys_label(REVIEW, &[Action::Down, Action::Up]), "j/k");
        assert_eq!(key_for(REVIEW, Action::GotoTop), "gg");
        assert_eq!(key_for(REVIEW, Action::HalfPageDown), "⌃d");
        assert_eq!(key_for(EXPORT_PREVIEW, Action::WriteFile), "w");
    }

    #[test]
    fn help_keeps_accelerators_removed_from_the_normal_bar() {
        let help_has = |action| {
            HELP.iter().any(|(_, items)| {
                items.iter().any(
                    |item| matches!(item, HelpItem::Act(actions, _) if actions.contains(&action)),
                )
            })
        };

        for action in [
            NextHunk,
            PrevHunk,
            NextComment,
            PrevComment,
            NarrowTree,
            WidenTree,
            AutoTreeWidth,
        ] {
            assert!(help_has(action), "{action:?} missing from help");
        }
    }
}
