# gitmap — a WinDirStat-style TUI git interface

2026-09-21 · @Nick Wanninger

## Goals

A single-binary terminal program that shows a git repository as a spatial map. Every file is a proportionally-sized block of sub-character "pixels", grouped into enclosing shapes by directory. The map is the primary navigation surface: move the mouse over it, a side pane shows the diff under the cursor, space stages, `c` commits.

Three things have to be true at once, and they pull against each other:

1. **The map is always visible.** It is not a mode you enter. Staging, diffing and committing all happen with the map on screen, so you keep spatial memory of where you are in the tree.
2. **Hover is instant.** Sub-frame feedback on the highlight, and a diff that lands fast enough that sweeping the mouse across a directory feels like flipping through the changes rather than issuing queries.
3. **One canvas, three meanings.** Working-tree status, file-age heatmap, and "what did this commit touch" all render through a single widget with a swappable colour function. Build the canvas once.

### Non-goals for v1

- Not a general git client. No rebase UI, no merge conflict resolution, no branch management beyond showing the current branch. `lazygit` and `tig` exist and are good; this complements them.
- No hunk-level or line-level staging. File-level only. The design leaves room for it — see the staging section.
- No remote operations. No fetch, push, pull. The map is about local state.
- Not a file manager. Clicking opens a diff, not an editor buffer.

### The core loop

```
┌─ map ─────────────────┬─ diff ──────────────────┐
│  ▀▀▀ ▄▄  ▀▀▀▀▀        │  src/render/canvas.rs   │
│  ██▀ ██  ████         │                         │
│      ▀▀  ▀▀           │  @@ -42,7 +42,9 @@      │
│   ▄▄▄▄▄▄▄             │  -    let w = area.w;   │
│   ███████   ← cursor  │  +    let w = area.w    │
│   ▀▀▀▀▀▀▀             │  +        .saturating…  │
├───────────────────────┴─────────────────────────┤
│ main  ✓3 ~7 +1    ⏎ open   space stage   c commit│
└─────────────────────────────────────────────────┘
```

Mouse moves, the highlight redraws immediately from a cached hit-test buffer, and a debounced request fetches the diff. Space toggles staged/unstaged for the file under the cursor and the block's colour changes in place. `c` opens a commit prompt over the map.

Split direction is chosen from the terminal's aspect ratio: vertical (map left, diff right) when columns ≥ 2.2 × rows, horizontal otherwise. Diffs want width, maps want square. Manual override on `|` and `-`.

## Language and TUI library

**Recommendation: Rust with ratatui + crossterm, git access via `gix` for reads and porcelain for writes.**

The deciding factor is not raw speed — every candidate here is fast enough to push 20k cells at 120 Hz. It's that this project needs four things that rarely come bundled: sub-cell colour control, reliable mouse-motion events across terminals, a fast git library, and a diff/syntax-highlight stack you don't have to write yourself. Rust is the only ecosystem where all four are mature today.

### The candidates

**Rust — ratatui + crossterm.** Ratatui is an immediate-mode library over a double-buffered cell grid; it diffs frames and emits only changed cells, which is exactly the right model for a canvas that mostly doesn't change. You get direct `Buffer` access, so the pixel canvas is a custom widget writing `▀` with independent fg/bg per cell — no fighting a widget abstraction. Crossterm handles mouse capture including motion, SGR extended coordinates, and bracketed paste, on Unix and Windows. Around it: `gix` (gitoxide) is a pure-Rust git implementation that is genuinely faster than libgit2 for history walks, `similar` gives you Myers/patience diffs, `syntect` or `tree-sitter` gives syntax highlighting, `notify` watches the worktree. This is a solved-parts project in Rust and a build-it-yourself project everywhere else.

**Zig — libvaxis.** You asked about Zig specifically, so: libvaxis is the real answer there and it is a better library than ratatui in a few respects. It's grapheme-aware by default via `zg` (correct width for emoji and combining marks, which ratatui gets wrong without help), it supports the Kitty keyboard protocol so you can distinguish `Ctrl+I` from Tab and get key-release events, and it has built-in Kitty graphics and Sixel support — which matters here, because it gives you a free path to a true-pixel rendering mode. Rendering is damage-tracked like ratatui's.

