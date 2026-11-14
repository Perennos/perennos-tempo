//! Owner-only loopback adapter for the native fleet prepare/apply API.

use super::{
    bootstrap, crypto, scan,
    store::{self, Paths},
};
use crate::{
    company,
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
    engine_candidates: J,
    cloud_accounts: J,
    cloud_last_checked: Option<Instant>,
    standings: J,
    floor_traffic: J,
    activity_last_checked: Option<Instant>,
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
            engine_candidates: J::Arr(vec![]),
            cloud_accounts: J::Arr(vec![]),
            cloud_last_checked: None,
            standings: J::Arr(vec![]),
            floor_traffic: J::Arr(vec![]),
            activity_last_checked: None,
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

fn authorized_state(req: &Request, token: &str) -> bool {
    req.method == "GET"
        && req.path == "/state"
        && req.headers.get("host").is_some_and(|s| s == HOST)
        && req.headers.get("origin").is_none_or(|s| s == ORIGIN)
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

fn snapshot(paths: &Paths, state: &mut State) -> J {
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
    let company_doc = company::read_only(&paths.home).unwrap_or(J::Null);
    if state
        .activity_last_checked
        .is_none_or(|last| last.elapsed() >= Duration::from_secs(15))
    {
        let (standings, traffic) = company::activity(&paths.ctx(), &company_doc);
        state.standings = standings;
        state.floor_traffic = traffic;
        state.activity_last_checked = Some(Instant::now());
    }
    J::Obj(vec![
        ("heading".into(), J::s(&super::heading(fleet.count()))),
        ("members".into(), J::Arr(members)),
        ("stage".into(), J::s(&state.stage)),
        ("error".into(), J::s(&state.error)),
        ("busy".into(), J::Bool(state.gate.busy)),
        ("review".into(), state.review.clone()),
        ("candidates".into(), state.candidates.clone()),
        ("company".into(), company_doc),
        ("engine_candidates".into(), state.engine_candidates.clone()),
        ("cloud_accounts".into(), state.cloud_accounts.clone()),
        ("standings".into(), state.standings.clone()),
        ("floor_traffic".into(), state.floor_traffic.clone()),
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
        "status" | "discover" | "updates" | "decline" | "scan-engines" | "scan-cloud" => {
            &["action"]
        }
        "prepare" => &["action", "target"],
        "approve" => &["action", "handle"],
        "approve-node" => &["action", "node"],
        "declare-cloud" => &["action", "node", "provider"],
        "rename-member" => &["action", "id", "name"],
        "remove-member" => &["action", "id"],
        "add-department" => &["action", "name"],
        "reorder-peer" => &["action", "id", "direction"],
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
    if action == "scan-engines" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        st.gate.busy = true;
        st.stage = "Checking engines already configured on approved nodes".into();
        st.error.clear();
        drop(st);
        std::thread::spawn(move || {
            let result = company::discover(&paths.ctx());
            if let Ok(mut st) = state.lock() {
                match result {
                    Ok(nodes) => {
                        st.engine_candidates = J::Arr(nodes);
                        st.stage =
                            "Engine scan finished; nothing is enrolled until you approve it".into();
                        st.finish(Ok(()));
                    }
                    Err(e) => st.finish(Err(e)),
                }
            }
        });
        return Ok(());
    }
    if action == "scan-cloud" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        if st
            .cloud_last_checked
            .is_some_and(|last| last.elapsed() < Duration::from_secs(15))
        {
            return Ok(());
        }
        st.gate.busy = true;
        st.stage = "Checking supported cloud sign-in status".into();
        st.error.clear();
        drop(st);
        std::thread::spawn(move || {
            let (accounts, cloud_node) = company::discover_cloud(&paths.ctx());
            if let Ok(mut st) = state.lock() {
                let mut nodes = match &st.engine_candidates {
                    J::Arr(items) => items.to_vec(),
                    _ => Vec::new(),
                };
                nodes.retain(|node| node.str("node_id") != Some("cloud"));
                if let Some(node) = cloud_node {
                    nodes.push(node);
                }
                st.engine_candidates = J::Arr(nodes);
                st.cloud_accounts = J::Arr(accounts);
                st.cloud_last_checked = Some(Instant::now());
                st.stage = "Cloud scan finished; approve each found lane before enrollment".into();
                st.finish(Ok(()));
            }
        });
        return Ok(());
    }
    if action == "approve-node" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        let node = body
            .str("node")
            .ok_or("Choose a node to approve.")?
            .to_string();
        let candidate = match &st.engine_candidates {
            J::Arr(items) => items
                .iter()
                .find(|n| n.str("node_id") == Some(&node))
                .cloned(),
            _ => None,
        }
        .ok_or("That node is no longer pending. Scan again.")?;
        let name = candidate.str("name").unwrap_or(&node).to_string();
        let engines = candidate.arr("engines").to_vec();
        drop(st);
        let doc = company::approve_node(&paths.home, &node, &name, &engines)?;
        let mut st = state
            .lock()
            .map_err(|_| "The workbench state is unavailable.")?;
        let remaining = match &st.engine_candidates {
            J::Arr(items) => items
                .iter()
                .filter(|n| n.str("node_id") != Some(&node))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        st.engine_candidates = J::Arr(remaining);
        st.stage = format!("{} and its listed engines are registered", name);
        st.error.clear();
        let _ = doc;
        return Ok(());
    }
    if action == "declare-cloud" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        let node = body
            .str("node")
            .ok_or("Choose a node for this lane.")?
            .to_string();
        let provider = body
            .str("provider")
            .filter(|p| company::safe_label(p))
            .ok_or("Enter a provider name only; sign-in values are never accepted.")?
            .to_string();
        let slug = provider.to_ascii_lowercase().replace(' ', "-");
        let J::Arr(nodes) = &mut st.engine_candidates else {
            return Err("Engine findings are unavailable.".into());
        };
        if !nodes.iter().any(|n| n.str("node_id") == Some(&node)) {
            if node != "cloud" {
                return Err("Scan engines before adding a cloud lane.".into());
            }
            nodes.push(J::Obj(vec![
                ("node_id".into(), J::s("cloud")),
                ("name".into(), J::s("Cloud services")),
                ("engines".into(), J::Arr(vec![])),
            ]));
        }
        let target = nodes
            .iter_mut()
            .find(|n| n.str("node_id") == Some(&node))
            .ok_or("Scan engines before adding a cloud lane.")?;
        let J::Arr(engines) = target.get("engines").cloned().unwrap_or(J::Arr(vec![])) else {
            return Err("Engine findings are invalid.".into());
        };
        let mut engines = engines;
        let id = format!("{node}:cloud:{slug}");
        if !engines.iter().any(|e| e.str("id") == Some(&id)) {
            engines.push(J::Obj(vec![
                ("id".into(), J::s(&id)),
                ("name".into(), J::s(&provider)),
                ("kind".into(), J::s("Cloud lane")),
                ("status".into(), J::s("Declared by owner; unverified")),
            ]));
        }
        target.set("engines", J::Arr(engines));
        st.stage = format!(
            "{provider} added as an unverified declaration; approve this node to register it"
        );
        return Ok(());
    }
    if action == "rename-member" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        let id = body.str("id").ok_or("Choose an employee.")?.to_string();
        let name = body.str("name").ok_or("Enter a display name.")?.to_string();
        drop(st);
        company::rename_member(&paths.home, &id, &name)?;
        return Ok(());
    }
    if action == "remove-member" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        let id = body.str("id").ok_or("Choose an employee.")?.to_string();
        drop(st);
        company::remove_member(&paths.home, &id)?;
        return Ok(());
    }
    if action == "add-department" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        let name = body
            .str("name")
            .ok_or("Enter a department name.")?
            .to_string();
        drop(st);
        company::add_department(&paths.home, &name)?;
        return Ok(());
    }
    if action == "reorder-peer" {
        if st.gate.busy {
            return Err("Wait for the current operation.".into());
        }
        let id = body.str("id").ok_or("Choose a peer.")?.to_string();
        let direction = body
            .str("direction")
            .ok_or("Choose a direction.")?
            .to_string();
        drop(st);
        company::reorder_peer(&paths.home, &id, &direction)?;
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
        if req.path == "/state" {
            if !authorized_state(&req, &token) {
                reply(
                    &mut stream,
                    "403 Forbidden",
                    "text/plain",
                    b"Owner access required.",
                );
                return;
            }
            let body = match state.lock() {
                Ok(mut state) => snapshot(&paths, &mut state),
                Err(_) => {
                    reply(
                        &mut stream,
                        "503 Service Unavailable",
                        "application/json",
                        b"\"The workbench state is unavailable.\"",
                    );
                    return;
                }
            };
            reply(
                &mut stream,
                "200 OK",
                "application/json",
                body.pretty().as_bytes(),
            );
            return;
        }
        match req.path.as_str() {
            "/" => reply(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                html(&paths).as_bytes(),
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
            "/conductor-welcome.js" => reply(
                &mut stream,
                "200 OK",
                "text/javascript; charset=utf-8",
                include_bytes!("../conductor/welcome.js"),
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
            Ok(mut st) => ("200 OK", snapshot(&paths, &mut st)),
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

fn base_html() -> String {
    format!("{}<link rel=\"stylesheet\" href=\"/workbench.css\"><div class=\"fleet-head\"><div><h1 id=\"heading\">Your fleet</h1><p>One computer, or a band of them.</p></div><button id=\"update-toggle\" aria-expanded=\"false\" aria-controls=\"updates\">Updates</button></div><section id=\"updates\" hidden aria-labelledby=\"update-title\"><h2 id=\"update-title\">Perennos Tempo updates</h2><p id=\"update-text\"></p><div class=\"actions\"><button id=\"check-update\">Check now</button><a id=\"release\" hidden target=\"_blank\" rel=\"noopener noreferrer\">Open release</a></div><label class=\"check\"><input id=\"automatic\" type=\"checkbox\">Check automatically</label></section><section aria-labelledby=\"members-title\"><h2 id=\"members-title\">In your fleet</h2><p class=\"small\">This computer is included in the count. Other computers appear below.</p><ul id=\"members\" class=\"machine-list\"></ul></section><section aria-labelledby=\"find-title\"><div class=\"section-tools\"><h2 id=\"find-title\">Bring another computer in</h2><button id=\"discover\" disabled>Find computers</button></div><p>Discovery checks your private network. Setup uses your existing SSH access and asks before making changes.</p><form id=\"target-form\"><label for=\"target\">SSH name or private address</label><div class=\"actions\"><input id=\"target\" maxlength=\"200\" autocomplete=\"off\" required><button id=\"prepare\" disabled>Review setup</button></div></form><ul id=\"candidates\" class=\"machine-list\"></ul></section><section id=\"review\" hidden aria-labelledby=\"review-title\"><h2 id=\"review-title\">Review this setup</h2><dl id=\"review-facts\"></dl><p id=\"changes\"></p><div class=\"actions\"><button id=\"approve\">Approve setup</button><button id=\"decline\">Decline</button></div></section><section class=\"operation\" aria-labelledby=\"operation-title\"><h2 id=\"operation-title\">Connection</h2><p id=\"stage\" role=\"status\" aria-live=\"polite\">Open Fleet from Perennos Tempo on this computer to continue.</p><p id=\"error\" role=\"alert\"></p></section><script src=\"/workbench.js\" defer></script>{}", crate::brand::page_top("Perennos Tempo — Fleet", false, false), crate::brand::PAGE_END)
}

fn html(paths: &Paths) -> String {
    let company_doc = company::read_only(&paths.home).unwrap_or_else(|_| company::initial());
    let tour = company::tour_markup(&paths.ctx(), &company_doc);
    let profile_id = profile_cache_id(&paths.home);
    let mut page = base_html().replacen(
        "<div class=\"fleet-head\">",
        &format!("<nav class=\"tempo-tabs\" data-profile-id=\"{profile_id}\" aria-label=\"Tempo workspace\"><button id=\"view-setup\" type=\"button\" aria-pressed=\"true\" aria-controls=\"setup-view\">Setup</button><button id=\"view-conductor\" type=\"button\" aria-pressed=\"false\" aria-controls=\"conductor-view\">Conductor</button></nav><section id=\"setup-view\" aria-label=\"Setup and connections\"><div class=\"fleet-head\">"),
        1,
    );
    let conductor = format!(
        "</section><section id=\"conductor-view\" hidden aria-labelledby=\"conductor-title\"><div class=\"conductor-head\"><div><p class=\"tt-kicker\">Perennos Tempo</p><h1 id=\"conductor-title\">Conductor</h1><p class=\"small\">Live operations from this profile's company, nodes, engines and work.</p></div><button id=\"conductor-setup-again\" type=\"button\">Scan again</button></div><p id=\"conductor-scan-freshness\" class=\"small\" aria-live=\"polite\">Waiting for first scan.</p><section aria-labelledby=\"map-title\"><h2 id=\"map-title\">Orbital operations map</h2><p class=\"small\">Approved and pending nodes appear here. Pending members join only after you approve them.</p><canvas id=\"conductor-map\" height=\"320\" aria-hidden=\"true\"></canvas><ul id=\"map-directory\" class=\"machine-list\" aria-label=\"Operations map directory\"></ul></section><div class=\"conductor-grid\"><section aria-labelledby=\"standings-title\"><h2 id=\"standings-title\">Employee of the Month</h2><p id=\"standings-empty\" class=\"small\">No completed work this month yet.</p><ol id=\"standings\" class=\"machine-list\"></ol></section><section aria-labelledby=\"traffic-title\"><h2 id=\"traffic-title\">Floor traffic</h2><p id=\"traffic-empty\" class=\"small\">No recent activity to show.</p><ol id=\"floor-traffic\" class=\"machine-list\"></ol></section></div><section id=\"conductor-company-mount\" aria-label=\"Org chart editor\"></section>{tour}</section><script src=\"/conductor-welcome.js\" defer></script><script src=\"/workbench.js\" defer></script>",
    );
    page = page.replacen(
        "<script src=\"/workbench.js\" defer></script>",
        &conductor,
        1,
    );
    page
}

fn profile_cache_id(home: &std::path::Path) -> String {
    let identity = home.to_string_lossy().to_lowercase();
    let hash = identity
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("{hash:016x}")
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
    fn state_get_is_read_only_and_requires_the_owner_token() {
        let request = |origin: Option<&str>, authorization: Option<&str>| {
            let mut headers = HashMap::from([("host".into(), HOST.into())]);
            if let Some(origin) = origin {
                headers.insert("origin".into(), origin.into());
            }
            if let Some(authorization) = authorization {
                headers.insert("authorization".into(), authorization.into());
            }
            Request {
                method: "GET".into(),
                path: "/state".into(),
                headers,
                body: Vec::new(),
            }
        };
        assert!(authorized_state(
            &request(None, Some("Bearer owner")),
            "owner"
        ));
        assert!(authorized_state(
            &request(Some(ORIGIN), Some("Bearer owner")),
            "owner"
        ));
        assert!(!authorized_state(
            &request(Some("http://127.0.0.1:18777"), Some("Bearer owner")),
            "owner"
        ));
        assert!(!authorized_state(&request(None, None), "owner"));
        let mut mutation = request(None, Some("Bearer owner"));
        mutation.method = "POST".into();
        assert!(!authorized_state(&mutation, "owner"));
    }

    #[test]
    fn snapshots_reuse_recent_activity_without_writing_a_profile() {
        let root = super::super::tests::tmp("activity-snapshot");
        let paths = Paths {
            home: root.join("home"),
            agent_data: root.join("agent"),
        };
        let mut state = State::new();
        state.standings = J::Arr(vec![J::Obj(vec![("name".into(), J::s("Cached employee"))])]);
        state.activity_last_checked = Some(Instant::now());
        let cached = snapshot(&paths, &mut state);
        assert_eq!(
            cached.arr("standings")[0].str("name"),
            Some("Cached employee")
        );
        assert!(!paths.home.join("company.json").exists());

        state.activity_last_checked = Some(Instant::now() - Duration::from_secs(16));
        let refreshed = snapshot(&paths, &mut state);
        assert!(refreshed.arr("standings").is_empty());
        assert!(!paths.home.join("company.json").exists());
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
        let root = super::super::tests::tmp("html-shell");
        let paths = Paths {
            home: root.join("home"),
            agent_data: root.join("agent"),
        };
        let html = html(&paths);
        assert!(html.contains("/workbench.js"));
        assert!(html.contains("id=\"conductor-view\""));
        assert!(html.contains("id=\"conductor-map\""));
        assert!(html.contains("id=\"conductor-first-run\""));
        assert!(html.contains("data-profile-id=\""));
        assert!(
            !html.contains("fleet.key") && !html.contains("Bearer ") && !html.contains("192.168.")
        );
        assert!(html.contains("id=\"prepare\" disabled"));
    }

    #[test]
    fn profile_cache_id_is_stable_and_scoped() {
        let first = super::profile_cache_id(std::path::Path::new("/users/alex/tempo"));
        assert_eq!(
            first,
            super::profile_cache_id(std::path::Path::new("/users/alex/tempo"))
        );
        assert_ne!(
            first,
            super::profile_cache_id(std::path::Path::new("/users/alex/other"))
        );
        assert_eq!(first.len(), 16);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
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
