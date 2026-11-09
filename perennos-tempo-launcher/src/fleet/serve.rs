//! `tempo-launch fleet serve`: the messenger's listener, its senders and its heartbeat.
//!
//! It listens only on this computer's private and tailnet addresses and on 127.0.0.1, never on
//! 0.0.0.0. The one wildcard socket in the program, mDNS on UDP 5353, is off unless `--announce`. One port carries three things, told apart by the first bytes: signed frames, a plain
//! HTTP (`/hello` to any private address; `/fleet.json` and `POST /call` to this computer only) and
//! pairing, which starts with `PAIR`.

use super::msg::{lock, Node};
use super::store::{self, now_s, Fleet, Paths};
use crate::setup::json::{parse, J};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// A machine that has not spoken for this long (three heartbeats missed) is shown as away.
pub const AWAY_S: u64 = 35;

// ---- what this computer says about itself

fn load_per_cpu() -> Option<f64> {
    let t = std::fs::read_to_string("/proc/loadavg").ok().or_else(|| {
        crate::setup::probe::run_with_timeout(
            "sysctl",
            &["-n", "vm.loadavg"],
            Duration::from_secs(1),
        )
        .map(|s| s.replace(['{', '}'], ""))
    })?;
    let one: f64 = t.split_whitespace().next()?.parse().ok()?;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    Some((one / cpus * 100.0).round() / 100.0)
}

fn loaded_models() -> Vec<String> {
    let port = crate::setup::apply::ollama_port();
    crate::http_get_at(
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        &format!("127.0.0.1:{port}"),
        "/api/ps",
        Duration::from_millis(600),
    )
    .filter(|(c, _)| *c == 200)
    .and_then(|(_, b)| parse(&b).ok())
    .map(|j| {
        j.arr("models")
            .iter()
            .filter_map(|m| m.str("name").map(String::from))
            .collect()
    })
    .unwrap_or_default()
}

/// Free graphics memory on an NVIDIA card, if there is one.
fn vram_free() -> Option<f64> {
    match crate::setup::probe::probe_gpu(&crate::setup::probe::RealHost) {
        crate::setup::probe::Gpu::Nvidia { free_gb, .. } => Some((free_gb * 10.0).round() / 10.0),
        _ => None,
    }
}

/// The last measured speed of the model loaded now: one fixed short prompt, timed once, kept
/// until the model (or Ollama's version) changes. Not measured while this computer is busy.
static TPS: Mutex<Option<(String, f64)>> = Mutex::new(None);

fn tps_now() -> Option<f64> {
    let model = loaded_models().into_iter().next()?;
    let port = crate::setup::apply::ollama_port();
    let version = crate::http_get_at(
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        &format!("127.0.0.1:{port}"),
        "/api/version",
        Duration::from_millis(600),
    )
    .and_then(|(_, b)| crate::json_str(&b, "version"))
    .unwrap_or_default();
    let key = format!("{model}@{version}");
    if let Some((k, v)) = lock(&TPS).as_ref() {
        if *k == key {
            return Some(*v);
        }
    }
    let idle = super::route::IN_FLIGHT.load(Ordering::SeqCst) == 0
        && load_per_cpu().is_none_or(|l| l < 0.5);
    if !idle {
        return lock(&TPS).as_ref().map(|x| x.1);
    }
    let v = super::route::measure_tps(port, &model)?;
    *lock(&TPS) = Some((key, v));
    Some(v)
}

fn agent_up() -> bool {
    let a = SocketAddr::from((Ipv4Addr::LOCALHOST, crate::AGENT.port));
    TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok()
}

/// The heartbeat body: load, memory, the models loaded, the queue, and what this computer offers.
pub fn health(node: &Node, with_tools: bool) -> J {
    let tools = with_tools
        .then(|| super::tools::advert(&node.paths, false))
        .flatten();
    health_with(node.queued(None), tools)
}

