//! Parsers for git's machine-readable output.
//!
//! Everything here is a pure function over bytes so it can be tested against
//! recorded fixtures without a repository. Paths are handled as bytes and only
//! converted at the edge, because non-UTF-8 paths exist on Linux.

use super::{Change, CommitMeta, Diff, DiffLine, FileStatus, Hunk, LineKind, TreeEntry};
use std::path::PathBuf;

/// Build a `PathBuf` from raw bytes without going through `str`.
#[cfg(unix)]
pub fn path_from_bytes(b: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(b))
}

#[cfg(not(unix))]
pub fn path_from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

/// Parse `git status --porcelain=v2 -z --untracked-files=all --no-renames`.
///
/// The `-z` form is NUL-separated with no quoting, so paths containing spaces,
/// newlines or invalid UTF-8 come through intact. Record layouts:
///
/// ```text
/// 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>          ordinary
/// 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path><NUL><orig>
/// u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path> unmerged
/// ? <path>                                               untracked
/// ! <path>                                               ignored
/// ```
///
/// Rename records carry a second NUL-terminated field (the original path) that
/// must be consumed even with `--no-renames`, in case the caller drops the flag.
pub fn status_v2(data: &[u8]) -> Vec<FileStatus> {
    let mut out = Vec::new();
    let mut fields = data.split(|&b| b == 0).filter(|f| !f.is_empty());

    while let Some(rec) = fields.next() {
        match rec[0] {
            b'1' | b'2' => {
                // `1 XY ...` — XY starts at offset 2.
                if rec.len() < 4 {
                    continue;
                }
                let staged = Change::from_code(rec[2]);
                let unstaged = Change::from_code(rec[3]);
                // A rename record's path is the 10th space-separated field;
                // an ordinary record's is the 9th.
                let want = if rec[0] == b'2' { 9 } else { 8 };
                let Some(path) = nth_space_field(rec, want) else {
                    continue;
                };
                if rec[0] == b'2' {
                    // Consume the original path that follows the NUL.
                    let _ = fields.next();
                }
                out.push(FileStatus {
                    path: path_from_bytes(path),
                    staged,
                    unstaged,
                });
            }
            b'u' => {
                let Some(path) = nth_space_field(rec, 10) else {
                    continue;
                };
                out.push(FileStatus {
                    path: path_from_bytes(path),
                    staged: Change::Conflicted,
                    unstaged: Change::Conflicted,
                });
            }
            b'?' => {
                out.push(FileStatus {
                    path: path_from_bytes(&rec[2..]),
                    staged: Change::None,
                    unstaged: Change::Untracked,
                });
            }
            // `!` ignored entries are not requested, and anything else is a
            // header line (`# branch.oid ...`) handled by `head_from_status`.
            _ => {}
        }
    }
    out
}

/// Return the `n`th space-separated field (0-indexed), with the final field
/// taking the whole remainder so paths may contain spaces.
fn nth_space_field(rec: &[u8], n: usize) -> Option<&[u8]> {
    let mut start = 0;
    for _ in 0..n {
        let sp = rec[start..].iter().position(|&b| b == b' ')?;
        start += sp + 1;
    }
    if start >= rec.len() {
        return None;
    }
    Some(&rec[start..])
}

/// Pull branch and OID out of the `# branch.*` headers that
/// `--porcelain=v2 --branch` emits.
pub fn head_from_status(data: &[u8]) -> (Option<String>, Option<String>) {
    let mut branch = None;
    let mut oid = None;
    for rec in data.split(|&b| b == 0) {
        let Ok(s) = std::str::from_utf8(rec) else {
            continue;
        };
        if let Some(v) = s.strip_prefix("# branch.head ") {
            if v != "(detached)" {
                branch = Some(v.to_string());
            }
        } else if let Some(v) = s.strip_prefix("# branch.oid ")
            && v != "(initial)"
        {
            oid = Some(v.to_string());
        }
    }
    (branch, oid)
}

