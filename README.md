# gittre

A lean terminal UI for **reviewing** git changes: read diffs, leave comments,
export them to markdown. It deliberately does *not* commit, stage, merge, or
push — it's a review tool, nothing else.

Inspired by [gitui](https://github.com/gitui-org/gitui) (the always-visible
command bar) and [hunk](https://github.com/modem-dev/hunk) (the continuous
multi-file diff stream).

## Install

Prebuilt macOS binaries are attached to
[GitHub Releases](https://github.com/AliciaSchep/gittre/releases), along with
a shell installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AliciaSchep/gittre/releases/latest/download/gittre-installer.sh | sh
```

Or build from source:

```sh
cargo install --path .
```

## Use

Run `gittre` inside a repository and pick what to review — no flags or
keystrokes to memorize; the bar at the bottom always shows the keys that work
right now:

```
╭ review what? ────────────────────────────────────╮
│ 1  Uncommitted changes                 2 files   │
│ 2  Staged changes                       1 file   │
│ 3  Branch vs fork point                2 files   │
│ 4  Branch vs a base you pick…                    │
│ 5  A specific commit…                            │
╰──────────────────────────────────────────────────╯
```

The review screen is one scrollable stream of every changed file, with a file
tree alongside (`Tab` to jump via the tree, `t` to hide it). The view
**reloads automatically** when files change, you commit, or switch branches —
your reading position survives. In very large repositories, `--no-watch`
disables that and `r` reloads on demand.

The branch scope reviews everything the branch introduced: it diffs from the
commit right before the branch's first own commit (its fork point), found
structurally — pushing the branch or the trunk moving on never shrinks it.

CLI shortcuts: `gittre -u` (uncommitted) · `-s` (staged) · `-b` (branch vs
fork point) · `-b <base>` (vs merge-base) · `gittre <rev>` · `gittre a..b` ·
`-C <path>`.

### Reading

The keys are helix-friendly. `j`/`k` move the cursor (`gg`/`ge` top/bottom,
`⌃d`/`⌃u` half page), `]`/`[` next/prev file, `n`/`p` next/prev hunk,
`/` search (`n`/`N` between matches), `v` or `x` select + `y` copy code /
`Y` copy patch, `o` view the full file, `E` open it in `$EDITOR` at the
current line. Mouse works: wheel scrolls, click jumps. `?` shows everything
else.

### Commenting

`c` comments on the cursor line (or the selection); comments render inline in
the diff. They're anchored to the code *content*, GitHub-style: edit the file
and they follow the lines they were written on; if the code disappears they
turn **outdated** — kept, flagged, and shown with the original snippet rather
than lost. Saved comments wrap and reflow with the diff pane. The comment
editor follows the selected line, grows and scrolls as needed, and supports
normal caret navigation; Enter saves and Alt+Enter adds a line. On a comment,
`c` edits and `d` deletes (`u` undoes the last delete). `e` previews all
comments as markdown before you copy or write them, or export headlessly:

```sh
gittre export            # markdown to stdout
gittre export -o rev.md  # or to a file
```

Comments live in `.git/gittre/comments.json` — never in your working tree.

## Notes

- Syntax highlighting needs a truecolor terminal (`COLORTERM=truecolor`);
  otherwise plain diff colors are used.
- Design decisions and roadmap live in [PLAN.md](PLAN.md).
- `dev/tui_drive.py` drives the TUI headlessly for testing
  (`uv run --with pyte dev/tui_drive.py <scenario.json>`).
