//! Conversation JSON (atomic save), the persistent workflow file, confirmation and output sinks.
use crate::config::Config;
use crate::util::{self, head_chars, now_str, redact};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

/// Asks before anything risky. Non-interactive runs deny.
pub trait Confirm {
    fn confirm(&self, what: &str, detail: &str) -> bool;
}

pub struct DenyAll;
impl Confirm for DenyAll {
    fn confirm(&self, _: &str, _: &str) -> bool {
        false
    }
}

/// y/N on the terminal when stdin is a TTY; otherwise denies.
pub struct TtyConfirm;
impl Confirm for TtyConfirm {
    fn confirm(&self, what: &str, detail: &str) -> bool {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            return false;
        }
        eprint!(
            "\n[confirm] {what}: {} -- allow? [y/N] ",
            head_chars(detail, 300)
        );
        let _ = std::io::stderr().flush();
        let mut l = String::new();
        stdin.read_line(&mut l).is_ok() && matches!(l.trim().to_lowercase().as_str(), "y" | "yes")
    }
}

/// Where streamed text and status lines go.
pub trait Sink {
    fn text(&mut self, s: &str);
    fn status(&mut self, s: &str);
}

/// Text to stdout, status lines to stderr (a status line never lands in the middle of streamed text).
#[derive(Default)]
pub struct StdSink {
    mid_line: bool,
}
impl Sink for StdSink {
    fn text(&mut self, s: &str) {
        print!("{s}");
        let _ = std::io::stdout().flush();
        self.mid_line = !s.ends_with('\n');
    }
    fn status(&mut self, s: &str) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
        eprintln!("[{s}]");
    }
}

#[derive(Default)]
pub struct VecSink {
    pub text: String,
    pub status: Vec<String>,
}
impl Sink for VecSink {
    fn text(&mut self, s: &str) {
        self.text.push_str(s);
    }
    fn status(&mut self, s: &str) {
        self.status.push(s.to_string());
    }
}

/// Receives each conversation event as it is emitted.
pub type EventHook = Box<dyn Fn(&Value)>;

pub struct Conv {
    pub id: String,
    pub path: PathBuf,
    pub wf: PathBuf,
    pub messages: Vec<Value>,
    pub events: Vec<Value>,
    /// repeat-call guard: key -> (result, times)
    pub seen_calls: HashMap<String, (String, u32)>,
    /// MCP tools offered so far in this conversation (prefix-stable mode keeps them).
    pub mcp_offered: Vec<String>,
    /// Called with every event as it is emitted (the HTTP server streams them live).
    pub on_event: Option<EventHook>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !id.contains("..")
}

