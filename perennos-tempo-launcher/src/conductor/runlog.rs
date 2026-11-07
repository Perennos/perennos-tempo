//! The run log: `<agent data>/runs/YYYY-MM-DD/<run-id>.jsonl`, one event per line.
//!
//! `{v:1, ts, run, parent, depth, member, engine, model, kind, ..., prev_hash, hash}`. `hash` is
//! SHA-256 over `prev_hash` plus the line as written up to the hash, so a line that was changed,
//! dropped or moved shows up when the chain is walked (`verify`). The first line's `prev_hash` is
//! 64 zeros. Every string goes through `redact` before it is written, and a long one is cut.
//!
//! A run that stopped can be picked up again: `done_steps` gives back every finished step by its
//! idempotency key, and the Conductor plays only the ones that are not in it.

use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::{civil, redact, Ctx};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const MAX_TEXT: usize = 20_000;

pub const KINDS: [&str; 6] = ["plan", "tool", "result", "final", "verdict", "stop"];

/// One event, before it gets its time, its place in the chain and its hash.
pub struct Ev<'a> {
    /// The section this one belongs under (None at the top).
    pub parent: Option<&'a str>,
    pub depth: u8,
    pub member: &'a str,
    pub engine: &'a str,
    pub model: &'a str,
    pub kind: &'a str,
    pub fields: Vec<(&'a str, J)>,
}

struct State {
    prev: String,
    n: usize,
    file: std::fs::File,
}

pub struct RunLog {
    pub run: String,
    pub path: PathBuf,
    /// `runs/<date>/<run>.jsonl`, how a take points back at its lines.
    rel: String,
    state: Mutex<State>,
}

pub fn runs_dir(ctx: &Ctx) -> PathBuf {
    ctx.agent_data.join("runs")
}

pub fn date_of(now: u64) -> String {
    let (y, m, d, ..) = civil(now);
    format!("{y:04}-{m:02}-{d:02}")
}

