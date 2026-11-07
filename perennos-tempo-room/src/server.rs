//! `tempo-room serve`: the room app, its JSON API and live events (SSE), std only.
//!
//! Security model (the same as `tempo-vault serve` and `tempo-local-agent serve`): loopback by
//! default; the Host header must be ours (no DNS rebinding); a present Origin must be ours and
//! cross-site fetches are refused (no CSRF); only GET and POST; POST bodies must be
//! `application/json` (a web page cannot send that cross-origin without a preflight, and OPTIONS
//! is refused) and at most 1 MiB. On top of that: the owner is the person at the machine (a
//! loopback connection carrying the owner cookie), members hold a session cookie from a one-time
//! invite (HttpOnly, SameSite=Strict), and everyone else sees only the join page.
use crate::bridges::whatsapp;
use crate::hub::{Hub, PostReq, Posted, Refusal, Speaker, TeamInput, TeamPatch};
use crate::store::{InviteError, Mode, Personality};
use crate::util::{local_offset_minutes, log, unix_now};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 16 * 1024;
pub const MAX_BODY: usize = 1 << 20;
const MAX_CONNS: usize = 96;
const MAX_STREAMS: usize = 48;
/// Live streams one person can hold open at once (a phone, a laptop, a tab or two).
const MAX_STREAMS_EACH: usize = 6;
/// How often a live stream checks that its viewer is still let in (and the longest it waits
/// for events between checks): someone removed stops getting messages within two of these.
const STREAM_RECHECK: Duration = Duration::from_secs(2);
/// A quiet live stream still sends a comment this often, so proxies keep it open.
const STREAM_KEEPALIVE: Duration = Duration::from_secs(15);
/// The longest a request (its line, headers and body) may take to arrive (SR-15).
const REQUEST_DEADLINE: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(15)
};

pub const INDEX_HTML: &str = include_str!("../assets/index.html");
pub const APP_JS: &str = include_str!("../assets/app.mjs");
pub const APP_CSS: &str = include_str!("../assets/app.css");
/// Talk to Tempo: the mic (see `voice.rs`); the same two files the chat surface uses.
pub const VOICE_JS: &str = include_str!("../assets/voice.mjs");
pub const VOICE_CSS: &str = include_str!("../assets/voice.css");
/// The slow three.js field behind the join and empty screens, and the three.js build it imports
/// (0.170.0 from npm, MIT, unmodified; imported only when WebGL works).
pub const FIELD_JS: &str = include_str!("../assets/field.mjs");
const THREE_JS: &str = include_str!("../assets/vendor/three.module.min.js");
const THREE_LICENSE: &str = include_str!("../assets/vendor/three.LICENSE");
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
pub const MANIFEST: &str = include_str!("../assets/manifest.webmanifest");

const APP_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; manifest-src 'self'; object-src 'none'; frame-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

pub struct ServeOpts {
    pub bind: IpAddr,
    pub port: u16,
    /// Extra Host values to accept (`name` or `name:port`): a tailnet name, a LAN name.
    pub allow_hosts: Vec<String>,
    /// The address people use, when a proxy such as `tailscale serve` is in front
    /// (`https://laptop.tail1234.ts.net`). Invite links use it and its host is allowed.
    pub public_url: Option<String>,
}

impl Default for ServeOpts {
    fn default() -> Self {
        ServeOpts {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: crate::store::DEFAULT_PORT,
            allow_hosts: vec![],
            public_url: None,
        }
    }
}

/// `https://name[:port][/...]` to (base URL without a trailing slash, host[:port]).
pub fn public_parts(url: &str) -> Result<(String, String), String> {
    let u = url.trim().trim_end_matches('/');
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .ok_or("--public-url starts with https:// (or http://)")?;
    let host = rest.split('/').next().unwrap_or("").to_lowercase();
    if host.is_empty() || host.contains('@') || rest.contains('/') {
        return Err("--public-url is just the address, like https://laptop.tail1234.ts.net".into());
    }
    let scheme = if u.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    Ok((format!("{scheme}://{host}"), host))
}

struct Shared {
    hub: Arc<Hub>,
    allowed: HashSet<String>,
    /// The address people outside this machine use in invite links (None: loopback only).
    public: Option<String>,
    port: u16,
    conns: AtomicUsize,
    streams: AtomicUsize,
    /// Live streams open per viewer (`owner` or a member id).
    streams_by: Mutex<HashMap<String, usize>>,
    stop: AtomicBool,
    /// The WhatsApp webhook listener serves only `/room/whatsapp`.
    webhook_only: bool,
    /// WhatsApp message ids already answered (kept across restarts).
    seen_wa: Mutex<whatsapp::Seen>,
}

/// A running server (tests use the addresses; `serve` blocks on it).
pub struct Running {
    pub addr: SocketAddr,
    /// When `--bind` is another address, the owner still connects on loopback here.
    pub loopback: Option<SocketAddr>,
    pub webhook: Option<SocketAddr>,
    shared: Vec<Arc<Shared>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Running {
    pub fn wait(self) {
        for t in self.threads {
            let _ = t.join();
        }
    }
    pub fn stop(self) {
        for s in &self.shared {
            s.stop.store(true, Ordering::SeqCst);
        }
        for a in [Some(self.addr), self.loopback, self.webhook]
            .into_iter()
            .flatten()
        {
            let _ = TcpStream::connect(a);
        }
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn host_set(port: u16, extra: &[String]) -> HashSet<String> {
    let mut allowed: HashSet<String> = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .into_iter()
    .collect();
    for h in extra {
        let h = h.trim().to_lowercase();
        if h.is_empty() {
            continue;
        }
        let has_port = if h.starts_with('[') {
            h.contains("]:")
        } else {
            h.contains(':')
        };
        if has_port {
            allowed.insert(h);
        } else {
            // A reverse proxy (Tailscale serve or Funnel) sends the bare name.
            allowed.insert(format!("{h}:{port}"));
            allowed.insert(h);
        }
    }
    allowed
}

fn accept_loop(listener: TcpListener, sh: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
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
    })
}

pub fn start(hub: Arc<Hub>, opts: ServeOpts) -> Result<Running, String> {
    let listener = TcpListener::bind((opts.bind, opts.port))
        .map_err(|e| format!("cannot listen on {}:{}: {e}", opts.bind, opts.port))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let port = addr.port();
    let mut extra = opts.allow_hosts.clone();
    let specific = !opts.bind.is_loopback() && !opts.bind.is_unspecified();
    if specific {
        extra.push(match opts.bind {
            IpAddr::V6(v6) => format!("[{v6}]:{port}"),
            v4 => format!("{v4}:{port}"),
        });
    }
    let mut public = (!opts.bind.is_loopback()).then(|| {
        let hp = opts
            .allow_hosts
            .first()
            .map(|h| {
                if h.contains(':') {
                    h.clone()
                } else {
                    format!("{h}:{port}")
                }
            })
            .unwrap_or_else(|| match opts.bind {
                IpAddr::V6(v6) => format!("[{v6}]:{port}"),
                v4 => format!("{v4}:{port}"),
            });
        format!("http://{hp}")
    });
    if let Some(u) = &opts.public_url {
        let (base, host) = public_parts(u)?;
        extra.push(host);
        public = Some(base);
    }
    let make = |allowed: HashSet<String>, webhook_only: bool| {
        Arc::new(Shared {
            hub: hub.clone(),
            allowed,
            public: public.clone(),
            port,
            conns: AtomicUsize::new(0),
            streams: AtomicUsize::new(0),
            streams_by: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            webhook_only,
            seen_wa: Mutex::new(whatsapp::Seen::load(Some(&hub.lock().dir))),
        })
    };
    let main = make(host_set(port, &extra), false);
    let mut threads = vec![accept_loop(listener, main.clone())];
    let mut shared = vec![main.clone()];
    // With --bind on a tailnet or LAN address, the owner still has loopback on the same port.
    let mut loopback = None;
    if specific {
        if let Ok(l) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            loopback = l.local_addr().ok();
            threads.push(accept_loop(l, main.clone()));
        }
    }
    let mut webhook = None;
    if let Some(wa) = &hub.bridges.whatsapp {
        // A busy port turns WhatsApp off, never the room.
        match TcpListener::bind((Ipv4Addr::LOCALHOST, wa.webhook_port)) {
            Ok(l) => {
                let wport = l.local_addr().map_err(|e| e.to_string())?.port();
                webhook = l.local_addr().ok();
                let hosts: Vec<String> = [wa.public_host.clone()]
                    .into_iter()
                    .filter(|h| !h.is_empty())
                    .collect();
                let sh = make(host_set(wport, &hosts), true);
                threads.push(accept_loop(l, sh.clone()));
                shared.push(sh);
            }
            Err(e) => log(&format!(
                "warning: the WhatsApp webhook cannot listen on 127.0.0.1:{} ({e}); WhatsApp is off, the room runs on",
                wa.webhook_port
            )),
        }
    }
    Ok(Running {
        addr,
        loopback,
        webhook,
        shared,
        threads,
    })
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    peer_loopback: bool,
    peer: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    /// The request came through a reverse proxy (`tailscale serve`, Funnel, any other): the TCP
    /// peer is then the proxy on loopback, not the person. Proxies add these headers and a
    /// browser or the CLI on this machine never sends them.
    fn proxied(&self) -> bool {
        const MARKS: [&str; 8] = [
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-real-ip",
            "tailscale-user-login",
            "tailscale-user-name",
            "tailscale-headers-info",
        ];
        self.headers
            .iter()
            .any(|(k, _)| MARKS.contains(&k.as_str()))
    }
    /// This machine itself: a loopback peer that no proxy stands behind.
    fn at_machine(&self) -> bool {
        self.peer_loopback && !self.proxied()
    }
    /// Who is asking, for rate limits: the TCP peer, or behind a proxy on loopback the last
    /// address it recorded (the proxy's own entry, which a visitor cannot forge).
    fn client(&self) -> String {
        if self.peer_loopback {
            if let Some(ip) = self
                .header("x-forwarded-for")
                .and_then(|v| v.rsplit(',').next())
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.len() <= 64)
            {
                return ip.to_string();
            }
        }
        self.peer.clone()
    }
    fn q(&self, k: &str) -> &str {
        self.query.get(k).map(String::as_str).unwrap_or("")
    }
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k == "cookie")
            .flat_map(|(_, v)| v.split(';'))
            .find_map(|kv| {
                let (k, v) = kv.trim().split_once('=')?;
                (k.trim() == name).then(|| v.trim().to_string())
            })
    }
}

/// Reads the socket until a fixed deadline, so a client that sends its request a byte at a time
/// cannot hold one of the connections for long (SR-15).
struct DeadlineRead {
    s: TcpStream,
    until: Instant,
}

impl Read for DeadlineRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.s.set_read_timeout(Some(left))?;
        self.s.read(buf)
    }
}

/// Too slow is 408; anything else wrong with the bytes is 400.
fn read_err(e: std::io::Error) -> (u16, &'static str) {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            (408, "the request took too long to arrive")
        }
        _ => (400, "bad request"),
    }
}

fn read_request(s: &TcpStream) -> Result<Req, (u16, &'static str)> {
    let bad = (400, "bad request");
    let peer = s.peer_addr().map_err(|_| bad)?;
    let clone = s.try_clone().map_err(|_| bad)?;
    let timed = DeadlineRead {
        s: clone,
        until: Instant::now() + REQUEST_DEADLINE,
    };
    let mut rd = BufReader::new(timed.take((MAX_HEAD + crate::voice::MAX_AUDIO + 1) as u64));
    let mut line = String::new();
    rd.read_line(&mut line).map_err(read_err)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if !target.starts_with('/') || !parts.next().unwrap_or("").starts_with("HTTP/1.") {
        return Err(bad);
    }
    let mut headers = Vec::new();
    let mut head = line.len();
    loop {
        let mut h = String::new();
        let n = rd.read_line(&mut h).map_err(read_err)?;
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
            (crate::util::pct_decode(k), crate::util::pct_decode(v))
        })
        .collect();
    let ip = match peer.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    let mut req = Req {
        method: method.to_string(),
        path: path.to_string(),
        query,
        headers,
        body: vec![],
        peer_loopback: ip.is_loopback(),
        peer: ip.to_string(),
    };
    if req.header("transfer-encoding").is_some() {
        return Err((411, "send a Content-Length body"));
    }
    let len: usize = match req.header("content-length") {
        Some(v) => v.parse().map_err(|_| bad)?,
        None => 0,
    };
    // a recording may be bigger than a message
    let cap = if req.path == "/room/voice/transcribe" {
        crate::voice::MAX_AUDIO
    } else {
        MAX_BODY
    };
    if len > cap {
        return Err((413, "body too large"));
    }
    req.body = vec![0; len];
    rd.read_exact(&mut req.body).map_err(read_err)?;
    Ok(req)
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

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

fn send_json(s: &mut TcpStream, code: u16, v: &Value, extra: &[(&str, String)]) {
    let _ = respond(s, code, "application/json", v.to_string().as_bytes(), extra);
}

fn refuse(s: &mut TcpStream, code: u16, why: &str) {
    send_json(s, code, &json!({ "error": why }), &[]);
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
        let ours = o
            .strip_prefix("http://")
            .or_else(|| o.strip_prefix("https://"))
            .is_some_and(|h| sh.allowed.contains(h));
        if !ours {
            return Err((403, "cross-origin request refused"));
        }
    }
    // Only our own origin may call the API: not another site, and not another page on this
    // machine or domain either (`same-site` is a different port or a sibling name).
    if matches!(
        req.header("sec-fetch-site"),
        Some("cross-site") | Some("same-site")
    ) {
        return Err((403, "cross-site request refused"));
    }
    if req.method != "GET" && req.method != "POST" {
        return Err((405, "only GET and POST"));
    }
    if req.method == "POST"
        && req.path != "/room/voice/transcribe"
        && !req
            .header("content-type")
            .is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json"))
    {
        return Err((415, "POST bodies must be application/json"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
enum Viewer {
    Owner,
    Member(String),
    Guest,
}

fn viewer(sh: &Shared, req: &Req) -> Viewer {
    // The owner is the person at the machine: a loopback connection that no proxy is behind,
    // carrying this run's control key (the CLI and MCP) or a live owner session (the browser).
    // Behind `tailscale serve` everyone arrives on loopback, but with the proxy's headers.
    if req.at_machine() {
        if let Some(k) = req.header("x-tempo-control") {
            if sh.hub.control_ok(k) {
                return Viewer::Owner;
            }
        }
        if let Some(t) = req.cookie("tr_owner") {
            if sh.hub.lock().owner_session_ok(&t, unix_now()) {
                return Viewer::Owner;
            }
        }
    }
    // A member's session counts as used now (written to disk at most once an hour), and one
    // idle for `session_days` no longer works.
    let mut st = sh.hub.lock();
    req.cookie("tr_member")
        .and_then(|t| st.touch_session(&t, unix_now()))
        .map(Viewer::Member)
        .unwrap_or(Viewer::Guest)
}

/// The owner's own browser session at the machine: a loopback connection with no proxy in front
/// and a live owner cookie. The control key (the CLI and MCP) is the owner too, but it is not
/// this: owner-only messages, the tools switch and approvals belong to the signed-in session.
fn owner_session(sh: &Shared, req: &Req) -> bool {
    owner_session_id(sh, req).is_some()
}

/// Which owner session this is: the hash of its cookie (what the data folder keeps, never the
/// cookie itself). An approval can be answered only by the session whose ask started the turn.
fn owner_session_id(sh: &Shared, req: &Req) -> Option<String> {
    if !req.at_machine() {
        return None;
    }
    let t = req.cookie("tr_owner")?;
    let live = sh.hub.lock().owner_session_ok(&t, unix_now());
    live.then(|| crate::util::sha256_hex(&t))
}

/// May this viewer see `room`?
fn sees(sh: &Shared, v: &Viewer, room: &str) -> bool {
    match v {
        Viewer::Owner => true,
        Viewer::Member(id) => sh
            .hub
            .lock()
            .member(id)
            .is_some_and(|m| m.rooms.iter().any(|r| r == room)),
        Viewer::Guest => false,
    }
}

fn handle(sh: &Arc<Shared>, mut s: TcpStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(60)));
    let req = match read_request(&s) {
        Ok(r) => r,
        Err((code, why)) => return refuse(&mut s, code, why),
    };
    // reading the request moved the deadline onto the socket; put the usual timeout back
    let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
    if let Err((code, why)) = check(sh, &req) {
        return refuse(&mut s, code, why);
    }
    if sh.webhook_only {
        return match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/room/whatsapp") => {
                let (code, body) = match &sh.hub.bridges.whatsapp {
                    Some(c) => whatsapp::verify(c, &req.query),
                    None => (404, "off".into()),
                };
                let _ = respond(
                    &mut s,
                    code,
                    "text/plain; charset=utf-8",
                    body.as_bytes(),
                    &[],
                );
            }
            ("POST", "/room/whatsapp") => {
                let sig = req.header("x-hub-signature-256").unwrap_or("").to_string();
                let code = whatsapp::inbound(&sh.hub, &sig, &req.body, &sh.seen_wa);
                send_json(&mut s, code, &json!({"ok": code == 200}), &[]);
            }
            _ => refuse(&mut s, 404, "not found"),
        };
    }
    if let Some(api) = req.path.strip_prefix("/room/") {
        if req.method == "GET" && api == "events" {
            return events(sh, s, &req);
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
        let v = viewer(sh, &req);
        let mut cookies: Vec<(&str, String)> = vec![];
        match route(sh, &req, api, &body, &v, &mut cookies) {
            Ok(out) => send_json(&mut s, 200, &out, &cookies),
            Err((code, out)) => send_json(&mut s, code, &out, &cookies),
        }
        return;
    }
    if req.method != "GET" {
        return refuse(&mut s, 405, "only GET here");
    }
    let csp = [
        ("Content-Security-Policy", APP_CSP.to_string()),
        (
            "Permissions-Policy",
            "camera=(), microphone=(self), geolocation=()".to_string(),
        ),
    ];
    let _ = match req.path.as_str() {
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
        "/field.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            FIELD_JS.as_bytes(),
            &[],
        ),
        "/vendor/three.module.min.js" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            THREE_JS.as_bytes(),
            &[],
        ),
        "/vendor/three.LICENSE" => respond(
            &mut s,
            200,
            "text/plain; charset=utf-8",
            THREE_LICENSE.as_bytes(),
            &[],
        ),
        "/voice.mjs" => respond(
            &mut s,
            200,
            "text/javascript; charset=utf-8",
            VOICE_JS.as_bytes(),
            &[],
        ),
        "/voice.css" => respond(
            &mut s,
            200,
            "text/css; charset=utf-8",
            VOICE_CSS.as_bytes(),
            &[],
        ),
        "/app.css" => respond(
            &mut s,
            200,
            "text/css; charset=utf-8",
            APP_CSS.as_bytes(),
            &[],
        ),
        "/tempo-theme.css" => respond(
            &mut s,
            200,
            "text/css; charset=utf-8",
            THEME_CSS.as_bytes(),
            &[],
        ),
        "/icon.svg" | "/favicon.ico" => {
            respond(&mut s, 200, "image/svg+xml", ICON_SVG.as_bytes(), &[])
        }
        "/manifest.webmanifest" => respond(
            &mut s,
            200,
            "application/manifest+json",
            MANIFEST.as_bytes(),
            &[],
        ),
        p if BRAND_FILES.iter().any(|f| f.0 == p) => {
            let f = BRAND_FILES.iter().find(|f| f.0 == p).expect("listed");
            respond(&mut s, 200, f.1, f.2, &[])
        }
        _ => {
            refuse(&mut s, 404, "not found");
            Ok(())
        }
    };
}

