//! Checks on a take: the repo's own tests, and Soundcheck's findings, run in an isolated copy.
//!
//! A take that writes files says so with blocks like
//!
//! ```text
//! === file: src/lib.rs ===
//! ...the whole file...
//! === end ===
//! ```
//!
//! The checks copy the owner's `workdir` into a scratch folder (never the original), lay those
//! files over the copy, and run the owner's check command there. The original is not touched. A
//! take with no file blocks has nothing to test, so its tests are "not applicable", not green.
//!
//! What this does not do: it runs the owner's own check command, set by the owner in
//! `conductor.json`, with the owner's rights. It is a copy, not a sandbox.

use crate::setup::json::{parse, J};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_FILES_IN_COPY: usize = 20_000;
const MAX_BYTES_IN_COPY: u64 = 300 << 20;
const MAX_BLOCKS: usize = 30;
const MAX_BLOCK: usize = 200_000;
const SKIP: [&str; 4] = [".git", "target", "node_modules", ".venv"];

#[derive(Clone, Debug, PartialEq)]
pub struct Tested {
    pub passed: bool,
    /// The end of the output, for feeding back to the member.
    pub tail: String,
}

/// Soundcheck findings by severity (critical counts as high).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub high: u32,
    pub med: u32,
    pub low: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Checked {
    pub tests: Option<Tested>,
    pub found: Option<Found>,
}

pub trait Checks: Sync {
    /// Check one answer. `key` names it (and its scratch folder) uniquely within a run.
    fn run(&self, key: &str, answer: &str) -> Checked {
        let _ = (key, answer);
        Checked::default()
    }
}

/// No repo checks are set up.
pub struct NoChecks;
impl Checks for NoChecks {}

/// The file blocks in an answer: (relative path, contents). Paths that are absolute, climb out
/// with `..`, or are odd are dropped.
pub fn file_blocks(answer: &str) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut lines = answer.lines();
    while let Some(l) = lines.next() {
        let Some(name) = l
            .trim()
            .strip_prefix("=== file:")
            .and_then(|r| r.trim().strip_suffix("==="))
        else {
            continue;
        };
        let mut body = String::new();
        for l in lines.by_ref() {
            if l.trim() == "=== end ===" {
                break;
            }
            body.push_str(l);
            body.push('\n');
        }
        let rel = PathBuf::from(name.trim());
        let fine = !name.trim().is_empty()
            && name.len() <= 200
            && rel.components().all(|c| matches!(c, Component::Normal(_)))
            && body.len() <= MAX_BLOCK;
        if fine && out.len() < MAX_BLOCKS {
            out.push((rel, body));
        }
    }
    out
}

