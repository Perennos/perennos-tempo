//! Owner-only loopback adapter for the native fleet prepare/apply API.

use super::{
    bootstrap, crypto, scan,
    store::{self, Paths},
};
use crate::{
    setup::json::{parse, J},
    Ctx,
};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const PORT: u16 = 18776;
const HOST: &str = "127.0.0.1:18776";
const ORIGIN: &str = "http://127.0.0.1:18776";
const SESSION: &str = "fleet-workbench/session.json";
const DEADLINE: Duration = Duration::from_secs(3);

struct Gate<T> {
    pending: Option<(String, Instant, T)>,
    busy: bool,
}

impl<T> Gate<T> {
    fn new() -> Self {
        Self {
            pending: None,
            busy: false,
        }
    }
    fn begin(&mut self) -> Result<(), String> {
        if self.busy {
            return Err("A fleet operation is already running.".into());
        }
        self.pending = None;
        self.busy = true;
        Ok(())
    }
    fn review(&mut self, value: T) -> String {
        let handle = crypto::hex(&crypto::random(24));
        self.pending = Some((handle.clone(), Instant::now(), value));
        self.busy = false;
        handle
    }
    fn consume(&mut self, handle: &str) -> Result<T, String> {
        if self.busy {
            return Err("A fleet operation is already running.".into());
        }
        let Some((id, at, _)) = &self.pending else {
            return Err("Prepare this computer again.".into());
        };
        if !crypto::same(id.as_bytes(), handle.as_bytes())
            || at.elapsed() > Duration::from_secs(600)
        {
            return Err("That review expired or changed. Prepare again.".into());
        }
        let (_, _, plan) = self.pending.take().expect("pending was checked");
        self.busy = true;
        Ok(plan)
    }
}

struct State {
    gate: Gate<bootstrap::Plan>,
    stage: String,
    error: String,
    review: J,
    candidates: J,
    touched: Instant,
}

impl State {
    fn new() -> Self {
        Self {
            gate: Gate::new(),
            stage: "Ready".into(),
            error: String::new(),
            review: J::Null,
            candidates: J::Arr(vec![]),
            touched: Instant::now(),
        }
    }
    fn finish(&mut self, result: Result<(), String>) {
        self.gate.busy = false;
        if let Err(e) = result {
            self.stage = "Needs attention".into();
            self.error = e;
        }
        self.touched = Instant::now();
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn request(stream: &mut TcpStream) -> Result<Request, String> {
    let end = Instant::now() + DEADLINE;
    let mut bytes = Vec::new();
    let split;
    loop {
        if bytes.len() >= 8192 {
            return Err("Headers are too large.".into());
        }
        read_one(stream, &mut bytes, end)?;
        if bytes.ends_with(b"\r\n\r\n") {
            split = bytes.len();
            break;
        }
    }
    let head = std::str::from_utf8(&bytes).map_err(|_| "Headers must be text.")?;
    let mut lines = head.split("\r\n");
    let words: Vec<_> = lines.next().unwrap_or("").split(' ').collect();
    if words.len() != 3 || words[2] != "HTTP/1.1" || !matches!(words[0], "GET" | "POST") {
        return Err("Unsupported request.".into());
    }
    let method = words[0].to_string();
    let path = words[1].to_string();
    let mut headers = HashMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("Malformed header.")?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b < 32 && b != b'\t')
        {
            return Err("Malformed header.".into());
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_string())
            .is_some()
        {
            return Err("Repeated header.".into());
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err("Transfer encoding is not supported.".into());
    }
    let size = match headers.get("content-length") {
        Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse::<usize>().map_err(|_| "Invalid body length.")?
        }
        Some(_) => return Err("Invalid body length.".into()),
        None if method == "GET" => 0,
        None => return Err("A body length is required.".into()),
    };
    if size > 4096 || (method == "GET" && size != 0) {
        return Err("Body is too large.".into());
    }
    for _ in 0..size {
        read_one(stream, &mut bytes, end)?;
    }
    Ok(Request {
        method,
        path,
        headers,
        body: bytes[split..].to_vec(),
    })
}

fn read_one(stream: &mut TcpStream, bytes: &mut Vec<u8>, end: Instant) -> Result<(), String> {
    let left = end.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("Request deadline exceeded.".into());
    }
    stream
        .set_read_timeout(Some(left))
        .map_err(|e| e.to_string())?;
    let mut byte = [0];
    stream
        .read_exact(&mut byte)
        .map_err(|_| "Request ended or exceeded its deadline.")?;
    bytes.push(byte[0]);
    Ok(())
}

