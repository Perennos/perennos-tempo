//! `tempo-local-agent serve`: the HTTP API the Tempo GUI chat surface uses (`/agent/*`), a
//! server-sent-events stream, the chat surface's static files (`--chat-dir`) and an optional
//! pass-through to the Tempo GUI server (`--gui-port`). Loopback only by default; std only.
//!
//! One worker thread owns the agent (its state is single-threaded) and runs turns in order.
//! HTTP threads read shared state, queue turns, and resolve only confirmations this server issued.
//! Every request must name an allowed Host, a present Origin must be ours, POST bodies are capped
//! JSON, and anything but GET and POST is refused.
use crate::agent::Agent;
use crate::config::Config;
use crate::conv::{valid_id, Confirm, Conv, Sink};
use crate::mcp::{McpHost, ServerCfg};
use crate::rag::{load_index, RagFile};
use crate::tools::{net_up, Env};
use crate::util::{self, cap, http_json, redact};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Largest POST body accepted.
pub const MAX_BODY: usize = 1 << 20;
const MAX_HEAD: usize = 16 * 1024;
const MAX_CONNS: usize = 64;
const MAX_STREAMS: usize = 16;

#[derive(Clone, Debug)]
pub struct ServeOpts {
    pub bind: IpAddr,
    pub port: u16,
    /// Extra Host values to accept (`name` or `name:port`), for a reverse proxy in front.
    pub allow_hosts: Vec<String>,
    /// Folder served at `/chat/` (the chat surface files).
    pub chat_dir: Option<PathBuf>,
    /// Pass every other path through to the Tempo GUI server on this loopback port.
    pub gui_port: Option<u16>,
    /// How long a confirmation waits for the GUI before it counts as denied.
    pub confirm_timeout: Duration,
    /// The agent's key: every request but `GET /agent/health`, the page and its files must send
    /// it as `X-Tempo-Key`. `serve` makes a fresh one at each start (tests may leave it unset).
    pub key: Option<String>,
    /// Listening on an address other than loopback needs this (and the key still applies).
    pub allow_remote: bool,
    /// Talk to Tempo: what is installed. None looks it up on every request (`voice.rs`).
    pub voice: Option<crate::voice::Voice>,
}

impl Default for ServeOpts {
    fn default() -> Self {
        ServeOpts {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8765,
            allow_hosts: vec![],
            chat_dir: None,
            gui_port: None,
            confirm_timeout: Duration::from_secs(900),
            key: None,
            allow_remote: false,
            voice: None,
        }
    }
}

#[derive(Default)]
struct ConvLog {
    /// A guest conversation, remembered here as well as in its file: if the file is lost the
    /// conversation stays a guest one (it never falls back to the owner's tools and memory).
    guest: bool,
    events: Vec<Value>,
    busy: bool,
    /// File names the last turn's memory search used.
    used: Vec<String>,
    /// The engine that answered the last turn (None until one has, or when it failed).
    engine: Option<String>,
}

type Pending = HashMap<String, (String, mpsc::Sender<bool>)>;

#[derive(Default)]
struct State {
    convs: HashMap<String, ConvLog>,
    /// Confirmation id -> (conversation, answer channel). Only ids issued here resolve.
    pending: Pending,
    embed_ok: Option<bool>,
}

type GraphCache = Option<(Vec<(String, u64)>, Value)>;

struct Shared {
    cfg: Config,
    opts: ServeOpts,
    allowed: HashSet<String>,
    state: Mutex<State>,
    changed: Condvar,
    jobs: Mutex<mpsc::Sender<Job>>,
    conns: AtomicUsize,
    streams: AtomicUsize,
    graph: Mutex<GraphCache>,
    /// The engines' last probe for health, reused for 15 s (probing takes seconds per host).
    hosts_seen: Mutex<Option<(Instant, Value)>>,
    // slash menu: the connected servers' tools (filled by the worker) and the catalog, kept 5 s
    mcp_tools: Mutex<Vec<Value>>,
    catalog: Mutex<Option<(Instant, Value)>>,
}

struct Job {
    conv: String,
    text: String,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A conversation event as the chat surface expects it.
fn gui_event(ev: &Value) -> Value {
    let kind = ev["type"].as_str().unwrap_or("status");
    let text = ev["text"].as_str().unwrap_or("");
    let t = ev["t"].clone();
    match kind {
        "tool_call" => {
            let (name, args) = text.split_once(' ').unwrap_or((text, ""));
            json!({"type": kind, "t": t, "name": name, "args": args})
        }
        "tool_result" => {
            let (name, rest) = text.split_once(": ").unwrap_or(("", text));
            json!({"type": kind, "t": t, "name": name, "text": rest})
        }
        _ => json!({"type": kind, "t": t, "text": text}),
    }
}

impl Shared {
    /// Load a saved conversation's events the first time it is asked for. False if it does not exist.
    fn ensure(&self, st: &mut State, id: &str) -> bool {
        if st.convs.contains_key(id) {
            return true;
        }
        if !valid_id(id) {
            return false;
        }
        let path = self
            .cfg
            .data_dir
            .join("conversations")
            .join(format!("{id}.json"));
        let Ok(txt) = std::fs::read_to_string(path) else {
            return false;
        };
        let saved: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
        let mut log = ConvLog {
            guest: saved["mode"] == "guest",
            ..ConvLog::default()
        };
        for (n, ev) in saved["events"].as_array().into_iter().flatten().enumerate() {
            let mut e = gui_event(ev);
            e["n"] = json!(n);
            log.events.push(e);
        }
        st.convs.insert(id.to_string(), log);
        true
    }

