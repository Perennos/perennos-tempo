//! Hooks: commands the owner lists in the agent's own config file, run around tool calls and at
//! the end of a turn. They are set in the config file only: no tool, conversation, HTTP call or
//! model can add, change or remove one (the config file is also one of the files the file tools
//! may never write). Hooks never run in guest conversations, and they do not ask for approval:
//! the owner wrote them. Each command still passes the same refusals as `run_command`.
use crate::conv::{Conv, Permission};
use crate::tools::{shell_env, Env};
use crate::util::{head_chars, redact};
use regex::Regex;
use serde_json::{json, Value};
use std::time::Duration;

/// How long a hook may run unless `hook_timeout_s` says otherwise.
pub const DEFAULT_TIMEOUT_S: f64 = 30.0;
/// The longest `hook_timeout_s` accepted (one hour).
const MAX_TIMEOUT_S: f64 = 3600.0;
/// The most bytes the `TEMPO_HOOK_EVENT` JSON may hold.
pub const EVENT_CAP: usize = 16_384;
/// The most characters of a hook's stderr put in a "blocked by hook" result.
const BLOCK_REASON_CAP: usize = 500;
/// The most characters of an after_tool hook's stdout appended to a tool result.
pub const APPEND_CAP: usize = 2_000;
/// The most characters of a hook's output shown in a status line.
const NOTE_CAP: usize = 300;

/// One hook: a command, and for tool hooks a regex the tool's name must match.
#[derive(Clone, Debug)]
pub struct Hook {
    matcher: Option<Regex>,
    pub command: String,
}

impl Hook {
    /// True when this hook is for the tool `name` (a hook with no `match` is for every tool).
    /// The regex is not anchored: `run_` matches `run_command`; write `^run_command$` for one.
    pub fn applies(&self, name: &str) -> bool {
        self.matcher.as_ref().is_none_or(|m| m.is_match(name))
    }
}

/// The hooks from the config file.
#[derive(Clone, Debug)]
pub struct Hooks {
    pub before_tool: Vec<Hook>,
    pub after_tool: Vec<Hook>,
    pub after_turn: Vec<Hook>,
    /// Seconds a hook may run before it is killed (`hook_timeout_s`).
    pub timeout_s: f64,
}

impl Default for Hooks {
    fn default() -> Hooks {
        Hooks {
            before_tool: vec![],
            after_tool: vec![],
            after_turn: vec![],
            timeout_s: DEFAULT_TIMEOUT_S,
        }
    }
}

impl Hooks {
    /// Read `hooks` and `hook_timeout_s` from the config object. Anything wrong is an error, never
    /// quietly skipped: a misspelled `before_tool` would otherwise switch a guard off unseen.
    pub fn from_json(cfg: &Value) -> Result<Hooks, String> {
        let mut h = Hooks::default();
        if let Some(t) = cfg.get("hook_timeout_s") {
            h.timeout_s = t
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
                .ok_or("hook_timeout_s must be a number of seconds above 0")?
                .min(MAX_TIMEOUT_S);
        }
        let Some(v) = cfg.get("hooks") else {
            return Ok(h);
        };
        let obj = v.as_object().ok_or("hooks must be an object")?;
        for key in obj.keys() {
            if !["before_tool", "after_tool", "after_turn"].contains(&key.as_str()) {
                return Err(format!(
                    "hooks.{key} is not a hook (use before_tool, after_tool or after_turn)"
                ));
            }
        }
        h.before_tool = list(obj.get("before_tool"), "before_tool")?;
        h.after_tool = list(obj.get("after_tool"), "after_tool")?;
        h.after_turn = list(obj.get("after_turn"), "after_turn")?;
        Ok(h)
    }

    /// The timeout as a `Duration` (never panics, whatever the struct holds).
    pub fn timeout(&self) -> Duration {
        let s = if self.timeout_s.is_finite() && self.timeout_s > 0.0 {
            self.timeout_s.min(MAX_TIMEOUT_S)
        } else {
            DEFAULT_TIMEOUT_S
        };
        Duration::from_secs_f64(s)
    }
}

