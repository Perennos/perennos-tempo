//! Takes: one judged attempt per line in `<agent data>/takes/takes.jsonl` (the contract is in
//! docs/WOODSHED.md). Other programs write most of them, so this reader is forgiving: a line it
//! cannot read is counted and skipped, a field that is missing gets a plain default, and a run log
//! with kinds it has never seen still gives up its prompt and answer when it has them.

use crate::setup::json::{parse, J};
use crate::Ctx;
use std::path::{Component, Path};

#[derive(Clone, Debug, PartialEq)]
pub struct Take {
    pub id: String,
    pub run: String,
    pub band: Option<String>,
    pub part: Option<String>,
    pub task: String,
    pub engine: String,
    pub model: String,
    /// Who wrote the answer's words: local, cloud or owner.
    pub author: String,
    pub prompt_ref: String,
    pub answer_ref: String,
    /// accept, reject or none.
    pub owner: Option<String>,
    pub judge: Option<f64>,
    pub tests: Option<bool>,
    pub winner: bool,
    pub pair: Option<String>,
    pub ts: String,
}

fn opt_str(j: &J, k: &str) -> Option<String> {
    j.str(k)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// One line of takes.jsonl. Only a take id is required.
pub fn parse_take(line: &str) -> Option<Take> {
    let j = parse(line.trim()).ok()?;
    let id = opt_str(&j, "take")?;
    let checks = j.get("checks").cloned().unwrap_or(J::Null);
    let author = match j.str("author") {
        Some(a @ ("local" | "cloud" | "owner")) => a.to_string(),
        // an author we do not know is never assumed to be local: it stays out of training
        Some(other) => other.to_string(),
        None => "unknown".to_string(),
    };
    Some(Take {
        id,
        run: opt_str(&j, "run").unwrap_or_default(),
        band: opt_str(&j, "band"),
        part: opt_str(&j, "part"),
        task: opt_str(&j, "task").unwrap_or_default(),
        engine: opt_str(&j, "engine").unwrap_or_default(),
        model: opt_str(&j, "model").unwrap_or_default(),
        author,
        prompt_ref: opt_str(&j, "prompt_ref").unwrap_or_default(),
        answer_ref: opt_str(&j, "answer_ref").unwrap_or_default(),
        owner: opt_str(&checks, "owner"),
        judge: checks.f64("judge"),
        tests: checks.bool("tests"),
        winner: j.bool("winner") == Some(true),
        pair: opt_str(&j, "pair"),
        ts: opt_str(&j, "ts").unwrap_or_default(),
    })
}

pub fn takes_file(ctx: &Ctx) -> std::path::PathBuf {
    ctx.agent_data.join("takes").join("takes.jsonl")
}

/// Every take, the last line for a take id winning (an owner who changes their mind appends a
/// new line), in the order the ids first appeared. The second number counts lines that could
/// not be read.
pub fn read_takes(ctx: &Ctx) -> (Vec<Take>, usize) {
    let text = std::fs::read_to_string(takes_file(ctx)).unwrap_or_default();
    let (mut out, mut bad): (Vec<Take>, usize) = (vec![], 0);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match parse_take(line) {
            Some(t) => match out.iter().position(|x| x.id == t.id) {
                Some(i) => out[i] = t,
                None => out.push(t),
            },
            None => bad += 1,
        }
    }
    (out, bad)
}

/// The band a take belongs to as a file key: its own name, or `default`.
pub fn key(t: &Take) -> &str {
    t.band.as_deref().unwrap_or("default")
}

// ---- reading what a take points at

/// What a take said and was asked: the messages before the answer, and the answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Example {
    pub messages: Vec<(String, String)>,
    pub answer: String,
}

/// `runs/<date>/<run>.jsonl#<n>` (line n of the log, counting from 0) or
/// `conversations/<id>.json#<n>` (event n of the conversation). Anything else, or a path that
/// leaves the agent's data folder, gives None.
pub fn split_ref(r: &str) -> Option<(&str, usize)> {
    let (path, n) = r.rsplit_once('#')?;
    let n: usize = n.trim().parse().ok()?;
    let p = Path::new(path);
    let plain = !path.is_empty()
        && p.is_relative()
        && p.components().all(|c| matches!(c, Component::Normal(_)));
    let known = path.starts_with("runs/") || path.starts_with("conversations/");
    (plain && known).then_some((path, n))
}

fn record(ctx: &Ctx, r: &str) -> Option<J> {
    let (path, n) = split_ref(r)?;
    let file = ctx.agent_data.join(path);
    let text = std::fs::read_to_string(file).ok()?;
    if path.ends_with(".jsonl") {
        parse(text.lines().nth(n)?).ok()
    } else {
        parse(&text).ok()?.arr("events").get(n).cloned()
    }
}

/// The first text found under any of `keys`, however the other program named it.
fn first_text(j: &J, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| j.str(k))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(String::from)
}

