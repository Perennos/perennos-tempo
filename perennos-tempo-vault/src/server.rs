//! `tempo-vault serve`: the vault app and its JSON API, on loopback.
//!
//! Security model (the same as `tempo-local-agent serve`): binds 127.0.0.1 by default; the Host
//! header must be ours (no DNS rebinding); a present Origin must be ours and cross-site fetches
//! are refused (no CSRF); only GET and POST; POST bodies must be `application/json` (a web page
//! cannot send that cross-origin without a preflight, and OPTIONS is refused) and at most 1 MiB;
//! `--read-only` refuses every change. Attachments are served under a sandboxing CSP, and HTML
//! files are never served as HTML. Paths are always vault-relative and cleaned.
use crate::graph::{self, GraphOpts};
use crate::parse::Prop;
use crate::render::{self, Opts};
use crate::util::{clean_rel, file_name, unix_now};
use crate::vault::Vault;
use crate::write::WriteError;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 1 << 20;
/// Pasted and dropped attachments (raw bytes, only on this path).
const MAX_UPLOAD: usize = 25 << 20;
const UPLOAD_PATH: &str = "/vault/upload";
const MAX_CONNS: usize = 64;
const MAX_STREAMS: usize = 16;

pub const INDEX_HTML: &str = include_str!("../assets/index.html");
pub const APP_JS: &str = include_str!("../assets/app.mjs");
pub const APP_CSS: &str = include_str!("../assets/app.css");
pub const MEMGRAPH_JS: &str = include_str!("../assets/memgraph.mjs");
pub const CANVAS_JS: &str = include_str!("../assets/canvas.mjs");
pub const THREE_JS: &str = include_str!("../assets/vendor/three.module.min.js");
pub const THREE_POST_JS: &str = include_str!("../assets/vendor/three-post.js");
// The second brain world: the same bytes as tempo-agent/gui-chat, so the chat surface and Vault draw one brain.
pub const BRAIN_WORLD_JS: &str = include_str!("../assets/brain-world.mjs");
pub const BRAIN_SIM_JS: &str = include_str!("../assets/brain-sim.mjs");
pub const BRAIN_FEED_JS: &str = include_str!("../assets/brain-feed.mjs");
pub const THREE_LICENSE: &str = include_str!("../assets/vendor/three.LICENSE");
pub const ICON_SVG: &str = include_str!("../assets/icon.svg");
pub const THEME_CSS: &str = include_str!("../assets/tempo-theme.css");
// The official artwork and the site's own fonts (SIL OFL 1.1), served as they are; same files in every Tempo crate.
const BRAND_FILES: [(&str, &str, &[u8]); 8] = [
    (
        "/perennos-insignia.webp",
        "image/webp",
        include_bytes!("../assets/perennos-insignia.webp"),
    ),
    (
        "/perennos-wordmark-cream.png",
        "image/png",
        include_bytes!("../assets/perennos-wordmark-cream.png"),
    ),
    (
        "/favicon-64.png",
        "image/png",
        include_bytes!("../assets/favicon-64.png"),
    ),
    (
        "/fonts/archivo-latin-variable.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/archivo-latin-variable.woff2"),
    ),
    (
        "/fonts/instrument-serif-italic-latin.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/instrument-serif-italic-latin.woff2"),
    ),
    (
        "/fonts/jetbrains-mono-latin-700.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/jetbrains-mono-latin-700.woff2"),
    ),
    (
        "/fonts/bodoni-moda-latin-variable.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/bodoni-moda-latin-variable.woff2"),
    ),
    (
        "/fonts/bodoni-moda-italic-latin-variable.woff2",
        "font/woff2",
        include_bytes!("../assets/fonts/bodoni-moda-italic-latin-variable.woff2"),
    ),
];

pub struct ServeOpts {
    pub bind: IpAddr,
    pub port: u16,
    /// Extra Host values to accept (`name` or `name:port`), for a reverse proxy in front.
    pub allow_hosts: Vec<String>,
    pub read_only: bool,
    /// How often to look for changes made outside the app (Obsidian, an editor, the agent).
    /// Zero turns the watcher off.
    pub watch: Duration,
    /// The agent the Tempo tab talks to (`TEMPO_AGENT_URL`, else 127.0.0.1:8765); this computer only.
    pub agent_url: Option<String>,
}

impl Default for ServeOpts {
    fn default() -> Self {
        ServeOpts {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8766,
            allow_hosts: vec![],
            read_only: false,
            watch: Duration::from_secs(2),
            agent_url: None,
        }
    }
}

#[derive(Default)]
struct State {
    gen: u64,
    recent: VecDeque<(u64, String)>,
}

struct Shared {
    vault: Mutex<Vault>,
    opts: ServeOpts,
    allowed: HashSet<String>,
    state: Mutex<State>,
    changed: Condvar,
    graphs: Mutex<VecDeque<(String, Arc<String>)>>,
    /// Positions by graph shape (nodes and edges): an edit that changes no link keeps its layout.
    layouts: Mutex<VecDeque<(u64, Value, Value)>>,
    conns: AtomicUsize,
    streams: AtomicUsize,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn bump(&self, paths: Vec<String>) {
        if paths.is_empty() {
            return;
        }
        let mut st = lock(&self.state);
        st.gen += 1;
        let g = st.gen;
        for p in paths {
            st.recent.push_back((g, p));
        }
        while st.recent.len() > 500 {
            st.recent.pop_front();
        }
        drop(st);
        lock(&self.graphs).clear();
        self.changed.notify_all();
    }
    fn gen(&self) -> u64 {
        lock(&self.state).gen
    }
}

/// A running server (tests use the address; `serve` blocks on it).
pub struct Running {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
    accept: std::thread::JoinHandle<()>,
}

impl Running {
    pub fn wait(self) {
        let _ = self.accept.join();
    }
    /// Stop accepting (tests).
    pub fn stop(self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        self.shared.changed.notify_all();
        let _ = self.accept.join();
    }
}

