# gittre — a lean git review TUI

A terminal UI for **reviewing** git changes: read diffs, fast. It deliberately
does *not* commit, stage, merge, stash, or push. Inspiration:
[gitui](https://github.com/gitui-org/gitui) (always-visible command hints,
keyboard-first) and [hunk](https://github.com/modem-dev/hunk) (review-first
continuous diff stream and file sidebar).

Commenting + markdown export is the product differentiator: comments persist
outside the worktree, follow code as it moves, render inline, and can be
previewed or exported as markdown.

## 1. Product scope

**In scope**
- View diffs for four first-class review scopes, reachable with zero memorized commands:
  1. **Uncommitted** — working tree vs HEAD (unstaged + staged together)
  2. **Staged** — index vs HEAD
  3. **Branch** — everything the current branch introduced (vs its fork point)
  4. **Commit** — a single commit picked from a log list
- Secondary scope: **commit range** (pick start/end in the log list). Nice-to-have, not v1-critical.
- Continuous multi-file diff browsing (scroll through the whole changeset) *and* a file tree to jump to a specific file.
- Explicit background reload with `r`, preserving the reader's position and
  UI state; a successful `$EDITOR` handoff also requests a reload.
- On-screen command bar at all times (gitui-style) plus a `?` help popup.

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
│   3  Branch vs fork point         27 files          │
│   4  Branch vs a base you pick…                     │
│   5  A specific commit…                             │
└─────────────────────────────────────────────────────┘
  1-5/↑↓ select   ⏎ open   q quit
```

- Counts are computed up front so empty scopes are visibly empty.
- Option 3 reviews against the **fork point**: the commit right before the
  first commit the branch introduced, found by walking HEAD's history with
  every *other* branch hidden (remote copies of the branch itself — same
  name under any remote — don't count; 2026-07 redesign, see §6). Recomputed
  on every load,
  so it survives rebases. When no fork point is definable (only branch in
  the repo, detached HEAD), the entry is hidden and option 4's detail says
  why.
- Option 4 compares against an explicitly picked branch (merge-base
  semantics) — always on the menu (2026-07), not just a fallback.
- Option 5 opens the commit log list (subject, author, age); Enter reviews a
  commit, and Space marks a range start so the next Enter reviews the range.
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
 t tree  s split  q scope  ? help
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
- `q` (or `Esc` at the top layer) returns to the scope picker (state for the
  previous scope is kept). `x` is helix's select-line, not an exit.
- Large files render lazily; binary files show a one-line "binary file changed" row.

### Keybindings (2026-07: helix-first, one table)

- Bindings target helix muscle memory: `x` selects the current line and
  extends line-wise on repeat, `gg`/`ge` jump to top/bottom as two-key
  chords, `⌃d`/`⌃u` half-page, `⌃f`/`⌃b` full page. (`v` select and `/` +
  `n`/`N` search already matched helix.) Originally `x` exited to the scope
  picker — the single most hostile binding for a helix user — so exit is
  `q`/`Esc` only.
- Every binding lives in `src/keymap.rs`: per-context tables of key sequence
  → action, with a guard for overloads (`n` is next-match while a search is
  live, next-hunk otherwise). Dispatch, the command-bar hints, and the `?`
  help popup all derive their key labels from the tables, so a rebind is a
  one-line change. A future config file would parse into these same tables.
- While a chord prefix is held, the command bar lists its completions
  (`gg top · ge bottom`); any other key cancels the chord.

## 3. Reload model

- Reviews are snapshots. `r` explicitly requests a background re-diff of the
  current scope; returning successfully from the sanctioned `$EDITOR` handoff
  requests the same refresh.
- The UI swaps in the new diff while preserving cursor/viewport position (by
  file + offset), tree folds, search, focus, and comment anchors.
- A subtle "reloading" / "reloaded" status appears in the title bar; the UI
  remains responsive while git work runs.
- Filesystem watching was removed in 2026-07. For this primarily personal tool,
  its debounce/filter/storm policy and `.git/index` fsmonitor feedback loop were
  not justified—especially because the main large-repo use case disabled it.

## 4. Architecture

**Crates**
| Concern | Crate |
|---|---|
| TUI framework | `ratatui` + `crossterm` |
| Git access | `git2` (libgit2 — diffs, log, merge-base, status) |
| Intraline word diff | `similar` |
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
```