fn authorized(req: &Request, token: &str) -> bool {
    req.headers.get("host").is_some_and(|s| s == HOST)
        && req.headers.get("origin").is_some_and(|s| s == ORIGIN)
        && req
            .headers
            .get("content-type")
            .is_some_and(|s| s == "application/json")
        && req
            .headers
            .get("authorization")
            .is_some_and(|s| crypto::same(s.as_bytes(), format!("Bearer {token}").as_bytes()))
}

fn reply(stream: &mut TcpStream, status: &str, mime: &str, body: &[u8]) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let headers = format!("HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src data:; font-src data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'\r\n\r\n", body.len());
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body);
}

fn snapshot(paths: &Paths, state: &State) -> J {
    let fleet = store::load(paths);
    let now = store::now_s();
    let members = fleet
        .machines
        .iter()
        .map(|m| {
            J::Obj(vec![
                ("name".into(), J::s(&m.name)),
                ("address".into(), J::s(&format!("{}:{}", m.addr, m.port))),
                (
                    "state".into(),
                    J::s(if !m.enabled {
                        "Switched off"
                    } else if m.last_seen > 0
                        && now.saturating_sub(m.last_seen) < super::serve::AWAY_S
                    {
                        "Answering"
                    } else {
                        "Not heard from recently"
                    }),
                ),
            ])
        })
        .collect();
    let cache = crate::updates::read_cache(&paths.home);
    let latest = cache
        .latest
        .filter(|v| *v > crate::updates::current_version());
    J::Obj(vec![
        ("heading".into(), J::s(&super::heading(fleet.count()))),
        ("members".into(), J::Arr(members)),
        ("stage".into(), J::s(&state.stage)),
        ("error".into(), J::s(&state.error)),
        ("busy".into(), J::Bool(state.gate.busy)),
        ("review".into(), state.review.clone()),
        ("candidates".into(), state.candidates.clone()),
        (
            "updates".into(),
            J::Obj(vec![
                (
                    "current".into(),
                    J::s(&crate::updates::current_version().tag()),
                ),
                ("latest".into(), latest.map_or(J::Null, |v| J::s(&v.tag()))),
                (
                    "url".into(),
                    latest.map_or(J::Null, |v| {
                        J::s(&format!("{}{v}", crate::updates::RELEASES_URL, v = v.tag()))
                    }),
                ),
                ("enabled".into(), J::Bool(cache.enabled)),
                (
                    "text".into(),
                    J::s(&crate::updates::status_text(&cache, now)),
                ),
            ]),
        ),
    ])
}