pub fn health_with(queued: usize, tools: Option<J>) -> J {
    let (total, free) = crate::setup::probe::probe_memory(&crate::setup::probe::RealHost);
    let r1 = |v: Option<f64>| v.map_or(J::Null, |x| J::n((x * 10.0).round() / 10.0));
    let mut o = vec![
        ("load".to_string(), load_per_cpu().map_or(J::Null, J::n)),
        (
            "cpus".into(),
            J::n(std::thread::available_parallelism().map_or(1, |n| n.get()) as f64),
        ),
        ("mem_free_gb".into(), r1(free)),
        ("mem_total_gb".into(), r1(total)),
        ("models".into(), J::strings(&loaded_models())),
        ("queue".into(), J::n(queued as f64)),
        ("jobs".into(), J::Bool(agent_up())),
        (
            "in_flight".into(),
            J::n(super::route::IN_FLIGHT.load(Ordering::SeqCst) as f64),
        ),
        ("vram_free_gb".into(), vram_free().map_or(J::Null, J::n)),
        ("tps".into(), tps_now().map_or(J::Null, J::n)),
        ("ver".into(), J::s(crate::VERSION)),
        ("at".into(), J::n(now_s() as f64)),
    ];
    if let Some(t) = tools {
        o.push(("tools".into(), t));
    }
    J::Obj(o)
}

/// A heartbeat arrived: keep what it said in fleet.json.
pub fn on_heartbeat(node: &Node, from: &str, body: &[u8]) {
    let Some(j) = std::str::from_utf8(body).ok().and_then(|t| parse(t).ok()) else {
        return;
    };
    let _ = store::update(&node.paths, |f| {
        if let Some(m) = f.machines.iter_mut().find(|m| m.name == from) {
            m.last_seen = now_s();
            // the offer rides on every sixth heartbeat: between those the last one stands
            if let Some(t) = j.get("tools") {
                m.tools = t.clone();
            } else if j.get("tools_none") == Some(&J::Bool(true)) {
                m.tools = J::Null;
            }
            let mut h = j;
            if let J::Obj(o) = &mut h {
                o.retain(|(k, _)| k != "tools" && k != "tools_none");
            }
            m.health = h;
        }
    });
}

// ---- the page of facts the GUI and the fleet page read

fn num(v: u64) -> J {
    J::n(v as f64)
}

pub fn view(p: &Paths, node: Option<&Node>) -> J {
    let f = store::load(p);
    let now = now_s();
    let machines: Vec<J> = f
        .machines
        .iter()
        .map(|m| {
            let age = now.saturating_sub(m.last_seen);
            let mut o = vec![
                ("name".to_string(), J::s(&m.name)),
                ("addr".into(), J::s(&m.addr)),
                ("role".into(), J::s(&m.role)),
                ("enabled".into(), J::Bool(m.enabled)),
                ("last_seen".into(), num(m.last_seen)),
                (
                    "up".into(),
                    J::Bool(m.enabled && m.last_seen > 0 && age < AWAY_S),
                ),
                ("health".into(), m.health.clone()),
                ("tools".into(), m.tools.clone()),
                ("ssh".into(), m.ssh.clone()),
            ];
            if m.last_seen > 0 {
                o.push(("age_s".into(), num(age)));
            }
            J::Obj(o)
        })
        .collect();
    J::Obj(vec![
        ("heading".into(), J::s(&super::heading(f.count()))),
        ("count".into(), num(f.count() as u64)),
        ("self".into(), J::s(&f.me)),
        (
            "queued".into(),
            num(node.map_or(0, |n| n.queued(None)) as u64),
        ),
        ("machines".into(), J::Arr(machines)),
    ])
}

// ---- connections

fn respond(s: &mut TcpStream, code: &str, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// A plain GET. `/hello` for a private address; `/fleet.json` for this computer only, and only
/// with a Host header that names it (a web page cannot reach it by a name it controls).
fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
}

