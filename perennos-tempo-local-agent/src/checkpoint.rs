//! Checkpoints. Before the agent changes a file in a turn, what was there is kept in
//! `<data>/checkpoints/<conversation>/<turn>/`, once per file per turn. `/undo` puts the last
//! changed turn back (the files as they are now go to the trash first, so nothing is ever lost),
//! and `/diff` shows what that turn did. Like Claude Code's rewind and Codex's undo.
use crate::util;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Checkpoints {
    root: PathBuf,
}

/// One file a turn changed: where it is, and the copy of what it held before (None: it did not
/// exist). The copy is byte for byte, so `/undo` restores any file exactly, text or not.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub kept: Option<PathBuf>,
}

impl Entry {
    /// What the file held before the turn, exactly.
    pub fn before(&self) -> Option<Vec<u8>> {
        self.kept.as_ref().and_then(|k| std::fs::read(k).ok())
    }

    /// The same, as text for diffs and counts (bytes that are not UTF-8 are shown replaced).
    pub fn before_text(&self) -> Option<String> {
        self.before()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

fn text_of(p: &Path) -> Option<String> {
    std::fs::read(p)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

impl Checkpoints {
    pub fn new(data_dir: &Path) -> Checkpoints {
        Checkpoints {
            root: data_dir.join("checkpoints"),
        }
    }

    fn dir(&self, conv: &str, turn: u64) -> PathBuf {
        self.root.join(conv).join(turn.to_string())
    }

    /// Keep what `path` holds now, before the turn changes it (only the first time in a turn).
    /// Only the list of kept files is read here, never their contents, and the copy is streamed.
    pub fn keep(&self, conv: &str, turn: u64, path: &Path) -> Result<(), String> {
        let d = self.dir(conv, turn);
        let path = util::resolve(path);
        let listed = self.entries_in(&d);
        if listed.iter().any(|e| e.path == path) {
            return Ok(());
        }
        std::fs::create_dir_all(&d).map_err(|e| format!("cannot keep a checkpoint: {e}"))?;
        let before = if path.is_file() {
            let f = d.join(format!("{}.before", listed.len()));
            std::fs::copy(&path, &f).map_err(|e| format!("cannot keep a checkpoint: {e}"))?;
            Some(
                f.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            None
        };
        let row = json!({"path": path.to_string_lossy(), "before": before});
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(d.join("manifest.jsonl"))
            .map_err(|e| format!("cannot keep a checkpoint: {e}"))?;
        writeln!(f, "{row}").map_err(|e| format!("cannot keep a checkpoint: {e}"))
    }

    fn entries_in(&self, d: &Path) -> Vec<Entry> {
        let text = std::fs::read_to_string(d.join("manifest.jsonl")).unwrap_or_default();
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|v| Entry {
                path: PathBuf::from(v["path"].as_str().unwrap_or("")),
                kept: v["before"].as_str().map(|f| d.join(f)),
            })
            .collect()
    }

    /// The files a turn changed.
    pub fn entries(&self, conv: &str, turn: u64) -> Vec<Entry> {
        self.entries_in(&self.dir(conv, turn))
    }

    /// The latest turn (at most `turn`) that changed files and was not undone yet.
    pub fn last_changed(&self, conv: &str, turn: u64) -> Option<u64> {
        (1..=turn)
            .rev()
            .find(|t| self.dir(conv, *t).join("manifest.jsonl").is_file())
    }

    /// Put the files of the latest changed turn back as they were, byte for byte. What they hold
    /// now goes to `trash` first; a file the turn created goes to the trash. Returns what was done.
    pub fn undo(&self, conv: &str, turn: u64, trash: &Path) -> Result<(u64, Vec<String>), String> {
        let t = self
            .last_changed(conv, turn)
            .ok_or("nothing to undo: no turn in this conversation changed a file")?;
        let d = self.dir(conv, t);
        let day = trash.join(util::date_ymd());
        let mut done = vec![];
        for e in self.entries_in(&d) {
            if e.path.exists() {
                std::fs::create_dir_all(&day).map_err(|x| format!("cannot use the trash: {x}"))?;
                let name = e
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                let mut dest = day.join(format!("{name}.undone"));
                let mut i = 1;
                while dest.exists() {
                    dest = day.join(format!("{name}.undone.{i}"));
                    i += 1;
                }
                std::fs::rename(&e.path, &dest)
                    .or_else(|_| std::fs::copy(&e.path, &dest).map(|_| ()))
                    .map_err(|x| format!("cannot move {} to the trash: {x}", e.path.display()))?;
            }
            match &e.kept {
                Some(k) => {
                    if let Some(p) = e.path.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    std::fs::copy(k, &e.path)
                        .map_err(|x| format!("cannot restore {}: {x}", e.path.display()))?;
                    done.push(format!("restored {}", e.path.display()));
                }
                None => done.push(format!("{} was new: moved to the trash", e.path.display())),
            }
        }
        // the turn is undone: it is not undone again, and the next /undo goes further back
        std::fs::rename(d.join("manifest.jsonl"), d.join("manifest.undone.jsonl"))
            .map_err(|x| x.to_string())?;
        Ok((t, done))
    }

    /// `name +a -r` for each file a turn changed (now against before).
    pub fn summary(&self, conv: &str, turn: u64) -> Vec<String> {
        self.entries(conv, turn)
            .into_iter()
            .map(|e| {
                let now = text_of(&e.path);
                let before = e.before_text();
                let (a, r) = crate::patch::counts(before.as_deref(), now.as_deref());
                let name = e
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match (before.is_some(), now.is_some()) {
                    (false, true) => format!("{name} (new) +{a}"),
                    (true, false) => format!("{name} (to the trash)"),
                    _ => format!("{name} +{a} -{r}"),
                }
            })
            .collect()
    }

    /// The changes of the latest changed turn, as a diff.
    pub fn diff(&self, conv: &str, turn: u64) -> Option<String> {
        let t = self.last_changed(conv, turn)?;
        Some(
            self.entries(conv, t)
                .into_iter()
                .map(|e| {
                    crate::patch::diff(
                        &e.path.display().to_string(),
                        e.before_text().as_deref(),
                        text_of(&e.path).as_deref(),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::tmpdir;

    #[test]
    fn keeps_undoes_and_never_deletes() {
        let d = tmpdir("ckpt");
        let (data, trash, work) = (d.join("data"), d.join("trash"), d.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let a = work.join("a.txt");
        std::fs::write(&a, "one\n").unwrap();
        let c = Checkpoints::new(&data);
        // turn 1 changes a.txt twice (kept once) and makes b.txt
        c.keep("c1", 1, &a).unwrap();
        std::fs::write(&a, "two\n").unwrap();
        c.keep("c1", 1, &a).unwrap();
        std::fs::write(&a, "three\nfour\n").unwrap();
        let b = work.join("b.txt");
        c.keep("c1", 1, &b).unwrap();
        std::fs::write(&b, "new\n").unwrap();
        assert_eq!(c.entries("c1", 1).len(), 2);
        assert_eq!(c.summary("c1", 1), ["a.txt +2 -1", "b.txt (new) +1"]);
        assert!(c.diff("c1", 3).unwrap().contains("-one\n+three\n+four"));
        let (t, done) = c.undo("c1", 3, &trash).unwrap();
        assert_eq!(t, 1);
        assert_eq!(done.len(), 2);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one\n");
        assert!(!b.exists(), "the new file left the folder");
        // nothing was deleted: both later versions are in the trash
        let kept: Vec<String> = std::fs::read_dir(trash.join(util::date_ymd()))
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect();
        assert!(
            kept.contains(&"three\nfour\n".to_string()) && kept.contains(&"new\n".to_string()),
            "{kept:?}"
        );
        // a second undo has nothing left
        assert!(c
            .undo("c1", 3, &trash)
            .unwrap_err()
            .contains("nothing to undo"));
        // a file that is not text comes back exactly
        let pic = work.join("pic.bin");
        let bytes = [0xffu8, 0xfe, 0x00, 0x80, 0x81, b'\n'];
        std::fs::write(&pic, bytes).unwrap();
        c.keep("c1", 4, &pic).unwrap();
        std::fs::write(&pic, "overwritten").unwrap();
        c.undo("c1", 4, &trash).unwrap();
        assert_eq!(std::fs::read(&pic).unwrap(), bytes);
    }
}