/// The hooks listed under one event name.
fn list(v: Option<&Value>, event: &str) -> Result<Vec<Hook>, String> {
    let Some(v) = v else {
        return Ok(vec![]);
    };
    let arr = v
        .as_array()
        .ok_or_else(|| format!("hooks.{event} must be a list"))?;
    arr.iter()
        .enumerate()
        .map(|(i, e)| {
            let at = format!("hooks.{event}[{i}]");
            let command = e
                .get("command")
                .and_then(Value::as_str)
                .filter(|c| !c.trim().is_empty())
                .ok_or_else(|| format!("{at} needs a \"command\""))?
                .to_string();
            let matcher = match e.get("match") {
                None | Some(Value::Null) => None,
                Some(m) => {
                    let m = m
                        .as_str()
                        .ok_or_else(|| format!("{at}: \"match\" must be a regex string"))?;
                    Some(Regex::new(m).map_err(|err| format!("{at}: bad \"match\" regex: {err}"))?)
                }
            };
            Ok(Hook { matcher, command })
        })
        .collect()
}

/// What the hooks of one event decided.
#[derive(Default, Debug)]
pub struct HookOutcome {
    /// before_tool: the tool result to give instead of running the tool.
    pub blocked: Option<String>,
    /// after_tool: text to add to the end of the tool result.
    pub append: String,
    /// Status lines to show (failures, skipped hooks, after_turn output).
    pub notes: Vec<String>,
}

/// The JSON a hook finds in `TEMPO_HOOK_EVENT`, at most `EVENT_CAP` bytes. When it is longer,
/// every string in `args` and `result` is cut to the same length, the longest that makes the
/// whole fit: the largest strings lose the most, the short ones stay whole, and the JSON stays
/// valid.
pub fn event_json(
    event: &str,
    conv: &str,
    tool: Option<&str>,
    args: Option<&Value>,
    result: Option<&str>,
) -> String {
    let mut ev =
        json!({"event": event, "conv": conv, "tool": tool, "args": args, "result": result});
    let text = ev.to_string();
    if text.len() <= EVENT_CAP {
        return text;
    }
    // the largest cut length that fits (a longer one never fits better)
    let (mut lo, mut hi) = (0, longest_string(&ev));
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if cut_strings(ev.clone(), mid).to_string().len() <= EVENT_CAP {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    ev = cut_strings(ev, lo);
    // only reachable with enormous keys or numbers: drop what is left of the payload
    for k in ["args", "result"] {
        if ev.to_string().len() <= EVENT_CAP {
            break;
        }
        ev[k] = Value::Null;
    }
    let text = ev.to_string();
    if text.len() <= EVENT_CAP {
        return text;
    }
    json!({"event": event, "conv": head_chars(conv, 200), "tool": null, "args": null, "result": null})
        .to_string()
}

/// The byte length of the longest string anywhere in `v` (0 when there is none).
fn longest_string(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(longest_string).max().unwrap_or(0),
        Value::Object(o) => o.values().map(longest_string).max().unwrap_or(0),
        _ => 0,
    }
}

