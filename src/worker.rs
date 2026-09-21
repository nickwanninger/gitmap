//! The git worker thread.
//!
//! One thread consuming a request queue. Serialising git operations avoids
//! concurrent `index.lock` contention, and the main thread never blocks on I/O
//! — that single invariant is what keeps the UI responsive.

use crate::git::{CommitMeta, Diff, FileStatus, GitBackend, HeadInfo, TreeEntry};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

pub enum Request {
    Status,
    Tree,
    /// Tagged with a sequence number so results for a stale hover are dropped.
    Diff {
        path: PathBuf,
        staged: bool,
        seq: u64,
    },
    Stage(PathBuf),
    Unstage(PathBuf),
    StageAll(Vec<PathBuf>),
    Commit {
        message: String,
        amend: bool,
    },
    /// Commit metadata for the log view.
    Log {
        limit: usize,
    },
    /// The history walk behind the heatmap and churn views. Unbounded and
    /// streamed; results arrive as a series of `History` messages.
    History,
    /// Which files a commit touched, plus its diff for the pane. Tagged with a
    /// sequence number so a result for a selection the user has already moved
    /// past is dropped, exactly as hover diffs are.
    CommitDetail {
        oid: String,
        seq: u64,
    },
    Quit,
}

pub enum Message {
    Status {
        files: Vec<FileStatus>,
        head: HeadInfo,
    },
    Tree(Vec<TreeEntry>),
    Diff {
        path: PathBuf,
        diff: Diff,
        seq: u64,
    },
    Log(Vec<CommitMeta>),
    /// One instalment of the streaming history walk. Cumulative: each message
    /// supersedes the last rather than adding to it.
    History {
        entries: Vec<(std::path::PathBuf, i64, u32)>,
        commits: usize,
        done: bool,
    },
    CommitDetail {
        oid: String,
        paths: Vec<std::path::PathBuf>,
        diff: Diff,
        seq: u64,
    },
    /// A staging or commit operation finished; the app refreshes status.
    Done(String),
    Error(String),
}

pub fn spawn(
    backend: Arc<dyn GitBackend>,
    rx: Receiver<Request>,
    tx: Sender<Message>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for req in rx {
            let out = handle(&*backend, req, &tx);
            match out {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    let _ = tx.send(Message::Error(format!("{e:#}")));
                }
            }
        }
    })
}

/// Returns `Ok(false)` to stop the worker.
fn handle(backend: &dyn GitBackend, req: Request, tx: &Sender<Message>) -> anyhow::Result<bool> {
    match req {
        Request::Quit => return Ok(false),
        Request::Status => {
            let files = backend.status()?;
            let head = backend.head()?;
            let _ = tx.send(Message::Status { files, head });
        }
        Request::Tree => {
            let _ = tx.send(Message::Tree(backend.tree_at_head()?));
        }
        Request::Diff { path, staged, seq } => {
            let diff = backend.diff(&path, staged)?;
            let _ = tx.send(Message::Diff { path, diff, seq });
        }
        Request::Stage(p) => {
            backend.stage(&p)?;
            let _ = tx.send(Message::Done(format!("staged {}", p.display())));
        }
        Request::Unstage(p) => {
            backend.unstage(&p)?;
            let _ = tx.send(Message::Done(format!("unstaged {}", p.display())));
        }
        Request::StageAll(paths) => {
            let n = paths.len();
            for p in &paths {
                backend.stage(p)?;
            }
            let _ = tx.send(Message::Done(format!("staged {n} files")));
        }
        Request::Log { limit } => {
            let _ = tx.send(Message::Log(backend.log(limit)?));
        }
        Request::History => {
            // Send each chunk onward; stop the walk if the app has gone away,
            // so quitting mid-walk does not leave git running.
            let mut alive = true;
            backend.history_stream(&mut |c| {
                alive = tx
                    .send(Message::History {
                        entries: c.entries,
                        commits: c.commits,
                        done: c.done,
                    })
                    .is_ok();
                alive
            })?;
        }
        Request::CommitDetail { oid, seq } => {
            let paths = backend.paths_in_commit(&oid)?;
            let diff = backend.commit_diff(&oid)?;
            let _ = tx.send(Message::CommitDetail {
                oid,
                paths,
                diff,
                seq,
            });
        }
        Request::Commit { message, amend } => {
            let oid = backend.commit(&message, amend)?;
            let short = &oid[..oid.len().min(8)];
            let _ = tx.send(Message::Done(format!("committed {short}")));
        }
    }
    Ok(true)
}
