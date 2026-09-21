//! Application state, event loop and drawing.
//!
//! The main thread owns all state and performs no blocking I/O. It selects on
//! one channel, drains everything available, collapses redundant events, and
//! draws once — event-driven rather than a fixed-rate loop, which matters for
//! battery and for SSH.

use crate::git::{Change, CommitMeta, Diff, FileStatus, HeadInfo};
use crate::input::hit::HitBuffer;
use crate::layout::tree::{NodeId, Scale, Tree};
use crate::layout::treemap::{self, Layout, Rect as PxRect};
use crate::render::canvas::Canvas;
use crate::render::palette::{self, ColorDepth, StatusPalette};
use crate::render::{diff as diffpane, map, timeline};
use crate::worker::{Message, Request};

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout as TuiLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

/// How long the cursor must hold still before a diff is fetched. Without this,
/// sweeping across a directory spawns a diff per file and the pane thrashes.
const DIFF_DEBOUNCE: Duration = Duration::from_millis(80);

/// Coalescing window: a burst of motion events produces one frame.
///
/// This bounds how long a single drain pass may run, not how long to sleep.
const COALESCE: Duration = Duration::from_millis(8);

/// Most events or messages one drain pass will take from a single channel.
///
/// A bound rather than "until empty" so a flooding producer cannot starve the
/// other channel or the frame that should follow it.
const DRAIN_MAX: usize = 512;

/// Widest the diff pane may get when the panes sit side by side.
///
/// 80 columns of diff text plus the pane's two border columns. Beyond this a
/// unified diff gains nothing but trailing whitespace, whereas the map turns
/// every extra column into pixels.
const DIFF_MAX_COLS: u16 = 82;

/// How many commits the log view loads.
///
/// Deep enough to browse recent history without paying for a full walk of a
/// large repository; the design doc's incremental history cache is what makes
/// going deeper cheap, and that is M2 work.
const LOG_LIMIT: usize = 200;

/// How often the gathering spinner advances a frame.
const SPINNER_TICK: Duration = Duration::from_millis(100);

/// Frames of the gathering spinner, one per redraw tick.
///
/// Braille rather than ASCII so it occupies one cell and reads as motion at the
/// size the pane gives it.
const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// Split direction. Diffs want width, maps want square.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Auto,
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Status,
    Heatmap,
    Churn,
    Log,
}

impl View {
    /// Every view, in tab-bar and cycle order.
    pub const ALL: [View; 4] = [View::Status, View::Heatmap, View::Churn, View::Log];

    /// Cycle order for `Tab`.
    fn next(self) -> View {
        let i = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// Cycle order for `Shift+Tab`.
    fn prev(self) -> View {
        let i = Self::ALL.iter().position(|&v| v == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// Short name for the tab bar.
    pub fn label(self) -> &'static str {
        match self {
            View::Status => "changes",
            View::Heatmap => "age",
            View::Churn => "churn",
            View::Log => "history",
        }
    }

    /// Whether the view needs the history walk's age and churn data.
    pub fn needs_history(self) -> bool {
        matches!(self, View::Heatmap | View::Churn)
    }

    /// One line saying what the view is for, shown under the tabs.
    pub fn blurb(self) -> &'static str {
        match self {
            View::Status => "what you have changed since HEAD",
            View::Heatmap => "how recently each file was last committed",
            View::Churn => "how many commits have touched each file",
            View::Log => "commits, and which files each one touched",
        }
    }
}

/// A modal overlay owning the keyboard.
pub enum Modal {
    None,
    Commit {
        message: String,
        amend: bool,
    },
    Help,
    /// A hook's stderr or any other error, dismissible.
    Error(String),
    Find {
        query: String,
    },
}

pub struct App {
    pub tree: Tree,
    pub layout: Layout,
    pub hits: HitBuffer,
    pub canvas: Canvas,

    pub status: HashMap<PathBuf, FileStatus>,
    pub head: Option<HeadInfo>,
    pub data: map::MapData,

    pub hovered: Option<NodeId>,
    /// A clicked file, which stops the diff following the mouse.
    pub pinned: Option<NodeId>,
    pub diff: Option<(PathBuf, Diff)>,
    pub diff_scroll: u16,

    /// Commit metadata, newest first. Loaded when the log view is first opened.
    pub log: Vec<CommitMeta>,
    /// Index into `log` of the highlighted commit.
    pub log_sel: usize,
    /// First visible row, so the selection can scroll without moving.
    log_top: usize,
    /// Files touched by the selected commit, and its diff.
    pub commit_paths: HashSet<PathBuf>,
    commit_diff: Option<Diff>,
    /// Tags commit-detail requests so stale replies are dropped.
    detail_seq: u64,
    /// Set once the heatmap's history walk has been requested.
    history_loaded: bool,
    /// Commits the streaming walk has covered so far, for the progress line.
    history_walked: usize,
    /// Whether that walk has finished. Until it has, churn counts are "commits
    /// in the most recent N", because git walks newest first.
    history_done: bool,
    /// Spinner frame, advanced on a timer so it turns at a constant rate
    /// rather than at whatever rate the UI happens to repaint.
    spinner: usize,
    spinner_at: Instant,
    /// Index into the churn ranking of the selected file, for j/k.
    churn_sel: usize,
    /// First visible row of the churn list, so the selection can scroll
    /// without the list jumping under it.
    churn_top: usize,
    /// What the worker is busy with, shown in the status bar so a slow first
    /// load on a large repository reads as progress rather than a hang.
    pending: Option<&'static str>,

    pub view: View,
    pub split: Split,
    pub modal: Modal,
    pub message: String,
    pub depth: ColorDepth,
    pub palette: StatusPalette,
    pub scale: Scale,

    /// Undo stack of (path, was_staged) for `u`.
    pub undo: Vec<(PathBuf, bool)>,

    tx: Sender<Request>,
    /// Monotonic tag; diff results behind the current value are dropped.
    diff_seq: u64,
    /// Set when the hover moved and a diff has not been requested yet.
    pending_hover: Option<Instant>,
    /// Geometry the current layout was computed for.
    laid_out_for: (u16, u16),
    map_area: Rect,
    tab_area: Rect,
    pub should_quit: bool,
    pub dirty: bool,
}

impl App {
    pub fn new(tx: Sender<Request>) -> App {
        App {
            tree: Tree::build(&[], Scale::Sqrt),
            layout: Layout {
                rects: Vec::new(),
                collapsed: Vec::new(),
            },
            hits: HitBuffer::empty(),
            canvas: Canvas::new(0, 0),
            status: HashMap::new(),
            head: None,
            data: map::MapData::new(),
            hovered: None,
            pinned: None,
            diff: None,
            diff_scroll: 0,
            log: Vec::new(),
            log_sel: 0,
            log_top: 0,
            commit_paths: HashSet::new(),
            commit_diff: None,
            detail_seq: 0,
            history_loaded: false,
            history_walked: 0,
            history_done: false,
            spinner: 0,
            spinner_at: Instant::now(),
            churn_sel: 0,
            churn_top: 0,
            pending: None,
            view: View::Status,
            split: Split::Auto,
            modal: Modal::None,
            message: String::new(),
            depth: ColorDepth::detect(),
            palette: StatusPalette::default(),
            scale: Scale::Sqrt,
            undo: Vec::new(),
            tx,
            diff_seq: 0,
            pending_hover: None,
            laid_out_for: (0, 0),
            map_area: Rect::new(0, 0, 0, 0),
            tab_area: Rect::new(0, 0, 0, 0),
            should_quit: false,
            dirty: true,
        }
    }

    /// Block for the next event, then drain everything else that is waiting.
    ///
    /// Only the newest mouse position survives the drain, which removes most of
    /// the perceived lag over a slow link where motion events arrive in bursts.
    pub fn pump(&mut self, events: &Receiver<Event>, msgs: &Receiver<Message>) -> Result<()> {
        let mut latest_motion: Option<MouseEvent> = None;
        let mut got_anything = false;

        // Wait for something to happen, with a timeout so the debounce timer
        // still fires on an otherwise idle terminal.
        let timeout = if self.pending_hover.is_some() {
            DIFF_DEBOUNCE
        } else if self.gathering() {
            // Tick fast enough for the spinner to read as motion while the
            // history walk streams in.
            SPINNER_TICK
        } else {
            Duration::from_millis(250)
        };

        // Advance the spinner on a wall clock, so its rate does not depend on
        // how often anything else forces a redraw.
        if self.gathering() && self.spinner_at.elapsed() >= SPINNER_TICK {
            self.spinner_at = Instant::now();
            self.spinner = self.spinner.wrapping_add(1);
            self.dirty = true;
        }

        crossbeam_select(events, msgs, timeout, |ev| match ev {
            Either::Event(e) => {
                got_anything = true;
                if let Event::Mouse(m) = &e
                    && matches!(m.kind, MouseEventKind::Moved)
                {
                    latest_motion = Some(*m);
                    return;
                }
                self.on_event(e);
            }
            Either::Msg(m) => {
                got_anything = true;
                self.on_message(m);
            }
        });

        // Drain what has already piled up, then return so a frame can be
        // drawn. Deliberately a bounded pass rather than a sleep-and-rescan
        // loop: a mouse producing motion faster than the coalescing window can
        // feed such a loop forever, and the UI then never draws at all. That is
        // a livelock, not a slow repository, and it presents as a total freeze.
        let deadline = Instant::now() + COALESCE;
        loop {
            let mut progressed = false;
            for _ in 0..DRAIN_MAX {
                let Ok(e) = events.try_recv() else { break };
                progressed = true;
                got_anything = true;
                if let Event::Mouse(m) = &e
                    && matches!(m.kind, MouseEventKind::Moved)
                {
                    latest_motion = Some(*m);
                    continue;
                }
                self.on_event(e);
                if Instant::now() >= deadline {
                    break;
                }
            }
            for _ in 0..DRAIN_MAX {
                let Ok(m) = msgs.try_recv() else { break };
                progressed = true;
                got_anything = true;
                self.on_message(m);
                if Instant::now() >= deadline {
                    break;
                }
            }
            if !progressed || Instant::now() >= deadline {
                break;
            }
        }

        if let Some(m) = latest_motion {
            self.on_motion(m);
        }
        let _ = got_anything;

        // Fire the debounced diff once the cursor has been still long enough.
        if let Some(t) = self.pending_hover
            && t.elapsed() >= DIFF_DEBOUNCE
        {
            self.pending_hover = None;
            self.request_diff();
        }
        Ok(())
    }