/// RFC 3339, in UTC.
pub fn rfc3339(now: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(now);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn cut_text(s: &str) -> String {
    let s = redact(s);
    if s.chars().count() <= MAX_TEXT {
        s
    } else {
        format!("{} [cut]", s.chars().take(MAX_TEXT).collect::<String>())
    }
}

fn clean(j: &J) -> J {
    match j {
        J::Str(s) => J::Str(cut_text(s)),
        J::Arr(a) => J::Arr(a.iter().map(clean).collect()),
        J::Obj(o) => J::Obj(o.iter().map(|(k, v)| (k.clone(), clean(v))).collect()),
        other => other.clone(),
    }
}

fn opt(s: Option<&str>) -> J {
    s.map_or(J::Null, J::s)
}

impl RunLog {
    /// A new log for `run`, started at `now`.
    pub fn create(ctx: &Ctx, run: &str, now: u64) -> std::io::Result<RunLog> {
        let rel = format!("runs/{}/{run}.jsonl", date_of(now));
        let path = ctx.agent_data.join(&rel);
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        Ok(RunLog {
            run: run.into(),
            path,
            rel,
            state: Mutex::new(State {
                prev: GENESIS.into(),
                n: 0,
                file,
            }),
        })
    }

    /// The log of an earlier run, to carry on in. It must still be an unbroken chain.
    pub fn reopen(ctx: &Ctx, run: &str) -> Result<RunLog, String> {
        let path = find(ctx, run).ok_or_else(|| {
            format!("no run called {run}. `tempo-launch conductor runs` lists the recent ones")
        })?;
        let n = verify(&path)
            .map_err(|e| format!("{run}'s log is not intact ({e}); not carrying on from it"))?;
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let prev = text
            .lines()
            .last()
            .and_then(|l| parse(l).ok())
            .and_then(|j| j.str("hash").map(String::from))
            .unwrap_or_else(|| GENESIS.into());
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let rel = format!(
            "runs/{}/{run}.jsonl",
            path.parent()
                .and_then(|p| p.file_name())
                .map_or(String::new(), |d| d.to_string_lossy().into_owned())
        );
        Ok(RunLog {
            run: run.into(),
            path,
            rel,
            state: Mutex::new(State { prev, n, file }),
        })
    }

    /// Write one event; the number of its line (from 1).
    pub fn emit(&self, now: u64, e: Ev) -> usize {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut o = vec![
            ("v".to_string(), J::n(1.0)),
            ("ts".into(), J::s(&rfc3339(now))),
            ("run".into(), J::s(&self.run)),
            ("parent".into(), opt(e.parent)),
            ("depth".into(), J::n(f64::from(e.depth))),
            ("member".into(), J::s(e.member)),
            ("engine".into(), J::s(e.engine)),
            ("model".into(), J::s(e.model)),
            ("kind".into(), J::s(e.kind)),
        ];
        for (k, v) in e.fields {
            o.push((k.to_string(), clean(&v)));
        }
        o.push(("prev_hash".into(), J::Str(st.prev.clone())));
        let line = J::Obj(o).compact();
        let body = &line[..line.len() - 1];
        let hash = sha256_hex(format!("{}{body}", st.prev).as_bytes());
        // one write per line, so a stop in the middle leaves whole lines
        let _ = writeln!(st.file, "{body},\"hash\":\"{hash}\"}}");
        st.prev = hash;
        st.n += 1;
        st.n
    }

    /// How a take points at line `n`: `runs/<date>/<run>.jsonl#<n>`.
    pub fn reference(&self, n: usize) -> String {
        format!("{}#{n}", self.rel)
    }
}

/// The file of run `run`, in whichever day's folder it is.
pub fn find(ctx: &Ctx, run: &str) -> Option<PathBuf> {
    if run.is_empty() || !run.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    std::fs::read_dir(runs_dir(ctx))
        .ok()?
        .flatten()
        .map(|d| d.path().join(format!("{run}.jsonl")))
        .find(|p| p.is_file())
}

/// Walk the chain: every line's `prev_hash` is the line before's `hash`, and every `hash` is the
/// SHA-256 of what it covers. The number of lines, or what is wrong and where.
pub fn verify(path: &Path) -> Result<usize, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut prev = GENESIS.to_string();
    let mut n = 0;
    for (i, line) in text.lines().enumerate() {
        let at = i + 1;
        let cut = line
            .rfind(",\"hash\":\"")
            .ok_or_else(|| format!("line {at} has no hash"))?;
        let (body, tail) = line.split_at(cut);
        let claimed = tail
            .strip_prefix(",\"hash\":\"")
            .and_then(|t| t.strip_suffix("\"}"))
            .ok_or_else(|| format!("line {at} ends the wrong way"))?;
        if !body.contains(&format!("\"prev_hash\":\"{prev}\"")) {
            return Err(format!("line {at} does not follow line {}", at - 1));
        }
        if sha256_hex(format!("{prev}{body}").as_bytes()) != claimed {
            return Err(format!("line {at} was changed"));
        }
        prev = claimed.to_string();
        n += 1;
    }
    Ok(n)
}

/// Every event of a log, as it was written.
pub fn read(path: &Path) -> Vec<J> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| parse(l).ok())
        .collect()
}

/// A finished step: the line it is on, and the event.
pub struct Done {
    pub n: usize,
    pub ev: J,
}