fn dispatch(paths: Paths, state: Arc<Mutex<State>>, body: J) -> Result<(), String> {
    let action = body
        .str("action")
        .ok_or("An action is required.")?
        .to_string();
    let allowed: &[&str] = match action.as_str() {
        "status" | "discover" | "updates" | "decline" => &["action"],
        "prepare" => &["action", "target"],
        "approve" => &["action", "handle"],
        "automatic" => &["action", "enabled"],
        _ => return Err("Unknown action.".into()),
    };
    let J::Obj(fields) = &body else {
        return Err("An object is required.".into());
    };
    let mut seen = std::collections::HashSet::new();
    if fields
        .iter()
        .any(|(k, _)| !allowed.contains(&k.as_str()) || !seen.insert(k))
    {
        return Err("Unexpected or repeated field.".into());
    }
    if action == "status" {
        return Ok(());
    }
    let mut st = state
        .lock()
        .map_err(|_| "The workbench state is unavailable.")?;
    st.touched = Instant::now();
    if action == "decline" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        st.gate.pending = None;
        st.review = J::Null;
        st.stage = "Nothing was changed".into();
        return Ok(());
    }
    if action == "approve" {
        let plan = st
            .gate
            .consume(body.str("handle").ok_or("A review handle is required.")?)?;
        st.review = J::Null;
        st.error.clear();
        st.stage = "Rechecking".into();
        drop(st);
        std::thread::spawn(move || {
            let approval = plan.approval();
            let result = bootstrap::apply_progress(&paths, &plan, Some(&approval), &mut |stage| {
                if let Ok(mut st) = state.lock() {
                    st.stage = stage.into();
                }
            });
            if let Ok(mut st) = state.lock() {
                st.finish(result);
            }
        });
        return Ok(());
    }
    if action == "automatic" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        crate::updates::set_enabled(
            &paths.home,
            body.bool("enabled")
                .ok_or("Choose automatic checks on or off.")?,
        )?;
        return Ok(());
    }
    let target = if action == "prepare" {
        Some(
            body.str("target")
                .filter(|s| super::ssh::target_ok(s))
                .ok_or("Choose a valid SSH target.")?
                .to_string(),
        )
    } else {
        None
    };
    st.gate.begin()?;
    st.review = J::Null;
    st.error.clear();
    st.stage = match action.as_str() {
        "prepare" => "Reading this computer",
        "discover" => "Finding private computers",
        _ => "Checking the official release",
    }
    .into();
    drop(st);
    std::thread::spawn(move || {
        let result = match action.as_str() {
            "prepare" => bootstrap::prepare(target.as_deref().unwrap_or("")).map(|plan| {
                if let Ok(mut st) = state.lock() {
                    let mut summary = plan.summary();
                    let handle = st.gate.review(plan);
                    if let J::Obj(ref mut fields) = summary {
                        fields.push(("handle".into(), J::s(&handle)));
                    }
                    st.review = summary;
                    st.stage = "Review setup".into();
                }
            }),
            "discover" => {
                let fleet = store::load(&paths);
                let found = scan::scan(&fleet, &mut std::io::sink());
                let mut candidates: Vec<J> = found
                    .into_iter()
                    .filter(|f| !f.in_fleet)
                    .map(|f| {
                        J::Obj(vec![
                            ("target".into(), J::s(&f.addr)),
                            ("name".into(), J::s(&f.name)),
                            (
                                "state".into(),
                                J::s(if f.ssh {
                                    "SSH is reachable"
                                } else {
                                    "Set up SSH access first"
                                }),
                            ),
                        ])
                    })
                    .collect();
                if let Some(home) =
                    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                {
                    if let Ok(text) =
                        std::fs::read_to_string(std::path::PathBuf::from(home).join(".ssh/config"))
                    {
                        for host in super::ssh::config_hosts(&text).into_iter().take(64) {
                            candidates.push(J::Obj(vec![
                                ("target".into(), J::s(&host)),
                                ("name".into(), J::s(&host)),
                                ("state".into(), J::s("Saved SSH connection; not checked")),
                            ]));
                        }
                    }
                }
                if let Ok(mut st) = state.lock() {
                    st.candidates = J::Arr(candidates);
                    st.stage = "Discovery finished".into();
                }
                Ok(())
            }
            _ => crate::updates::check_local(&paths.home, false).map(|_| {
                if let Ok(mut st) = state.lock() {
                    st.stage = "Release check finished".into();
                }
            }),
        };
        if let Ok(mut st) = state.lock() {
            st.finish(result);
        }
    });
    Ok(())
}

fn connection(mut stream: TcpStream, paths: Paths, token: String, state: Arc<Mutex<State>>) {
    let response = request(&mut stream);
    let Ok(req) = response else {
        reply(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"Request refused.",
        );
        return;
    };
    if req.headers.get("host").is_none_or(|s| s != HOST) {
        reply(&mut stream, "403 Forbidden", "text/plain", b"Host refused.");
        return;
    }
    if req.method == "GET" {
        match req.path.as_str() {
            "/" => reply(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                html().as_bytes(),
            ),
            "/workbench.js" => reply(
                &mut stream,
                "200 OK",
                "text/javascript; charset=utf-8",
                include_bytes!("onboarding.js"),
            ),
            "/workbench.css" => reply(
                &mut stream,
                "200 OK",
                "text/css; charset=utf-8",
                include_bytes!("onboarding.css"),
            ),
            _ => reply(&mut stream, "404 Not Found", "text/plain", b"Not found."),
        }
        return;
    }
    if req.path != "/api" || !authorized(&req, &token) {
        reply(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            b"Owner access required. Open Fleet from Perennos Tempo on this computer.",
        );
        return;
    }
    let result = std::str::from_utf8(&req.body)
        .map_err(|_| "The request must be UTF-8.".to_string())
        .and_then(parse)
        .and_then(|body| dispatch(paths.clone(), state.clone(), body));
    let (status, body) = match result {
        Ok(()) => match state.lock() {
            Ok(st) => ("200 OK", snapshot(&paths, &st)),
            Err(_) => (
                "503 Service Unavailable",
                J::s("The workbench state is unavailable."),
            ),
        },
        Err(e) => ("409 Conflict", J::Obj(vec![("error".into(), J::s(&e))])),
    };
    reply(
        &mut stream,
        status,
        "application/json",
        body.pretty().as_bytes(),
    );
}