/// Parse `git ls-tree -r -z --long HEAD`.
///
/// Each record is `<mode> SP <type> SP <oid> SP* <size> TAB <path>`. The size
/// column is right-aligned with padding, and is `-` for anything that is not a
/// blob (submodules report as `commit`).
pub fn ls_tree(data: &[u8]) -> Vec<TreeEntry> {
    let mut out = Vec::new();
    for rec in data.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let (meta, path) = (&rec[..tab], &rec[tab + 1..]);
        let Ok(meta) = std::str::from_utf8(meta) else {
            continue;
        };
        let mut cols = meta.split_whitespace();
        let (_mode, kind, _oid) = (cols.next(), cols.next(), cols.next());
        // Skip submodules and symlinks: a submodule is one tree entry we do not
        // recurse into, and a symlink's "size" is its target's length.
        if kind != Some("blob") {
            continue;
        }
        let size = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        out.push(TreeEntry {
            path: path_from_bytes(path),
            size,
            loc: None,
        });
    }
    out
}

/// Parse a unified diff as emitted by `git diff --no-color --no-ext-diff`.
///
/// Everything before the first `@@` is header noise the pane reconstructs from
/// the path it already has, so it is dropped.
pub fn unified_diff(text: &str) -> Diff {
    let mut diff = Diff::default();
    let mut hunk: Option<Hunk> = None;

    for line in text.lines() {
        if line.starts_with("@@") {
            if let Some(h) = hunk.take() {
                diff.hunks.push(h);
            }
            hunk = Some(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        if hunk.is_none() {
            // Header region. `Binary files ... differ` is the one fact worth keeping.
            if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
                diff.binary = true;
            }
            continue;
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b'\\') => (LineKind::Meta, line),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // A totally empty line in a diff body is an empty context line.
            None => (LineKind::Context, line),
            _ => continue,
        };
        hunk.as_mut().unwrap().lines.push(DiffLine {
            kind,
            text: text.to_string(),
        });
    }
    if let Some(h) = hunk {
        diff.hunks.push(h);
    }
    diff
}

/// Parse `git log --format=%H%x00%at%x00%an%x00%s -z`.
///
/// Note that `-z` makes git separate *commits* with NUL as well, so the stream
/// is a flat run of fields four at a time.
pub fn log(data: &[u8]) -> Vec<CommitMeta> {
    let fields: Vec<&[u8]> = data.split(|&b| b == 0).collect();
    let mut out = Vec::new();
    for c in fields.chunks(4) {
        if c.len() < 4 || c[0].is_empty() {
            continue;
        }
        let s = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
        out.push(CommitMeta {
            oid: s(c[0]),
            time: s(c[1]).parse().unwrap_or(0),
            author: s(c[2]),
            subject: s(c[3]),
        });
    }
    out
}

/// Parse `git log --format=%H%x00%at --name-only -z --no-renames`.
///
/// The stream is flat and NUL-separated, but the record boundary is a newline
/// rather than a NUL: git emits `<oid>NUL<time>NUL\n<path>NUL<path>NUL...`, so
/// the newline that `--name-only` puts between the header and the file list
/// arrives attached to the *front* of the commit's first path. Left in place it
/// would make `\nsrc/a.rs` a different key from `src/a.rs`, so the same file
/// would appear twice with its commits split between the two. A commit that
/// touched nothing (an empty commit, or a merge) emits no path field at all.
///
/// Returns, per path, the time it was last touched and how many commits in the
/// walk touched it — the age and churn inputs the heatmap needs.
pub fn history_walk(data: &[u8]) -> Vec<(PathBuf, i64, u32)> {
    use std::collections::HashMap;
    let mut acc: HashMap<PathBuf, (i64, u32)> = HashMap::new();

    let mut fields = data.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut time: i64 = 0;

    while let Some(f) = fields.next() {
        // A bare 40-char hex field starts a commit; its time follows, with the
        // commit's first path glued on after a newline when it has one.
        if looks_like_oid(f) {
            let Some(tf) = fields.next() else { break };
            // Older git glues the first path onto the time field after a
            // newline; newer git emits it as its own field with the newline
            // leading. Handle both rather than depending on the version.
            let (t, first) = match tf.iter().position(|&b| b == b'\n') {
                Some(i) => (&tf[..i], Some(&tf[i + 1..])),
                None => (tf, None),
            };
            time = std::str::from_utf8(t)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            if let Some(p) = first
                && !p.is_empty()
            {
                touch(&mut acc, p, time);
            }
            continue;
        }
        touch(&mut acc, strip_leading_newline(f), time);
    }

    acc.into_iter().map(|(p, (t, n))| (p, t, n)).collect()
}