pub fn start(vault: Vault, opts: ServeOpts) -> Result<Running, String> {
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
    let watch = opts.watch;
    let shared = Arc::new(Shared {
        vault: Mutex::new(vault),
        opts,
        allowed,
        state: Mutex::new(State::default()),
        changed: Condvar::new(),
        graphs: Mutex::new(VecDeque::new()),
        layouts: Mutex::new(VecDeque::new()),
        conns: AtomicUsize::new(0),
        streams: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
    });
    if !watch.is_zero() {
        let w = shared.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(watch);
            if w.stop.load(Ordering::SeqCst) {
                break;
            }
            let r = lock(&w.vault).refresh();
            w.bump(r.changed);
        });
    }
    let sh = shared.clone();
    let accept = std::thread::spawn(move || {
        for s in listener.incoming() {
            if sh.stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(s) = s else { continue };
            let sh = sh.clone();
            if sh.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                sh.conns.fetch_sub(1, Ordering::SeqCst);
                let mut s = s;
                let _ = respond(&mut s, 503, "application/json", br#"{"error":"busy"}"#, &[]);
                continue;
            }
            std::thread::spawn(move || {
                handle(&sh, s);
                sh.conns.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    Ok(Running {
        addr,
        shared,
        accept,
    })
}

pub fn serve(vault: Vault, opts: ServeOpts) -> Result<(), String> {
    let ro = opts.read_only;
    let r = start(vault, opts)?;
    eprintln!(
        "[Perennos Tempo Vault on http://{}/ {}]",
        r.addr,
        if ro { "(read-only)" } else { "" }
    );
    r.wait();
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
    fn flag(&self, k: &str, default: bool) -> bool {
        match self.q(k) {
            "1" | "true" | "yes" => true,
            "0" | "false" | "no" => false,
            _ => default,
        }
    }
}

fn form_decode(s: &str) -> String {
    crate::util::pct_decode(&s.replace('+', " "))
}

fn read_request(s: &TcpStream) -> Result<Req, (u16, &'static str)> {
    let bad = (400, "bad request");
    let clone = s.try_clone().map_err(|_| bad)?;
    let mut rd = BufReader::new(clone.take((MAX_HEAD + MAX_UPLOAD) as u64));
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
            (form_decode(k), form_decode(v))
        })
        .collect();
    let mut req = Req {
        method: method.to_string(),
        path: crate::util::pct_decode(path),
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
    if len
        > if req.path == UPLOAD_PATH {
            MAX_UPLOAD
        } else {
            MAX_BODY
        }
    {
        return Err((413, "body too large"));
    }
    req.body = vec![0; len];
    rd.read_exact(&mut req.body).map_err(|_| bad)?;
    Ok(req)
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

const APP_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob: https:; media-src 'self'; connect-src 'self'; object-src 'none'; frame-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
const FILE_CSP: &str = "default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'; sandbox";

fn respond(
    s: &mut TcpStream,
    code: u16,
    ctype: &str,
    body: &[u8],
    extra: &[(&str, String)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n",
        reason(code),
        body.len()
    );
    if code == 405 {
        head.push_str("Allow: GET, POST\r\n");
    }
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    s.flush()
}

/// The Tempo panel's calls, passed to the agent on this computer. Only for the person at this
/// computer, in this app: the vault must be served on loopback (not read-only), the request must
/// come from loopback under a loopback Host name, and nothing may say it came through a proxy.
/// Only the fields each call needs are passed on.
fn ask_proxy(sh: &Arc<Shared>, s: &mut TcpStream, req: &Req, what: &str) {
    let peer_local = s.peer_addr().is_ok_and(|a| a.ip().is_loopback());
    let host_local = req.header("host").is_some_and(|h| {
        let name = h.rsplit_once(':').map_or(h, |(n, p)| {
            if p.chars().all(|c| c.is_ascii_digit()) {
                n
            } else {
                h
            }
        });
        matches!(name, "127.0.0.1" | "localhost" | "[::1]")
    });
    let proxied = [
        "x-forwarded-for",
        "forwarded",
        "x-real-ip",
        "x-forwarded-host",
        "via",
    ]
    .iter()
    .any(|h| req.header(h).is_some());
    if !sh.opts.bind.is_loopback() || !peer_local || !host_local || proxied {
        return refuse(
            s,
            403,
            "Tempo can be asked from the vault only on the computer it runs on",
        );
    }
    if sh.opts.read_only {
        return refuse(s, 403, "this vault is served read-only");
    }
    let body: Value = if req.method == "POST" {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return refuse(s, 400, "body is not JSON"),
        }
    } else {
        Value::Null
    };
    let conv = |v: &str| crate::ask::conv_ok(v).then(|| v.to_string());
    let call = match (req.method.as_str(), what) {
        ("GET", "health") => Some(("GET", "/agent/health".to_string(), None)),
        ("POST", "new") => Some(("POST", "/agent/new".to_string(), Some(json!({})))),
        ("POST", "send") => {
            let text: String = body["text"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(20_000)
                .collect();
            conv(body_str(&body, "conv"))
                .filter(|_| !text.trim().is_empty())
                .map(|c| {
                    (
                        "POST",
                        "/agent/send".to_string(),
                        Some(json!({"conv": c, "text": text})),
                    )
                })
        }
        ("GET", "events") => conv(req.q("conv")).map(|c| {
            let since: usize = req.q("since").parse().unwrap_or(0);
            ("GET", format!("/agent/events?conv={c}&since={since}"), None)
        }),
        ("POST", "confirm") => conv(body_str(&body, "conv"))
            .filter(|_| crate::ask::conv_ok(body_str(&body, "id")))
            .map(|c| {
                (
                    "POST",
                    "/agent/confirm".to_string(),
                    Some(json!({"conv": c, "id": body_str(&body, "id"), "ok": body["ok"] == true})),
                )
            }),
        _ => None,
    };
    let Some((method, path, payload)) = call else {
        return refuse(s, 400, "not something the Tempo panel asks");
    };
    // The agent's key comes from the page (which got it from Perennos Tempo, `#key=`), never from
    // this server: reading <agent data>/control.key here would let any program on this computer
    // talk to the agent through the vault without it.
    let key = req
        .header("x-tempo-key")
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.len() <= 256 && k.chars().all(|c| c.is_ascii_hexdigit()));
    if key.is_none() && what != "health" {
        return send_json(
            s,
            401,
            &json!({"error": "Tempo's key is missing", "hint": "open Tempo from Perennos Tempo to ask"}),
        );
    }
    match crate::ask::call(
        sh.opts.agent_url.as_deref(),
        method,
        &path,
        payload.as_ref(),
        key,
        Duration::from_secs(15),
    ) {
        Ok((code, v)) => send_json(s, if code == 0 { 502 } else { code }, &v),
        Err(e) => send_json(s, 502, &json!({"error": e, "offline": true})),
    }
}

fn send_json(s: &mut TcpStream, code: u16, v: &Value) {
    let _ = respond(s, code, "application/json", v.to_string().as_bytes(), &[]);
}

fn refuse(s: &mut TcpStream, code: u16, why: &str) {
    send_json(s, code, &json!({ "error": why }));
}

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
        if !o
            .strip_prefix("http://")
            .or_else(|| o.strip_prefix("https://"))
            .is_some_and(|h| sh.allowed.contains(h))
        {
            return Err((403, "cross-origin request refused"));
        }
    }
    if req.header("sec-fetch-site") == Some("cross-site") {
        return Err((403, "cross-site request refused"));
    }
    if req.method != "GET" && req.method != "POST" {
        return Err((405, "only GET and POST"));
    }
    // JSON everywhere; raw bytes only for an upload. Neither is a type a page on another site can
    // send without a preflight, which is refused.
    let ctype = req
        .header("content-type")
        .unwrap_or("")
        .to_ascii_lowercase();
    let ok_type = ctype.starts_with("application/json")
        || (req.path == UPLOAD_PATH && ctype.starts_with("application/octet-stream"));
    if req.method == "POST" && !ok_type {
        return Err((415, "POST bodies must be application/json"));
    }
    Ok(())
}

fn handle(sh: &Arc<Shared>, mut s: TcpStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(60)));
    let req = match read_request(&s) {
        Ok(r) => r,
        Err((code, why)) => return refuse(&mut s, code, why),
    };
    if let Err((code, why)) = check(sh, &req) {
        return refuse(&mut s, code, why);
    }
    if let Some(api) = req.path.strip_prefix("/vault/") {
        if let Some(what) = api.strip_prefix("agent/") {
            return ask_proxy(sh, &mut s, &req, what);
        }
        if req.method == "GET" && api == "events" {
            let since = req.q("since").parse().unwrap_or_else(|_| sh.gen());
            return events(sh, s, since);
        }
        if req.method == "GET" && api == "file" {
            return file(sh, &mut s, &req);
        }
        if req.method == "POST" && api == "upload" {
            let r = upload(sh, &req);
            return match r {
                Ok(v) => send_json(&mut s, 200, &v),
                Err((code, v)) => send_json(&mut s, code, &v),
            };
        }
        let body: Value = if req.method == "POST" {
            match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(_) => return refuse(&mut s, 400, "body is not JSON"),
            }
        } else {
            Value::Null
        };
        match route(sh, &req, api, &body) {
            Ok(v) => send_json(&mut s, 200, &v),
            Err((code, v)) => send_json(&mut s, code, &v),
        }
        return;
    }
    if req.method != "GET" {
        return refuse(&mut s, 405, "only GET here");
    }
    let csp = [("Content-Security-Policy", APP_CSP.to_string())];
    let r = match req.path.as_str() {
        "/" | "/index.html" => respond(
            &mut s,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
            &csp,
        ),
        "/app.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            APP_JS.as_bytes(),
            &[],
        ),
        "/app.css" => respond(
            &mut s,
            200,
            "text/css; charset=utf-8",
            APP_CSS.as_bytes(),
            &[],
        ),
        "/memgraph.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            MEMGRAPH_JS.as_bytes(),
            &[],
        ),
        "/canvas.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            CANVAS_JS.as_bytes(),
            &[],
        ),
        "/vendor/three.module.min.js" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            THREE_JS.as_bytes(),
            &[],
        ),
        "/vendor/three-post.js" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            THREE_POST_JS.as_bytes(),
            &[],
        ),
        "/brain-world.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            BRAIN_WORLD_JS.as_bytes(),
            &[],
        ),
        "/brain-sim.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            BRAIN_SIM_JS.as_bytes(),
            &[],
        ),
        "/brain-feed.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            BRAIN_FEED_JS.as_bytes(),
            &[],
        ),
        "/vendor/three.LICENSE" => respond(
            &mut s,
            200,
            "text/plain; charset=utf-8",
            THREE_LICENSE.as_bytes(),
            &[],
        ),
        "/tempo-theme.css" => respond(
            &mut s,
            200,
            "text/css; charset=utf-8",
            THEME_CSS.as_bytes(),
            &[],
        ),
        p if BRAND_FILES.iter().any(|f| f.0 == p) => {
            let f = BRAND_FILES.iter().find(|f| f.0 == p).expect("listed");
            respond(&mut s, 200, f.1, f.2, &[])
        }
        "/icon.svg" | "/favicon.ico" => {
            respond(&mut s, 200, "image/svg+xml", ICON_SVG.as_bytes(), &[])
        }
        _ => {
            refuse(&mut s, 404, "not found");
            Ok(())
        }
    };
    let _ = r;
}