    /// Apply a worker message. Public so integration tests can drive the app
    /// deterministically instead of racing the pump loop.
    pub fn apply(&mut self, m: Message) {
        self.on_message(m)
    }

    fn on_message(&mut self, m: Message) {
        self.dirty = true;
        match m {
            Message::Status { files, head } => {
                self.status = files.into_iter().map(|f| (f.path.clone(), f)).collect();
                self.head = Some(head);
                self.data.status = self.status.clone();
                // Untracked files are not in HEAD's tree, so fold them in or
                // they would be invisible on the map.
                self.merge_untracked();
            }
            Message::Tree(entries) => {
                let pairs: Vec<(PathBuf, u64)> =
                    entries.into_iter().map(|e| (e.path, e.size)).collect();
                self.tree = Tree::build(&pairs, self.scale);
                self.merge_untracked();
                // Force a relayout on the next draw.
                self.laid_out_for = (0, 0);
                self.hovered = None;
                self.pinned = None;
            }
            Message::Diff { path, diff, seq } => {
                // Drop results whose sequence is behind the current hover.
                if seq == self.diff_seq {
                    self.diff = Some((path, diff));
                    self.diff_scroll = 0;
                }
            }
            Message::Done(s) => {
                self.message = s;
                // Refresh rather than trusting an optimistic local mutation:
                // a hook or filter may have changed the outcome.
                let _ = self.tx.send(Request::Status);
                // The file under the cursor just changed sides.
                self.request_diff();
            }
            Message::History {
                entries,
                commits,
                done,
            } => {
                // Ages are relative to now, so they are computed once here
                // rather than on every redraw.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                self.data.age = entries
                    .iter()
                    .map(|(p, t, _)| (p.clone(), (now - t).max(0)))
                    .collect();
                self.data.churn = entries.into_iter().map(|(p, _, n)| (p, n)).collect();
                self.history_walked = commits;
                self.history_done = done;
                if done {
                    self.pending = None;
                }
                // Keep the churn selection on the same file as the ranking
                // reshuffles underneath it.
                if self.view == View::Churn && done {
                    self.sync_churn_sel();
                }
            }
            Message::Log(commits) => {
                self.pending = None;
                self.log = commits;
                self.log_sel = 0;
                self.log_top = 0;
                self.request_commit_detail();
            }
            Message::CommitDetail {
                oid,
                paths,
                diff,
                seq,
            } => {
                // Drop a reply for a selection the user has already moved past.
                if seq == self.detail_seq {
                    self.commit_paths = paths.into_iter().collect();
                    self.commit_diff = Some(diff);
                    self.data.commit_paths = self.commit_paths.clone();
                    self.diff_scroll = 0;
                    let _ = oid;
                }
            }
            Message::Error(e) => {
                self.modal = Modal::Error(e);
            }
        }
    }

    /// Add untracked paths to the tree so they can be seen and staged.
    ///
    /// Geometry is otherwise a function of HEAD; a new file is a genuine
    /// add/remove, which is one of the events that legitimately relayouts.
    fn merge_untracked(&mut self) {
        let extra: Vec<PathBuf> = self
            .status
            .values()
            .filter(|s| s.unstaged == Change::Untracked || s.staged == Change::Added)
            .map(|s| s.path.clone())
            .filter(|p| self.tree.find(p).is_none())
            .collect();
        if extra.is_empty() {
            return;
        }
        let mut pairs: Vec<(PathBuf, u64)> = self
            .tree
            .files_under(self.tree.root)
            .into_iter()
            .map(|f| {
                let n = self.tree.node(f);
                (n.path.clone(), n.size)
            })
            .collect();
        for p in extra {
            // Size the new file from disk; fall back to a small default so it
            // still gets a hoverable block.
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(256);
            pairs.push((p, size.max(1)));
        }
        self.tree = Tree::build(&pairs, self.scale);
        self.laid_out_for = (0, 0);
    }

    fn on_event(&mut self, e: Event) {
        self.dirty = true;
        match e {
            Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Resize(_, _) => self.laid_out_for = (0, 0),
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        // Modals own the keyboard while they are up.
        match &mut self.modal {
            Modal::Error(_) => {
                self.modal = Modal::None;
                return;
            }
            Modal::Help => {
                self.modal = Modal::None;
                return;
            }
            Modal::Commit { message, amend } => {
                match k.code {
                    KeyCode::Esc => self.modal = Modal::None,
                    KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        *amend = !*amend;
                    }
                    KeyCode::Enter => {
                        let (msg, amend) = (message.clone(), *amend);
                        if msg.trim().is_empty() {
                            self.modal = Modal::Error("empty commit message".into());
                        } else {
                            self.modal = Modal::None;
                            let _ = self.tx.send(Request::Commit {
                                message: msg,
                                amend,
                            });
                        }
                    }
                    KeyCode::Backspace => {
                        message.pop();
                    }
                    KeyCode::Char(c) => message.push(c),
                    _ => {}
                }
                return;
            }
            Modal::Find { query } => {
                match k.code {
                    KeyCode::Esc => self.modal = Modal::None,
                    KeyCode::Enter => {
                        let q = query.clone();
                        self.modal = Modal::None;
                        self.find(&q);
                    }
                    KeyCode::Backspace => {
                        query.pop();
                    }
                    KeyCode::Char(c) => query.push(c),
                    _ => {}
                }
                return;
            }
            Modal::None => {}
        }

        match k.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Char('?') => self.modal = Modal::Help,
            // Jump straight to a view; the digits match the tab bar.
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if let Some(&v) = View::ALL.get(i)
                    && v != self.view
                {
                    self.view = v;
                    self.on_view_changed();
                }
            }
            KeyCode::Char(' ') => self.toggle_stage(),
            KeyCode::Char('a') => self.stage_under_cursor(),
            KeyCode::Char('c') => {
                self.modal = Modal::Commit {
                    message: String::new(),
                    amend: false,
                }
            }
            // Ctrl+U before plain `u`, or undo swallows the scroll binding.
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.diff_scroll = self.diff_scroll.saturating_sub(5)
            }
            KeyCode::Char('u') => self.undo_last(),
            KeyCode::Tab => {
                self.view = self.view.next();
                self.on_view_changed();
            }
            KeyCode::BackTab => {
                // Shift+Tab walks the cycle backwards.
                self.view = self.view.prev();
                self.on_view_changed();
            }
            KeyCode::Char('|') => self.split = Split::Vertical,
            KeyCode::Char('-') => self.split = Split::Horizontal,
            KeyCode::Char('/') => {
                self.modal = Modal::Find {
                    query: String::new(),
                }
            }
            // Keyboard navigation is a first-class equal to the mouse, not a
            // fallback: it is what makes the tool work over a laggy link and
            // in terminals where motion reporting does not survive.
            KeyCode::Char('n') => self.step_changed(1),
            KeyCode::Char('p') => self.step_changed(-1),
            KeyCode::Char('j') | KeyCode::Down => {
                if self.view == View::Log {
                    self.step_commit(1);
                } else if self.view == View::Churn {
                    self.step_churn(1);
                } else {
                    self.diff_scroll = self.diff_scroll.saturating_add(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.view == View::Log {
                    self.step_commit(-1);
                } else if self.view == View::Churn {
                    self.step_churn(-1);
                } else {
                    self.diff_scroll = self.diff_scroll.saturating_sub(1);
                }
            }
            // In the log view j/k drive the selection, so the diff underneath
            // scrolls with Ctrl+D / Ctrl+U instead.
            KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.diff_scroll = self.diff_scroll.saturating_add(5)
            }
            KeyCode::Char('y') => self.yank(),
            KeyCode::Enter => self.pinned = self.hovered,
            KeyCode::Esc => {
                self.pinned = None;
                self.message.clear();
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::Moved => self.on_motion(m),
            MouseEventKind::Down(MouseButton::Left) => {
                // A click on the tab bar switches view rather than pinning.
                if let Some(v) = self.tab_at(m.column, m.row) {
                    if v != self.view {
                        self.view = v;
                        self.on_view_changed();
                    }
                    return;
                }
                self.on_motion(m);
                self.pinned = self.hovered;
                self.request_diff();
            }
            MouseEventKind::ScrollDown => self.diff_scroll = self.diff_scroll.saturating_add(3),
            MouseEventKind::ScrollUp => self.diff_scroll = self.diff_scroll.saturating_sub(3),
            _ => {}
        }
    }