type Fail = (u16, Value);

/// Talk to Tempo: the mic in a room. The audio is a raw body (not JSON) and goes straight to the
/// agent's whisper.cpp on this machine; nothing is kept here. The owner at the machine may use it
/// anywhere; a member only where the owner turned it on for that room.
fn voice_api(sh: &Arc<Shared>, req: &Req, api: &str, mut s: TcpStream) {
    let v = viewer(sh, req);
    let room = if api == "voice/speak" || api == "voice/guests" {
        serde_json::from_slice::<Value>(&req.body)
            .ok()
            .and_then(|b| b["room"].as_str().map(str::to_string))
            .unwrap_or_default()
    } else {
        req.q("room").to_string()
    };
    let owner = v == Viewer::Owner;
    let may = owner || (sees(sh, &v, &room) && crate::voice::allowed(&sh.hub.lock().dir, &room));
    let limit = Duration::from_secs(200);
    let agent = &sh.hub.agent;
    let pass =
        |s: &mut TcpStream, r: Result<(u16, Vec<u8>, String), crate::agent::AgentError>| match r {
            Ok((code, body, ct)) => {
                let _ = respond(s, code, &ct, &body, &[]);
            }
            Err(_) => refuse(
                s,
                503,
                "Tempo's agent isn't answering. Start it, then try again.",
            ),
        };
    match (req.method.as_str(), api) {
        ("GET", "voice") => {
            if !may {
                // a member where the owner hasn't turned it on: the page just hides the mic
                return send_json(&mut s, 200, &json!({"allowed": false}), &[]);
            }
            match agent.voice(false, "/agent/voice", "", b"", Duration::from_secs(10)) {
                Ok((200, body, _)) => {
                    let mut j: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    j["allowed"] = json!(true);
                    j["owner"] = json!(owner);
                    j["guests"] = json!(crate::voice::allowed(&sh.hub.lock().dir, &room));
                    send_json(&mut s, 200, &j, &[]);
                }
                Ok(_) | Err(_) => send_json(&mut s, 200, &json!({"allowed": false}), &[]),
            }
        }
        ("POST", "voice/transcribe") | ("POST", "voice/speak") if !may => refuse(
            &mut s,
            403,
            "The owner hasn't turned voice on for this room.",
        ),
        ("POST", "voice/transcribe") => {
            let ct = req
                .header("content-type")
                .unwrap_or("audio/wav")
                .to_string();
            let lang: String = req
                .q("lang")
                .chars()
                .filter(char::is_ascii_alphabetic)
                .take(8)
                .collect();
            let path = format!("/agent/voice/transcribe?lang={lang}");
            pass(&mut s, agent.voice(true, &path, &ct, &req.body, limit))
        }
        ("POST", "voice/speak") => {
            let text = serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|b| b["text"].as_str().map(str::to_string))
                .unwrap_or_default();
            let b = json!({"text": text}).to_string();
            pass(
                &mut s,
                agent.voice(
                    true,
                    "/agent/voice/speak",
                    "application/json",
                    b.as_bytes(),
                    limit,
                ),
            )
        }
        ("POST", "voice/guests") => {
            if !owner {
                return refuse(&mut s, 403, "Only the owner, at the machine, can do that.");
            }
            let on = serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|b| b["on"].as_bool())
                .unwrap_or(false);
            let dir = sh.hub.lock().dir.clone();
            let known = sh.hub.lock().rooms.iter().any(|r| r.id == room);
            if !known {
                return refuse(&mut s, 404, "No such room.");
            }
            match crate::voice::set(&dir, &room, on) {
                Ok(()) => send_json(&mut s, 200, &json!({"ok": true, "guests": on}), &[]),
                Err(e) => refuse(&mut s, 500, &e),
            }
        }
        _ => refuse(&mut s, 404, "not found"),
    }
}

fn fail(code: u16, why: impl Into<String>) -> Fail {
    (code, json!({ "error": why.into() }))
}

fn from_refusal(r: Refusal) -> Fail {
    fail(r.0, r.1)
}

fn need_owner(v: &Viewer) -> Result<(), Fail> {
    match v {
        Viewer::Owner => Ok(()),
        _ => Err(fail(403, "Only the owner, at the machine, can do that.")),
    }
}

fn body_str<'a>(b: &'a Value, k: &str) -> &'a str {
    b[k].as_str().unwrap_or("")
}

const COOKIE: &str = "Path=/; HttpOnly; SameSite=Strict";

/// Member cookies also carry `Secure` when people reach the room over https (`--public-url`).
fn member_cookie(sh: &Shared) -> String {
    if sh
        .public
        .as_deref()
        .is_some_and(|p| p.starts_with("https://"))
    {
        format!("{COOKIE}; Secure")
    } else {
        COOKIE.to_string()
    }
}

/// A room as one viewer sees it. `private`: the owner's own session, which may see owner-only
/// messages and the tools switch; anyone else gets neither (not even in the last-message preview
/// or the count).
fn room_json(
    sh: &Shared,
    st: &crate::store::Store,
    r: &crate::store::Room,
    members: usize,
    private: bool,
) -> Value {
    let shown = |m: &&crate::store::Msg| private || !m.owner_only;
    let mut j = json!({
        "id": r.id,
        "name": r.name,
        "mode": r.mode.as_str(),
        "personality": r.personality.as_str(),
        "count": if private { r.next.saturating_sub(1) } else { r.tail.iter().filter(shown).map(|m| m.id).max().unwrap_or(0) },
        // the last thing someone said (a join note only when nothing else is there)
        "last": r
            .tail
            .iter()
            .rev()
            .filter(shown)
            .find(|m| m.kind != crate::store::Kind::System)
            .or(r.tail.iter().rev().find(shown))
            .map(|m| sh.hub.msg_view(m)),
        "members": members,
        "team": r.team,
        "thinking": sh.hub.thinking(&r.id),
        "thinking_who": sh.hub.thinking_who(&r.id),
    });
    if private {
        if let (Some(o), Some(t)) = (
            j.as_object_mut(),
            sh.hub.tools_state_in(st, &r.id).as_object(),
        ) {
            o.extend(t.clone());
        }
    }
    j
}

