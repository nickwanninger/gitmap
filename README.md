# gitmap

A WinDirStat-style TUI git interface. Every file in the repository is a
proportionally-sized block of sub-character pixels, grouped into enclosing
shapes by directory. The map is the primary navigation surface: hover a block
to see its diff, `space` stages, `c` commits.

See [designdoc.md](designdoc.md) for the full design.

## Status

M0 and M1 of the design doc are implemented:

- Squarified treemap layout (ordered variant), laid out from HEAD
- Half-block pixel canvas with OKLab colour ramps and truecolor/256/16 fallback
- Status view: working-tree state across the whole repo, unchanged files kept
  visible so changes read against the mass they sit in
- A tab bar across the top of the side pane showing all three views at once,
  each with a line saying what it shows. Click a tab, press `1`/`2`/`3`, or
  cycle with `Tab`
- Age view: colours each file by how recently it was last committed, on a
  magma ramp with a log-scaled time axis. Backed by a real history walk
  (`git log --name-only`), which also yields per-file churn. Files with no
  history render as a cold neutral rather than borrowing the ramp's dark end,
  so "untracked" never reads as "ancient". The pane reports the hovered file's
  last-touched time and commit count
- History view: a braille commits-per-day strip coloured on GitHub's
  contribution greens, a scrollable commit list, and the selected commit's
  diff. The map colours the files that commit touched, leaving the rest dim but
  visible, so a change set reads as a shape within the codebase. `j`/`k` move
  the selection, `Ctrl+D`/`Ctrl+U` scroll the diff, `y` yanks the SHA
- Per-directory hue: each directory owns an arc of the hue circle sized by its
  share of the repo, and its subdirectories recursively subdivide that arc.
  Files inherit their directory's hue, so a folder reads as one colour family
  and the map is legible as a *map*. Status is carried by chroma instead —
  unchanged files are dim and barely tinted, untracked stronger, unstaged
  stronger again, staged fully saturated. Which *kind* of change a file has
  (added / modified / deleted) lives in the diff pane and status line
- Per-file hue jitter keyed on the path, spreading files within their own
  directory's arc so neighbours stay distinguishable without leaving the family
- Hover with a cell-resolution hit buffer and an 80 ms debounced diff fetch.
  The highlight is graded through the hovered node's ancestors: the block
  itself is brightest, its directory siblings next, then the enclosing
  directory, so the map answers "where am I?" as well as "what is under the
  cursor?"
- File-level staging, directory staging, undo, and an inline commit prompt
- Keyboard navigation as a first-class equal: `n`/`p`, `/`, `j`/`k`
- Panic hook that restores the terminal

Not yet implemented: the history walk's on-disk incremental cache (M2), range
selection and timeline scrubbing in the log view (M3), hunk-level staging and
syntax highlighting (M4), the `gix` backend, and circle-pack layout.

## Build and run

```sh
cargo build --release
./target/release/gitmap [PATH]
```

Options:

```
--scale <linear|sqrt|log>            area transform (default: sqrt)
--split <auto|vertical|horizontal>   pane split (default: auto)
```

## Keys

| Key | Action |
| --- | --- |
| mouse move | hover: highlight + debounced diff |
| click | pin the hovered file |
| `space` | stage / unstage the target |
| `a` | stage everything under the hovered directory |
| `c` | commit prompt (`Ctrl+A` toggles `--amend`) |
| `n` / `p` | next / previous changed file in path order |
| `/` | find a file by name |
| `Tab` / `Shift+Tab` | cycle view: status → heatmap → log |
| `1` / `2` / `3` | jump straight to a view |
| `j` / `k` | scroll the diff, or move the log selection |
| `Ctrl+D` / `Ctrl+U` | scroll the commit diff in the log view |
| `\|` / `-` | force vertical / horizontal split |
| `u` | undo the last staging action |
| `?` | help |
| `q` | quit |

Hold Shift to use the terminal's own text selection, which mouse capture
otherwise takes over.

## Architecture

Threads and channels, no async — see the design doc's rationale. The main
thread owns all state and performs no blocking I/O; a single git worker thread
serialises subprocess calls so background refreshes never contend for
`index.lock`.

```
src/
  main.rs          arg parsing, terminal setup/teardown, panic hook
  app.rs           AppState, event loop, message handling
  worker.rs        the git worker thread
  git/             GitBackend trait, porcelain impl, -z/porcelain=v2 parsers
  layout/          path list → tree, squarified treemap
  render/          pixel canvas, OKLab palette, map widget, diff pane, timeline
  input/           hit buffer
```

Reads and writes both go through porcelain for now. The design doc's plan is to
swap the history walk to `gix` once there is something to measure, keeping
porcelain as the oracle in tests.

## Tests

```sh
cargo test
```

Unit tests cover the two places the design doc calls out as worth real
investment — the porcelain parsers (fixtures with renames, unmerged entries,
submodules, and paths containing spaces and newlines) and the treemap
(non-empty rectangles, no overlap, containment, proportional area, ordering
stability). `tests/smoke.rs` builds a real fixture repository and drives the
whole pipeline into a `TestBackend`.

To eyeball the layout:

```sh
cargo test --test smoke visual -- --ignored --nocapture
```