impl Conv {
    pub fn open(cfg: &Config, cid: Option<&str>) -> Result<Conv, String> {
        let id = match cid {
            Some(c) if !valid_id(c) => {
                return Err(format!(
                    "bad conversation id '{c}' (letters, digits, - _ . only)"
                ))
            }
            Some(c) => c.to_string(),
            None => format!(
                "{}-{}-{}",
                util::date_ymd(),
                util::time_hms(),
                util::short_id()
            ),
        };
        let path = cfg
            .data_dir
            .join("conversations")
            .join(format!("{id}.json"));
        let wf = cfg.data_dir.join("workflows").join(format!("{id}.md"));
        let mut c = Conv {
            id: id.clone(),
            path,
            wf,
            messages: vec![],
            events: vec![],
            seen_calls: HashMap::new(),
            mcp_offered: vec![],
            on_event: None,
        };
        if let Ok(txt) = std::fs::read_to_string(&c.path) {
            let d: Value = serde_json::from_str(&txt)
                .map_err(|e| format!("conversation file {} is corrupt: {e}", c.path.display()))?;
            c.messages = d["messages"].as_array().cloned().unwrap_or_default();
            c.events = d["events"].as_array().cloned().unwrap_or_default();
            c.mcp_offered = d["mcp_offered"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
        }
        if !c.wf.exists() {
            std::fs::create_dir_all(c.wf.parent().unwrap_or(&cfg.data_dir))
                .map_err(|e| e.to_string())?;
            std::fs::write(&c.wf, format!("# Workflow {id}\nStarted {}\n\n## Task list\n(none yet)\n\n## Notes\n\n## Log\n", now_str())).map_err(|e| format!("cannot write workflow file: {e}"))?;
        }
        Ok(c)
    }

    /// Atomic: write a temp file, then rename over the real one.
    pub fn save(&self) {
        let tmp = self.path.with_extension("json.tmp");
        if let Some(p) = self.path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if std::fs::write(
            &tmp,
            json!({"messages": self.messages, "events": self.events, "mcp_offered": self.mcp_offered}).to_string(),
        )
        .is_ok()
        {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn emit(&mut self, kind: &str, text: &str) {
        let n = self.events.len();
        let ev = json!({"n": n, "t": now_str(), "type": kind, "text": util::cap(text, 3000)});
        if let Some(f) = &self.on_event {
            f(&ev);
        }
        self.events.push(ev);
    }

    pub fn wf_text(&self) -> String {
        std::fs::read_to_string(&self.wf).unwrap_or_default()
    }

    pub fn wf_log(&self, line: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.wf)
        {
            let _ = writeln!(f, "- {} {}", now_str(), head_chars(&redact(line), 300));
        }
    }

    pub fn wf_update(&self, tasks: Option<&Value>, note: Option<&str>) -> String {
        let mut t = self.wf_text();
        if let Some(tasks) = tasks.and_then(|t| t.as_array()) {
            let lines: Vec<String> = tasks
                .iter()
                .map(|x| match x.as_object() {
                    Some(o) => format!(
                        "- [{}] {}",
                        if o.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                            'x'
                        } else {
                            ' '
                        },
                        o.get("text").and_then(|s| s.as_str()).unwrap_or("")
                    ),
                    None => format!(
                        "- [ ] {}",
                        x.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| x.to_string())
                    ),
                })
                .collect();
            let body = if lines.is_empty() {
                "(none)".to_string()
            } else {
                lines.join("\n")
            };
            let rx = Regex::new(r"(?s)## Task list\n.*?\n\n## Notes").expect("regex");
            t = rx
                .replace(&t, |_: &regex::Captures| {
                    format!("## Task list\n{body}\n\n## Notes")
                })
                .into_owned();
        }
        if let Some(note) = note.filter(|n| !n.is_empty()) {
            t = if t.contains("\n## Log") {
                t.replacen("\n## Log", &format!("- {} {note}\n\n## Log", now_str()), 1)
            } else {
                format!("{t}\n- {note}\n")
            };
        }
        match std::fs::write(&self.wf, t) {
            Ok(_) => "workflow updated".into(),
            Err(e) => format!("error: cannot write workflow: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::tmpdir;

    fn cfg() -> Config {
        Config::defaults(Some(tmpdir("conv").to_string_lossy().into_owned()))
    }

    #[test]
    fn workflow_file_layout_update_and_log() {
        let c = cfg();
        let cv = Conv::open(&c, Some("t1")).unwrap();
        let t = cv.wf_text();
        assert!(
            t.starts_with("# Workflow t1\nStarted ")
                && t.contains("## Task list\n(none yet)\n\n## Notes\n\n## Log\n")
        );
        let r = cv.wf_update(
            Some(&json!([{"text": "first", "done": true}, {"text": "second"}, "third"])),
            Some("a note"),
        );
        assert_eq!(r, "workflow updated");
        let t = cv.wf_text();
        assert!(
            t.contains("## Task list\n- [x] first\n- [ ] second\n- [ ] third\n\n## Notes"),
            "{t}"
        );
        assert!(t.contains("a note\n\n## Log"));
        cv.wf_update(Some(&json!([])), None);
        assert!(cv.wf_text().contains("## Task list\n(none)\n\n## Notes"));
        cv.wf_log("did api_key=supersecretvalue");
        let t = cv.wf_text();
        assert!(t.contains("[REDACTED]") && !t.contains("supersecretvalue"));
        assert!(t.trim_end().ends_with("[REDACTED]"));
    }

    #[test]
    fn save_is_atomic_and_reopen_continues() {
        let c = cfg();
        let mut cv = Conv::open(&c, Some("keep")).unwrap();
        cv.messages.push(json!({"role": "user", "content": "hi"}));
        cv.emit("user", "hi");
        cv.save();
        assert!(!cv.path.with_extension("json.tmp").exists());
        let again = Conv::open(&c, Some("keep")).unwrap();
        assert_eq!(again.messages.len(), 1);
        assert_eq!(again.events.len(), 1);
    }

    #[test]
    fn bad_ids_rejected_and_ids_generated() {
        let c = cfg();
        assert!(Conv::open(&c, Some("../evil")).is_err());
        assert!(Conv::open(&c, Some("a/b")).is_err());
        let g = Conv::open(&c, None).unwrap();
        assert!(valid_id(&g.id));
    }

    #[test]
    fn non_tty_confirm_denies() {
        assert!(!DenyAll.confirm("x", "y"));
    }
}
