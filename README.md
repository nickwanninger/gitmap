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
- Heatmap view scaffold (`Tab`) — the colorizer is in place; the history walk
  that feeds it is M2
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

Not yet implemented: the history walk and its on-disk cache (M2), the log and
timeline views (M3), hunk-level staging and syntax highlighting (M4), the `gix`
backend, and circle-pack layout.

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
| `j` / `k` | scroll the diff |
| `Tab` | cycle view |
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
  render/          pixel canvas, OKLab palette, map widget, diff pane
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