**Concurrency model** — gitui-style, no async runtime: one worker thread does
git work (diff/log can be slow on big repos) and sends results over a standard
channel that the UI loop drains alongside terminal input. The UI never blocks;
a spinner shows while a diff loads.

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
- **M3 — reload infrastructure.** Background re-diff with stale-response
  rejection and UI-state preservation. Originally included filesystem
  watching; watching was removed in 2026-07, leaving explicit `r` and the
  `$EDITOR` return refresh. ✅
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

  7. ✅ **Persistent cursor**: promote the
     select-mode cursor to a permanent `cursor` field on `Stream`, rendered
     every frame. `j`/`k`, paging, and `g`/`G` move the cursor vim-style (view
     follows via the existing `move_cursor` clamp/keep-in-view logic); jumps
     (`]`/`[`, `n`/`p`, search, tree Enter) set it. Position-dependent reads
     (`current_file`, `current_position`, `comment_at_top`, `line_target`)
     switch from `self.scroll` to the cursor row, so `c`/`o`/`E`/`d` act on
     the highlighted line instead of the top of the screen. Select mode
     collapses to "`v` sets anchor = cursor". Follow-ons: click in the stream
     sets the cursor; reload anchor/restore and tree sync track the cursor
     row. No data-model or storage changes.

  Deferred to a later phase: **side-by-side view** (`s` toggle, auto-fallback
  when narrow).
- **M5 — commenting** (design settled, see §7). Sub-milestones:
  1. ✅ Model + storage + add/edit/delete + inline rendering + counts.
  2. ✅ Re-anchoring engine + outdated handling (GitHub-style).
  3. ✅ Markdown export (`e` popup + headless `gittre export [-o PATH]`).
  4. Polish: `}`/`{` comment navigation ✅; resolve state deferred.
- **M6 — review UX polish.** ✅ Anchored, wrapping, scrollable comment editor
  with caret navigation and paste support; saved inline comments reflow with
  the diff pane; content-aware file tree that grows on wide terminals; exact
  markdown preview with scrolling, clipboard copy, and an explicit write step.
  Drag-resizing the tree remains deferred, but pane sizing distinguishes
  automatic from a future manual width.

Each milestone is shippable; M1–M3 is already a useful daily tool.

## 6. Open questions / accepted defaults

- **Branch scope = fork point, not upstream merge-base** (2026-07). The old
  detection preferred the upstream tracking branch, so a pushed branch
  (upstream = `origin/<same-branch>`) reviewed only *unpushed* commits —
  usually nothing. The fork point (parent of the first commit only this
  branch has, ignoring the branch's own remote copies) matches "review what
  this branch introduces" regardless of what the trunk is called. Remote
  copies are matched by *name only* — the configured upstream is not
  exempted, because a branch created with `checkout -b feat origin/main`
  tracks origin/main, and exempting it from the hide set made the fork
  point fall back to a stale local main (2026-07 bug fix). Trade-off: a
  branch pushed under a *different* name and tracked gets its remote copy
  hidden, shrinking the review to unpushed commits — rare; use `-b` there.
  Known divergence: after merging the trunk *into* the branch, the merged-in trunk
  changes appear in the diff (merge-base semantics would hide them) — use an
  explicit base for that workflow. Explicit `-b <base>` and the base picker
  keep merge-base semantics; the fork walk is capped at 10k own commits and
  suggests `-b` past that.
- **Unified diff is the default view**; side-by-side is a toggle, not the default,
  because unified survives narrow terminals (and will simplify comment anchoring
  later).
- **No pager/difftool integration in v1** (hunk does this); revisit later.
- Comment editing keeps Enter as save and Alt+Enter as newline because modified
  Enter keys are not reported consistently across terminal protocols.

## 7. M5 commenting design (settled)

Modeled on how GitHub handles review comments and staleness: a comment owns
its context forever (it stores a snippet of the code it was made on), so a
failed re-anchor degrades to an **"outdated"** display state instead of data
loss.

- **Model:** id, path, side (new/old), line range, captured snippet (the
  selected lines, signs included), body, created-at, scope label (metadata),
  outdated flag.
- **Pool:** one per repo — like a PR review. A comment appears in whichever
  scope its anchor matches (comment on uncommitted work, still see it when
  reviewing the branch after committing). Scope at creation is metadata only.
- **Anchoring:** file line numbers + snippet, never diff-row indices.
  Re-anchor cascade on every (re)load: exact (snippet still at stored lines)
  → moved (snippet found elsewhere in the file; position updated) → outdated
  (collapsed "⚠ n outdated" row at the top of the file's section, expandable,
  showing the preserved snippet).
- **UX:** `v` select → `c` comment (Enter saves, Alt+Enter newline, Esc
  cancels); bare `c` comments the current line. Inline blocks with a colored
  gutter bar; counts in tree + title. `}`/`{` jump between comments; on a
  comment: `c` edits, `d` deletes. `D` deletes **all** comments (confirm
  prompt) to start a review anew; `u` restores whatever the last delete
  removed (single or all). No resolve state for now (revisit later).
- **Storage:** JSON at `<gitdir>/gittre/comments.json` — never touches the
  working tree; linked worktrees get their own review (separate gitdir).
- **Export (M5.3):** markdown grouped by file — line refs, fenced snippet,
  body; outdated flagged. `e` opens an exact preview with copy/write actions;
  also `gittre export`.