/// The steps a log already has, by idempotency key (`idem`), for picking a run up again.
pub fn done_steps(path: &Path) -> HashMap<String, Done> {
    read(path)
        .into_iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(e.str("kind"), Some("result" | "tool")) && e.str("error").is_none()
        })
        .filter_map(|(i, e)| {
            let k = e.str("idem")?.to_string();
            Some((k, Done { n: i + 1, ev: e }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::tests::{ctx_in, tmp};

    fn ev<'a>(kind: &'a str, text: &str) -> Ev<'a> {
        Ev {
            parent: None,
            depth: 0,
            member: "m",
            engine: "local",
            model: "x",
            kind,
            fields: vec![("text", J::s(text))],
        }
    }

    #[test]
    fn lines_chain_and_a_changed_line_breaks_the_chain() {
        let root = tmp("runlog");
        let ctx = ctx_in(&root, false);
        let log = RunLog::create(&ctx, "r-1", 1_793_000_000).unwrap();
        assert_eq!(log.emit(1_793_000_000, ev("plan", "one")), 1);
        assert_eq!(log.emit(1_793_000_001, ev("result", "two")), 2);
        log.emit(1_793_000_002, ev("final", "three"));
        assert_eq!(verify(&log.path).unwrap(), 3);
        assert!(
            log.path.ends_with("runs/2026-10-26/r-1.jsonl")
                || log.path.to_string_lossy().contains("/runs/20")
        );
        assert_eq!(log.reference(2), "runs/2026-10-26/r-1.jsonl#2");
        let text = std::fs::read_to_string(&log.path).unwrap();
        let first = parse(text.lines().next().unwrap()).unwrap();
        assert_eq!(first.str("prev_hash"), Some(GENESIS));
        assert_eq!(first.f64("v"), Some(1.0));
        for k in ["ts", "run", "member", "engine", "model", "kind", "hash"] {
            assert!(first.str(k).is_some(), "{k}");
        }
        assert!(first.get("parent") == Some(&J::Null) && first.f64("depth") == Some(0.0));
        // change one word
        std::fs::write(&log.path, text.replacen("two", "twp", 1)).unwrap();
        assert!(verify(&log.path)
            .unwrap_err()
            .contains("line 2 was changed"));
        // drop the middle line
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(&log.path, format!("{}\n{}\n", lines[0], lines[2])).unwrap();
        assert!(verify(&log.path).unwrap_err().contains("does not follow"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn secrets_are_redacted_and_long_text_is_cut_before_writing() {
        let root = tmp("runlog-redact");
        let ctx = ctx_in(&root, false);
        let log = RunLog::create(&ctx, "r-2", 1_793_000_000).unwrap();
        // built at run time so the leak scan never sees a key in the source
        let key = format!("sk-{}", "a1".repeat(15));
        log.emit(1_793_000_000, ev("result", &format!("my key is {key} ok")));
        log.emit(1_793_000_000, ev("result", &"x".repeat(MAX_TEXT + 500)));
        let text = std::fs::read_to_string(&log.path).unwrap();
        assert!(!text.contains(&key), "the key was written");
        assert!(text.contains("[cut]"));
        assert_eq!(verify(&log.path).unwrap(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_log_can_be_reopened_and_its_finished_steps_found() {
        let root = tmp("runlog-reopen");
        let ctx = ctx_in(&root, false);
        let log = RunLog::create(&ctx, "r-3", 1_793_000_000).unwrap();
        let mut e = ev("result", "answer");
        e.fields.push(("idem", J::s("1/t1/r1")));
        log.emit(1_793_000_000, e);
        let mut failed = ev("result", "");
        failed.fields.push(("idem", J::s("2/t1/r1")));
        failed.fields.push(("error", J::s("engine down")));
        log.emit(1_793_000_000, failed);
        drop(log);
        let again = RunLog::reopen(&ctx, "r-3").unwrap();
        assert_eq!(again.emit(1_793_000_005, ev("final", "end")), 3);
        assert_eq!(verify(&again.path).unwrap(), 3);
        let done = done_steps(&again.path);
        assert_eq!(done.len(), 1, "a failed step is not finished");
        assert_eq!(done["1/t1/r1"].n, 1);
        assert!(RunLog::reopen(&ctx, "nope").is_err());
        assert!(find(&ctx, "../x").is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
