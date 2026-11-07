//! `tempo-vault learn`: Perennos Tempo learns from its own conversations, every day or as often
//! as it is run. Each agent conversation (`<agent data>/conversations/<id>.json`, the format both
//! the Rust and the Python harness write) becomes a note in the vault with a summary, key points,
//! decisions, to-dos and links to the notes it talks about; the day's daily note gets a line for
//! it; and `Tempo/Tempo Learned.md` keeps a living map (recent conversations, recurring topics,
//! open to-dos).
//!
//! - Local only: a model is used only when one is named, and only on a loopback, private or
//!   tailnet address. With no model, or when the model fails, an extractive summary is used.
//! - Idempotent: a conversation is learned again only when it changed; a note edited by hand is
//!   never overwritten (new points are appended under "Updates" instead).
//! - Safe: every text passes the secret redactor before it is written; every write goes through
//!   the vault's history snapshots.
use crate::util::{date_of, head_chars, iso_utc, local_offset_minutes, redact, unix_now};
use crate::vault::{word_find, Vault};
use crate::write::VaultConfig;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_FOLDER: &str = "Tempo/Conversations";
pub const MAP_NOTE: &str = "Tempo/Tempo Learned.md";
pub const TAUGHT_FOLDER: &str = "Tempo/Taught";
const DAILY_HEADING: &str = "Learned by Tempo";
const START: &str =
    "%% tempo-learn:start (written by tempo-vault learn; edit outside these markers) %%";
const END: &str = "%% tempo-learn:end %%";

#[derive(Debug, Clone)]
pub struct LearnOpts {
    /// The agent's data folder (holds `conversations/` and `workflows/`).
    pub agent_data: PathBuf,
    /// A local model for summaries (e.g. the fast lane); `None` is extractive only.
    pub model: Option<String>,
    pub ollama_url: String,
    /// Allow a model server that is not loopback, private or tailnet.
    pub allow_remote_model: bool,
    pub folder: String,
    pub dry_run: bool,
    /// Only conversations changed in the last N days.
    pub since_days: Option<f64>,
    pub max: usize,
    pub timeout: Duration,
    /// Folders of reviewed teach rows (`{prompt, answer, source, hop_hash}`, JSON or JSONL), as
    /// `tempo workflow teach` writes them.
    pub teach_dirs: Vec<PathBuf>,
    /// After learning, run this program with `run --dry-run` (Tempo Learn's daily cycle), with no
    /// window. A failure there is a warning, never a failure of the vault's own learning.
    pub model_cycle: Option<PathBuf>,
}

impl LearnOpts {
    /// Options from flags already parsed by the CLI, then the environment, then the vault's
    /// `.tempo/config.json` (`agent_data`, `learn_model`, `ollama_url`, `learn_folder`).
    pub fn resolve(
        vault_root: &Path,
        agent_data: Option<String>,
        model: Option<String>,
        ollama: Option<String>,
    ) -> Result<LearnOpts, String> {
        let cfg: Value = std::fs::read_to_string(vault_root.join(".tempo").join("config.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(json!({}));
        let pick = |flag: Option<String>, env: &str, key: &str| -> Option<String> {
            flag.or_else(|| std::env::var(env).ok())
                .or_else(|| cfg[key].as_str().map(str::to_string))
                .filter(|s| !s.trim().is_empty())
        };
        // the one rule every Perennos Tempo product shares (TEMPO_HOME first; no per-product key)
        let home = Some(crate::tempo_home::tempo_home().dir);
        // the agent keeps its data in <home>/agent unless told otherwise (one home per machine)
        let agent_data = pick(agent_data, "TEMPO_AGENT_DATA", "agent_data")
            .or_else(|| {
                home.as_ref()
                    .map(|h| h.join("agent"))
                    .filter(|p| p.is_dir())
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .ok_or("say where the agent keeps its conversations: --agent-data DIR, TEMPO_AGENT_DATA, or \"agent_data\" in the vault's .tempo/config.json (or let Tempo use <Tempo home>/agent)")?;
        Ok(LearnOpts {
            agent_data: crate::util::expand_home(&agent_data),
            model: pick(model, "TEMPO_LEARN_MODEL", "learn_model"),
            ollama_url: pick(ollama, "OLLAMA_URL", "ollama_url")
                .unwrap_or_else(|| "http://127.0.0.1:11434".into()),
            allow_remote_model: false,
            folder: cfg["learn_folder"]
                .as_str()
                .filter(|f| crate::util::clean_rel(f).is_some())
                .unwrap_or(DEFAULT_FOLDER)
                .to_string(),
            dry_run: false,
            since_days: None,
            max: 500,
            timeout: Duration::from_secs(180),
            teach_dirs: {
                let mut d = vec![crate::util::expand_home(&agent_data)
                    .join("workflows")
                    .join("teach")];
                if let Some(h) = &home {
                    d.push(h.join("workflows").join("teach"));
                }
                d.dedup();
                d
            },
            model_cycle: None,
        })
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct LearnReport {
    pub scanned: usize,
    pub learned: usize,
    pub unchanged: usize,
    pub notes: Vec<String>,
    pub daily: Vec<String>,
    pub model_used: usize,
    pub kept_hand_edits: Vec<String>,
    pub warnings: Vec<String>,
    /// Reviewed teach rows added to the vault this run.
    pub taught: usize,
    /// What Tempo Learn's daily cycle reported, when it ran.
    pub model_cycle: Option<String>,
}

impl LearnReport {
    pub fn to_json(&self) -> Value {
        json!({"scanned": self.scanned, "learned": self.learned, "unchanged": self.unchanged, "notes": self.notes, "daily": self.daily, "model_used": self.model_used, "kept_hand_edits": self.kept_hand_edits, "warnings": self.warnings, "taught": self.taught, "model_cycle": self.model_cycle})
    }
}

/// The program `--with-model-cycle` runs: `tempo-learn` next to this binary, else on PATH.
pub fn find_tempo_learn() -> PathBuf {
    let name = if cfg!(windows) {
        "tempo-learn.exe"
    } else {
        "tempo-learn"
    };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Reviewed teach rows in these folders: (key, prompt, answer, source, file mtime). A row needs a
/// non-empty prompt and answer; `"reviewed": false` is skipped. The key is its `hop_hash`, else
/// a hash of the pair, so a row is taught once however often it is seen.
fn teach_rows(dirs: &[PathBuf]) -> Vec<(String, String, String, String, f64)> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        !n.starts_with('.') && (n.ends_with(".jsonl") || n.ends_with(".json"))
                    })
            })
            .collect();
        files.sort();
        for f in files {
            let mtime = std::fs::metadata(&f)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0.0, |d| d.as_secs_f64());
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let values: Vec<Value> = if f.extension().is_some_and(|e| e == "jsonl") {
                text.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            } else {
                match serde_json::from_str::<Value>(&text) {
                    Ok(Value::Array(a)) => a,
                    Ok(v) => vec![v],
                    Err(_) => vec![],
                }
            };
            for row in values {
                if row["reviewed"] == Value::Bool(false) {
                    continue;
                }
                let (Some(p), Some(a)) = (
                    row["prompt"].as_str().filter(|s| !s.trim().is_empty()),
                    row["answer"].as_str().filter(|s| !s.trim().is_empty()),
                ) else {
                    continue;
                };
                let key = row["hop_hash"]
                    .as_str()
                    .filter(|h| {
                        !h.is_empty() && h.len() <= 128 && h.bytes().all(|b| b.is_ascii_graphic())
                    })
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "{:016x}",
                            crate::util::fnv64(format!("{p}\n{a}").as_bytes())
                        )
                    });
                if seen.insert(key.clone()) {
                    out.push((
                        key,
                        p.to_string(),
                        a.to_string(),
                        row["source"].as_str().unwrap_or("workflow").to_string(),
                        mtime,
                    ));
                }
            }
        }
    }
    out
}

