//! Git access behind a single trait, so the porcelain and library
//! implementations stay swappable and testable against a fixture repo.

pub mod parse;
pub mod porcelain;

use anyhow::Result;
use std::path::{Path, PathBuf};

pub use porcelain::PorcelainBackend;

/// How a path differs from HEAD, in the index and in the worktree.
///
/// Kept separate rather than collapsed into one state because staging is a
/// per-side fact: a file can be both staged and modified again on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    Added,
    Modified,
    Deleted,
    Untracked,
    Conflicted,
}

impl Change {
    /// Map a single `git status --porcelain=v2` XY code point.
    pub fn from_code(c: u8) -> Change {
        match c {
            b'A' => Change::Added,
            b'M' | b'T' => Change::Modified,
            b'D' => Change::Deleted,
            b'U' => Change::Conflicted,
            _ => Change::None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileStatus {
    pub path: PathBuf,
    /// Index vs HEAD — the staged side.
    pub staged: Change,
    /// Worktree vs index — the unstaged side.
    pub unstaged: Change,
}

impl FileStatus {
    pub fn is_staged(&self) -> bool {
        self.staged != Change::None
    }

    /// The change worth colouring. The unstaged side wins when both are
    /// present, since that is the edit the user is currently making.
    pub fn dominant(&self) -> Change {
        if self.unstaged != Change::None {
            self.unstaged
        } else {
            self.staged
        }
    }
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub size: u64,
    /// Lines of code, counted lazily from the worktree. `None` until measured.
    pub loc: Option<u64>,
}

/// A parsed unified diff. Hunk boundaries are first-class from the start so
/// hunk-level staging can be added later without reworking the pane.
#[derive(Debug, Clone, Default)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    /// True when git reported the file as binary rather than emitting text.
    pub binary: bool,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`
    Meta,
}

#[derive(Debug, Clone)]
pub struct CommitMeta {
    pub oid: String,
    pub time: i64,
    pub author: String,
    pub subject: String,
}

/// What the repository is in the middle of, if anything. Surfaced in the status
/// bar, because otherwise the tool lies about what committing will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoState {
    Clean,
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
    Bisecting,
}

impl RepoState {
    pub fn label(&self) -> Option<&'static str> {
        match self {
            RepoState::Clean => None,
            RepoState::Merging => Some("MERGING"),
            RepoState::Rebasing => Some("REBASING"),
            RepoState::CherryPicking => Some("CHERRY-PICKING"),
            RepoState::Reverting => Some("REVERTING"),
            RepoState::Bisecting => Some("BISECTING"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeadInfo {
    /// Branch name, or `None` when detached.
    pub branch: Option<String>,
    pub oid: Option<String>,
    pub state: RepoState,
}

impl HeadInfo {
    pub fn label(&self) -> String {
        match (&self.branch, &self.oid) {
            (Some(b), _) => b.clone(),
            (None, Some(oid)) => format!("detached @ {}", &oid[..oid.len().min(8)]),
            (None, None) => "no commits".to_string(),
        }
    }
}

/// One instalment of a streaming history walk.
pub struct HistoryChunk {
    /// Per path: last touch time, and commits touching it so far. Cumulative —
    /// each chunk supersedes the last rather than adding to it.
    pub entries: Vec<(PathBuf, i64, u32)>,
    /// Commits walked so far, for the progress line.
    pub commits: usize,
    /// Whether this is the final chunk. Until it is, counts are "commits in
    /// the most recent N", because git walks newest first.
    pub done: bool,
}

pub trait GitBackend: Send + Sync {
    fn status(&self) -> Result<Vec<FileStatus>>;
    fn tree_at_head(&self) -> Result<Vec<TreeEntry>>;
    fn diff(&self, path: &Path, staged: bool) -> Result<Diff>;
    fn head(&self) -> Result<HeadInfo>;
    fn log(&self, limit: usize) -> Result<Vec<CommitMeta>>;
    /// Paths a commit touched, for colouring the map in the log view.
    fn paths_in_commit(&self, oid: &str) -> Result<Vec<PathBuf>>;
    /// The full diff of a commit against its first parent.
    fn commit_diff(&self, oid: &str) -> Result<Diff>;
    /// One history walk: per path, when it was last touched and by how many
    /// commits. Feeds the heatmap.
    fn history(&self, limit: usize) -> Result<Vec<(PathBuf, i64, u32)>>;

    /// The same walk, unbounded, delivered in chunks as git produces them.
    ///
    /// `on_chunk` is called with everything parsed so far plus the number of
    /// commits seen, and returns false to abort the walk. Streaming is what
    /// lets the walk be unbounded: a full walk of a 27k-commit repository takes
    /// over two seconds, but the first counts land in about 25ms, so the map
    /// can start painting immediately and refine as the rest arrives.
    ///
    /// The default implementation falls back to one bounded `history` call, so
    /// a backend that cannot stream still works.
    fn history_stream(
        &self,
        on_chunk: &mut dyn FnMut(HistoryChunk) -> bool,
    ) -> Result<()> {
        let entries = self.history(2000)?;
        on_chunk(HistoryChunk {
            entries,
            commits: 0,
            done: true,
        });
        Ok(())
    }

    fn stage(&self, path: &Path) -> Result<()>;
    fn unstage(&self, path: &Path) -> Result<()>;
    fn commit(&self, msg: &str, amend: bool) -> Result<String>;
}