fn route(
    sh: &Arc<Shared>,
    req: &Req,
    api: &str,
    b: &Value,
    v: &Viewer,
    cookies: &mut Vec<(&'static str, String)>,
) -> Result<Value, Fail> {
    let hub = &sh.hub;
    let post = req.method == "POST";
    match (post, api) {
        (false, "health") => {
            let rooms = hub.lock().rooms.len();
            let up = match hub.agent_up() {
                Some(u) => u,
                None => {
                    let u = hub.agent.health(Duration::from_secs(2));
                    hub.set_agent_up(u);
                    u
                }
            };
            Ok(
                json!({"ok": true, "app": "tempo-room", "version": env!("CARGO_PKG_VERSION"), "rooms": rooms, "agent": if up { "up" } else { "down" }}),
            )
        }
        (false, "me") => {
            let session = match v {
                Viewer::Owner => owner_session_id(sh, req),
                _ => None,
            };
            let st = hub.lock();
            let off = local_offset_minutes(st.cfg.utc_offset.as_deref());
            let today = crate::util::date_of(unix_now(), off);
            let birthday = st.cfg.birthday_md().is_some_and(|md| today.ends_with(&md));
            let (role, id, name, rooms): (&str, Option<String>, String, Vec<String>) = match v {
                Viewer::Owner => (
                    "owner",
                    None,
                    st.cfg.owner_name.clone(),
                    st.rooms.iter().map(|r| r.id.clone()).collect(),
                ),
                Viewer::Member(mid) => match st.member(mid) {
                    Some(m) => (
                        "member",
                        Some(m.id.clone()),
                        m.name.clone(),
                        m.rooms.clone(),
                    ),
                    None => ("guest", None, String::new(), vec![]),
                },
                Viewer::Guest => ("guest", None, String::new(), vec![]),
            };
            Ok(json!({
                "role": role, "id": id, "name": name, "rooms": rooms, "session": session,
                "owner_name": st.cfg.owner_name,
                "birthday": birthday,
                "loopback": req.at_machine(),
                "shared": sh.public.is_some(),
                "agent": match hub.agent_up() { Some(true) => "up", Some(false) => "down", None => "unknown" },
                "version": env!("CARGO_PKG_VERSION"),
            }))
        }
        (false, "rooms") => {
            if *v == Viewer::Guest {
                return Err(fail(403, "Join a room with an invite first."));
            }
            let private = owner_session(sh, req);
            let st = hub.lock();
            let list: Vec<Value> = st
                .rooms
                .iter()
                .filter(|r| match v {
                    Viewer::Owner => true,
                    Viewer::Member(id) => st.member(id).is_some_and(|m| m.rooms.contains(&r.id)),
                    Viewer::Guest => false,
                })
                .map(|r| {
                    let n = st
                        .members
                        .iter()
                        .filter(|m| m.rooms.contains(&r.id))
                        .count();
                    room_json(sh, &st, r, n, private)
                })
                .collect();
            Ok(json!({"rooms": list, "seq": hub.seq()}))
        }
        (false, "messages") => {
            let room = req.q("room");
            if !sees(sh, v, room) {
                return Err(fail(403, "You are not in that room."));
            }
            let limit = req.q("limit").parse().unwrap_or(60);
            let since = req.q("since").parse().ok();
            let before = req.q("before").parse().ok();
            // owner-only messages go to the owner's own session and to no one else
            let private = owner_session(sh, req);
            let msgs = hub.lock().messages_for(room, since, before, limit, private);
            // whether this viewer has anything older to page to (hidden messages do not count)
            let more = msgs.first().is_some_and(|f| {
                !hub.lock()
                    .messages_for(room, None, Some(f.id), 1, private)
                    .is_empty()
            });
            Ok(json!({
                "room": room,
                "more": more,
                "messages": msgs.iter().map(|m| hub.msg_view(m)).collect::<Vec<_>>(),
                "thinking": hub.thinking(room),
                "thinking_who": hub.thinking_who(room),
                "seq": hub.seq(),
            }))
        }
        (false, "members") => {
            let room = req.q("room");
            if !sees(sh, v, room) {
                return Err(fail(403, "You are not in that room."));
            }
            let st = hub.lock();
            let owner = *v == Viewer::Owner;
            let list: Vec<Value> = st
                .members
                .iter()
                .filter(|m| m.rooms.iter().any(|r| r == room))
                .map(|m| {
                    if owner {
                        json!({"id": m.id, "name": m.name, "muted": m.muted.iter().any(|r| r == room), "joined": m.joined, "devices": m.sessions.len()})
                    } else {
                        json!({"name": m.name})
                    }
                })
                .collect();
            // the teammates present: who they are, never their role or model
            let team: Vec<Value> = st
                .teammates_in(room)
                .iter()
                .map(|t| t.public_json())
                .collect();
            Ok(json!({"members": list, "owner_name": st.cfg.owner_name, "team": team}))
        }
        (true, "owner") => {
            if !req.at_machine() {
                return Err(fail(
                    403,
                    "The owner signs in at the machine itself (loopback).",
                ));
            }
            let key = format!("owner:{}", req.peer);
            if hub.limit(&key, 20, Duration::from_secs(600)).is_err() {
                return Err(fail(429, "Too many tries; wait a few minutes."));
            }
            // The owner link is used up here and traded for a session: the cookie is a new
            // random value, never the link.
            let session = hub
                .lock()
                .redeem_owner_link(body_str(b, "token"), unix_now())
                .map_err(|e| fail(403, e))?;
            cookies.push((
                "Set-Cookie",
                format!("tr_owner={session}; {COOKIE}; Max-Age=2592000"),
            ));
            Ok(json!({"ok": true}))
        }
        (true, "owner-link") => {
            need_owner(v)?;
            let token = hub.lock().new_owner_link().map_err(|e| fail(500, e))?;
            Ok(json!({
                "ok": true,
                "link": format!("http://127.0.0.1:{}/#owner={token}", sh.port),
            }))
        }
        (true, "logout") => {
            // End the session on the server too, not only in the browser. A member's sign-out
            // ends this device's session only; their other devices keep working.
            if let Some(t) = req.cookie("tr_owner") {
                hub.lock().end_owner_session(&t);
            }
            if let Some(t) = req.cookie("tr_member") {
                hub.lock().end_member_session(&t);
            }
            cookies.push(("Set-Cookie", format!("tr_owner=; {COOKIE}; Max-Age=0")));
            cookies.push((
                "Set-Cookie",
                format!("tr_member=; {}; Max-Age=0", member_cookie(sh)),
            ));
            Ok(json!({"ok": true}))
        }
        (true, "join") => {
            let limit = hub.lock().cfg.limits.join_per_ip;
            if hub
                .limit(
                    &format!("join:{}", req.client()),
                    limit,
                    Duration::from_secs(600),
                )
                .is_err()
            {
                return Err(fail(429, "Too many tries from here; wait ten minutes."));
            }
            let existing = match v {
                Viewer::Member(id) => Some(id.clone()),
                _ => None,
            };
            let (room, mid, token, name) = {
                let mut st = hub.lock();
                let idx = st
                    .check_invite(body_str(b, "code"), unix_now())
                    .map_err(|e| {
                        fail(
                            if e == InviteError::NotFound { 404 } else { 410 },
                            e.message(),
                        )
                    })?;
                if st.invites[idx].member.is_some() {
                    // A sign-in link: one more device for the member it was made for. No name
                    // is asked, no member is made, their rooms stay as they are, and the
                    // cookie the browser may already hold is not looked at.
                    let (mid, token) = st
                        .redeem_device(idx, unix_now())
                        .map_err(|e| fail(400, e))?;
                    let m = st.member(&mid);
                    let name = m.map(|m| m.name.clone()).unwrap_or_default();
                    let rooms = m.map(|m| m.rooms.clone()).unwrap_or_default();
                    drop(st);
                    cookies.push((
                        "Set-Cookie",
                        format!("tr_member={token}; {}; Max-Age=31536000", member_cookie(sh)),
                    ));
                    return Ok(
                        json!({"ok": true, "device": true, "room": rooms.first(), "rooms": rooms, "member": mid, "name": name}),
                    );
                }
                let room = st.invites[idx].room.clone();
                let (mid, token) = st
                    .redeem(idx, body_str(b, "name"), existing.as_deref())
                    .map_err(|e| fail(400, e))?;
                let name = st.member(&mid).map(|m| m.name.clone()).unwrap_or_default();
                (room, mid, token, name)
            };
            if let Some(t) = token {
                cookies.push((
                    "Set-Cookie",
                    format!("tr_member={t}; {}; Max-Age=31536000", member_cookie(sh)),
                ));
            }
            // a guest is in: the room's owner conversation is set aside before anything else
            hub.settle_owner_rooms();
            hub.post_system(&room, &format!("{name} joined the room."));
            hub.emit(&room, "room", json!({"room": room}));
            Ok(json!({"ok": true, "room": room, "member": mid, "name": name}))
        }
        (true, "post") => {
            let room = body_str(b, "room");
            let text = body_str(b, "text");
            let who = match v {
                Viewer::Owner => Speaker::owner(&hub.lock().cfg.owner_name),
                Viewer::Member(id) => {
                    let name = hub
                        .lock()
                        .member(id)
                        .map(|m| m.name.clone())
                        .unwrap_or_default();
                    Speaker::member(id, &name)
                }
                Viewer::Guest => return Err(fail(403, "Join a room with an invite first.")),
            };
            if !sees(sh, v, room) {
                return Err(fail(403, "You are not in that room."));
            }
            let via = match body_str(b, "via") {
                "cli" | "mcp" if *v == Viewer::Owner => body_str(b, "via"),
                _ => "app",
            };
            if b["as_tempo"] == true {
                need_owner(v)?;
                if hub.lock().room(room).is_none() {
                    return Err(fail(404, "No such room."));
                }
                let t = text.trim();
                if t.is_empty() || t.chars().count() > crate::store::MAX_TEXT {
                    return Err(fail(400, "Say something (4,000 characters at most)."));
                }
                if hub
                    .limit(&format!("tempo-post:{via}"), 30, Duration::from_secs(60))
                    .is_err()
                {
                    return Err(fail(429, "Too many posts as Tempo; wait a minute."));
                }
                let m = hub
                    .post_tempo(room, t, None, via, None)
                    .ok_or_else(|| fail(500, "could not save"))?;
                return Ok(json!({"ok": true, "msg": m.to_json()}));
            }
            let mut r = PostReq::new(room, who, text, via);
            r.reply_to = b["reply_to"].as_u64();
            r.owner_session = owner_session_id(sh, req);
            match hub.post(r).map_err(from_refusal)? {
                Posted::Msg(m) => Ok(json!({"ok": true, "msg": hub.msg_view(&m)})),
                Posted::Note(n) => Ok(json!({"ok": true, "note": n})),
            }
        }
        (true, "create") => {
            need_owner(v)?;
            let mode = Mode::parse(body_str(b, "mode")).unwrap_or(Mode::Mention);
            let p = Personality::parse(body_str(b, "personality")).unwrap_or(Personality::Warm);
            let id = hub
                .lock()
                .create_room(body_str(b, "name"), mode, p)
                .map_err(|e| fail(400, e))?;
            hub.emit(&id, "room", json!({"room": id}));
            let private = owner_session(sh, req);
            let st = hub.lock();
            let r = st.room(&id).ok_or_else(|| fail(500, "lost the room"))?;
            Ok(json!({"ok": true, "room": room_json(sh, &st, r, 0, private)}))
        }
        (true, "update") => {
            need_owner(v)?;
            let id = body_str(b, "room").to_string();
            {
                let mut st = hub.lock();
                let r = st.room_mut(&id).ok_or_else(|| fail(404, "No such room."))?;
                if let Some(n) = b["name"].as_str().map(str::trim).filter(|n| !n.is_empty()) {
                    r.name = crate::util::head_chars(n, 60).to_string();
                }
                if let Some(m) = b["mode"].as_str() {
                    r.mode =
                        Mode::parse(m).ok_or_else(|| fail(400, "mode is always or mention"))?;
                }
                if let Some(p) = b["personality"].as_str() {
                    r.personality = Personality::parse(p)
                        .ok_or_else(|| fail(400, "personality is plain, warm or playful"))?;
                }
                st.save_rooms().map_err(|e| fail(500, e))?;
            }
            hub.emit(&id, "room", json!({"room": id}));
            Ok(json!({"ok": true}))
        }
        (true, "invite") => {
            need_owner(v)?;
            let base = sh
                .public
                .clone()
                .unwrap_or_else(|| format!("http://127.0.0.1:{}", sh.port));
            // With "member": a one-time sign-in link for one more device of that member.
            if let Some(mid) = b["member"].as_str() {
                let (code, expires, name) = {
                    let mut st = hub.lock();
                    let (code, expires) = st
                        .new_device_invite(mid, b["hours"].as_f64(), unix_now())
                        .map_err(|e| fail(404, e))?;
                    let name = st.member(mid).map(|m| m.name.clone()).unwrap_or_default();
                    (code, expires, name)
                };
                hub.settle_owner_rooms();
                return Ok(json!({
                    "ok": true,
                    "code": code,
                    "link": format!("{base}/#signin={code}"),
                    "expires": expires,
                    "member": mid,
                    "name": name,
                    "local_only": sh.public.is_none(),
                }));
            }
            let room = body_str(b, "room");
            let (code, expires) = hub
                .lock()
                .new_invite(room, b["hours"].as_f64())
                .map_err(|e| fail(400, e))?;
            // an open invite is a guest to come: the room's owner conversation is set aside now
            hub.settle_owner_rooms();
            hub.emit(room, "room", json!({"room": room}));
            Ok(json!({
                "ok": true,
                "code": code,
                "link": format!("{base}/#join={code}"),
                "expires": expires,
                "local_only": sh.public.is_none(),
            }))
        }
        // ---- owner-only rooms (Room phase 2b). These three belong to the owner's own signed-in
        // session at the machine: not the control key (the CLI and MCP), not a member, not a
        // bridge, and nothing that came through a proxy.
        (true, "tools") => {
            need_owner(v)?;
            if !owner_session(sh, req) {
                return Err(fail(
                    403,
                    "Only the owner, signed in at the machine, can change that.",
                ));
            }
            let on = b["on"] == true;
            hub.set_tools(body_str(b, "room"), on)
                .map(|state| {
                    let mut o = json!({"ok": true});
                    if let (Some(x), Some(y)) = (o.as_object_mut(), state.as_object()) {
                        x.extend(y.clone());
                    }
                    o
                })
                .map_err(from_refusal)
        }
        (true, "revoke-invites") => {
            need_owner(v)?;
            if !owner_session(sh, req) {
                return Err(fail(
                    403,
                    "Only the owner, signed in at the machine, can change that.",
                ));
            }
            let n = hub
                .revoke_invites(body_str(b, "room"))
                .map_err(from_refusal)?;
            Ok(json!({"ok": true, "revoked": n, "state": hub.tools_state(body_str(b, "room"))}))
        }
        (true, "approve") => {
            // anything but the owner's own session at the machine: 403, the agent is not called
            let no = || fail(403, "That approval cannot be answered from here.");
            need_owner(v).map_err(|_| no())?;
            let Some(session) = owner_session_id(sh, req) else {
                return Err(no());
            };
            hub.approve(
                body_str(b, "room"),
                body_str(b, "id"),
                b["ok"] == true,
                &session,
            )
            .map(|_| json!({"ok": true}))
            .map_err(from_refusal)
        }
        (true, "mute") => {
            need_owner(v)?;
            let room = body_str(b, "room");
            let on = b["muted"].as_bool().unwrap_or(true);
            let name = hub
                .lock()
                .set_muted(body_str(b, "member"), room, on)
                .map_err(|e| fail(404, e))?;
            hub.post_system(
                room,
                &if on {
                    format!("{name} was muted by the owner.")
                } else {
                    format!("{name} can post again.")
                },
            );
            hub.emit(room, "room", json!({"room": room}));
            Ok(json!({"ok": true}))
        }
        (true, "remove") => {
            need_owner(v)?;
            let mid = body_str(b, "member");
            let rooms = hub
                .lock()
                .member(mid)
                .map(|m| m.rooms.clone())
                .unwrap_or_default();
            let (name, to) = hub.lock().remove_member(mid).map_err(|e| fail(404, e))?;
            for r in &rooms {
                hub.post_system(r, &format!("{name} is no longer in this room."));
                hub.emit(r, "room", json!({"room": r}));
            }
            log(&format!(
                "member {mid} removed; record kept in {}",
                to.display()
            ));
            Ok(json!({"ok": true}))
        }
        (true, "haiku") => {
            let room = body_str(b, "room");
            if !sees(sh, v, room) {
                return Err(fail(403, "You are not in that room."));
            }
            let who = match v {
                Viewer::Member(id) => id.clone(),
                _ => "owner".into(),
            };
            if hub
                .limit(&format!("haiku:{who}"), 3, Duration::from_secs(600))
                .is_err()
            {
                return Err(fail(429, "One haiku at a time."));
            }
            hub.set_haiku(room);
            Ok(
                json!({"ok": true, "note": "Up, up, down, down... Tempo will answer the next question here as a haiku."}),
            )
        }
        (true, "settings") => {
            need_owner(v)?;
            let mut st = hub.lock();
            if let Some(n) = b["owner_name"].as_str() {
                let n = crate::store::valid_name(n).map_err(|e| fail(400, e))?;
                if st.teammate_named_like(&n) {
                    return Err(fail(400, "A teammate already has that name."));
                }
                st.cfg.owner_name = n.clone();
                st.cfg.raw["owner_name"] = json!(n);
            }
            st.save_config().map_err(|e| fail(500, e))?;
            Ok(json!({"ok": true, "owner_name": st.cfg.owner_name}))
        }
        // ---- the owner's team (owner only: owner session or control key, loopback, no proxy)
        (false, "team") => {
            need_owner(v)?;
            let st = hub.lock();
            let list: Vec<Value> = st
                .team
                .iter()
                .map(|t| {
                    let mut j = t.to_json();
                    j["rooms"] = json!(st
                        .rooms
                        .iter()
                        .filter(|r| r.team.contains(&t.handle))
                        .map(|r| r.id.clone())
                        .collect::<Vec<_>>());
                    j
                })
                .collect();
            Ok(json!({"team": list}))
        }
        (false, "team/models") => {
            need_owner(v)?;
            Ok(match hub.ollama_models() {
                Ok(list) => json!({
                    "ollama": "up",
                    "models": list.iter().map(|m| json!({"name": m.name, "size": m.size})).collect::<Vec<_>>(),
                }),
                Err(note) => json!({"ollama": "down", "models": [], "note": note}),
            })
        }
        (true, "team/add") => {
            need_owner(v)?;
            let rooms: Vec<String> = b["rooms"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str().map(str::to_string))
                .collect();
            let input = TeamInput {
                handle: body_str(b, "handle").trim().to_string(),
                name: body_str(b, "name").to_string(),
                role: body_str(b, "role").to_string(),
                kind: body_str(b, "kind").to_string(),
                model: b["model"].as_str().map(str::to_string),
            };
            let (t, note) = hub.team_add(&input, &rooms).map_err(from_refusal)?;
            Ok(json!({"ok": true, "teammate": t.to_json(), "note": note}))
        }
        (true, "team/edit") => {
            need_owner(v)?;
            let patch = TeamPatch {
                name: b["name"].as_str().map(str::to_string),
                role: b["role"].as_str().map(str::to_string),
                kind: b["kind"].as_str().map(str::to_string),
                model: b["model"].as_str().map(str::to_string),
            };
            let (t, note) = hub
                .team_edit(body_str(b, "handle"), &patch)
                .map_err(from_refusal)?;
            Ok(json!({"ok": true, "teammate": t.to_json(), "note": note}))
        }
        (true, "team/remove") => {
            need_owner(v)?;
            hub.team_remove(body_str(b, "handle"))
                .map_err(from_refusal)?;
            Ok(json!({"ok": true}))
        }
        (true, "team/room") => {
            need_owner(v)?;
            let on = b["on"].as_bool().unwrap_or(true);
            hub.team_room(body_str(b, "room"), body_str(b, "handle"), on)
                .map_err(from_refusal)?;
            Ok(json!({"ok": true}))
        }
        (false, "whatsapp") | (true, "whatsapp") => {
            Err(fail(404, "the WhatsApp webhook has its own port"))
        }
        _ => Err(fail(404, "no such endpoint")),
    }
}

/// Server-sent events: `message`, `typing` and `room`, only for rooms the viewer is in.
fn events(sh: &Arc<Shared>, mut s: TcpStream, req: &Req) {
    let v = viewer(sh, req);
    if v == Viewer::Guest {
        return refuse(&mut s, 403, "Join a room with an invite first.");
    }
    // owner-only events (messages written in owner mode, their approvals) go to the owner's own
    // session only, and a stream that stops being that stops getting them
    let mut private = owner_session(sh, req);
    // One person cannot take all the live streams.
    let who = match &v {
        Viewer::Member(id) => id.clone(),
        _ => "owner".to_string(),
    };
    {
        let mut by = sh.streams_by.lock().unwrap_or_else(|e| e.into_inner());
        let n = by.entry(who.clone()).or_insert(0);
        if *n >= MAX_STREAMS_EACH {
            return refuse(&mut s, 503, "too many live streams open for you");
        }
        *n += 1;
    }
    let release = |sh: &Shared| {
        let mut by = sh.streams_by.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = by.get_mut(&who) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                by.remove(&who);
            }
        }
    };
    if sh.streams.fetch_add(1, Ordering::SeqCst) >= MAX_STREAMS {
        sh.streams.fetch_sub(1, Ordering::SeqCst);
        release(sh);
        return refuse(&mut s, 503, "too many live streams");
    }
    let mut since = req
        .header("last-event-id")
        .and_then(|x| x.parse::<u64>().ok())
        .or_else(|| req.q("since").parse().ok())
        .unwrap_or_else(|| sh.hub.seq());
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\nretry: 2000\n\n";
    let mut ok = s.write_all(head.as_bytes()).and_then(|_| s.flush()).is_ok();
    let (mut last_check, mut last_write) = (Instant::now(), Instant::now());
    while ok && !sh.stop.load(Ordering::SeqCst) && !sh.hub.stop.load(Ordering::SeqCst) {
        // someone removed (or signed out) while watching: the stream ends within a few seconds,
        // because the wait below is short and the check runs every STREAM_RECHECK
        if last_check.elapsed() >= STREAM_RECHECK {
            if viewer(sh, req) == Viewer::Guest {
                break;
            }
            private = owner_session(sh, req);
            last_check = Instant::now();
        }
        let out = match sh.hub.wait_events(since, STREAM_RECHECK) {
            None => {
                since = sh.hub.seq();
                // with an id, so the browser's Last-Event-ID moves on
                format!("id: {since}\nevent: reset\ndata: {{}}\n\n")
            }
            Some(evs) => {
                let mut out = String::new();
                for e in evs {
                    since = since.max(e.seq);
                    if sees(sh, &v, &e.room) && (private || !e.owner_only) {
                        out.push_str(&format!(
                            "id: {}\nevent: {}\ndata: {}\n\n",
                            e.seq, e.kind, e.data
                        ));
                    }
                }
                if out.is_empty() {
                    if last_write.elapsed() < STREAM_KEEPALIVE {
                        continue; // nothing to send yet; go back to the check and the wait
                    }
                    out.push_str(": keep-alive\n\n");
                }
                out
            }
        };
        ok = s.write_all(out.as_bytes()).and_then(|_| s.flush()).is_ok();
        last_write = Instant::now();
    }
    sh.streams.fetch_sub(1, Ordering::SeqCst);
    release(sh);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridges::Outbound;
    use crate::hub::tests::hub_at;
    use crate::testutil::{dead_url, FakeAgent, Script};

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

    fn get(a: SocketAddr, path: &str, cookie: &str) -> (u16, Value) {
        let (c, _, b) = http(
            a,
            &format!("GET {path} HTTP/1.1\r\nHost: {a}\r\nCookie: {cookie}\r\n\r\n"),
        );
        (c, serde_json::from_str(&b).unwrap_or(Value::Null))
    }

    fn post(a: SocketAddr, path: &str, cookie: &str, body: &Value) -> (u16, String, Value) {
        let b = body.to_string();
        let (c, head, out) = http(a, &format!("POST {path} HTTP/1.1\r\nHost: {a}\r\nOrigin: http://{a}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len()));
        (c, head, serde_json::from_str(&out).unwrap_or(Value::Null))
    }

    fn set_cookie(head: &str, name: &str) -> Option<String> {
        head.lines()
            .filter_map(|l| l.strip_prefix("Set-Cookie: "))
            .find(|l| l.starts_with(&format!("{name}=")))
            .map(str::to_string)
    }

    fn opts() -> ServeOpts {
        ServeOpts {
            port: 0,
            ..ServeOpts::default()
        }
    }

    #[test]
    fn refuses_what_it_should() {
        let (hub, _) = hub_at(&dead_url(), "srv-refuse", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let (c, h) = get(a, "/room/health", "");
        assert_eq!(c, 200);
        assert_eq!(h["ok"], true);
        assert_eq!(h["app"], "tempo-room");
        assert_eq!(h["rooms"], 1);
        assert_eq!(h["agent"], "down");
        assert!(h["version"].is_string());
        let (c, head, body) = http(a, &format!("GET / HTTP/1.1\r\nHost: {a}\r\n\r\n"));
        assert_eq!(c, 200);
        assert!(head.contains(
            "Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'"
        ));
        assert!(body.contains("<html"));
        let bad_host = http(a, "GET /room/health HTTP/1.1\r\nHost: evil.example\r\n\r\n").0;
        assert_eq!(bad_host, 403, "DNS rebinding");
        assert_eq!(
            http(
                a,
                &format!(
                    "GET /room/me HTTP/1.1\r\nHost: {a}\r\nOrigin: http://evil.example\r\n\r\n"
                )
            )
            .0,
            403
        );
        assert_eq!(
            http(
                a,
                &format!(
                    "GET /room/me HTTP/1.1\r\nHost: {a}\r\nSec-Fetch-Site: cross-site\r\n\r\n"
                )
            )
            .0,
            403
        );
        for m in ["PUT", "DELETE", "OPTIONS", "PATCH", "HEAD"] {
            assert_eq!(
                http(a, &format!("{m} /room/post HTTP/1.1\r\nHost: {a}\r\n\r\n")).0,
                405,
                "{m}"
            );
        }
        assert_eq!(http(a, &format!("POST /room/post HTTP/1.1\r\nHost: {a}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}")).0, 415, "form posts (CSRF) are refused");
        assert_eq!(http(a, &format!("POST /room/post HTTP/1.1\r\nHost: {a}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1)).0, 413);
        assert_eq!(http(a, &format!("POST /room/post HTTP/1.1\r\nHost: {a}\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\nxyz")).0, 400);
        // Guests see nothing but the join page.
        assert_eq!(get(a, "/room/rooms", "").0, 403);
        assert_eq!(get(a, "/room/messages?room=family", "").0, 403);
        assert_eq!(get(a, "/room/events", "").0, 403);
        assert_eq!(
            post(
                a,
                "/room/post",
                "",
                &json!({"room": "family", "text": "hi"})
            )
            .0,
            403
        );
        assert_eq!(post(a, "/room/create", "", &json!({"name": "x"})).0, 403);
        assert_eq!(get(a, "/room/me", "").1["role"], "guest");
        assert_eq!(get(a, "/room/nope", "").0, 404);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn owner_invites_members_join_and_talk() {
        let agent = FakeAgent::start(vec![Script::Answer("Hi Mia!".into())]);
        let (hub, d) = hub_at(&agent.url(), "srv-flow", json!({"owner_name": "Riley"}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let link = hub.lock().new_owner_link().unwrap();
        // Owner login: the right link, from loopback, sets an HttpOnly SameSite=Strict cookie.
        let (c, _, _) = post(a, "/room/owner", "", &json!({"token": "nope"}));
        assert_eq!(c, 403);
        let (c, head, _) = post(a, "/room/owner", "", &json!({"token": link}));
        assert_eq!(c, 200);
        let sc = set_cookie(&head, "tr_owner").unwrap();
        assert!(
            sc.contains("HttpOnly") && sc.contains("SameSite=Strict") && sc.contains("Path=/"),
            "{sc}"
        );
        let owner = sc.split(';').next().unwrap().to_string();
        assert_ne!(
            owner,
            format!("tr_owner={link}"),
            "the cookie is not the link"
        );
        assert_eq!(get(a, "/room/me", &owner).1["role"], "owner");
        let (c, _, made) = post(
            a,
            "/room/create",
            &owner,
            &json!({"name": "Team Room", "personality": "playful"}),
        );
        assert_eq!((c, made["room"]["id"].as_str()), (200, Some("team-room")));
        let (c, _, inv) = post(
            a,
            "/room/invite",
            &owner,
            &json!({"room": "family", "hours": 2}),
        );
        assert_eq!(c, 200);
        let code = inv["code"].as_str().unwrap().to_string();
        assert!(inv["link"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/#join={code}")));
        assert_eq!(inv["local_only"], true);
        // A guest joins with the code and a name.
        let (c, head, j) = post(a, "/room/join", "", &json!({"code": code, "name": "Mia"}));
        assert_eq!(c, 200, "{j}");
        let sc = set_cookie(&head, "tr_member").unwrap();
        assert!(
            sc.contains("HttpOnly") && sc.contains("SameSite=Strict"),
            "{sc}"
        );
        let mia = sc.split(';').next().unwrap().to_string();
        let (c, _, again) = post(a, "/room/join", "", &json!({"code": code, "name": "Eve"}));
        assert_eq!(c, 410, "one-time: {again}");
        let (c, _, _) = post(
            a,
            "/room/join",
            "",
            &json!({"code": "ZZZZZ-ZZZZZ", "name": "Eve"}),
        );
        assert_eq!(c, 404);
        let (_, me) = get(a, "/room/me", &mia);
        assert_eq!(
            (me["role"].as_str(), me["name"].as_str()),
            (Some("member"), Some("Mia"))
        );
        let (_, rooms) = get(a, "/room/rooms", &mia);
        let ids: Vec<&str> = rooms["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["family"], "members see only their rooms");
        assert_eq!(get(a, "/room/messages?room=team-room", &mia).0, 403);
        assert_eq!(
            post(a, "/room/create", &mia, &json!({"name": "Mine"})).0,
            403
        );
        assert_eq!(
            post(a, "/room/invite", &mia, &json!({"room": "family"})).0,
            403
        );
        // Mia posts and asks Tempo; Tempo answers in the thread.
        let (c, _, p) = post(
            a,
            "/room/post",
            &mia,
            &json!({"room": "family", "text": "@tempo hello!"}),
        );
        assert_eq!(c, 200, "{p}");
        let qid = p["msg"]["id"].as_u64().unwrap();
        let t0 = Instant::now();
        let answer = loop {
            let (_, m) = get(a, "/room/messages?room=family", &mia);
            if let Some(x) = m["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["kind"] == "tempo" && x["reply_to"] == qid)
            {
                break x.clone();
            }
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(answer["text"], "Hi Mia!");
        assert_eq!(answer["reply_name"], "Mia");
        // Members cannot post as Tempo; the owner can (MCP and the CLI use this).
        let (c, _, _) = post(
            a,
            "/room/post",
            &mia,
            &json!({"room": "family", "text": "I am Tempo", "as_tempo": true}),
        );
        assert_eq!(c, 403);
        let (c, _, p) = post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "Dinner is at 7.", "as_tempo": true, "via": "mcp"}),
        );
        assert_eq!(
            (c, p["msg"]["kind"].as_str(), p["msg"]["via"].as_str()),
            (200, Some("tempo"), Some("mcp"))
        );
        // A cross-origin post carrying Mia's cookie is still refused.
        let b = json!({"room": "family", "text": "csrf"}).to_string();
        let (c, _, _) = http(a, &format!("POST /room/post HTTP/1.1\r\nHost: {a}\r\nOrigin: http://evil.example\r\nCookie: {mia}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len()));
        assert_eq!(c, 403);
        // Mute, then remove: the record goes to trash and the cookie stops working.
        let (_, ms) = get(a, "/room/members?room=family", &owner);
        let mid = ms["members"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(
            get(a, "/room/members?room=family", &mia).1["members"][0].get("id"),
            None,
            "members see names only"
        );
        assert_eq!(
            post(
                a,
                "/room/mute",
                &owner,
                &json!({"room": "family", "member": mid, "muted": true})
            )
            .0,
            200
        );
        assert_eq!(
            post(
                a,
                "/room/post",
                &mia,
                &json!({"room": "family", "text": "can I talk?"})
            )
            .0,
            403
        );
        assert_eq!(
            post(
                a,
                "/room/mute",
                &owner,
                &json!({"room": "family", "member": mid, "muted": false})
            )
            .0,
            200
        );
        assert_eq!(
            post(
                a,
                "/room/post",
                &mia,
                &json!({"room": "family", "text": "back"})
            )
            .0,
            200
        );
        assert_eq!(
            post(a, "/room/remove", &owner, &json!({"member": mid})).0,
            200
        );
        assert_eq!(get(a, "/room/me", &mia).1["role"], "guest");
        assert_eq!(
            post(
                a,
                "/room/post",
                &mia,
                &json!({"room": "family", "text": "still here?"})
            )
            .0,
            403
        );
        let trash: Vec<_> = std::fs::read_dir(d.join("trash"))
            .unwrap()
            .flatten()
            .collect();
        assert!(trash
            .iter()
            .any(|e| std::fs::read_to_string(e.path()).unwrap().contains(&mid)));
        // Settings and logout.
        let (c, _, s) = post(
            a,
            "/room/settings",
            &owner,
            &json!({"owner_name": "Riley B"}),
        );
        assert_eq!((c, s["owner_name"].as_str()), (200, Some("Riley B")));
        let (_, head, _) = post(a, "/room/logout", &owner, &json!({}));
        assert!(set_cookie(&head, "tr_owner").unwrap().contains("Max-Age=0"));
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn a_public_url_is_allowed_and_used_for_invites() {
        let (hub, _) = hub_at(&dead_url(), "srv-public", json!({}));
        assert!(public_parts("laptop.ts.net").is_err());
        assert!(public_parts("https://x@laptop.ts.net").is_err());
        assert!(public_parts("https://laptop.ts.net/room").is_err());
        let r = start(
            hub.clone(),
            ServeOpts {
                port: 0,
                public_url: Some("https://Laptop.tail1234.ts.net/".into()),
                ..ServeOpts::default()
            },
        )
        .unwrap();
        let a = r.addr;
        let (c, _, _) = http(a, "GET /room/health HTTP/1.1\r\nHost: laptop.tail1234.ts.net\r\nOrigin: https://laptop.tail1234.ts.net\r\n\r\n");
        assert_eq!(c, 200, "the proxy's host and HTTPS origin are ours");
        let token = hub.lock().new_owner_session(unix_now()).unwrap();
        let (_, _, inv) = post(
            a,
            "/room/invite",
            &format!("tr_owner={token}"),
            &json!({"room": "family"}),
        );
        let code = inv["code"].as_str().unwrap();
        assert_eq!(
            inv["link"],
            format!("https://laptop.tail1234.ts.net/#join={code}")
        );
        assert_eq!(inv["local_only"], false);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn owner_needs_loopback_and_the_cookie() {
        let (hub, _) = hub_at(&dead_url(), "srv-viewer", json!({}));
        let token = hub.lock().new_owner_session(unix_now()).unwrap();
        let sh = Shared {
            hub: hub.clone(),
            allowed: HashSet::new(),
            public: None,
            port: 1,
            conns: AtomicUsize::new(0),
            streams: AtomicUsize::new(0),
            streams_by: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            webhook_only: false,
            seen_wa: Mutex::new(whatsapp::Seen::load(None)),
        };
        let req = |loopback: bool, cookie: &str| Req {
            method: "GET".into(),
            path: "/room/me".into(),
            query: HashMap::new(),
            headers: vec![("cookie".into(), cookie.into())],
            body: vec![],
            peer_loopback: loopback,
            peer: "203.0.113.2".into(),
        };
        assert_eq!(
            viewer(&sh, &req(true, &format!("a=b; tr_owner={token}"))),
            Viewer::Owner
        );
        assert_eq!(
            viewer(&sh, &req(false, &format!("tr_owner={token}"))),
            Viewer::Guest,
            "not from another machine"
        );
        assert_eq!(viewer(&sh, &req(true, "tr_owner=guess")), Viewer::Guest);
        assert_eq!(viewer(&sh, &req(true, "")), Viewer::Guest);
        hub.shutdown();
    }

    #[test]
    fn a_client_sending_its_request_a_byte_at_a_time_is_cut_off() {
        let (hub, _) = hub_at(&dead_url(), "srv-slow", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let mut s = TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let t0 = Instant::now();
        s.write_all(format!("GET /room/health HTTP/1.1\r\nHost: {a}\r\n").as_bytes())
            .unwrap();
        // one header byte every 200 ms: each read is quick, the request never ends
        let (mut got, mut cut) = (Vec::new(), false);
        let mut buf = [0u8; 1024];
        while t0.elapsed() < Duration::from_secs(8) {
            if s.write_all(b"x").is_err() {
                cut = true;
                break;
            }
            match s.read(&mut buf) {
                Ok(n) => {
                    got.extend_from_slice(&buf[..n]);
                    cut = true;
                    break;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(150))
                }
                Err(_) => {
                    cut = true; // reset: the server closed while bytes were still coming
                    break;
                }
            }
        }
        let took = t0.elapsed();
        let got = String::from_utf8_lossy(&got).to_string();
        assert!(cut, "the server never cut the connection");
        assert!(
            took < REQUEST_DEADLINE + Duration::from_secs(2),
            "held for {took:?}"
        );
        assert!(got.is_empty() || got.starts_with("HTTP/1.1 408"), "{got}");
        // the server still answers everyone else
        assert_eq!(get(a, "/room/health", "").0, 200);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn a_removed_member_stops_getting_live_events_within_seconds() {
        let (hub, _) = hub_at(&dead_url(), "srv-sse-remove", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let token = hub.lock().new_owner_session(unix_now()).unwrap();
        let owner = format!("tr_owner={token}");
        let (_, _, inv) = post(a, "/room/invite", &owner, &json!({"room": "family"}));
        let (_, head, _) = post(
            a,
            "/room/join",
            "",
            &json!({"code": inv["code"], "name": "Lee"}),
        );
        let lee = set_cookie(&head, "tr_member")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let t = std::thread::spawn(move || {
            let mut s = TcpStream::connect(a).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            s.write_all(
                format!("GET /room/events HTTP/1.1\r\nHost: {a}\r\nCookie: {lee}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
            let mut buf = vec![0u8; 4096];
            let mut got = String::new();
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
                }
            }
            (Instant::now(), got)
        });
        std::thread::sleep(Duration::from_millis(500));
        let (_, ms) = get(a, "/room/members?room=family", &owner);
        let id = ms["members"][0]["id"].as_str().unwrap().to_string();
        let removed = Instant::now();
        assert_eq!(
            post(a, "/room/remove", &owner, &json!({"member": id})).0,
            200
        );
        // a quiet room: nothing else wakes the stream, and it still ends within 5 s
        let (closed, got) = t.join().unwrap();
        assert!(got.starts_with("HTTP/1.1 200 OK"), "{got}");
        let took = closed.duration_since(removed);
        assert!(
            took <= Duration::from_secs(5),
            "the stream stayed open {took:?}"
        );
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn live_events_reach_only_the_rooms_you_are_in() {
        let (hub, _) = hub_at(&dead_url(), "srv-sse", json!({}));
        hub.lock()
            .create_room("Secret", Mode::Mention, Personality::Plain)
            .unwrap();
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let token = hub.lock().new_owner_session(unix_now()).unwrap();
        let owner = format!("tr_owner={token}");
        let (_, _, inv) = post(a, "/room/invite", &owner, &json!({"room": "family"}));
        let (_, head, _) = post(
            a,
            "/room/join",
            "",
            &json!({"code": inv["code"], "name": "Sam"}),
        );
        let sam = set_cookie(&head, "tr_member")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let since = hub.seq();
        let sam2 = sam.clone();
        let t = std::thread::spawn(move || {
            let mut s = TcpStream::connect(a).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            s.write_all(format!("GET /room/events?since={since} HTTP/1.1\r\nHost: {a}\r\nCookie: {sam2}\r\n\r\n").as_bytes()).unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = String::new();
            let until = Instant::now() + Duration::from_secs(10);
            while !got.contains("for the family") && Instant::now() < until {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
                }
            }
            got
        });
        std::thread::sleep(Duration::from_millis(300));
        post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "secret", "text": "for the owner only"}),
        );
        post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "for the family"}),
        );
        let got = t.join().unwrap();
        assert!(
            got.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream"),
            "{got}"
        );
        assert!(
            got.contains("event: message") && got.contains("for the family"),
            "{got}"
        );
        assert!(!got.contains("for the owner only"), "{got}");
        // A viewer coming back with an id from an earlier run is told to reload.
        let mut s = TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(format!("GET /room/events HTTP/1.1\r\nHost: {a}\r\nCookie: {sam}\r\nLast-Event-ID: 57\r\n\r\n").as_bytes()).unwrap();
        let mut buf = vec![0u8; 4096];
        let mut got = String::new();
        while !got.contains("event: reset") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "{got}");
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            got.contains(&format!("id: {}\nevent: reset", hub.seq())),
            "{got}"
        );
        r.stop();
        hub.shutdown();
    }

    /// A request with extra header lines (the way a reverse proxy adds its own).
    fn with_headers(
        a: SocketAddr,
        method: &str,
        path: &str,
        extra: &str,
        body: &str,
    ) -> (u16, String, String) {
        let ct = if method == "POST" {
            format!(
                "Content-Type: application/json\r\nOrigin: http://{a}\r\nContent-Length: {}\r\n",
                body.len()
            )
        } else {
            String::new()
        };
        http(
            a,
            &format!("{method} {path} HTTP/1.1\r\nHost: {a}\r\n{ct}{extra}\r\n{body}"),
        )
    }

    #[test]
    fn the_owner_cookie_is_a_session_never_the_owner_link() {
        let (hub, d) = hub_at(&dead_url(), "srv-session", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let link = hub.lock().new_owner_link().unwrap();
        // Only a hash of the link is on disk, and it is not in the config in the clear.
        let cfg = std::fs::read_to_string(d.join("config.json")).unwrap();
        assert!(
            !cfg.contains(&link) && !cfg.contains("owner_token"),
            "{cfg}"
        );
        assert!(cfg.contains(&crate::util::sha256_hex(&link)));
        // Trading the link: the cookie is a different random value.
        let (c, head, _) = post(a, "/room/owner", "", &json!({"token": link}));
        assert_eq!(c, 200);
        let cookie = set_cookie(&head, "tr_owner").unwrap();
        let session = cookie.split(';').next().unwrap().to_string();
        assert_ne!(session, format!("tr_owner={link}"));
        assert_eq!(get(a, "/room/me", &session).1["role"], "owner");
        // The link was used up; offering it again, or as the cookie, gets nothing.
        assert_eq!(post(a, "/room/owner", "", &json!({"token": link})).0, 403);
        assert_eq!(
            get(a, "/room/me", &format!("tr_owner={link}")).1["role"],
            "guest"
        );
        // The session is stored only as a hash.
        let on_disk = std::fs::read_to_string(d.join("owner-sessions.json")).unwrap();
        let value = session.trim_start_matches("tr_owner=");
        assert!(!on_disk.contains(value) && on_disk.contains(&crate::util::sha256_hex(value)));
        // Sign out ends it on the server: the cookie in hand no longer works.
        let (c, _, _) = post(a, "/room/logout", &session, &json!({}));
        assert_eq!(c, 200);
        assert_eq!(
            get(a, "/room/me", &session).1["role"],
            "guest",
            "logout revokes"
        );
        // A session that has expired stops working too.
        let t = hub
            .lock()
            .new_owner_session(unix_now() - 40.0 * 86_400.0)
            .unwrap();
        assert_eq!(
            get(a, "/room/me", &format!("tr_owner={t}")).1["role"],
            "guest"
        );
        // A new link replaces an unused one.
        let one = hub.lock().new_owner_link().unwrap();
        let two = hub.lock().new_owner_link().unwrap();
        assert_eq!(post(a, "/room/owner", "", &json!({"token": one})).0, 403);
        assert_eq!(post(a, "/room/owner", "", &json!({"token": two})).0, 200);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn a_remote_member_can_never_be_the_owner_because_the_peer_is_loopback() {
        // Behind `tailscale serve` every request comes from 127.0.0.1; the proxy adds its own
        // headers. Even a stolen owner session or the control key does nothing through it.
        let (hub, _) = hub_at(&dead_url(), "srv-proxy", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let key = "k".repeat(64);
        hub.set_control_key(&key);
        let session = format!(
            "tr_owner={}",
            hub.lock().new_owner_session(unix_now()).unwrap()
        );
        let me = |extra: &str| -> Value {
            serde_json::from_str(&with_headers(a, "GET", "/room/me", extra, "").2).unwrap()
        };
        // At the machine (no proxy header): the session and the control key both work.
        assert_eq!(me(&format!("Cookie: {session}\r\n"))["role"], "owner");
        assert_eq!(me(&format!("X-Tempo-Control: {key}\r\n"))["role"], "owner");
        assert_eq!(me("X-Tempo-Control: wrong\r\n")["role"], "guest");
        assert_eq!(me("X-Tempo-Control: \r\n")["role"], "guest");
        for proxy in [
            "X-Forwarded-For: 203.0.113.9\r\n",
            "Forwarded: for=203.0.113.9\r\n",
            "X-Real-Ip: 203.0.113.9\r\n",
            "X-Forwarded-Proto: https\r\n",
            "Tailscale-User-Login: mia@example.com\r\n",
        ] {
            let v = me(&format!("Cookie: {session}\r\n{proxy}"));
            assert_eq!(v["role"], "guest", "{proxy}");
            assert_eq!(v["loopback"], false, "{proxy}");
            assert_eq!(
                me(&format!("X-Tempo-Control: {key}\r\n{proxy}"))["role"],
                "guest",
                "{proxy}"
            );
        }
        // The owner sign-in is closed to proxied requests, and they do not use up the tries.
        let link = hub.lock().new_owner_link().unwrap();
        let body = json!({"token": link}).to_string();
        for _ in 0..25 {
            let (c, _, _) = with_headers(
                a,
                "POST",
                "/room/owner",
                "X-Forwarded-For: 203.0.113.9\r\n",
                &body,
            );
            assert_eq!(c, 403);
        }
        let (c, _, _) = with_headers(a, "POST", "/room/owner", "", &body);
        assert_eq!(
            c, 200,
            "the real owner can still sign in, and the link was not used up"
        );
        // Owner-only actions through the proxy are refused even with the owner's own cookie.
        let (c, _, _) = with_headers(
            a,
            "POST",
            "/room/create",
            &format!("Cookie: {session}\r\nX-Forwarded-For: 203.0.113.9\r\n"),
            &json!({"name": "Nope"}).to_string(),
        );
        assert_eq!(c, 403);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn member_cookies_are_secure_behind_https_and_tries_count_per_visitor() {
        let (hub, _) = hub_at(
            &dead_url(),
            "srv-secure",
            json!({"limits": {"join_per_ip": 2}}),
        );
        let r = start(
            hub.clone(),
            ServeOpts {
                port: 0,
                public_url: Some("https://laptop.tail1234.ts.net".into()),
                ..ServeOpts::default()
            },
        )
        .unwrap();
        let a = r.addr;
        let (code, _) = hub.lock().new_invite("family", None).unwrap();
        let body = json!({"code": code, "name": "Mia"}).to_string();
        let (c, head, _) = with_headers(
            a,
            "POST",
            "/room/join",
            "X-Forwarded-For: 203.0.113.1\r\n",
            &body,
        );
        assert_eq!(c, 200);
        let sc = set_cookie(&head, "tr_member").unwrap();
        assert!(
            sc.contains("Secure") && sc.contains("HttpOnly") && sc.contains("SameSite=Strict"),
            "{sc}"
        );
        // One visitor using up their tries does not lock the others out (all arrive on loopback).
        let wrong = json!({"code": "ZZZZZ-ZZZZZ", "name": "Eve"}).to_string();
        for _ in 0..2 {
            assert_eq!(
                with_headers(
                    a,
                    "POST",
                    "/room/join",
                    "X-Forwarded-For: 203.0.113.2\r\n",
                    &wrong
                )
                .0,
                404
            );
        }
        assert_eq!(
            with_headers(
                a,
                "POST",
                "/room/join",
                "X-Forwarded-For: 203.0.113.2\r\n",
                &wrong
            )
            .0,
            429
        );
        assert_eq!(
            with_headers(
                a,
                "POST",
                "/room/join",
                "X-Forwarded-For: 203.0.113.3\r\n",
                &wrong
            )
            .0,
            404
        );
        // A visitor cannot pick their own bucket by sending a different first entry.
        assert_eq!(
            with_headers(
                a,
                "POST",
                "/room/join",
                "X-Forwarded-For: 1.2.3.4, 203.0.113.2\r\n",
                &wrong
            )
            .0,
            429
        );
        // Without https the cookie is not marked Secure (it would never be sent back).
        let (hub2, _) = hub_at(&dead_url(), "srv-plain", json!({}));
        let r2 = start(hub2.clone(), opts()).unwrap();
        let (code2, _) = hub2.lock().new_invite("family", None).unwrap();
        let (_, head2, _) = post(
            r2.addr,
            "/room/join",
            "",
            &json!({"code": code2, "name": "Ann"}),
        );
        assert!(!set_cookie(&head2, "tr_member").unwrap().contains("Secure"));
        r.stop();
        r2.stop();
        hub.shutdown();
        hub2.shutdown();
    }

    #[test]
    fn same_site_but_not_same_origin_requests_are_refused() {
        let (hub, _) = hub_at(&dead_url(), "srv-samesite", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        for site in ["same-site", "cross-site"] {
            let (c, _, _) = http(
                a,
                &format!("GET /room/me HTTP/1.1\r\nHost: {a}\r\nSec-Fetch-Site: {site}\r\n\r\n"),
            );
            assert_eq!(c, 403, "{site}");
        }
        for site in ["same-origin", "none"] {
            let (c, _, _) = http(
                a,
                &format!("GET /room/me HTTP/1.1\r\nHost: {a}\r\nSec-Fetch-Site: {site}\r\n\r\n"),
            );
            assert_eq!(c, 200, "{site}");
        }
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn one_person_cannot_use_up_all_the_live_streams() {
        let (hub, _) = hub_at(&dead_url(), "srv-streams", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let (code, _) = hub.lock().new_invite("family", None).unwrap();
        let (_, head, _) = post(a, "/room/join", "", &json!({"code": code, "name": "Sam"}));
        let sam = set_cookie(&head, "tr_member")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let mut open = vec![];
        for i in 0..MAX_STREAMS_EACH {
            let mut s = TcpStream::connect(a).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.write_all(
                format!("GET /room/events HTTP/1.1\r\nHost: {a}\r\nCookie: {sam}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap();
            assert!(
                String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
                "stream {i}"
            );
            open.push(s);
        }
        let (c, _, _) = http(
            a,
            &format!("GET /room/events HTTP/1.1\r\nHost: {a}\r\nCookie: {sam}\r\n\r\n"),
        );
        assert_eq!(c, 503, "one more than a person may hold");
        // Someone else still gets one.
        let owner = format!(
            "tr_owner={}",
            hub.lock().new_owner_session(unix_now()).unwrap()
        );
        let mut s = TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(
            format!("GET /room/events HTTP/1.1\r\nHost: {a}\r\nCookie: {owner}\r\n\r\n").as_bytes(),
        )
        .unwrap();
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
        // Closing one frees a place (the server notices at its next write or check).
        drop(open);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn the_app_has_no_inline_script_and_builds_messages_from_text_nodes() {
        // The CSP (script-src 'self', no 'unsafe-inline') only helps if the page has none.
        let html = INDEX_HTML.to_lowercase();
        assert!(!html.contains("javascript:"));
        for tag in html.split("<script").skip(1) {
            let head = tag.split('>').next().unwrap_or("");
            assert!(head.contains("src="), "an inline <script>: {head}");
        }
        assert!(!html.contains("<style") && !html.contains(" style="));
        for attr in [
            " onclick=",
            " onload=",
            " onerror=",
            " onsubmit=",
            " onfocus=",
        ] {
            assert!(!html.contains(attr), "{attr}");
        }
        for sink in [
            "innerhtml",
            "outerhtml",
            "insertadjacenthtml",
            "document.write",
            "eval(",
            "new function",
            "srcdoc",
            "createcontextualfragment",
        ] {
            // the one mention in app.mjs is its own comment saying it never uses innerHTML
            let hits = APP_JS
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.to_lowercase().contains(sink))
                .count();
            assert_eq!(hits, 0, "app.mjs uses {sink}");
        }
        assert!(APP_CSP.contains("script-src 'self'") && !APP_CSP.contains("unsafe-inline"));
        assert!(
            APP_CSP.contains("connect-src 'self'") && APP_CSP.contains("frame-ancestors 'none'")
        );
    }

    // ---- devices: sign-in links, sign-out, idle expiry (SR-16)

    fn cookie_pair(head: &str) -> String {
        set_cookie(head, "tr_member")
            .expect("a member cookie")
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    /// Owner session cookie for a started server.
    fn owner_cookie(hub: &Hub) -> String {
        format!(
            "tr_owner={}",
            hub.lock().new_owner_session(unix_now()).unwrap()
        )
    }

    /// The owner invites `name` to `room`; returns the member's cookie and id.
    fn join_as(hub: &Hub, a: SocketAddr, owner: &str, room: &str, name: &str) -> (String, String) {
        let (c, _, inv) = post(a, "/room/invite", owner, &json!({"room": room}));
        assert_eq!(c, 200);
        let (c, head, _) = post(
            a,
            "/room/join",
            "",
            &json!({"code": inv["code"], "name": name}),
        );
        assert_eq!(c, 200);
        let id = hub
            .lock()
            .members
            .iter()
            .find(|m| m.name == name)
            .unwrap()
            .id
            .clone();
        (cookie_pair(&head), id)
    }

    fn device_link(a: SocketAddr, owner: &str, member: &str) -> Value {
        let (c, _, j) = post(a, "/room/invite", owner, &json!({"member": member}));
        assert_eq!(c, 200, "{j}");
        j
    }

    #[test]
    fn a_second_device_signs_in_with_a_link_and_sees_the_same_rooms() {
        let (hub, d) = hub_at(&dead_url(), "srv-device", json!({}));
        hub.lock()
            .create_room("Team", Mode::Mention, Personality::Plain)
            .unwrap();
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (mia1, mid) = join_as(&hub, a, &owner, "family", "Mia");
        // the same person (with her cookie) gets a second room, as before
        let (c, _, inv) = post(a, "/room/invite", &owner, &json!({"room": "team"}));
        assert_eq!(c, 200);
        let (c, head, j) = post(a, "/room/join", &mia1, &json!({"code": inv["code"]}));
        assert_eq!((c, j["member"].as_str()), (200, Some(mid.as_str())), "{j}");
        assert!(set_cookie(&head, "tr_member").is_none(), "no second cookie");
        let rooms_of = |cookie: &str| -> Vec<String> {
            let (_, j) = get(a, "/room/rooms", cookie);
            let mut v: Vec<String> = j["rooms"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect();
            v.sort();
            v
        };
        assert_eq!(rooms_of(&mia1), ["family", "team"]);

        // The owner makes a sign-in link for Mia.
        let msgs_before = hub.lock().room("family").unwrap().tail.len();
        let j = device_link(a, &owner, &mid);
        let code = j["code"].as_str().unwrap().to_string();
        assert!(
            j["link"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/#signin={code}")),
            "{j}"
        );
        assert_eq!(
            (j["member"].as_str(), j["name"].as_str()),
            (Some(mid.as_str()), Some("Mia"))
        );
        let hours = (j["expires"].as_f64().unwrap() - unix_now()) / 3600.0;
        assert!(
            (47.9..=48.1).contains(&hours),
            "invite_hours by default: {hours}"
        );
        let invites = std::fs::read_to_string(d.join("invites.json")).unwrap();
        assert!(!invites.contains(&code), "stored as a hash");

        // Device 2: no name, no cookie. It becomes Mia, with the same rooms.
        let (c, head, j2) = post(a, "/room/join", "", &json!({"code": code}));
        assert_eq!(c, 200, "{j2}");
        assert_eq!(
            (j2["device"].as_bool(), j2["member"].as_str()),
            (Some(true), Some(mid.as_str()))
        );
        let sc = set_cookie(&head, "tr_member").unwrap();
        assert!(
            sc.contains("HttpOnly") && sc.contains("SameSite=Strict") && sc.contains("Path=/"),
            "{sc}"
        );
        let mia2 = cookie_pair(&head);
        assert_ne!(mia1, mia2);
        let (_, me2) = get(a, "/room/me", &mia2);
        assert_eq!(
            (
                me2["role"].as_str(),
                me2["id"].as_str(),
                me2["name"].as_str()
            ),
            (Some("member"), Some(mid.as_str()), Some("Mia"))
        );
        assert_eq!(rooms_of(&mia2), rooms_of(&mia1));
        assert_eq!(
            get(a, "/room/me", &mia1).1["role"],
            "member",
            "device 1 still works"
        );
        // Mia can post from the new device.
        let (c, _, _) = post(
            a,
            "/room/post",
            &mia2,
            &json!({"room": "family", "text": "hi from my phone"}),
        );
        assert_eq!(c, 200);
        // nothing else changed: still one Mia, her rooms as they were, no "joined" note
        {
            let st = hub.lock();
            assert_eq!(st.members.len(), 1);
            assert_eq!(st.members[0].rooms.len(), 2);
            assert_eq!(st.members[0].sessions.len(), 2);
            assert_eq!(
                st.room("family").unwrap().tail.len(),
                msgs_before + 1,
                "only her own post"
            );
        }
        let file = std::fs::read_to_string(d.join("members.json")).unwrap();
        for c in [&mia1, &mia2] {
            assert!(
                !file.contains(c.split('=').nth(1).unwrap()),
                "only hashes on disk"
            );
        }
        // one time
        let (c, head, _) = post(a, "/room/join", "", &json!({"code": code}));
        assert_eq!(c, 410);
        assert!(set_cookie(&head, "tr_member").is_none());
        // the owner sees how many devices
        let (_, ms) = get(a, "/room/members?room=family", &owner);
        assert_eq!(ms["members"][0]["devices"], 2);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn signing_out_ends_this_device_only() {
        let (hub, _) = hub_at(&dead_url(), "srv-signout", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (mia1, mid) = join_as(&hub, a, &owner, "family", "Mia");
        let code = device_link(a, &owner, &mid)["code"].clone();
        let (_, head, _) = post(a, "/room/join", "", &json!({"code": code}));
        let mia2 = cookie_pair(&head);
        let (c, head, _) = post(a, "/room/logout", &mia1, &json!({}));
        assert_eq!(c, 200);
        let cleared = set_cookie(&head, "tr_member").unwrap();
        assert!(
            cleared.contains("Max-Age=0") && cleared.contains("HttpOnly"),
            "{cleared}"
        );
        // The old cookie is dead on the server, even with the cookie still in hand.
        assert_eq!(get(a, "/room/me", &mia1).1["role"], "guest");
        assert_eq!(get(a, "/room/rooms", &mia1).0, 403);
        assert_eq!(
            post(
                a,
                "/room/post",
                &mia1,
                &json!({"room": "family", "text": "x"})
            )
            .0,
            403
        );
        // Device 2 and the member record stay.
        assert_eq!(get(a, "/room/me", &mia2).1["role"], "member");
        assert_eq!(get(a, "/room/rooms", &mia2).0, 200);
        let (_, ms) = get(a, "/room/members?room=family", &owner);
        assert_eq!(ms["members"][0]["name"], "Mia");
        assert_eq!(ms["members"][0]["devices"], 1);
        // Signing out again, or with no cookie, is harmless.
        assert_eq!(post(a, "/room/logout", &mia1, &json!({})).0, 200);
        assert_eq!(post(a, "/room/logout", "", &json!({})).0, 200);
        assert_eq!(get(a, "/room/me", &mia2).1["role"], "member");
        // Mia can get device 1 back with a new link; removing her ends everything.
        let code = device_link(a, &owner, &mid)["code"].clone();
        let (c, head, _) = post(a, "/room/join", "", &json!({"code": code}));
        assert_eq!(c, 200);
        let mia3 = cookie_pair(&head);
        assert_eq!(
            post(a, "/room/remove", &owner, &json!({"member": mid})).0,
            200
        );
        for c in [&mia1, &mia2, &mia3] {
            assert_eq!(get(a, "/room/me", c).1["role"], "guest", "{c}");
        }
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn a_sign_in_link_cannot_be_misused() {
        let (hub, d) = hub_at(&dead_url(), "srv-device-misuse", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (mia, mia_id) = join_as(&hub, a, &owner, "family", "Mia");
        let (sam, sam_id) = join_as(&hub, a, &owner, "family", "Sam");
        let sam_rooms = hub.lock().member(&sam_id).unwrap().rooms.clone();

        // Only the owner, at the machine, can make one.
        for (who, cookie) in [("a member", &mia), ("a guest", &String::new())] {
            let (c, _, _) = post(a, "/room/invite", cookie, &json!({"member": mia_id}));
            assert_eq!(c, 403, "{who}");
        }
        let (c, _, _) = post(a, "/room/invite", &mia, &json!({"member": sam_id}));
        assert_eq!(c, 403, "a member cannot make one for someone else");
        let (c, _, _) = with_headers(
            a,
            "POST",
            "/room/invite",
            &format!("Cookie: {owner}\r\nX-Forwarded-For: 203.0.113.9\r\n"),
            &json!({"member": mia_id}).to_string(),
        );
        assert_eq!(c, 403, "behind a proxy the owner cookie is a guest");
        assert_eq!(
            post(a, "/room/invite", &owner, &json!({"member": "m_nobody"})).0,
            404
        );
        assert!(
            hub.lock().invites.iter().all(|i| i.member.is_none()),
            "no sign-in link was made"
        );

        // Wrong member: Mia's code, used by Sam (with his cookie), signs in Mia, never Sam.
        let code = device_link(a, &owner, &mia_id)["code"].clone();
        let (c, head, j) = post(
            a,
            "/room/join",
            &sam,
            &json!({"code": code, "name": "Sam", "member": sam_id}),
        );
        assert_eq!(
            (c, j["member"].as_str()),
            (200, Some(mia_id.as_str())),
            "{j}"
        );
        let got = cookie_pair(&head);
        assert_eq!(get(a, "/room/me", &got).1["id"], json!(mia_id));
        assert_eq!(
            get(a, "/room/me", &sam).1["id"],
            json!(sam_id),
            "Sam's own cookie is unchanged"
        );
        {
            let st = hub.lock();
            assert_eq!(st.member(&sam_id).unwrap().rooms, sam_rooms);
            assert_eq!(st.member(&sam_id).unwrap().sessions.len(), 1);
            assert_eq!(st.member(&mia_id).unwrap().sessions.len(), 2);
        }
        // Used twice.
        assert_eq!(post(a, "/room/join", "", &json!({"code": code})).0, 410);
        // Expired.
        let code = device_link(a, &owner, &mia_id)["code"].clone();
        for i in hub.lock().invites.iter_mut() {
            i.expires = unix_now() - 1.0;
        }
        let (c, head, _) = post(a, "/room/join", "", &json!({"code": code}));
        assert_eq!(c, 410);
        assert!(set_cookie(&head, "tr_member").is_none());
        // A made-up code.
        assert_eq!(
            post(a, "/room/join", "", &json!({"code": "ZZZZZ-ZZZZZ"})).0,
            404
        );
        // A removed member's pending link fails.
        let code = device_link(a, &owner, &sam_id)["code"].clone();
        assert_eq!(
            post(a, "/room/remove", &owner, &json!({"member": sam_id})).0,
            200
        );
        let (c, head, _) = post(a, "/room/join", "", &json!({"code": code}));
        assert_eq!(c, 404);
        assert!(set_cookie(&head, "tr_member").is_none());
        assert_eq!(
            post(a, "/room/invite", &owner, &json!({"member": sam_id})).0,
            404
        );
        assert_eq!(hub.lock().members.len(), 1, "nobody was made");
        let file = std::fs::read_to_string(d.join("members.json")).unwrap();
        assert!(!file.contains("Sam"));
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn join_tries_count_for_sign_in_codes_too() {
        let (hub, _) = hub_at(
            &dead_url(),
            "srv-device-limit",
            json!({"limits": {"join_per_ip": 3}}),
        );
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (_, mid) = join_as(&hub, a, &owner, "family", "Mia");
        let code = device_link(a, &owner, &mid)["code"].clone();
        // join_as used one try; two wrong codes use the rest, and the right one is turned away
        assert_eq!(
            post(a, "/room/join", "", &json!({"code": "AAAAA-AAAAA"})).0,
            404
        );
        assert_eq!(
            post(a, "/room/join", "", &json!({"code": "BBBBB-BBBBB"})).0,
            404
        );
        let (c, head, _) = post(a, "/room/join", "", &json!({"code": code}));
        assert_eq!(c, 429);
        assert!(set_cookie(&head, "tr_member").is_none());
        assert_eq!(hub.lock().member(&mid).unwrap().sessions.len(), 1);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn an_idle_device_stops_working_and_use_keeps_it_alive() {
        let (hub, d) = hub_at(&dead_url(), "srv-idle", json!({"session_days": 30}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (mia1, mid) = join_as(&hub, a, &owner, "family", "Mia");
        let code = device_link(a, &owner, &mid)["code"].clone();
        let (_, head, _) = post(a, "/room/join", "", &json!({"code": code}));
        let mia2 = cookie_pair(&head);
        // Device 1 was last used 31 days ago, device 2 25 days ago.
        {
            let mut st = hub.lock();
            let now = unix_now();
            let m = st.member_mut(&mid).unwrap();
            m.sessions[0].last_seen = now - 31.0 * 86_400.0;
            m.sessions[1].last_seen = now - 25.0 * 86_400.0;
        }
        assert_eq!(
            get(a, "/room/me", &mia1).1["role"],
            "guest",
            "idle past session_days"
        );
        assert_eq!(get(a, "/room/rooms", &mia1).0, 403);
        assert_eq!(get(a, "/room/me", &mia2).1["role"], "member");
        // Using device 2 wrote its time (it was over an hour old), so it is fresh again.
        let seen = hub.lock().member(&mid).unwrap().sessions[1].last_seen;
        assert!(unix_now() - seen < 60.0, "{seen}");
        let file: Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("members.json")).unwrap())
                .unwrap();
        assert!(
            unix_now()
                - file["members"][0]["sessions"][1]["last_seen"]
                    .as_f64()
                    .unwrap()
                < 60.0
        );
        // Ordinary use does not rewrite the file.
        std::fs::remove_file(d.join("members.json")).unwrap();
        for _ in 0..3 {
            assert_eq!(get(a, "/room/me", &mia2).1["role"], "member");
        }
        assert!(!d.join("members.json").exists());
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn the_app_explains_devices_in_plain_words() {
        // The sign-out wording and the owner's "New sign-in link" are in the app, and the
        // wording rules hold.
        assert!(APP_JS.contains("New sign-in link"));
        assert!(APP_JS.contains("this device only"));
        assert!(APP_JS.contains("#signin=") || APP_JS.contains("signin"));
        for bad in ["ledger", "claim"] {
            assert!(!APP_JS.to_lowercase().contains(bad), "{bad}");
            assert!(!INDEX_HTML.to_lowercase().contains(bad), "{bad}");
        }
    }
    // ---- teammates

    const OLD_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; manifest-src 'self'; object-src 'none'; frame-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

    #[test]
    fn the_owner_manages_the_team_over_http_and_members_see_only_who_is_here() {
        let ollama =
            crate::testutil::fake_ollama(&["mock:4b", "qwen3:4b"], |_| "Basil, of course.".into());
        let agent = FakeAgent::start(vec![]);
        let (hub, d) = hub_at(
            &agent.url(),
            "srv-team",
            json!({"owner_name": "Riley", "ollama_url": ollama.url}),
        );
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (mia, _) = join_as(&hub, a, &owner, "family", "Mia");
        assert_eq!(get(a, "/room/team", &owner).1["team"], json!([]));
        let (c, _, j) = post(
            a,
            "/room/team/add",
            &owner,
            &json!({"handle": "sage", "name": "Sage", "kind": "engine", "model": "mock:4b", "role": "Cooking coach, very kind.", "rooms": ["family"]}),
        );
        assert_eq!(c, 200, "{j}");
        assert_eq!(j["teammate"]["role"], "Cooking coach, very kind.");
        assert!(j["note"].is_null());
        // bad ones are refused with a reason
        for bad in [
            json!({"handle": "Tempo", "name": "Tempo", "kind": "agent"}),
            json!({"handle": "mia", "name": "Helper", "kind": "agent"}),
            json!({"handle": "helper", "name": "Helper", "kind": "engine"}),
            json!({"handle": "helper", "name": "Helper", "kind": "engine", "model": "absent:1b"}),
            json!({"handle": "sage", "name": "Again", "kind": "agent"}),
            json!({"handle": "helper", "name": "Helper", "kind": "agent", "rooms": ["nowhere"]}),
        ] {
            let (c, _, j) = post(a, "/room/team/add", &owner, &bad);
            assert!(c == 400 || c == 404, "{bad}: {c} {j}");
            assert!(j["error"].as_str().is_some_and(|e| !e.is_empty()));
        }
        let l = get(a, "/room/team", &owner).1;
        assert_eq!(l["team"].as_array().unwrap().len(), 1);
        assert_eq!(l["team"][0]["rooms"], json!(["family"]));
        let m = get(a, "/room/team/models", &owner).1;
        assert_eq!(m["ollama"], "up");
        assert_eq!(m["models"][0]["name"], "mock:4b");
        // a member sees who is in the room, never the role or the model
        let (c, mem) = get(a, "/room/members?room=family", &mia);
        assert_eq!(c, 200);
        assert_eq!(
            mem["team"],
            json!([{"handle": "sage", "name": "Sage", "kind": "engine"}])
        );
        assert!(!mem.to_string().contains("Cooking coach") && !mem.to_string().contains("mock:4b"));
        let rooms = get(a, "/room/rooms", &mia).1;
        assert_eq!(rooms["rooms"][0]["team"], json!(["sage"]));
        // Mia asks @sage and the answer is a teammate message
        let (c, _, j) = post(
            a,
            "/room/post",
            &mia,
            &json!({"room": "family", "text": "@sage a spice?"}),
        );
        assert_eq!(c, 200);
        let qid = j["msg"]["id"].as_u64().unwrap();
        let t0 = Instant::now();
        let ans = loop {
            let msgs = get(a, "/room/messages?room=family", &mia).1;
            if let Some(m) = msgs["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["kind"] == "teammate")
            {
                break m.clone();
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "no answer");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            (ans["name"].as_str(), ans["from"].as_str()),
            (Some("Sage"), Some("team:sage"))
        );
        assert_eq!(ans["reply_to"].as_u64(), Some(qid));
        // edit, out of the room and back in, remove
        let (c, _, j) = post(
            a,
            "/room/team/edit",
            &owner,
            &json!({"handle": "sage", "role": "Terse.", "name": "Sage the Wise"}),
        );
        assert_eq!(c, 200, "{j}");
        assert_eq!(
            (
                j["teammate"]["role"].as_str(),
                j["teammate"]["model"].as_str()
            ),
            (Some("Terse."), Some("mock:4b"))
        );
        assert_eq!(
            post(
                a,
                "/room/team/edit",
                &owner,
                &json!({"handle": "ghost", "role": "x"})
            )
            .0,
            404
        );
        assert_eq!(
            post(
                a,
                "/room/team/room",
                &owner,
                &json!({"room": "family", "handle": "sage", "on": false})
            )
            .0,
            200
        );
        assert_eq!(
            get(a, "/room/members?room=family", &mia).1["team"],
            json!([])
        );
        assert_eq!(
            post(
                a,
                "/room/team/room",
                &owner,
                &json!({"room": "family", "handle": "sage", "on": true})
            )
            .0,
            200
        );
        assert_eq!(
            get(a, "/room/members?room=family", &mia).1["team"][0]["name"],
            "Sage the Wise"
        );
        assert_eq!(
            post(
                a,
                "/room/team/room",
                &owner,
                &json!({"room": "family", "handle": "ghost", "on": true})
            )
            .0,
            404
        );
        assert_eq!(
            post(a, "/room/team/remove", &owner, &json!({"handle": "sage"})).0,
            200
        );
        assert_eq!(get(a, "/room/team", &owner).1["team"], json!([]));
        assert!(std::fs::read_dir(d.join("trash"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("teammate-sage")));
        // the owner cannot be renamed into a teammate's name
        post(
            a,
            "/room/team/add",
            &owner,
            &json!({"handle": "coder", "name": "Coder", "kind": "agent"}),
        );
        let (c, _, _) = post(a, "/room/settings", &owner, &json!({"owner_name": "Coder"}));
        assert_eq!(c, 400);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn only_the_owner_can_manage_the_team_or_see_the_models() {
        let ollama = crate::testutil::fake_ollama(&["mock:4b"], |_| "x".into());
        let agent = FakeAgent::start(vec![]);
        let (hub, _) = hub_at(
            &agent.url(),
            "srv-team-403",
            json!({"ollama_url": ollama.url}),
        );
        let key = "k".repeat(40);
        hub.set_control_key(&key);
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (mia, _) = join_as(&hub, a, &owner, "family", "Mia");
        post(
            a,
            "/room/team/add",
            &owner,
            &json!({"handle": "coder", "name": "Coder", "kind": "agent", "rooms": ["family"]}),
        );
        let before = hub.lock().team.clone();
        let add = json!({"handle": "sage", "name": "Sage", "kind": "agent"});
        let routes: Vec<(&str, &str, Value)> = vec![
            ("GET", "/room/team", Value::Null),
            ("GET", "/room/team/models", Value::Null),
            ("POST", "/room/team/add", add),
            (
                "POST",
                "/room/team/edit",
                json!({"handle": "coder", "role": "mine now"}),
            ),
            (
                "POST",
                "/room/team/room",
                json!({"room": "family", "handle": "coder", "on": false}),
            ),
            ("POST", "/room/team/remove", json!({"handle": "coder"})),
        ];
        let proxy = format!("Cookie: {owner}\r\nX-Forwarded-For: 203.0.113.9\r\n");
        let proxy_key = format!("X-Tempo-Control: {key}\r\nX-Real-Ip: 203.0.113.9\r\n");
        for (method, path, body) in &routes {
            let body = if body.is_null() {
                String::new()
            } else {
                body.to_string()
            };
            for (who, extra) in [
                ("a member", format!("Cookie: {mia}\r\n")),
                ("nobody", String::new()),
                ("a guess", "Cookie: tr_owner=guess\r\n".to_string()),
                ("the owner behind a proxy", proxy.clone()),
                ("the control key behind a proxy", proxy_key.clone()),
            ] {
                let (c, _, out) = with_headers(a, method, path, &extra, &body);
                assert_eq!(c, 403, "{who} {method} {path}: {out}");
            }
        }
        assert_eq!(hub.lock().team, before, "nothing changed");
        assert!(
            ollama.requests().is_empty(),
            "the models list was never fetched for them"
        );
        // the owner at the machine, by cookie or by the control key, can
        assert_eq!(get(a, "/room/team", &owner).0, 200);
        let (c, _, out) = with_headers(
            a,
            "GET",
            "/room/team/models",
            &format!("X-Tempo-Control: {key}\r\n"),
            "",
        );
        assert_eq!(c, 200, "{out}");
        assert!(!ollama.requests().is_empty());
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn the_csp_is_unchanged_and_the_app_speaks_of_teams_and_sharing() {
        assert_eq!(APP_CSP, OLD_CSP);
        let (hub, _) = hub_at(&dead_url(), "srv-csp", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let (c, head, _) = with_headers(r.addr, "GET", "/", "", "");
        assert_eq!(c, 200);
        assert!(
            head.contains(&format!("Content-Security-Policy: {OLD_CSP}")),
            "{head}"
        );
        for must in ["Team", "Share this room", "in this room", "installed"] {
            assert!(APP_JS.contains(must), "{must}");
        }
        assert!(INDEX_HTML.contains("Share"));
        let all = format!("{APP_JS}\n{INDEX_HTML}\n{APP_CSS}").to_lowercase();
        for bad in ["ledger", "claim"] {
            assert!(!all.contains(bad), "{bad}");
        }
        assert!(!all.contains("perennos tempo room tempo room"));
        r.stop();
        hub.shutdown();
    }

    // ---- owner-only rooms (Room phase 2b), over HTTP

    /// Read the live stream as `cookie` from `since` until `marker` shows up (or 12 s pass).
    fn sse_until(
        a: SocketAddr,
        cookie: &str,
        since: u64,
        marker: &'static str,
    ) -> std::thread::JoinHandle<String> {
        let cookie = cookie.to_string();
        std::thread::spawn(move || {
            let mut s = TcpStream::connect(a).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(12))).unwrap();
            s.write_all(format!("GET /room/events?since={since} HTTP/1.1\r\nHost: {a}\r\nCookie: {cookie}\r\n\r\n").as_bytes()).unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = String::new();
            let until = Instant::now() + Duration::from_secs(12);
            while !got.contains(marker) && Instant::now() < until {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.push_str(&String::from_utf8_lossy(&buf[..n])),
                }
            }
            got
        })
    }

    fn wait_for_tempo(a: SocketAddr, owner: &str, room: &str, after: u64) -> Value {
        let t0 = Instant::now();
        loop {
            let (_, j) = get(a, &format!("/room/messages?room={room}&limit=100"), owner);
            if let Some(m) = j["messages"].as_array().and_then(|l| {
                l.iter()
                    .find(|m| m["kind"] == "tempo" && m["reply_to"] == after)
            }) {
                return m.clone();
            }
            assert!(t0.elapsed() < Duration::from_secs(15), "no answer: {j}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn tools_on(a: SocketAddr, owner: &str, room: &str) -> (u16, Value) {
        let (c, _, j) = post(a, "/room/tools", owner, &json!({"room": room, "on": true}));
        (c, j)
    }

    fn tools_of(hub: &Hub, room: &str) -> bool {
        hub.lock().room(room).unwrap().tools
    }

    #[test]
    fn members_get_the_mic_only_where_the_owner_turned_it_on() {
        let (hub, _) = hub_at(&dead_url(), "srv-voice", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (mia, _) = join_as(&hub, a, &owner, "family", "Mia");
        let audio = |cookie: &str, len: usize| {
            let head = format!(
                "POST /room/voice/transcribe?room=family HTTP/1.1\r\nHost: {a}\r\nOrigin: http://{a}\r\nCookie: {cookie}\r\nContent-Type: audio/wav\r\nContent-Length: {len}\r\n\r\n"
            );
            http(a, &head)
        };
        // off for members by default: the page is told to hide the mic, and the audio is refused
        let (c, j) = get(a, "/room/voice?room=family", &mia);
        assert_eq!((c, &j["allowed"]), (200, &json!(false)));
        let (c, _, b) = audio(&mia, 0);
        assert_eq!(c, 403, "{b}");
        assert_eq!(audio("", 0).0, 403, "a stranger never");
        // only the owner at the machine can switch it
        let on = json!({"room": "family", "on": true});
        assert_eq!(post(a, "/room/voice/guests", &mia, &on).0, 403);
        assert_eq!(post(a, "/room/voice/guests", "", &on).0, 403);
        assert_eq!(post(a, "/room/voice/guests", &owner, &on).0, 200);
        // now a member gets through the gate (the agent here is not running, so it says so)
        let (c, _, b) = audio(&mia, 0);
        assert_eq!(c, 503, "{b}");
        assert!(b.contains("agent isn't answering"));
        // a recording over the cap is refused before it is read
        assert_eq!(audio(&mia, crate::voice::MAX_AUDIO + 1).0, 413);
        // off again
        let off = json!({"room": "family", "on": false});
        assert_eq!(post(a, "/room/voice/guests", &owner, &off).0, 200);
        assert_eq!(audio(&mia, 0).0, 403);
        // the page may use the microphone (and nothing else)
        let (_, head, _) = with_headers(a, "GET", "/", "", "");
        assert!(
            head.contains("microphone=(self)") && head.contains("camera=()"),
            "{head}"
        );
        let (c, _, js) = with_headers(a, "GET", "/voice.mjs", "", "");
        assert!(c == 200 && js.contains("mountVoice"));
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn only_the_owners_own_session_can_turn_tools_on_and_a_bridge_room_never_can() {
        let (hub, _) = hub_at(&dead_url(), "srv-tools", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        hub.set_control_key("the-control-key");
        // off by default, and the owner's room list says so
        let (_, rooms) = get(a, "/room/rooms", &owner);
        assert_eq!(rooms["rooms"][0]["tools"], false);
        assert_eq!(rooms["rooms"][0]["tools_active"], false);
        // a member (of this very room), a guest, a forged cookie, a proxied owner, the control
        // key alone (the CLI and MCP): 403 and nothing changes
        let (mia, _) = join_as(&hub, a, &owner, "family", "Mia");
        let body = json!({"room": "family", "on": true}).to_string();
        for (who, cookie, extra) in [
            ("member", mia.clone(), String::new()),
            ("guest", String::new(), String::new()),
            (
                "forged cookie",
                "tr_owner=forged".to_string(),
                String::new(),
            ),
            (
                "proxied owner",
                owner.clone(),
                "X-Forwarded-For: 203.0.113.9\r\n".to_string(),
            ),
            (
                "tailscale owner",
                owner.clone(),
                "Tailscale-User-Login: riley@example.com\r\n".to_string(),
            ),
            (
                "control key",
                String::new(),
                "X-Tempo-Control: the-control-key\r\n".to_string(),
            ),
            (
                "control key and a member cookie",
                mia.clone(),
                "X-Tempo-Control: the-control-key\r\n".to_string(),
            ),
        ] {
            let (c, _, out) = with_headers(
                a,
                "POST",
                "/room/tools",
                &format!("Cookie: {cookie}\r\n{extra}"),
                &body,
            );
            assert_eq!(c, 403, "{who}: {out}");
            assert!(!tools_of(&hub, "family"), "{who} changed the switch");
        }
        // a different route does not carry it either
        let (c, _, _) = post(
            a,
            "/room/update",
            &owner,
            &json!({"room": "family", "tools": true}),
        );
        assert_eq!(c, 200);
        assert!(!tools_of(&hub, "family"), "/room/update must not set tools");
        // the owner's own session can; with Mia in the room it is on but waiting
        let (c, j) = tools_on(a, &owner, "family");
        assert_eq!(c, 200, "{j}");
        assert_eq!(
            (&j["tools"], &j["tools_active"]),
            (&json!(true), &json!(false))
        );
        assert!(j["tools_note"].as_str().unwrap().contains("waiting"), "{j}");
        // the member sees no sign of it
        let (_, rooms) = get(a, "/room/rooms", &mia);
        assert!(
            rooms["rooms"][0].get("tools").is_none()
                && rooms["rooms"][0].get("tools_active").is_none()
        );
        r.stop();
        hub.shutdown();

        // a room a bridge reaches
        let fake = crate::testutil::FakeHttp::start(|_| (200, vec![], "{}".into()));
        let out = Outbound {
            discord: Some(crate::bridges::DiscordConf {
                token: "t".into(),
                token_from: "TEST".into(),
                channels: vec![("1".into(), "family".into())],
                api: fake.url.clone(),
                poll: Duration::from_secs(60),
            }),
            ..Default::default()
        };
        let (hub, _) = crate::hub::tests::hub_with(&dead_url(), "srv-tools-bridge", json!({}), out);
        let r = start(hub.clone(), opts()).unwrap();
        let owner = owner_cookie(&hub);
        let (c, _, j) = post(
            r.addr,
            "/room/tools",
            &owner,
            &json!({"room": "family", "on": true}),
        );
        assert_eq!(c, 409, "{j}");
        assert!(j["error"].as_str().unwrap().contains("never use tools"));
        assert!(!tools_of(&hub, "family"));
        let (_, rooms) = get(r.addr, "/room/rooms", &owner);
        assert_eq!(rooms["rooms"][0]["bridged"], true);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn a_member_is_never_served_what_was_said_in_owner_mode() {
        let agent = FakeAgent::start(vec![
            Script::Answer("PRIVATE-ANSWER about the gift".into()),
            Script::Answer("a guest-side answer".into()),
        ]);
        let (hub, d) = hub_at(&agent.url(), "srv-private", json!({"owner_name": "Riley"}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        hub.set_control_key("ck");
        let since0 = hub.seq();
        assert_eq!(tools_on(a, &owner, "family").0, 200);
        // the owner, alone, asks in owner mode
        let (c, _, j) = post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "@tempo PRIVATE-QUESTION about the gift"}),
        );
        assert_eq!(c, 200, "{j}");
        assert_eq!(j["msg"]["owner_only"], true);
        let q = j["msg"]["id"].as_u64().unwrap();
        let ans = wait_for_tempo(a, &owner, "family", q);
        assert_eq!(ans["owner_only"], true);
        assert_eq!(agent.mode_of(&agent.send_convs()[0]), "owner");
        // the owner sees both, in history and in the room list
        let (_, h) = get(a, "/room/messages?room=family&limit=100", &owner);
        assert!(
            h.to_string().contains("PRIVATE-QUESTION") && h.to_string().contains("PRIVATE-ANSWER")
        );
        // Sam joins afterwards (the invite alone already ends owner mode)
        let (sam, sam_id) = join_as(&hub, a, &owner, "family", "Sam");
        assert!(sam_id.starts_with("m_"));
        let (_, _, pub_msg) = post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "a public line for Sam"}),
        );
        let public_id = pub_msg["msg"]["id"].as_u64().unwrap();
        assert!(pub_msg["msg"].get("owner_only").is_none());
        // history, newest and paged both ways, and the room list: nothing private
        let big = public_id + 50;
        for path in [
            "/room/messages?room=family&limit=100".to_string(),
            "/room/messages?room=family&limit=1".to_string(),
            format!("/room/messages?room=family&before={big}&limit=100"),
            "/room/messages?room=family&since=0&limit=100".to_string(),
            "/room/rooms".to_string(),
        ] {
            let (c, j) = get(a, &path, &sam);
            assert_eq!(c, 200, "{path}");
            let text = j.to_string();
            for secret in ["PRIVATE", "gift", "owner_only", "Approval"] {
                assert!(!text.contains(secret), "{secret} in {path}: {text}");
            }
            assert!(
                text.contains("public line for Sam")
                    || path.contains("limit=1")
                    || path.contains("before"),
                "{path}: {text}"
            );
        }
        // nothing older to page to (the hidden messages do not count, so no "earlier" button)
        let (_, h) = get(a, "/room/messages?room=family&limit=100", &sam);
        assert_eq!(h["more"], false, "{h}");
        let (_, h) = get(a, "/room/messages?room=family&limit=100", &owner);
        assert_eq!(h["more"], false, "{h}");
        let (_, rooms) = get(a, "/room/rooms", &sam);
        assert_eq!(
            rooms["rooms"][0]["last"]["id"], public_id,
            "the preview is the public line"
        );
        assert_eq!(rooms["rooms"][0]["count"], public_id);
        // the owner's own session still has them (the room list included)
        let (_, rooms) = get(a, "/room/rooms", &owner);
        assert_eq!(rooms["rooms"][0]["count"], public_id);
        // the control key alone (the CLI and MCP) is the owner, but not the signed-in session
        let (c, _, body) = with_headers(
            a,
            "GET",
            "/room/messages?room=family&limit=100",
            "X-Tempo-Control: ck\r\n",
            "",
        );
        assert_eq!(c, 200);
        assert!(!body.contains("PRIVATE"), "{body}");
        // a reply to a private message quotes nothing of it
        let (_, _, rj) = post(
            a,
            "/room/post",
            &sam,
            &json!({"room": "family", "text": "what is that about?", "reply_to": ans["id"]}),
        );
        assert!(
            rj["msg"].get("reply_text").is_none() && rj["msg"].get("reply_to").is_none(),
            "{rj}"
        );
        // live: a stream that asks for everything since before the owner spoke is not sent it
        let t = sse_until(a, &sam, since0, "SAM-MARKER");
        std::thread::sleep(Duration::from_millis(200));
        post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "SAM-MARKER"}),
        );
        let got = t.join().unwrap();
        assert!(got.contains("SAM-MARKER"), "{got}");
        for secret in ["PRIVATE", "gift", "owner_only"] {
            assert!(!got.contains(secret), "{secret} in the stream: {got}");
        }
        // the owner's stream, from the same point, has them
        let t = sse_until(a, &owner, since0, "OWNER-MARKER");
        std::thread::sleep(Duration::from_millis(200));
        post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "OWNER-MARKER"}),
        );
        let got = t.join().unwrap();
        assert!(
            got.contains("PRIVATE-QUESTION") && got.contains("PRIVATE-ANSWER"),
            "{got}"
        );
        // Sam's own question to Tempo is a guest conversation and the prompt has none of it
        let (_, _, qj) = post(
            a,
            "/room/post",
            &sam,
            &json!({"room": "family", "text": "@tempo tldr"}),
        );
        wait_for_tempo(a, &owner, "family", qj["msg"]["id"].as_u64().unwrap());
        let prompt = agent.prompts().last().unwrap().clone();
        assert!(
            !prompt.contains("PRIVATE") && !prompt.contains("gift"),
            "{prompt}"
        );
        assert_eq!(agent.mode_of(agent.send_convs().last().unwrap()), "guest");
        // and the room's own file keeps it for the owner, flagged
        let file = std::fs::read_to_string(d.join("messages").join("family.jsonl")).unwrap();
        assert!(file.contains("\"owner_only\":true"));
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn a_stopped_owner_turn_is_told_to_the_owner_only_even_on_a_live_stream() {
        let agent = FakeAgent::start(vec![Script::Answer("PRIVATE-ANSWER".into())]);
        agent.slow_answers(30);
        let (hub, _) = hub_at(&agent.url(), "srv-stopped", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        assert_eq!(tools_on(a, &owner, "family").0, 200);
        let (_, _, j) = post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "@tempo a slow one"}),
        );
        assert_eq!(j["msg"]["owner_only"], true);
        let t0 = Instant::now();
        while agent.prompts().is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        // Sam joins while it is in flight
        let (sam, _) = join_as(&hub, a, &owner, "family", "Sam");
        let since = hub.seq();
        let ts = sse_until(a, &sam, since, "END-MARKER");
        let to = sse_until(a, &owner, since, "END-MARKER");
        let t0 = Instant::now();
        while hub.thinking("family") {
            assert!(t0.elapsed() < Duration::from_secs(20));
            std::thread::sleep(Duration::from_millis(20));
        }
        post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "END-MARKER"}),
        );
        let (sg, og) = (ts.join().unwrap(), to.join().unwrap());
        assert!(og.contains("Stopped."), "the owner is told: {og}");
        assert!(!sg.contains("Stopped") && !sg.contains("PRIVATE"), "{sg}");
        assert!(sg.contains("END-MARKER"));
        let (_, h) = get(a, "/room/messages?room=family&limit=100", &sam);
        assert!(!h.to_string().contains("Stopped") && !h.to_string().contains("PRIVATE-ANSWER"));
        let (_, h) = get(a, "/room/messages?room=family&limit=100", &owner);
        assert!(h.to_string().contains("Stopped."));
        r.stop();
        hub.shutdown();
    }

    fn confirm_ids(agent: &FakeAgent) -> Vec<(String, bool)> {
        agent
            .confirm_calls()
            .iter()
            .map(|c| (c["id"].as_str().unwrap().to_string(), c["ok"] == true))
            .collect()
    }

    fn wait_card(a: SocketAddr, owner: &str, id: &str) -> Value {
        let t0 = Instant::now();
        loop {
            let (_, j) = get(a, "/room/messages?room=family&limit=100", owner);
            if let Some(m) = j["messages"]
                .as_array()
                .and_then(|l| l.iter().find(|m| m["approval"]["id"] == id))
            {
                return m.clone();
            }
            assert!(t0.elapsed() < Duration::from_secs(15), "no card {id}: {j}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn approve(a: SocketAddr, cookie: &str, extra: &str, room: &str, id: &str, ok: bool) -> u16 {
        let body = json!({"room": room, "id": id, "ok": ok}).to_string();
        with_headers(
            a,
            "POST",
            "/room/approve",
            &format!("Cookie: {cookie}\r\n{extra}"),
            &body,
        )
        .0
    }

    #[test]
    fn only_the_owner_at_the_machine_approves_and_every_other_path_is_a_403_with_no_agent_call() {
        let agent = FakeAgent::start(vec![
            Script::ConfirmId("c-1".into(), "Written.".into()),
            Script::ConfirmId("c-2".into(), "Left alone.".into()),
            Script::ConfirmId("c-3".into(), "never posted".into()),
        ]);
        let (hub, _) = hub_at(&agent.url(), "srv-approve", json!({}));
        hub.lock()
            .create_room("Other", Mode::Mention, Personality::Warm)
            .unwrap();
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        hub.set_control_key("ck");
        // Sam is a member of #other (not of #family: that would end owner mode there)
        let (sam, _) = join_as(&hub, a, &owner, "other", "Sam");
        assert_eq!(tools_on(a, &owner, "family").0, 200);
        assert_eq!(tools_on(a, &owner, "other").0, 200);
        let (_, _, j) = post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "@tempo write the file"}),
        );
        let q = j["msg"]["id"].as_u64().unwrap();
        let card = wait_card(a, &owner, "c-1");
        assert_eq!(card["approval"]["state"], "pending");
        assert_eq!(card["approval"]["what"], "run a command");
        assert_eq!(card["approval"]["detail"], "echo hi > notes.txt");
        assert_eq!(card["owner_only"], true);
        // the card is the owner's: a member is never served it (and cannot ask about its room)
        let (c, _) = get(a, "/room/messages?room=family", &sam);
        assert_eq!(c, 403);
        // every way that is not the owner's own session at the machine
        let none = String::new();
        let proxied = "X-Forwarded-For: 203.0.113.9\r\n";
        let attempts: Vec<(&str, u16)> = vec![
            (
                "a member of another room",
                approve(a, &sam, "", "family", "c-1", true),
            ),
            (
                "a member claiming the other room",
                approve(a, &sam, "", "other", "c-1", true),
            ),
            ("a guest", approve(a, &none, "", "family", "c-1", true)),
            (
                "a forged cookie",
                approve(a, "tr_owner=forged", "", "family", "c-1", true),
            ),
            (
                "the owner from a proxy",
                approve(a, &owner, proxied, "family", "c-1", true),
            ),
            (
                "the owner behind Tailscale",
                approve(
                    a,
                    &owner,
                    "Tailscale-User-Login: x@y.z\r\n",
                    "family",
                    "c-1",
                    true,
                ),
            ),
            (
                "the control key alone",
                approve(a, &none, "X-Tempo-Control: ck\r\n", "family", "c-1", true),
            ),
            ("a wrong id", approve(a, &owner, "", "family", "c-9", true)),
            ("an empty id", approve(a, &owner, "", "family", "", true)),
            ("another room", approve(a, &owner, "", "other", "c-1", true)),
            ("no room", approve(a, &owner, "", "nowhere", "c-1", true)),
        ];
        for (who, code) in attempts {
            assert_eq!(code, 403, "{who}");
        }
        assert!(
            agent.confirm_calls().is_empty(),
            "the agent was called: {:?}",
            agent.confirm_calls()
        );
        // the owner at the machine approves: exactly one call, ok:true
        assert_eq!(approve(a, &owner, "", "family", "c-1", true), 200);
        assert_eq!(confirm_ids(&agent), [("c-1".to_string(), true)]);
        let ans = wait_for_tempo(a, &owner, "family", q);
        assert_eq!(ans["text"], "Written.");
        // a second tap does nothing
        assert_eq!(approve(a, &owner, "", "family", "c-1", true), 403);
        assert_eq!(confirm_ids(&agent).len(), 1);
        // the card now says so
        assert_eq!(wait_card(a, &owner, "c-1")["approval"]["state"], "approved");
        // deny sends ok:false
        let (_, _, j) = post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "@tempo and the risky one"}),
        );
        wait_card(a, &owner, "c-2");
        assert_eq!(approve(a, &owner, "", "family", "c-2", false), 200);
        // (a settled confirmation still in the event window is denied again: harmless)
        let ids = confirm_ids(&agent);
        assert_eq!(ids.iter().filter(|(_, ok)| *ok).count(), 1, "{ids:?}");
        assert!(ids.contains(&("c-2".to_string(), false)), "{ids:?}");
        wait_for_tempo(a, &owner, "family", j["msg"]["id"].as_u64().unwrap());
        // a third waits; then someone joins the room: the owner's tap is a 403 and makes no call
        post(
            a,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "@tempo one more"}),
        );
        wait_card(a, &owner, "c-3");
        let (_lee, _) = join_as(&hub, a, &owner, "family", "Lee");
        let before = confirm_ids(&agent);
        assert_eq!(approve(a, &owner, "", "family", "c-3", true), 403);
        assert_eq!(confirm_ids(&agent), before, "the tap made no call");
        assert!(
            !confirm_ids(&agent)
                .iter()
                .any(|(id, ok)| id == "c-3" && *ok),
            "{:?}",
            confirm_ids(&agent)
        );
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn the_owner_can_revoke_open_invites_and_nobody_else_can() {
        let (hub, _) = hub_at(&dead_url(), "srv-revoke", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let owner = owner_cookie(&hub);
        let (_, _, inv) = post(a, "/room/invite", &owner, &json!({"room": "family"}));
        let code = inv["code"].as_str().unwrap().to_string();
        assert_eq!(tools_on(a, &owner, "family").1["invites"], 1);
        let (c, _, _) = post(a, "/room/revoke-invites", "", &json!({"room": "family"}));
        assert_eq!(c, 403);
        let (c, _, j) = post(
            a,
            "/room/revoke-invites",
            &owner,
            &json!({"room": "family"}),
        );
        assert_eq!(
            (c, &j["revoked"], &j["state"]["tools_active"]),
            (200, &json!(1), &json!(true))
        );
        let (c, _, _) = post(a, "/room/join", "", &json!({"code": code, "name": "Late"}));
        assert_eq!(c, 404, "a revoked code no longer opens the room");
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn the_app_words_the_tools_switch_and_the_approval_plainly() {
        for must in [
            "Tempo can use its tools here while you are the only one in this room",
            "Approve on the computer running Perennos Tempo",
            "Approval needed",
            "Cancel open invites",
        ] {
            assert!(APP_JS.contains(must), "{must}");
        }
        // no pop-up windows for any of it, and the words the product avoids stay out
        let tools = &APP_JS
            [APP_JS.find("function toolsSection").unwrap()..APP_JS.find("// ---- Team:").unwrap()];
        let card = &APP_JS[APP_JS.find("function fillApproval").unwrap()
            ..APP_JS.find("function msgNode").unwrap()];
        for part in [tools, card] {
            for popup in ["confirm(", "alert(", "prompt(", "window.open"] {
                assert!(!part.contains(popup), "{popup}");
            }
        }
        let all = format!(
            "{APP_JS}\n{INDEX_HTML}\n{APP_CSS}\n{}",
            include_str!("../README.md")
        )
        .to_lowercase();
        for bad in ["ledger", "claim"] {
            assert!(!all.contains(bad), "{bad}");
        }
    }

    /// An address of this machine that is not loopback (the way a phone on the tailnet or LAN
    /// reaches Room): found without sending anything. `None` where the machine has none.
    fn other_local_ip() -> Option<std::net::Ipv4Addr> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("192.0.2.1:9").ok()?;
        match s.local_addr().ok()?.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
            _ => None,
        }
    }

    #[test]
    fn the_owner_cookie_on_a_non_loopback_connection_can_neither_turn_tools_on_nor_approve() {
        let Some(ip) = other_local_ip() else {
            eprintln!("(no non-loopback address on this machine: the real-peer check is skipped)");
            return;
        };
        let agent = FakeAgent::start(vec![Script::ConfirmId("c-1".into(), "Written.".into())]);
        let (hub, _) = hub_at(&agent.url(), "srv-nonloop", json!({}));
        let r = match start(
            hub.clone(),
            ServeOpts {
                bind: IpAddr::V4(ip),
                port: 0,
                ..ServeOpts::default()
            },
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("(cannot listen on {ip}: {e}; the real-peer check is skipped)");
                return;
            }
        };
        let (far, near) = (
            r.addr,
            r.loopback.expect("the owner's own loopback listener"),
        );
        let owner = owner_cookie(&hub);
        // at the machine (loopback) the cookie is the owner; from the other address it is nobody
        assert_eq!(get(near, "/room/me", &owner).1["role"], "owner");
        assert_eq!(get(far, "/room/me", &owner).1["role"], "guest");
        assert_eq!(get(near, "/room/me", &owner).1["loopback"], true);
        let (c, _, _) = post(
            far,
            "/room/tools",
            &owner,
            &json!({"room": "family", "on": true}),
        );
        assert_eq!(c, 403);
        assert!(!tools_of(&hub, "family"));
        assert_eq!(tools_on(near, &owner, "family").0, 200);
        post(
            near,
            "/room/post",
            &owner,
            &json!({"room": "family", "text": "@tempo write it"}),
        );
        wait_card(near, &owner, "c-1");
        let (c, _) = get(far, "/room/messages?room=family", &owner);
        assert_eq!(
            c, 403,
            "the owner's cookie shows nothing from another address"
        );
        assert_eq!(approve(far, &owner, "", "family", "c-1", true), 403);
        assert!(
            agent.confirm_calls().is_empty(),
            "{:?}",
            agent.confirm_calls()
        );
        assert_eq!(approve(near, &owner, "", "family", "c-1", true), 200);
        assert_eq!(confirm_ids(&agent), [("c-1".to_string(), true)]);
        r.stop();
        hub.shutdown();
    }

    #[test]
    fn a_session_minted_with_the_control_key_cannot_approve_what_another_session_asked() {
        let agent = FakeAgent::start(vec![Script::ConfirmId("c-1".into(), "Done.".into())]);
        let (hub, _) = hub_at(&agent.url(), "srv-minted", json!({}));
        let r = start(hub.clone(), opts()).unwrap();
        let a = r.addr;
        let first = owner_cookie(&hub);
        hub.set_control_key("ck");
        assert_eq!(tools_on(a, &first, "family").0, 200);
        let (_, _, j) = post(
            a,
            "/room/post",
            &first,
            &json!({"room": "family", "text": "@tempo write it"}),
        );
        let q = j["msg"]["id"].as_u64().unwrap();
        let card = wait_card(a, &first, "c-1");
        // the card names the asking session, and /me tells a window whether it is that one
        let (_, me1) = get(a, "/room/me", &first);
        assert!(me1["session"].as_str().is_some_and(|s| s.len() == 64));
        assert_eq!(card["approval"]["session"], me1["session"]);
        // the control key (the launcher does this) makes an owner link; the link makes a session
        let (c, _, body) = with_headers(
            a,
            "POST",
            "/room/owner-link",
            "X-Tempo-Control: ck\r\n",
            "{}",
        );
        assert_eq!(c, 200, "{body}");
        let link: Value = serde_json::from_str(&body).unwrap();
        let token = link["link"]
            .as_str()
            .unwrap()
            .split("#owner=")
            .nth(1)
            .unwrap()
            .to_string();
        let (c, head, _) = post(a, "/room/owner", "", &json!({"token": token}));
        assert_eq!(c, 200);
        let second = set_cookie(&head, "tr_owner")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let (_, me2) = get(a, "/room/me", &second);
        assert_eq!(me2["role"], "owner");
        assert_ne!(me2["session"], me1["session"]);
        // the new session sees the card but cannot answer it: 403, the agent is never called
        assert_eq!(wait_card(a, &second, "c-1")["approval"]["state"], "pending");
        assert_eq!(approve(a, &second, "", "family", "c-1", true), 403);
        assert_eq!(approve(a, &second, "", "family", "c-1", false), 403);
        assert!(
            agent.confirm_calls().is_empty(),
            "{:?}",
            agent.confirm_calls()
        );
        // revoking invites is the signed-in session's too, never the control key's
        let (c, _, _) = with_headers(
            a,
            "POST",
            "/room/revoke-invites",
            "X-Tempo-Control: ck\r\nContent-Length: 0\r\n",
            "",
        );
        assert_ne!(c, 200);
        let (c, _, _) = with_headers(
            a,
            "POST",
            "/room/revoke-invites",
            "X-Tempo-Control: ck\r\n",
            &json!({"room": "family"}).to_string(),
        );
        assert_eq!(c, 403);
        // the asking session still can
        assert_eq!(approve(a, &first, "", "family", "c-1", true), 200);
        assert_eq!(confirm_ids(&agent), [("c-1".to_string(), true)]);
        assert_eq!(wait_for_tempo(a, &first, "family", q)["text"], "Done.");
        r.stop();
        hub.shutdown();
    }
}
