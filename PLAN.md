# gittre — a lean git review TUI

A terminal UI for **reviewing** git changes: read diffs, fast. It deliberately
does *not* commit, stage, merge, stash, or push. Inspiration:
[gitui](https://github.com/gitui-org/gitui) (always-visible command hints,
keyboard-first) and [hunk](https://github.com/modem-dev/hunk) (review-first
continuous diff stream, file sidebar, watch mode).

Commenting + markdown export is the planned differentiator but is **deferred**
— it needs more design thought. Current thinking is parked in §7 so nothing in
v1 paints us into a corner.

## 1. Product scope

**In scope**
- View diffs for four first-class review scopes, reachable with zero memorized commands:
  1. **Uncommitted** — working tree vs HEAD (unstaged + staged together)
  2. **Staged** — index vs HEAD
  3. **Branch** — current branch vs its base (auto-detected merge-base)
  4. **Commit** — a single commit picked from a log list
- Secondary scope: **commit range** (pick start/end in the log list). Nice-to-have, not v1-critical.
- Continuous multi-file diff browsing (scroll through the whole changeset) *and* a file tree to jump to a specific file.
- Automatic reload when the repo changes (working tree edits, index changes, new commits).
- On-screen command bar at all times (gitui-style) plus a `?` help popup.

**Deferred (planned, needs more design — see §7)**
- Line/range comments with persistence across sessions and export to markdown.

**Out of scope (permanently, by design)**
- Committing, staging/unstaging, amending, merging, rebasing, branching, stashing, pushing/pulling.
- Editing files.

## 2. UX design

### Launch & scope selection
Running `gittre` in a repo opens the **scope picker** — a simple menu, no flags needed:

```
┌ gittre ── review what? ─────────────────────────────┐
│ ▸ 1  Uncommitted changes          12 files          │
│   2  Staged changes                3 files          │
│   3  Branch vs main (merge-base)  27 files          │
│   4  A specific commit…                             │
│   5  A commit range…                                │
└─────────────────────────────────────────────────────┘
  1-5/↑↓ select   ⏎ open   q quit
```

- Counts are computed up front so empty scopes are visibly empty.
- Option 3 auto-detects the base: upstream tracking branch if set, else merge-base
  with `main`/`master`/`develop` (first that exists); the detected base is shown
  inline and can be changed with `b`.
- Options 4/5 open the commit log list (subject, author, age); Enter picks a
  commit, and in range mode you press Enter twice (start, then end).
- CLI shortcuts exist for scripting/muscle-memory but are never required:
  `gittre` (picker) · `gittre -u` (uncommitted) · `gittre -s` (staged) ·
  `gittre -b [base]` (branch) · `gittre <sha>` · `gittre <a>..<b>`.

### Main review screen

```
┌ gittre ── branch vs main (27 files, +412 −88) ──────────────┐
│ Files          │  src/app.rs                                 │
│ ▾ src/         │  @@ -10,6 +10,14 @@                         │
│   app.rs   M   │   10  10   use ratatui::prelude::*;         │
│   git.rs   M   │   11     - fn old_thing() {                 │
│   ui/          │       11 + fn new_thing() {                 │
│     tree.rs A  │       12 +     let x = load();              │
│ README.md  M   │  ...                                        │
│                │                                             │
└────────────────┴─────────────────────────────────────────────┘
 ↑↓/jk scroll  ]/[ file  n/p hunk  ⏎ tree/jump
 t tree  s split  x scope  ? help  q back
```

- **Right pane: the diff stream.** All changed files concatenated into one
  scrollable view (hunk-style) — browsing multiple files is just scrolling.
  Sticky header shows the current file. `]`/`[` jump between files,
  `n`/`p` between hunks.
- **Left pane: file tree.** Directory tree of changed files with status letters
  (M/A/D/R), in the same order the stream scrolls through them. Normally
  passive — a "you are here" marker follows the diff. `Tab` activates it as a
  jump menu (selection seeded from the current file): `j/k` select, `Enter`
  opens a file (or toggles a directory), `Esc` cancels; control returns to the
  diff after a jump. `t` hides/shows the pane.
- **Bottom command bar** always lists the actions valid *right now* (gitui's
  signature pattern). It changes with focus/mode; `?` opens a full help popup.
- `s` toggles unified ↔ side-by-side (side-by-side only when the terminal is
  wide enough; auto-fall-back to unified like hunk).
- `x` returns to the scope picker (state for the previous scope is kept).
- Large files render lazily; binary files show a one-line "binary file changed" row.

## 3. Auto-reload

- `notify` (FSEvents on macOS) watches the working tree plus `.git/HEAD`,
  `.git/index`, and `.git/refs/`.
- Events are debounced (~250 ms) and trigger a background re-diff of the current
  scope; the UI swaps in the new diff while preserving scroll position (by
  file + hunk, not raw offset).
- A subtle "reloaded" flash in the title bar; no full-screen disruption.
- Watching applies to live scopes; fixed commit/range scopes only rewatch refs
  (in case a ref is rewritten).

## 4. Architecture

**Crates**
| Concern | Crate |
|---|---|
| TUI framework | `ratatui` + `crossterm` |
| Git access | `git2` (libgit2 — diffs, log, merge-base, status) |
| Intraline word diff | `similar` |
| File watching | `notify` + `notify-debouncer-mini` |
| Syntax highlighting (M4) | `syntect` |
| CLI args | `clap` |

