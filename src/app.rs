//! Application state, event loop and drawing.
//!
//! The main thread owns all state and performs no blocking I/O. It selects on
//! one channel, drains everything available, collapses redundant events, and
//! draws once — event-driven rather than a fixed-rate loop, which matters for
//! battery and for SSH.

use crate::git::{Change, Diff, FileStatus, HeadInfo};
use crate::input::hit::HitBuffer;
use crate::layout::tree::{NodeId, Scale, Tree};
use crate::layout::treemap::{self, Layout, Rect as PxRect};
use crate::render::canvas::Canvas;
use crate::render::palette::{ColorDepth, StatusPalette};
use crate::render::{diff as diffpane, map};
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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

/// How long the cursor must hold still before a diff is fetched. Without this,
/// sweeping across a directory spawns a diff per file and the pane thrashes.
const DIFF_DEBOUNCE: Duration = Duration::from_millis(80);

/// Coalescing window: a burst of motion events produces one frame.
const COALESCE: Duration = Duration::from_millis(8);

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
        } else {
            Duration::from_millis(250)
        };

        crossbeam_select(events, msgs, timeout, |ev| match ev {
            Either::Event(e) => {
                got_anything = true;
                if let Event::Mouse(m) = &e {
                    if matches!(m.kind, MouseEventKind::Moved) {
                        latest_motion = Some(*m);
                        return;
                    }
                }
                self.on_event(e);
            }
            Either::Msg(m) => {
                got_anything = true;
                self.on_message(m);
            }
        });

        // Drain whatever else piled up while we were waiting.
        loop {
            let mut progressed = false;
            while let Ok(e) = events.try_recv() {
                progressed = true;
                got_anything = true;
                if let Event::Mouse(m) = &e {
                    if matches!(m.kind, MouseEventKind::Moved) {
                        latest_motion = Some(*m);
                        continue;
                    }
                }
                self.on_event(e);
            }
            while let Ok(m) = msgs.try_recv() {
                progressed = true;
                got_anything = true;
                self.on_message(m);
            }
            if !progressed {
                break;
            }
            // Small coalescing window so a burst becomes one frame.
            std::thread::sleep(COALESCE);
        }

        if let Some(m) = latest_motion {
            self.on_motion(m);
        }
        let _ = got_anything;

        // Fire the debounced diff once the cursor has been still long enough.
        if let Some(t) = self.pending_hover {
            if t.elapsed() >= DIFF_DEBOUNCE {
                self.pending_hover = None;
                self.request_diff();
            }
        }
        Ok(())
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
                            let _ = self.tx.send(Request::Commit { message: msg, amend });
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
            KeyCode::Char(' ') => self.toggle_stage(),
            KeyCode::Char('a') => self.stage_under_cursor(),
            KeyCode::Char('c') => {
                self.modal = Modal::Commit {
                    message: String::new(),
                    amend: false,
                }
            }
            KeyCode::Char('u') => self.undo_last(),
            KeyCode::Tab => {
                self.view = match self.view {
                    View::Status => View::Heatmap,
                    View::Heatmap => View::Status,
                };
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
            KeyCode::Char('j') | KeyCode::Down => self.diff_scroll = self.diff_scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.diff_scroll = self.diff_scroll.saturating_sub(1),
            KeyCode::Char('y') => self.yank_path(),
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
                self.on_motion(m);
                self.pinned = self.hovered;
                self.request_diff();
            }
            MouseEventKind::ScrollDown => {
                self.diff_scroll = self.diff_scroll.saturating_add(3)
            }
            MouseEventKind::ScrollUp => self.diff_scroll = self.diff_scroll.saturating_sub(3),
            _ => {}
        }
    }

    fn on_motion(&mut self, m: MouseEvent) {
        let area = self.map_area;
        if m.column < area.x
            || m.row < area.y
            || m.column >= area.right()
            || m.row >= area.bottom()
        {
            return;
        }
        let id = self.hits.at(m.column - area.x, m.row - area.y);
        if id != self.hovered {
            self.hovered = id;
            self.dirty = true;
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

    fn toggle_stage(&mut self) {
        let Some(id) = self.target() else { return };
        if self.tree.node(id).is_dir {
            self.stage_under_cursor();
            return;
        }
        let path = self.tree.node(id).path.clone();
        let staged = self.status.get(&path).map(|s| s.is_staged()).unwrap_or(false);
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

        let cur = self.target().and_then(|t| changed.iter().position(|&c| c == t));
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

    fn find(&mut self, query: &str) {
        if query.is_empty() {
            return;
        }
        let q = query.to_lowercase();
        let found = self.tree.files_under(self.tree.root).into_iter().find(|&f| {
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

    fn yank_path(&mut self) {
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
        let dir = if vertical {
            Direction::Horizontal
        } else {
            Direction::Vertical
        };
        let parts = TuiLayout::default()
            .direction(dir)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(body);
        (parts[0], parts[1], status)
    }

    fn draw_side(&self, f: &mut Frame, area: Rect) {
        let title = match self.view {
            View::Status => " diff ",
            View::Heatmap => " heatmap ",
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);

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

        spans.push(Span::raw(format!(
            "  ✓{staged} ~{unstaged} +{untracked}  ",
        )));

        if !self.message.is_empty() {
            spans.push(Span::styled(
                format!("{}  ", self.message),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            let hint = match self.target() {
                Some(id) => self.tree.node(id).path.display().to_string(),
                None => "space stage · a stage dir · c commit · Tab view · ? help".into(),
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

const HELP: &str = "\
mouse move   hover: highlight + diff
click        pin the hovered file
space        stage / unstage the target
a            stage everything under the hovered directory
c            commit prompt
n / p        next / previous changed file
/            find a file by name
j / k        scroll the diff
Tab          cycle view: status → heatmap
| / -        force vertical / horizontal split
y            show the hovered path (Shift-drag to select)
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
    use std::sync::mpsc;

    fn app() -> (App, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::channel();
        (App::new(tx), rx)
    }

    fn with_files(a: &mut App, files: &[(&str, u64)]) {
        let pairs: Vec<(PathBuf, u64)> = files
            .iter()
            .map(|(p, s)| (PathBuf::from(*p), *s))
            .collect();
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
