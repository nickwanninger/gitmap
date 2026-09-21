//! Parsers for git's machine-readable output.
//!
//! Everything here is a pure function over bytes so it can be tested against
//! recorded fixtures without a repository. Paths are handled as bytes and only
//! converted at the edge, because non-UTF-8 paths exist on Linux.

use super::{Change, CommitMeta, Diff, DiffLine, FileStatus, Hunk, LineKind, TreeEntry};
use std::path::PathBuf;

/// Build a `PathBuf` from raw bytes without going through `str`.
#[cfg(unix)]
fn path_from_bytes(b: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(b))
}

#[cfg(not(unix))]
fn path_from_bytes(b: &[u8]) -> PathBuf {
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
