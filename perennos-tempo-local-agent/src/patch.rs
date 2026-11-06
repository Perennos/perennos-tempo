//! `apply_patch`: several files changed in one step, in the patch format Codex uses:
//!
//! ```text
//! *** Begin Patch
//! *** Add File: notes/todo.md
//! +first line
//! *** Update File: src/main.rs
//! *** Move to: src/app.rs            (optional)
//! @@ fn main() {                     (optional: the line the change comes after)
//!      let x = 1;
//! -    println!("{x}");
//! +    println!("x = {x}");
//! *** Delete File: old.txt           (moved to the trash, never deleted)
//! *** End Patch
//! ```
//!
//! Each hunk is found by its context and removed lines, exactly first, then ignoring trailing
//! spaces, then ignoring the indentation. Nothing is written unless every hunk of every file
//! fits, so a patch applies whole or not at all. Line endings (CRLF) are kept.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hunk {
    /// The `@@ ...` line: the change comes after the first line that matches it.
    pub anchor: Option<String>,
    /// (' ' | '-' | '+', text)
    pub lines: Vec<(char, String)>,
    /// `*** End of File`: the hunk ends at the end of the file.
    pub at_end: bool,
}

/// One file as the patch leaves it: `after` None means it goes to the trash.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
    /// For a move: the old path (its file goes to the trash once the new one is written).
    pub moved_from: Option<PathBuf>,
    /// An `*** Add File` (over an existing file it is an overwrite, which asks).
    pub add: bool,
}

pub fn parse(src: &str) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .ok_or("a patch starts with *** Begin Patch")?;
    let end = lines
        .iter()
        .rposition(|l| l.trim() == "*** End Patch")
        .filter(|e| *e > start)
        .ok_or("a patch ends with *** End Patch")?;
    let mut ops: Vec<Op> = vec![];
    let mut i = start + 1;
    while i < end {
        let l = lines[i];
        if let Some(p) = l.strip_prefix("*** Add File: ") {
            let mut body = vec![];
            i += 1;
            while i < end && !lines[i].starts_with("*** ") {
                body.push(
                    lines[i]
                        .strip_prefix('+')
                        .ok_or_else(|| format!("in Add File {p}: every line starts with +"))?
                        .to_string(),
                );
                i += 1;
            }
            ops.push(Op::Add {
                path: p.trim().into(),
                lines: body,
            });
        } else if let Some(p) = l.strip_prefix("*** Delete File: ") {
            ops.push(Op::Delete {
                path: p.trim().into(),
            });
            i += 1;
        } else if let Some(p) = l.strip_prefix("*** Update File: ") {
            i += 1;
            let mut move_to = None;
            if let Some(m) = lines.get(i).and_then(|x| x.strip_prefix("*** Move to: ")) {
                move_to = Some(m.trim().to_string());
                i += 1;
            }
            let mut hunks: Vec<Hunk> = vec![];
            let mut cur = Hunk::default();
            while i < end
                && !(lines[i].starts_with("*** ") && !lines[i].starts_with("*** End of File"))
            {
                let x = lines[i];
                if x.starts_with("*** End of File") {
                    cur.at_end = true;
                } else if let Some(a) = x.strip_prefix("@@") {
                    if !cur.lines.is_empty() || cur.anchor.is_some() {
                        hunks.push(std::mem::take(&mut cur));
                    }
                    let a = a.trim();
                    cur.anchor = (!a.is_empty()).then(|| a.to_string());
                } else if x.is_empty() {
                    cur.lines.push((' ', String::new())); // an empty context line
                } else {
                    // the first character, whatever its width (`x.split_at(1)` would split a
                    // multibyte character and panic)
                    let mut cs = x.chars();
                    let k = cs.next().unwrap_or(' ');
                    let rest = cs.as_str();
                    if !matches!(k, ' ' | '-' | '+') {
                        return Err(format!(
                            "in Update File {p}: line {:?} does not start with a space, - or +",
                            head(x)
                        ));
                    }
                    cur.lines.push((k, rest.to_string()));
                }
                i += 1;
            }
            if !cur.lines.is_empty() || cur.anchor.is_some() {
                hunks.push(cur);
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(format!("Update File {p} has no changes"));
            }
            ops.push(Op::Update {
                path: p.trim().into(),
                move_to,
                hunks,
            });
        } else if l.trim().is_empty() {
            i += 1;
        } else {
            return Err(format!(
                "expected *** Add File, *** Update File or *** Delete File, got {:?}",
                head(l)
            ));
        }
    }
    if ops.is_empty() {
        return Err("the patch changes nothing".into());
    }
    Ok(ops)
}