    fn push(&self, conv: &str, mut ev: Value) {
        let mut st = lock(&self.state);
        let log = st.convs.entry(conv.to_string()).or_default();
        if ev["type"] == "status" {
            if let Some(m) = ev["text"].as_str().and_then(|t| t.strip_prefix("memory: ")) {
                log.used = m
                    .split(", ")
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
        }
        ev["n"] = json!(log.events.len());
        log.events.push(ev);
        drop(st);
        self.changed.notify_all();
    }
}

/// Streams the turn's text as `delta` events (batched about every 120 ms) and its conversation events in order.
struct Live {
    shared: Arc<Shared>,
    conv: String,
    buf: RefCell<String>,
    last: Cell<Instant>,
}

impl Live {
    fn text(&self, s: &str) {
        self.buf.borrow_mut().push_str(s);
        if self.last.get().elapsed() >= Duration::from_millis(120) {
            self.flush();
        }
    }
    fn flush(&self) {
        let t = std::mem::take(&mut *self.buf.borrow_mut());
        if !t.is_empty() {
            self.shared.push(
                &self.conv,
                json!({"type": "delta", "t": util::now_str(), "text": redact(&t)}),
            );
        }
        self.last.set(Instant::now());
    }
    fn event(&self, ev: &Value) {
        self.flush();
        self.shared.push(&self.conv, gui_event(ev));
    }
}

struct LiveSink(Rc<Live>);

impl Sink for LiveSink {
    fn text(&mut self, s: &str) {
        self.0.text(s);
    }
    fn status(&mut self, _: &str) {} // every status line is also a conversation event
}

static CONFIRM_SEQ: AtomicU64 = AtomicU64::new(1);

/// Asks the GUI: posts a `confirm` event and waits for /agent/confirm with that id. Timeout denies.
struct ServerConfirm {
    shared: Arc<Shared>,
    current: Arc<Mutex<String>>,
}

impl Confirm for ServerConfirm {
    fn confirm(&self, what: &str, detail: &str) -> bool {
        let conv = lock(&self.current).clone();
        if conv.is_empty() {
            return false;
        }
        let id = format!(
            "{}{:x}",
            util::secret_id(),
            CONFIRM_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = mpsc::channel();
        lock(&self.shared.state)
            .pending
            .insert(id.clone(), (conv.clone(), tx));
        self.shared.push(
            &conv,
            json!({"type": "confirm", "t": util::now_str(), "id": id, "what": what, "detail": detail}),
        );
        let ok = rx
            .recv_timeout(self.shared.opts.confirm_timeout)
            .unwrap_or(false);
        lock(&self.shared.state).pending.remove(&id);
        let verdict = if ok {
            "approved"
        } else {
            "denied or timed out"
        };
        self.shared.push(
            &conv,
            json!({"type": "status", "t": util::now_str(), "text": format!("{what}: {verdict}")}),
        );
        ok
    }
}

fn worker(shared: Arc<Shared>, servers: BTreeMap<String, ServerCfg>, rx: mpsc::Receiver<Job>) {
    let cfg = shared.cfg.clone();
    let current = Arc::new(Mutex::new(String::new()));
    let confirm = ServerConfirm {
        shared: shared.clone(),
        current: current.clone(),
    };
    let mut agent = Agent::new(Env::new(
        cfg.clone(),
        Box::new(confirm),
        McpHost::new(&cfg, servers),
    ));
    {
        let mut rag = agent.env.rag.borrow_mut();
        rag.build_index(&cfg, false);
        lock(&shared.state).embed_ok = rag.embed_ok;
    }
    {
        // slash menu: what the connected servers offer, and which of it asks first
        let mut mcp = agent.env.mcp.borrow_mut();
        let tools: Vec<Value> = mcp
            .load_catalog()
            .into_iter()
            .map(|(server, t)| {
                let tool = t["name"].as_str().unwrap_or("");
                json!({"server": server, "tool": tool, "name": crate::mcp::exposed(&server, tool),
                    "description": t["description"].as_str().map(|d| crate::util::cap(d, 160)).unwrap_or_default(),
                    "approval": mcp.needs_confirm(&server, tool)})
            })
            .collect();
        *lock(&shared.mcp_tools) = tools;
    }
    for job in rx {
        lock(&current).clone_from(&job.conv);
        let live = Rc::new(Live {
            shared: shared.clone(),
            conv: job.conv.clone(),
            buf: RefCell::new(String::new()),
            last: Cell::new(Instant::now()),
        });
        let mut served = None;
        match Conv::open(&cfg, Some(&job.conv)) {
            Ok(mut conv) => {
                if lock(&shared.state)
                    .convs
                    .get(&job.conv)
                    .is_some_and(|l| l.guest)
                {
                    conv.guest = true;
                }
                let tap = live.clone();
                conv.on_event = Some(Box::new(move |ev| tap.event(ev)));
                let mut sink = LiveSink(live.clone());
                let _ = agent.turn(&mut conv, &job.text, &mut sink); // an error is already an event
                live.flush();
                conv.on_event = None;
                served = conv.served_by.take();
            }
            Err(e) => shared.push(
                &job.conv,
                json!({"type": "error", "t": util::now_str(), "text": e}),
            ),
        }
        lock(&current).clear();
        let embed = agent.env.rag.borrow().embed_ok;
        let mut st = lock(&shared.state);
        st.embed_ok = embed;
        if let Some(l) = st.convs.get_mut(&job.conv) {
            l.busy = false;
            l.engine = served;
        }
        drop(st);
        shared.changed.notify_all();
    }
    agent.env.mcp.borrow_mut().stop_all();
}

/// A running server (tests use the address; `serve` blocks on it).
pub struct Running {
    pub addr: SocketAddr,
    accept: std::thread::JoinHandle<()>,
}

impl Running {
    pub fn wait(self) {
        let _ = self.accept.join();
    }
}

/// Bind, start the worker and the accept loop, and return at once.
pub fn start(
    cfg: Config,
    servers: BTreeMap<String, ServerCfg>,
    opts: ServeOpts,
) -> Result<Running, String> {
    let listener = TcpListener::bind((opts.bind, opts.port))
        .map_err(|e| format!("cannot listen on {}:{}: {e}", opts.bind, opts.port))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let port = addr.port();
    let mut allowed: HashSet<String> = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .into_iter()
    .collect();
    // bound to one particular address (a tailnet IP): that address is ours too
    if !opts.bind.is_loopback() && !opts.bind.is_unspecified() {
        allowed.insert(match opts.bind {
            IpAddr::V6(v6) => format!("[{v6}]:{port}"),
            v4 => format!("{v4}:{port}"),
        });
    }
    for h in &opts.allow_hosts {
        let h = h.to_lowercase();
        // behind `tailscale serve` on 443 (or 80) the browser sends a Host with no port
        if let Some(bare) = h.strip_suffix(":443").or_else(|| h.strip_suffix(":80")) {
            allowed.insert(bare.to_string());
        }
        allowed.insert(if h.contains(':') && !h.ends_with(']') {
            h
        } else {
            format!("{h}:{port}")
        });
    }
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared {
        cfg,
        opts,
        allowed,
        state: Mutex::new(State::default()),
        changed: Condvar::new(),
        jobs: Mutex::new(tx),
        conns: AtomicUsize::new(0),
        streams: AtomicUsize::new(0),
        graph: Mutex::new(None),
        hosts_seen: Mutex::new(None),
        mcp_tools: Mutex::new(vec![]),
        catalog: Mutex::new(None),
    });
    let w = shared.clone();
    std::thread::spawn(move || worker(w, servers, rx));
    let accept = std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let sh = shared.clone();
            if sh.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                sh.conns.fetch_sub(1, Ordering::SeqCst);
                let mut s = s;
                let _ = respond(&mut s, 503, "application/json", br#"{"error":"busy"}"#);
                continue;
            }
            std::thread::spawn(move || {
                handle(&sh, s);
                sh.conns.fetch_sub(1, Ordering::SeqCst);
            });
        }
        // the worker thread is never joined, so its `Env` is not dropped: end its commands here
        crate::tools::end_all_background();
    });
    Ok(Running { addr, accept })
}

/// Serve until the process ends. A fresh key is written to `<data>/control.key` first; every
/// client (the page, Tempo Room, the Vault's Tempo tab, the launcher) sends it.
pub fn serve(
    cfg: Config,
    servers: BTreeMap<String, ServerCfg>,
    mut opts: ServeOpts,
) -> Result<(), String> {
    check_bind(&opts)?;
    let key = util::new_key();
    let path = util::write_key(&cfg.data_dir, &key)?;
    opts.key = Some(key);
    let r = start(cfg, servers, opts)?;
    eprintln!(
        "[serving the agent API on http://{}/agent/; its key is in {} (open Perennos Tempo, or run tempo-launch open agent)]",
        r.addr,
        path.display()
    );
    r.wait();
    Ok(())
}

