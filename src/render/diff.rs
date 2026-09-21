//! The diff pane.

use crate::git::{Diff, LineKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Flatten a diff into styled lines, hunk headers included.
pub fn lines(diff: &Diff, path: &str) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(Span::styled(
            path.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if diff.binary {
        out.push(Line::from(Span::styled(
            "binary file",
            Style::default().fg(Color::DarkGray),
        )));
        return out;
    }
    if diff.hunks.is_empty() {
        out.push(Line::from(Span::styled(
            "no changes",
            Style::default().fg(Color::DarkGray),
        )));
        return out;
    }

    for h in &diff.hunks {
        out.push(Line::from(Span::styled(
            h.header.clone(),
            Style::default().fg(Color::Cyan),
        )));
        for l in &h.lines {
            let (prefix, style) = match l.kind {
                LineKind::Added => ("+", Style::default().fg(Color::Green)),
                LineKind::Removed => ("-", Style::default().fg(Color::Red)),
                LineKind::Context => (" ", Style::default().fg(Color::Gray)),
                LineKind::Meta => ("", Style::default().fg(Color::DarkGray)),
            };
            out.push(Line::from(Span::styled(
                format!("{prefix}{}", l.text),
                style,
            )));
        }
        out.push(Line::from(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DiffLine, Hunk};

    #[test]
    fn renders_header_and_hunks() {
        let d = Diff {
            binary: false,
            hunks: vec![Hunk {
                header: "@@ -1,2 +1,2 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Removed,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: LineKind::Added,
                        text: "new".into(),
                    },
                ],
            }],
        };
        let l = lines(&d, "src/x.rs");
        let text: Vec<String> = l.iter().map(|x| x.to_string()).collect();
        assert_eq!(text[0], "src/x.rs");
        assert!(text.contains(&"@@ -1,2 +1,2 @@".to_string()));
        assert!(text.contains(&"-old".to_string()));
        assert!(text.contains(&"+new".to_string()));
    }

    #[test]
    fn binary_and_empty_are_labelled() {
        let b = lines(
            &Diff {
                binary: true,
                hunks: vec![],
            },
            "i.png",
        );
        assert!(b.iter().any(|l| l.to_string() == "binary file"));

        let e = lines(&Diff::default(), "x.rs");
        assert!(e.iter().any(|l| l.to_string() == "no changes"));
    }
}