The cost is everything around it. Zig has no stable language or stdlib; the 0.13 → 0.14 → 0.15 transitions each broke build systems, `async`, and I/O interfaces, and libvaxis follows Zig's tip, so you will spend real time on churn that has nothing to do with your program. There is no native git library, so you're on `libgit2` through `@cImport` or shelling out. There is no mature diff library, no syntax highlighter (you'd bind tree-sitter's C API, which is at least pleasant from Zig). Practically: expect to write 2–3× more supporting code and re-fix your build a few times a year.

The honest recommendation is to build it in Rust. If you want to write Zig, the version of this that makes sense is a **narrower** tool — accept shelling out to porcelain for everything, skip syntax highlighting in v1, and use libvaxis's Kitty graphics support as the differentiating feature instead of half-blocks. That's a good project. It is not a faster path to the tool described in this doc.

**Go — Bubble Tea + Lip Gloss.** The most productive option by a margin, and the easiest to distribute. The problem is architectural: Bubble Tea's Elm loop has your `View()` return a fully-rendered string each frame, which is re-diffed downstream. For a dense canvas that means allocating and scanning a large string per frame, and styling is applied by embedding ANSI in that string rather than by writing typed cells. It works — people have built heavy TUIs on it — but you'd be fighting the grain for the one part of this app that's performance-sensitive. `go-git` is also noticeably slower than `gix` on large-history operations, which is what the heatmap needs.

**C++ — FTXUI.** Competent, good component model, header-only-ish. But you'd be back to manual dependency management for libgit2, a diff library and a highlighter, with no cargo to hold it together. Choose this only if you already have a C++ toolchain you love.

### Summary

|  | Canvas control | Mouse motion | Git library | Ecosystem risk |
| --- | --- | --- | --- | --- |
| Rust / ratatui | Direct cell buffer | crossterm, solid | `gix`, excellent | Low |
| Zig / libvaxis | Direct, grapheme-aware, + Kitty graphics | Excellent, Kitty protocol | None native | High — language churn |
| Go / Bubble Tea | Via rendered strings | Good | `go-git`, slow | Low |
| C++ / FTXUI | Direct | Adequate | libgit2 via FFI | Medium — manual deps |

## The pixel canvas

### Half blocks are the right default

Write `▀` (U+2580, UPPER HALF BLOCK) into every cell with the top pixel's colour as foreground and the bottom pixel's as background. One cell becomes two independently-coloured square-ish pixels stacked vertically, which also corrects the aspect ratio — terminal cells are roughly 1:2, so two half-cells are close to square. A 200×55 terminal gives you a 200×110 pixel canvas: about 22,000 pixels to spend on the repo.

The canvas API is a plain framebuffer, decoupled from the terminal:

```
struct Canvas { w: u16, h: u16, px: Vec<Rgb> }   // h is pixel rows = 2 × cell rows
impl Canvas {
    fn set(&mut self, x: u16, y: u16, c: Rgb);
    fn fill_rect(&mut self, r: PixelRect, c: Rgb);
    fn blit(&self, buf: &mut ratatui::Buffer, area: Rect);  // pairs rows into ▀
}
```

Only `blit` knows about terminals. That keeps the layout and colour logic testable without a PTY, and lets you add a second backend later.

### Why not the denser options

- **Quadrant blocks** (`▘▝▀▖▌▞▛` …) give 2×2 = four subpixels, but a cell still has only two colours. Usable only when each 2×2 group is two-toned, which a multi-colour file map is not. Skip.
- **Braille** (U+2800–28FF) gives 2×4 = eight subpixels at one foreground colour per cell. Excellent for monochrome density plots — worth keeping in mind for the commit timeline sparkline — useless for the map, where colour *is* the information.
- **Sextants** (U+1FB00, Symbols for Legacy Computing) give 2×3 with the same two-colour limit as quadrants, plus patchy font coverage. Skip.
- **Kitty graphics / Sixel** give true pixels with no colour constraint. This is a legitimate high-fidelity mode, not a replacement: it works in Kitty, WezTerm, Ghostty and foot, and degrades to nothing elsewhere. Design the canvas so `blit` has a second implementation that emits a PNG/RGB payload, and let the user opt in. `ratatui-image` handles protocol detection if you go this way; libvaxis has it natively.

Note on cost: half-block rendering means a full-canvas redraw emits an SGR sequence per cell in the worst case. Ratatui's frame diff means you only pay that when colours actually change, so hover highlights (a few dozen cells) are cheap and a view switch (everything) is the expensive case at roughly 20k cells × \~20 bytes ≈ 400 KB. That's a few milliseconds locally and visible latency over a slow SSH link — worth measuring early.