**Module layout**
```
src/
  main.rs          // clap, terminal setup/teardown, panic hook
  app.rs           // App state machine: ScopePicker | LogPicker | Review; event loop
  event.rs         // input events + worker messages merged into one channel
  git/
    scope.rs       // resolve ScopeKey -> (old tree, new tree/workdir), base detection
    diff.rs        // load Diff -> Vec<FileDiff{ hunks, lines }>; runs on worker thread
    log.rs         // commit list for the pickers
  ui/
    review.rs      // diff stream widget (virtualized scrolling)
    tree.rs        // changed-files tree widget
    bar.rs         // contextual command bar
    popups.rs      // help (later: comment editor, export)
  watch.rs         // notify wiring -> debounced ReloadRequested events
```

**Concurrency model** — gitui-style, no async runtime: one worker thread does
git work (diff/log can be slow on big repos) and sends results over a
`crossbeam` channel that the event loop `select!`s alongside input and watcher
events. The UI never blocks; a spinner shows while a diff loads.

**Performance guardrails** — diff lines are rendered virtualized (only the
visible window is styled), syntax highlighting is per-visible-line with a
cache, and files above a size threshold start collapsed with a "press ⏎ to
expand" row.

## 5. Milestones

- **M0 — skeleton.** Cargo project, ratatui event loop, command bar widget,
  clean startup/teardown, `q` quits. CI with fmt/clippy/test.
- **M1 — read one diff well.** Uncommitted scope only: unified diff stream,
  file tree, jump/scroll/hunk navigation, status letters. This is the heart of
  the tool — get the diff rendering right before anything else.
- **M2 — all scopes.** Scope picker, staged, branch-vs-base with auto base
  detection, commit log picker, `x` to switch. CLI shortcuts. Help popup.
- **M3 — auto-reload.** Watcher, debounce, background re-diff, scroll
  preservation. ✅
- **M4 — polish**, in priority order:
  1. ✅ **`/` search** across the diff stream. Smart-case; matches highlighted;
     while a search is live `n`/`N` walk matches and `Esc` clears it
     (restoring `n`/`p` to hunk nav).
  2. ✅ **Mouse**: wheel scrolls the pane under the pointer; click a tree row to
     jump; click to choose in pickers. (Capture disables native terminal
     selection — built-in copy below compensates; shift+drag bypasses.)
  3. ✅ **Cursor + selection + copy**: visible line cursor; `v` select, `y`
     copies clean code (new side, no signs), `Y` copies patch-style with
     signs; clipboard via `arboard`. The cursor/selection machinery is shared
     groundwork for M5 commenting.
  4. ✅ **Full-file view**, both flavors: `o` opens an internal read-only pager
     at the current line (reads the git blob for historical scopes, disk for
     worktree scopes; `q`/`Esc` back); `E` suspends the TUI and opens
     `$EDITOR` at file:line (disk content — exact for worktree scopes, best
     effort for historical ones).
  5. ✅ **Commit ranges**: Space in the log picker marks a base, Enter picks the
     tip → review `base..tip`; CLI `gittre a..b`.
  6. ✅ **Syntax highlighting** (`syntect`, per-visible-line, cached,
     independent per-line like delta; add/remove stays as bg tint).

  Deferred to a later phase: **side-by-side view** (`s` toggle, auto-fallback
  when narrow).
- **M5 — commenting** (design TBD, see §7). Line/range comments, persistence,
  markdown export. Builds on the M4 cursor/selection.

Each milestone is shippable; M1–M3 is already a useful daily tool.

## 6. Open questions / accepted defaults

- **Base detection order** (upstream → main → master → develop) is a heuristic;
  `-b <ref>` and the `b` key are the escape hatch.
- **Unified diff is the default view**; side-by-side is a toggle, not the default,
  because unified survives narrow terminals (and will simplify comment anchoring
  later).
- **No pager/difftool integration in v1** (hunk does this); revisit later.
- Keys `c`, `v`, `e`, `d` are **reserved unbound** in the review screen so
  commenting can claim them later without breaking habits.

## 7. Deferred: commenting (parked design, needs more thought)

Nothing here is committed — this is the earlier sketch, kept so v1 decisions
stay compatible with it.

- **UX sketch:** `c` on a line opens a text popup; `v` selects a line range,
  then `c` comments on it. Comments render inline under the anchored lines;
  counts badge the file tree. `e` exports to markdown grouped by file, each
  comment with a fenced snippet of its anchored lines.
- **Model sketch:** comment = scope key (commit sha / range / working-tree /
  staged / branch-base) + path + side (old/new) + line range + captured code
  snippet + body + timestamp.
- **Storage idea:** JSON at `.git/gittre/comments.json` — inside `.git/` so it
  never touches the working tree and needs no gitignore entry.
- **Hard problem to think through:** re-anchoring comments when a live diff
  changes underneath (exact-line match, then snippet search, then an "orphaned"
  bucket rather than silent loss). This is where the subtle bugs live and the
  main reason the feature is deferred.
- **Open questions:** per-scope vs. cross-scope comments; single reviewer vs.
  shareable/tracked comment files; whether export should also be a headless
  `gittre export` subcommand; comment threads/replies or flat notes.

**V1 compatibility requirements** (what the deferred design asks of v1):
diff rendering keeps stable per-file/per-line identities (needed for anchoring),
popups are a generic widget, and the reserved keys above stay free.