/// Drop the newline `--name-only` puts before a commit's first path.
///
/// A path cannot begin with a newline in any real tree, so this is unambiguous;
/// leaving it on would split one file across two keys.
fn strip_leading_newline(f: &[u8]) -> &[u8] {
    match f.first() {
        Some(b'\n') => &f[1..],
        _ => f,
    }
}

/// Incremental version of `history_walk`, for a streaming walk.
///
/// Holds the accumulator and the partial trailing field between chunks, so a
/// path split across a read boundary is not counted as two truncated paths.
#[derive(Default)]
pub struct HistoryAccum {
    acc: std::collections::HashMap<PathBuf, (i64, u32)>,
    /// Bytes after the last NUL, which may be half a field.
    partial: Vec<u8>,
    time: i64,
    /// Commits seen, for the progress line.
    pub commits: usize,
}

impl HistoryAccum {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next chunk of git's output.
    ///
    /// Only whole fields are consumed. An oid additionally needs the time field
    /// that follows it, so when a chunk ends between the two, both are held
    /// back: consuming the oid alone would drop the commit's timestamp and then
    /// count the time field as a filename on the next push.
    pub fn push(&mut self, data: &[u8]) {
        self.partial.extend_from_slice(data);
        let Some(last_nul) = self.partial.iter().rposition(|&b| b == 0) else {
            return;
        };

        // Consume complete fields, but stop at a trailing oid whose time has
        // not arrived; `taken` is how far into the buffer we got.
        let mut taken = 0usize;
        let mut start = 0usize;
        let mut pending_oid = false;
        let end = last_nul + 1;
        while start < end {
            let Some(rel) = self.partial[start..end].iter().position(|&b| b == 0) else {
                break;
            };
            let f = &self.partial[start..start + rel];
            let next = start + rel + 1;

            if f.is_empty() {
                start = next;
                taken = next;
                continue;
            }

            if pending_oid {
                // This field is the time, possibly with the first path glued on.
                let (t, first) = match f.iter().position(|&b| b == b'\n') {
                    Some(i) => (&f[..i], Some(&f[i + 1..])),
                    None => (f, None),
                };
                self.time = std::str::from_utf8(t)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(self.time);
                if let Some(p) = first
                    && !p.is_empty()
                {
                    let p = p.to_vec();
                    touch(&mut self.acc, &p, self.time);
                }
                pending_oid = false;
                start = next;
                taken = next;
                continue;
            }

            if looks_like_oid(f) {
                // Hold the oid until its time field is also here.
                self.commits += 1;
                pending_oid = true;
                start = next;
                continue;
            }

            let p = strip_leading_newline(f).to_vec();
            touch(&mut self.acc, &p, self.time);
            start = next;
            taken = next;
        }

        if pending_oid {
            // Un-count it; the next push will see the whole pair.
            self.commits -= 1;
        }
        self.partial.drain(..taken);
    }

    /// Snapshot of the counts so far.
    pub fn entries(&self) -> Vec<(PathBuf, i64, u32)> {
        self.acc
            .iter()
            .map(|(p, &(t, n))| (p.clone(), t, n))
            .collect()
    }
}

/// Record that `path` was touched at `time`, keeping the newest timestamp.
fn touch(acc: &mut std::collections::HashMap<PathBuf, (i64, u32)>, path: &[u8], time: i64) {
    let e = acc.entry(path_from_bytes(path)).or_insert((time, 0));
    // The walk runs newest first, so the first sighting is the latest touch.
    e.0 = e.0.max(time);
    e.1 += 1;
}