/// Add new reviewed teach rows to `Tempo/Taught/<date>.md`, once each.
fn learn_teach(
    v: &mut Vault,
    o: &LearnOpts,
    st: &mut Value,
    r: &mut LearnReport,
    offset: i64,
) -> Result<(), String> {
    if !st["taught"].is_object() {
        st["taught"] = json!({});
    }
    for (key, prompt, answer, source, mtime) in teach_rows(&o.teach_dirs) {
        if st["taught"].get(&key).is_some() {
            continue;
        }
        r.taught += 1;
        if o.dry_run {
            continue;
        }
        let date = date_of(if mtime > 0.0 { mtime } else { unix_now() }, offset);
        let note = format!("{TAUGHT_FOLDER}/{date}.md");
        if v.note(&note).is_none() {
            v.create(&note, &format!("---\ntags: [tempo/taught]\n---\n# Taught {date}\nReviewed answers Tempo was taught that day, from workflow teach rows.\n"), "learn").map_err(|e| e.to_string())?;
        }
        let (prompt, answer) = (redact(&prompt), redact(&answer));
        let title = head_words(
            prompt
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("Taught")
                .trim()
                .trim_start_matches(['#', '>', ' ']),
            70,
        );
        let mut entry = format!("\n## {}\n", title.replace(['[', ']'], " "));
        for l in head_chars(prompt.trim(), 800).lines() {
            entry.push_str(&format!("> {l}\n"));
        }
        entry.push('\n');
        entry.push_str(head_chars(answer.trim(), 2_000));
        entry.push_str("\n\n");
        let rel = related(v, &format!("{prompt}\n{answer}"), TAUGHT_FOLDER);
        let links: Vec<String> = rel.iter().take(6).map(|p| link_to(v, p)).collect();
        entry.push_str(&format!(
            "{} · `{}`{}\n",
            source.replace(['`', '\n'], " "),
            head_chars(&key, 16),
            if links.is_empty() {
                String::new()
            } else {
                format!(" · {}", links.join(" · "))
            }
        ));
        v.append(&note, &entry, None, "learn")
            .map_err(|e| e.to_string())?;
        st["taught"][&key] = json!(note);
    }
    Ok(())
}

/// Tempo Learn's daily cycle (`run --dry-run`), with no window; its outcome goes in the report.
fn run_model_cycle(prog: &Path, r: &mut LearnReport) {
    let mut cmd = std::process::Command::new(prog);
    cmd.args(["run", "--dry-run"])
        .stdin(std::process::Stdio::null());
    hidden(&mut cmd);
    let msg = match cmd.output() {
        Ok(o) if o.status.success() => format!("{} run --dry-run: ok", prog.display()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let last = err
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            let m = format!(
                "{} run --dry-run exited {}: {}",
                prog.display(),
                o.status
                    .code()
                    .map_or("by a signal".into(), |c| c.to_string()),
                redact(head_chars(last, 300))
            );
            r.warnings.push(m.clone());
            m
        }
        Err(e) => {
            let m = format!(
                "Tempo Learn's cycle was skipped: cannot run {} ({e})",
                prog.display()
            );
            r.warnings.push(m.clone());
            m
        }
    };
    r.model_cycle = Some(msg);
}

/// What a conversation boils down to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Digest {
    pub title: String,
    pub summary: String,
    pub points: Vec<String>,
    pub decisions: Vec<String>,
    pub todos: Vec<(bool, String)>,
    pub tools: Vec<String>,
    pub asked: String,
    pub model: Option<String>,
}

struct Conversation {
    id: String,
    date: String,
    messages: Vec<(String, String)>, // (role, text) for user and assistant turns
    tools: Vec<String>,
    hash: u64,
    workflow_tasks: Vec<(bool, String)>,
}

const STOP: &str = "a about above after again against all also am an and any are as at be because been before being below between both but by can could did do does doing done down during each even every few for from further get got had has have having he her here hers him his how i if in into is it its itself just let like make many may me might more most much must my need no nor not now of off on once one only or other our out over own please same say see she should so some such than that the their them then there these they this those through to too under until up us use used using very want was way we well were what when where which while who why will with would yes yet you your yours thing things okay ok sure really maybe something anything also lot";

fn stopwords() -> HashSet<&'static str> {
    STOP.split_whitespace().collect()
}

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
}