### Colour

Detect truecolor from `COLORTERM` (`truecolor` or `24bit`) and the terminfo `RGB`/`Tc` capability; fall back to a 256-colour quantisation of the same ramps, and to 16 colours as a last resort with the map degrading to a 4-state status palette.

Ramps matter more than usual here because the heatmap encodes a continuous variable. Use a perceptually uniform sequential ramp — viridis or magma, sampled in OKLab — rather than an HSV sweep, which produces false banding at yellow and cyan. Keep a small `palette.rs` with named ramps so themes are data, not code.

Status colours should be categorical and distinguishable under the common colour-vision deficiencies: avoid red/green as the only distinction between modified and added. A workable set is added = blue-cyan, modified = amber, deleted = magenta-red, untracked = dim grey-green, staged = the same hue at full saturation with unstaged desaturated to \~40%. That last trick means staging is visible as a *brightness* change on the same hue, which reads well at one-pixel sizes.

## Layout

### Treemap, not circle packing

The reference image is a circle pack, and it's beautiful, but it's the wrong algorithm for a terminal. Circle packing wastes 30–40% of its area on gaps, and that waste compounds with nesting depth — a file three directories deep in a circle pack gets maybe half the pixels it would in a treemap. At 22,000 total pixels you cannot afford that; small files collapse to a single pixel and stop being hoverable targets.

Use a **squarified treemap** (Bruls, Huizing & van Wijk 2000) as the primary layout. It fills the space completely and keeps rectangles close to 1:1, which is what makes them easy to hit with a mouse and easy to tell apart. The algorithm is about 80 lines: sort children, greedily add to the current row while the worst aspect ratio improves, lay the row out along the shorter side, recurse into the remaining rectangle.

Keep circle packing as an alternate `--layout=pack` mode. It's genuinely better for the "orient me in an unfamiliar codebase" case, where directory identity matters more than per-file precision, and it's the right layout if you add a Kitty-graphics high-fidelity mode where pixels are cheap.

### Sizing

Raw byte size makes one vendored blob swallow the map. Use **lines of code** as the default metric with a **square-root** transform:

```
area(file) = sqrt(max(loc, 1))
```

Square root compresses the dynamic range enough that a 5,000-line file is \~30× a 5-line file rather than 1,000×, while still reading as "bigger". Offer `--size=bytes|loc|churn` and `--scale=linear|sqrt|log`; `churn` (lines changed over the last N commits) makes a striking alternate map but needs the same history walk as the heatmap.

Enforce a minimum of 2×2 pixels per file so nothing becomes unhoverable, and when a directory can't fit its children at that minimum, collapse it into a single aggregate block labelled with the child count. Clicking a collapsed block zooms into it — which gives you drill-down navigation for free.

### Stability is a real problem

Squarified treemaps are notoriously unstable: change one file's size and the whole subtree reflows. In an interactive tool that means your hover target moves out from under the cursor while you're looking at it, which is genuinely maddening.

Three mitigations, applied together:

1. **Sort by path, not by size.** The classic squarify sorts descending by area for optimal aspect ratios. Sorting by path instead costs you some squareness and buys you an ordering that only changes when files are added or removed — see Shneiderman & Wattenberg's ordered treemaps for the formal version.
2. **Compute layout from HEAD, not from the working tree.** The geometry is a function of the committed tree; only *colour* is a function of working-tree status. Staging a file recolours it and moves nothing.
3. **Cache and invalidate explicitly.** Recompute only on: terminal resize, HEAD change, file add/remove, or explicit refresh. Not on content edits.

### Labels

Directory names go in the gutter between nested rectangles when there's room (≥ 1 cell of padding and enough width for 3+ characters), otherwise they're dropped and shown only in the status line on hover. File names never render in the map — there isn't room, and the diff pane header plus a hover tooltip covers it. The reference image's leader lines are attractive but the placement problem is harder than it looks; defer it.

One cheap win: reserve one cell row at the top of each directory rectangle for its label, drawn with the directory's own dim tint as background. That gives visual grouping without drawing borders, which would eat pixels.

## Interaction

### Mouse capture

Enable mouse reporting mode 1003 (any-event tracking, which reports motion with no button held — this is what makes hover work; 1002 only reports motion during a drag) plus 1006 (SGR extended coordinates, required above 223 columns; without it a wide terminal silently reports garbage). Crossterm's `EnableMouseCapture` sets both.

Two consequences worth designing around:

- **Mouse capture steals text selection.** Users expect to be able to select and copy from a terminal. Follow the convention: holding Shift bypasses the application and gives the terminal's native selection. Say so in the help. Also provide `y` to yank the hovered path and `Y` to yank the diff, so the common cases don't need selection at all.
- **Motion events flood the input channel**, especially over SSH where they queue up and you end up processing stale positions. Drain the event queue every tick and keep only the last motion event before rendering. This single line removes most of the perceived lag.

### Hit testing

Don't walk the treemap tree per mouse event. Rasterise an ID buffer alongside the pixel buffer during layout:

```
struct HitBuffer { w: u16, h: u16, ids: Vec<Option<FileId>> }   // cell resolution
```

One entry per *cell*, not per pixel — the terminal reports mouse position in cells and there is no sub-cell resolution, so a cell that straddles two files resolves to whichever file owns the top half. Lookup is `ids[y * w + x]`, which is O(1) and nanoseconds. The buffer is rebuilt only when layout is, so it costs nothing per frame.

The highlight itself is drawn as a separate overlay pass: brighten the hovered file's pixels by a fixed OKLab lightness delta and draw a one-cell outline around its bounding box. Never mutate the base canvas, so un-hovering is just a redraw of the previous state.

### Debouncing the diff

Hover feedback and diff fetching run at different speeds. The highlight updates on the same frame as the motion event. The diff request waits \~80 ms of cursor stillness before firing, and any in-flight request for a now-stale path is cancelled (in practice: tag each request with a monotonically increasing sequence number and drop results whose sequence is behind the current hover).

Without this, sweeping across a directory spawns fifty `git diff` invocations and the pane thrashes. With it, a sweep is silent and the diff appears when you settle.

Keep a small LRU of rendered diffs keyed by `(path, index_blob_oid, worktree_mtime)`, so moving back and forth between two files is instant.

### Keybindings

| Key | Action |
| --- | --- |
| Mouse move | Hover: highlight + debounced diff |
| Click | Pin the hovered file (diff stops following the mouse) |
| Double-click | Zoom into the directory / open file in `$EDITOR` |
| `space` | Toggle stage/unstage for the hovered or pinned file |
| `a` | Stage everything under the hovered *directory* |
| `c` | Commit prompt |
| `Enter` | Open in `$EDITOR` at the first hunk |
| `Tab` | Cycle view: status → heatmap → log |
| `j` / `k` | Scroll the diff pane |
| `n` / `p` | Next / previous changed file in path order |
| `/` | Fuzzy-find a file, which moves the hover |
| `y` / `Y` | Yank path / yank diff |
| `u` | Undo last stage operation |
| \` | ` `-\` |
| `?` | Help overlay |
| `q` | Quit |

Keyboard navigation with `n`/`p` and `/` is not an afterthought — it's what makes the tool usable over a connection where mouse motion is too laggy, and what makes it accessible. Every mouse action should have a keyboard equivalent.

## Git backend

### The split

You proposed building on porcelain. That's right for *writes* and wrong for the history walk, so split it:

**Porcelain for anything that mutates or that depends on user configuration.** `git add`, `git restore --staged`, `git commit` — these respect hooks, `.gitattributes`, filters, sparse-checkout, signing config, `core.autocrlf`, aliases, and the user's `pre-commit` setup. Reimplementing that surface is how you produce a tool that quietly corrupts someone's workflow. The cost is a process spawn per operation, \~5–20 ms, which is invisible for a user-initiated action.

**A library for the history walk.** The heatmap needs the last-modified commit time for every file in the repo, and the log view needs commit metadata plus changed paths. Doing that through `git log --name-status` over a large repo means parsing megabytes of text: on a 50k-commit repository that's several seconds and a lot of allocation. `gix` walks the commit graph and diffs trees directly, using the commit-graph file when present, and is roughly an order of magnitude faster. This is the one place where the difference is felt.

### The interface

Put everything behind one trait so both implementations are swappable and testable against a fixture repo:

```rust
trait GitBackend {
    fn status(&self) -> Result<Vec<FileStatus>>;
    fn tree_at_head(&self) -> Result<Vec<TreeEntry>>;     // path, blob size, loc
    fn diff(&self, path: &Path, staged: bool) -> Result<Diff>;
    fn log(&self, range: Range, limit: usize) -> Result<Vec<CommitMeta>>;
    fn paths_in_commit(&self, oid: Oid) -> Result<Vec<PathBuf>>;
    fn last_touched(&self) -> Result<HashMap<PathBuf, Time>>;  // the heatmap walk