fn head(s: &str) -> String {
    s.chars().take(60).collect()
}

/// Where `old` sits in `lines` at or after `from`: exact, then without trailing spaces, then
/// without any surrounding spaces.
fn find(lines: &[String], old: &[String], from: usize, at_end: bool) -> Option<usize> {
    if old.is_empty() {
        return Some(if at_end {
            lines.len()
        } else {
            from.min(lines.len())
        });
    }
    let fits = |norm: &dyn Fn(&str) -> String| -> Option<usize> {
        let cands: Vec<usize> = if at_end {
            lines
                .len()
                .checked_sub(old.len())
                .into_iter()
                .filter(|s| *s >= from)
                .collect()
        } else {
            (from..=lines.len().saturating_sub(old.len())).collect()
        };
        cands.into_iter().find(|&s| {
            lines.len() >= s + old.len()
                && old.iter().zip(&lines[s..]).all(|(a, b)| norm(a) == norm(b))
        })
    };
    fits(&|s: &str| s.to_string())
        .or_else(|| fits(&|s: &str| s.trim_end().to_string()))
        .or_else(|| fits(&|s: &str| s.trim().to_string()))
}

/// Apply the hunks of one file to its text.
pub fn apply_hunks(text: &str, hunks: &[Hunk], name: &str) -> Result<String, String> {
    let crlf = text.contains("\r\n");
    let trailing_newline = text.ends_with('\n') || text.is_empty();
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    let mut cursor = 0usize;
    for (n, h) in hunks.iter().enumerate() {
        if let Some(a) = &h.anchor {
            let at = lines[cursor.min(lines.len())..]
                .iter()
                .position(|l| l.trim() == a.trim() || l.contains(a.trim()))
                .map(|p| p + cursor)
                .ok_or_else(|| {
                    format!(
                        "{name}: hunk {} is after a line {:?}, which is not in the file",
                        n + 1,
                        head(a)
                    )
                })?;
            cursor = at + 1;
        }
        let old: Vec<String> = h
            .lines
            .iter()
            .filter(|(k, _)| *k != '+')
            .map(|(_, t)| t.clone())
            .collect();
        let new: Vec<String> = h
            .lines
            .iter()
            .filter(|(k, _)| *k != '-')
            .map(|(_, t)| t.clone())
            .collect();
        let at = find(&lines, &old, cursor, h.at_end).ok_or_else(|| {
            let first = old.iter().find(|l| !l.trim().is_empty()).cloned().unwrap_or_default();
            format!("{name}: hunk {} does not fit the file (its lines starting {:?} are not there as written); read the file again and redo the patch", n + 1, head(&first))
        })?;
        lines.splice(at..at + old.len(), new.iter().cloned());
        cursor = at + new.len();
    }
    let sep = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(sep);
    if trailing_newline && !out.is_empty() {
        out.push_str(sep);
    }
    Ok(out)
}

