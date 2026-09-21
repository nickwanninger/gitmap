//! Subprocess-backed git access.
//!
//! Shelling out is the right call for anything that mutates or that depends on
//! user configuration: hooks, filters, `.gitattributes`, sparse-checkout,
//! signing and `core.autocrlf` all just work, and reimplementing that surface
//! is how a tool quietly corrupts someone's workflow.

use super::{CommitMeta, Diff, FileStatus, GitBackend, HeadInfo, RepoState, TreeEntry, parse};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct PorcelainBackend {
    root: PathBuf,
}

impl PorcelainBackend {
    /// Discover the repository containing `start`.
    pub fn discover(start: &Path) -> Result<Self> {
        let out = Command::new("git")
            .current_dir(start)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("failed to run git; is it on PATH?")?;
        if !out.status.success() {
            bail!(
                "not a git repository: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run a read-only git command.
    ///
    /// `GIT_OPTIONAL_LOCKS=0` keeps a background refresh from taking
    /// `index.lock` out from under the user's own shell.
    fn read(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(args)
            .output()
            .with_context(|| format!("git {}", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Run a mutating git command, surfacing stderr on failure so a rejecting
    /// hook is reported rather than swallowed.
    fn write_cmd(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .output()
            .with_context(|| format!("git {}", args.join(" ")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if err.is_empty() {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            } else {
                err
            };
            bail!("{}", msg);
        }
        Ok(out.stdout)
    }

    /// Detect an in-progress operation from the marker files git leaves behind.
    fn repo_state(&self) -> RepoState {
        let git_dir = match self.read(&["rev-parse", "--absolute-git-dir"]) {
            Ok(o) => PathBuf::from(String::from_utf8_lossy(&o).trim().to_string()),
            Err(_) => self.root.join(".git"),
        };
        let has = |p: &str| git_dir.join(p).exists();
        if has("rebase-merge") || has("rebase-apply") {
            RepoState::Rebasing
        } else if has("MERGE_HEAD") {
            RepoState::Merging
        } else if has("CHERRY_PICK_HEAD") {
            RepoState::CherryPicking
        } else if has("REVERT_HEAD") {
            RepoState::Reverting
        } else if has("BISECT_LOG") {
            RepoState::Bisecting
        } else {
            RepoState::Clean
        }
    }
}

/// Render a path for git's command line. Paths after `--` are taken literally,
/// so lossy conversion is the only compromise and only bites on the non-UTF-8
/// paths that exist on Linux.
fn arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl GitBackend for PorcelainBackend {
    fn status(&self) -> Result<Vec<FileStatus>> {
        let out = self.read(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--no-renames",
        ])?;
        Ok(parse::status_v2(&out))
    }

    fn tree_at_head(&self) -> Result<Vec<TreeEntry>> {
        // An unborn HEAD is not an error: a fresh repo has no tree yet.
        match self.read(&["ls-tree", "-r", "-z", "--long", "HEAD"]) {
            Ok(out) => Ok(parse::ls_tree(&out)),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn diff(&self, path: &Path, staged: bool) -> Result<Diff> {
        let p = arg(path);
        let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
        if staged {
            args.push("--cached");
        }
        args.push("--");
        args.push(&p);
        let out = self.read(&args)?;
        let text = String::from_utf8_lossy(&out);
        let diff = parse::unified_diff(&text);

        // An untracked file has no diff against the index at all; show its
        // contents as one synthetic all-added hunk so hovering it is useful.
        if diff.hunks.is_empty() && !diff.binary && !staged {
            let abs = self.root.join(path);
            if let Ok(content) = std::fs::read(&abs)
                && let Ok(text) = String::from_utf8(content)
            {
                let lines: Vec<_> = text
                    .lines()
                    .map(|l| super::DiffLine {
                        kind: super::LineKind::Added,
                        text: l.to_string(),
                    })
                    .collect();
                if !lines.is_empty() {
                    return Ok(Diff {
                        hunks: vec![super::Hunk {
                            header: format!("@@ +1,{} @@ (untracked)", lines.len()),
                            lines,
                        }],
                        binary: false,
                    });
                }
            }
        }
        Ok(diff)
    }

    fn head(&self) -> Result<HeadInfo> {
        let out = self.read(&["status", "--porcelain=v2", "-z", "--branch", "-uno"])?;
        let (branch, oid) = parse::head_from_status(&out);
        Ok(HeadInfo {
            branch,
            oid,
            state: self.repo_state(),
        })
    }

    fn log(&self, limit: usize) -> Result<Vec<CommitMeta>> {
        let n = limit.to_string();
        let out = self.read(&["log", "--format=%H%x00%at%x00%an%x00%s", "-z", "-n", &n])?;
        Ok(parse::log(&out))
    }

    fn paths_in_commit(&self, oid: &str) -> Result<Vec<PathBuf>> {
        // `--name-only --format=` with -z gives a bare NUL-separated path list.
        // A merge commit reports nothing by default, which is the right answer
        // here: its own diff against the first parent is what the log shows,
        // and `git show` on a merge is empty unless asked for a combined diff.
        let out = self.read(&["show", "--name-only", "--format=", "-z", oid])?;
        Ok(out
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(parse::path_from_bytes)
            .collect())
    }

    fn commit_diff(&self, oid: &str) -> Result<Diff> {
        let out = self.read(&["show", "--no-color", "--no-ext-diff", "--format=", oid])?;
        Ok(parse::unified_diff(&String::from_utf8_lossy(&out)))
    }

    fn history(&self, limit: usize) -> Result<Vec<(PathBuf, i64, u32)>> {
        // `--no-renames` keeps the parse simple; rename detection changes the
        // map's semantics and is a deliberate later decision.
        let n = limit.to_string();
        let out = self.read(&[
            "log",
            "--format=%H%x00%at",
            "--name-only",
            "-z",
            "--no-renames",
            "-n",
            &n,
        ])?;
        Ok(parse::history_walk(&out))
    }

    fn stage(&self, path: &Path) -> Result<()> {
        // `-A` so a deletion stages as a deletion rather than being ignored.
        self.write_cmd(&["add", "-A", "--", &arg(path)])?;
        Ok(())
    }

    fn unstage(&self, path: &Path) -> Result<()> {
        // `restore --staged` fails on an unborn HEAD, where `rm --cached` is
        // the only way to take something back out of the index.
        let p = arg(path);
        if self.write_cmd(&["restore", "--staged", "--", &p]).is_err() {
            self.write_cmd(&["rm", "--cached", "-q", "--", &p])?;
        }
        Ok(())
    }

    fn commit(&self, msg: &str, amend: bool) -> Result<String> {
        use std::io::Write;

        // `-F -` on stdin avoids every shell-quoting problem.
        let mut args = vec!["commit", "-F", "-"];
        if amend {
            args.push("--amend");
        }
        let mut child = Command::new("git")
            .current_dir(&self.root)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("git commit")?;
        child
            .stdin
            .take()
            .context("commit stdin")?
            .write_all(msg.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            // A failing pre-commit hook writes to stdout as often as stderr.
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let sout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let msg = [err, sout]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "{}",
                if msg.is_empty() {
                    "commit failed".into()
                } else {
                    msg
                }
            );
        }
        let oid = self.read(&["rev-parse", "HEAD"])?;
        Ok(String::from_utf8_lossy(&oid).trim().to_string())
    }
}