    fn stage(&self, path: &Path) -> Result<()>;
    fn unstage(&self, path: &Path) -> Result<()>;
    fn commit(&self, msg: &str, amend: bool) -> Result<Oid>;
}
```

Ship `PorcelainBackend` first — the whole thing works, end to end, with zero library risk — then swap `last_touched`, `log` and `tree_at_head` to `GixBackend` once you can measure the difference. Keep the porcelain implementation forever as a fallback and as the oracle in tests: any divergence between the two is a bug in the fast path.

### Porcelain call reference

Always `-z` for NUL-separated output, never parse the human format, and always `--` before paths.

| Need | Command |
| --- | --- |
| Status | `git status --porcelain=v2 -z --untracked-files=all --no-renames` |
| Tree at HEAD | `git ls-tree -r -z --long HEAD` |
| File diff (unstaged) | `git diff --no-color --no-ext-diff -- <path>` |
| File diff (staged) | `git diff --cached --no-color --no-ext-diff -- <path>` |
| Log | `git log --format=%H%x00%at%x00%an%x00%s%x00 -z -n <N>` |
| Paths in a commit | `git show --name-only --format= -z <oid>` |
| Heatmap walk | `git log --format=%H%x00%at --name-only -z --no-renames` |
| Repo root | `git rev-parse --show-toplevel` |

Use `--porcelain=v2` rather than v1: it reports the index and worktree modes, both blob OIDs, and the stage state explicitly, which you need to key the diff cache. `--no-renames` keeps the parse simple for v1; rename detection changes the map's semantics (does a renamed file keep its old position?) and is a deliberate later decision.

Run every git subprocess with `GIT_OPTIONAL_LOCKS=0` for read operations so a background refresh never takes `index.lock` out from under the user's own shell, and set `GIT_CONFIG_NOSYSTEM=` off — you *want* their config.

## The three views

All three share the treemap geometry. They differ only in the function mapping a file to a colour, and in what occupies the second pane. Formalise that:

```rust
trait Colorizer {
    fn color(&self, f: &FileNode) -> Rgb;
    fn legend(&self) -> Legend;
}
```

Switching views is swapping a `Box<dyn Colorizer>` and redrawing. No relayout, so the map appears to stay still while its meaning changes — which is the whole point, and is why layout was made a function of HEAD rather than of status.

### 1. Status view (default)

Colour by working-tree state: added, modified, deleted, untracked, unchanged. Staged files at full saturation, unstaged at \~40%. Unchanged files stay in a very dim neutral so the codebase's shape is always visible behind the changes — this is what you meant by wanting "some kind of view of the codebase as well", and it matters: changes are only meaningful relative to the mass they sit in.

Second pane: the diff of the hovered or pinned file. Status line shows the branch and the staged/unstaged/untracked counts.

### 2. Heatmap view

Colour by time since the file's last modifying commit, on a magma ramp — recent is bright, ancient is near-black. Use a **log-scaled** time axis (hours → days → weeks → months → years), because linear time makes everything older than a month indistinguishable, and the interesting structure is at the recent end.

This view answers "what part of this codebase is alive?" and it's the one most likely to teach you something about an unfamiliar repo. Two variants worth having behind a toggle:

- **Age** — time since last touched. Shows dead zones.
- **Churn** — commits touching the file in a trailing window. Shows hot spots, which correlate with defect density and are where you look first when something is fragile.

Second pane: a per-file commit list for the hovered file (`git log --oneline -- <path>`), which turns the heatmap into a browsable history.

The data behind this is one full history walk producing `HashMap<PathBuf, (last_time, commit_count)>`. Cache it on disk under `$XDG_CACHE_HOME/gitmap/<repo-hash>.bin` keyed by HEAD OID; on startup, if the cached HEAD is an ancestor of the current HEAD, walk only the commits since and merge. That turns a multi-second cold start into a few milliseconds on every subsequent run.

### 3. Log / timeline view

The second pane becomes a scrollable commit list; the map colours the files each selected commit touched. Moving the selection down the log animates the map, and you see a change set as a *shape* — which is a genuinely different way to read history than a file list. Commits that touch scattered directories look different from commits confined to one module, at a glance.

Layout: the commit list on one side, and above or below it a compact timeline strip drawn with braille (dense, monochrome, and colour isn't carrying information here) showing commits per day over the visible range, with the selected commit marked. Dragging on the strip scrubs the selection.

Selection can extend to a range (shift-click, or `v` then movement), colouring the union of files touched across the range with intensity by touch count. That makes "what did this feature branch actually change" a single glance.

Keys: `j`/`k` move the selection, `Enter` shows the full commit diff, `y` yanks the SHA.

## Architecture and performance

### Threads, not async

This program has maybe four concurrent activities and none of them are network-bound. Async buys you nothing here and costs you a runtime and coloured functions. Use plain threads and channels.

```
  ┌──────────────┐  Event   ┌───────────────┐
  │ input thread │─────────▶│               │
  └──────────────┘          │  main thread  │──▶ terminal
  ┌──────────────┐  Msg     │  state + draw │
  │ git worker   │─────────▶│               │
  └──────▲───────┘          └───────┬───────┘
         │      Request             │
         └──────────────────────────┘
  ┌──────────────┐  FsEvent
  │ fs watcher   │─────────────────▶
  └──────────────┘
