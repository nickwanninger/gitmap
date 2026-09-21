//! End-to-end: build a fixture repository, drive the real porcelain backend
//! through the worker, and render into a `TestBackend`.
//!
//! This is the test that proves the map actually draws — the unit tests cover
//! the pieces, but only this one exercises parse → tree → layout → canvas →
//! blit against output from a real `git`.

use gitmap::app::App;
use gitmap::git::{GitBackend, PorcelainBackend};
use gitmap::worker::{self, Message, Request};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repo with one unchanged, one modified, one staged and one untracked file.
fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gitmap-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);

    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n".repeat(20)).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n".repeat(50)).unwrap();
    std::fs::write(dir.join("src/untouched.rs"), "// quiet\n".repeat(10)).unwrap();
    std::fs::write(dir.join("README.md"), "# hi\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);

    std::fs::write(dir.join("src/main.rs"), "fn main() { /* edited */ }\n").unwrap();
    std::fs::write(dir.join("README.md"), "# hi\nmore\n").unwrap();
    git(&dir, &["add", "README.md"]);
    std::fs::write(dir.join("new file.txt"), "brand new\n").unwrap();
    dir
}

/// Run the app until both the tree and the status have arrived, then render.
fn render(dir: &Path, w: u16, h: u16) -> (App, Terminal<TestBackend>) {
    let backend = Arc::new(PorcelainBackend::discover(dir).expect("discover"));
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (msg_tx, msg_rx) = mpsc::channel::<Message>();
    let handle = worker::spawn(backend, req_rx, msg_tx);

    req_tx.send(Request::Tree).unwrap();
    req_tx.send(Request::Status).unwrap();

    let mut app = App::new(req_tx.clone());
    // Drain both replies rather than pumping, so the test is deterministic.
    for _ in 0..2 {
        let m = msg_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker reply");
        app.apply(m);
    }

    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();

    req_tx.send(Request::Quit).unwrap();
    drop(req_tx);
    let _ = handle.join();
    (app, term)
}