/// What the patch does to each file, relative to `dir`, without writing anything.
pub fn plan(src: &str, dir: &Path) -> Result<Vec<Change>, String> {
    let at = |p: &str| -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            dir.join(p)
        }
    };
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let mut out: Vec<Change> = vec![];
    let ops = parse(src)?;
    // each file once: two changes to one file would each start from what is on disk now
    let mut named: Vec<PathBuf> = vec![];
    for op in &ops {
        let (p, m) = match op {
            Op::Add { path, .. } | Op::Delete { path } => (path, None),
            Op::Update { path, move_to, .. } => (path, move_to.as_ref()),
        };
        for q in [Some(p), m].into_iter().flatten() {
            let q = crate::util::resolve(&at(q));
            if named.contains(&q) {
                return Err(format!("the patch names {} more than once; put all its changes under one *** Update File", q.display()));
            }
            named.push(q);
        }
    }
    for op in ops {
        match op {
            Op::Add { path, lines } => {
                let p = at(&path);
                let before = read(&p);
                let mut text = lines.join("\n");
                text.push('\n');
                out.push(Change {
                    path: p,
                    before,
                    after: Some(text),
                    moved_from: None,
                    add: true,
                });
            }
            Op::Delete { path } => {
                let p = at(&path);
                let before =
                    read(&p).ok_or_else(|| format!("Delete File {path}: there is no such file"))?;
                out.push(Change {
                    path: p,
                    before: Some(before),
                    after: None,
                    moved_from: None,
                    add: false,
                });
            }
            Op::Update {
                path,
                move_to,
                hunks,
            } => {
                let p = at(&path);
                let before = read(&p).ok_or_else(|| {
                    format!("Update File {path}: there is no such file (use *** Add File)")
                })?;
                let after = apply_hunks(&before, &hunks, &path)?;
                match move_to {
                    Some(m) => {
                        let to = at(&m);
                        out.push(Change {
                            before: read(&to),
                            path: to,
                            after: Some(after),
                            moved_from: Some(p),
                            add: false,
                        });
                    }
                    None => out.push(Change {
                        path: p,
                        before: Some(before),
                        after: Some(after),
                        moved_from: None,
                        add: false,
                    }),
                }
            }
        }
    }
    Ok(out)
}

/// `+n -m` for a change, by lines (for the turn's summary).
pub fn counts(before: Option<&str>, after: Option<&str>) -> (usize, usize) {
    let (a, b): (Vec<&str>, Vec<&str>) = (
        before.map_or(vec![], |t| t.lines().collect()),
        after.map_or(vec![], |t| t.lines().collect()),
    );
    // common prefix and suffix, then what is left on each side
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    (b.len() - pre - suf, a.len() - pre - suf)
}