fn html() -> String {
    format!("{}<link rel=\"stylesheet\" href=\"/workbench.css\"><div class=\"fleet-head\"><div><h1 id=\"heading\">Your fleet</h1><p>One computer, or a band of them.</p></div><button id=\"update-toggle\" aria-expanded=\"false\" aria-controls=\"updates\">Updates</button></div><section id=\"updates\" hidden aria-labelledby=\"update-title\"><h2 id=\"update-title\">Perennos Tempo updates</h2><p id=\"update-text\"></p><div class=\"actions\"><button id=\"check-update\">Check now</button><a id=\"release\" hidden target=\"_blank\" rel=\"noopener noreferrer\">Open release</a></div><label class=\"check\"><input id=\"automatic\" type=\"checkbox\">Check automatically</label></section><section aria-labelledby=\"members-title\"><h2 id=\"members-title\">In your fleet</h2><p class=\"small\">This computer is included in the count. Other computers appear below.</p><ul id=\"members\" class=\"machine-list\"></ul></section><section aria-labelledby=\"find-title\"><div class=\"section-tools\"><h2 id=\"find-title\">Bring another computer in</h2><button id=\"discover\" disabled>Find computers</button></div><p>Discovery checks your private network. Setup uses your existing SSH access and asks before making changes.</p><form id=\"target-form\"><label for=\"target\">SSH name or private address</label><div class=\"actions\"><input id=\"target\" maxlength=\"200\" autocomplete=\"off\" required><button id=\"prepare\" disabled>Review setup</button></div></form><ul id=\"candidates\" class=\"machine-list\"></ul></section><section id=\"review\" hidden aria-labelledby=\"review-title\"><h2 id=\"review-title\">Review this setup</h2><dl id=\"review-facts\"></dl><p id=\"changes\"></p><div class=\"actions\"><button id=\"approve\">Approve setup</button><button id=\"decline\">Decline</button></div></section><section class=\"operation\" aria-labelledby=\"operation-title\"><h2 id=\"operation-title\">Connection</h2><p id=\"stage\" role=\"status\" aria-live=\"polite\">Open Fleet from Perennos Tempo on this computer to continue.</p><p id=\"error\" role=\"alert\"></p></section><script src=\"/workbench.js\" defer></script>{}", crate::brand::page_top("Perennos Tempo — Fleet", false, false), crate::brand::PAGE_END)
}

// Installation authority needs stricter protection than best-effort key-file ACLs.
fn private_session(paths: &Paths, token: &str) -> Result<(), String> {
    let dir = paths.file("fleet-workbench");
    if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("The workbench folder must not be a link.".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::MetadataExt, process::CommandExt};
        if std::fs::metadata(&dir)
            .map_err(|e| e.to_string())?
            .file_attributes()
            & 0x400
            != 0
        {
            return Err("The workbench folder must not be a reparse point.".into());
        }
        let said = std::process::Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| e.to_string())?;
        let sid = String::from_utf8_lossy(&said.stdout)
            .split(',')
            .nth(1)
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        if !said.status.success()
            || !sid.starts_with("S-1-")
            || !sid
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
        {
            return Err("Could not identify the current Windows owner.".into());
        }
        let reset = std::process::Command::new("icacls")
            .arg(&dir)
            .arg("/reset")
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| e.to_string())?;
        if !reset.status.success() {
            return Err("Could not reset the workbench directory ACL.".into());
        }
        let output = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("*{sid}:(OI)(CI)F"))
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err("Could not protect the owner session with a Windows ACL.".into());
        }
    }
    let body = J::Obj(vec![("token".into(), J::s(token))]).pretty();
    let path = paths.file(SESSION);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("The session file must not be a link.".into());
    }
    crypto::write_private(&path, body.as_bytes()).map_err(|e| e.to_string())
}