type Fail = (u16, Value);

fn fail(code: u16, why: impl Into<String>) -> Fail {
    (code, json!({ "error": why.into() }))
}

fn werr(e: WriteError) -> Fail {
    match e {
        WriteError::Conflict(mtime, text) => (
            409,
            json!({"error": "the note changed on disk since you opened it", "conflict": true, "mtime": mtime, "text": text}),
        ),
        WriteError::BadPath(p) => fail(400, format!("not a usable vault path: {p}")),
        WriteError::NotFound(p) => fail(404, format!("not found: {p}")),
        WriteError::Exists(p) => fail(409, format!("already exists: {p}")),
        WriteError::Invalid(m) => fail(400, m),
        WriteError::Io(m) => fail(500, m),
    }
}

fn path_arg(v: &str) -> Result<String, Fail> {
    clean_rel(v).ok_or_else(|| fail(400, "missing or unusable path"))
}

fn body_str<'a>(b: &'a Value, k: &str) -> &'a str {
    b[k].as_str().unwrap_or("")
}

fn history_entry(id: &str) -> Value {
    // <yyyymmdd>T<hhmmss><ms>Z[-n].md
    let t = id.get(..8).zip(id.get(9..15)).map(|(d, h)| {
        format!(
            "{}-{}-{} {}:{}:{} UTC",
            &d[..4],
            &d[4..6],
            &d[6..8],
            &h[..2],
            &h[2..4],
            &h[4..6]
        )
    });
    json!({"id": id, "time": t})
}

fn backlinks_json(v: &Vault, p: &str) -> Vec<Value> {
    let mut by_src: Vec<(String, Vec<Value>)> = vec![];
    for b in v.backlinks(p) {
        let ctx = v
            .read(&b.source)
            .and_then(|t| {
                t.lines()
                    .nth(b.line.saturating_sub(1))
                    .map(|l| crate::util::head_chars(l.trim(), 240).to_string())
            })
            .unwrap_or_default();
        let item = json!({"line": b.line, "embed": b.embed, "text": ctx});
        match by_src.iter_mut().find(|(s, _)| *s == b.source) {
            Some((_, list)) => list.push(item),
            None => by_src.push((b.source.clone(), vec![item])),
        }
    }
    by_src.into_iter().map(|(s, lines)| json!({"source": s, "title": v.note(&s).map(|n| n.title.clone()), "lines": lines})).collect()
}

/// A `.base` or `.canvas` file in the shape of a note, so the app opens it the same way.
fn doc_json(v: &Vault, p: &str) -> Result<Value, Fail> {
    let st = v
        .file_stat(p)
        .ok_or_else(|| fail(404, format!("no file {p}")))?;
    let text = v
        .read_text(p)
        .ok_or_else(|| fail(415, format!("{p} is not a text file this app can open")))?;
    Ok(json!({
        "path": p,
        "kind": render::ext(p),
        "title": crate::util::file_name(p).rsplit_once('.').map_or(p, |(n, _)| n),
        "text": text,
        "mtime": st.mtime,
        "size": st.size,
        "props": [],
        "aliases": [],
        "tags": [],
        "headings": [],
        "tasks": [],
        "links": [],
        "backlinks": backlinks_json(v, p),
        "words": 0,
        "history": v.history(p).len(),
    }))
}

fn note_json(v: &Vault, p: &str) -> Result<Value, Fail> {
    if v.note(p).is_none() && crate::write::is_doc(p) {
        return doc_json(v, p);
    }
    let n = v.note(p).ok_or_else(|| fail(404, format!("no note {p}")))?;
    let text = v.read(p).unwrap_or_default();
    let links: Vec<Value> = v
        .links_of(p)
        .into_iter()
        .map(|(l, r)| json!({"target": l.target, "heading": l.heading, "block": l.block, "alias": l.alias, "embed": l.embed, "wiki": l.wiki, "line": l.line, "to": r}))
        .collect();
    let backlinks = backlinks_json(v, p);
    Ok(json!({
        "path": p,
        "title": n.title,
        "text": text,
        "mtime": n.mtime,
        "size": n.size,
        "props": n.parsed.props.iter().map(|(k, val)| json!({"key": k, "value": val.to_json()})).collect::<Vec<_>>(),
        "aliases": n.parsed.aliases,
        "tags": n.parsed.tags,
        "headings": n.parsed.headings.iter().map(|h| json!({"level": h.level, "text": h.text, "line": h.line})).collect::<Vec<_>>(),
        "tasks": n.parsed.tasks.iter().map(|t| json!({"line": t.line, "status": t.status.to_string(), "done": t.done, "text": t.text, "due": t.due})).collect::<Vec<_>>(),
        "links": links,
        "backlinks": backlinks,
        "words": n.parsed.words,
        "history": v.history(p).len(),
    }))
}

fn graph_json(sh: &Shared, req: &Req) -> Result<Arc<String>, Fail> {
    let mut keys: Vec<(&String, &String)> = req.query.iter().collect();
    keys.sort();
    let key = format!("{}|{keys:?}", sh.gen());
    if let Some((_, g)) = lock(&sh.graphs).iter().find(|(k, _)| *k == key) {
        return Ok(g.clone());
    }
    let center = Some(req.q("center"))
        .filter(|c| !c.is_empty())
        .map(path_arg)
        .transpose()?;
    let o = GraphOpts {
        depth: req.q("depth").parse().unwrap_or(2).clamp(1, 6),
        tags: req.flag("tags", false),
        attachments: req.flag("attachments", true),
        unresolved: req.flag("unresolved", false),
        orphans: req.flag("orphans", true),
        filter: Some(req.q("filter").to_string()).filter(|f| !f.trim().is_empty()),
        max_nodes: req.q("max").parse().unwrap_or(5_000).clamp(10, 50_000),
        layout: false,
        center,
        ..GraphOpts::default()
    };
    // build under the lock, lay out without it (the layout is the slow part)
    let mut g = graph::build(&lock(&sh.vault), &o, unix_now());
    let shape = graph::signature(&g);
    let known = lock(&sh.layouts)
        .iter()
        .find(|(k, _, _)| *k == shape)
        .map(|(_, a, b)| (a.clone(), b.clone()));
    match known {
        Some((p3, p2)) => {
            g["pos3"] = p3;
            g["pos2"] = p2;
        }
        None => {
            graph::add_layout(&mut g);
            let mut l = lock(&sh.layouts);
            l.push_back((shape, g["pos3"].clone(), g["pos2"].clone()));
            while l.len() > 6 {
                l.pop_front();
            }
        }
    }
    let g = Arc::new(g.to_string());
    let mut cache = lock(&sh.graphs);
    cache.push_back((key, g.clone()));
    while cache.len() > 8 {
        cache.pop_front();
    }
    Ok(g)
}