/// `v` with every string longer than `max` bytes cut short (on a character boundary, ending in a
/// marker when there is room for one).
fn cut_strings(mut v: Value, max: usize) -> Value {
    const MARK: &str = "...[cut]";
    match &mut v {
        Value::String(s) if s.len() > max => {
            if max <= MARK.len() {
                s.clear();
            } else {
                let mut keep = max - MARK.len();
                while !s.is_char_boundary(keep) {
                    keep -= 1;
                }
                s.truncate(keep);
                s.push_str(MARK);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| *x = cut_strings(x.take(), max)),
        Value::Object(o) => o.values_mut().for_each(|x| *x = cut_strings(x.take(), max)),
        _ => {}
    }
    v
}

/// How a hook that was started ended.
enum Ended {
    /// It exited by itself: its code, stdout and stderr.
    Exit(i32, String, String),
    /// It did not run, or did not finish; the status line says why.
    Failed(String),
}

impl Env {
    /// True when hooks of `event` run in this conversation: never in a guest one; in plan mode
    /// only before_tool.
    fn hooks_run(&self, conv: &Conv, event: &str) -> bool {
        !conv.guest && (event == "before_tool" || conv.permission != Permission::Plan)
    }

    /// Run one hook command with the event in `TEMPO_HOOK_EVENT`, in the conversation's folder.
    /// A command `refusal` rejects (deleting, a protected service, the deny list) does not run.
    fn run_hook(&self, conv: &Conv, hook: &Hook, event: &str, json: &str) -> Ended {
        let shown = redact(&head_chars(hook.command.trim(), 60));
        if let Some(why) = self.refusal(&hook.command) {
            return Ended::Failed(format!(
                "{event} hook `{shown}` did not run ({})",
                why.trim_start_matches("refused: ")
            ));
        }
        let secs = self.cfg.hooks.timeout();
        let r = shell_env(
            &hook.command,
            &self.cwd(conv),
            secs,
            &[("TEMPO_HOOK_EVENT", json)],
        );
        if let Some(e) = r.spawn_error {
            return Ended::Failed(format!("{event} hook `{shown}` could not start: {e}"));
        }
        if r.timed_out {
            return Ended::Failed(format!(
                "{event} hook `{shown}` timed out (hook_timeout_s {:.0}s) and was killed",
                secs.as_secs_f64()
            ));
        }
        Ended::Exit(r.code, r.stdout, r.stderr)
    }

    /// before_tool hooks for a call about to run. Exit code 2 from a hook blocks the call (the
    /// result becomes "blocked by hook: <its stderr>"). Any other failure (another exit code, a
    /// timeout, a hook that cannot start, a command that is refused) only adds a status line and
    /// the tool runs: a refused hook is not trusted to say no, and the line says it did not run.
    pub fn hooks_before(&self, conv: &Conv, tool: &str, args: &Value) -> HookOutcome {
        let mut out = HookOutcome::default();
        if !self.hooks_run(conv, "before_tool") {
            return out;
        }
        let mut json = None;
        for hook in self
            .cfg
            .hooks
            .before_tool
            .iter()
            .filter(|h| h.applies(tool))
        {
            let ev = json.get_or_insert_with(|| {
                event_json("before_tool", &conv.id, Some(tool), Some(args), None)
            });
            match self.run_hook(conv, hook, "before_tool", ev) {
                Ended::Exit(2, _, err) => {
                    let why = redact(&head_chars(err.trim(), BLOCK_REASON_CAP));
                    let why = if why.is_empty() {
                        "the hook gave no reason".to_string()
                    } else {
                        why
                    };
                    out.notes
                        .push(format!("before_tool hook blocked {tool}: {}", head_chars(&why, NOTE_CAP)));
                    out.blocked = Some(format!("blocked by hook: {why}"));
                    return out;
                }
                Ended::Exit(0, ..) => {}
                Ended::Exit(code, ..) => out.notes.push(format!(
                    "before_tool hook `{}` exited with code {code} (only exit code 2 blocks); the tool still runs",
                    redact(&head_chars(hook.command.trim(), 60))
                )),
                Ended::Failed(note) => out.notes.push(format!("{note}; the tool still runs")),
            }
        }
        out
    }

    /// after_tool hooks for a call that ran: what a hook prints (trimmed, at most `APPEND_CAP`
    /// characters) goes on the end of the tool result. A failed hook only adds a status line.
    pub fn hooks_after(&self, conv: &Conv, tool: &str, args: &Value, result: &str) -> HookOutcome {
        let mut out = HookOutcome::default();
        if !self.hooks_run(conv, "after_tool") {
            return out;
        }
        let mut json = None;
        for hook in self.cfg.hooks.after_tool.iter().filter(|h| h.applies(tool)) {
            let ev = json.get_or_insert_with(|| {
                event_json("after_tool", &conv.id, Some(tool), Some(args), Some(result))
            });
            match self.run_hook(conv, hook, "after_tool", ev) {
                Ended::Exit(0, stdout, _) => {
                    let text = redact(&head_chars(stdout.trim(), APPEND_CAP));
                    if !text.is_empty() {
                        out.append.push_str(&format!("\n[hook] {text}"));
                    }
                }
                Ended::Exit(code, ..) => out.notes.push(format!(
                    "after_tool hook `{}` exited with code {code}; its output was not used",
                    redact(&head_chars(hook.command.trim(), 60))
                )),
                Ended::Failed(note) => out.notes.push(note),
            }
        }
        out
    }

    /// after_turn hooks, once when a turn ends. Their output is shown as status lines only.
    pub fn hooks_turn_end(&self, conv: &Conv) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.hooks_run(conv, "after_turn") {
            return notes;
        }
        let ev = event_json("after_turn", &conv.id, None, None, None);
        for hook in &self.cfg.hooks.after_turn {
            let shown = redact(&head_chars(hook.command.trim(), 60));
            match self.run_hook(conv, hook, "after_turn", &ev) {
                Ended::Exit(0, stdout, _) => {
                    let text = redact(&head_chars(stdout.trim(), NOTE_CAP));
                    if !text.is_empty() {
                        notes.push(format!("after_turn hook: {text}"));
                    }
                }
                Ended::Exit(code, ..) => {
                    notes.push(format!("after_turn hook `{shown}` exited with code {code}"))
                }
                Ended::Failed(note) => notes.push(note),
            }
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::conv::DenyAll;
    use crate::mcp::McpHost;
    use crate::util::testutil::tmpdir;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::time::Instant;

    fn env_with(hooks: Value) -> (Env, Conv, PathBuf) {
        let d = tmpdir("hooks");
        let mut cfg = Config::defaults(Some(d.to_string_lossy().into_owned()));
        cfg.trash_dir = tmpdir("hooks-trash");
        cfg.hooks = Hooks::from_json(&hooks).unwrap();
        let conv = Conv::open(&cfg, Some("hc")).unwrap();
        let e = Env::new(cfg.clone(), Box::new(DenyAll), McpHost::empty(&cfg));
        (e, conv, d)
    }

    #[test]
    fn the_config_is_read_and_a_mistake_is_an_error() {
        let h = Hooks::from_json(&json!({
            "hooks": {"before_tool": [{"match": "^run_", "command": "a"}],
                      "after_tool": [{"command": "b"}], "after_turn": [{"command": "c"}]},
            "hook_timeout_s": 5}))
        .unwrap();
        assert_eq!(
            (h.before_tool.len(), h.after_tool.len(), h.after_turn.len()),
            (1, 1, 1)
        );
        assert_eq!(h.timeout(), Duration::from_secs(5));
        assert_eq!(
            Hooks::from_json(&json!({})).unwrap().timeout(),
            Duration::from_secs(30)
        );
        for bad in [
            json!({"hooks": {"before_tools": []}}),
            json!({"hooks": {"before_tool": [{"match": "(", "command": "x"}]}}),
            json!({"hooks": {"before_tool": [{"match": "x"}]}}),
            json!({"hooks": {"before_tool": [{"command": ""}]}}),
            json!({"hooks": {"after_turn": {"command": "x"}}}),
            json!({"hooks": []}),
            json!({"hook_timeout_s": 0}),
            json!({"hook_timeout_s": "soon"}),
        ] {
            assert!(Hooks::from_json(&bad).is_err(), "{bad}");
        }
        // and the whole config refuses to load with one
        assert!(Config::from_json(&json!({"hooks": {"nope": []}}), None).is_err());
    }

    #[test]
    fn match_selects_tools_by_regex_on_the_name() {
        let h = Hooks::from_json(&json!({"hooks": {"before_tool": [
            {"match": "^(run_command|write_file)$", "command": "x"}, {"command": "y"}]}}))
        .unwrap();
        let (m, all) = (&h.before_tool[0], &h.before_tool[1]);
        assert!(m.applies("run_command") && m.applies("write_file"));
        assert!(!m.applies("read_file") && !m.applies("run_command2"));
        assert!(all.applies("anything"));
    }

    #[test]
    fn the_event_stays_valid_json_under_16_kb_with_a_huge_payload() {
        let big = "\u{e9}\"\\".repeat(500_000);
        let args = json!({"command": big, "list": [big, "short"], "n": 1, "nested": {"x": big}});
        let t = event_json(
            "after_tool",
            "conv-1",
            Some("run_command"),
            Some(&args),
            Some(&big),
        );
        assert!(t.len() <= EVENT_CAP, "{} bytes", t.len());
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(
            (v["event"].as_str(), v["conv"].as_str()),
            (Some("after_tool"), Some("conv-1"))
        );
        assert_eq!(v["tool"], "run_command");
        assert_eq!(v["args"]["n"], 1);
        assert_eq!(v["args"]["list"][1], "short");
        assert!(v["args"]["command"].as_str().unwrap().ends_with("...[cut]"));
        assert!(!v["result"].as_str().unwrap().is_empty());
        // small events are untouched, and unused fields are null
        let t = event_json("after_turn", "c", None, None, None);
        assert_eq!(
            t,
            r#"{"args":null,"conv":"c","event":"after_turn","result":null,"tool":null}"#
        );
        // thousands of small strings that cannot be cut to nothing still fit
        let many = json!({"a": (0..5000).map(|i| format!("value number {i}")).collect::<Vec<_>>()});
        let t = event_json("before_tool", "c", Some("t"), Some(&many), None);
        assert!(t.len() <= EVENT_CAP);
        serde_json::from_str::<Value>(&t).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_gets_the_event_in_its_environment() {
        let (e, c, d) = env_with(json!({"hooks": {"before_tool": [
            {"command": "printf %s \"$TEMPO_HOOK_EVENT\" > event.json"}]}}));
        let huge = json!({"command": "x".repeat(1_000_000)});
        let o = e.hooks_before(&c, "run_command", &huge);
        assert!(o.blocked.is_none() && o.notes.is_empty(), "{o:?}");
        let raw = std::fs::read_to_string(d.join("event.json")).unwrap();
        assert!(raw.len() <= EVENT_CAP);
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            (v["event"].as_str(), v["conv"].as_str(), v["tool"].as_str()),
            (Some("before_tool"), Some("hc"), Some("run_command"))
        );
        assert!(v["args"]["command"].as_str().unwrap().len() < 20_000);
        assert!(v["result"].is_null());
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_2_blocks_and_other_failures_do_not() {
        let (e, c, _d) = env_with(json!({"hooks": {"before_tool": [
            {"match": "^a$", "command": "echo 'not today' >&2; exit 2"},
            {"match": "^b$", "command": "echo oops >&2; exit 1"},
            {"match": "^c$", "command": "exit 2"}]}}));
        let o = e.hooks_before(&c, "a", &json!({}));
        assert_eq!(o.blocked.as_deref(), Some("blocked by hook: not today"));
        assert!(o.notes[0].contains("blocked a"));
        let o = e.hooks_before(&c, "b", &json!({}));
        assert!(o.blocked.is_none());
        assert!(
            o.notes[0].contains("exited with code 1") && o.notes[0].contains("tool still runs")
        );
        let o = e.hooks_before(&c, "c", &json!({}));
        assert_eq!(
            o.blocked.as_deref(),
            Some("blocked by hook: the hook gave no reason")
        );
        // the reason is capped
        let (e, c, _d) = env_with(json!({"hooks": {"before_tool": [
            {"command": "head -c 5000 /dev/zero | tr '\\0' z >&2; exit 2"}]}}));
        let r = e.hooks_before(&c, "a", &json!({})).blocked.unwrap();
        assert_eq!(r.chars().count(), "blocked by hook: ".len() + 500);
    }

    #[cfg(unix)]
    #[test]
    fn after_tool_output_is_appended_and_capped() {
        let (e, c, _d) = env_with(json!({"hooks": {"after_tool": [
            {"match": "^a$", "command": "echo '  lint ok  '"},
            {"match": "^b$", "command": "head -c 5000 /dev/zero | tr '\\0' q"},
            {"match": "^c$", "command": "echo text; exit 1"}]}}));
        assert_eq!(
            e.hooks_after(&c, "a", &json!({}), "res").append,
            "\n[hook] lint ok"
        );
        let o = e.hooks_after(&c, "b", &json!({}), "res");
        assert_eq!(o.append, format!("\n[hook] {}", "q".repeat(APPEND_CAP)));
        let o = e.hooks_after(&c, "c", &json!({}), "res");
        assert!(
            o.append.is_empty() && o.notes[0].contains("exited with code 1"),
            "{o:?}"
        );
        assert!(e
            .hooks_after(&c, "other", &json!({}), "res")
            .append
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_hook_is_killed_and_only_adds_a_status_line() {
        let (e, c, _d) = env_with(json!({"hook_timeout_s": 1, "hooks": {"before_tool": [
            {"command": "echo start; exec sleep 20"}]}}));
        let t = Instant::now();
        let o = e.hooks_before(&c, "run_command", &json!({}));
        assert!(t.elapsed() < Duration::from_secs(8));
        assert!(o.blocked.is_none());
        assert!(
            o.notes[0].contains("timeout") && o.notes[0].contains("killed"),
            "{o:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn no_hook_runs_in_a_guest_conversation() {
        let (e, mut c, d) = env_with(json!({"hooks": {
            "before_tool": [{"command": "touch before"}],
            "after_tool": [{"command": "touch after; echo hi"}],
            "after_turn": [{"command": "touch turn"}]}}));
        c.guest = true;
        assert!(e
            .hooks_before(&c, "run_command", &json!({}))
            .blocked
            .is_none());
        assert!(e
            .hooks_after(&c, "run_command", &json!({}), "r")
            .append
            .is_empty());
        assert!(e.hooks_turn_end(&c).is_empty());
        for f in ["before", "after", "turn"] {
            assert!(!d.join(f).exists(), "{f} ran in a guest conversation");
        }
        c.guest = false;
        e.hooks_before(&c, "run_command", &json!({}));
        assert!(
            d.join("before").exists(),
            "the same hook runs for the owner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn in_plan_mode_only_before_tool_hooks_run() {
        let (e, mut c, d) = env_with(json!({"hooks": {
            "before_tool": [{"command": "touch before"}],
            "after_tool": [{"command": "touch after"}],
            "after_turn": [{"command": "touch turn"}]}}));
        c.permission = Permission::Plan;
        e.hooks_before(&c, "read_file", &json!({}));
        e.hooks_after(&c, "read_file", &json!({}), "r");
        e.hooks_turn_end(&c);
        assert!(d.join("before").exists());
        assert!(!d.join("after").exists() && !d.join("turn").exists());
        c.permission = Permission::Default;
        e.hooks_after(&c, "read_file", &json!({}), "r");
        e.hooks_turn_end(&c);
        assert!(d.join("after").exists() && d.join("turn").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_command_the_refusals_reject_never_runs_and_never_blocks() {
        let (e, c, d) = env_with(json!({"hooks": {
            "before_tool": [{"command": "touch ran; rm somefile"}],
            "after_tool": [{"command": "touch ran2; rm somefile"}],
            "after_turn": [{"command": "touch ran3; rm somefile"}]}}));
        let o = e.hooks_before(&c, "run_command", &json!({}));
        assert!(o.blocked.is_none());
        assert!(
            o.notes[0].contains("did not run") && o.notes[0].contains("deleting"),
            "{o:?}"
        );
        let o = e.hooks_after(&c, "run_command", &json!({}), "r");
        assert!(o.append.is_empty() && o.notes[0].contains("did not run"));
        assert!(e.hooks_turn_end(&c)[0].contains("did not run"));
        for f in ["ran", "ran2", "ran3"] {
            assert!(!d.join(f).exists(), "{f}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_owners_deny_list_covers_hooks_too() {
        let d = tmpdir("hooks-deny");
        let mut cfg = Config::defaults(Some(d.to_string_lossy().into_owned()));
        cfg.command_deny = vec!["touch".into()];
        cfg.hooks = Hooks::from_json(&json!({"hooks": {"after_turn": [{"command": "touch ran"}]}}))
            .unwrap();
        let conv = Conv::open(&cfg, Some("hc")).unwrap();
        let e = Env::new(cfg.clone(), Box::new(DenyAll), McpHost::empty(&cfg));
        assert!(e.hooks_turn_end(&conv)[0].contains("command_deny"));
        assert!(!d.join("ran").exists());
    }

    #[test]
    fn a_hook_that_cannot_start_only_adds_a_status_line() {
        // a folder that cannot be a working directory: the shell fails to start
        let (mut e, c, d) = env_with(json!({"hooks": {"after_turn": [{"command": "echo hi"}]}}));
        let file = d.join("a-file");
        std::fs::write(&file, "x").unwrap();
        e.cfg.command_cwd = file.join("below");
        let notes = e.hooks_turn_end(&c);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("could not start") || notes[0].contains("exited"),
            "{notes:?}"
        );
    }
}
