//! Test helpers: a tiny HTTP server with a handler closure that records every request (the fake
//! agent, fake Discord, Telegram and Graph APIs are built on it), and a scriptable fake agent.
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    /// Path with the query string.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub at: Instant,
}

impl Seen {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(h, _)| h == k)
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

pub type Reply = (u16, Vec<(String, String)>, String);
type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

pub struct FakeHttp {
    pub addr: SocketAddr,
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
}

fn read(s: &TcpStream) -> Option<Seen> {
    let mut rd = BufReader::new(s.try_clone().ok()?);
    let mut line = String::new();
    rd.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = vec![];
    loop {
        let mut h = String::new();
        if rd.read_line(&mut h).ok()? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    rd.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
        at: Instant::now(),
    })
}

impl FakeHttp {
    pub fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> FakeHttp {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let h: Arc<Handler> = Arc::new(handler);
        let (sn, st) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            for s in l.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut s) = s else { continue };
                let (h, sn) = (h.clone(), sn.clone());
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                    let Some(req) = read(&s) else { return };
                    sn.lock().unwrap().push(req.clone());
                    let (code, hdrs, body) = h(&req);
                    let mut head = format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in hdrs {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(body.as_bytes());
                    let _ = s.flush();
                });
            }
        });
        FakeHttp {
            addr,
            url: format!("http://{addr}"),
            seen,
            stop,
        }
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn wait_for(&self, what: &str, f: impl Fn(&[Seen]) -> bool) -> Vec<Seen> {
        let t0 = Instant::now();
        loop {
            let r = self.requests();
            if f(&r) {
                return r;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "timed out waiting for {what}: {:?}",
                r.iter()
                    .map(|s| format!("{} {}", s.method, s.path))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

/// An address where nothing listens (the agent is down).
pub fn dead_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    drop(l);
    format!("http://{a}")
}

/// How the fake agent handles one `/agent/send`.
#[derive(Clone, Debug)]
pub enum Script {
    /// Busy for two polls, then this answer (after some tool output that must never be posted).
    Answer(String),
    /// Refuse with 409 this many times first, then answer.
    BusyThen(u32, String),
    /// Ask to confirm something (the id is always `c-77`); once answered, answer.
    Confirm(String),
    /// The same with an id of your own (an owner conversation's approval card).
    ConfirmId(String, String),
    /// A confirmation with an id, what it asks and its detail, then an answer.
    ConfirmWith(String, String, String, String),
    /// The turn fails.
    Error(String),
    /// The turn never ends (the agent stays busy).
    Hang,
}

#[derive(Default)]
struct Turn {
    later: Vec<(String, Value)>,
    polls_left: u32,
}

#[derive(Default)]
struct AgentState {
    convs: HashMap<String, Vec<Value>>,
    busy: HashMap<String, bool>,
    turns: HashMap<String, Turn>,
    script: VecDeque<Script>,
    busy_left: u32,
    next: u32,
    /// Per conversation: the confirmation waiting (its id) and the answer to give afterwards.
    confirm_answer: HashMap<String, (String, String)>,
    /// Behave like an older agent without guest conversations.
    no_guest: bool,
    /// Take a model and a role in `/agent/new` and say so in the health (`"roles": true`).
    roles: bool,
    /// Polls before an answer arrives (0: the usual two); more makes a turn last longer.
    answer_polls: u32,
    /// The `roles` answer of the next health calls, one each, before `roles` applies again.
    health_roles: VecDeque<bool>,
    /// How long `/agent/confirm` takes to answer (a slow agent).
    confirm_delay: Duration,
    /// `/agent/confirm` fails with a 500 (an agent that cannot be reached for it).
    confirm_fail: bool,
    /// When set, every request but the health check must carry this `X-Tempo-Key` (else 401).
    key: Option<String>,
}

pub struct FakeAgent {
    pub http: FakeHttp,
    state: Arc<Mutex<AgentState>>,
}

fn push(st: &mut AgentState, conv: &str, kind: &str, extra: Value) {
    let evs = st.convs.entry(conv.to_string()).or_default();
    let mut e = json!({"n": evs.len(), "t": "2026-11-06 12:00:00", "type": kind});
    if let (Some(o), Some(x)) = (e.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    evs.push(e);
}

pub const TOOL_SECRET: &str = "TOOL-OUTPUT-NEVER-POSTED";

impl FakeAgent {
    /// From now on answer health like an agent without guest mode.
    pub fn without_guest_mode(&self) {
        self.state.lock().unwrap().no_guest = true;
    }

    /// From now on behave like an agent that takes a teammate's model and role.
    pub fn with_roles(&self) {
        self.state.lock().unwrap().roles = true;
    }

    /// From now on an answer takes this many polls to arrive (a turn that stays in flight).
    pub fn slow_answers(&self, polls: u32) {
        self.state.lock().unwrap().answer_polls = polls;
    }

    /// The next health calls answer `roles` as given, one each (the flag changes between calls).
    pub fn roles_script(&self, answers: &[bool]) {
        self.state.lock().unwrap().health_roles = answers.iter().copied().collect();
    }

    /// From now on require this key (a restarted agent has a new one).
    pub fn require_key(&self, key: &str) {
        self.state.lock().unwrap().key = Some(key.to_string());
    }

    pub fn start(script: Vec<Script>) -> FakeAgent {
        let state = Arc::new(Mutex::new(AgentState {
            script: script.into(),
            ..Default::default()
        }));
        let st = state.clone();
        let http = FakeHttp::start(move |r| {
            let mut s = st.lock().unwrap();
            let body = r.json();
            let ok = |v: Value| (200, vec![], v.to_string());
            let path = r.path.split('?').next().unwrap_or("");
            if let Some(k) = &s.key {
                if path != "/agent/health" && r.header("x-tempo-key") != Some(k.as_str()) {
                    return (401, vec![], json!({"error": "key required"}).to_string());
                }
            }
            match (r.method.as_str(), path) {
                ("GET", "/agent/health") if s.no_guest => {
                    ok(json!({"net": true, "models": ["fake"], "stream": true}))
                }
                ("GET", "/agent/health") => {
                    let mut h =
                        json!({"net": true, "models": ["fake"], "stream": true, "guest": true});
                    let roles = s.health_roles.pop_front().unwrap_or(s.roles);
                    if roles {
                        h["roles"] = json!(true);
                    }
                    ok(h)
                }
                ("POST", "/agent/new") => {
                    // the real agent's rules: a model is letters, digits and : . - _ / (100 at
                    // most), a role 2,000 characters at most; otherwise 400
                    if s.roles {
                        let model_ok = body["model"].as_str().is_none_or(|m| {
                            !m.is_empty()
                                && m.len() <= 100
                                && m.chars()
                                    .all(|c| c.is_ascii_alphanumeric() || ":.-_/".contains(c))
                        });
                        let role_ok = body["role"]
                            .as_str()
                            .is_none_or(|r| r.chars().count() <= 2000);
                        if !model_ok || !role_ok {
                            return (
                                400,
                                vec![],
                                json!({"error": "bad model or role"}).to_string(),
                            );
                        }
                    }
                    s.next += 1;
                    let id = format!("conv-{}", s.next);
                    s.convs.insert(id.clone(), vec![]);
                    ok(json!({"conv": id}))
                }
                ("POST", "/agent/send") => {
                    let conv = body["conv"].as_str().unwrap_or("").to_string();
                    let text = body["text"].as_str().unwrap_or("").to_string();
                    if !s.convs.contains_key(&conv) {
                        return (
                            404,
                            vec![],
                            json!({"error": "unknown conversation"}).to_string(),
                        );
                    }
                    if s.busy_left > 0 || s.busy.get(&conv) == Some(&true) {
                        s.busy_left = s.busy_left.saturating_sub(1);
                        return (409, vec![], json!({"error": "busy"}).to_string());
                    }
                    let step = s
                        .script
                        .pop_front()
                        .unwrap_or(Script::Answer("(no script)".into()));
                    let step = match step {
                        Script::BusyThen(n, a) if n > 0 => {
                            s.busy_left = n - 1;
                            s.script.push_front(Script::Answer(a));
                            return (409, vec![], json!({"error": "busy"}).to_string());
                        }
                        Script::BusyThen(_, a) => Script::Answer(a),
                        x => x,
                    };
                    push(&mut s, &conv, "user", json!({"text": text}));
                    s.busy.insert(conv.clone(), true);
                    match step {
                        Script::Answer(a) => {
                            let later = vec![
                                ("status".to_string(), json!({"text": "memory: notes.md"})),
                                (
                                    "tool_call".to_string(),
                                    json!({"name": "read_file", "args": "{}"}),
                                ),
                                (
                                    "tool_result".to_string(),
                                    json!({"name": "read_file", "text": TOOL_SECRET}),
                                ),
                                ("delta".to_string(), json!({"text": "partial"})),
                                ("assistant".to_string(), json!({"text": a})),
                            ];
                            let polls_left = if s.answer_polls > 0 {
                                s.answer_polls
                            } else {
                                2
                            };
                            s.turns.insert(conv.clone(), Turn { later, polls_left });
                        }
                        Script::Confirm(a) => {
                            push(
                                &mut s,
                                &conv,
                                "confirm",
                                json!({"id": "c-77", "what": "run a command", "detail": "rm -rf /important"}),
                            );
                            s.confirm_answer.insert(conv.clone(), ("c-77".into(), a));
                        }
                        Script::ConfirmWith(id, what, detail, a) => {
                            push(
                                &mut s,
                                &conv,
                                "confirm",
                                json!({"id": id, "what": what, "detail": detail}),
                            );
                            s.confirm_answer.insert(conv.clone(), (id, a));
                        }
                        Script::ConfirmId(id, a) => {
                            push(
                                &mut s,
                                &conv,
                                "confirm",
                                json!({"id": id, "what": "run a command", "detail": "echo hi > notes.txt"}),
                            );
                            s.confirm_answer.insert(conv.clone(), (id, a));
                        }
                        Script::Error(e) => {
                            push(&mut s, &conv, "error", json!({"text": e}));
                            s.busy.insert(conv.clone(), false);
                        }
                        Script::Hang => {}
                        Script::BusyThen(..) => unreachable!(),
                    }
                    ok(json!({"ok": true}))
                }
                ("POST", "/agent/confirm") => {
                    if s.confirm_fail {
                        return (500, vec![], json!({"error": "broken"}).to_string());
                    }
                    std::thread::sleep(s.confirm_delay);
                    let conv = body["conv"].as_str().unwrap_or("").to_string();
                    // like the real agent: only the id it issued for that conversation counts
                    let waiting = s
                        .confirm_answer
                        .get(&conv)
                        .is_some_and(|(id, _)| body["id"].as_str() == Some(id.as_str()));
                    if let Some((_, a)) = waiting.then(|| s.confirm_answer.remove(&conv)).flatten()
                    {
                        let verdict = if body["ok"] == true {
                            "approved"
                        } else {
                            "denied or timed out"
                        };
                        push(
                            &mut s,
                            &conv,
                            "status",
                            json!({"text": format!("run a command: {verdict}")}),
                        );
                        push(&mut s, &conv, "assistant", json!({"text": a}));
                        s.busy.insert(conv, false);
                        ok(json!({"ok": true}))
                    } else {
                        ok(json!({"ok": false}))
                    }
                }
                ("GET", "/agent/events") => {
                    let q: HashMap<&str, &str> = r
                        .path
                        .split_once('?')
                        .map(|x| x.1)
                        .unwrap_or("")
                        .split('&')
                        .filter_map(|kv| kv.split_once('='))
                        .collect();
                    let conv = q.get("conv").copied().unwrap_or("").to_string();
                    let since: usize = q.get("since").and_then(|x| x.parse().ok()).unwrap_or(0);
                    let mut done = None;
                    if let Some(t) = s.turns.get_mut(&conv) {
                        if t.polls_left == 0 {
                            done = Some(std::mem::take(&mut t.later));
                        } else {
                            t.polls_left -= 1;
                        }
                    }
                    if let Some(later) = done {
                        s.turns.remove(&conv);
                        for (k, v) in later {
                            push(&mut s, &conv, &k, v);
                        }
                        s.busy.insert(conv.clone(), false);
                    }
                    let evs = s.convs.get(&conv).cloned().unwrap_or_default();
                    ok(
                        json!({"events": evs.get(since..).unwrap_or(&[]), "busy": s.busy.get(&conv) == Some(&true)}),
                    )
                }
                _ => (404, vec![], json!({"error": "not found"}).to_string()),
            }
        });
        FakeAgent { http, state }
    }

    pub fn url(&self) -> String {
        self.http.url.clone()
    }

    pub fn script(&self, s: Script) {
        self.state.lock().unwrap().script.push_back(s);
    }

    /// The agent's own timeout for an unanswered approval: every waiting one ends as "denied or
    /// timed out" and its turn goes on to answer.
    pub fn time_out_confirms(&self) {
        let mut s = self.state.lock().unwrap();
        let waiting: Vec<(String, (String, String))> = s.confirm_answer.drain().collect();
        for (conv, (_, a)) in waiting {
            push(
                &mut s,
                &conv,
                "status",
                json!({"text": "run a command: denied or timed out"}),
            );
            push(&mut s, &conv, "assistant", json!({"text": a}));
            s.busy.insert(conv, false);
        }
    }

    /// From now on `/agent/confirm` takes this long to answer.
    pub fn slow_confirms(&self, d: Duration) {
        self.state.lock().unwrap().confirm_delay = d;
    }

    /// From now on `/agent/confirm` fails.
    pub fn failing_confirms(&self) {
        self.state.lock().unwrap().confirm_fail = true;
    }

    /// The bodies of every `/agent/confirm`, in order.
    pub fn confirm_calls(&self) -> Vec<Value> {
        self.http
            .requests()
            .iter()
            .filter(|r| r.path == "/agent/confirm")
            .map(|r| r.json())
            .collect()
    }

    /// The bodies of every `/agent/new`, in order.
    pub fn new_bodies(&self) -> Vec<Value> {
        self.http
            .requests()
            .iter()
            .filter(|r| r.path == "/agent/new")
            .map(|r| r.json())
            .collect()
    }

    /// The conversation of every `/agent/send`, in order.
    pub fn send_convs(&self) -> Vec<String> {
        self.http
            .requests()
            .iter()
            .filter(|r| r.path == "/agent/send")
            .map(|r| r.json()["conv"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// The mode a conversation was made with (`guest` or `owner`), from the order of `/agent/new`.
    pub fn mode_of(&self, conv: &str) -> String {
        let n: usize = conv.trim_start_matches("conv-").parse().unwrap_or(0);
        self.new_bodies()
            .get(n.wrapping_sub(1))
            .and_then(|b| b["mode"].as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// The texts sent to the agent, in order.
    pub fn prompts(&self) -> Vec<String> {
        self.http
            .requests()
            .iter()
            .filter(|r| r.path == "/agent/send")
            .map(|r| r.json()["text"].as_str().unwrap_or("").to_string())
            .collect()
    }
}

/// A mock Ollama: `GET /api/tags` lists `models`; `POST /api/chat` answers with
/// `{"message": {"role": "assistant", "content": answer(body)}}` (stream false, one JSON reply).
/// A model that is not in `models` gets Ollama's 404.
pub fn fake_ollama(
    models: &[&str],
    answer: impl Fn(&Value) -> String + Send + Sync + 'static,
) -> FakeHttp {
    let models: Vec<String> = models.iter().map(|m| m.to_string()).collect();
    FakeHttp::start(move |r| {
        match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/api/tags") => (
            200,
            vec![],
            json!({"models": models.iter().map(|m| json!({"name": m, "model": m, "size": 2_500_000_000u64})).collect::<Vec<_>>()}).to_string(),
        ),
        ("POST", "/api/chat") => {
            let b = r.json();
            let known = b["model"].as_str().is_some_and(|m| models.iter().any(|x| x == m));
            if !known {
                return (404, vec![], json!({"error": "model not found"}).to_string());
            }
            (
                200,
                vec![],
                json!({"model": b["model"], "message": {"role": "assistant", "content": answer(&b)}, "done": true}).to_string(),
            )
        }
        _ => (404, vec![], json!({"error": "not found"}).to_string()),
    }
    })
}

/// Every file under `dir`, as text (for leak checks).
pub fn all_text(dir: &std::path::Path) -> String {
    let mut out = String::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.push_str(&all_text(&p));
            } else if let Ok(t) = std::fs::read_to_string(&p) {
                out.push_str(&t);
                out.push('\n');
            }
        }
    }
    out
}