#[test]
fn status_and_tree_reach_the_app() {
    let dir = fixture("state");
    let (app, _) = render(&dir, 120, 40);

    assert!(app.tree.len() > 1, "tree did not load");
    assert_eq!(
        app.head.as_ref().unwrap().label(),
        "main",
        "branch name should reach the status bar"
    );

    // Every change state made it through the parser.
    let s = &app.status;
    assert!(s.contains_key(Path::new("src/main.rs")), "modified missing");
    assert!(s.contains_key(Path::new("README.md")), "staged missing");
    assert!(
        s.contains_key(Path::new("new file.txt")),
        "untracked path with a space missing"
    );
    assert!(s[Path::new("README.md")].is_staged());
    assert!(!s[Path::new("src/main.rs")].is_staged());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn map_renders_half_blocks_and_a_status_bar() {
    let dir = fixture("render");
    let (_, term) = render(&dir, 120, 40);
    let buf = term.backend().buffer();

    // The map area is drawn with upper-half-block cells.
    let blocks = buf.content().iter().filter(|c| c.symbol() == "▀").count();
    assert!(
        blocks > 500,
        "expected a dense map, got {blocks} block cells"
    );

    // The status bar carries the branch and the counts.
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("main"), "branch missing from the status bar");
    assert!(text.contains('✓') && text.contains('~'), "counts missing");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn distinct_files_occupy_distinct_cells() {
    let dir = fixture("hit");
    let (app, _) = render(&dir, 120, 40);

    // Sweep the map area and collect which files are hoverable.
    let mut seen = std::collections::HashSet::new();
    for y in 0..40u16 {
        for x in 0..120u16 {
            if let Some(id) = app.hits.at(x, y) {
                seen.insert(app.tree.node(id).path.clone());
            }
        }
    }
    assert!(
        seen.len() >= 4,
        "expected every file to be hoverable, only found {seen:?}"
    );
    assert!(seen.contains(Path::new("src/main.rs")));
    assert!(seen.contains(Path::new("src/lib.rs")));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn staging_round_trips_through_real_git() {
    let dir = fixture("stage");
    let backend = PorcelainBackend::discover(&dir).unwrap();
    let p = Path::new("src/main.rs");

    assert!(
        !backend
            .status()
            .unwrap()
            .iter()
            .find(|s| s.path == p)
            .unwrap()
            .is_staged()
    );

    backend.stage(p).unwrap();
    assert!(
        backend
            .status()
            .unwrap()
            .iter()
            .find(|s| s.path == p)
            .unwrap()
            .is_staged(),
        "stage did not take"
    );

    backend.unstage(p).unwrap();
    assert!(
        !backend
            .status()
            .unwrap()
            .iter()
            .find(|s| s.path == p)
            .unwrap()
            .is_staged(),
        "unstage did not take"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diff_of_a_modified_file_has_hunks() {
    let dir = fixture("diff");
    let backend = PorcelainBackend::discover(&dir).unwrap();

    let d = backend.diff(Path::new("src/main.rs"), false).unwrap();
    assert!(!d.hunks.is_empty(), "modified file produced no hunks");

    // The staged side of README.md is where its change lives.
    let d = backend.diff(Path::new("README.md"), true).unwrap();
    assert!(!d.hunks.is_empty(), "staged diff produced no hunks");

    // An untracked file falls back to showing its contents.
    let d = backend.diff(Path::new("new file.txt"), false).unwrap();
    assert!(!d.hunks.is_empty(), "untracked file showed nothing");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn commit_creates_a_commit() {
    let dir = fixture("commit");
    let backend = PorcelainBackend::discover(&dir).unwrap();

    let oid = backend.commit("a test commit", false).unwrap();
    assert_eq!(oid.len(), 40, "expected a full oid, got {oid:?}");

    let log = backend.log(5).unwrap();
    assert_eq!(log[0].subject, "a test commit");
    // The author comes from the user's own git config, which is the point of
    // committing through porcelain — so only assert that one was recorded.
    assert!(!log[0].author.is_empty());

    // README.md was the only staged file, so it is no longer dirty.
    let s = backend.status().unwrap();
    assert!(
        !s.iter().any(|f| f.path == Path::new("README.md")),
        "committed file should be clean"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_repository_does_not_panic() {
    // An unborn HEAD has no tree; the map must still draw.
    let dir = std::env::temp_dir().join(format!("gitmap-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("only.txt"), "hello\n").unwrap();

    let (app, term) = render(&dir, 80, 24);
    assert!(app.head.is_some());
    // The untracked file is still on the map.
    assert!(app.tree.find(Path::new("only.txt")).is_some());
    let _ = term;

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tiny_terminal_does_not_panic() {
    let dir = fixture("tiny");
    for (w, h) in [(20u16, 6u16), (10, 4), (200, 60)] {
        let (_, _) = render(&dir, w, h);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Not an assertion — prints the map as a letter grid, one letter per file, so
/// a human can check the treemap's shape. Colour is not used here because
/// neighbouring files with the same status share a colour and would merge.
/// Run with: cargo test --test smoke visual -- --ignored --nocapture
#[test]
#[ignore]
fn visual() {
    // GITMAP_VISUAL_REPO points this at a real repository instead of the
    // fixture, which is the only way to see how the jitter reads at density.
    let (dir, owned) = match std::env::var("GITMAP_VISUAL_REPO") {
        Ok(p) => (PathBuf::from(p), false),
        Err(_) => (fixture("visual"), true),
    };
    let (app, term) = render(&dir, 100, 30);

    let files: Vec<_> = app.tree.files_under(app.tree.root);
    for y in 0..app.hits.h {
        let mut line = String::new();
        for x in 0..app.hits.w {
            line.push(match app.hits.at(x, y) {
                Some(id) => {
                    (b'a' + files.iter().position(|&f| f == id).unwrap_or(25) as u8) as char
                }
                None => '.',
            });
        }
        println!("{line}");
    }
    for (i, &f) in files.iter().enumerate() {
        println!(
            "{} = {}",
            (b'a' + i as u8) as char,
            app.tree.node(f).path.display()
        );
    }
    // Shade grid: shows whether neighbouring blocks are actually separable.
    println!("\n--- shading (each char = one cell background) ---");
    {
        let buf = term.backend().buffer();
        let mut lums = std::collections::BTreeSet::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let c = buf.cell((x, y)).unwrap();
                if c.symbol() == "\u{2580}" {
                    if let ratatui::style::Color::Rgb(r, g, b) = c.bg {
                        lums.insert((r as u16 + g as u16 + b as u16) / 3);
                    }
                }
            }
        }
        println!("distinct background luminances in the map: {}", lums.len());
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                let c = buf.cell((x, y)).unwrap();
                if c.symbol() == "\u{2580}" {
                    let l = match c.bg {
                        ratatui::style::Color::Rgb(r, g, b) => (r as u16 + g as u16 + b as u16) / 3,
                        _ => 0,
                    };
                    line.push(b"0123456789abcdef"[(l / 16).min(15) as usize] as char);
                } else {
                    line.push(' ');
                }
            }
            println!("{}", line.trim_end());
        }
    }

    println!("\n--- rendered frame ---");
    let buf = term.backend().buffer();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        println!("{}", line.trim_end());
    }
    if owned {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