    fn on_motion(&mut self, m: MouseEvent) {
        let area = self.map_area;
        if m.column < area.x || m.row < area.y || m.column >= area.right() || m.row >= area.bottom()
        {
            return;
        }
        let id = self.hits.at(m.column - area.x, m.row - area.y);
        if id != self.hovered {
            self.hovered = id;
            self.dirty = true;
            // Hovering a block moves the churn list to the matching row, so the
            // two halves of the view never disagree about what is selected.
            if self.view == View::Churn && self.pinned.is_none() {
                self.sync_churn_sel();
            }
            // The highlight redraws on this frame; the diff waits for stillness.
            if self.pinned.is_none() {
                self.pending_hover = Some(Instant::now());
            }
        }
    }

    /// The file the diff and staging actions apply to.
    fn target(&self) -> Option<NodeId> {
        self.pinned.or(self.hovered)
    }

    fn request_diff(&mut self) {
        let Some(id) = self.target() else { return };
        let node = self.tree.node(id);
        if node.is_dir {
            return;
        }
        let path = node.path.clone();
        // Show the staged side when the file has staged changes and no
        // unstaged ones, which is what the user just looked at after staging.
        let staged = self
            .status
            .get(&path)
            .map(|s| s.is_staged() && s.unstaged == Change::None)
            .unwrap_or(false);
        self.diff_seq += 1;
        let _ = self.tx.send(Request::Diff {
            path,
            staged,
            seq: self.diff_seq,
        });
    }

    /// Seconds since the epoch, for turning a commit time into an age.
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Public wrapper so integration tests can drive a view switch.
    pub fn apply_view_change(&mut self) {
        self.on_view_changed()
    }

    /// React to a view switch: the log needs its data fetched the first time.
    fn on_view_changed(&mut self) {
        self.diff_scroll = 0;
        if self.view.needs_history() && !self.history_loaded {
            // Lazily loaded, like the log: a session that never opens the
            // heatmap should not pay for the walk.
            self.history_loaded = true;
            self.pending = Some("gathering history…");
            let _ = self.tx.send(Request::History);
        }
        // Entering churn, start the selection from whatever the map is already
        // pointing at, so switching tabs does not throw away the file the user
        // had found in another view.
        if self.view == View::Churn {
            self.sync_churn_sel();
        }
        if self.view == View::Log {
            if self.log.is_empty() {
                // Lazily loaded: a repo the user never opens the log on should
                // not pay for the walk.
                self.pending = Some("loading commits…");
                let _ = self.tx.send(Request::Log { limit: LOG_LIMIT });
            } else {
                self.request_commit_detail();
            }
        } else {
            // Leaving the log clears its highlight so the status and heatmap
            // views are not tinted by a stale commit selection.
            self.data.commit_paths.clear();
        }
    }

    /// Ask for the selected commit's touched paths and diff.
    fn request_commit_detail(&mut self) {
        let Some(c) = self.log.get(self.log_sel) else {
            return;
        };
        self.detail_seq += 1;
        let _ = self.tx.send(Request::CommitDetail {
            oid: c.oid.clone(),
            seq: self.detail_seq,
        });
    }

    /// Move the log selection, keeping it on screen.
    fn step_commit(&mut self, delta: i32) {
        if self.log.is_empty() {
            return;
        }
        let n = self.log.len() as i32;
        let next = (self.log_sel as i32 + delta).clamp(0, n - 1) as usize;
        if next == self.log_sel {
            return;
        }
        self.log_sel = next;
        self.request_commit_detail();
    }

    fn toggle_stage(&mut self) {
        let Some(id) = self.target() else { return };
        if self.tree.node(id).is_dir {
            self.stage_under_cursor();
            return;
        }
        let path = self.tree.node(id).path.clone();
        let staged = self
            .status
            .get(&path)
            .map(|s| s.is_staged())
            .unwrap_or(false);
        self.undo.push((path.clone(), staged));
        let req = if staged {
            Request::Unstage(path)
        } else {
            Request::Stage(path)
        };
        let _ = self.tx.send(req);
    }

    /// Stage every changed file beneath the hovered directory.
    fn stage_under_cursor(&mut self) {
        let Some(id) = self.target() else { return };
        let node_is_dir = self.tree.node(id).is_dir;
        let ids = if node_is_dir {
            self.tree.files_under(id)
        } else {
            vec![id]
        };
        let paths: Vec<PathBuf> = ids
            .into_iter()
            .map(|i| self.tree.node(i).path.clone())
            .filter(|p| {
                self.status
                    .get(p)
                    .map(|s| s.unstaged != Change::None)
                    .unwrap_or(false)
            })
            .collect();
        if paths.is_empty() {
            self.message = "nothing to stage there".into();
            return;
        }
        for p in &paths {
            let staged = self.status.get(p).map(|s| s.is_staged()).unwrap_or(false);
            self.undo.push((p.clone(), staged));
        }
        let _ = self.tx.send(Request::StageAll(paths));
    }

    fn undo_last(&mut self) {
        let Some((path, was_staged)) = self.undo.pop() else {
            self.message = "nothing to undo".into();
            return;
        };
        let req = if was_staged {
            Request::Stage(path)
        } else {
            Request::Unstage(path)
        };
        let _ = self.tx.send(req);
    }

    /// Move the hover to the next/previous changed file in path order.
    fn step_changed(&mut self, delta: i32) {
        let mut changed: Vec<NodeId> = self
            .tree
            .files_under(self.tree.root)
            .into_iter()
            .filter(|&f| {
                self.status
                    .get(&self.tree.node(f).path)
                    .map(|s| s.dominant() != Change::None)
                    .unwrap_or(false)
            })
            .collect();
        if changed.is_empty() {
            self.message = "no changed files".into();
            return;
        }
        changed.sort_by(|&a, &b| self.tree.node(a).path.cmp(&self.tree.node(b).path));

        let cur = self
            .target()
            .and_then(|t| changed.iter().position(|&c| c == t));
        let next = match cur {
            Some(i) => {
                let n = changed.len() as i32;
                (((i as i32 + delta) % n) + n) as usize % changed.len()
            }
            None => 0,
        };
        self.hovered = Some(changed[next]);
        self.pinned = Some(changed[next]);
        self.request_diff();
    }

    /// Files the history walk saw change, most-changed first.
    ///
    /// One order shared by the list and by `j` / `k`, so the selection a
    /// keypress moves is the line the user is looking at. Ties break on path
    /// so the order is stable between frames rather than following the hash
    /// map's iteration order.
    fn churn_ranking(&self) -> Vec<(PathBuf, u32)> {
        let mut v: Vec<(PathBuf, u32)> = self
            .data
            .churn
            .iter()
            .filter(|&(_, &n)| n > 0)
            .map(|(p, &n)| (p.clone(), n))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Point the churn selection at the currently targeted file, if that file
    /// appears in the ranking at all. Leaves the selection alone when it does
    /// not, so an untracked hover does not reset the list to the top.
    fn sync_churn_sel(&mut self) {
        let Some(id) = self.target() else { return };
        let path = self.tree.node(id).path.clone();
        if let Some(i) = self.churn_ranking().iter().position(|(p, _)| *p == path) {
            self.churn_sel = i;
        }
    }

    /// Why a file has no history, in the cases we can actually distinguish.
    ///
    /// "No commits touch this file" is only true for something never committed.
    /// An untracked file is exactly that; anything else tracked but absent from
    /// a completed walk was renamed, since the walk passes `--no-renames` and
    /// so stops at a rename rather than following through it.
    fn no_history_reason(&self, path: &std::path::Path) -> String {
        let untracked = self
            .status
            .get(path)
            .map(|s| s.unstaged == Change::Untracked)
            .unwrap_or(false);
        if untracked {
            "untracked · not in any commit".to_string()
        } else {
            "no commits touch this file".to_string()
        }
    }

    /// Whether a history walk is in flight, so the UI should animate.
    fn gathering(&self) -> bool {
        self.history_loaded && !self.history_done
    }

    /// Where the churn ramp saturates for the current walk.
    fn churn_saturation(&self) -> u32 {
        let mut counts: Vec<u32> = self.data.churn.values().copied().collect();
        map::ChurnColorizer::saturation_point(&mut counts)
    }

    /// Move the churn selection, and point the map at the same file.
    ///
    /// Setting `pinned` as well as `hovered` is what keeps the highlight where
    /// the keyboard put it: a stray mouse motion would otherwise drag the map's
    /// highlight off the row the list still shows as selected.
    fn step_churn(&mut self, delta: i32) {
        let ranked = self.churn_ranking();
        if ranked.is_empty() {
            self.message = "no history yet".into();
            return;
        }
        let n = ranked.len() as i32;
        let i = (((self.churn_sel as i32 + delta) % n) + n) % n;
        self.churn_sel = i as usize;
        // A ranked file may be missing from the tree — deleted since, or
        // outside the current HEAD listing. The row still selects; there is
        // just nothing on the map to point at.
        let id = self.tree.find(&ranked[self.churn_sel].0);
        self.hovered = id;
        self.pinned = id;
        self.dirty = true;
    }

    fn find(&mut self, query: &str) {
        if query.is_empty() {
            return;
        }
        let q = query.to_lowercase();
        let found = self
            .tree
            .files_under(self.tree.root)
            .into_iter()
            .find(|&f| {
                self.tree
                    .node(f)
                    .path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&q)
            });
        match found {
            Some(f) => {
                self.hovered = Some(f);
                self.pinned = Some(f);
                self.request_diff();
            }
            None => self.message = format!("no match: {query}"),
        }
    }