fn route(sh: &Arc<Shared>, req: &Req, api: &str, b: &Value) -> Result<Value, Fail> {
    let post = req.method == "POST";
    if post && sh.opts.read_only && api != "preview" {
        return Err(fail(403, "this vault is served read-only"));
    }
    let origin = "app";
    match (post, api) {
        (false, "health") => {
            let v = lock(&sh.vault);
            let name = v
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("vault")
                .to_string();
            Ok(
                json!({"ok": true, "app": "tempo-vault", "version": env!("CARGO_PKG_VERSION"), "name": name, "notes": v.notes().count(), "attachments": v.files().count(), "read_only": sh.opts.read_only, "gen": sh.gen()}),
            )
        }
        (false, "tree") => {
            let v = lock(&sh.vault);
            let notes: Vec<Value> = v
                .notes()
                .map(|n| json!({"path": n.path, "title": n.title, "mtime": n.mtime, "size": n.size, "tags": n.parsed.tags, "words": n.parsed.words, "open_tasks": n.parsed.tasks.iter().filter(|t| !t.done).count()}))
                .collect();
            let files: Vec<Value> = v
                .files()
                .map(|(p, st)| json!({"path": p, "mtime": st.mtime, "size": st.size}))
                .collect();
            Ok(json!({"gen": sh.gen(), "notes": notes, "files": files}))
        }
        (false, "note") => note_json(&lock(&sh.vault), &path_arg(req.q("path"))?),
        (false, "render") => {
            let p = path_arg(req.q("path"))?;
            let v = lock(&sh.vault);
            let html = if v.note(&p).is_none() && render::ext(&p) == "base" {
                render::base_file_html(&v, &p, &Opts::app())
            } else if v.note(&p).is_none() && render::ext(&p) == "canvas" {
                render::canvas_file_html(&v, &p, &Opts::app())
            } else {
                render::note_html(&v, &p, &Opts::app())
            }
            .ok_or_else(|| fail(404, format!("no note {p}")))?;
            Ok(json!({"path": p, "html": html}))
        }
        (false, "canvas") => {
            // the canvas as the editor needs it: the file's own JSON (kept as written), its
            // mtime, and every text and note card rendered
            let p = path_arg(req.q("path"))?;
            let v = lock(&sh.vault);
            let st = *v
                .file_stat(&p)
                .filter(|_| render::ext(&p) == "canvas")
                .ok_or_else(|| fail(404, format!("no canvas {p}")))?;
            let text = v
                .read_text(&p)
                .ok_or_else(|| fail(415, format!("{p} cannot be read as text")))?;
            let (cards, problem) = match crate::canvas::parse(&text) {
                Ok(c) => (render::canvas_cards_html(&v, &p, &c, &Opts::app()), None),
                Err(e) => (vec![], Some(e)),
            };
            let files: serde_json::Map<String, Value> = v
                .files()
                .filter(|(f, _)| render::IMAGE.contains(&render::ext(f).as_str()))
                .map(|(f, _)| (f.clone(), json!(true)))
                .collect();
            Ok(json!({
                "path": p,
                "mtime": st.mtime,
                "text": text,
                "problem": problem,
                "html": cards.into_iter().map(|(k, h)| (k, Value::String(h))).collect::<serde_json::Map<String, Value>>(),
                "images": files.keys().collect::<Vec<_>>(),
            }))
        }
        (true, "preview") => {
            let p = clean_rel(body_str(b, "path")).unwrap_or_else(|| "Untitled.md".into());
            let v = lock(&sh.vault);
            Ok(
                json!({"html": render::markdown_html(Some(&v), &p, body_str(b, "text"), &Opts::app())}),
            )
        }
        (false, "search") => {
            let limit = req.q("limit").parse().unwrap_or(50).clamp(1, 500);
            let hits = lock(&sh.vault).search(req.q("q"), limit);
            Ok(
                json!({"q": req.q("q"), "hits": hits.iter().map(|h| json!({"path": h.path, "title": h.title, "score": h.score, "line": h.line, "snippet": h.snippet, "ranges": h.ranges})).collect::<Vec<_>>()}),
            )
        }
        (false, "suggest") => {
            let limit = req.q("limit").parse().unwrap_or(20).clamp(1, 200);
            let s = lock(&sh.vault).suggest(req.q("q"), limit);
            Ok(
                json!({"items": s.iter().map(|(p, t, sc)| json!({"path": p, "title": t, "score": sc})).collect::<Vec<_>>()}),
            )
        }
        (false, "graph") => {
            let g = graph_json(sh, req)?;
            serde_json::from_str(&g).map_err(|e| fail(500, e.to_string()))
        }
        (false, "tags") => {
            let t = lock(&sh.vault).tags();
            Ok(
                json!({"tags": t.iter().map(|(k, n)| json!({"tag": k, "count": n})).collect::<Vec<_>>()}),
            )
        }
        (false, "tasks") => {
            let v = lock(&sh.vault);
            let today = crate::util::date_of(
                unix_now(),
                crate::util::local_offset_minutes(
                    crate::write::VaultConfig::load(&v.root)
                        .utc_offset
                        .as_deref(),
                ),
            );
            let rules = if req.q("rules").is_empty() {
                "not done"
            } else {
                req.q("rules")
            };
            let found = render::task_query(&v, rules, &today);
            Ok(
                json!({"today": today, "tasks": found.iter().map(|(p, t)| json!({"path": p, "line": t.line, "done": t.done, "status": t.status.to_string(), "text": t.text, "due": t.due})).collect::<Vec<_>>()}),
            )
        }
        (false, "mentions") => {
            let p = path_arg(req.q("path"))?;
            let m = lock(&sh.vault).mentions(&p);
            Ok(
                json!({"mentions": m.iter().map(|(s, l, t)| json!({"source": s, "line": l, "text": t})).collect::<Vec<_>>()}),
            )
        }
        (false, "history") => {
            let p = path_arg(req.q("path"))?;
            let ids = lock(&sh.vault).history(&p);
            Ok(
                json!({"path": p, "versions": ids.iter().map(|i| history_entry(i)).collect::<Vec<_>>()}),
            )
        }
        (false, "history/text") => {
            let p = path_arg(req.q("path"))?;
            let t = lock(&sh.vault)
                .history_text(&p, req.q("id"))
                .ok_or_else(|| fail(404, "no such version"))?;
            Ok(json!({"path": p, "id": req.q("id"), "text": t}))
        }
        (false, "stats") => Ok(lock(&sh.vault).stats()),
        (false, "unresolved") => {
            let u = lock(&sh.vault).unresolved();
            let mut by: Vec<(String, Vec<Value>)> = vec![];
            for (src, target, line) in u {
                let key = target.to_lowercase();
                let item = json!({"source": src, "line": line});
                match by.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, l)) => l.push(item),
                    None => by.push((key, vec![item])),
                }
            }
            by.sort_by_key(|(_, l)| std::cmp::Reverse(l.len()));
            Ok(
                json!({"unresolved": by.into_iter().map(|(t, l)| json!({"target": t, "count": l.len(), "from": l})).collect::<Vec<_>>()}),
            )
        }
        (false, "orphans") => Ok(json!({"orphans": lock(&sh.vault).orphans()})),
        (false, "templates") => {
            let v = lock(&sh.vault);
            let folder = crate::write::VaultConfig::load(&v.root).templates_folder;
            let prefix = format!("{}/", folder.trim_matches('/'));
            Ok(
                json!({"folder": folder, "templates": v.notes().filter(|n| n.path.starts_with(&prefix)).map(|n| n.path.clone()).collect::<Vec<_>>()}),
            )
        }
        (false, "changes") => {
            let v = lock(&sh.vault);
            let limit: usize = req.q("limit").parse().unwrap_or(50).clamp(1, 500);
            let text = v
                .safe_abs(".tempo/changes.jsonl")
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            let lines: Vec<Value> = text
                .lines()
                .rev()
                .take(limit)
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            Ok(json!({"changes": lines}))
        }
        (false, "activity") => {
            // edits per day over the last year and a bit: every change the vault logged (the app,
            // the command line, Tempo), and each note's last edit (Obsidian and other editors)
            let v = lock(&sh.vault);
            let off = crate::util::local_offset_minutes(
                crate::write::VaultConfig::load(&v.root)
                    .utc_offset
                    .as_deref(),
            );
            let today = crate::util::date_of(crate::util::unix_now(), off);
            let from = crate::util::date_of(crate::util::unix_now() - 400.0 * 86_400.0, off);
            let mut days: std::collections::BTreeMap<String, u32> = Default::default();
            let text = v
                .safe_abs(".tempo/changes.jsonl")
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            for l in text.lines() {
                if let Some(d) = serde_json::from_str::<Value>(l).ok().and_then(|r| {
                    r["t"]
                        .as_str()
                        .map(|t| t.get(..10).unwrap_or("").to_string())
                }) {
                    if d.len() == 10 && d >= from {
                        *days.entry(d).or_insert(0) += 1;
                    }
                }
            }
            for n in v.notes() {
                let d = crate::util::date_of(n.mtime, off);
                if d >= from {
                    *days.entry(d).or_insert(0) += 1;
                }
            }
            Ok(json!({"today": today, "days": days}))
        }
        (false, "learn") => {
            let v = lock(&sh.vault);
            let st = v
                .safe_abs(".tempo/learn.json")
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .unwrap_or(json!({}));
            Ok(
                json!({"last_run": st["last_run"], "notes": st["notes_written"], "conversations": st["conversations"].as_object().map(|o| o.len())}),
            )
        }
        (true, "write") => {
            let p = path_arg(body_str(b, "path"))?;
            let m = lock(&sh.vault)
                .write(&p, body_str(b, "text"), b["mtime"].as_f64(), origin)
                .map_err(werr)?;
            sh.bump(vec![p.clone()]);
            Ok(json!({"path": p, "mtime": m}))
        }
        (true, "create") => {
            let p = body_str(b, "path");
            let mut v = lock(&sh.vault);
            let made = match b["template"].as_str().filter(|t| !t.is_empty()) {
                Some(t) => v.from_template(p, t, origin),
                None => v.create(p, body_str(b, "text"), origin),
            }
            .map_err(werr)?;
            drop(v);
            sh.bump(vec![made.clone()]);
            Ok(json!({"path": made}))
        }
        (true, "append") => {
            let heading = b["heading"].as_str().filter(|h| !h.trim().is_empty());
            let p = lock(&sh.vault)
                .append(body_str(b, "path"), body_str(b, "text"), heading, origin)
                .map_err(werr)?;
            sh.bump(vec![p.clone()]);
            Ok(json!({"path": p}))
        }
        (true, "prop") => {
            let p = path_arg(body_str(b, "path"))?;
            lock(&sh.vault)
                .set_prop(
                    &p,
                    body_str(b, "key"),
                    &Prop::from_json(&b["value"]),
                    origin,
                )
                .map_err(werr)?;
            sh.bump(vec![p.clone()]);
            Ok(json!({"path": p}))
        }
        (true, "task") => {
            let p = path_arg(body_str(b, "path"))?;
            let line = b["line"]
                .as_u64()
                .ok_or_else(|| fail(400, "line is required"))? as usize;
            lock(&sh.vault)
                .set_task(&p, line, b["done"].as_bool().unwrap_or(true), origin)
                .map_err(werr)?;
            sh.bump(vec![p.clone()]);
            Ok(json!({"path": p}))
        }
        (true, "daily") => {
            let p = lock(&sh.vault)
                .daily(b["date"].as_str().filter(|d| !d.is_empty()), origin)
                .map_err(werr)?;
            sh.bump(vec![p.clone()]);
            Ok(json!({"path": p}))
        }
        (true, "rename") => {
            let (from, to) = (body_str(b, "from"), body_str(b, "to"));
            let mut v = lock(&sh.vault);
            let changed = v.rename(from, to, origin).map_err(werr)?;
            let to_clean = if v.note(&clean_rel(to).unwrap_or_default()).is_some()
                || v.has_file(&clean_rel(to).unwrap_or_default())
            {
                clean_rel(to).unwrap_or_default()
            } else {
                format!("{}.md", clean_rel(to).unwrap_or_default())
            };
            drop(v);
            let mut paths = changed.clone();
            paths.push(clean_rel(from).unwrap_or_default());
            paths.push(to_clean.clone());
            sh.bump(paths);
            Ok(json!({"path": to_clean, "changed": changed}))
        }
        (true, "trash") => {
            let p = path_arg(body_str(b, "path"))?;
            let to = lock(&sh.vault).trash(&p, origin).map_err(werr)?;
            sh.bump(vec![p.clone()]);
            Ok(json!({"path": p, "to": to}))
        }
        (true, "restore") => {
            let p = path_arg(body_str(b, "path"))?;
            lock(&sh.vault)
                .restore(&p, body_str(b, "id"), origin)
                .map_err(werr)?;
            sh.bump(vec![p.clone()]);
            Ok(json!({"path": p}))
        }
        (true, "refresh") => {
            let r = lock(&sh.vault).refresh();
            let n = r.changed.len();
            sh.bump(r.changed);
            Ok(json!({"changed": n}))
        }
        (false, _) | (true, _) if is_known(api) => Err(fail(405, "wrong method for this endpoint")),
        _ => Err(fail(404, "no such endpoint")),
    }
}