fn existing(paths: &Paths) -> Option<String> {
    let text = std::fs::read_to_string(paths.file(SESSION)).ok()?;
    let json = parse(&text).ok()?;
    let token = json
        .str("token")
        .filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let mut stream =
        TcpStream::connect_timeout(&HOST.parse().ok()?, Duration::from_millis(300)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    let body = "{\"action\":\"status\"}";
    write!(stream, "POST /api HTTP/1.1\r\nHost: {HOST}\r\nOrigin: {ORIGIN}\r\nContent-Type: application/json\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).ok()?;
    let mut status = [0u8; 16];
    stream.read_exact(&mut status).ok()?;
    if &status != b"HTTP/1.1 200 OK\r" {
        return None;
    }
    Some(token.into())
}

pub fn run(ctx: &Ctx) -> Result<(), String> {
    let paths = Paths::of(ctx);
    if let Some(token) = existing(&paths) {
        return ctx.open(&format!("{ORIGIN}/#key={token}"));
    }
    let listener = TcpListener::bind(("127.0.0.1", PORT)).map_err(|_| {
        "The Fleet workbench port is busy. Close the other listener, then open Fleet again."
    })?;
    let token = crypto::hex(&crypto::random(32));
    private_session(&paths, &token)?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    ctx.open(&format!("{ORIGIN}/#key={token}"))?;
    let state = Arc::new(Mutex::new(State::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    loop {
        if state
            .lock()
            .is_ok_and(|s| !s.gate.busy && s.touched.elapsed() > Duration::from_secs(1800))
        {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if connections.fetch_add(1, Ordering::SeqCst) >= 4 {
                    connections.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let (paths, token, state, count) = (
                    paths.clone(),
                    token.clone(),
                    state.clone(),
                    connections.clone(),
                );
                std::thread::spawn(move || {
                    connection(stream, paths, token, state);
                    count.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Result<Request, String> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.write_all(text.as_bytes()).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        request(&mut listener.accept().unwrap().0)
    }
    fn wire(extra: &str, length: &str) -> String {
        format!("POST /api HTTP/1.1\r\nHost: {HOST}\r\nOrigin: {ORIGIN}\r\nContent-Type: application/json\r\nAuthorization: Bearer owner\r\nContent-Length: {length}\r\n{extra}\r\n{{}}")
    }

    #[test]
    fn owner_origin_host_content_type_and_port_are_all_required() {
        let wire = wire("", "2");
        assert!(authorized(&parsed(&wire).unwrap(), "owner"));
        for changed in [
            wire.replace(HOST, "localhost:18776"),
            wire.replace(ORIGIN, "http://127.0.0.1:18777"),
            wire.replace("Bearer owner", "Bearer guest"),
            wire.replace("application/json", "text/plain"),
            wire.replace("Authorization: Bearer owner\r\n", ""),
        ] {
            assert!(!authorized(&parsed(&changed).unwrap(), "owner"));
        }
    }

    #[test]
    fn ambiguous_oversized_and_chunked_bodies_are_refused() {
        assert!(parsed(&wire("", "2")).is_ok());
        for changed in [
            wire("Content-Length: 2\r\n", "2"),
            wire("Transfer-Encoding: chunked\r\n", "2"),
            wire("", "4097"),
            wire("", "+2"),
            wire(" origin: ignored\r\n", "2"),
        ] {
            assert!(parsed(&changed).is_err());
        }
    }

    #[test]
    fn double_approval_forgery_decline_and_polling_cannot_repeat_work() {
        let mut gate = Gate::new();
        let handle = gate.review(47);
        assert!(gate.consume("forged").is_err());
        assert_eq!(gate.consume(&handle).unwrap(), 47);
        assert!(gate.consume(&handle).is_err());
        gate.busy = false;
        assert!(gate.consume(&handle).is_err());
        let handle = gate.review(48);
        gate.pending = None;
        assert!(gate.consume(&handle).is_err());
        let root = super::super::tests::tmp("poll");
        let paths = Paths {
            home: root.join("home"),
            agent_data: root.join("agent"),
        };
        let state = Arc::new(Mutex::new(State::new()));
        for _ in 0..5 {
            dispatch(
                paths.clone(),
                state.clone(),
                parse("{\"action\":\"status\"}").unwrap(),
            )
            .unwrap();
        }
        assert!(!paths.home.exists());
        assert!(!state.lock().unwrap().gate.busy);
        assert!(dispatch(
            paths,
            state,
            parse("{\"action\":\"status\",\"url\":\"untrusted\"}").unwrap()
        )
        .is_err());
    }

    #[test]
    fn unauthenticated_shell_contains_no_inventory_or_owner_authority() {
        let html = html();
        assert!(html.contains("/workbench.js"));
        assert!(
            !html.contains("fleet.key") && !html.contains("Bearer ") && !html.contains("192.168.")
        );
        assert!(html.contains("id=\"prepare\" disabled"));
    }

    #[test]
    fn slow_requests_share_one_absolute_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let peer = std::thread::spawn(move || {
            for _ in 0..40 {
                if client.write_all(b"G").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let start = Instant::now();
        assert!(request(&mut listener.accept().unwrap().0).is_err());
        assert!(start.elapsed() < Duration::from_secs(4));
        peer.join().unwrap();
    }
}