fn copy_tree(from: &Path, to: &Path, files: &mut usize, bytes: &mut u64) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(from)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let name = e.file_name();
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue; // a link could lead out of the copy
        }
        if meta.is_dir() {
            if !SKIP.iter().any(|s| name == *s) {
                copy_tree(&e.path(), &to.join(&name), files, bytes)?;
            }
        } else if meta.is_file() {
            *files += 1;
            *bytes += meta.len();
            if *files > MAX_FILES_IN_COPY || *bytes > MAX_BYTES_IN_COPY {
                return Err("the folder is too big to copy for a check".into());
            }
            std::fs::copy(e.path(), to.join(&name)).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Run `argv` in `dir`, output to a file, stopped after `limit`. Whether it passed, and the end
/// of what it printed.
fn run_limited(
    argv: &[String],
    dir: &Path,
    limit: Duration,
    log: &Path,
) -> Result<(bool, String), String> {
    let out = std::fs::File::create(log).map_err(|e| e.to_string())?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", argv[0]))?;
    let start = Instant::now();
    let (passed, note) = loop {
        match child.try_wait() {
            Ok(Some(s)) => break (s.success(), String::new()),
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, format!("\n[stopped after {} s]", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let text = String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned();
    let tail: String = text
        .chars()
        .rev()
        .take(4000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok((passed, format!("{tail}{note}")))
}

/// The repo's own checks for a band, in an isolated copy of its working folder.
pub struct RepoChecks {
    pub workdir: PathBuf,
    /// The check command: a program and its arguments (no shell).
    pub command: Vec<String>,
    /// The agent program, for Soundcheck's quick scanners (None: skip them).
    pub soundcheck_exe: Option<PathBuf>,
    /// Where the scratch copies go. Only folders made here are ever removed.
    pub scratch: PathBuf,
    pub timeout: Duration,
}

impl RepoChecks {
    fn count(&self, copy: &Path) -> Option<Found> {
        let exe = self.soundcheck_exe.as_ref()?;
        let argv = vec![
            exe.to_string_lossy().into_owned(),
            "soundcheck".into(),
            "--all".into(),
            ".".into(),
            "--scanners-only".into(),
            "--json".into(),
        ];
        let (_, text) =
            run_limited(&argv, copy, self.timeout, &copy.join(".soundcheck.out")).ok()?;
        let start = text.find('{')?;
        let j = parse(&text[start..]).ok()?;
        let mut f = Found {
            high: 0,
            med: 0,
            low: 0,
        };
        let rows: &[J] = j.arr("findings");
        for r in rows {
            match r.str("severity") {
                Some("critical" | "high") => f.high += 1,
                Some("medium") => f.med += 1,
                Some("low") => f.low += 1,
                _ => {}
            }
        }
        Some(f)
    }
}

impl Checks for RepoChecks {
    fn run(&self, key: &str, answer: &str) -> Checked {
        let blocks = file_blocks(answer);
        if blocks.is_empty()
            || key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            return Checked::default();
        }
        let copy = self.scratch.join(key);
        let (mut files, mut bytes) = (0, 0);
        let made = copy_tree(&self.workdir, &copy, &mut files, &mut bytes).and_then(|()| {
            for (rel, body) in &blocks {
                let to = copy.join(rel);
                if let Some(d) = to.parent() {
                    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                std::fs::write(&to, body).map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        let mut out = Checked::default();
        match made {
            Err(e) => {
                out.tests = Some(Tested {
                    passed: false,
                    tail: format!("could not set up the isolated copy: {e}"),
                })
            }
            Ok(()) => {
                if !self.command.is_empty() {
                    out.tests = Some(
                        match run_limited(
                            &self.command,
                            &copy,
                            self.timeout,
                            &copy.join(".check.out"),
                        ) {
                            Ok((passed, tail)) => Tested { passed, tail },
                            Err(e) => Tested {
                                passed: false,
                                tail: e,
                            },
                        },
                    );
                }
                out.found = self.count(&copy);
            }
        }
        // the copy is ours alone: made above, under the scratch folder, from the key
        if copy.starts_with(&self.scratch) && copy != self.scratch {
            let _ = std::fs::remove_dir_all(&copy);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::conductor::tests::tmp;

    #[test]
    fn file_blocks_are_read_and_unsafe_paths_dropped() {
        let a = "words\n=== file: src/a.txt ===\nhello\n=== end ===\n=== file: ../evil ===\nx\n=== end ===\n=== file: /etc/x ===\ny\n=== end ===\n=== file: b.txt ===\nbee\n=== end ===\n";
        let b = file_blocks(a);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], (PathBuf::from("src/a.txt"), "hello\n".to_string()));
        assert_eq!(b[1].0, PathBuf::from("b.txt"));
        assert!(file_blocks("no blocks here").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn checks_run_in_a_copy_and_the_original_is_not_touched() {
        let root = tmp("checks");
        let work = root.join("work");
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::write(work.join("sub/keep.txt"), "original").unwrap();
        std::fs::create_dir_all(work.join("target")).unwrap();
        std::fs::write(work.join("target/big"), "skipped").unwrap();
        let rc = RepoChecks {
            workdir: work.clone(),
            // green when the answer's file says ok and the original is still beside it
            command: vec![
                "sh".into(),
                "-c".into(),
                "grep -q ok answer.txt && test -f sub/keep.txt && test ! -e target/big".into(),
            ],
            soundcheck_exe: None,
            scratch: root.join("scratch"),
            timeout: Duration::from_secs(20),
        };
        let good = rc.run("t1", "=== file: answer.txt ===\nok\n=== end ===\n");
        assert_eq!(
            good.tests.as_ref().map(|t| t.passed),
            Some(true),
            "{good:?}"
        );
        let bad = rc.run("t2", "=== file: answer.txt ===\nnope\n=== end ===\n");
        assert_eq!(bad.tests.as_ref().map(|t| t.passed), Some(false));
        assert!(
            !work.join("answer.txt").exists(),
            "the original stays as it was"
        );
        assert!(
            !root.join("scratch/t1").exists(),
            "the copy is cleared away"
        );
        // no file blocks: nothing to test, which is not the same as green
        assert_eq!(rc.run("t3", "just words"), Checked::default());
        // a key that could climb out is refused
        assert_eq!(
            rc.run("../x", "=== file: a ===\nb\n=== end ===\n"),
            Checked::default()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_check_that_runs_too_long_is_stopped_and_counts_as_red() {
        let root = tmp("checks-slow");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let rc = RepoChecks {
            workdir: work,
            command: vec!["sleep".into(), "30".into()],
            soundcheck_exe: None,
            scratch: root.join("scratch"),
            timeout: Duration::from_millis(300),
        };
        let r = rc.run("slow", "=== file: a ===\nb\n=== end ===\n");
        let t = r.tests.unwrap();
        assert!(!t.passed && t.tail.contains("stopped after"));
        let _ = std::fs::remove_dir_all(root);
    }
}