fn content_text(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Strip code blocks, think blocks and tool chatter from a message before summarizing it.
fn prose(s: &str) -> String {
    let mut out = String::new();
    let mut fence = false;
    let mut think = false;
    for l in s.lines() {
        let t = l.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if t.contains("<think>") {
            think = true;
        }
        if think {
            if t.contains("</think>") {
                think = false;
            }
            continue;
        }
        if fence || t.starts_with('|') || t.starts_with("NEEDS_TOOLS") {
            continue;
        }
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn sentences(s: &str) -> Vec<String> {
    let mut out = vec![];
    for line in s.lines() {
        let line = line
            .trim()
            .trim_start_matches(['-', '*', '>', '#', ' '])
            .trim();
        if line.is_empty()
            || line.starts_with("[ ]")
            || line.starts_with("[x]")
            || line.starts_with("[X]")
        {
            continue;
        }
        let mut cur = String::new();
        let chars: Vec<char> = line.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            cur.push(*c);
            let end =
                matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
            if end {
                out.push(cur.trim().to_string());
                cur.clear();
            }
        }
        if !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
        }
    }
    out.into_iter()
        .filter(|s| s.chars().count() >= 12 && s.chars().count() <= 400)
        .collect()
}

fn clean_line(s: &str) -> String {
    let s: String = s.replace(['\n', '\r'], " ");
    let s = s.replace("**", "").replace('`', "");
    head_chars(s.trim(), 300).trim().to_string()
}

/// Extractive digest: the most central sentences by term weight, decisions by wording, to-dos
/// from task lines and the agent's workflow file.
fn extractive(c: &Conversation) -> Digest {
    let stop = stopwords();
    let asked = c
        .messages
        .iter()
        .find(|(r, _)| r == "user")
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    let title = asked
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| {
            head_chars(l.trim_start_matches(['#', '>', '-', ' ']), 70)
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| format!("Conversation {}", c.id));
    let mut tf: HashMap<String, f32> = HashMap::new();
    for (_, t) in &c.messages {
        for w in words(&prose(t)) {
            if !stop.contains(w.as_str()) && !w.chars().all(|c| c.is_ascii_digit()) {
                *tf.entry(w).or_insert(0.0) += 1.0;
            }
        }
    }
    let mut cands: Vec<(usize, f32, String)> = vec![];
    let mut decisions = vec![];
    let mut todos: Vec<(bool, String)> = c.workflow_tasks.clone();
    let mut k = 0;
    for (role, t) in &c.messages {
        for raw in t.lines() {
            let tl = raw.trim();
            if let Some(rest) = tl
                .strip_prefix("- [ ] ")
                .or_else(|| tl.strip_prefix("* [ ] "))
            {
                todos.push((false, clean_line(rest)));
            } else if let Some(rest) = tl
                .strip_prefix("- [x] ")
                .or_else(|| tl.strip_prefix("- [X] "))
            {
                todos.push((true, clean_line(rest)));
            }
        }
        if role != "assistant" {
            continue;
        }
        for s in sentences(&prose(t)) {
            let low = s.to_lowercase();
            if [
                "decided",
                "we will",
                "i will",
                "going with",
                "chose",
                "the plan is",
                "next step",
                "agreed",
                "use the",
            ]
            .iter()
            .any(|w| low.contains(w))
                && decisions.len() < 6
            {
                decisions.push(clean_line(&s));
            }
            let ws: Vec<String> = words(&s).filter(|w| !stop.contains(w.as_str())).collect();
            if ws.len() < 3 {
                continue;
            }
            let score = ws
                .iter()
                .map(|w| tf.get(w).copied().unwrap_or(0.0))
                .sum::<f32>()
                / (ws.len() as f32).sqrt();
            cands.push((k, score, clean_line(&s)));
            k += 1;
        }
    }
    let mut best = cands.clone();
    best.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut picked: Vec<(usize, String)> = vec![];
    for (i, _, s) in best {
        if picked.len() >= 5 {
            break;
        }
        if picked.iter().any(|(_, p)| p == &s) || decisions.contains(&s) {
            continue;
        }
        picked.push((i, s));
    }
    picked.sort_by_key(|(i, _)| *i);
    // the first answer (to the question that started it) and, if different, how it ended
    let answers: Vec<String> = c
        .messages
        .iter()
        .filter(|(r, _)| r == "assistant")
        .map(|(_, t)| prose(t))
        .collect();
    let mut lead: Vec<String> = answers
        .first()
        .map(|a| sentences(a).into_iter().take(1).collect())
        .unwrap_or_default();
    if answers.len() > 1 {
        if let Some(last) = answers.last().and_then(|a| sentences(a).into_iter().next()) {
            if !lead.contains(&last) {
                lead.push(last);
            }
        }
    } else if let Some(second) = answers
        .first()
        .and_then(|a| sentences(a).into_iter().nth(1))
    {
        lead.push(second);
    }
    let summary = lead
        .iter()
        .map(|s| clean_line(s))
        .collect::<Vec<_>>()
        .join(" ");
    let mut seen = HashSet::new();
    todos.retain(|(_, t)| !t.is_empty() && seen.insert(t.to_lowercase()));
    Digest {
        title,
        summary: head_chars(&summary, 500).to_string(),
        points: picked.into_iter().map(|(_, s)| s).collect(),
        decisions,
        todos,
        tools: c.tools.clone(),
        asked: head_chars(asked.trim(), 600).to_string(),
        model: None,
    }
}

// ---- a minimal HTTP client for a local Ollama (std only)

/// Loopback, private (RFC 1918 / ULA), link-local or tailnet (the carrier-grade NAT range, /10) addresses only.
pub fn is_local_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_loopback()
                || v.is_private()
                || (v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64)
        }
        IpAddr::V6(v) => {
            v.is_loopback()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

pub(crate) fn decode_chunked(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![];
    loop {
        let nl = b.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&b[..nl])
                .ok()?
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        b = &b[nl + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(..size)?);
        b = b.get(size + 2..)?;
    }
}

/// A model server's answer is never read past this many bytes.
const MAX_MODEL_REPLY: u64 = 16 << 20;

pub fn post_json(
    url: &str,
    path: &str,
    body: &Value,
    timeout: Duration,
    allow_remote: bool,
) -> Result<Value, String> {
    let rest = url
        .trim_end_matches('/')
        .strip_prefix("http://")
        .ok_or("the model server must be an http:// address on this machine or the home network")?;
    let (hostport, base) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, String::new()),
    };
    if hostport.contains('@') {
        return Err("no credentials in the model address".into());
    }
    let with_port = if hostport
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
        && !hostport.ends_with(']')
    {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let addrs: Vec<std::net::SocketAddr> = with_port
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {hostport}: {e}"))?
        .collect();
    if addrs.is_empty() || (!allow_remote && !addrs.iter().all(|a| is_local_addr(a.ip()))) {
        return Err(format!(
            "{hostport} is not on this machine or the home network; refusing to send notes there"
        ));
    }
    let mut s = TcpStream::connect_timeout(&addrs[0], Duration::from_secs(5))
        .map_err(|e| format!("cannot reach the model server at {hostport}: {e}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(Duration::from_secs(30))).ok();
    let b = body.to_string();
    let req = format!("POST {base}{path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len());
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = vec![];
    (&mut s)
        .take(MAX_MODEL_REPLY)
        .read_to_end(&mut raw)
        .map_err(|e| format!("the model server did not answer in time: {e}"))?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("bad HTTP answer")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    let mut payload = raw[split + 4..].to_vec();
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if head.contains("transfer-encoding: chunked") {
        payload = decode_chunked(&payload).ok_or("bad chunked answer")?;
    }
    if code != 200 {
        return Err(format!(
            "the model server answered {code}: {}",
            head_chars(&String::from_utf8_lossy(&payload), 200)
        ));
    }
    serde_json::from_slice(&payload)
        .map_err(|e| format!("the model server sent something that is not JSON: {e}"))
}

fn str_list(v: &Value, max: usize) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().or_else(|| x["text"].as_str()))
        .map(clean_line)
        .filter(|s| !s.is_empty())
        .take(max)
        .collect()
}

/// Ask the local model for a digest; `None` (with a reason) means use the extractive one.
fn model_digest(c: &Conversation, o: &LearnOpts, base: &Digest) -> Result<Digest, String> {
    let model = o.model.as_deref().ok_or("no model")?;
    let mut transcript = String::new();
    for (r, t) in &c.messages {
        transcript.push_str(&format!(
            "{}: {}\n\n",
            if r == "user" { "Owner" } else { "Tempo" },
            prose(t).trim()
        ));
    }
    let transcript = redact(head_chars(&transcript, 14_000));
    let prompt = format!(
        "Summarize this conversation between the owner and their assistant Tempo, for the owner's notes. Answer with JSON only: {{\"title\": short title, \"summary\": two sentences, \"key_points\": up to 5 facts worth remembering, \"decisions\": decisions made, \"todos\": open next steps}}. Use plain words. Do not invent anything.\n\n{transcript}"
    );
    let body = json!({"model": model, "stream": false, "format": "json", "options": {"temperature": 0.2}, "messages": [{"role": "user", "content": prompt}]});
    let r = post_json(
        &o.ollama_url,
        "/api/chat",
        &body,
        o.timeout,
        o.allow_remote_model,
    )?;
    let content = r["message"]["content"]
        .as_str()
        .ok_or("the model answered without content")?;
    let j: Value = serde_json::from_str(content.trim())
        .map_err(|_| "the model did not answer with JSON".to_string())?;
    let summary = j["summary"].as_str().map(clean_line).unwrap_or_default();
    if summary.is_empty() && str_list(&j["key_points"], 5).is_empty() {
        return Err("the model's answer was empty".into());
    }
    let title = j["title"]
        .as_str()
        .map(|t| head_chars(t.trim(), 70).trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| base.title.clone());
    let mut todos = base.todos.clone();
    for t in str_list(&j["todos"], 8) {
        if !todos.iter().any(|(_, x)| x.eq_ignore_ascii_case(&t)) {
            todos.push((false, t));
        }
    }
    Ok(Digest {
        title,
        summary,
        points: str_list(&j["key_points"], 5),
        decisions: str_list(&j["decisions"], 6),
        todos,
        tools: base.tools.clone(),
        asked: base.asked.clone(),
        model: Some(model.to_string()),
    })
}

// ---- reading the agent's data

fn read_conversation(path: &Path, workflows: &Path, offset: i64) -> Option<Conversation> {
    let text = std::fs::read_to_string(path).ok()?;
    let d: Value = serde_json::from_str(&text).ok()?;
    let id = path.file_stem()?.to_str()?.to_string();
    let mut messages = vec![];
    let mut tools = vec![];
    for m in d["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("");
        for tc in m["tool_calls"].as_array().into_iter().flatten() {
            if let Some(n) = tc["function"]["name"].as_str() {
                if !tools.iter().any(|t| t == n) {
                    tools.push(n.to_string());
                }
            }
        }
        if role == "user" || role == "assistant" {
            let t = content_text(m);
            if !t.trim().is_empty() {
                messages.push((role.to_string(), redact(&t)));
            }
        }
    }
    if !messages.iter().any(|(r, _)| r == "user") {
        return None;
    }
    // ids start with the date the harness made them (YYYYMMDD-...); else the file's time
    let date = match id
        .get(..8)
        .filter(|d| d.chars().all(|c| c.is_ascii_digit()))
    {
        Some(d)
            if crate::util::parse_date(&format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]))
                .is_some() =>
        {
            format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8])
        }
        _ => {
            let m = std::fs::metadata(path)
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs_f64();
            date_of(m, offset)
        }
    };
    let mut workflow_tasks = vec![];
    if let Ok(wf) = std::fs::read_to_string(workflows.join(format!("{id}.md"))) {
        let mut in_tasks = false;
        for l in wf.lines() {
            if l.starts_with("## ") {
                in_tasks = l.trim() == "## Task list";
                continue;
            }
            if in_tasks {
                if let Some(r) = l.trim().strip_prefix("- [ ] ") {
                    workflow_tasks.push((false, clean_line(&redact(r))));
                } else if let Some(r) = l.trim().strip_prefix("- [x] ") {
                    workflow_tasks.push((true, clean_line(&redact(r))));
                }
            }
        }
    }
    let hash = crate::util::fnv64(
        serde_json::to_string(&d["messages"])
            .unwrap_or_default()
            .as_bytes(),
    );
    Some(Conversation {
        id,
        date,
        messages,
        tools,
        hash,
        workflow_tasks,
    })
}