fn http(node: &Arc<Node>, mut r: BufReader<TcpStream>, peer: SocketAddr, port: u16) {
    let mut head = String::new();
    let mut line = String::new();
    while head.len() < 8192 && r.read_line(&mut line).unwrap_or(0) > 0 {
        let end = line == "\r\n" || line == "\n";
        head.push_str(&line);
        line.clear();
        if end {
            break;
        }
    }
    let first = head.lines().next().unwrap_or("");
    let path = first.split_whitespace().nth(1).unwrap_or("");
    let post = first.starts_with("POST ");
    let host = header(&head, "host")
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    let token = header(&head, "x-tempo-token").unwrap_or_default();
    let want: usize = header(&head, "content-length")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; want.min(1 << 17)];
    if post && want <= 1 << 17 {
        let _ = std::io::Read::read_exact(&mut r, &mut body);
    }
    let mut s = r.into_inner();
    let local = peer.ip().is_loopback()
        && [format!("127.0.0.1:{port}"), format!("localhost:{port}")].contains(&host);
    if post {
        // the agent's door to the fleet's tools: this computer, a Host that names it, and the token
        let ok = local
            && want <= 1 << 17
            && super::crypto::same(
                token.as_bytes(),
                super::tools::local_token(&node.paths).as_bytes(),
            );
        if path == "/call" && ok {
            let (code, out) = super::tools::http_call(node, &body);
            respond(&mut s, code, &out);
        } else {
            respond(
                &mut s,
                "403 Forbidden",
                "{\"error\":\"this computer only, with its token\"}",
            );
        }
        return;
    }
    match path {
        "/hello" if crate::private_ip(peer.ip()) => {
            let o = J::Obj(vec![
                ("app".into(), J::s("tempo-fleet")),
                ("name".into(), J::s(&node.me)),
                ("v".into(), J::n(1.0)),
                (
                    "invite".into(),
                    J::Bool(super::pair::invite_open(&node.paths)),
                ),
            ]);
            respond(&mut s, "200 OK", &o.compact());
        }
        "/fleet.json" if local => {
            respond(&mut s, "200 OK", &view(&node.paths, Some(node)).compact())
        }
        "/fleet.json" => respond(
            &mut s,
            "403 Forbidden",
            "{\"error\":\"this computer only\"}",
        ),
        _ => respond(&mut s, "404 Not Found", "{\"error\":\"not found\"}"),
    }
}

pub fn conn(node: &Arc<Node>, s: TcpStream) {
    let Ok(peer) = s.peer_addr() else { return };
    let port = s.local_addr().map_or(0, |a| a.port());
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(300)));
    let mut r = BufReader::new(s);
    let first = match r.fill_buf() {
        Ok(b) if b.len() >= 4 => [b[0], b[1], b[2], b[3]],
        _ => return,
    };
    match &first {
        b"GET " | b"POST" => http(node, r, peer, port),
        b"PAIR" => super::pair::serve_pair(node, r, peer),
        _ => node.frames(r),
    }
}