fn message_list(j: &J) -> Vec<(String, String)> {
    j.arr("messages")
        .iter()
        .filter_map(|m| {
            let role = m.str("role")?;
            let content = m.str("content").map(str::trim).filter(|c| !c.is_empty())?;
            matches!(role, "system" | "user" | "assistant").then(|| (role.into(), content.into()))
        })
        .collect()
}

const PROMPT_KEYS: &[&str] = &["prompt", "input", "task", "text", "content", "message"];
const ANSWER_KEYS: &[&str] = &["answer", "output", "response", "text", "content", "result"];

/// The messages the model was given at `r`.
pub fn prompt_at(ctx: &Ctx, r: &str) -> Option<Vec<(String, String)>> {
    let j = record(ctx, r)?;
    let mut m = Vec::new();
    if let Some(sys) = first_text(&j, &["system"]) {
        m.push(("system".to_string(), sys));
    }
    let listed = message_list(&j);
    if listed.is_empty() {
        m.push(("user".to_string(), first_text(&j, PROMPT_KEYS)?));
    } else {
        m.extend(listed);
    }
    Some(m)
}

/// What the model said at `r`.
pub fn answer_at(ctx: &Ctx, r: &str) -> Option<String> {
    let j = record(ctx, r)?;
    first_text(&j, ANSWER_KEYS).or_else(|| {
        message_list(&j)
            .into_iter()
            .rev()
            .find(|(role, _)| role == "assistant")
            .map(|(_, c)| c)
    })
}

/// A take's prompt and answer together, or None when either cannot be read.
pub fn example_of(ctx: &Ctx, t: &Take) -> Option<Example> {
    Some(Example {
        messages: prompt_at(ctx, &t.prompt_ref)?,
        answer: answer_at(ctx, &t.answer_ref)?,
    })
}

#[cfg(test)]
pub struct TestDir(std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    fn create(path: std::path::PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    pub fn new(tag: &str) -> Self {
        assert!(tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..128 {
            let path = std::env::temp_dir().join(format!(
                "tempo-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match Self::create(path) {
                Ok(dir) => return dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot create test directory: {e}"),
            }
        }
        panic!("could not create an exclusive test directory after 128 collisions");
    }
}

#[cfg(test)]
impl std::ops::Deref for TestDir {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                if std::thread::panicking() {
                    eprintln!(
                        "could not remove owned test directory {}: {e}",
                        self.0.display()
                    );
                } else {
                    panic!(
                        "could not remove owned test directory {}: {e}",
                        self.0.display()
                    );
                }
            }
        }
    }
}

#[cfg(test)]
pub struct TestCtx {
    ctx: Ctx,
    _dir: TestDir,
}

#[cfg(test)]
impl std::ops::Deref for TestCtx {
    type Target = Ctx;
    fn deref(&self) -> &Ctx {
        &self.ctx
    }
}