fn is_known(api: &str) -> bool {
    matches!(
        api,
        "health"
            | "tree"
            | "note"
            | "render"
            | "preview"
            | "search"
            | "suggest"
            | "graph"
            | "tags"
            | "tasks"
            | "mentions"
            | "history"
            | "history/text"
            | "stats"
            | "unresolved"
            | "orphans"
            | "templates"
            | "changes"
            | "learn"
            | "write"
            | "create"
            | "append"
            | "prop"
            | "task"
            | "daily"
            | "rename"
            | "trash"
            | "restore"
            | "refresh"
            | "events"
            | "file"
            | "canvas"
            | "activity"
    )
}

/// A pasted or dropped attachment: `?name=<file name>&note=<the note it goes with>`, raw body.
fn upload(sh: &Arc<Shared>, req: &Req) -> Result<Value, Fail> {
    if sh.opts.read_only {
        return Err(fail(403, "this vault is served read-only"));
    }
    if req.body.is_empty() {
        return Err(fail(400, "empty file"));
    }
    let note = clean_rel(req.q("note"));
    let mut v = lock(&sh.vault);
    let folder = v.attachment_folder(note.as_deref());
    let rel = v
        .add_file(&folder, req.q("name"), &req.body, "app")
        .map_err(werr)?;
    let name = file_name(&rel).to_string();
    // a bare name when it is unique in the vault, else the path (as Obsidian writes it)
    let unique = v
        .files()
        .filter(|(p, _)| file_name(p).eq_ignore_ascii_case(&name))
        .count()
        <= 1;
    drop(v);
    sh.bump(vec![rel.clone()]);
    Ok(json!({"path": rel, "embed": format!("![[{}]]", if unique { name } else { rel.clone() })}))
}