pub struct Handle {
    pub addrs: Vec<SocketAddr>,
    stop: Arc<AtomicBool>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for a in &self.addrs {
            let _ = TcpStream::connect_timeout(a, Duration::from_millis(200));
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Listen on `addrs` and nothing else (what a test of the receiving side wants).
pub fn listen(node: &Arc<Node>, addrs: &[SocketAddr]) -> Result<Handle, String> {
    let _ = super::tools::local_token(&node.paths);
    let stop = Arc::new(AtomicBool::new(false));
    let mut bound = Vec::new();
    for a in addrs {
        if let Ok(l) = TcpListener::bind(a) {
            bound.push(l.local_addr().map_err(|e| e.to_string())?);
            let (node, stop) = (node.clone(), stop.clone());
            std::thread::spawn(move || {
                for c in l.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Ok(c) = c {
                        let node = node.clone();
                        std::thread::spawn(move || conn(&node, c));
                    }
                }
            });
        }
    }
    if bound.is_empty() {
        return Err("no address could be listened on (is it already running?)".into());
    }
    Ok(Handle { addrs: bound, stop })
}

/// Listen, send what waits in the outbox, send heartbeats. Returns when listening.
pub fn start(node: &Arc<Node>, addrs: &[SocketAddr]) -> Result<Handle, String> {
    let h = listen(node, addrs)?;
    let stop = h.stop.clone();
    let hb: Arc<Mutex<(u64, Vec<u8>)>> = Arc::new(Mutex::new((0, Vec::new())));
    {
        let (node, stop, hb) = (node.clone(), stop.clone(), hb.clone());
        std::thread::spawn(move || {
            let mut tick = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let mut h = health(&node, tick.is_multiple_of(6));
                if tick.is_multiple_of(6) && h.get("tools").is_none() {
                    h.set("tools_none", J::Bool(true));
                }
                tick += 1;
                let body = h.compact().into_bytes();
                {
                    let mut g = lock(&hb);
                    *g = (g.0 + 1, body);
                }
                let end = Instant::now() + HEARTBEAT;
                while Instant::now() < end && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
    }
    {
        let (node, stop) = (node.clone(), stop.clone());
        std::thread::spawn(move || supervise(&node, &stop, &hb));
    }
    Ok(h)
}

/// One sender per machine in the fleet: it delivers what is due and sends the heartbeat.
fn supervise(node: &Arc<Node>, stop: &Arc<AtomicBool>, hb: &Arc<Mutex<(u64, Vec<u8>)>>) {
    let running: Arc<Mutex<HashSet<String>>> = Arc::default();
    let cell: Arc<Mutex<Fleet>> = Arc::default();
    while !stop.load(Ordering::SeqCst) {
        let fleet = store::load(&node.paths);
        for m in fleet.peers().filter(|m| m.name != node.me) {
            if lock(&running).insert(m.name.clone()) {
                let (node, stop, hb, cell, running, name) = (
                    node.clone(),
                    stop.clone(),
                    hb.clone(),
                    cell.clone(),
                    running.clone(),
                    m.name.clone(),
                );
                std::thread::spawn(move || {
                    sender(&node, &stop, &hb, &cell, &name);
                    lock(&running).remove(&name);
                });
            }
        }
        *lock(&cell) = fleet;
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn sender(
    node: &Arc<Node>,
    stop: &AtomicBool,
    hb: &Mutex<(u64, Vec<u8>)>,
    cell: &Mutex<Fleet>,
    name: &str,
) {
    let mut sent = 0;
    while !stop.load(Ordering::SeqCst) {
        let Some(m) = lock(cell).peers().find(|m| m.name == name).cloned() else {
            return;
        };
        let _ = node.deliver(&m);
        let (seq, body) = lock(hb).clone();
        if seq != sent && !body.is_empty() {
            sent = seq;
            let _ = node.ephemeral(&m, "heartbeat", &body);
        }
        let g = lock(&node.kick.0);
        let _ = node.kick.1.wait_timeout(g, Duration::from_millis(50));
    }
}

/// Where the messenger may listen: the private and tailnet addresses of this computer, and
/// 127.0.0.1. Never 0.0.0.0.
pub fn listen_on(port: u16) -> Vec<SocketAddr> {
    let mut v: Vec<IpAddr> = super::scan::listen_addrs(&super::scan::local_addrs())
        .into_iter()
        .map(IpAddr::V4)
        .collect();
    v.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    v.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()
}

/// `fleet serve`: listen until the program is ended.
pub fn run(p: &Paths, quiet: bool, announce: bool) -> i32 {
    let Some(key) = store::read_key(p) else {
        eprintln!(
            "tempo-launch: fleet serve: there is no fleet key yet; run tempo-launch fleet init"
        );
        return 1;
    };
    let node = match Node::open(p, key) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("tempo-launch: fleet serve: {e}");
            return 1;
        }
    };
    lock(&node.handlers).push(Arc::new(super::jobs::JobHandler(Arc::new(
        super::jobs::AgentRunner(p.clone()),
    ))));
    lock(&node.handlers).push(Arc::new(super::tools::CallHandler::new(
        p.clone(),
        Arc::new(super::tools::RealRunner(p.clone())),
    )));
    super::blob::attach(&node);
    lock(&node.handlers).push(Arc::new(super::stage::StageHandler::new(
        p.clone(),
        Arc::new(crate::stage::remote::RealRunner),
    )));
    let addrs = listen_on(store::fleet_port());
    let h = match start(&node, &addrs) {
        Ok(h) => h,
        Err(_) if quiet => return 0, // the messenger is already running
        Err(e) => {
            eprintln!("tempo-launch: fleet serve: {e}");
            return 1;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let own = super::scan::listen_addrs(&super::scan::local_addrs());
    if announce {
        // answering mDNS needs UDP 5353 on every interface, which is what makes a firewall ask: only when asked for
        let (name, port, stop) = (node.me.clone(), store::fleet_port(), stop.clone());
        std::thread::spawn(move || {
            if let Err(e) = super::mdns::announce(&stop, &name, port, &own, &["v=1".to_string()]) {
                eprintln!("fleet: {e}");
            }
        });
    }
    if !quiet {
        let list: Vec<String> = h.addrs.iter().map(|a| a.to_string()).collect();
        println!(
            "Fleet messenger {} listening on {} (private addresses and this computer only).",
            node.me,
            list.join(", ")
        );
    }
    loop {
        std::thread::park();
    }
}