```

- **Input thread** blocks on `crossterm::event::read()` and forwards to the main channel. Nothing else.
- **Git worker** is a single thread consuming a request queue, with the ability to drop superseded requests (the hover-diff sequence number). One thread is enough and keeps git operations serialised, which avoids concurrent `index.lock` contention. If the history walk turns out to dominate startup, give it its own second thread so it can't block interactive diffs.
- **FS watcher** (`notify`) coalesces worktree events with a 200 ms debounce and triggers a status refresh. Ignore `.git/` except for `HEAD`, `index`, and `refs/`.
- **Main thread** owns all state, selects on the channel, and redraws.

The main thread never performs I/O that can block. That's the single invariant that keeps the UI responsive; enforce it by making `AppState` hold no handles to anything blocking.

### Redraw policy

Event-driven, not fixed-rate. Block on the channel; on wake, drain everything available, collapse redundant events (keep only the newest mouse position, merge status updates), then draw once. Add a \~8 ms coalescing window so a burst of motion events produces one frame.

This matters for battery and for SSH. A 60 Hz render loop on an idle TUI is rude.

### Budget

Targets on a 100k-file repo, 50k commits, 200×55 terminal:

| Operation | Target | Notes |
| --- | --- | --- |
| Hover highlight | < 2 ms | Hit buffer lookup + overlay redraw |
| View switch | < 16 ms | Recolour 22k pixels, no relayout |
| Treemap layout | < 30 ms | Only on resize / HEAD change |
| Status refresh | < 100 ms | Porcelain spawn dominates |
| Diff fetch | < 50 ms | After debounce |
| Cold heatmap walk | < 3 s | `gix`; cached thereafter |
| Warm start | < 150 ms | Cache hit + status |

### Caches

| Cache | Key | Invalidated by |
| --- | --- | --- |
| Treemap layout | terminal size + HEAD OID + file set | Resize, commit, add/remove |
| Hit buffer | same as layout | Same |
| Diff LRU (≈64) | path + index OID + mtime | Write to file, stage/unstage |
| Heatmap | HEAD OID | New commits (incremental merge) |
| Syntax highlight | path + content hash | Content change |

### Module layout

```
src/
  main.rs          arg parsing, terminal setup/teardown, panic hook
  app.rs           AppState, event loop, message handling
  git/
    mod.rs         GitBackend trait, shared types
    porcelain.rs   subprocess implementation
    gix.rs         library implementation for reads
    parse.rs       porcelain=v2 and -z parsers (heavily unit-tested)
  layout/
    treemap.rs     squarified, ordered variant
    pack.rs        circle packing (alt mode)
    tree.rs        path list → nested FileNode tree
  render/
    canvas.rs      pixel framebuffer + half-block blit
    palette.rs     OKLab ramps, truecolor detection and fallbacks
    map.rs         the map widget, Colorizer implementations
    diff.rs        diff pane, syntax highlighting
    timeline.rs    braille commit sparkline
  input/
    hit.rs         HitBuffer
    keys.rs        keymap, configurable