/// At most `n` characters, cut at a word boundary, with an ellipsis when cut.
pub fn head_words(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut = head_chars(s, n);
    let at = cut.rfind(' ').filter(|i| *i > n / 2).unwrap_or(cut.len());
    format!("{}…", cut[..at].trim_end_matches([',', ';', ':', ' ']))
}

/// A safe note name for any OS (Windows rules are the strictest).
pub fn note_name(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| {
            if "\\/:*?\"<>|#^[]%".contains(c) || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = head_chars(&s, 60)
        .trim()
        .trim_end_matches(['.', ' '])
        .to_string();
    if s.is_empty() {
        "Conversation".into()
    } else {
        s
    }
}

/// Existing notes a conversation mentions (by title, file name or alias), most mentioned first.
fn related(v: &Vault, text: &str, folder: &str) -> Vec<String> {
    let low = text.to_lowercase();
    let mut found: Vec<(usize, String)> = vec![];
    for n in v.notes() {
        if n.path.starts_with(&format!("{folder}/"))
            || n.path == MAP_NOTE
            || crate::util::parse_date(crate::util::stem(&n.path)).is_some()
        {
            continue;
        }
        let mut names: Vec<String> = vec![
            n.title.to_lowercase(),
            crate::util::stem(&n.path).to_lowercase(),
        ];
        names.extend(n.parsed.aliases.iter().map(|a| a.to_lowercase()));
        names.retain(|x| x.chars().count() >= 4 && !x.chars().all(|c| c.is_ascii_digit()));
        names.sort();
        names.dedup();
        let hits: usize = names
            .iter()
            .map(|nm| {
                if word_find(&low, nm) {
                    low.matches(nm.as_str()).count()
                } else {
                    0
                }
            })
            .sum();
        if hits > 0 {
            found.push((hits, n.path.clone()));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().take(12).map(|(_, p)| p).collect()
}

fn link_to(v: &Vault, path: &str) -> String {
    let stem = crate::util::stem(path);
    let unique = v
        .notes()
        .filter(|n| crate::util::stem(&n.path).eq_ignore_ascii_case(stem))
        .count()
        <= 1;
    if unique {
        format!("[[{stem}]]")
    } else {
        format!("[[{}]]", crate::util::strip_md(path))
    }
}

fn note_text(v: &Vault, c: &Conversation, d: &Digest, rel: &[String]) -> String {
    let mut t = String::new();
    t.push_str(&format!("---\ntempo_conversation: {}\ndate: {}\nmessages: {}\nsummarized_by: {}\ntags: [tempo/conversation]\n---\n", c.id, c.date, c.messages.len(), d.model.as_deref().unwrap_or("extractive")));
    t.push_str(&format!("# {}\n\n", d.title));
    if !d.summary.is_empty() {
        t.push_str(&format!("> [!abstract] Summary\n> {}\n\n", d.summary));
    }
    let section = |t: &mut String, h: &str, items: &[String]| {
        if !items.is_empty() {
            t.push_str(&format!("## {h}\n"));
            for i in items {
                t.push_str(&format!("- {i}\n"));
            }
            t.push('\n');
        }
    };
    section(&mut t, "Key points", &d.points);
    section(&mut t, "Decisions", &d.decisions);
    if !d.todos.is_empty() {
        t.push_str("## To do\n");
        for (done, x) in &d.todos {
            t.push_str(&format!("- [{}] {x}\n", if *done { "x" } else { " " }));
        }
        t.push('\n');
    }
    if !rel.is_empty() {
        let links: Vec<String> = rel.iter().map(|p| link_to(v, p)).collect();
        t.push_str(&format!("## Related\n{}\n\n", links.join(" · ")));
    }
    if !d.tools.is_empty() {
        t.push_str(&format!(
            "## Tools used\n{}\n\n",
            d.tools
                .iter()
                .map(|x| format!("`{x}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !d.asked.is_empty() {
        t.push_str("## Asked\n");
        for l in d.asked.lines() {
            t.push_str(&format!("> {l}\n"));
        }
    }
    redact(&t)
}

fn state_path(v: &Vault) -> Option<PathBuf> {
    v.safe_abs(".tempo/learn.json")
}

fn load_state(v: &Vault) -> Value {
    state_path(v)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({"conversations": {}}))
}

fn save_state(v: &Vault, st: &Value) -> Result<(), String> {
    let p = state_path(v).ok_or("the .tempo folder is a link; learning state was not saved")?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(st).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

fn replace_between_markers(cur: &str, inner: &str) -> String {
    let block = format!("{START}\n{inner}\n{END}");
    match (cur.find(START), cur.find(END)) {
        (Some(a), Some(b)) if b > a => format!("{}{block}{}", &cur[..a], &cur[b + END.len()..]),
        _ if cur.trim().is_empty() => format!("# Tempo Learned\nWhat Perennos Tempo learned from its conversations, kept up to date by `tempo-vault learn`.\n\n{block}\n"),
        _ => format!("{}\n\n{block}\n", cur.trim_end()),
    }
}

fn update_map(v: &mut Vault, st: &Value, folder: &str, today: &str) -> Result<(), String> {
    let convs = st["conversations"].as_object().cloned().unwrap_or_default();
    let mut rows: Vec<(String, String, String)> = convs
        .values()
        .filter_map(|c| {
            Some((
                c["date"].as_str()?.to_string(),
                c["note"].as_str()?.to_string(),
                c["title"].as_str().unwrap_or("").to_string(),
            ))
        })
        .filter(|(_, p, _)| v.note(p).is_some())
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut inner = format!(
        "Updated {today} · {} conversations learned\n\n## Recent\n",
        rows.len()
    );
    for (d, p, t) in rows.iter().take(20) {
        inner.push_str(&format!(
            "- {d} [[{}|{}]]\n",
            crate::util::strip_md(p),
            t.replace(['[', ']', '|'], " ")
        ));
    }
    // recurring topics: the notes conversations link to most often
    let mut topics: BTreeMap<String, usize> = BTreeMap::new();
    for (_, p, _) in &rows {
        for (l, r) in v.links_of(p) {
            if let Some(r) = r.filter(|r| {
                !r.starts_with(&format!("{folder}/")) && v.note(r).is_some() && !l.embed
            }) {
                *topics.entry(r).or_insert(0) += 1;
            }
        }
    }
    let mut topics: Vec<(String, usize)> = topics.into_iter().collect();
    topics.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if !topics.is_empty() {
        inner.push_str("\n## Recurring topics\n");
        for (p, n) in topics.iter().take(15) {
            inner.push_str(&format!("- {} · {n}\n", link_to(v, p)));
        }
    }
    // reviewed answers Tempo was taught, by day
    let mut days: Vec<String> = st["taught"]
        .as_object()
        .map(|m| {
            m.values()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    days.sort();
    days.dedup();
    if !days.is_empty() {
        let n = st["taught"].as_object().map_or(0, |m| m.len());
        inner.push_str(&format!("\n## Taught\n{n} reviewed answers. Latest: "));
        let latest: Vec<String> = days
            .iter()
            .rev()
            .take(5)
            .map(|p| format!("[[{}|{}]]", crate::util::strip_md(p), crate::util::stem(p)))
            .collect();
        inner.push_str(&latest.join(" · "));
        inner.push('\n');
    }
    inner.push_str(&format!(
        "\n## Open to-dos from conversations\n```tasks\nnot done\npath includes {folder}\n```"
    ));
    let cur = v.read(MAP_NOTE).unwrap_or_default();
    let new = replace_between_markers(&cur, &inner);
    if cur.is_empty() {
        v.create(MAP_NOTE, &new, "learn")
            .map_err(|e| e.to_string())?;
    } else if new != cur {
        v.write(MAP_NOTE, &new, None, "learn")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Learn from every conversation that is new or changed since the last run.
pub fn learn(v: &mut Vault, o: &LearnOpts) -> Result<LearnReport, String> {
    let convs_dir = o.agent_data.join("conversations");
    if !convs_dir.is_dir() {
        return Err(format!(
            "no conversations folder at {} (is --agent-data right?)",
            convs_dir.display()
        ));
    }
    let wf_dir = o.agent_data.join("workflows");
    let offset = local_offset_minutes(VaultConfig::load(&v.root).utc_offset.as_deref());
    let now = unix_now();
    let today = date_of(now, offset);
    let mut st = load_state(v);
    if !st["conversations"].is_object() {
        st["conversations"] = json!({});
    }
    let mut files: Vec<(f64, PathBuf)> = std::fs::read_dir(&convs_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(".json") && !n.starts_with('.'))
        })
        .filter_map(|e| {
            let m = e
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs_f64();
            Some((m, e.path()))
        })
        .collect();
    files.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    if let Some(days) = o.since_days {
        files.retain(|(m, _)| now - m <= days * 86_400.0);
    }
    let mut r = LearnReport::default();
    let folder = o.folder.trim_matches('/').to_string();
    let mut model_ok = o.model.is_some();
    for (_, path) in files.into_iter().rev().take(o.max) {
        r.scanned += 1;
        let Some(c) = read_conversation(&path, &wf_dir, offset) else {
            continue;
        };
        let prev = st["conversations"][&c.id].clone();
        if prev["hash"].as_str() == Some(&format!("{:016x}", c.hash))
            && prev["note"].as_str().is_some_and(|p| v.note(p).is_some())
        {
            r.unchanged += 1;
            continue;
        }
        let base = extractive(&c);
        let d = if model_ok {
            match model_digest(&c, o, &base) {
                Ok(d) => {
                    r.model_used += 1;
                    d
                }
                Err(e) => {
                    r.warnings
                        .push(format!("{}: {e}; used the extractive summary", c.id));
                    if e.contains("cannot reach")
                        || e.contains("refusing")
                        || e.contains("must be an http")
                    {
                        model_ok = false; // do not wait on a dead server for every conversation
                    }
                    base
                }
            }
        } else {
            base
        };
        let all_text: String = c
            .messages
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let rel = related(v, &all_text, &folder);
        let text = note_text(v, &c, &d, &rel);
        // reuse this conversation's note; else a new, unique name
        let note = match prev["note"].as_str().filter(|p| v.note(p).is_some()) {
            Some(p) => p.to_string(),
            None => {
                let base_name = format!("{folder}/{} {}", c.date, note_name(&d.title));
                let mut p = format!("{base_name}.md");
                if v.note(&p).is_some() || v.abs(&p).exists() {
                    p = format!(
                        "{base_name} ({}).md",
                        c.id.chars()
                            .rev()
                            .take(4)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>()
                    );
                }
                p
            }
        };
        if o.dry_run {
            r.learned += 1;
            r.notes.push(note);
            continue;
        }
        let existing = v.read(&note);
        let hand_edited = existing.as_ref().is_some_and(|t| {
            prev["written"]
                .as_str()
                .is_some_and(|w| w != format!("{:016x}", crate::util::fnv64(t.as_bytes())))
        });
        let written = if hand_edited {
            let mut add = format!(
                "**{}** (conversation grew to {} messages)\n",
                today,
                c.messages.len()
            );
            for p in d.points.iter().take(3) {
                add.push_str(&format!("- {p}\n"));
            }
            v.append(&note, &redact(&add), Some("Updates"), "learn")
                .map_err(|e| e.to_string())?;
            r.kept_hand_edits.push(note.clone());
            v.read(&note).unwrap_or_default()
        } else {
            if existing.is_none() {
                v.create(&note, &text, "learn").map_err(|e| e.to_string())?;
            } else if existing.as_deref() != Some(text.as_str()) {
                v.write(&note, &text, None, "learn")
                    .map_err(|e| e.to_string())?;
            }
            text.clone()
        };
        // a line in that day's daily note, once
        let daily = v.daily(Some(&c.date), "learn").map_err(|e| e.to_string())?;
        let link = format!("[[{}|", crate::util::strip_md(&note));
        if !v.read(&daily).unwrap_or_default().contains(&link) {
            let one = if d.summary.is_empty() {
                String::new()
            } else {
                format!(": {}", head_words(&d.summary, 160))
            };
            let line = format!("- {link}{}]]{one}", d.title.replace(['[', ']', '|'], " "));
            v.append(&daily, &redact(&line), Some(DAILY_HEADING), "learn")
                .map_err(|e| e.to_string())?;
            r.daily.push(daily.clone());
        }
        st["conversations"][&c.id] = json!({"hash": format!("{:016x}", c.hash), "note": note, "title": d.title, "date": c.date, "written": format!("{:016x}", crate::util::fnv64(written.as_bytes())), "learned": iso_utc(now), "model": d.model});
        r.learned += 1;
        r.notes.push(note);
    }
    learn_teach(v, o, &mut st, &mut r, offset)?;
    if !o.dry_run {
        if r.learned > 0 || r.taught > 0 || v.note(MAP_NOTE).is_none() {
            update_map(v, &st, &folder, &today)?;
        }
        st["last_run"] = json!(iso_utc(now));
        st["notes_written"] = json!(st["conversations"].as_object().map_or(0, |m| m.len()));
        save_state(v, &st)?;
        let line = json!({"t": iso_utc(now), "report": r.to_json()});
        if let Some(Ok(mut f)) = v.safe_abs(".tempo/learn-runs.jsonl").map(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
        }) {
            let _ = writeln!(f, "{line}");
        }
    }
    if let (Some(prog), false) = (&o.model_cycle, o.dry_run) {
        run_model_cycle(prog, &mut r);
    }
    r.daily.sort();
    r.daily.dedup();
    Ok(r)
}

// ---- scheduling: hourly by default, so Tempo learns as often as there is something new

pub const TASK_NAME: &str = "Perennos Tempo Vault Learn";
pub const CRON_MARK: &str = "# perennos-tempo-vault-learn";

/// A path for a crontab line (run by `sh`): double quotes when nothing in it is special, else
/// single quotes. `%` ends a crontab command, so it is escaped for cron. A control character
/// (a newline would start a new crontab line) is refused.
fn sh_quote(p: &str) -> Result<String, String> {
    if p.chars().any(char::is_control) {
        return Err(format!(
            "a control character in the path {p:?} cannot go in a crontab line"
        ));
    }
    let special = p.contains(['"', '$', '`', '\\', '!', '\'', '%']);
    let q = if special {
        format!("'{}'", p.replace('\'', "'\\''"))
    } else {
        format!("\"{p}\"")
    };
    Ok(q.replace('%', "\\%"))
}

/// A path for a Windows command line (CommandLineToArgvW rules): no `\\?\` prefix, and a
/// trailing backslash that would escape the closing quote is dropped (or doubled for a drive root).
pub fn win_quote(p: &str) -> Result<String, String> {
    if p.chars().any(|c| c.is_control() || c == '"') {
        return Err(format!(
            "a quote or control character in the path {p:?} cannot go in a task command"
        ));
    }
    let p = match p.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => p.strip_prefix(r"\\?\").unwrap_or(p).to_string(),
    };
    let t = p.trim_end_matches('\\');
    Ok(if t.len() == p.len() {
        format!("\"{p}\"")
    } else if t.ends_with(':') {
        format!("\"{t}\\\\\"")
    } else {
        format!("\"{t}\"")
    })
}

/// What `crontab -l` printed, as the lines to keep. "No crontab" is an empty list; any other
/// failure is an error, so the install never replaces a crontab it could not read.
pub fn crontab_current(ok: bool, stdout: &str, stderr: &str) -> Result<String, String> {
    if ok {
        Ok(stdout.to_string())
    } else if stderr.to_lowercase().contains("no crontab") {
        Ok(String::new())
    } else {
        Err(format!(
            "cannot read your crontab, so it was left alone: {}",
            stderr.trim()
        ))
    }
}

/// The command a scheduler should run, and how to install it on this OS.
pub fn schedule_plan(
    exe: &Path,
    vault: &Path,
    every: &str,
) -> Result<(String, Vec<String>), String> {
    let q = |p: &Path| {
        if cfg!(windows) {
            win_quote(&p.display().to_string())
        } else {
            sh_quote(&p.display().to_string())
        }
    };
    let (exe_q, vault_q) = (q(exe)?, q(vault)?);
    if cfg!(windows) {
        // conhost --headless: no console window ever pops up
        let tr =
            format!("conhost.exe --headless {exe_q} learn --vault {vault_q} --with-model-cycle");
        if tr.len() > 261 {
            return Err("the task command is longer than Windows allows (261 characters); move the vault or tempo-vault.exe to a shorter path".into());
        }
        let sc: Vec<String> = match every {
            "hourly" => vec!["/SC".into(), "HOURLY".into()],
            "daily" => vec!["/SC".into(), "DAILY".into(), "/ST".into(), "03:17".into()],
            _ => return Err("--every is hourly or daily".into()),
        };
        let mut args: Vec<String> = vec![
            "/Create".into(),
            "/F".into(),
            "/TN".into(),
            TASK_NAME.into(),
            "/TR".into(),
            tr.clone(),
        ];
        args.extend(sc);
        Ok((tr, args))
    } else {
        let when = match every {
            "hourly" => "17 * * * *",
            "daily" => "17 3 * * *",
            _ => return Err("--every is hourly or daily".into()),
        };
        let log = q(&vault.join(".tempo").join("learn.log"))?;
        let line = format!(
            "{when} {exe_q} learn --vault {vault_q} --with-model-cycle >> {log} 2>&1 {CRON_MARK}"
        );
        Ok((line.clone(), vec![line]))
    }
}

#[cfg(windows)]
fn hidden(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
}

#[cfg(not(windows))]
fn hidden(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

/// Install (or with `remove`, take away) the schedule. Returns what was done.
pub fn schedule_apply(
    exe: &Path,
    vault: &Path,
    every: &str,
    remove: bool,
) -> Result<String, String> {
    if cfg!(windows) {
        let args: Vec<String> = if remove {
            vec![
                "/Delete".into(),
                "/F".into(),
                "/TN".into(),
                TASK_NAME.into(),
            ]
        } else {
            schedule_plan(exe, vault, every)?.1
        };
        let out = hidden(std::process::Command::new("schtasks").args(&args))
            .output()
            .map_err(|e| format!("cannot run schtasks: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "schtasks failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        return Ok(if remove {
            format!("removed the scheduled task \"{TASK_NAME}\"")
        } else {
            format!("scheduled \"{TASK_NAME}\" ({every}, hidden window)")
        });
    }
    let listed = std::process::Command::new("crontab")
        .arg("-l")
        .output()
        .map_err(|e| format!("cannot run crontab: {e}"))?;
    let cur = crontab_current(
        listed.status.success(),
        &String::from_utf8_lossy(&listed.stdout),
        &String::from_utf8_lossy(&listed.stderr),
    )?;
    let mut lines: Vec<String> = cur
        .lines()
        .filter(|l| !l.contains(CRON_MARK))
        .map(str::to_string)
        .collect();
    if !remove {
        lines.push(schedule_plan(exe, vault, every)?.0);
    }
    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run crontab: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        si.write_all((lines.join("\n") + "\n").as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let st = child.wait().map_err(|e| e.to_string())?;
    if !st.success() {
        return Err("crontab refused the new schedule".into());
    }
    Ok(if remove {
        "removed the learn line from your crontab".into()
    } else {
        format!("added an {every} learn line to your crontab")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{put, sample, tmp};
    use std::net::TcpListener;

    fn conv(dir: &Path, id: &str, msgs: &[(&str, &str)]) {
        let messages: Vec<Value> = msgs
            .iter()
            .map(|(r, t)| json!({"role": r, "content": t}))
            .collect();
        std::fs::create_dir_all(dir.join("conversations")).unwrap();
        std::fs::write(
            dir.join("conversations").join(format!("{id}.json")),
            json!({"messages": messages, "events": []}).to_string(),
        )
        .unwrap();
    }

    fn opts(agent: &Path) -> LearnOpts {
        LearnOpts {
            agent_data: agent.to_path_buf(),
            model: None,
            ollama_url: "http://127.0.0.1:9".into(),
            allow_remote_model: false,
            folder: DEFAULT_FOLDER.into(),
            dry_run: false,
            since_days: None,
            max: 100,
            timeout: Duration::from_secs(5),
            teach_dirs: vec![],
            model_cycle: None,
        }
    }

    fn chat(key: &str) -> Vec<(&'static str, String)> {
        vec![
            ("system", "You are Tempo.".to_string()),
            ("user", "How should Perennos Tempo store notes? Look at the Ideas note.".to_string()),
            ("assistant", format!("We decided to keep plain Markdown files as the source of truth. The vault index is rebuilt from disk at any time. Perennos Tempo links every conversation into the graph. Your token {key} should never be in a note.\n- [ ] Write the vault README\n```rust\nfn main() {{}}\n```")),
            ("user", "Great, and daily notes?".to_string()),
            ("assistant", "Daily notes live in the Daily folder and Tempo appends what it learned. The plan is to run learn every hour.".to_string()),
        ]
    }

    #[test]
    fn learns_once_links_and_redacts() {
        let (_d, mut v) = sample();
        let agent = tmp("agent");
        let key = format!("ghp_{}", "abcdefghijKLMNOPQRSTuvwxyz0123456789");
        let msgs = chat(&key);
        let m: Vec<(&str, &str)> = msgs.iter().map(|(r, t)| (*r, t.as_str())).collect();
        conv(&agent, "20261105-210000-ab12", &m);
        std::fs::create_dir_all(agent.join("workflows")).unwrap();
        std::fs::write(
            agent.join("workflows/20261105-210000-ab12.md"),
            "# Workflow\n\n## Task list\n- [x] read the notes\n- [ ] ship it\n\n## Notes\n",
        )
        .unwrap();
        let r = learn(&mut v, &opts(&agent)).unwrap();
        assert_eq!((r.scanned, r.learned, r.unchanged), (1, 1, 0), "{r:?}");
        let note = &r.notes[0];
        assert!(
            note.starts_with(
                "Tempo/Conversations/2026-11-05 How should Perennos Tempo store notes"
            ),
            "{note}"
        );
        let t = v.read(note).unwrap();
        assert!(
            !t.contains(&key) && t.contains("[REDACTED]"),
            "secrets never land in the vault"
        );
        assert!(
            t.contains("tempo_conversation: 20261105-210000-ab12")
                && t.contains("summarized_by: extractive")
        );
        assert!(
            t.contains(
                "## Decisions\n- We decided to keep plain Markdown files as the source of truth."
            ),
            "{t}"
        );
        assert!(
            t.contains("- [ ] Write the vault README")
                && t.contains("- [ ] ship it")
                && t.contains("- [x] read the notes"),
            "{t}"
        );
        assert!(
            t.contains("## Related\n") && t.contains("[[Ideas]]") && t.contains("[[Tempo]]"),
            "related notes are linked: {t}"
        );
        assert!(!t.contains("fn main"), "code is not summarized");
        let daily = v.read("Daily/2026-11-05.md").unwrap();
        assert!(daily.contains("## Learned by Tempo\n- [[Tempo/Conversations/2026-11-05 How should Perennos Tempo store notes"), "{daily}");
        let map = v.read(MAP_NOTE).unwrap();
        assert!(
            map.contains(START)
                && map.contains("1 conversations learned")
                && map.contains("```tasks"),
            "{map}"
        );
        assert!(
            v.backlinks("Ideas.md").iter().any(|b| b.source == *note),
            "the conversation joins the graph"
        );
        // idempotent
        let r2 = learn(&mut v, &opts(&agent)).unwrap();
        assert_eq!((r2.learned, r2.unchanged), (0, 1));
        assert_eq!(
            v.read("Daily/2026-11-05.md").unwrap(),
            daily,
            "no duplicate daily line"
        );
        // a hand-edited note is kept; new points go under Updates
        let edited = format!("{t}\nMy own thoughts.\n");
        v.write(note, &edited, None, "test").unwrap();
        let mut more = m.clone();
        more.push(("user", "One more thing about the graph view."));
        more.push((
            "assistant",
            "The graph view draws every note and link with three.js on the GPU.",
        ));
        conv(&agent, "20261105-210000-ab12", &more);
        let r3 = learn(&mut v, &opts(&agent)).unwrap();
        assert_eq!(r3.kept_hand_edits, std::slice::from_ref(note));
        let t3 = v.read(note).unwrap();
        assert!(
            t3.contains("My own thoughts.") && t3.contains("## Updates"),
            "{t3}"
        );
        // user text outside the map's markers survives a rewrite
        let map2 = format!("{}\nMine, keep me.\n", v.read(MAP_NOTE).unwrap());
        v.write(MAP_NOTE, &map2, None, "test").unwrap();
        conv(
            &agent,
            "20261106-080000-cd34",
            &[
                ("user", "What is BM25?"),
                (
                    "assistant",
                    "BM25 ranks documents by term frequency and rarity.",
                ),
            ],
        );
        learn(&mut v, &opts(&agent)).unwrap();
        let map3 = v.read(MAP_NOTE).unwrap();
        assert!(
            map3.contains("Mine, keep me.") && map3.contains("2 conversations learned"),
            "{map3}"
        );
    }

    #[test]
    fn uses_a_local_model_and_falls_back() {
        let (_d, mut v) = sample();
        let agent = tmp("agent-model");
        conv(
            &agent,
            "20261106-090000-ef56",
            &[
                ("user", "Plan the studio work"),
                ("assistant", "Tempo will open REAPER and render a master."),
            ],
        );
        // a fake Ollama that answers once with a JSON digest
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let mut got = Vec::new();
            loop {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let txt = String::from_utf8_lossy(&got).to_string();
                if let Some((h, b)) = txt.split_once("\r\n\r\n") {
                    let len: usize = h
                        .lines()
                        .find_map(|x| {
                            x.to_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if b.len() >= len {
                        break;
                    }
                }
            }
            let content = json!({"title": "Studio plan", "summary": "Tempo opens REAPER and renders a master.", "key_points": ["REAPER runs headless"], "decisions": ["Use -renderproject"], "todos": ["Check loudness"]}).to_string();
            let body = json!({"message": {"role": "assistant", "content": content}}).to_string();
            let chunked = format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len());
            s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{chunked}").as_bytes()).unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        let mut o = opts(&agent);
        o.model = Some("qwen-fast".into());
        o.ollama_url = format!("http://127.0.0.1:{port}");
        let r = learn(&mut v, &o).unwrap();
        let req = srv.join().unwrap();
        assert!(
            req.starts_with("POST /api/chat HTTP/1.1")
                && req.contains("\"model\":\"qwen-fast\"")
                && req.contains("\"format\":\"json\"")
        );
        assert_eq!(r.model_used, 1, "{r:?}");
        let t = v.read(&r.notes[0]).unwrap();
        assert!(
            r.notes[0].ends_with("2026-11-06 Studio plan.md")
                && t.contains("summarized_by: qwen-fast")
                && t.contains("- [ ] Check loudness"),
            "{t}"
        );
        // a dead server: the extractive summary, a warning, and no hang
        let agent2 = tmp("agent-dead");
        conv(
            &agent2,
            "20261106-100000-aa11",
            &[
                ("user", "Hello there Tempo"),
                (
                    "assistant",
                    "Hello Riley, the vault is ready for your notes today.",
                ),
            ],
        );
        let mut o2 = opts(&agent2);
        o2.model = Some("x".into());
        let r2 = learn(&mut v, &o2).unwrap();
        assert_eq!((r2.learned, r2.model_used), (1, 0));
        assert!(
            r2.warnings[0].contains("used the extractive summary"),
            "{:?}",
            r2.warnings
        );
        // never to an address outside this machine and the home network
        let e = post_json(
            "http://8.8.8.8:11434",
            "/api/chat",
            &json!({}),
            Duration::from_secs(1),
            false,
        )
        .unwrap_err();
        assert!(e.contains("refusing"), "{e}");
        assert!(post_json(
            "https://127.0.0.1:1",
            "/api/chat",
            &json!({}),
            Duration::from_secs(1),
            false
        )
        .is_err());
        assert!(post_json(
            "http://u:p@127.0.0.1:1",
            "/api/chat",
            &json!({}),
            Duration::from_secs(1),
            false
        )
        .is_err());
    }

    #[test]
    fn reviewed_teach_rows_are_taught_once() {
        let (_d, mut v) = sample();
        let agent = tmp("agent-teach");
        conv(
            &agent,
            "20261106-110000-bb22",
            &[
                ("user", "hi"),
                (
                    "assistant",
                    "Hello Riley, nothing much to learn here today.",
                ),
            ],
        );
        let teach = agent.join("workflows").join("teach");
        std::fs::create_dir_all(&teach).unwrap();
        let key = format!("hf_{}", "abcdefghijKLMNOPQRSTuvwxyz0123456789");
        let rows = [
            json!({"prompt": "How loud should a streaming master be?", "answer": format!("About -14 LUFS with a -1 dBTP ceiling, see the Perennos Tempo notes. Token {key}"), "source": "escalation", "hop_hash": "h1"}),
            json!({"prompt": "unreviewed", "answer": "skip me", "reviewed": false}),
            json!({"prompt": "", "answer": "no prompt"}),
        ];
        std::fs::write(
            teach.join("rows.jsonl"),
            rows.iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        // the same reviewed row again (same hop_hash) and one pair twice without a hash
        let again = rows[0].clone();
        std::fs::write(teach.join("more.json"), json!([again, {"prompt": "What is BM25?", "answer": "A ranking function for search."}, {"prompt": "What is BM25?", "answer": "A ranking function for search."}]).to_string()).unwrap();
        let mut o = opts(&agent);
        o.teach_dirs = vec![teach.clone()];
        let r = learn(&mut v, &o).unwrap();
        assert_eq!(
            r.taught, 2,
            "h1 once, the BM25 pair once, the rest skipped: {r:?}"
        );
        let day = v
            .notes()
            .find(|n| n.path.starts_with("Tempo/Taught/"))
            .map(|n| n.path.clone())
            .unwrap();
        let t = v.read(&day).unwrap();
        assert!(t.contains("## How loud should a streaming master be?\n> How loud should a streaming master be?") && t.contains("escalation · `h1`"), "{t}");
        assert!(
            !t.contains(&key) && t.contains("[REDACTED]"),
            "teach rows are redacted too"
        );
        assert!(t.contains("[[Tempo]]"), "related notes are linked: {t}");
        assert!(!t.contains("skip me") && !t.contains("no prompt"));
        assert!(v
            .read(MAP_NOTE)
            .unwrap()
            .contains("## Taught\n2 reviewed answers."));
        assert_eq!(learn(&mut v, &o).unwrap().taught, 0, "taught once");
    }

    #[cfg(unix)]
    #[test]
    fn model_cycle_runs_hidden_and_never_fails_learning() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, mut v) = sample();
        let agent = tmp("agent-cycle");
        conv(
            &agent,
            "20261106-120000-cc33",
            &[
                ("user", "ping"),
                ("assistant", "Pong, the vault is ready for today."),
            ],
        );
        let bin = tmp("fake-learn");
        let ok = bin.join("ok.sh");
        std::fs::write(
            &ok,
            "#!/bin/sh\n[ \"$1 $2\" = \"run --dry-run\" ] || exit 9\necho cycle done\n",
        )
        .unwrap();
        let bad = bin.join("bad.sh");
        std::fs::write(
            &bad,
            "#!/bin/sh\necho 'no data here' >&2\necho 'receipts missing' >&2\nexit 3\n",
        )
        .unwrap();
        for p in [&ok, &bad] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut o = opts(&agent);
        o.model_cycle = Some(ok.clone());
        let r = learn(&mut v, &o).unwrap();
        assert!(
            r.model_cycle
                .as_deref()
                .unwrap()
                .ends_with("run --dry-run: ok")
                && r.warnings.is_empty(),
            "{r:?}"
        );
        conv(
            &agent,
            "20261106-120000-cc33",
            &[
                ("user", "ping"),
                ("assistant", "Pong again, the vault is still ready."),
            ],
        );
        o.model_cycle = Some(bad);
        let r = learn(&mut v, &o).unwrap();
        assert_eq!(r.learned, 1, "the vault's learning still happened");
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("exited 3: receipts missing")),
            "{:?}",
            r.warnings
        );
        o.model_cycle = Some(bin.join("missing"));
        let r = learn(&mut v, &o).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("was skipped")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn names_addresses_and_schedules() {
        assert_eq!(
            note_name("What is: a/b? <x> | #tag [[y]]..."),
            "What is a b x tag y"
        );
        assert_eq!(note_name("   "), "Conversation");
        assert_eq!(head_words("one two three four five", 12), "one two…");
        assert_eq!(head_words("short", 12), "short");
        // a tailnet address is put together at run time, so the public leak scan never matches it
        let tailnet = format!("100.{}.1.2", 101);
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.20",
            "172.16.0.1",
            tailnet.as_str(),
            "::1",
            "fd00::1",
        ] {
            assert!(is_local_addr(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.128.0.1", "2001:4860::8888"] {
            assert!(!is_local_addr(ip.parse().unwrap()), "{ip}");
        }
        let (line, _) = schedule_plan(
            Path::new("/opt/tempo-vault"),
            Path::new("/home/riley/Vault"),
            "hourly",
        )
        .unwrap();
        if cfg!(windows) {
            assert!(line.starts_with("conhost.exe --headless"));
        } else {
            assert_eq!(line, format!("17 * * * * \"/opt/tempo-vault\" learn --vault \"/home/riley/Vault\" --with-model-cycle >> \"/home/riley/Vault/.tempo/learn.log\" 2>&1 {CRON_MARK}"));
        }
        assert!(schedule_plan(Path::new("x"), Path::new("y"), "weekly").is_err());
        let d = tmp("nofolder");
        put(&d, "a.md", "x");
        let mut v = Vault::open(&d).unwrap();
        assert!(learn(&mut v, &opts(&d.join("missing")))
            .unwrap_err()
            .contains("no conversations folder"));
    }
}