    /// `y` yanks the hovered path, or the selected SHA in the log view.
    fn yank(&mut self) {
        if self.view == View::Log {
            if let Some(c) = self.log.get(self.log_sel) {
                self.message = c.oid.clone();
            }
            return;
        }
        match self.target() {
            Some(id) => {
                let p = self.tree.node(id).path.display().to_string();
                // No clipboard dependency in v1: print it in the status line so
                // it can be selected with Shift held.
                self.message = p;
            }
            None => self.message = "nothing hovered".into(),
        }
    }

    /// Recompute layout, canvas and hit buffer for the map area.
    fn relayout(&mut self, area: Rect) {
        if self.laid_out_for == (area.width, area.height) && !self.tree.nodes.is_empty() {
            return;
        }
        self.laid_out_for = (area.width, area.height);
        let (pw, ph) = (area.width, area.height.saturating_mul(2));
        self.canvas = Canvas::new(pw, ph);
        self.layout = treemap::layout(&self.tree, PxRect::new(0.0, 0.0, pw as f64, ph as f64));
        let tree = &self.tree;
        let collapsed = &self.layout.collapsed;
        self.hits = HitBuffer::build(&self.layout, area.width, area.height, &|id| {
            !tree.node(id).is_dir || collapsed[id]
        });
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let (map_area, side_area, status_area) = self.split_areas(area);
        self.map_area = map_area;
        self.relayout(map_area);

        let colorizer: Box<dyn map::Colorizer> = match self.view {
            View::Status => Box::new(map::StatusColorizer {
                palette: StatusPalette::default(),
            }),
            View::Heatmap => Box::new(map::HeatColorizer {
                palette: StatusPalette::default(),
            }),
            View::Churn => Box::new(map::ChurnColorizer {
                max: self.churn_saturation(),
            }),
            View::Log => Box::new(map::LogColorizer {
                palette: StatusPalette::default(),
            }),
        };

        // Move the canvas out so the draw call can borrow the rest of `self`
        // immutably, then put it back.
        let mut canvas = std::mem::replace(&mut self.canvas, Canvas::new(0, 0));
        map::draw(
            &mut canvas,
            &self.tree,
            &self.layout,
            &self.data,
            &*colorizer,
            &self.palette,
            self.depth,
            self.target(),
        );
        canvas.blit(f.buffer_mut(), map_area);
        self.canvas = canvas;

        self.draw_side(f, side_area);
        self.draw_status(f, status_area);
        self.draw_modal(f, area);
    }