```

A panic hook that restores the terminal (leave alternate screen, disable raw mode, disable mouse capture) before printing is not optional — a TUI that panics without cleanup leaves the user with a wrecked shell. Wire it up on day one.

## Staging and committing

### v1: file-level

Space on a hovered file runs `git add -- <path>` if unstaged, `git restore --staged -- <path>` if staged. Space on a hovered *directory* label, or `a` anywhere, applies to every changed file beneath it. Deleted files need `git rm --cached` semantics, which `git add -A -- <path>` handles correctly — use `-A` so deletions are staged like modifications.

After any staging operation, refresh status rather than mutating local state optimistically. The refresh is \~50 ms and it guarantees the map reflects what git actually thinks, including cases where a hook or filter changed the outcome. Optimistically recolour immediately so the click feels instant, then reconcile when status returns; if they disagree, the refresh wins silently.

Keep an undo stack of `(path, previous_state)` so `u` reverses the last staging action. This is the operation people fat-finger most, and mouse-driven staging makes fat-fingering easier.

### v2: hunk-level

The natural extension: when the diff pane has focus, space stages the hunk under the cursor. Implementation is `git apply --cached` with a synthesised patch — take the hunk, rebuild a minimal valid unified diff with correct headers, and pipe it in. The awkward parts are line-ending normalisation, the no-newline-at-EOF marker, and recomputing `@@` offsets when staging a hunk out of order. `git apply --cached --recount` handles the last one for you; lean on it.

This is a meaningful chunk of work and it's why it isn't in v1. Design the diff pane's data model with hunk boundaries as first-class now, so adding it later doesn't require reworking the pane.

### Committing

Two paths, both worth having:

**Inline prompt** (`c`) — a modal over the map with a subject line and an optional body, showing the staged file count and a live 50/72-character guide. Submit with `Ctrl+Enter`, cancel with `Esc`. Sends the message via `git commit -F -` on stdin, which avoids all shell-quoting problems. This is the fast path and should be the default.

**Editor handoff** (`C`) — for real commit messages. The sequence matters and is easy to get wrong:

1. Disable mouse capture, leave raw mode, leave the alternate screen.
2. Spawn `$GIT_EDITOR` / `$EDITOR` on a temp file pre-filled like git's own template, inheriting stdin/stdout/stderr, and **wait**.
3. On exit, re-enter the alternate screen, re-enable raw mode and mouse capture, and force a full redraw (the terminal state is unknown after an arbitrary editor).
4. If the message is empty or all-comments, abort the commit as git would.

Running `git commit` without `-m` would do all this itself, but then git owns the terminal during the editor and you lose control of the cleanup; driving it yourself is more predictable.

**Signing, hooks and templates come free** because you're shelling out. `commit.gpgsign`, `user.signingkey`, `commit.template`, `pre-commit` and `commit-msg` hooks all just work. A `pre-commit` hook that fails must surface its stderr in a dismissible overlay rather than being swallowed — that's the single most common way a commit silently doesn't happen.

`Ctrl+A` in the prompt toggles `--amend`, repopulating the message from `HEAD`. Guard it with a warning if `HEAD` is pushed to an upstream branch (`git branch --contains` against the tracking ref).

## Risks and compatibility

### The three that could sink it

**Mouse hover doesn't work everywhere.** This is the biggest product risk, because hover is the central interaction. Failure modes: tmux needs `set -g mouse on` and passes through inconsistently across versions; screen is worse; some terminals report 1002 (drag only) when asked for 1003; over a high-latency SSH link motion events arrive in bursts and hover feels drunk. Mitigation is architectural, not incremental — build the keyboard navigation path (`n`/`p`, `/`, arrow keys moving a cursor cell-by-cell) as a first-class equal, not a fallback. Then a terminal without working hover degrades to a perfectly good keyboard tool. Detect at startup by probing for motion events and print a one-line hint if none arrive.

**Very large repositories.** A 100k-file monorepo has more files than you have pixels. At that point the treemap must aggregate: below a size threshold, a directory renders as one block with a child count, and you drill in. Design for this from the start rather than discovering it on a real repo — the aggregation rule is a few lines in the tree builder, but retrofitting it touches layout, hit testing and every colorizer.

**Terminal font coverage for block characters.** U+2580 is near-universal, so half blocks are safe. Braille (for the timeline) is less so on some Windows fonts, and the Legacy Computing block is genuinely patchy. Stick to half blocks for anything load-bearing and treat braille as decorative with an ASCII fallback.

### Compatibility matrix

| Terminal | Truecolor | Motion (1003) | Kitty graphics | Notes |
| --- | --- | --- | --- | --- |
| Kitty | Yes | Yes | Yes | Best target |
| WezTerm | Yes | Yes | Yes | Best target |
| Ghostty | Yes | Yes | Yes | Best target |
| Alacritty | Yes | Yes | No | Solid |
| foot | Yes | Yes | Sixel | Solid |
| iTerm2 | Yes | Yes | Own protocol | Solid |
| Windows Terminal | Yes | Yes | No | ConPTY mouse quirks; test |
| tmux | Passthrough | Needs `mouse on` | Passthrough only in recent versions | Test explicitly |
| Terminal.app | No (256) | Partial | No | Degraded mode |
| Linux console | No (16) | No | No | Keyboard only |

### Smaller risks

- **Binary and generated files** distort the map. Respect `.gitattributes` `binary`/`linguist-generated`, and offer a config list of glob patterns to dim or exclude. A `node_modules` that survives into the map ruins it.
- **Submodules** appear as single tree entries. Render as one block with a distinct hatch and don't recurse in v1.
- **Symlinks** must not be followed when counting lines.
- **Detached HEAD, mid-rebase, mid-merge** states need explicit handling in the status bar or the tool lies about what committing will do. Read `.git/rebase-merge`, `.git/MERGE_HEAD` and surface the state prominently.
- **Non-UTF-8 paths** exist on Linux. Use `OsString`/byte paths internally and only lossy-convert at render time.

### Open questions

1. **Should renames preserve map position?** Rename detection is off in v1, so a rename shows as a delete plus an add in different places. Turning it on requires deciding whether the block animates or teleports.
2. **Does the map show the whole repo or only changed files?** This doc assumes the whole repo, dimmed, because context is the point — but a "changed files only" mode would give changes far more pixels. Probably a toggle; which is the default is a real UX question.
3. **Should hover follow the mouse into the diff pane**, or does entering the pane pin the file? Pinning on entry is probably right, but it needs trying.
4. **Is per-directory hue worth it?** Giving each top-level directory a distinct hue and encoding status as saturation/lightness within it would make the map far more legible as a *map*, at the cost of status being harder to read. Worth prototyping both.

## Milestones

### M0 — the spike (one day)

Before committing to any of this, write a single file that:

1. Shells out to `git ls-tree -r --long HEAD`, parses paths and sizes.
2. Builds a nested tree and runs squarified treemap over it.
3. Renders it with half blocks, one random colour per top-level directory.
4. Enables mouse capture and prints the file under the cursor in the bottom row.

That's maybe 400 lines and it settles every question that matters: whether the treemap is legible at terminal resolution, whether hover feels good in *your* terminal and over *your* SSH links, and whether half blocks look the way you're imagining. If hover feels bad here, the whole design needs rethinking — better to know on day one.

If you're still torn on the language, write M0 twice — Rust and Zig. It's a day each and you'll have a real opinion instead of a comparison table.

### M1 — usable (1–2 weeks)

Status view with real colours, diff pane with debounced hover fetch, split layout with aspect-ratio selection, space to stage, `c` to commit inline, `q` to quit, panic hook restoring the terminal. Keyboard navigation (`n`/`p`, `/`) working from the start. `PorcelainBackend` only.

At the end of M1 you should be using it for your own commits. That's the bar; everything after this is informed by daily use rather than speculation.

### M2 — the codebase view (1 week)

Heatmap view with the history walk and on-disk cache. Directory labels. Drill-down via double-click with a breadcrumb. Config file for palette, size metric and ignore globs. `--size=churn`.

### M3 — history (1–2 weeks)

Log view, commit list pane, braille timeline strip, range selection, per-commit map colouring. Swap the history walk to `gix` and measure.

### M4 — polish

Hunk-level staging. Syntax highlighting in the diff pane. Circle-pack alternate layout. Kitty graphics high-fidelity mode. Rename detection.

### Testing

Two things are worth real test investment, because they're where correctness bugs hide and where they're expensive:

- **The porcelain parsers.** Fixture-driven tests over recorded `--porcelain=v2 -z` output covering renames, merge conflicts, submodules, non-UTF-8 paths, and files with spaces and newlines in their names. This is the code most likely to be subtly wrong and least likely to announce it.
- **The treemap.** Property tests: every file gets a non-empty rectangle, rectangles never overlap, the union covers the parent, and the area ratio between any two files is within tolerance of their size ratio. Plus a stability test — perturb one file's size and assert that no other file's rectangle moves.

For everything else, the canvas being a plain framebuffer means you can snapshot-test rendering without a PTY: render to a `Canvas`, dump it as a text grid, compare. Fast and deterministic.