/// Only loopback, unless the owner asked for more (and then the key still applies).
pub fn check_bind(opts: &ServeOpts) -> Result<(), String> {
    if !opts.bind.is_loopback() && !opts.allow_remote {
        return Err(format!(
            "the agent listens only on this computer; to listen on {} pass --allow-remote (every request still needs the key)",
            opts.bind
        ));
    }
    Ok(())
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn q(&self, k: &str) -> &str {
        self.query.get(k).map(String::as_str).unwrap_or("")
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(s: &TcpStream) -> Result<Req, (u16, &'static str)> {
    let bad = (400, "bad request");
    let clone = s.try_clone().map_err(|_| bad)?;
    let mut rd = BufReader::new(clone.take((MAX_HEAD + crate::voice::MAX_AUDIO + 1) as u64));
    let mut line = String::new();
    rd.read_line(&mut line).map_err(|_| bad)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if !target.starts_with('/') || !parts.next().unwrap_or("").starts_with("HTTP/1.") {
        return Err(bad);
    }
    let mut headers = Vec::new();
    let mut head = line.len();
    loop {
        let mut h = String::new();
        let n = rd.read_line(&mut h).map_err(|_| bad)?;
        head += n;
        if head > MAX_HEAD {
            return Err((431, "headers too large"));
        }
        if n == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect();
    let mut req = Req {
        method: method.to_string(),
        path: path.to_string(),
        query,
        headers,
        body: vec![],
    };
    if req.header("transfer-encoding").is_some() {
        return Err((411, "send a Content-Length body"));
    }
    let len: usize = match req.header("content-length") {
        Some(v) => v.parse().map_err(|_| bad)?,
        None => 0,
    };
    // a recording may be bigger than a chat message
    let cap = if req.path == "/agent/voice/transcribe" {
        crate::voice::MAX_AUDIO
    } else {
        MAX_BODY
    };
    if len > cap {
        return Err((413, "body too large"));
    }
    req.body = vec![0; len];
    rd.read_exact(&mut req.body).map_err(|_| bad)?;
    Ok(req)
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn respond(s: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let allow = if code == 405 {
        "Allow: GET, POST\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n{allow}Connection: close\r\n\r\n",
        reason(code),
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}

fn send_json(s: &mut TcpStream, code: u16, v: &Value) {
    let _ = respond(s, code, "application/json", v.to_string().as_bytes());
}

fn refuse(s: &mut TcpStream, code: u16, why: &str) {
    send_json(s, code, &json!({ "error": why }));
}

/// Host must be one of ours (no DNS rebinding); a present Origin must be ours (no cross-site
/// requests from a web page); a cross-site fetch is refused.
fn check(sh: &Shared, req: &Req) -> Result<(), (u16, &'static str)> {
    let host = req
        .header("host")
        .ok_or((400, "missing Host"))?
        .to_ascii_lowercase();
    if !sh.allowed.contains(&host) {
        return Err((403, "host not allowed"));
    }
    if let Some(o) = req.header("origin") {
        let o = o.to_ascii_lowercase();
        let ours = o
            .strip_prefix("http://")
            .or_else(|| o.strip_prefix("https://"))
            .is_some_and(|h| sh.allowed.contains(h));
        if !ours {
            return Err((403, "cross-origin request refused"));
        }
    }
    // a page on another port of this computer is "same-site": not ours either
    if matches!(
        req.header("sec-fetch-site"),
        Some("cross-site" | "same-site")
    ) {
        return Err((403, "cross-site request refused"));
    }
    if req.method != "GET" && req.method != "POST" {
        return Err((405, "only GET and POST"));
    }
    if req.method == "POST"
        && req.path != "/agent/voice/transcribe"
        && !req
            .header("content-type")
            .is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json"))
    {
        return Err((415, "POST bodies must be application/json"));
    }
    Ok(())
}

fn handle(sh: &Arc<Shared>, mut s: TcpStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let req = match read_request(&s) {
        Ok(r) => r,
        Err((code, why)) => return refuse(&mut s, code, why),
    };
    if let Err((code, why)) = check(sh, &req) {
        return refuse(&mut s, code, why);
    }
    // a request with the key (or a server with none, as in tests)
    let keyed = sh.opts.key.as_ref().is_none_or(|key| {
        let given = req.header("x-tempo-key").unwrap_or("");
        util::same_secret(given.trim().as_bytes(), key.as_bytes())
    });
    if let Some(api) = req.path.strip_prefix("/agent/") {
        // everything but health needs the key (the page and its files do not)
        if sh.opts.key.is_some() {
            let open = req.method == "GET" && api == "health";
            if !open && !keyed {
                return send_json(
                    &mut s,
                    401,
                    &json!({"error": "this needs the agent's key", "hint": "open Perennos Tempo, or run tempo-launch open agent"}),
                );
            }
        }
        if api == "voice" || api.starts_with("voice/") {
            return voice_api(sh, &req, api, s);
        }
        let body: Value = if req.method == "POST" {
            match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(_) => return refuse(&mut s, 400, "body is not JSON"),
            }
        } else {
            Value::Null
        };
        if api == "health" && req.method == "GET" {
            return send_json(&mut s, 200, &health(sh, keyed));
        }
        return route(sh, &req, api, &body, s);
    }
    if let (Some(rel), Some(dir)) = (req.path.strip_prefix("/chat/"), &sh.opts.chat_dir) {
        return static_file(&mut s, dir, rel);
    }
    match sh.opts.gui_port {
        // listening beyond this computer, the GUI behind the agent needs the key too
        Some(_) if sh.opts.allow_remote && !keyed => send_json(
            &mut s,
            401,
            &json!({"error": "this needs the agent's key", "hint": "open Perennos Tempo, or run tempo-launch open agent"}),
        ),
        Some(port) => pass_through(&req, s, port),
        None => refuse(&mut s, 404, "not found"),
    }
}

/// Talk to Tempo (`voice.rs`): the audio is a raw body, not JSON, and it is only ever heard by
/// this computer, so a connection from anywhere but loopback is refused even with the key.
fn voice_api(sh: &Shared, req: &Req, api: &str, mut s: TcpStream) {
    if !s.peer_addr().is_ok_and(|a| a.ip().is_loopback()) {
        return refuse(&mut s, 403, "voice only answers on this computer");
    }
    let v = sh.opts.voice.clone().unwrap_or_else(|| {
        crate::voice::Voice::discover(
            &crate::products::tempo_home(&|k| std::env::var(k).ok()),
            std::env::var_os("PATH"),
            std::env::var_os("TEMPO_WHISPER_MODEL").map(PathBuf::from),
            sh.cfg.data_dir.join("voice-tmp"),
            sh.cfg.trash_dir.clone(),
        )
    });
    let fail = |s: &mut TcpStream, (code, why): (u16, String)| refuse(s, code, &why);
    match (req.method.as_str(), api) {
        ("GET", "voice") => send_json(&mut s, 200, &v.status()),
        ("POST", "voice/transcribe") => {
            let ct = req.header("content-type").unwrap_or("");
            match crate::voice::transcribe(&v, ct, &req.body, req.q("lang")) {
                Ok(out) => send_json(&mut s, 200, &out),
                Err(e) => fail(&mut s, e),
            }
        }
        ("POST", "voice/speak") => {
            let text = serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|b| b.get("text").and_then(Value::as_str).map(str::to_string));
            match text.map(|t| crate::voice::speak(&v, &t)) {
                Some(Ok(wav)) => {
                    let _ = respond(&mut s, 200, "audio/wav", &wav);
                }
                Some(Err(e)) => fail(&mut s, e),
                None => refuse(&mut s, 400, "send {\"text\": \"...\"}"),
            }
        }
        _ => refuse(&mut s, 404, "not found"),
    }
}

fn route(sh: &Arc<Shared>, req: &Req, api: &str, body: &Value, mut s: TcpStream) {
    let post = req.method == "POST";
    match (post, api) {
        (false, "catalog") => {
            // everything the slash menu lists; built at most every 5 s
            let v = {
                let mut seen = lock(&sh.catalog);
                match seen.as_ref() {
                    Some((t, v)) if t.elapsed() < Duration::from_secs(5) => v.clone(),
                    _ => {
                        let mcp = lock(&sh.mcp_tools).clone();
                        let home = crate::tempo_home::tempo_home().dir;
                        let v = crate::catalog::build(&sh.cfg, &home, &mcp);
                        *seen = Some((Instant::now(), v.clone()));
                        v
                    }
                }
            };
            send_json(&mut s, 200, &v);
        }
        (false, "convs") => send_json(&mut s, 200, &json!(convs(&sh.cfg))),
        (true, "new") => {
            // {"model": "<tag>", "role": "<text>", "engine": "<url>"}, all optional: a Room
            // teammate's model (tried first; the config's models stay as fallbacks), the role its
            // owner wrote for it and the engine to ask first (one of the configured ones)
            let model = match body.get("model").and_then(Value::as_str).map(str::trim) {
                Some(m) if !m.is_empty() && !crate::conv::valid_model(m) => {
                    return refuse(
                        &mut s,
                        400,
                        "model: letters, digits and : . - _ / only, at most 100 characters",
                    )
                }
                m => m.filter(|m| !m.is_empty()).map(str::to_string),
            };
            let role = match body.get("role").and_then(Value::as_str).map(str::trim) {
                Some(r) if r.chars().count() > crate::conv::MAX_ROLE => {
                    return refuse(&mut s, 400, "role: at most 2000 characters")
                }
                r => r.filter(|r| !r.is_empty()).map(util::redact),
            };
            let engine = match body.get("engine") {
                None | Some(Value::Null) => None,
                Some(Value::String(e)) if e.trim().is_empty() => None,
                Some(Value::String(e)) => match sh.cfg.find_engine(e) {
                    Some(found) => Some(found),
                    None => {
                        return refuse(
                            &mut s,
                            400,
                            "engine: not one of this agent's engines (ollama_url, ollama_fallbacks or the urls of a host)",
                        )
                    }
                },
                Some(_) => return refuse(&mut s, 400, "engine: a URL as text"),
            };
            // "format": a JSON Schema for the answers (the Conductor's band spec uses one)
            let format = match body.get("format") {
                None | Some(Value::Null) => None,
                Some(f) if f.is_object() && f.to_string().len() <= 8192 => Some(f.clone()),
                Some(_) => {
                    return refuse(&mut s, 400, "format: a JSON Schema object, at most 8 KB")
                }
            };
            new_conv(sh, body["mode"] == "guest", model, role, engine, format, s)
        }

        (true, "send") => {
            let conv = body["conv"].as_str().unwrap_or("");
            let text = body["text"].as_str().unwrap_or("").trim();
            if !valid_id(conv) || text.is_empty() {
                return refuse(&mut s, 400, "need conv and text");
            }
            let verdict = {
                let mut st = lock(&sh.state);
                if !sh.ensure(&mut st, conv) {
                    Err((404, "unknown conversation"))
                } else {
                    match st.convs.get_mut(conv) {
                        Some(log) if !log.busy => {
                            log.busy = true;
                            Ok(())
                        }
                        _ => Err((409, "busy")),
                    }
                }
            };
            if let Err((code, why)) = verdict {
                return refuse(&mut s, code, why);
            }
            sh.changed.notify_all();
            let job = Job {
                conv: conv.to_string(),
                text: text.to_string(),
            };
            if lock(&sh.jobs).send(job).is_err() {
                return refuse(&mut s, 503, "agent worker stopped");
            }
            send_json(&mut s, 200, &json!({"ok": true}));
        }
        (true, "confirm") => {
            let (conv, id) = (
                body["conv"].as_str().unwrap_or(""),
                body["id"].as_str().unwrap_or(""),
            );
            let ok = body["ok"] == true; // anything but an explicit true denies
            let tx = {
                let mut st = lock(&sh.state);
                match st.pending.get(id) {
                    Some((c, _)) if c == conv => st.pending.remove(id).map(|(_, tx)| tx),
                    _ => None,
                }
            };
            let done = tx.is_some_and(|tx| tx.send(ok).is_ok());
            send_json(&mut s, 200, &json!({ "ok": done }));
        }
        (false, "events") => {
            let since: usize = req.q("since").parse().unwrap_or(0);
            let mut st = lock(&sh.state);
            let v = if sh.ensure(&mut st, req.q("conv")) {
                let log = &st.convs[req.q("conv")];
                json!({"events": log.events.get(since..).unwrap_or(&[]), "busy": log.busy, "engine": log.engine})
            } else {
                json!({"events": [], "busy": false})
            };
            drop(st);
            send_json(&mut s, 200, &v);
        }
        (false, "stream") => {
            let since = req
                .header("last-event-id")
                .and_then(|v| v.parse::<usize>().ok())
                .map(|n| n + 1)
                .unwrap_or_else(|| req.q("since").parse().unwrap_or(0));
            stream(sh, s, req.q("conv"), since);
        }
        (false, "workflow") => {
            let conv = req.q("conv");
            if !valid_id(conv) {
                return refuse(&mut s, 400, "bad conversation id");
            }
            let p = sh.cfg.data_dir.join("workflows").join(format!("{conv}.md"));
            let text = std::fs::read_to_string(p).unwrap_or_default();
            send_json(&mut s, 200, &json!({ "text": cap(&text, 100_000) }));
        }
        (false, "memory") => {
            let files = load_index(&sh.cfg);
            let used = used_srcs(sh, req.q("conv"), &files);
            let list: Vec<Value> = files
                .iter()
                .map(|f| json!({"src": f.src, "name": file_name(&f.src), "kind": kind_of(&sh.cfg, f), "chunks": f.chunks.len(), "mtime": f.mtime}))
                .collect();
            let embed = lock(&sh.state).embed_ok;
            send_json(
                &mut s,
                200,
                &json!({"files": list, "used": used, "embed": embed}),
            );
        }
        (false, "memory/file") => {
            let files = load_index(&sh.cfg);
            match files.iter().find(|f| f.src == req.q("src")) {
                Some(f) => {
                    let chunks: Vec<String> =
                        f.chunks.iter().take(6).map(|c| cap(c, 1200)).collect();
                    send_json(
                        &mut s,
                        200,
                        &json!({"src": f.src, "name": file_name(&f.src), "kind": kind_of(&sh.cfg, f), "total": f.chunks.len(), "chunks": chunks}),
                    );
                }
                None => refuse(&mut s, 404, "not an indexed memory file"),
            }
        }
        (false, "memory/graph") => {
            let files = load_index(&sh.cfg);
            let used: HashSet<String> = used_srcs(sh, req.q("conv"), &files).into_iter().collect();
            let mut g = graph_cached(sh, &files);
            for n in g["nodes"].as_array_mut().into_iter().flatten() {
                let u = n["src"].as_str().is_some_and(|s| used.contains(s));
                n["used"] = json!(u);
            }
            send_json(&mut s, 200, &g);
        }
        _ => refuse(&mut s, 404, "not found"),
    }
}

fn file_name(src: &str) -> String {
    Path::new(src)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn kind_of(cfg: &Config, f: &RagFile) -> &'static str {
    if cfg.pinned_current_files.iter().any(|(p, _)| *p == f.src) {
        "current"
    } else if f.from_glob {
        "memory"
    } else {
        "core"
    }
}

fn used_srcs(sh: &Shared, conv: &str, files: &[RagFile]) -> Vec<String> {
    let names: Vec<String> = lock(&sh.state)
        .convs
        .get(conv)
        .map(|l| l.used.clone())
        .unwrap_or_default();
    files
        .iter()
        .filter(|f| names.contains(&file_name(&f.src)))
        .map(|f| f.src.clone())
        .collect()
}

/// Health. Without the key it says only what a caller needs to talk to this agent (`auth`,
/// `stream`, `guest`); with it, the engines too, probed at most every 15 s (an unkeyed caller
/// can neither learn the machines' addresses nor make the agent probe them over and over).
fn health(sh: &Shared, keyed: bool) -> Value {
    // `stream: true` tells the chat surface it can use /agent/stream instead of polling.
    // `guest: true`: POST /agent/new {"mode":"guest"} makes a room-safe conversation.
    // `auth: "key"`: every other call needs X-Tempo-Key (see `serve`).
    let auth = if sh.opts.key.is_some() { "key" } else { "none" };
    if !keyed {
        return json!({"ok": true, "stream": true, "guest": true, "roles": true, "engines": true, "auth": auth});
    }
    let hosts = {
        let mut seen = lock(&sh.hosts_seen);
        match seen.as_ref() {
            Some((t, v)) if t.elapsed() < Duration::from_secs(15) => v.clone(),
            _ => {
                let mut hosts = Map::new();
                for (name, h) in sh.cfg.all_hosts() {
                    let up = h.urls.iter().any(|u| {
                        let t = if u.contains("127.0.0.1") || u.contains("localhost") {
                            5
                        } else {
                            2
                        };
                        http_json(&format!("{u}/api/tags"), None, Duration::from_secs(t)).is_ok()
                    });
                    let v = if up {
                        "up".to_string()
                    } else {
                        format!("unreachable on {}", h.urls.join(", "))
                    };
                    hosts.insert(name, json!(v));
                }
                let v = Value::Object(hosts);
                *seen = Some((Instant::now(), v.clone()));
                v
            }
        }
    };
    let embed = lock(&sh.state).embed_ok;
    json!({"ok": true, "net": net_up(&sh.cfg), "models": sh.cfg.models, "hosts": hosts, "rag": embed, "stream": true, "guest": true, "roles": true, "engines": true, "auth": auth})
}

/// A new conversation: a guest one for a chat room (no tools, memory or owner context), with the
/// model, role and engine a Room teammate was given, if any.
fn new_conv(
    sh: &Arc<Shared>,
    guest: bool,
    model: Option<String>,
    role: Option<String>,
    engine: Option<String>,
    format: Option<Value>,
    mut s: TcpStream,
) {
    match Conv::open(&sh.cfg, None) {
        Ok(mut c) => {
            c.guest = guest;
            c.model = model;
            c.role = role;
            c.engine = engine;
            c.format = format;
            if guest
                || c.model.is_some()
                || c.role.is_some()
                || c.engine.is_some()
                || c.format.is_some()
            {
                c.save();
            }
            lock(&sh.state).convs.entry(c.id.clone()).or_default().guest = c.guest;
            send_json(&mut s, 200, &json!({"conv": c.id}));
        }
        Err(e) => send_json(&mut s, 500, &json!({ "error": e })),
    }
}

fn convs(cfg: &Config) -> Vec<String> {
    let mut fs: Vec<(f64, String)> = std::fs::read_dir(cfg.data_dir.join("conversations"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?.to_string();
            valid_id(&id).then(|| (util::mtime_of(&e.path()), id))
        })
        .collect();
    fs.sort_by(|a, b| b.0.total_cmp(&a.0));
    fs.into_iter().take(30).map(|(_, id)| id).collect()
}

fn stream(sh: &Arc<Shared>, mut s: TcpStream, conv: &str, since: usize) {
    if sh.streams.fetch_add(1, Ordering::SeqCst) >= MAX_STREAMS {
        sh.streams.fetch_sub(1, Ordering::SeqCst);
        return refuse(&mut s, 503, "too many streams");
    }
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\nretry: 2000\n\n";
    let mut ok = s.write_all(head.as_bytes()).is_ok();
    let (mut next, mut last_busy) = (since, None);
    while ok {
        let (events, busy) = {
            let mut st = lock(&sh.state);
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                sh.ensure(&mut st, conv);
                let (len, busy) = st
                    .convs
                    .get(conv)
                    .map_or((0, false), |l| (l.events.len(), l.busy));
                let now = Instant::now();
                if len > next || Some(busy) != last_busy || now >= deadline {
                    break;
                }
                st = sh
                    .changed
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            let log = st.convs.get(conv);
            (
                log.and_then(|l| l.events.get(next..))
                    .map(<[Value]>::to_vec)
                    .unwrap_or_default(),
                log.is_some_and(|l| l.busy),
            )
        };
        let mut out = String::new();
        for e in &events {
            out.push_str(&format!("id: {}\ndata: {e}\n\n", e["n"]));
        }
        next += events.len();
        if Some(busy) != last_busy {
            out.push_str(&format!(
                "event: state\ndata: {}\n\n",
                json!({ "busy": busy })
            ));
            last_busy = Some(busy);
        }
        if out.is_empty() {
            out.push_str(": keep-alive\n\n");
        }
        ok = s.write_all(out.as_bytes()).and_then(|_| s.flush()).is_ok();
    }
    sh.streams.fetch_sub(1, Ordering::SeqCst);
}

fn static_file(s: &mut TcpStream, dir: &Path, rel: &str) {
    let safe = !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains("..")
        && !rel.contains("//")
        && rel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'));
    let found = safe
        .then(|| dir.join(rel))
        .filter(|p| p.is_file())
        .and_then(|p| Some((p.canonicalize().ok()?, dir.canonicalize().ok()?)))
        .filter(|(p, d)| p.starts_with(d));
    let Some((p, _)) = found else {
        return refuse(s, 404, "not found");
    };
    let ctype = match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "mjs" | "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    match std::fs::read(&p) {
        Ok(b) => {
            let _ = respond(s, 200, ctype, &b);
        }
        Err(_) => refuse(s, 404, "not found"),
    }
}

/// Everything that is not /agent/* or /chat/* goes to the Tempo GUI server on loopback, byte for byte.
fn pass_through(req: &Req, mut s: TcpStream, port: u16) {
    let Ok(mut up) = TcpStream::connect(("127.0.0.1", port)) else {
        return refuse(&mut s, 502, "GUI server unavailable");
    };
    let _ = up.set_read_timeout(Some(Duration::from_secs(600)));
    let mut head = format!("{} {}", req.method, req.path);
    if !req.query.is_empty() {
        let q: Vec<String> = req
            .query
            .iter()
            .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
            .collect();
        head.push('?');
        head.push_str(&q.join("&"));
    }
    head.push_str(" HTTP/1.1\r\n");
    for (k, v) in &req.headers {
        if !matches!(
            k.as_str(),
            "host" | "connection" | "content-length" | "keep-alive"
        ) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str(&format!(
        "host: 127.0.0.1:{port}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
        req.body.len()
    ));
    if up.write_all(head.as_bytes()).is_err() || up.write_all(&req.body).is_err() {
        return refuse(&mut s, 502, "GUI server unavailable");
    }
    let _ = std::io::copy(&mut up, &mut s);
}

fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ---- second-brain graph: files as nodes; an edge joins files sharing >= 2 of their top-12 tf-idf terms

const GRAPH_STOP: &str = "that this with from have will your what when they there their which about would should could into also been were than then them only some more most other such each here just like make made does done over very after before because where while these those name description type true false none null http https file files using used need needs want first last next still even much many well back down same both every until within without across between";

fn word_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[a-z][a-z0-9]{3,}").expect("static regex"))
}

fn graph_cached(sh: &Shared, files: &[RagFile]) -> Value {
    let key: Vec<(String, u64)> = files
        .iter()
        .map(|f| (f.src.clone(), f.mtime.to_bits()))
        .collect();
    let mut cache = lock(&sh.graph);
    if let Some((k, v)) = cache.as_ref() {
        if *k == key {
            return v.clone();
        }
    }
    let v = graph(&sh.cfg, files);
    *cache = Some((key, v.clone()));
    v
}

pub fn graph(cfg: &Config, files: &[RagFile]) -> Value {
    const TERMS: usize = 12;
    const PER_NODE: usize = 4;
    let files = &files[..files.len().min(400)];
    let stop: HashSet<&str> = GRAPH_STOP.split_whitespace().collect();
    let mut df: HashMap<String, usize> = HashMap::new();
    let tfs: Vec<HashMap<String, usize>> = files
        .iter()
        .map(|f| {
            let mut tf = HashMap::new();
            for c in &f.chunks {
                let low = c.to_lowercase();
                for m in word_rx().find_iter(&low) {
                    if !stop.contains(m.as_str()) {
                        *tf.entry(m.as_str().to_string()).or_insert(0) += 1;
                    }
                }
            }
            for w in tf.keys() {
                *df.entry(w.clone()).or_insert(0) += 1;
            }
            tf
        })
        .collect();
    let n = files.len();
    let tops: Vec<HashSet<&String>> = tfs
        .iter()
        .map(|tf| {
            let score = |w: &String| tf[w] as f64 * ((1 + n) as f64 / (1 + df[w]) as f64).ln();
            let mut ws: Vec<&String> = tf.keys().collect();
            ws.sort_by(|a, b| score(b).total_cmp(&score(a)).then_with(|| a.cmp(b)));
            ws.into_iter().take(TERMS).collect()
        })
        .collect();
    let mut cand: Vec<Vec<(usize, usize)>> = vec![vec![]; n];
    for i in 0..n {
        for j in i + 1..n {
            let shared = tops[i].intersection(&tops[j]).count();
            if shared >= 2 {
                cand[i].push((shared, j));
                cand[j].push((shared, i));
            }
        }
    }
    let mut edges: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for (i, c) in cand.iter_mut().enumerate() {
        c.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for &(sh, j) in c.iter().take(PER_NODE) {
            edges.insert((i.min(j), i.max(j)), sh);
        }
    }
    let nodes: Vec<Value> = files
        .iter()
        .enumerate()
        .map(|(i, f)| json!({"id": i, "src": f.src, "name": file_name(&f.src), "kind": kind_of(cfg, f), "chunks": f.chunks.len(), "used": false}))
        .collect();
    let edges: Vec<Value> = edges
        .into_iter()
        .map(|((a, b), s)| json!({"a": a, "b": b, "w": (s as f64 / TERMS as f64 * 1000.0).round() / 1000.0}))
        .collect();
    json!({ "nodes": nodes, "edges": edges })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::*;
    use std::io::Read;

    struct Srv {
        addr: SocketAddr,
        cfg: Config,
        _mock: Mock,
    }

    fn server(script: Vec<(u16, String)>, tweak: impl FnOnce(&mut Config, &mut ServeOpts)) -> Srv {
        let d = tmpdir("serve");
        let mut c = Config::defaults(Some(d.to_string_lossy().into_owned()));
        let mock = Mock::start(chat_script(script));
        c.ollama_url = mock.url.clone();
        c.models = vec!["heavy-a".into()];
        c.fast_models = vec![];
        c.retries = 0;
        c.retry_delay_ms = 0;
        c.offline_probe = "127.0.0.1:9".into();
        let mut o = ServeOpts {
            port: 0,
            ..ServeOpts::default()
        };
        tweak(&mut c, &mut o);
        crate::config::ensure_dirs(&c).unwrap();
        let r = start(c.clone(), BTreeMap::new(), o).unwrap();
        Srv {
            addr: r.addr,
            cfg: c,
            _mock: mock,
        }
    }

    /// Raw HTTP so each test controls every header. Returns (status, body).
    fn raw(addr: SocketAddr, req: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        let code = out
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = out
            .split_once("\r\n\r\n")
            .map(|x| x.1)
            .unwrap_or("")
            .to_string();
        (code, body)
    }

    fn get(srv: &Srv, path: &str) -> (u16, Value) {
        let (c, b) = raw(
            srv.addr,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                srv.addr.port()
            ),
        );
        (c, serde_json::from_str(&b).unwrap_or(Value::Null))
    }

    fn post(srv: &Srv, path: &str, body: &Value) -> (u16, Value) {
        let b = body.to_string();
        let (c, out) = raw(
            srv.addr,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
                b.len(),
                p = srv.addr.port()
            ),
        );
        (c, serde_json::from_str(&out).unwrap_or(Value::Null))
    }

    fn wait_idle(srv: &Srv, conv: &str) -> Vec<Value> {
        let t0 = Instant::now();
        loop {
            let (_, v) = get(srv, &format!("/agent/events?conv={conv}&since=0"));
            if v["busy"] == false && v["events"].as_array().is_some_and(|e| !e.is_empty()) {
                return v["events"].as_array().unwrap().clone();
            }
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "turn did not finish: {v}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    #[test]
    fn the_catalog_lists_commands_tools_and_needs_the_key() {
        let srv = server(vec![], |_, o| o.key = Some("k3y".repeat(20)));
        let p = srv.addr.port();
        let (c, _) = raw(
            srv.addr,
            &format!("GET /agent/catalog HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n"),
        );
        assert_eq!(c, 401);
        let (c, b) = raw(
            srv.addr,
            &format!(
                "GET /agent/catalog HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nX-Tempo-Key: {}\r\n\r\n",
                "k3y".repeat(20)
            ),
        );
        assert_eq!(c, 200);
        let v: Value = serde_json::from_str(&b).unwrap();
        let has = |list: &str, n: &str| v[list].as_array().unwrap().iter().any(|x| x["name"] == n);
        assert!(has("commands", "/undo") && has("commands", "/soundcheck") && has("tools", "grep"));
    }

    #[test]
    fn guest_conversations_for_chat_rooms() {
        let srv = server(vec![text_resp("Hello from the room")], |_, _| {});
        let (_, h) = get(&srv, "/agent/health");
        assert_eq!(h["guest"], true, "rooms can see that guest mode is there");
        let (c, v) = post(&srv, "/agent/new", &json!({"mode": "guest"}));
        assert_eq!(c, 200);
        let conv = v["conv"].as_str().unwrap().to_string();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(srv.cfg.data_dir.join(format!("conversations/{conv}.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["mode"], "guest");
        let (c, _) = post(
            &srv,
            "/agent/send",
            &json!({"conv": conv, "text": "hi tempo"}),
        );
        assert_eq!(c, 200);
        let evs = wait_idle(&srv, &conv);
        assert!(
            evs.iter()
                .any(|e| e["text"] == "memory: off (guest conversation)"),
            "{evs:?}"
        );
        assert_eq!(evs.last().unwrap()["text"], "Hello from the room");
    }

    #[test]
    fn a_room_teammate_gets_its_own_model_and_role() {
        let srv = server(vec![text_resp("Booked for Friday.")], |_, _| {});
        let (_, h) = get(&srv, "/agent/health");
        assert_eq!(
            h["roles"], true,
            "rooms can see that teammates are supported"
        );
        let role = "You are the band's booking manager. Keep answers short.";
        let (c, v) = post(
            &srv,
            "/agent/new",
            &json!({"mode": "guest", "model": "teammate-model:8b", "role": role}),
        );
        assert_eq!(c, 200);
        let conv = v["conv"].as_str().unwrap().to_string();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(srv.cfg.data_dir.join(format!("conversations/{conv}.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (
                saved["mode"].as_str(),
                saved["model"].as_str(),
                saved["role"].as_str()
            ),
            (Some("guest"), Some("teammate-model:8b"), Some(role))
        );
        let (c, _) = post(
            &srv,
            "/agent/send",
            &json!({"conv": conv, "text": "can we play friday?"}),
        );
        assert_eq!(c, 200);
        let evs = wait_idle(&srv, &conv);
        assert_eq!(evs.last().unwrap()["text"], "Booked for Friday.");
        // a bad model name or an overlong role is refused, and nothing is made
        assert_eq!(
            post(
                &srv,
                "/agent/new",
                &json!({"mode": "guest", "model": "bad name; rm"})
            )
            .0,
            400
        );
        assert_eq!(
            post(
                &srv,
                "/agent/new",
                &json!({"mode": "guest", "role": "x".repeat(2001)})
            )
            .0,
            400
        );
    }

    #[test]
    fn a_conversation_can_be_pinned_to_one_of_the_configured_engines() {
        let other = Mock::start(chat_script(vec![text_resp(
            "Answered on the other engine.",
        )]));
        let url = other.url.clone();
        let srv = server(vec![], move |c, _| c.ollama_fallbacks = vec![url]);
        let (_, h) = get(&srv, "/agent/health");
        assert_eq!(h["engines"], true, "callers can see that pinning is there");
        // an engine the agent does not know, a wrong type: refused, and no conversation is made
        for bad in [json!("http://10.9.9.9:11434"), json!(7), json!({"url": 1})] {
            let (c, v) = post(&srv, "/agent/new", &json!({"mode": "guest", "engine": bad}));
            assert_eq!(c, 400, "{bad}");
            assert!(v["error"].as_str().unwrap().starts_with("engine:"), "{v}");
        }
        assert!(convs(&srv.cfg).is_empty());
        // a configured one is taken, with or without a trailing slash, and saved as written
        let (c, v) = post(
            &srv,
            "/agent/new",
            &json!({"mode": "guest", "engine": format!("{}/", other.url)}),
        );
        assert_eq!(c, 200);
        let conv = v["conv"].as_str().unwrap().to_string();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(srv.cfg.data_dir.join(format!("conversations/{conv}.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["engine"], other.url.as_str());
        let (c, _) = post(&srv, "/agent/send", &json!({"conv": conv, "text": "hello"}));
        assert_eq!(c, 200);
        let evs = wait_idle(&srv, &conv);
        assert_eq!(evs.last().unwrap()["text"], "Answered on the other engine.");
        assert_eq!(other.bodies("/api/chat").len(), 1);
        // the engine that answered comes with the events
        let (_, v) = get(&srv, &format!("/agent/events?conv={conv}&since=0"));
        assert_eq!(v["engine"], other.url.as_str());
        // no engine, no pin
        let (_, v) = post(&srv, "/agent/new", &json!({"mode": "guest", "engine": ""}));
        let conv = v["conv"].as_str().unwrap();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(srv.cfg.data_dir.join(format!("conversations/{conv}.json")))
                .unwrap(),
        )
        .unwrap();
        assert!(saved["engine"].is_null());
    }

    #[test]
    fn a_conversation_can_be_opened_with_a_json_schema() {
        let srv = server(vec![text_resp("{}")], |_, _| {});
        let schema = json!({"type": "object", "required": ["name"]});
        let (c, v) = post(
            &srv,
            "/agent/new",
            &json!({"mode": "guest", "format": schema}),
        );
        assert_eq!(c, 200);
        let conv = v["conv"].as_str().unwrap();
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(srv.cfg.data_dir.join(format!("conversations/{conv}.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["format"], schema);
        for bad in [json!("json"), json!(7), json!([1])] {
            let (c, v) = post(&srv, "/agent/new", &json!({"mode": "guest", "format": bad}));
            assert_eq!(c, 400, "{v}");
        }
    }

    #[test]
    fn a_guest_conversation_stays_a_guest_even_if_its_file_is_lost() {
        let srv = server(vec![text_resp("Hello from the room")], |_, _| {});
        let (_, v) = post(&srv, "/agent/new", &json!({"mode": "guest"}));
        let conv = v["conv"].as_str().unwrap().to_string();
        let file = srv.cfg.data_dir.join(format!("conversations/{conv}.json"));
        // the file is gone (a failed save, a clean-up): the room's conversation must not turn into the owner's
        std::fs::remove_file(&file).unwrap();
        let (c, _) = post(
            &srv,
            "/agent/send",
            &json!({"conv": conv, "text": "list the files in C:\\"}),
        );
        assert_eq!(c, 200);
        let evs = wait_idle(&srv, &conv);
        assert!(
            evs.iter()
                .any(|e| e["text"] == "memory: off (guest conversation)"),
            "{evs:?}"
        );
        assert!(
            evs.iter().all(|e| !e["text"]
                .as_str()
                .is_some_and(|t| t.contains("tools offered: 1"))),
            "{evs:?}"
        );
    }

    #[test]
    fn device_names_are_not_conversation_ids() {
        for bad in [
            "con", "CON", "nul", "Aux.x", "prn", "com1", "LPT9", "com3.log", "conin$",
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
        for ok in [
            "console",
            "com0",
            "com10",
            "lpt",
            "2026-11-06-0102-ab12",
            "a.b",
        ] {
            assert!(valid_id(ok), "{ok}");
        }
    }

    #[test]
    fn same_site_pages_are_refused_too() {
        let srv = server(vec![], |_, _| {});
        let h = format!("Host: 127.0.0.1:{}\r\n", srv.addr.port());
        for site in ["cross-site", "same-site"] {
            let (c, _) = raw(
                srv.addr,
                &format!("GET /agent/convs HTTP/1.1\r\n{h}Sec-Fetch-Site: {site}\r\n\r\n"),
            );
            assert_eq!(c, 403, "{site}");
        }
        for site in ["same-origin", "none"] {
            let (c, _) = raw(
                srv.addr,
                &format!("GET /agent/convs HTTP/1.1\r\n{h}Sec-Fetch-Site: {site}\r\n\r\n"),
            );
            assert_eq!(c, 200, "{site}");
        }
    }

    #[test]
    fn every_call_but_health_needs_the_key() {
        let chat = tmpdir("chat-key");
        std::fs::write(chat.join("chat.mjs"), "export {}").unwrap();
        let srv = server(vec![], |_, o| {
            o.key = Some("k3y".repeat(20));
            o.chat_dir = Some(chat.clone());
        });
        let p = srv.addr.port();
        let get = |path: &str, extra: &str| {
            raw(
                srv.addr,
                &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n{extra}\r\n"),
            )
        };
        let (c, b) = get("/agent/convs", "");
        assert_eq!(c, 401);
        assert!(b.contains("tempo-launch open agent"), "{b}");
        assert_eq!(get("/agent/convs", "X-Tempo-Key: wrong\r\n").0, 401);
        assert_eq!(
            get(
                "/agent/convs",
                &format!("X-Tempo-Key: {}\r\n", "k3y".repeat(20))
            )
            .0,
            200
        );
        let (c, b) = get("/agent/health", "");
        assert_eq!(c, 200);
        assert!(
            b.contains("\"auth\":\"key\"") && b.contains("\"guest\":true"),
            "{b}"
        );
        // without the key, health names no engines, machines or models
        assert!(!b.contains("hosts") && !b.contains("models"), "{b}");
        let (_, b) = get(
            "/agent/health",
            &format!("X-Tempo-Key: {}\r\n", "k3y".repeat(20)),
        );
        assert!(b.contains("\"hosts\"") && b.contains("\"models\""), "{b}");
        // the page's own files need no key
        assert_eq!(get("/chat/chat.mjs", "").0, 200);
        // a POST without the key changes nothing
        let body = "{}";
        let (c, _) = raw(srv.addr, &format!("POST /agent/new HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()));
        assert_eq!(c, 401);
    }

    #[test]
    fn listening_beyond_this_computer_is_asked_for() {
        let o = ServeOpts {
            bind: "0.0.0.0".parse().unwrap(),
            ..ServeOpts::default()
        };
        assert!(check_bind(&o).unwrap_err().contains("--allow-remote"));
        assert!(check_bind(&ServeOpts {
            allow_remote: true,
            ..o
        })
        .is_ok());
        assert!(check_bind(&ServeOpts::default()).is_ok());
        let d = tmpdir("key-file");
        let k = util::new_key();
        assert!(k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit()) && k != util::new_key());
        let p = util::write_key(&d, &k).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), k);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            util::same_secret(b"abc", b"abc")
                && !util::same_secret(b"abc", b"abd")
                && !util::same_secret(b"abc", b"ab")
        );
    }

    #[test]
    fn a_host_behind_tailscale_serve_has_no_port_and_https_origin_is_ours() {
        let srv = server(vec![], |_, o| {
            o.allow_hosts = vec!["tempo.local:443".into()]
        });
        let req = |extra: &str| {
            raw(
                srv.addr,
                &format!("GET /agent/convs HTTP/1.1\r\n{extra}\r\n"),
            )
        };
        assert_eq!(
            req("Host: tempo.local\r\n").0,
            200,
            "the bare name from the HTTPS front"
        );
        assert_eq!(
            req("Host: tempo.local\r\nOrigin: https://tempo.local\r\n").0,
            200
        );
        assert_eq!(
            req("Host: tempo.local\r\nOrigin: https://evil.example\r\n").0,
            403
        );
        assert_eq!(req("Host: evil.example\r\n").0, 403);
    }

    #[test]
    fn host_origin_method_and_body_checks() {
        let srv = server(vec![], |_, o| o.allow_hosts = vec!["tempo.local".into()]);
        let p = srv.addr.port();
        let req = |extra: &str| {
            raw(
                srv.addr,
                &format!("GET /agent/convs HTTP/1.1\r\n{extra}\r\n"),
            )
        };
        assert_eq!(req(&format!("Host: 127.0.0.1:{p}\r\n")).0, 200);
        assert_eq!(req(&format!("Host: localhost:{p}\r\n")).0, 200);
        assert_eq!(
            req(&format!("Host: tempo.local:{p}\r\n")).0,
            200,
            "--allow-host"
        );
        assert_eq!(req("").0, 400, "no Host");
        assert_eq!(
            req(&format!("Host: evil.example:{p}\r\n")).0,
            403,
            "DNS rebinding"
        );
        assert_eq!(
            req(&format!("Host: 127.0.0.1:{}\r\n", p.wrapping_add(1))).0,
            403
        );
        let h = format!("Host: 127.0.0.1:{p}\r\n");
        assert_eq!(req(&format!("{h}Origin: http://evil.example\r\n")).0, 403);
        assert_eq!(req(&format!("{h}Origin: http://127.0.0.1:{p}\r\n")).0, 200);
        assert_eq!(req(&format!("{h}Sec-Fetch-Site: cross-site\r\n")).0, 403);
        for m in ["PUT", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
            let (c, _) = raw(srv.addr, &format!("{m} /agent/convs HTTP/1.1\r\n{h}\r\n"));
            assert_eq!(c, 405, "{m}");
        }
        let (c, _) = raw(srv.addr, &format!("POST /agent/new HTTP/1.1\r\n{h}Content-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}"));
        assert_eq!(c, 415, "form posts (CSRF) are refused");
        let (c, _) = raw(srv.addr, &format!("POST /agent/new HTTP/1.1\r\n{h}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1));
        assert_eq!(c, 413);
        let (c, _) = raw(srv.addr, &format!("POST /agent/new HTTP/1.1\r\n{h}Content-Type: application/json\r\nContent-Length: 3\r\n\r\nxyz"));
        assert_eq!(c, 400, "not JSON");
        assert_eq!(get(&srv, "/agent/nope").0, 404);
        assert_eq!(get(&srv, "/elsewhere").0, 404, "no GUI server configured");
    }

    #[test]
    fn a_turn_streams_events_in_the_gui_shape() {
        let script = vec![
            call_resp("list_dir", json!({"path": "."})),
            text_resp("Here is the folder."),
        ];
        let srv = server(script, |_, _| {});
        let (c, v) = post(&srv, "/agent/new", &json!({}));
        assert_eq!(c, 200);
        let conv = v["conv"].as_str().unwrap().to_string();
        let (c, _) = post(
            &srv,
            "/agent/send",
            &json!({"conv": conv, "text": "list the folder"}),
        );
        assert_eq!(c, 200);
        let evs = wait_idle(&srv, &conv);
        let kinds: Vec<&str> = evs.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(kinds[0], "user");
        assert!(kinds.contains(&"delta"), "{kinds:?}");
        let call = evs.iter().find(|e| e["type"] == "tool_call").unwrap();
        assert_eq!(call["name"], "list_dir");
        assert_eq!(call["args"], r#"{"path":"."}"#);
        let res = evs.iter().find(|e| e["type"] == "tool_result").unwrap();
        assert_eq!(res["name"], "list_dir");
        assert_eq!(evs.last().unwrap()["type"], "assistant");
        assert_eq!(evs.last().unwrap()["text"], "Here is the folder.");
        for (i, e) in evs.iter().enumerate() {
            assert_eq!(e["n"], json!(i));
        }
        // A restarted GUI reads the same history (deltas are live-only, never saved).
        let srv2_events = {
            let mut st = State::default();
            let sh = Shared {
                cfg: srv.cfg.clone(),
                opts: ServeOpts::default(),
                allowed: HashSet::new(),
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
                jobs: Mutex::new(mpsc::channel().0),
                conns: AtomicUsize::new(0),
                streams: AtomicUsize::new(0),
                graph: Mutex::new(None),
                hosts_seen: Mutex::new(None),
                mcp_tools: Mutex::new(vec![]),
                catalog: Mutex::new(None),
            };
            assert!(sh.ensure(&mut st, &conv));
            st.convs[&conv].events.clone()
        };
        assert!(srv2_events.iter().all(|e| e["type"] != "delta"));
        assert_eq!(srv2_events.last().unwrap()["text"], "Here is the folder.");
        // The SSE stream replays from `since` with ids, then reports the idle state.
        let mut s = TcpStream::connect(srv.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "GET /agent/stream?conv={conv}&since=0 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            srv.addr.port()
        )
        .unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = String::new();
        while !got.contains("event: state") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0);
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(got.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream"));
        assert!(got.contains("id: 0\ndata: {"), "{got}");
        assert!(got.contains("event: state\ndata: {\"busy\":false}"));
        let data: Vec<Value> = got
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str(d).ok())
            .filter(|v: &Value| v.get("type").is_some())
            .collect();
        assert_eq!(data.len(), evs.len());
        // Unknown conversations cannot be sent to, and reading never creates one.
        let (c, _) = post(
            &srv,
            "/agent/send",
            &json!({"conv": "no-such-conv", "text": "hi"}),
        );
        assert_eq!(c, 404);
        let (_, v) = get(&srv, "/agent/events?conv=nobody&since=0");
        assert_eq!(v, json!({"events": [], "busy": false}));
        assert!(!srv.cfg.data_dir.join("conversations/nobody.json").exists());
        let (_, l) = get(&srv, "/agent/convs");
        assert_eq!(l, json!([conv]));
    }

    #[test]
    fn confirmations_resolve_only_with_the_issued_id() {
        let srv = server(vec![], |_, _| {});
        let f = srv.cfg.data_dir.join("keep.txt");
        std::fs::write(&f, "original").unwrap();
        let path = f.to_string_lossy().into_owned();
        // Re-script the engine for this server: overwrite (needs approval), then a reply.
        let mock = Mock::start(chat_script(vec![
            call_resp("write_file", json!({"path": path, "content": "new"})),
            text_resp("done"),
            call_resp("write_file", json!({"path": path, "content": "newer"})),
            text_resp("done again"),
        ]));
        let mut cfg = srv.cfg.clone();
        cfg.ollama_url = mock.url.clone();
        let opts = ServeOpts {
            port: 0,
            confirm_timeout: Duration::from_secs(10),
            ..ServeOpts::default()
        };
        let r = start(cfg.clone(), BTreeMap::new(), opts).unwrap();
        let srv = Srv {
            addr: r.addr,
            cfg,
            _mock: mock,
        };
        let (_, v) = post(&srv, "/agent/new", &json!({}));
        let conv = v["conv"].as_str().unwrap().to_string();
        post(
            &srv,
            "/agent/send",
            &json!({"conv": conv, "text": "write the file"}),
        );
        let t0 = Instant::now();
        let ask = loop {
            let (_, v) = get(&srv, &format!("/agent/events?conv={conv}&since=0"));
            if let Some(e) = v["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["type"] == "confirm")
            {
                break e.clone();
            }
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(ask["what"], "overwrite file");
        let id = ask["id"].as_str().unwrap();
        assert!(id.len() >= 32, "an approval id must not be guessable: {id}");
        assert_eq!(
            post(
                &srv,
                "/agent/confirm",
                &json!({"conv": conv, "id": "forged", "ok": true})
            )
            .1,
            json!({"ok": false})
        );
        assert_eq!(
            post(
                &srv,
                "/agent/confirm",
                &json!({"conv": "other", "id": id, "ok": true})
            )
            .1,
            json!({"ok": false})
        );
        assert_eq!(
            post(
                &srv,
                "/agent/confirm",
                &json!({"conv": conv, "id": id, "ok": "yes"})
            )
            .1,
            json!({"ok": true})
        );
        let evs = wait_idle(&srv, &conv);
        assert!(
            evs.iter()
                .any(|e| e["text"] == "overwrite file: denied or timed out"),
            "a non-true answer denies"
        );
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
        assert_eq!(
            post(
                &srv,
                "/agent/confirm",
                &json!({"conv": conv, "id": id, "ok": true})
            )
            .1,
            json!({"ok": false}),
            "an id resolves once"
        );
        // Second turn: approve.
        let before = evs.len();
        post(
            &srv,
            "/agent/send",
            &json!({"conv": conv, "text": "write the file again"}),
        );
        let t0 = Instant::now();
        let id2 = loop {
            let (_, v) = get(&srv, &format!("/agent/events?conv={conv}&since={before}"));
            if let Some(e) = v["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["type"] == "confirm")
            {
                break e["id"].as_str().unwrap().to_string();
            }
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            post(
                &srv,
                "/agent/confirm",
                &json!({"conv": conv, "id": id2, "ok": true})
            )
            .1,
            json!({"ok": true})
        );
        wait_idle(&srv, &conv);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "newer");
    }

    #[test]
    fn unanswered_confirmations_time_out_as_denied() {
        let srv = server(vec![], |_, _| {});
        let f = srv.cfg.data_dir.join("keep.txt");
        std::fs::write(&f, "original").unwrap();
        let mock = Mock::start(chat_script(vec![
            call_resp(
                "write_file",
                json!({"path": f.to_string_lossy(), "content": "new"}),
            ),
            text_resp("ok"),
        ]));
        let mut cfg = srv.cfg.clone();
        cfg.ollama_url = mock.url.clone();
        let opts = ServeOpts {
            port: 0,
            confirm_timeout: Duration::from_millis(200),
            ..ServeOpts::default()
        };
        let r = start(cfg.clone(), BTreeMap::new(), opts).unwrap();
        let srv = Srv {
            addr: r.addr,
            cfg,
            _mock: mock,
        };
        let (_, v) = post(&srv, "/agent/new", &json!({}));
        let conv = v["conv"].as_str().unwrap().to_string();
        post(
            &srv,
            "/agent/send",
            &json!({"conv": conv, "text": "write the file"}),
        );
        let evs = wait_idle(&srv, &conv);
        assert!(evs
            .iter()
            .any(|e| e["text"] == "overwrite file: denied or timed out"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "original");
    }

    #[test]
    fn memory_views_and_static_files() {
        let srv = server(vec![], |c, o| {
            let notes = c.data_dir.join("notes");
            std::fs::create_dir_all(&notes).unwrap();
            for i in 0..6 {
                let topic = [
                    "rust crate agent loop lanes tests",
                    "gpu lease comfyui render vram",
                ][i % 2];
                std::fs::write(
                    notes.join(format!("n{i}.md")),
                    format!("{} \n\n{}", topic.repeat(4), topic.repeat(3)),
                )
                .unwrap();
            }
            let secret = format!("api_key = {}", "abcdef".repeat(4));
            std::fs::write(
                notes.join("core.md"),
                format!("# Core\n\nrust crate agent loop\n\n{secret}\n"),
            )
            .unwrap();
            c.rag_files = vec![notes.join("core.md").to_string_lossy().into_owned()];
            c.rag_globs = vec![(notes.join("n*.md").to_string_lossy().into_owned(), 50)];
            let chat = c.data_dir.join("chat");
            std::fs::create_dir_all(&chat).unwrap();
            std::fs::write(chat.join("chat.css"), ".ch{}").unwrap();
            std::fs::write(c.data_dir.join("outside.txt"), "no").unwrap();
            o.chat_dir = Some(chat);
        });
        // The worker builds the index when it starts.
        let t0 = Instant::now();
        let files = loop {
            let (_, v) = get(&srv, "/agent/memory");
            if v["files"].as_array().is_some_and(|f| f.len() == 7) {
                break v;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "{v}");
            std::thread::sleep(Duration::from_millis(30));
        };
        let core = files["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "core.md")
            .unwrap();
        assert_eq!(core["kind"], "core");
        assert_eq!(files["used"], json!([]));
        let src = core["src"].as_str().unwrap();
        let (c, f) = get(&srv, &format!("/agent/memory/file?src={}", pct_encode(src)));
        assert_eq!(c, 200);
        let text = f["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect::<String>();
        assert!(text.contains("[REDACTED]") && !text.contains(&"abcdef".repeat(4)));
        let (c, _) = get(
            &srv,
            &format!("/agent/memory/file?src={}", pct_encode("/etc/passwd")),
        );
        assert_eq!(c, 404, "only indexed files");
        let (_, g) = get(&srv, "/agent/memory/graph");
        assert_eq!(g["nodes"].as_array().unwrap().len(), 7);
        assert!(g["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["used"] == false));
        let edges = g["edges"].as_array().unwrap();
        assert!(!edges.is_empty());
        for e in edges {
            let w = e["w"].as_f64().unwrap();
            assert!(w > 0.0 && w <= 1.0 && e["a"].as_u64() < e["b"].as_u64());
        }
        // Static chat files: only inside the folder.
        let p = srv.addr.port();
        let (c, b) = raw(
            srv.addr,
            &format!("GET /chat/chat.css HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n"),
        );
        assert_eq!((c, b.as_str()), (200, ".ch{}"));
        for bad in [
            "/chat/../outside.txt",
            "/chat/%2e%2e/outside.txt",
            "/chat//etc/passwd",
            "/chat/",
        ] {
            let (c, _) = raw(
                srv.addr,
                &format!("GET {bad} HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\n\r\n"),
            );
            assert_eq!(c, 404, "{bad}");
        }
        let (c, h) = get(&srv, "/agent/health");
        assert_eq!(c, 200);
        assert_eq!(h["models"], json!(["heavy-a"]));
        assert!(h["hosts"]["local"].is_string(), "{h}");
        assert_eq!(h["stream"], true);
    }

    #[test]
    fn query_decoding() {
        assert_eq!(pct_decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(pct_decode("%zz"), "%zz");
        assert_eq!(pct_encode("/a b"), "%2Fa%20b");
    }
}