    /// Choose the split from the terminal's aspect ratio: diffs want width,
    /// maps want square.
    fn split_areas(&self, area: Rect) -> (Rect, Rect, Rect) {
        let rows = TuiLayout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        let (body, status) = (rows[0], rows[1]);

        let vertical = match self.split {
            Split::Vertical => true,
            Split::Horizontal => false,
            Split::Auto => body.width as f32 >= 2.2 * body.height as f32,
        };
        if vertical {
            // Side by side. The diff takes half, but never more than
            // DIFF_MAX_COLS: past roughly 80 columns a unified diff just grows
            // whitespace, while the map can always use the pixels. On a wide
            // terminal the surplus therefore goes to the map.
            let diff_w = (body.width / 2).min(DIFF_MAX_COLS);
            let parts = TuiLayout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(diff_w)])
                .split(body);
            (parts[0], parts[1], status)
        } else {
            // Stacked. Width is shared by both panes, so there is nothing to
            // cap; the split is by height.
            let parts = TuiLayout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(body);
            (parts[0], parts[1], status)
        }
    }

    fn draw_side(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL);
        let inner = block.inner(area);
        f.render_widget(block, area);

        // Tab bar, then a one-line description of what the view shows. The
        // description is what makes "age" mean something without having to
        // press the key and guess from the colours.
        let rows = TuiLayout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);
        self.tab_area = rows[0];
        self.draw_tabs(f, rows[0]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                self.view.blurb(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))),
            rows[1],
        );
        let inner = rows[2];

        if self.view == View::Log {
            self.draw_log(f, inner);
            return;
        }

        // In the heatmap the diff is beside the point: what the view claims
        // about a file is its age and churn, so that is what it should show.
        if self.view == View::Heatmap {
            self.draw_age_pane(f, inner);
            return;
        }

        // Same argument for churn: the view's claim is a commit count, and the
        // ranking is the part a colour ramp cannot tell you.
        if self.view == View::Churn {
            self.draw_churn_pane(f, inner);
            return;
        }

        let lines = match (&self.diff, self.target()) {
            (Some((path, d)), _) => diffpane::lines(d, &path.display().to_string()),
            (None, Some(id)) => {
                let n = self.tree.node(id);
                vec![
                    Line::from(Span::styled(
                        n.path.display().to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(if n.is_dir {
                        format!("directory · {} files", self.tree.files_under(id).len())
                    } else {
                        format!("{} bytes", n.size)
                    }),
                ]
            }
            (None, None) => vec![Line::from(Span::styled(
                "hover a block, or press n / p / ?",
                Style::default().fg(Color::DarkGray),
            ))],
        };
        f.render_widget(
            Paragraph::new(lines)
                .scroll((self.diff_scroll, 0))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    /// The heatmap's pane: when the hovered file was last committed, and how
    /// often it has been touched.
    fn draw_age_pane(&self, f: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        match self.target() {
            Some(id) => {
                let path = self.tree.node(id).path.clone();
                lines.push(Line::from(Span::styled(
                    path.display().to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                match self.data.age.get(&path) {
                    Some(&age) => {
                        lines.push(Line::from(format!("last touched  {}", human_age(age))));
                        let churn = self.data.churn.get(&path).copied().unwrap_or(0);
                        lines.push(Line::from(format!("commits       {churn}")));
                    }
                    // While the walk is still running, absence means "not
                    // reached yet", not "never committed". Saying the latter
                    // would be wrong for most files for most of the walk.
                    None if self.gathering() => lines.push(Line::from(vec![
                        Span::styled(
                            format!("{} ", SPINNER[self.spinner % SPINNER.len()]),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            "gathering history…",
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])),
                    None => lines.push(Line::from(Span::styled(
                        self.no_history_reason(&path),
                        Style::default().fg(Color::DarkGray),
                    ))),
                }
            }
            None => lines.push(Line::from(Span::styled(
                "hover a block to see when it last changed",
                Style::default().fg(Color::DarkGray),
            ))),
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    /// The churn pane: the selected file's commit count, then the files the
    /// walk saw change most. The ranking is the part the map cannot show —
    /// the busiest files are often small, so they are easy to miss as blocks.
    fn draw_churn_pane(&mut self, f: &mut Frame, area: Rect) {
        let ranked = self.churn_ranking();

        // The ranking is withheld until the walk finishes. git walks newest
        // first, so a partial count is "commits in the most recent N" — a real
        // quantity, but not the one this list claims, and the order visibly
        // reshuffles while it fills. The map updates live regardless: a block
        // growing greener reads as progress, whereas a top-ten list rewriting
        // itself reads as a glitch.
        if !self.history_done {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    format!("{} ", SPINNER[self.spinner % SPINNER.len()]),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    if self.history_walked == 0 {
                        "gathering history…".to_string()
                    } else {
                        format!("gathering history… {} commits", self.history_walked)
                    },
                    Style::default().fg(Color::DarkGray),
                ),
            ])];
            lines.push(Line::from(Span::styled(
                "  ranking appears when complete",
                Style::default().fg(Color::DarkGray),
            )));
            if !ranked.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  {} files so far · map is live", ranked.len()),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            f.render_widget(Paragraph::new(lines), area);
            return;
        }

        if ranked.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no history here",
                    Style::default().fg(Color::DarkGray),
                ))),
                area,
            );
            return;
        }

        let mut head = Vec::new();
        if let Some(id) = self.target() {
            let path = self.tree.node(id).path.clone();
            head.push(Line::from(Span::styled(
                path.display().to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            let n = self.data.churn.get(&path).copied().unwrap_or(0);
            head.push(Line::from(match n {
                0 => self.no_history_reason(&path),
                1 => "1 commit".to_string(),
                n => format!("{n} commits"),
            }));
            head.push(Line::from(""));
        }
        head.push(Line::from(Span::styled(
            "most changed · j / k to walk",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));

        // Scroll only as far as it takes to keep the selection on screen, so
        // the list holds still while the cursor moves inside it.
        let rows = area.height.saturating_sub(head.len() as u16) as usize;
        self.churn_sel = self.churn_sel.min(ranked.len() - 1);
        if rows > 0 {
            if self.churn_sel < self.churn_top {
                self.churn_top = self.churn_sel;
            } else if self.churn_sel >= self.churn_top + rows {
                self.churn_top = self.churn_sel + 1 - rows;
            }
        }
        let top = self.churn_top.min(ranked.len().saturating_sub(1));

        let ramp = map::ChurnColorizer {
            max: self.churn_saturation(),
        };
        let hovered = self.target().map(|id| self.tree.node(id).path.clone());
        let width = ranked
            .iter()
            .skip(top)
            .take(rows)
            .map(|e| e.1.to_string().len())
            .max()
            .unwrap_or(1);

        let mut lines = head;
        for (i, (path, n)) in ranked.iter().enumerate().skip(top).take(rows) {
            // Highlight the row the map is pointing at, however it got there:
            // a mouse hover over a block lights up its line in the list too.
            let selected = i == self.churn_sel || hovered.as_deref() == Some(path.as_path());
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // Tint the count through the colorizer, so a line in the list and
            // its block on the map are the same colour by construction rather
            // than by two copies of the ramp maths agreeing.
            let c = ramp.color_for(*n);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{n:>width$} "),
                    if selected {
                        style
                    } else {
                        Style::default().fg(Color::Rgb(c.0, c.1, c.2))
                    },
                ),
                Span::styled(path.display().to_string(), style),
            ]));
        }

        f.render_widget(Paragraph::new(lines), area);
    }

    /// Draw the view tabs, so every view is visible rather than being a name
    /// that only appears once you have already switched to it.
    fn draw_tabs(&self, f: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        for (i, v) in View::ALL.iter().enumerate() {
            let selected = *v == self.view;
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            // The digit doubles as the shortcut that jumps straight here.
            spans.push(Span::styled(format!(" {} {} ", i + 1, v.label()), style));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Which tab, if any, sits under a cell. Used for click-to-switch.
    fn tab_at(&self, col: u16, row: u16) -> Option<View> {
        let a = self.tab_area;
        if row != a.y || col < a.x {
            return None;
        }
        let mut x = a.x;
        for v in View::ALL {
            // Width must match `draw_tabs`: " N label " plus one space.
            let w = v.label().chars().count() as u16 + 5;
            if col >= x && col < x + w - 1 {
                return Some(v);
            }
            x += w;
        }
        None
    }

    /// The log pane: a braille commits-per-day strip, the commit list, and the
    /// selected commit's diff underneath.
    fn draw_log(&mut self, f: &mut Frame, area: Rect) {
        if self.log.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "loading history…",
                    Style::default().fg(Color::DarkGray),
                ))),
                area,
            );
            return;
        }

        // Strip on top, list in the middle, diff below. The list gets a fixed
        // share so the diff always has room to be useful.
        let list_h = (area.height.saturating_sub(3) / 2).clamp(1, 14);
        let rows = TuiLayout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(list_h),
                Constraint::Min(0),
            ])
            .split(area);
        self.draw_timeline(f, rows[0]);
        self.draw_commit_list(f, rows[1]);
        self.draw_commit_diff(f, rows[2]);
    }

    fn draw_timeline(&self, f: &mut Frame, area: Rect) {
        let times: Vec<i64> = self.log.iter().map(|c| c.time).collect();
        let (counts, day_of) = timeline::commits_per_day(&times);
        // One span per cell so each can take its own green: colour tracks how
        // busy the day was, the same way a contribution graph reads, and the
        // braille dot height carries the same signal a second time.
        let strip: Vec<Span> = timeline::cells(&counts, area.width)
            .into_iter()
            .map(|c| {
                let g = palette::contrib_green(c.level);
                Span::styled(
                    c.ch.to_string(),
                    Style::default().fg(Color::Rgb(g.0, g.1, g.2)),
                )
            })
            .collect();

        // Mark where the selection sits, labelled with how long ago it was.
        let marker = match day_of.get(self.log_sel) {
            Some(&day) if !counts.is_empty() && area.width > 0 => {
                let col =
                    (day * area.width as usize / counts.len().max(1)).min(area.width as usize - 1);
                let label = self
                    .log
                    .get(self.log_sel)
                    .map(|c| human_age(self.now().saturating_sub(c.time)))
                    .unwrap_or_default();
                marker_line(area.width as usize, col, &label)
            }
            _ => String::new(),
        };

        f.render_widget(
            Paragraph::new(vec![
                Line::from(strip),
                Line::from(Span::styled(marker, Style::default().fg(Color::Cyan))),
            ]),
            area,
        );
    }

    fn draw_commit_list(&mut self, f: &mut Frame, area: Rect) {
        // Keep the selection on screen without moving it more than necessary.
        let h = area.height.max(1) as usize;
        if self.log_sel < self.log_top {
            self.log_top = self.log_sel;
        } else if self.log_sel >= self.log_top + h {
            self.log_top = self.log_sel + 1 - h;
        }

        let mut lines = Vec::with_capacity(h);
        for (i, c) in self.log.iter().enumerate().skip(self.log_top).take(h) {
            let selected = i == self.log_sel;
            let short = &c.oid[..c.oid.len().min(7)];
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let text = format!(
                "{} {} {}",
                if selected { "▸" } else { " " },
                short,
                c.subject
            );
            // Pad so the selection highlight spans the pane.
            let text = format!("{text:<w$}", w = area.width as usize);
            lines.push(Line::from(Span::styled(text, style)));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_commit_diff(&self, f: &mut Frame, area: Rect) {
        let Some(c) = self.log.get(self.log_sel) else {
            return;
        };
        let lines = match &self.commit_diff {
            Some(d) => {
                let header = format!(
                    "{} · {} · {} file(s)",
                    &c.oid[..c.oid.len().min(8)],
                    c.author,
                    self.commit_paths.len()
                );
                diffpane::lines(d, &header)
            }
            None => vec![Line::from(Span::styled(
                "loading…",
                Style::default().fg(Color::DarkGray),
            ))],
        };
        f.render_widget(
            Paragraph::new(lines)
                .scroll((self.diff_scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let (staged, unstaged, untracked) = map::counts(&self.status);
        let mut spans = Vec::new();

        let head = self
            .head
            .as_ref()
            .map(|h| h.label())
            .unwrap_or_else(|| "…".into());
        spans.push(Span::styled(
            format!(" {head} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));

        // An in-progress operation is shown prominently, or the tool lies
        // about what committing will do.
        if let Some(state) = self.head.as_ref().and_then(|h| h.state.label()) {
            spans.push(Span::styled(
                format!(" {state} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        spans.push(Span::raw(
            format!("  ✓{staged} ~{unstaged} +{untracked}  ",),
        ));

        if let Some(p) = self.pending {
            spans.push(Span::styled(
                format!("{p}  "),
                Style::default().fg(Color::Yellow),
            ));
        }

        if !self.message.is_empty() {
            spans.push(Span::styled(
                format!("{}  ", self.message),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            let hint = if self.view == View::Log {
                match self.log.get(self.log_sel) {
                    Some(c) => format!(
                        "{}  j/k commit · Ctrl+D/U diff · y sha · Tab view",
                        c.subject
                    ),
                    None => "j/k commit · Tab view · ? help".into(),
                }
            } else if self.view == View::Churn {
                match self.target() {
                    Some(id) => format!(
                        "{}  j/k file · Tab view",
                        self.tree.node(id).path.display()
                    ),
                    None => "j/k file · Tab view · ? help".into(),
                }
            } else {
                match self.target() {
                    Some(id) => self.tree.node(id).path.display().to_string(),
                    None => "space stage · a stage dir · c commit · Tab view · ? help".into(),
                }
            };
            spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
        }

        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_modal(&self, f: &mut Frame, area: Rect) {
        let (title, body, height): (&str, Vec<Line>, u16) = match &self.modal {
            Modal::None => return,
            Modal::Help => (
                " help ",
                HELP.lines().map(|l| Line::from(l.to_string())).collect(),
                HELP.lines().count() as u16 + 2,
            ),
            Modal::Error(e) => (
                " error ",
                e.lines().map(|l| Line::from(l.to_string())).collect(),
                (e.lines().count() as u16 + 3).min(20),
            ),
            Modal::Commit { message, amend } => {
                let (staged, _, _) = map::counts(&self.status);
                let len = message.chars().count();
                // Live 50/72 guide, the conventional subject/body limits.
                let guide = if len > 72 {
                    Style::default().fg(Color::Red)
                } else if len > 50 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                (
                    if *amend {
                        " commit --amend "
                    } else {
                        " commit "
                    },
                    vec![
                        Line::from(format!("{staged} file(s) staged")),
                        Line::from(""),
                        Line::from(Span::styled(
                            format!("{message}▏"),
                            Style::default().fg(Color::White),
                        )),
                        Line::from(""),
                        Line::from(Span::styled(format!("{len}/50"), guide)),
                        Line::from(Span::styled(
                            "Enter commit · Ctrl+A amend · Esc cancel",
                            Style::default().fg(Color::DarkGray),
                        )),
                    ],
                    9,
                )
            }
            Modal::Find { query } => (
                " find ",
                vec![
                    Line::from(format!("{query}▏")),
                    Line::from(Span::styled(
                        "Enter jump · Esc cancel",
                        Style::default().fg(Color::DarkGray),
                    )),
                ],
                5,
            ),
        };

        let w = (area.width * 3 / 4).clamp(20, 76);
        let h = height.min(area.height.saturating_sub(2)).max(3);
        let rect = Rect::new(
            area.x + (area.width.saturating_sub(w)) / 2,
            area.y + (area.height.saturating_sub(h)) / 2,
            w,
            h,
        );
        f.render_widget(Clear, rect);
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), inner);
    }
}

/// Lay out the timeline's selection marker: an arrow at `col`, with `label`
/// beside it on whichever side has room.
///
/// The label prefers the right, because reading left-to-right that puts the
/// arrow first and the text after it. It flips to the left when the right edge
/// is too close, and is dropped entirely when neither side fits — an arrow with
/// no label still says where the selection is, whereas a truncated label says
/// nothing useful.
fn marker_line(width: usize, col: usize, label: &str) -> String {
    let mut line = vec![' '; width];
    if width == 0 {
        return String::new();
    }
    let col = col.min(width - 1);
    line[col] = '▲';

    if !label.is_empty() {
        let n = label.chars().count();
        // One space between the arrow and the text on either side.
        let right_start = col + 2;
        let fits_right = right_start + n <= width;
        // On the left the label ends one cell before the arrow.
        let fits_left = col >= n + 1;

        let start = if fits_right {
            Some(right_start)
        } else if fits_left {
            Some(col - 1 - n)
        } else {
            None
        };
        if let Some(start) = start {
            for (i, ch) in label.chars().enumerate() {
                line[start + i] = ch;
            }
        }
    }
    line.into_iter().collect()
}

/// Render a duration in seconds as the coarsest unit that still says
/// something: "3 days ago" is more use than "271,442 seconds ago".
fn human_age(secs: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    let s = secs.max(0);
    let (n, unit) = match s {
        _ if s < MIN => (s, "second"),
        _ if s < HOUR => (s / MIN, "minute"),
        _ if s < DAY => (s / HOUR, "hour"),
        _ if s < MONTH => (s / DAY, "day"),
        _ if s < YEAR => (s / MONTH, "month"),
        _ => (s / YEAR, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

const HELP: &str = "\
mouse move   hover: highlight + diff
click        pin the hovered file
space        stage / unstage the target
a            stage everything under the hovered directory
c            commit prompt
n / p        next / previous changed file
/            find a file by name
j / k        scroll the diff, or move the log / churn selection
Ctrl+D / U   scroll the commit diff in the log view
Tab          cycle view: changes → age → churn → history
Shift+Tab    cycle the other way
1 … 4        jump straight to a view (or click its tab)
| / -        force vertical / horizontal split
y            show the hovered path, or the commit SHA in the log view
u            undo the last staging action
Esc          unpin
q            quit

Hold Shift to use the terminal's own text selection.";

// --- channel plumbing ----------------------------------------------------

enum Either {
    Event(Event),
    Msg(Message),
}

/// Wait for either channel, with a timeout.
///
/// `std::sync::mpsc` has no select, and pulling in a crate for one function is
/// not worth it: a short poll loop is indistinguishable at these timescales and
/// keeps the dependency list honest.
fn crossbeam_select(
    events: &Receiver<Event>,
    msgs: &Receiver<Message>,
    timeout: Duration,
    mut f: impl FnMut(Either),
) {
    let start = Instant::now();
    loop {
        if let Ok(e) = events.try_recv() {
            f(Either::Event(e));
            return;
        }
        if let Ok(m) = msgs.try_recv() {
            f(Either::Msg(m));
            return;
        }
        if start.elapsed() >= timeout {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::TreeEntry;
    use std::path::Path;
    use std::sync::mpsc;

    fn app() -> (App, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::channel();
        (App::new(tx), rx)
    }

    fn with_files(a: &mut App, files: &[(&str, u64)]) {
        let pairs: Vec<(PathBuf, u64)> =
            files.iter().map(|(p, s)| (PathBuf::from(*p), *s)).collect();
        a.tree = Tree::build(&pairs, Scale::Linear);
    }

    fn set_status(a: &mut App, path: &str, staged: Change, unstaged: Change) {
        let p = PathBuf::from(path);
        a.status.insert(
            p.clone(),
            FileStatus {
                path: p,
                staged,
                unstaged,
            },
        );
        a.data.status = a.status.clone();
    }

    #[test]
    fn space_stages_an_unstaged_file() {
        let (mut a, rx) = app();
        with_files(&mut a, &[("x.rs", 10)]);
        set_status(&mut a, "x.rs", Change::None, Change::Modified);
        a.hovered = a.tree.find(std::path::Path::new("x.rs"));

        a.toggle_stage();
        match rx.try_recv().unwrap() {
            Request::Stage(p) => assert_eq!(p, PathBuf::from("x.rs")),
            _ => panic!("expected a stage request"),
        }
    }

    #[test]
    fn space_unstages_a_staged_file() {
        let (mut a, rx) = app();
        with_files(&mut a, &[("x.rs", 10)]);
        set_status(&mut a, "x.rs", Change::Modified, Change::None);
        a.hovered = a.tree.find(std::path::Path::new("x.rs"));

        a.toggle_stage();
        assert!(matches!(rx.try_recv().unwrap(), Request::Unstage(_)));
    }

    #[test]
    fn undo_reverses_the_last_staging_action() {
        let (mut a, rx) = app();
        with_files(&mut a, &[("x.rs", 10)]);
        set_status(&mut a, "x.rs", Change::None, Change::Modified);
        a.hovered = a.tree.find(std::path::Path::new("x.rs"));

        a.toggle_stage();
        let _ = rx.try_recv();
        a.undo_last();
        // It was unstaged before, so undo puts it back to unstaged.
        assert!(matches!(rx.try_recv().unwrap(), Request::Unstage(_)));
    }

    #[test]
    fn stale_diff_results_are_dropped() {
        let (mut a, _rx) = app();
        a.diff_seq = 7;
        a.on_message(Message::Diff {
            path: PathBuf::from("old.rs"),
            diff: Diff::default(),
            seq: 3,
        });
        assert!(a.diff.is_none(), "a stale diff must not land");

        a.on_message(Message::Diff {
            path: PathBuf::from("cur.rs"),
            diff: Diff::default(),
            seq: 7,
        });
        assert!(a.diff.is_some());
    }

    #[test]
    fn staging_a_directory_collects_only_changed_files() {
        let (mut a, rx) = app();
        with_files(
            &mut a,
            &[("src/a.rs", 10), ("src/b.rs", 10), ("src/c.rs", 10)],
        );
        set_status(&mut a, "src/a.rs", Change::None, Change::Modified);
        set_status(&mut a, "src/c.rs", Change::None, Change::Modified);
        a.hovered = a.tree.find(std::path::Path::new("src"));

        a.stage_under_cursor();
        match rx.try_recv().unwrap() {
            Request::StageAll(paths) => {
                assert_eq!(paths.len(), 2, "unchanged files must not be staged");
                assert!(paths.contains(&PathBuf::from("src/a.rs")));
                assert!(paths.contains(&PathBuf::from("src/c.rs")));
            }
            _ => panic!("expected StageAll"),
        }
    }

    #[test]
    fn n_and_p_cycle_changed_files_in_path_order() {
        let (mut a, _rx) = app();
        with_files(&mut a, &[("a.rs", 10), ("b.rs", 10), ("c.rs", 10)]);
        set_status(&mut a, "a.rs", Change::None, Change::Modified);
        set_status(&mut a, "c.rs", Change::None, Change::Modified);

        a.step_changed(1);
        assert_eq!(a.tree.node(a.hovered.unwrap()).path, PathBuf::from("a.rs"));
        a.step_changed(1);
        assert_eq!(a.tree.node(a.hovered.unwrap()).path, PathBuf::from("c.rs"));
        // Wraps rather than stopping at the end.
        a.step_changed(1);
        assert_eq!(a.tree.node(a.hovered.unwrap()).path, PathBuf::from("a.rs"));
        a.step_changed(-1);
        assert_eq!(a.tree.node(a.hovered.unwrap()).path, PathBuf::from("c.rs"));
    }

    #[test]
    fn find_jumps_to_a_matching_path() {
        let (mut a, _rx) = app();
        with_files(&mut a, &[("src/parser.rs", 10), ("src/main.rs", 10)]);
        a.find("parse");
        assert_eq!(
            a.tree.node(a.hovered.unwrap()).path,
            PathBuf::from("src/parser.rs")
        );

        a.find("nonexistent");
        assert!(a.message.contains("no match"));
    }

    #[test]
    fn pinned_target_wins_over_hover() {
        let (mut a, _rx) = app();
        with_files(&mut a, &[("a.rs", 10), ("b.rs", 10)]);
        let (x, y) = (
            a.tree.find(std::path::Path::new("a.rs")),
            a.tree.find(std::path::Path::new("b.rs")),
        );
        a.hovered = x;
        assert_eq!(a.target(), x);
        a.pinned = y;
        assert_eq!(a.target(), y, "pinning must stop the diff following hover");
    }

    #[test]
    fn split_follows_aspect_ratio() {
        let (a, _rx) = app();
        // Wide terminal: map left, diff right.
        let (m, s, _) = a.split_areas(Rect::new(0, 0, 200, 55));
        assert!(m.x < s.x && m.y == s.y, "wide should split vertically");
        // Tall terminal: stacked.
        let (m, s, _) = a.split_areas(Rect::new(0, 0, 80, 60));
        assert!(m.y < s.y, "tall should split horizontally");
    }

    fn commits(n: usize) -> Vec<CommitMeta> {
        (0..n)
            .map(|i| CommitMeta {
                oid: format!("{:040x}", i),
                time: 1_700_000_000 + i as i64 * 86_400,
                author: "t".into(),
                subject: format!("commit {i}"),
            })
            .collect()
    }

    #[test]
    fn pump_returns_promptly_under_a_motion_flood() {
        // Regression: the drain loop used to sleep and rescan while any event
        // had arrived, so a mouse producing motion faster than the coalescing
        // window kept it fed forever and no frame was ever drawn. On a large
        // repository that presented as the whole UI freezing.
        let (mut a, _rx) = app();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let (_m_tx, m_rx) = mpsc::channel::<Message>();

        let producer = std::thread::spawn(move || {
            for i in 0..5_000u32 {
                let e = Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: (i % 80) as u16,
                    row: (i % 20) as u16,
                    modifiers: KeyModifiers::NONE,
                });
                if ev_tx.send(e).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        std::thread::sleep(Duration::from_millis(20));
        let t = Instant::now();
        a.pump(&ev_rx, &m_rx).unwrap();
        let took = t.elapsed();
        drop(m_rx);
        drop(ev_rx);
        let _ = producer.join();

        assert!(
            took < Duration::from_millis(500),
            "pump took {took:?}; the UI cannot draw while it is inside pump"
        );
    }

    #[test]
    fn status_refresh_is_not_quadratic_in_tree_size() {
        // Regression: merging untracked and newly-added paths did a linear
        // `find` per path over every node, so a repository with tens of
        // thousands of files and thousands of staged additions spent minutes
        // inside a single status message.
        let (mut a, _rx) = app();
        let files: Vec<(PathBuf, u64)> = (0..20_000)
            .map(|i| (PathBuf::from(format!("src/d{}/f{i}.rs", i % 100)), 100))
            .collect();
        a.tree = Tree::build(&files, Scale::Linear);

        let status: Vec<FileStatus> = (0..4_000)
            .map(|i| FileStatus {
                path: PathBuf::from(format!("new/d{}/n{i}.rs", i % 50)),
                staged: Change::Added,
                unstaged: Change::None,
            })
            .collect();

        let t = Instant::now();
        a.on_message(Message::Status {
            files: status,
            head: HeadInfo {
                branch: Some("main".into()),
                oid: None,
                state: crate::git::RepoState::Clean,
            },
        });
        let took = t.elapsed();
        assert!(
            took < Duration::from_secs(2),
            "status refresh took {took:?} on a 20k-file tree"
        );
    }

    #[test]
    fn marker_puts_the_label_to_the_right_when_it_fits() {
        let l = marker_line(30, 2, "3 days ago");
        assert_eq!(l, "  ▲ 3 days ago                ");
        assert_eq!(l.chars().count(), 30, "line must fill the width");
    }

    #[test]
    fn marker_flips_the_label_left_near_the_right_edge() {
        // At the far right there is no room after the arrow, so the label has
        // to go before it rather than being cut off.
        let l = marker_line(20, 18, "3 days ago");
        assert_eq!(l, "       3 days ago ▲ ");
        assert!(
            l.contains("3 days ago"),
            "label was dropped despite fitting"
        );
        assert_eq!(l.chars().count(), 20);
    }

    #[test]
    fn marker_keeps_the_arrow_when_the_label_cannot_fit() {
        // Narrow pane: an unlabelled arrow still says where the selection is,
        // which beats a truncated label that says nothing.
        let l = marker_line(8, 4, "11 months ago");
        assert_eq!(l, "    ▲   ");
        assert!(l.contains('▲'));
    }

    #[test]
    fn marker_handles_the_extremes() {
        // Column 0 and the last column must not panic or spill.
        for (w, col) in [(1usize, 0usize), (10, 0), (10, 9), (40, 39)] {
            let l = marker_line(w, col, "2 hours ago");
            assert_eq!(l.chars().count(), w, "width {w} col {col}");
            assert!(l.contains('▲'), "arrow lost at width {w} col {col}");
        }
        assert_eq!(marker_line(0, 0, "x"), "");
        // An out-of-range column clamps rather than panicking.
        let l = marker_line(5, 99, "x");
        assert_eq!(l.chars().count(), 5);
        assert!(l.ends_with('▲'));
    }

    #[test]
    fn marker_label_never_overwrites_the_arrow() {
        for col in 0..30usize {
            let l = marker_line(30, col, "5 days ago");
            assert_eq!(
                l.chars().filter(|&c| c == '▲').count(),
                1,
                "arrow clobbered at col {col}: {l:?}"
            );
        }
    }

    #[test]
    fn human_age_picks_a_sensible_unit() {
        assert_eq!(human_age(30), "30 seconds ago");
        assert_eq!(human_age(60), "1 minute ago");
        assert_eq!(human_age(3 * 3600), "3 hours ago");
        assert_eq!(human_age(2 * 86_400), "2 days ago");
        assert_eq!(human_age(60 * 86_400), "2 months ago");
        assert_eq!(human_age(800 * 86_400), "2 years ago");
        // Never negative, whatever clock skew produces.
        assert_eq!(human_age(-5), "0 seconds ago");
    }

    #[test]
    fn every_view_has_a_label_and_a_blurb() {
        // The tab bar is what makes the views discoverable, so each needs a
        // short name and a line saying what it actually shows.
        for v in View::ALL {
            assert!(!v.label().is_empty());
            assert!(v.blurb().len() > 10, "{:?} has no useful blurb", v);
        }
        // Labels must be distinct or the tabs are ambiguous.
        let mut seen: Vec<&str> = View::ALL.iter().map(|v| v.label()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), View::ALL.len());
    }

    #[test]
    fn digits_jump_straight_to_a_view() {
        let (mut a, _rx) = app();
        a.on_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(a.view, View::Log);
        a.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert_eq!(a.view, View::Churn);
        a.on_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(a.view, View::Status);
        a.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(a.view, View::Heatmap);
    }

    #[test]
    fn opening_the_heatmap_requests_history_once() {
        let (mut a, rx) = app();
        a.view = View::Heatmap;
        a.on_view_changed();
        assert!(
            matches!(rx.try_recv(), Ok(Request::History { .. })),
            "the heatmap needs its history walk"
        );
        // Going away and coming back must not re-walk.
        a.view = View::Status;
        a.on_view_changed();
        a.view = View::Heatmap;
        a.on_view_changed();
        assert!(rx.try_recv().is_err(), "history should be fetched once");
    }

    #[test]
    fn history_fills_age_and_churn() {
        let (mut a, _rx) = app();
        a.on_message(Message::History {
            entries: vec![
                (PathBuf::from("a.rs"), 1_700_000_000, 4),
                (PathBuf::from("b.rs"), 1_600_000_000, 1),
            ],
            commits: 5,
            done: true,
        });
        assert_eq!(a.data.age.len(), 2);
        assert_eq!(a.data.churn[Path::new("a.rs")], 4);
        // Ages are seconds-since, so the older file has the larger value.
        assert!(a.data.age[Path::new("b.rs")] > a.data.age[Path::new("a.rs")]);
    }

    /// A tree plus a history walk, which is what the churn view needs before
    /// any of its navigation means anything.
    fn churn_app() -> (App, mpsc::Receiver<Request>) {
        let (mut a, rx) = app();
        a.on_message(Message::Tree(vec![
            TreeEntry {
                path: PathBuf::from("busy.rs"),
                size: 100,
                loc: None,
            },
            TreeEntry {
                path: PathBuf::from("mid.rs"),
                size: 100,
                loc: None,
            },
            TreeEntry {
                path: PathBuf::from("quiet.rs"),
                size: 100,
                loc: None,
            },
        ]));
        a.on_message(Message::History {
            entries: vec![
                (PathBuf::from("busy.rs"), 1_700_000_000, 9),
                (PathBuf::from("mid.rs"), 1_700_000_000, 5),
                (PathBuf::from("quiet.rs"), 1_700_000_000, 1),
            ],
            commits: 9,
            done: true,
        });
        a.view = View::Churn;
        (a, rx)
    }

    #[test]
    fn churn_ranking_is_most_changed_first() {
        let (a, _rx) = churn_app();
        let r = a.churn_ranking();
        let names: Vec<String> = r.iter().map(|(p, _)| p.display().to_string()).collect();
        assert_eq!(names, ["busy.rs", "mid.rs", "quiet.rs"]);
    }

    #[test]
    fn jk_walks_the_churn_list_and_moves_the_map_highlight() {
        let (mut a, _rx) = churn_app();
        a.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(a.churn_sel, 1);
        // The map must point at the same file the list selected, or the two
        // halves of the view disagree.
        let t = a.target().expect("stepping should target a file");
        assert_eq!(a.tree.node(t).path, PathBuf::from("mid.rs"));

        a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(a.churn_sel, 2);
        let t = a.target().unwrap();
        assert_eq!(a.tree.node(t).path, PathBuf::from("quiet.rs"));

        a.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(a.churn_sel, 1);
        a.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(a.churn_sel, 0);
        let t = a.target().unwrap();
        assert_eq!(a.tree.node(t).path, PathBuf::from("busy.rs"));
    }

    #[test]
    fn churn_selection_wraps_at_both_ends() {
        let (mut a, _rx) = churn_app();
        a.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(a.churn_sel, 2, "k at the top should wrap to the bottom");
        a.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(a.churn_sel, 0, "j at the bottom should wrap to the top");
    }

    #[test]
    fn jk_does_not_scroll_the_diff_in_the_churn_view() {
        let (mut a, _rx) = churn_app();
        a.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(a.diff_scroll, 0, "j drives the list here, not the diff");
    }

    #[test]
    fn entering_churn_keeps_the_file_the_map_was_on() {
        let (mut a, _rx) = churn_app();
        a.view = View::Status;
        let id = a.tree.find(Path::new("quiet.rs")).unwrap();
        a.pinned = Some(id);
        a.view = View::Churn;
        a.on_view_changed();
        assert_eq!(
            a.churn_sel, 2,
            "switching to churn should land on the already-selected file"
        );
    }

    #[test]
    fn hovering_a_block_moves_the_churn_selection() {
        let (mut a, _rx) = churn_app();
        let id = a.tree.find(Path::new("mid.rs")).unwrap();
        a.hovered = Some(id);
        a.sync_churn_sel();
        assert_eq!(a.churn_sel, 1);
    }

    #[test]
    fn clicking_a_tab_switches_view() {
        let (mut a, _rx) = app();
        a.tab_area = Rect::new(0, 0, 40, 1);
        // Tab 1 starts at x=0; tab 2 begins after " 1 changes " plus a space.
        let first = a.tab_at(1, 0);
        assert_eq!(first, Some(View::Status));
        let second_x = View::Status.label().chars().count() as u16 + 5 + 1;
        assert_eq!(a.tab_at(second_x, 0), Some(View::Heatmap));
        // Rows other than the bar are not tabs.
        assert_eq!(a.tab_at(1, 5), None);
    }

    #[test]
    fn tab_cycles_all_views() {
        let (mut a, _rx) = app();
        assert_eq!(a.view, View::Status);
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.view, View::Heatmap);
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.view, View::Churn);
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.view, View::Log);
        a.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.view, View::Status, "Tab should wrap");
    }

    #[test]
    fn shift_tab_cycles_backwards() {
        let (mut a, _rx) = app();
        a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(a.view, View::Log);
        a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(a.view, View::Churn);
        a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(a.view, View::Heatmap);
    }

    #[test]
    fn opening_the_log_requests_it_once() {
        let (mut a, rx) = app();
        a.view = View::Log;
        a.on_view_changed();
        assert!(
            matches!(rx.try_recv(), Ok(Request::Log { .. })),
            "entering the log should fetch it"
        );

        // With the log already loaded, re-entering asks only for the detail of
        // the current selection, not the whole log again.
        a.log = commits(3);
        a.on_view_changed();
        assert!(matches!(rx.try_recv(), Ok(Request::CommitDetail { .. })));
        assert!(rx.try_recv().is_err(), "no second log request");
    }

    #[test]
    fn j_and_k_move_the_log_selection() {
        let (mut a, rx) = app();
        a.view = View::Log;
        a.log = commits(5);
        while rx.try_recv().is_ok() {}

        a.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(a.log_sel, 1);
        assert!(matches!(rx.try_recv(), Ok(Request::CommitDetail { .. })));
        a.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(a.log_sel, 0);
    }

    #[test]
    fn log_selection_clamps_at_both_ends() {
        let (mut a, _rx) = app();
        a.view = View::Log;
        a.log = commits(3);
        a.step_commit(-1);
        assert_eq!(a.log_sel, 0, "should not run off the newest end");
        for _ in 0..10 {
            a.step_commit(1);
        }
        assert_eq!(a.log_sel, 2, "should stop at the oldest commit");
    }

    #[test]
    fn j_still_scrolls_the_diff_outside_the_log() {
        let (mut a, _rx) = app();
        a.view = View::Status;
        a.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(a.diff_scroll, 1);
        assert_eq!(a.log_sel, 0, "status view must not move a log selection");
    }

    #[test]
    fn stale_commit_details_are_dropped() {
        let (mut a, _rx) = app();
        a.detail_seq = 4;
        a.on_message(Message::CommitDetail {
            oid: "old".into(),
            paths: vec![PathBuf::from("stale.rs")],
            diff: Diff::default(),
            seq: 2,
        });
        assert!(
            a.commit_paths.is_empty(),
            "a superseded selection must not repaint the map"
        );
        a.on_message(Message::CommitDetail {
            oid: "cur".into(),
            paths: vec![PathBuf::from("fresh.rs")],
            diff: Diff::default(),
            seq: 4,
        });
        assert!(a.commit_paths.contains(Path::new("fresh.rs")));
    }

    #[test]
    fn leaving_the_log_clears_the_commit_highlight() {
        // Otherwise the status view would stay tinted by a stale selection.
        let (mut a, _rx) = app();
        a.view = View::Log;
        a.data.commit_paths.insert(PathBuf::from("x.rs"));
        a.view = View::Status;
        a.on_view_changed();
        assert!(a.data.commit_paths.is_empty());
    }

    #[test]
    fn y_yanks_the_sha_in_the_log_view() {
        let (mut a, _rx) = app();
        a.view = View::Log;
        a.log = commits(2);
        a.log_sel = 1;
        a.yank();
        assert_eq!(a.message, a.log[1].oid);
    }

    #[test]
    fn diff_pane_is_capped_side_by_side() {
        let (a, _rx) = app();
        // Very wide terminal: half would be 150 columns, so the cap bites and
        // the surplus goes to the map.
        let (m, d, _) = a.split_areas(Rect::new(0, 0, 300, 60));
        assert_eq!(d.width, DIFF_MAX_COLS);
        assert_eq!(m.width + d.width, 300, "panes must still tile the row");
        assert!(m.width > d.width, "the map should get the surplus");
    }

    #[test]
    fn diff_pane_takes_half_when_under_the_cap() {
        let (a, _rx) = app();
        // 120 columns: half is 60, under the cap, so nothing is clamped.
        let (m, d, _) = a.split_areas(Rect::new(0, 0, 120, 40));
        assert_eq!(d.width, 60);
        assert_eq!(m.width, 60);
    }

    #[test]
    fn cap_leaves_the_diff_usable_at_exactly_the_threshold() {
        let (a, _rx) = app();
        // Half of 164 is exactly the cap; no rounding surprises either side.
        let (_, d, _) = a.split_areas(Rect::new(0, 0, 164, 40));
        assert_eq!(d.width, DIFF_MAX_COLS);
        let (_, d, _) = a.split_areas(Rect::new(0, 0, 162, 40));
        assert_eq!(d.width, 81);
    }

    #[test]
    fn stacked_split_is_not_capped() {
        // Stacked panes share the full width, so the cap does not apply — it
        // would only shrink the diff for no reason.
        let (mut a, _rx) = app();
        a.split = Split::Horizontal;
        let (m, d, _) = a.split_areas(Rect::new(0, 0, 300, 60));
        assert_eq!(d.width, 300);
        assert_eq!(m.width, 300);
        assert!(m.y < d.y);
    }

    #[test]
    fn narrow_terminal_still_gives_the_map_room() {
        // The cap must never starve the map on a small terminal. Forced
        // side-by-side, because Auto would stack these and the column split
        // would not apply at all.
        for w in [20u16, 40, 60, 80] {
            let (mut a, _rx) = app();
            a.split = Split::Vertical;
            let (m, d, _) = a.split_areas(Rect::new(0, 0, w, 30));
            assert!(m.width >= 1, "map vanished at width {w}");
            assert_eq!(m.width + d.width, w, "panes do not tile at width {w}");
        }
    }

    #[test]
    fn split_override_is_respected() {
        let (mut a, _rx) = app();
        a.split = Split::Horizontal;
        let (m, s, _) = a.split_areas(Rect::new(0, 0, 200, 55));
        assert!(m.y < s.y, "explicit horizontal must override the ratio");
    }

    #[test]
    fn commit_modal_rejects_an_empty_message() {
        let (mut a, rx) = app();
        a.modal = Modal::Commit {
            message: "   ".into(),
            amend: false,
        };
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(a.modal, Modal::Error(_)));
        assert!(rx.try_recv().is_err(), "nothing should have been sent");
    }

    #[test]
    fn commit_modal_sends_the_message() {
        let (mut a, rx) = app();
        a.modal = Modal::Commit {
            message: "fix the thing".into(),
            amend: true,
        };
        a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match rx.try_recv().unwrap() {
            Request::Commit { message, amend } => {
                assert_eq!(message, "fix the thing");
                assert!(amend);
            }
            _ => panic!("expected a commit request"),
        }
    }

    #[test]
    fn modal_swallows_keys_meant_for_the_map() {
        let (mut a, _rx) = app();
        a.modal = Modal::Commit {
            message: String::new(),
            amend: false,
        };
        // `q` must type a character, not quit.
        a.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!a.should_quit);
        match &a.modal {
            Modal::Commit { message, .. } => assert_eq!(message, "q"),
            _ => panic!("modal closed unexpectedly"),
        }
    }

    #[test]
    fn untracked_files_are_added_to_the_tree() {
        let (mut a, _rx) = app();
        with_files(&mut a, &[("tracked.rs", 10)]);
        set_status(&mut a, "brand_new.rs", Change::None, Change::Untracked);
        a.merge_untracked();
        assert!(
            a.tree.find(std::path::Path::new("brand_new.rs")).is_some(),
            "an untracked file must be visible on the map to be stageable"
        );
    }
}