/// Server-sent events: `change` with the new generation and the paths that changed.
fn events(sh: &Arc<Shared>, mut s: TcpStream, since: u64) {
    if sh.streams.fetch_add(1, Ordering::SeqCst) >= MAX_STREAMS {
        sh.streams.fetch_sub(1, Ordering::SeqCst);
        return refuse(&mut s, 503, "too many streams");
    }
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\nretry: 2000\n\n";
    let mut ok = s.write_all(head.as_bytes()).and_then(|_| s.flush()).is_ok();
    let mut next = since;
    while ok && !sh.stop.load(Ordering::SeqCst) {
        let (gen, paths) = {
            let mut st = lock(&sh.state);
            let deadline = Instant::now() + Duration::from_secs(15);
            while st.gen <= next && !sh.stop.load(Ordering::SeqCst) {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                st = sh
                    .changed
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            let mut paths: Vec<String> = st
                .recent
                .iter()
                .filter(|(g, _)| *g > next)
                .map(|(_, p)| p.clone())
                .collect();
            paths.sort();
            paths.dedup();
            (st.gen, paths)
        };
        let out = if gen > next {
            next = gen;
            format!(
                "id: {gen}\nevent: change\ndata: {}\n\n",
                json!({"gen": gen, "paths": paths})
            )
        } else {
            ": keep-alive\n\n".to_string()
        };
        ok = s.write_all(out.as_bytes()).and_then(|_| s.flush()).is_ok();
    }
    sh.streams.fetch_sub(1, Ordering::SeqCst);
}

pub fn content_type(rel: &str) -> (&'static str, bool) {
    // (type, shown inline)
    match render::ext(rel).as_str() {
        "png" => ("image/png", true),
        "jpg" | "jpeg" => ("image/jpeg", true),
        "gif" => ("image/gif", true),
        "webp" => ("image/webp", true),
        "svg" => ("image/svg+xml", true),
        "bmp" => ("image/bmp", true),
        "avif" => ("image/avif", true),
        "ico" => ("image/x-icon", true),
        "mp3" => ("audio/mpeg", true),
        "wav" => ("audio/wav", true),
        "m4a" => ("audio/mp4", true),
        "ogg" | "opus" => ("audio/ogg", true),
        "flac" => ("audio/flac", true),
        "aac" => ("audio/aac", true),
        "mp4" | "m4v" => ("video/mp4", true),
        "webm" => ("video/webm", true),
        "mov" => ("video/quicktime", true),
        "ogv" => ("video/ogg", true),
        "pdf" => ("application/pdf", true),
        "txt" | "csv" | "json" | "canvas" | "md" | "log" | "html" | "htm" | "xml" | "js"
        | "css" => ("text/plain; charset=utf-8", true),
        _ => ("application/octet-stream", false),
    }
}

/// An attachment's bytes (with single-range support so audio and video can seek).
fn file(sh: &Shared, s: &mut TcpStream, req: &Req) {
    let Some(rel) = clean_rel(req.q("path")) else {
        return refuse(s, 400, "missing or unusable path");
    };
    let abs = {
        let v = lock(&sh.vault);
        if !v.has_file(&rel) && v.note(&rel).is_none() {
            return refuse(s, 404, "not found");
        }
        match v.safe_abs(&rel) {
            Some(p) => p,
            None => return refuse(s, 404, "not found"),
        }
    };
    let Ok(mut f) = std::fs::File::open(&abs) else {
        return refuse(s, 404, "not found");
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let (ctype, inline) = content_type(&rel);
    let mut code = 200;
    let (mut start, mut end) = (0u64, len.saturating_sub(1));
    if let Some(r) = req.header("range").and_then(|r| r.strip_prefix("bytes=")) {
        let parsed = r
            .split_once('-')
            .and_then(|(a, b)| match (a.trim(), b.trim()) {
                ("", b) => b
                    .parse::<u64>()
                    .ok()
                    .map(|n| (len.saturating_sub(n), len.saturating_sub(1))),
                (a, "") => a.parse().ok().map(|a| (a, len.saturating_sub(1))),
                (a, b) => Some((
                    a.parse().ok()?,
                    b.parse::<u64>().ok()?.min(len.saturating_sub(1)),
                )),
            });
        match parsed {
            Some((a, b)) if a <= b && b < len => {
                start = a;
                end = b;
                code = 206;
            }
            _ => {
                let _ = respond(
                    s,
                    416,
                    "application/json",
                    br#"{"error":"bad range"}"#,
                    &[("Content-Range", format!("bytes */{len}"))],
                );
                return;
            }
        }
    }
    let count = if len == 0 { 0 } else { end - start + 1 };
    let disp = format!(
        "{}; filename=\"{}\"",
        if inline { "inline" } else { "attachment" },
        file_name(&rel).replace(['"', '\\', '\r', '\n'], "_")
    );
    let mut head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {count}\r\nAccept-Ranges: bytes\r\nCache-Control: private, max-age=60\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: {FILE_CSP}\r\nContent-Disposition: {disp}\r\nCross-Origin-Resource-Policy: same-origin\r\n",
        reason(code)
    );
    if code == 206 {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    if s.write_all(head.as_bytes()).is_err() || f.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    let _ = std::io::copy(&mut (&mut f).take(count), s);
    let _ = s.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::tests::{put, sample};

    fn http(addr: SocketAddr, raw: &str) -> (u16, String, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let code = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (code, head.to_string(), body.to_string())
    }

    fn get(addr: SocketAddr, path: &str) -> (u16, String, String) {
        http(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
        )
    }

    fn post(addr: SocketAddr, path: &str, body: &Value) -> (u16, Value) {
        let b = body.to_string();
        let (c, _, body) = http(addr, &format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len()));
        (c, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    fn json_of(r: (u16, String, String)) -> (u16, Value) {
        (r.0, serde_json::from_str(&r.2).unwrap_or(Value::Null))
    }

    fn opts(read_only: bool) -> ServeOpts {
        ServeOpts {
            port: 0,
            read_only,
            watch: Duration::from_millis(200),
            ..ServeOpts::default()
        }
    }

    /// A stand-in agent: answers every call with {"conv":"c1"} and keeps each request it got.
    fn fake_agent() -> (String, Arc<Mutex<Vec<String>>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let got = Arc::new(Mutex::new(vec![]));
        let g = got.clone();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut buf = [0u8; 8192];
                let n = c.read(&mut buf).unwrap_or(0);
                g.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let body = "{\"conv\":\"c1\"}";
                let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        (url, got)
    }

    #[test]
    fn the_tempo_tab_passes_the_pages_key_and_never_its_own() {
        let (url, got) = fake_agent();
        let (_d, v) = sample();
        let r = start(
            v,
            ServeOpts {
                agent_url: Some(url),
                ..opts(false)
            },
        )
        .unwrap();
        let a = r.addr;
        let post = |extra: &str| {
            http(a, &format!("POST /vault/agent/new HTTP/1.1\r\nHost: {a}\r\nContent-Type: application/json\r\n{extra}Content-Length: 2\r\n\r\n{{}}"))
        };
        // without the page's key: refused here, the agent never hears of it
        let (c, _, b) = post("");
        assert_eq!(c, 401, "{b}");
        assert!(b.contains("open Tempo from Perennos Tempo"), "{b}");
        assert!(got.lock().unwrap().is_empty());
        // with it: passed on as it came
        let (c, _, b) = post("X-Tempo-Key: 00ff00ff\r\n");
        assert_eq!(c, 200, "{b}");
        assert!(got.lock().unwrap()[0].contains("X-Tempo-Key: 00ff00ff\r\n"));
        // health needs no key (the panel shows whether Tempo is running)
        let (c, _, _) = http(
            a,
            &format!("GET /vault/agent/health HTTP/1.1\r\nHost: {a}\r\n\r\n"),
        );
        assert_eq!(c, 200);
        assert!(
            !got.lock().unwrap()[1].contains("X-Tempo-Key"),
            "no key of its own"
        );
        // only the key goes along, nothing else the page sent; a key that is not hex is refused
        let (c, _, _) = post("X-Tempo-Key: abc\r\nInjected: yes\r\n");
        assert_eq!(c, 200);
        assert!(!got.lock().unwrap()[2].contains("Injected"));
        let (c, _, _) = post("X-Tempo-Key: not-hex\r\n");
        assert_eq!(c, 401);
        assert_eq!(got.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_vault_behind_tailscale_serve_has_no_port_and_https_origin_is_ours() {
        let (_d, v) = sample();
        let r = start(
            v,
            ServeOpts {
                allow_hosts: vec!["tempo.local:443".into()],
                ..opts(false)
            },
        )
        .unwrap();
        let a = r.addr;
        let get = |extra: &str| http(a, &format!("GET /vault/health HTTP/1.1\r\n{extra}\r\n")).0;
        assert_eq!(
            get("Host: tempo.local\r\n"),
            200,
            "the bare name from the HTTPS front"
        );
        assert_eq!(
            get("Host: tempo.local\r\nOrigin: https://tempo.local\r\n"),
            200
        );
        assert_eq!(
            get("Host: tempo.local\r\nOrigin: https://evil.example\r\n"),
            403
        );
        assert_eq!(get("Host: evil.example\r\n"), 403);
    }

    #[test]
    fn the_tempo_panel_reaches_the_agent_only_from_this_computer() {
        let (_d, v) = sample();
        let r = start(v, opts(false)).unwrap();
        let a = r.addr;
        // through a proxy (as tailscale serve or nginx would add) or naming another host: refused
        for extra in [
            "X-Forwarded-For: 203.0.113.5\r\n",
            "Forwarded: for=203.0.113.5\r\n",
            "Via: 1.1 proxy\r\n",
        ] {
            let (c, _, b) = http(
                a,
                &format!("GET /vault/agent/health HTTP/1.1\r\nHost: {a}\r\n{extra}\r\n"),
            );
            assert_eq!(c, 403, "{extra}: {b}");
        }
        // only the five calls, with clean ids
        let (c, _, _) = http(
            a,
            &format!("GET /vault/agent/memory HTTP/1.1\r\nHost: {a}\r\n\r\n"),
        );
        assert_eq!(c, 400);
        let (c, _, _) = http(
            a,
            &format!("GET /vault/agent/events?conv=../x HTTP/1.1\r\nHost: {a}\r\n\r\n"),
        );
        assert_eq!(c, 400);
        // a read-only vault never relays
        let (_d2, v2) = sample();
        let ro = start(v2, opts(true)).unwrap();
        let b = ro.addr;
        let (c, _, _) = http(
            b,
            &format!("GET /vault/agent/health HTTP/1.1\r\nHost: {b}\r\n\r\n"),
        );
        assert_eq!(c, 403);
    }

    #[test]
    fn api_reads_writes_and_refuses() {
        let (d, v) = sample();
        let r = start(v, opts(false)).unwrap();
        let a = r.addr;
        let (c, h) = json_of(get(a, "/vault/health"));
        assert_eq!(
            (c, h["ok"].as_bool(), h["notes"].as_u64()),
            (200, Some(true), Some(5))
        );
        // the app shell carries a strict CSP
        let (c, head, body) = get(a, "/");
        assert_eq!(c, 200);
        assert!(
            head.contains("Content-Security-Policy: default-src 'self'; script-src 'self'")
                && body.contains("<html")
        );
        // security: wrong host, cross-site, wrong method or type
        assert_eq!(
            http(
                a,
                "GET /vault/health HTTP/1.1\r\nHost: evil.example\r\n\r\n"
            )
            .0,
            403
        );
        assert_eq!(
            http(
                a,
                &format!(
                    "GET /vault/tree HTTP/1.1\r\nHost: {a}\r\nOrigin: http://evil.example\r\n\r\n"
                )
            )
            .0,
            403
        );
        assert_eq!(
            http(
                a,
                &format!(
                    "GET /vault/tree HTTP/1.1\r\nHost: {a}\r\nSec-Fetch-Site: cross-site\r\n\r\n"
                )
            )
            .0,
            403
        );
        assert_eq!(
            http(
                a,
                &format!("OPTIONS /vault/write HTTP/1.1\r\nHost: {a}\r\n\r\n")
            )
            .0,
            405
        );
        assert_eq!(http(a, &format!("POST /vault/write HTTP/1.1\r\nHost: {a}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}")).0, 415);
        assert_eq!(get(a, "/vault/note?path=../etc/passwd").0, 400);
        assert_eq!(get(a, "/vault/file?path=.obsidian/app.json").0, 400);
        assert_eq!(get(a, "/vault/nope").0, 404);
        // reads
        let (_, n) = json_of(get(a, "/vault/note?path=Ideas.md"));
        assert_eq!(n["title"], "Ideas");
        let back: Vec<&str> = n["backlinks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["source"].as_str().unwrap())
            .collect();
        assert_eq!(back, ["Home.md", "Projects/Tempo.md"]);
        assert!(n["backlinks"][0]["lines"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[[Ideas]]"));
        let (_, html) = json_of(get(a, "/vault/render?path=Home.md"));
        assert!(html["html"]
            .as_str()
            .unwrap()
            .contains("data-path=\"Projects/Tempo.md\""));
        let (_, s) = json_of(get(a, "/vault/search?q=perennos+tempo"));
        assert!(s["hits"].as_array().unwrap().len() >= 2);
        let (_, q) = json_of(get(a, "/vault/suggest?q=tmp"));
        assert_eq!(q["items"][0]["path"], "Projects/Tempo.md");
        let (_, g) = json_of(get(a, "/vault/graph?tags=1"));
        assert_eq!(
            g["pos3"].as_array().unwrap().len(),
            g["nodes"].as_array().unwrap().len() * 3
        );
        let (_, t) = json_of(get(a, "/vault/tasks"));
        assert_eq!(t["tasks"].as_array().unwrap().len(), 1);
        // writes, conflicts and history
        let (c, w) = post(
            a,
            "/vault/write",
            &json!({"path": "Ideas.md", "text": "# Ideas\nnew", "mtime": n["mtime"]}),
        );
        assert_eq!(c, 200, "{w}");
        let (c, w2) = post(
            a,
            "/vault/write",
            &json!({"path": "Ideas.md", "text": "stale", "mtime": n["mtime"]}),
        );
        assert_eq!((c, w2["conflict"].as_bool()), (409, Some(true)));
        assert_eq!(w2["text"], "# Ideas\nnew");
        let (_, hist) = json_of(get(a, "/vault/history?path=Ideas.md"));
        let id = hist["versions"][0]["id"].as_str().unwrap().to_string();
        let (_, old) = json_of(get(
            a,
            &format!("/vault/history/text?path=Ideas.md&id={id}"),
        ));
        assert!(old["text"].as_str().unwrap().contains("## Next"));
        assert_eq!(
            post(
                a,
                "/vault/task",
                &json!({"path": "Projects/Tempo.md", "line": 7, "done": true})
            )
            .0,
            200
        );
        assert!(std::fs::read_to_string(d.join("Projects/Tempo.md"))
            .unwrap()
            .contains("- [x] ship vault"));
        let (c, made) = post(
            a,
            "/vault/create",
            &json!({"path": "Inbox/New idea", "text": "hello [[Home]]"}),
        );
        assert_eq!((c, made["path"].as_str()), (200, Some("Inbox/New idea.md")));
        let (c, ren) = post(
            a,
            "/vault/rename",
            &json!({"from": "Home.md", "to": "Start.md"}),
        );
        assert_eq!(c, 200, "{ren}");
        assert!(std::fs::read_to_string(d.join("Inbox/New idea.md"))
            .unwrap()
            .contains("[[Start]]"));
        let (c, tr) = post(a, "/vault/trash", &json!({"path": "Inbox/New idea.md"}));
        assert_eq!(c, 200);
        assert!(
            d.join(tr["to"].as_str().unwrap()).is_file(),
            "trash moves, never deletes"
        );
        let (c, day) = post(a, "/vault/daily", &json!({"date": "2026-11-06"}));
        assert_eq!(
            (c, day["path"].as_str()),
            (200, Some("Daily/2026-11-06.md"))
        );
        let (_, prev) = post(
            a,
            "/vault/preview",
            &json!({"path": "x.md", "text": "**b** [[Start]]"}),
        );
        assert!(prev["html"]
            .as_str()
            .unwrap()
            .contains("<strong>b</strong>"));
        // attachments: inline image with a sandbox CSP, ranges, never HTML
        put(&d, "img/page.html", "<script>alert(1)</script>");
        std::thread::sleep(Duration::from_millis(500));
        let (c, head, body) = get(a, "/vault/file?path=img/logo.png");
        assert_eq!((c, body.as_str()), (200, "PNG"));
        assert!(head.contains("Content-Type: image/png") && head.contains("sandbox"));
        let (c, head, _) = get(a, "/vault/file?path=img/page.html");
        assert_eq!(c, 200, "the watcher picked up the new file");
        assert!(head.contains("text/plain") && !head.contains("text/html"));
        let (c, head, body) = http(a, &format!("GET /vault/file?path=img/logo.png HTTP/1.1\r\nHost: {a}\r\nRange: bytes=1-\r\n\r\n"));
        assert_eq!((c, body.as_str()), (206, "NG"));
        assert!(head.contains("Content-Range: bytes 1-2/3"));
        assert_eq!(http(a, &format!("GET /vault/file?path=img/logo.png HTTP/1.1\r\nHost: {a}\r\nRange: bytes=9-10\r\n\r\n")).0, 416);
        r.stop();
    }

    #[test]
    fn uploads_take_raw_bytes_only_on_their_path() {
        let (d, v) = sample();
        let r = start(
            v,
            ServeOpts {
                watch: Duration::ZERO,
                ..opts(false)
            },
        )
        .unwrap();
        let a = r.addr;
        let up = |q: &str, ctype: &str, body: &[u8], origin: &str| {
            let mut s = TcpStream::connect(a).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            write!(s, "POST /vault/upload?{q} HTTP/1.1\r\nHost: {a}\r\nOrigin: {origin}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            s.write_all(body).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            let code: u16 = out
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            (
                code,
                serde_json::from_str::<Value>(out.split_once("\r\n\r\n").map_or("", |x| x.1))
                    .unwrap_or(Value::Null),
            )
        };
        let me = format!("http://{a}");
        let (c, j) = up(
            "name=Pasted%20image%2020261106.png&note=Projects/Tempo.md",
            "application/octet-stream",
            b"\x89PNG",
            &me,
        );
        assert_eq!(c, 200, "{j}");
        assert_eq!(
            (j["path"].as_str(), j["embed"].as_str()),
            (
                Some("Attachments/Pasted image 20261106.png"),
                Some("![[Pasted image 20261106.png]]")
            )
        );
        assert_eq!(
            std::fs::read(d.join("Attachments/Pasted image 20261106.png")).unwrap(),
            b"\x89PNG"
        );
        assert_eq!(
            up(
                "name=x.png",
                "application/octet-stream",
                b"x",
                "http://evil.example"
            )
            .0,
            403,
            "cross-origin refused"
        );
        assert_eq!(up("name=x.png", "text/plain", b"x", &me).0, 415);
        assert_eq!(http(a, &format!("POST /vault/write HTTP/1.1\r\nHost: {a}\r\nContent-Type: application/octet-stream\r\nContent-Length: 2\r\n\r\nhi")).0, 415, "raw bytes only for uploads");
        let big = vec![0u8; MAX_BODY + 10];
        assert_eq!(
            up("name=big.bin", "application/octet-stream", &big, &me).0,
            200,
            "uploads may be larger than JSON bodies"
        );
        r.stop();
    }

    #[test]
    fn graph_layout_survives_text_edits() {
        let (_d, v) = sample();
        let r = start(
            v,
            ServeOpts {
                watch: Duration::ZERO,
                ..opts(false)
            },
        )
        .unwrap();
        let a = r.addr;
        let g1 = json_of(get(a, "/vault/graph")).1;
        let (_, n) = json_of(get(a, "/vault/note?path=Ideas.md"));
        let same_links = "# Ideas\n## Next\nUse Perennos Tempo every day; it is great. [[Home]]\n- [x] think\n#project/ideas\n";
        assert_eq!(
            post(
                a,
                "/vault/write",
                &json!({"path": "Ideas.md", "text": same_links, "mtime": n["mtime"]})
            )
            .0,
            200
        );
        let g2 = json_of(get(a, "/vault/graph")).1;
        assert_eq!(g1["pos3"], g2["pos3"], "a text edit keeps the picture");
        assert_eq!(
            post(
                a,
                "/vault/write",
                &json!({"path": "Ideas.md", "text": format!("{same_links}[[Archive/Index]]")})
            )
            .0,
            200
        );
        let g3 = json_of(get(a, "/vault/graph")).1;
        assert_ne!(g2["pos3"], g3["pos3"], "a new link lays it out again");
        r.stop();
    }

    #[test]
    fn read_only_and_live_events() {
        let (d, v) = sample();
        let ro = start(v, opts(true)).unwrap();
        let (c, e) = post(
            ro.addr,
            "/vault/write",
            &json!({"path": "Ideas.md", "text": "x"}),
        );
        assert_eq!(
            (c, e["error"].as_str()),
            (403, Some("this vault is served read-only"))
        );
        assert_eq!(
            post(ro.addr, "/vault/preview", &json!({"text": "ok"})).0,
            200,
            "preview changes nothing"
        );
        // a change made outside the app arrives as an event
        let a = ro.addr;
        let since = json_of(get(a, "/vault/health")).1["gen"].as_u64().unwrap();
        let t = std::thread::spawn(move || {
            let mut s = TcpStream::connect(a).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            s.write_all(
                format!("GET /vault/events?since={since} HTTP/1.1\r\nHost: {a}\r\n\r\n").as_bytes(),
            )
            .unwrap();
            let mut buf = vec![0u8; 4096];
            let mut got = String::new();
            let until = Instant::now() + Duration::from_secs(15);
            while !got.contains("event: change") && Instant::now() < until {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
                }
            }
            got
        });
        std::thread::sleep(Duration::from_millis(300));
        put(&d, "Outside.md", "written by another app");
        let got = t.join().unwrap();
        assert!(
            got.contains("event: change") && got.contains("Outside.md"),
            "{got}"
        );
        ro.stop();
    }
}
