//! The persistent workflow file: a Markdown task list, notes and a log that survive
//! restarts, so "continue" picks up the plan. Same layout as the Python harness:
//!
//! ```text
//! # Workflow <id>
//! Started <time>
//!
//! ## Task list
//! - [x] done thing
//! - [ ] next thing
//!
//! ## Notes
//! - <time> note
//!
//! ## Log
//! - <time> tool -> result
//! ```
use crate::text::{now, redact, truncate_chars};
use serde_json::Value;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub text: String,
    pub done: bool,
}

impl Task {
    /// From a tool argument: `{"text": .., "done": ..}` or a bare string.
    pub fn from_value(v: &Value) -> Task {
        match v {
            Value::Object(m) => Task {
                text: match m.get("text") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                },
                done: m.get("done").and_then(Value::as_bool).unwrap_or(false),
            },
            Value::String(s) => Task { text: s.clone(), done: false },
            other => Task { text: other.to_string(), done: false },
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workflow {
    path: PathBuf,
}

impl Workflow {
    /// Open the workflow file, creating it (and its folder) from the template if missing.
    pub fn open(path: impl Into<PathBuf>, id: &str) -> io::Result<Workflow> {
        let path = path.into();
        if !path.exists() {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                fs::create_dir_all(dir)?;
            }
            fs::write(&path, format!("# Workflow {id}\nStarted {}\n\n## Task list\n(none yet)\n\n## Notes\n\n## Log\n", now()))?;
        }
        Ok(Workflow { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn text(&self) -> io::Result<String> {
        fs::read_to_string(&self.path)
    }

    /// The last `n` characters, for the system prompt.
    pub fn tail(&self, n: usize) -> String {
        let t = self.text().unwrap_or_default();
        let len = t.chars().count();
        t.chars().skip(len.saturating_sub(n)).collect()
    }

    /// Append one redacted line (at most 300 characters) to the log.
    pub fn log(&self, line: &str) -> io::Result<()> {
        let mut f = fs::OpenOptions::new().append(true).create(true).open(&self.path)?;
        writeln!(f, "- {} {}", now(), truncate_chars(&redact(line), 300))
    }

    /// Replace the task list and/or add a note. `Some(&[])` clears the list.
    pub fn update(&self, tasks: Option<&[Task]>, note: Option<&str>) -> io::Result<String> {
        let mut t = self.text()?;
        if let Some(tasks) = tasks {
            let body = if tasks.is_empty() {
                "(none)".to_string()
            } else {
                tasks.iter().map(|x| format!("- [{}] {}", if x.done { 'x' } else { ' ' }, x.text)).collect::<Vec<_>>().join("\n")
            };
            const HEAD: &str = "## Task list\n";
            if let Some(a) = t.find(HEAD) {
                if let Some(b) = t[a + HEAD.len()..].find("\n\n## Notes") {
                    t.replace_range(a + HEAD.len()..a + HEAD.len() + b, &body);
                }
            }
        }
        if let Some(note) = note.filter(|n| !n.is_empty()) {
            if let Some(i) = t.find("\n## Log") {
                t.replace_range(i..i + "\n## Log".len(), &format!("- {} {note}\n\n## Log", now()));
            } else {
                t.push_str(&format!("\n- {note}\n"));
            }
        }
        fs::write(&self.path, t)?;
        Ok("workflow updated".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::json;

    #[test]
    fn creates_template_and_keeps_existing_file() {
        let dir = TempDir::new("wf");
        let path = dir.path().join("nested").join("w1.md");
        let wf = Workflow::open(&path, "w1").unwrap();
        let t = wf.text().unwrap();
        assert!(t.starts_with("# Workflow w1\nStarted "));
        assert!(t.ends_with("\n\n## Task list\n(none yet)\n\n## Notes\n\n## Log\n"));
        fs::write(&path, "kept").unwrap();
        assert_eq!(Workflow::open(&path, "w1").unwrap().text().unwrap(), "kept");
    }

    #[test]
    fn updates_tasks_notes_and_log() {
        let dir = TempDir::new("wf");
        let wf = Workflow::open(dir.path().join("w.md"), "w").unwrap();
        let tasks: Vec<Task> =
            [json!({"text": "plan", "done": true}), json!("build"), json!({"text": 3})].iter().map(Task::from_value).collect();
        assert_eq!(wf.update(Some(&tasks), Some("first note")).unwrap(), "workflow updated");
        wf.update(None, Some("second note")).unwrap();
        wf.update(None, Some("")).unwrap();
        let secret = format!("sk-{}", "z".repeat(24));
        wf.log(&format!("read_file -> {secret} {}", "x".repeat(400))).unwrap();
        let t = wf.text().unwrap();
        assert!(t.contains("## Task list\n- [x] plan\n- [ ] build\n- [ ] 3\n\n## Notes\n"), "{t}");
        let notes = &t[t.find("## Notes").unwrap()..t.find("## Log").unwrap()];
        assert!(notes.find("first note").unwrap() < notes.find("second note").unwrap());
        assert_eq!(notes.matches("\n- ").count(), 2, "an empty note adds nothing");
        let log = t.lines().last().unwrap();
        assert!(log.contains("read_file -> [REDACTED] x") && !log.contains(&secret));
        assert!(log.chars().count() < 340, "log lines are capped");
        // Clearing and re-filling the list keeps the rest of the file.
        wf.update(Some(&[]), None).unwrap();
        assert!(wf.text().unwrap().contains("## Task list\n(none)\n\n## Notes\n- "));
        assert!(wf.tail(20).trim_end().ends_with(&log[log.len() - 10..]));
    }
}