#[cfg(test)]
pub fn test_ctx(tag: &str) -> TestCtx {
    let d = TestDir::new(&format!("woodshed-{tag}"));
    let ctx = Ctx {
        home: d.to_path_buf(),
        agent_data: d.join("agent"),
        vault: d.join("vault"),
        bin_dirs: vec![],
        browser: false,
        opened: Default::default(),
    };
    TestCtx { ctx, _dir: d }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_directories_are_exclusive_and_cleaned_on_drop_and_unwind() {
        let survivor = TestDir::new("woodshed-owner");
        let existing = survivor.join("preexisting");
        std::fs::create_dir(&existing).unwrap();
        std::fs::write(existing.join("sentinel"), "untouched").unwrap();
        let error = TestDir::create(existing.clone()).err().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(existing.join("sentinel")).unwrap(),
            "untouched"
        );
        let mut removed = std::path::PathBuf::new();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = TestDir::new("woodshed-owner");
            assert_ne!(dir.as_ref(), survivor.as_ref());
            removed = dir.to_path_buf();
            std::fs::write(dir.join("owned"), "x").unwrap();
            panic!("exercise unwind cleanup");
        }));
        assert!(!removed.exists());
        assert!(survivor.is_dir());
        let path = survivor.to_path_buf();
        drop(survivor);
        assert!(!path.exists());
    }

    #[test]
    fn a_line_with_only_an_id_is_still_a_take() {
        let t = parse_take(r#"{"take":"t1"}"#).unwrap();
        assert_eq!(
            (t.id.as_str(), t.winner, t.author.as_str()),
            ("t1", false, "unknown")
        );
        assert!(t.band.is_none() && t.owner.is_none() && t.judge.is_none());
        assert!(parse_take("not json").is_none());
        assert!(parse_take(r#"{"v":1}"#).is_none(), "no id, no take");
        assert!(parse_take("").is_none());
    }

    #[test]
    fn the_whole_contract_line_is_read() {
        let t = parse_take(
            r#"{"v":1,"take":"a","run":"r1","band":"social-media","part":"lead","task":"write","engine":"local","model":"qwen3:8b","author":"local","prompt_ref":"runs/2026-11-07/r1.jsonl#2","answer_ref":"runs/2026-11-07/r1.jsonl#3","checks":{"tests":true,"soundcheck":{"high":0,"med":1,"low":2},"judge":0.9,"owner":"accept"},"winner":true,"pair":"b","ts":"2026-11-07T10:00:00Z","something_new":[1]}"#,
        )
        .unwrap();
        assert_eq!(t.band.as_deref(), Some("social-media"));
        assert_eq!(
            (t.tests, t.judge, t.owner.as_deref()),
            (Some(true), Some(0.9), Some("accept"))
        );
        assert!(t.winner);
        assert_eq!(t.pair.as_deref(), Some("b"));
        // null band and null checks are fine
        let n = parse_take(r#"{"take":"x","band":null,"checks":null,"winner":true}"#).unwrap();
        assert_eq!((n.band, n.winner), (None, true));
    }

    #[test]
    fn the_last_line_for_a_take_wins_and_bad_lines_are_counted() {
        let ctx = test_ctx("takes");
        std::fs::create_dir_all(ctx.agent_data.join("takes")).unwrap();
        std::fs::write(
            takes_file(&ctx),
            "{\"take\":\"a\",\"winner\":true}\nnope\n\n{\"take\":\"b\"}\n{\"take\":\"a\",\"winner\":false}\n{\"no\":1}\n",
        )
        .unwrap();
        let (v, bad) = read_takes(&ctx);
        assert_eq!(
            v.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(!v[0].winner);
        assert_eq!(bad, 2);
        // no file at all is no takes
        let none = test_ctx("none");
        assert_eq!(read_takes(&none), (vec![], 0));
    }

    #[test]
    fn refs_stay_inside_the_agents_folder() {
        assert_eq!(
            split_ref("runs/2026-11-07/r.jsonl#4"),
            Some(("runs/2026-11-07/r.jsonl", 4))
        );
        assert_eq!(
            split_ref("conversations/c1.json#0"),
            Some(("conversations/c1.json", 0))
        );
        for bad in [
            "runs/../../etc/passwd#1",
            "/etc/passwd#1",
            "runs/x.jsonl",
            "runs/x.jsonl#-1",
            "secrets/key.json#0",
            "runs/x.jsonl#a",
            "C:/x#1",
        ] {
            assert_eq!(split_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn run_logs_with_unknown_kinds_still_give_prompt_and_answer() {
        let ctx = test_ctx("runlog");
        let d = ctx.agent_data.join("runs/2026-11-07");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("r1.jsonl"),
            [
                r#"{"kind":"start","run":"r1"}"#,
                r#"{"kind":"prompt","system":"You write hooks.","messages":[{"role":"user","content":"hook for a synth"},{"role":"tool","content":"x"}]}"#,
                r#"{"kind":"a-kind-from-the-future","answer":"Hear it first.","extra":{"a":1}}"#,
                r#"{"kind":"model_turn","messages":[{"role":"user","content":"q"},{"role":"assistant","content":"from the list"}]}"#,
                r#"{"kind":"note","prompt":"plain prompt text"}"#,
                "this line is not json",
                r#"{"kind":"empty"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let at = |n: usize| format!("runs/2026-11-07/r1.jsonl#{n}");
        assert_eq!(
            prompt_at(&ctx, &at(1)).unwrap(),
            vec![
                ("system".to_string(), "You write hooks.".to_string()),
                ("user".to_string(), "hook for a synth".to_string())
            ]
        );
        assert_eq!(answer_at(&ctx, &at(2)).unwrap(), "Hear it first.");
        assert_eq!(answer_at(&ctx, &at(3)).unwrap(), "from the list");
        assert_eq!(prompt_at(&ctx, &at(4)).unwrap()[0].1, "plain prompt text");
        // a line that is not JSON, one with nothing in it, one past the end: no text, no panic
        for n in [5, 6, 99] {
            assert!(answer_at(&ctx, &at(n)).is_none());
            assert!(prompt_at(&ctx, &at(n)).is_none());
        }
        assert!(answer_at(&ctx, "runs/2026-11-07/missing.jsonl#0").is_none());
    }

    #[test]
    fn conversation_events_are_read_by_number() {
        let ctx = test_ctx("conv");
        std::fs::create_dir_all(ctx.agent_data.join("conversations")).unwrap();
        std::fs::write(
            ctx.agent_data.join("conversations/c1.json"),
            r#"{"events":[{"n":0,"type":"user","text":"caption please"},{"n":1,"type":"status","text":"lane: fast"},{"n":2,"type":"assistant","text":"Out now."}]}"#,
        )
        .unwrap();
        let t = parse_take(
            r#"{"take":"owner-c1-2","prompt_ref":"conversations/c1.json#0","answer_ref":"conversations/c1.json#2","winner":true}"#,
        )
        .unwrap();
        let e = example_of(&ctx, &t).unwrap();
        assert_eq!(
            e.messages,
            vec![("user".to_string(), "caption please".to_string())]
        );
        assert_eq!(e.answer, "Out now.");
    }
}
