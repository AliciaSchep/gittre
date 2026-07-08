# Agent guide

gittre is a review-only git TUI (Rust). Read [README.md](README.md) for what
it does; read [PLAN.md](PLAN.md) for design decisions, their rationale, and
milestone status — **keep PLAN.md current when you change direction**.

## Hard constraints

- **Review-only, forever.** No committing, staging, merging, stashing,
  pushing, or file editing (the `$EDITOR` handoff is the sanctioned escape
  hatch). Reject features that mutate the repo.
- **The command bar teaches the UI.** Any new mode or keybinding must show up
  in the contextual hints (`App::hints`) and the `?` help popup
  (`src/ui/popups.rs`), or it doesn't exist.
- **No emoji in the UI.** Cell width is unreliable across terminals (and
  crashes pyte). Use single-width glyphs: ✎ ▐ ⚠ ● ▏.

## Map

```
src/main.rs        clap CLI (flags, a..b ranges, export subcommand), terminal
                   setup, mouse-capture panic hook
src/app.rs         the App state machine: Screen{Picker,Log,Base,Review} +
                   input modes (search/export/comment-draft/confirm) + all
                   key routing. Biggest file; most changes end up here.
src/event.rs       AppEvent channel + background diff-loader thread
src/watch.rs       notify watcher -> debounced RepoChanged (worktree-aware,
                   git-ignore filtered)
src/comments.rs    comment model, JSON store (<gitdir>/gittre/), re-anchoring
                   cascade (exact -> moved -> outdated), markdown export
src/git/cli.rs     worktree diffs via the git CLI (unified-diff parser,
                   status parsing, untracked synthesis) — see invariant below
src/git/scope.rs   Scope enum, libgit2 diff building per scope, base
                   detection, full-file content per scope
src/git/diff.rs    git2 diff -> DiffResult (tree-order sorted; tested here)
src/git/log.rs     commit list for the picker
src/ui/review.rs   the diff Stream: rows, cursor, selection, search, comment
                   blocks, syntax cache. Second-biggest; owns all position
                   logic.
src/ui/tree.rs     changed-files sidebar (jump-menu model, passive marker)
src/ui/{picker,fileview,bar,popups,highlight}.rs
```

## Load-bearing invariants

- **Everything positional keys off `Stream.cursor`** (not scroll): `c`, `o`,
  `E`, `d`, tree sync, sticky title. If you add a position-dependent feature,
  read the cursor.
- **Stream rows are rebuilt wholesale** (reload, comment change); position is
  preserved via `Stream::anchor()/restore()` (file path + offset + screen
  offset). Never hold row indices across a rebuild.
- **Tree order == stream order** (`tree_order` in git/diff.rs, tested). The
  sidebar and the stream must always traverse files identically.
- **Comments anchor to content, not positions.** The snippet is the source of
  truth for re-anchoring; "outdated" is a display state, never data loss.
  `CommentStore` persists on every mutation (atomic tmp+rename).
- **git2 handles don't cross threads**: the loader thread opens its own
  `Repository`. A `seq` counter drops stale loader responses.
- **The UI thread never computes a diff.** All git-expensive work (scope
  loads, reloads, picker counts) goes through the loader thread; the picker
  renders instantly with "…" counts that fill in. Keep it that way — a
  synchronous diff on the UI thread is exactly what made large repos hang.
- **Worktree scopes (uncommitted/staged) shell out to `git`** and parse its
  output (src/git/cli.rs); libgit2 lacks fsmonitor/untracked-cache/parallelism
  and measured 20x slower on a large repo. Object-database scopes
  (commit/branch/range) stay on libgit2, where it is fast. The libgit2
  worktree path remains only as a fallback (no git binary, unborn HEAD).
  This also means users can fix slow repos with
  `git config core.fsmonitor true` + `core.untrackedCache true` — gittre
  inherits it.
- Rename detection is skipped above `RENAME_DETECTION_LIMIT` changed files;
  `--no-watch` + `r` is the escape hatch for repos where reloads are costly
  (a one-time hint suggests it when a reload exceeds 1s).
- Files over `MAX_CONTENT_FILE_SIZE` (1MB) load as **stubs** (GitHub-style):
  no content up front, `Enter` on the stub loads that one file's diff via the
  loader thread and splices it in. Diff-load phase timings go to GITTRE_LOG.
- **Worktrees:** never assume `<workdir>/.git` is a directory. Use
  `repo.path()` (per-worktree gitdir) and `repo.commondir()` (shared refs) —
  the watcher and comment store depend on this.

## Verify at the surface, not just with tests

`cargo test` covers the git/comments layers, but every feature here was
verified by driving the real TUI in a pty and reading screen snapshots:

```sh
uv run --with pyte dev/tui_drive.py scenario.json
```

Scenario format is documented in the script's docstring: keystrokes
(`<ESC>`, `<C-c>` tokens), atomic `send_raw` for SGR mouse escapes, `shell`
steps to mutate the repo mid-run (this is how auto-reload and comment
re-anchoring were tested), resizes, `env`, and `rawlog` for asserting
truecolor bytes. Build throwaway repos in a temp dir for scenarios; snapshot
before/after each interaction and read the dumps.

pyte limits to know: no alternate-screen buffer (the `$EDITOR` suspend/resume
flow leaves ghost rows in dumps — not a real bug), no styling in text dumps,
chokes on some emoji.

Debugging the live TUI: set `GITTRE_LOG=/tmp/gittre.log` — event traffic
(reloads, loads with timings) appends there, since the TUI owns the terminal.

A lesson the harness already encodes: kernel pty buffers are ~16KB, so the
driver drains the pty on a dedicated thread. If a TUI under test ever seems
to freeze for seconds per frame, suspect the *harness* not draining before
suspecting the app.

Before committing: `cargo fmt && cargo clippy --all-targets && cargo test`
— all three have stayed clean at every commit.

## Ecosystem gotchas (cost real time)

- **git2 0.21** returns `Result` where older versions returned values:
  `commit.summary()` is `Result<Option<&str>>`, `reference.shorthand()` /
  `signature.name()` are `Result<&str>`.
- **ratatui 0.30**: use the `ratatui::crossterm` re-export everywhere;
  `crossterm` is not a direct dependency.
- **Raw mode swallows Ctrl-C** — it's handled as a key event in
  `App::on_key`; don't remove it.
- **syntect startup is slow in debug builds** (~1s blank first frame);
  release is 0.02s. Don't chase it as a perf bug.
- Watch for **key collisions with modifier guards**: match arms like
  `Char('d')` must exclude CONTROL or they shadow Ctrl-d paging (clippy's
  unreachable-pattern warning catches this — heed it).

## Releasing

Releases are handled by [cargo-dist](https://opensource.axo.dev/cargo-dist/)
(config in `dist-workspace.toml`; generated workflow in
`.github/workflows/release.yml` — regenerate with `dist generate` after
config changes, never hand-edit it). Targets: macOS arm64 + x86_64, plus a
shell installer. To cut a release:

```sh
# 1. bump version in Cargo.toml, commit
# 2. tag and push — the tag triggers the release workflow
git tag v0.2.0
git push && git push --tags
```

CI builds the artifacts and attaches them to a GitHub Release. Verify config
changes locally with `dist plan` (what would ship) and `dist build` (real
host-arch artifacts in `target/distrib/`).

## Deferred by explicit decision (don't re-litigate, ask first)

Side-by-side view; comment resolve/done state (both in PLAN.md with context);
macOS code signing/notarization (unsigned is fine while installs go through
the shell installer or cargo — revisit only if browser downloads for
non-technical users become a real path).