/// A short unified-style diff (common prefix and suffix trimmed) for `/diff`.
pub fn diff(name: &str, before: Option<&str>, after: Option<&str>) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (
        before.map_or(vec![], |t| t.lines().collect()),
        after.map_or(vec![], |t| t.lines().collect()),
    );
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let mut s = format!(
        "--- {name}{}\n+++ {name}{}\n@@ line {} @@\n",
        if before.is_none() { " (new)" } else { "" },
        if after.is_none() {
            " (to the trash)"
        } else {
            ""
        },
        pre + 1
    );
    for l in &a[pre..a.len() - suf] {
        s.push_str(&format!("-{l}\n"));
    }
    for l in &b[pre..b.len() - suf] {
        s.push_str(&format!("+{l}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_applies_hunks() {
        let p = "*** Begin Patch\n*** Update File: a.rs\n@@ fn main() {\n     let x = 1;\n-    println!(\"{x}\");\n+    println!(\"x = {x}\");\n*** End Patch\n";
        let ops = parse(p).unwrap();
        assert!(
            matches!(&ops[0], Op::Update { path, hunks, .. } if path == "a.rs" && hunks.len() == 1 && hunks[0].anchor.as_deref() == Some("fn main() {"))
        );
        let src = "fn helper() {\n    let x = 1;\n    println!(\"{x}\");\n}\nfn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let out = apply_hunks(
            src,
            match &ops[0] {
                Op::Update { hunks, .. } => hunks,
                _ => unreachable!(),
            },
            "a.rs",
        )
        .unwrap();
        assert_eq!(out, "fn helper() {\n    let x = 1;\n    println!(\"{x}\");\n}\nfn main() {\n    let x = 1;\n    println!(\"x = {x}\");\n}\n", "the anchor picks the second copy");
        // spacing differences still fit; CRLF is kept
        let h = vec![Hunk {
            anchor: None,
            lines: vec![(' ', "a".into()), ('-', "b  ".into()), ('+', "B".into())],
            at_end: false,
        }];
        assert_eq!(
            apply_hunks("a\r\n  b\r\nc\r\n", &h, "x").unwrap(),
            "a\r\nB\r\nc\r\n"
        );
        // a hunk that does not fit says so and nothing changes
        let bad = vec![Hunk {
            anchor: None,
            lines: vec![('-', "nope".into())],
            at_end: false,
        }];
        assert!(apply_hunks("a\n", &bad, "x")
            .unwrap_err()
            .contains("does not fit"));
        // two hunks in order, and one at the end of the file
        let two = vec![
            Hunk {
                anchor: None,
                lines: vec![('-', "1".into()), ('+', "one".into())],
                at_end: false,
            },
            Hunk {
                anchor: None,
                lines: vec![(' ', "3".into()), ('+', "4".into())],
                at_end: true,
            },
        ];
        assert_eq!(
            apply_hunks("1\n2\n3\n", &two, "x").unwrap(),
            "one\n2\n3\n4\n"
        );
    }

    #[test]
    fn plans_adds_moves_and_deletes() {
        let d = crate::util::testutil::tmpdir("patch-plan");
        std::fs::write(d.join("old.txt"), "bye\n").unwrap();
        std::fs::write(d.join("m.txt"), "a\nb\n").unwrap();
        let p = "*** Begin Patch\n*** Add File: new/n.md\n+hello\n+world\n*** Delete File: old.txt\n*** Update File: m.txt\n*** Move to: moved.txt\n@@\n a\n-b\n+c\n*** End Patch";
        let ch = plan(p, &d).unwrap();
        assert_eq!(ch.len(), 3);
        assert_eq!(
            (ch[0].after.as_deref(), ch[0].before.is_none()),
            (Some("hello\nworld\n"), true)
        );
        assert!(ch[1].after.is_none() && ch[1].before.as_deref() == Some("bye\n"));
        assert_eq!(ch[2].path, d.join("moved.txt"));
        assert_eq!(ch[2].moved_from.as_deref(), Some(d.join("m.txt").as_path()));
        assert_eq!(ch[2].after.as_deref(), Some("a\nc\n"));
        for bad in [
            "no header",
            "*** Begin Patch\n*** Frobnicate: x\n*** End Patch",
            "*** Begin Patch\n*** Add File: x\nmissing plus\n*** End Patch",
            "*** Begin Patch\n*** End Patch",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(plan(
            "*** Begin Patch\n*** Update File: missing.txt\n-a\n+b\n*** End Patch",
            &d
        )
        .unwrap_err()
        .contains("no such file"));
        assert!(plan("*** Begin Patch\n*** Update File: m.txt\n-a\n+b\n*** Update File: ./m.txt\n-b\n+c\n*** End Patch", &d).unwrap_err().contains("more than once"));
        // a line that starts with a wide character is refused cleanly, not a crash
        assert!(
            parse("*** Begin Patch\n*** Update File: m.txt\n\u{e9}a\n*** End Patch")
                .unwrap_err()
                .contains("does not start with")
        );
        assert_eq!(counts(Some("a\nb\nc\n"), Some("a\nB\nB2\nc\n")), (2, 1));
        assert!(diff("m.txt", Some("a\nb\n"), Some("a\nc\n")).contains("-b\n+c\n"));
    }
}