/// Whether a field is a bare 40-char hex oid.
fn looks_like_oid(f: &[u8]) -> bool {
    f.len() == 40 && f.iter().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_ordinary_and_untracked() {
        let data = b"1 M. N... 100644 100644 100644 abc def src/main.rs\0\
                     1 .M N... 100644 100644 100644 abc def src/app.rs\0\
                     ? new file.txt\0";
        let st = status_v2(data);
        assert_eq!(st.len(), 3);
        assert_eq!(st[0].path, PathBuf::from("src/main.rs"));
        assert_eq!(st[0].staged, Change::Modified);
        assert_eq!(st[0].unstaged, Change::None);
        assert_eq!(st[1].staged, Change::None);
        assert_eq!(st[1].unstaged, Change::Modified);
        // Spaces in an untracked path survive because -z does not quote.
        assert_eq!(st[2].path, PathBuf::from("new file.txt"));
        assert_eq!(st[2].unstaged, Change::Untracked);
    }

    #[test]
    fn status_path_with_spaces_and_newline() {
        let data = b"1 .M N... 100644 100644 100644 abc def dir/a b\nc.rs\0";
        let st = status_v2(data);
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].path, PathBuf::from("dir/a b\nc.rs"));
    }

    #[test]
    fn status_rename_consumes_original_path() {
        // A rename record is followed by a second NUL-terminated field. If it
        // is not consumed, the original path is misread as the next record.
        let data = b"2 R. N... 100644 100644 100644 abc def R100 new.rs\0old.rs\0\
                     ? after.txt\0";
        let st = status_v2(data);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].path, PathBuf::from("new.rs"));
        assert_eq!(st[1].path, PathBuf::from("after.txt"));
        assert_eq!(st[1].unstaged, Change::Untracked);
    }

    #[test]
    fn status_unmerged() {
        let data = b"u UU N... 100644 100644 100644 100644 a1 a2 a3 conflict.rs\0";
        let st = status_v2(data);
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].path, PathBuf::from("conflict.rs"));
        assert_eq!(st[0].staged, Change::Conflicted);
    }

    #[test]
    fn head_headers() {
        let data = b"# branch.oid 1234abcd\0# branch.head main\0? x\0";
        let (b, o) = head_from_status(data);
        assert_eq!(b.as_deref(), Some("main"));
        assert_eq!(o.as_deref(), Some("1234abcd"));

        let (b, _) = head_from_status(b"# branch.head (detached)\0");
        assert_eq!(b, None);
    }

    #[test]
    fn ls_tree_sizes_and_submodules() {
        let data = b"100644 blob abc123     1024\tsrc/main.rs\0\
                     160000 commit def456       -\tvendor/sub\0\
                     100644 blob 999aaa       12\ta b.txt\0";
        let t = ls_tree(data);
        assert_eq!(t.len(), 2, "submodule entries are skipped");
        assert_eq!(t[0].path, PathBuf::from("src/main.rs"));
        assert_eq!(t[0].size, 1024);
        assert_eq!(t[1].path, PathBuf::from("a b.txt"));
        assert_eq!(t[1].size, 12);
    }

    #[test]
    fn diff_hunks() {
        // `concat!` rather than `\`-continuation: the continuation form strips
        // leading whitespace, which would silently eat a context line's space.
        let text = concat!(
            "diff --git a/x b/x\n",
            "index 111..222 100644\n",
            "--- a/x\n",
            "+++ b/x\n",
            "@@ -1,3 +1,4 @@\n",
            " ctx\n",
            "-gone\n",
            "+new\n",
            "+also\n",
            "@@ -10,2 +11,2 @@ fn f()\n",
            "-a\n",
            "+b\n",
            "\\ No newline at end of file\n",
        );
        let d = unified_diff(text);
        assert_eq!(d.hunks.len(), 2);
        assert!(!d.binary);
        assert_eq!(d.hunks[0].lines.len(), 4);
        assert_eq!(d.hunks[0].lines[0].kind, LineKind::Context);
        assert_eq!(d.hunks[0].lines[1].kind, LineKind::Removed);
        assert_eq!(d.hunks[0].lines[1].text, "gone");
        assert_eq!(d.hunks[1].header, "@@ -10,2 +11,2 @@ fn f()");
        assert_eq!(d.hunks[1].lines[2].kind, LineKind::Meta);
    }

    #[test]
    fn diff_binary() {
        let d =
            unified_diff("diff --git a/i.png b/i.png\nBinary files a/i.png and b/i.png differ\n");
        assert!(d.binary);
        assert!(d.hunks.is_empty());
    }

    #[test]
    fn history_walk_collects_times_and_counts() {
        // The shape git actually emits: the time and the first path share one
        // field, separated by a newline.
        let data = b"\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x002000\nsrc/a.rs\x00src/b.rs\x00\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\x001000\nsrc/a.rs\x00docs/c.md\x00";
        let mut w = history_walk(data);
        w.sort_by(|x, y| x.0.cmp(&y.0));

        assert_eq!(w.len(), 3, "got {w:?}");
        let get = |p: &str| w.iter().find(|e| e.0 == PathBuf::from(p)).unwrap();
        // a.rs was touched by both commits; its age comes from the newest.
        assert_eq!(get("src/a.rs").1, 2000);
        assert_eq!(get("src/a.rs").2, 2, "churn should count both commits");
        assert_eq!(get("src/b.rs").1, 2000);
        assert_eq!(get("src/b.rs").2, 1);
        assert_eq!(get("docs/c.md").1, 1000);
    }

    #[test]
    fn history_walk_handles_the_newline_leading_the_first_path() {
        // Regression: git emits `<oid>NUL<time>NUL\n<first path>NUL...`, so the
        // newline leads the first path rather than sitting inside the time
        // field. Left attached it made "\nsrc/a.rs" a separate key, so the file
        // showed up twice with its commit count split between the two rows.
        let data = b"\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x002000\x00\nsrc/a.rs\x00src/b.rs\x00\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\x001000\x00\nsrc/a.rs\x00";
        let w = history_walk(data);

        assert_eq!(w.len(), 2, "a leading newline must not fork a path: {w:?}");
        let get = |p: &str| w.iter().find(|e| e.0 == PathBuf::from(p)).unwrap();
        assert_eq!(get("src/a.rs").2, 2, "both commits must land on one key");
        assert_eq!(get("src/a.rs").1, 2000);
        assert_eq!(get("src/b.rs").2, 1);
        assert!(
            !w.iter().any(|e| e.0.to_string_lossy().starts_with('\n')),
            "no path may keep its leading newline: {w:?}"
        );
    }

    #[test]
    fn history_accum_matches_the_one_shot_walk_at_every_split() {
        // The streaming walk reads fixed-size blocks, so a NUL-separated field
        // is routinely cut in half. Feeding the same bytes one split at a time
        // must land on the same counts as parsing them whole.
        let data: &[u8] = b"\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x002000\x00\nsrc/a.rs\x00src/b.rs\x00\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\x001000\x00\nsrc/a.rs\x00docs/c.md\x00";
        let mut want = history_walk(data);
        want.sort();

        for split in 0..data.len() {
            let mut acc = HistoryAccum::new();
            acc.push(&data[..split]);
            acc.push(&data[split..]);
            let mut got = acc.entries();
            got.sort();
            assert_eq!(got, want, "split at {split} changed the result");
            assert_eq!(acc.commits, 2, "split at {split} lost a commit");
        }
    }

    #[test]
    fn history_accum_handles_byte_at_a_time_delivery() {
        // The pathological case: every field split across many pushes.
        let data: &[u8] = b"\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x002000\x00\nsrc/a.rs\x00src/b.rs\x00";
        let mut acc = HistoryAccum::new();
        for b in data {
            acc.push(std::slice::from_ref(b));
        }
        let mut got = acc.entries();
        got.sort();
        let mut want = history_walk(data);
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn history_walk_handles_a_commit_touching_nothing() {
        // An empty commit or a merge emits a time field with no path glued on.
        let data = b"\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\x003000\x00\
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\x002000\nonly.rs\x00";
        let w = history_walk(data);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, PathBuf::from("only.rs"));
        assert_eq!(w[0].1, 2000, "the empty commit must not claim the file");
    }

    #[test]
    fn history_walk_is_empty_for_no_input() {
        assert!(history_walk(b"").is_empty());
    }

    #[test]
    fn log_records() {
        let data = b"aaa\x001700000000\x00Ada\x00first subject\x00\
                     bbb\x001700000100\x00Bob\x00second\x00";
        let l = log(data);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].oid, "aaa");
        assert_eq!(l[0].time, 1700000000);
        assert_eq!(l[1].author, "Bob");
        assert_eq!(l[1].subject, "second");
    }
}
