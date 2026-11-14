use super::msg::{encode, read_frame, Bad, Frame, Node};
use super::serve::{self, Handle};
use super::store::{self, Machine, Paths};
use super::*;
use ed25519_dalek::SigningKey;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

/// An exclusively created test folder. Unconfirmed work keeps its scratch.
struct Scratch {
    path: PathBuf,
    token: Vec<u8>,
    nodes: Vec<Arc<Node>>,
    servers: Vec<Arc<serve::Completion>>,
    cleaned: bool,
}

impl Scratch {
    fn create(path: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir(&path)?;
        let token = crypto::random(32);
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join(".fixture-owner"))?;
        marker.write_all(&token)?;
        Ok(Self {
            path,
            token,
            nodes: Vec::new(),
            servers: Vec::new(),
            cleaned: false,
        })
    }

    fn remember_node(&mut self, node: &Arc<Node>) {
        if !self.nodes.iter().any(|saved| Arc::ptr_eq(saved, node)) {
            self.nodes.push(node.clone());
        }
    }

    fn cleanup(&mut self, limit: Duration) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + limit;
        for server in &self.servers {
            server
                .wait(deadline.saturating_duration_since(std::time::Instant::now()))
                .map_err(str::to_string)?;
        }
        for node in &self.nodes {
            if !node
                .handlers
                .lock()
                .map_err(|_| "node handler lock poisoned")?
                .is_empty()
            {
                return Err("node handlers may own untracked work".into());
            }
        }
        if self.nodes.iter().any(|node| Arc::strong_count(node) != 1) {
            return Err("node still has unconfirmed owners".into());
        }
        // Scratch owns the last strong references; dropping them here completes Node destructors.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.nodes.clear())).is_err() {
            return Err("node destructor panicked".into());
        }
        let marker = self.path.join(".fixture-owner");
        if !std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.file_type().is_dir())
            || !std::fs::symlink_metadata(&marker).is_ok_and(|m| m.file_type().is_file())
            || !std::fs::read(marker).is_ok_and(|token| token == self.token)
        {
            return Err("scratch ownership marker is missing or changed".into());
        }
        let mut removal_error = None;
        for attempt in 0..20 {
            match std::fs::remove_dir_all(&self.path) {
                Ok(()) => {
                    self.cleaned = true;
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.cleaned = true;
                    return Ok(());
                }
                Err(e) => {
                    removal_error = Some(e);
                    if attempt < 19 {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
        Err(format!(
            "scratch removal failed: {}",
            removal_error.unwrap()
        ))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(reason) = self.cleanup(Duration::from_secs(3)) {
            eprintln!(
                "Fleet test scratch retained at {}: {reason}",
                self.path.display()
            );
        }
    }
}

// Rust 1.99's Windows GNU backend still exposes an initializer function to this lint
// even though the source initializer below is already const. Keep the expectation local;
// an unfulfilled expectation asks us to remove it when the compiler stops reporting it.
thread_local! {
    #[cfg_attr(
        all(windows, target_env = "gnu"),
        expect(clippy::missing_const_for_thread_local)
    )]
    static MINE: std::cell::RefCell<Vec<Scratch>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn tmp(tag: &str) -> PathBuf {
    for _ in 0..32 {
        let d = std::env::temp_dir().join(format!(
            "tempo-fleet-{tag}-{}-{}",
            std::process::id(),
            crypto::hex(&crypto::random(16))
        ));
        match Scratch::create(d.clone()) {
            Ok(scratch) => {
                let fixture = d.join("fixture");
                std::fs::create_dir(&fixture).expect("cannot create Fleet test fixture");
                MINE.with(|m| m.borrow_mut().push(scratch));
                return fixture;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot create owned Fleet test scratch: {e}"),
        }
    }
    panic!("could not allocate unique Fleet test scratch")
}

pub fn paths(tag: &str) -> Paths {
    let d = tmp(tag);
    Paths {
        home: d.join("home"),
        agent_data: d.join("home").join("agent"),
    }
}

pub fn open_node(paths: &Paths, key: [u8; 32]) -> Result<Arc<Node>, String> {
    let node = Node::open(paths, key)?;
    MINE.with(|m| {
        let mut fixtures = m.borrow_mut();
        let scratch = fixtures
            .iter_mut()
            .find(|s| paths.home.starts_with(&s.path))
            .expect("node must belong to this test's owned scratch");
        scratch.remember_node(&node);
    });
    Ok(node)
}

pub(super) fn track_server(
    node: &Arc<Node>,
    completion: Arc<serve::Completion>,
) -> Result<(), String> {
    MINE.with(|m| {
        let mut fixtures = m.borrow_mut();
        let scratch = fixtures
            .iter_mut()
            .find(|s| node.paths.home.starts_with(&s.path))
            .ok_or("test server requires owned scratch")?;
        scratch.remember_node(node);
        scratch.servers.push(completion);
        Ok(())
    })
}

fn scratch_for_check(tag: &str) -> Scratch {
    let path = std::env::temp_dir().join(format!(
        "tempo-fleet-check-{tag}-{}",
        crypto::hex(&crypto::random(16))
    ));
    Scratch::create(path).unwrap()
}

#[test]
fn scratch_refuses_existing_directories_and_cleans_its_own_on_panic() {
    let root = tmp("scratch-ownership");
    let existing = root.join("existing");
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("keep"), b"not ours").unwrap();
    assert_eq!(
        Scratch::create(existing.clone()).err().unwrap().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(std::fs::read(existing.join("keep")).unwrap(), b"not ours");
    let scratch = scratch_for_check("panic");
    let owned = scratch.path.clone();
    let result = std::panic::catch_unwind(|| {
        let _scratch = scratch;
        std::fs::write(owned.join("fixture"), b"ours").unwrap();
        panic!("fixture panic");
    });
    assert!(result.is_err());
    assert!(!owned.exists());
    assert!(existing.join("keep").exists());
    let mut scratch = scratch_for_check("marker");
    let uncertain = scratch.path.clone();
    std::fs::write(uncertain.join(".fixture-owner"), b"changed").unwrap();
    assert!(scratch.cleanup(Duration::ZERO).is_err());
    assert!(uncertain.exists());
    std::fs::write(uncertain.join(".fixture-owner"), &scratch.token).unwrap();
    scratch.cleanup(Duration::ZERO).unwrap();
}

#[test]
fn scratch_waits_for_its_nodes_before_cleanup() {
    let mut scratch = scratch_for_check("node");
    let owned = scratch.path.clone();
    let paths = Paths {
        home: owned.join("home"),
        agent_data: owned.join("agent"),
    };
    std::fs::create_dir(&paths.home).unwrap();
    let node = Node::open(&paths, [7; 32]).unwrap();
    scratch.remember_node(&node);
    assert!(scratch.cleanup(Duration::ZERO).is_err());
    drop(node);
    scratch.cleanup(Duration::ZERO).unwrap();
    assert!(!owned.exists());
}

#[test]
fn scratch_retains_nodes_with_untracked_handlers() {
    let mut scratch = scratch_for_check("untracked-handler");
    let owned = scratch.path.clone();
    let paths = Paths {
        home: owned.join("home"),
        agent_data: owned.join("agent"),
    };
    std::fs::create_dir(&paths.home).unwrap();
    let node = Node::open(&paths, [7; 32]).unwrap();
    let handler = super::blob::attach(&node);
    scratch.remember_node(&node);
    drop(handler);
    drop(node);
    assert_eq!(
        scratch.cleanup(Duration::ZERO),
        Err("node handlers may own untracked work".into())
    );
    assert!(owned.exists());
    scratch.nodes[0].handlers.lock().unwrap().clear();
    scratch.cleanup(Duration::ZERO).unwrap();
    assert!(!owned.exists());
}

#[test]
fn server_without_registered_scratch_is_refused_before_listening() {
    let mut scratch = scratch_for_check("unregistered-server");
    let paths = Paths {
        home: scratch.path.join("home"),
        agent_data: scratch.path.join("agent"),
    };
    std::fs::create_dir(&paths.home).unwrap();
    let node = Node::open(&paths, [7; 32]).unwrap();
    let error = serve::listen(&node, &[SocketAddr::from(([127, 0, 0, 1], 0))])
        .err()
        .unwrap();
    assert!(error.contains("owned scratch"));
    drop(node);
    scratch.cleanup(Duration::ZERO).unwrap();
}

#[test]
fn scratch_retains_a_blocked_handler_until_completion() {
    let mut scratch = scratch_for_check("blocked");
    let owned = scratch.path.clone();
    let tracker = Arc::new(serve::Completion::default());
    scratch.servers.push(tracker.clone());
    let (started, ready) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    tracker.spawn(move || {
        started.send(()).unwrap();
        let _ = blocked.recv();
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(scratch.cleanup(Duration::from_millis(20)).is_err());
    assert!(owned.exists());
    release.send(()).unwrap();
    scratch.cleanup(Duration::from_secs(2)).unwrap();
    assert!(!owned.exists());
}

#[test]
fn real_blocked_handler_retains_scratch_until_peer_closes() {
    let paths = paths("real-blocked-handler");
    store::update(&paths, |f| f.me = "blocked".into()).unwrap();
    let node = open_node(&paths, [7; 32]).unwrap();
    let handle = serve::listen(&node, &[SocketAddr::from(([127, 0, 0, 1], 0))]).unwrap();
    let completion = handle.completion();
    let client = TcpStream::connect_timeout(&handle.addrs[0], Duration::from_secs(1)).unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while completion.active() < 2 && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(completion.active() >= 2, "handler was not tracked");
    handle.stop();
    drop(handle);
    let timed_out = MINE.with(|m| {
        let mut fixtures = m.borrow_mut();
        let scratch = fixtures
            .iter_mut()
            .find(|s| paths.home.starts_with(&s.path))
            .unwrap();
        scratch.cleanup(Duration::from_millis(20))
    });
    assert_eq!(timed_out, Err("server completion timed out".into()));
    assert!(paths.home.exists());
    drop(client);
    completion.wait(Duration::from_secs(2)).unwrap();
    drop(node);
    MINE.with(|m| {
        let mut fixtures = m.borrow_mut();
        fixtures
            .iter_mut()
            .find(|s| paths.home.starts_with(&s.path))
            .unwrap()
            .cleanup(Duration::from_secs(2))
            .unwrap();
    });
    assert!(!paths.home.exists());
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub struct Rig {
    pub paths: Paths,
    pub node: Arc<Node>,
    pub port: u16,
    pub handle: Option<Handle>,
}

fn stop_for_restart(rig: Rig) -> Result<Paths, String> {
    let Rig {
        paths,
        node,
        handle,
        ..
    } = rig;
    if let Some(handle) = handle {
        let completion = handle.completion();
        handle.stop();
        drop(handle);
        completion
            .wait(Duration::from_secs(3))
            .map_err(str::to_string)?;
    }
    if !node
        .handlers
        .lock()
        .map_err(|_| "node handler lock poisoned")?
        .is_empty()
    {
        return Err("restart node has untracked handler work".into());
    }
    MINE.with(|m| -> Result<(), String> {
        let mut fixtures = m.borrow_mut();
        let scratch = fixtures
            .iter_mut()
            .find(|s| paths.home.starts_with(&s.path))
            .ok_or("restart requires owned scratch")?;
        let index = scratch
            .nodes
            .iter()
            .position(|saved| Arc::ptr_eq(saved, &node))
            .ok_or("restart node is not owned")?;
        if Arc::strong_count(&node) != 2 {
            return Err("restart node still has unconfirmed owners".into());
        }
        drop(scratch.nodes.remove(index));
        Ok(())
    })?;
    drop(node);
    Ok(paths)
}

/// A node named `name` listening only (no senders) on this computer, with the shared `key`.
pub fn rig(name: &str, key: [u8; 32]) -> Rig {
    let paths = paths(name);
    store::update(&paths, |f| f.me = name.into()).unwrap();
    let node = open_node(&paths, key).unwrap();
    // A free port can be taken by a parallel test between asking and binding, so try a few.
    let (port, handle) = (0..20)
        .find_map(|_| {
            let port = free_port();
            serve::listen(&node, &[SocketAddr::from(([127, 0, 0, 1], port))])
                .ok()
                .map(|h| (port, h))
        })
        .expect("no free port on 127.0.0.1 after 20 tries");
    Rig {
        paths,
        node,
        port,
        handle: Some(handle),
    }
}

/// Tell `a` about `b` (as a fleet machine on this computer's own address).
pub fn know(a: &Rig, b_name: &str, b_port: u16, id: &str) -> Machine {
    let mut m = Machine::new(b_name, "127.0.0.1", "both");
    m.port = b_port;
    m.id = id.to_string();
    let mm = m.clone();
    store::update(&a.paths, |f| f.machines.push(mm)).unwrap();
    m
}

/// Each knows and pins the other: what pairing does. Returns `b` as `a` knows it.
pub fn link(a: &Rig, b: &Rig) -> Machine {
    let mb = know(a, &b.node.me, b.port, &b.node.identity_hex());
    know(b, &a.node.me, a.port, &a.node.identity_hex());
    mb
}

const KEY: [u8; 32] = [7; 32];

fn out_of(f: impl FnOnce(&mut Vec<u8>) -> i32) -> (i32, String) {
    let mut v = Vec::new();
    let c = f(&mut v);
    (c, String::from_utf8(v).unwrap())
}

// ---- the heading

#[test]
fn the_heading_counts_in_words_and_pluralises() {
    assert_eq!(heading(1), "One machine, one field.");
    assert_eq!(heading(2), "Two machines, one field.");
    assert_eq!(heading(3), "Three machines, one field.");
    assert_eq!(heading(12), "Twelve machines, one field.");
    assert_eq!(heading(21), "Twenty-one machines, one field.");
    assert_eq!(heading(40), "Forty machines, one field.");
    assert_eq!(heading(100), "100 machines, one field.");
    assert_eq!(words(0), "zero");
    assert_eq!(words(99), "ninety-nine");
}

#[test]
fn the_heading_is_the_real_count_from_the_fleet() {
    let p = paths("heading");
    assert_eq!(heading(store::load(&p).count()), "One machine, one field.");
    store::update(&p, |f| {
        f.machines.push(Machine::new("a", "10.0.0.2", "engine"));
        let mut b = Machine::new("b", "10.0.0.3", "engine");
        b.enabled = false;
        f.machines.push(b);
        f.machines.push(Machine::new("c", "10.0.0.4", "agent"));
    })
    .unwrap();
    // a machine switched off is not counted
    assert_eq!(heading_now(&p.ctx()), "Three machines, one field.");
    let html = page::section(&p, true);
    assert!(html.contains("Three machines, one field."), "{html}");
}

// ---- add, remove, toggle

fn settings(p: &Paths) -> J {
    parse(&std::fs::read_to_string(p.agent_data.join("config.json")).unwrap()).unwrap()
}

#[test]
fn add_remove_and_toggle_write_the_settings_and_keep_a_backup() {
    let p = paths("config");
    std::fs::create_dir_all(&p.agent_data).unwrap();
    std::fs::write(
        p.agent_data.join("config.json"),
        "{\"models\":[\"qwen3:8b\"],\"ollama_fallbacks\":[\"http://10.0.0.99:11434\"]}",
    )
    .unwrap();
    let (c, said) = out_of(|o| add(&p, None, "192.168.0.20", Some("engine"), Some("tower"), o));
    assert_eq!(c, 0, "{said}");
    let cfg = settings(&p);
    assert_eq!(
        cfg.strs("ollama_fallbacks"),
        ["http://10.0.0.99:11434", "http://192.168.0.20:11434"]
    );
    assert_eq!(
        cfg.get("hosts").unwrap().get("tower").unwrap().strs("urls"),
        ["http://192.168.0.20:11434"]
    );
    assert_eq!(
        cfg.strs("models"),
        ["qwen3:8b"],
        "what the user set is untouched"
    );
    let kept: Vec<_> = std::fs::read_dir(p.home.join("trash")).unwrap().collect();
    assert_eq!(kept.len(), 1, "the old file was backed up");
    let f = store::load(&p);
    assert_eq!(
        (
            f.machines.len(),
            f.machines[0].name.as_str(),
            f.machines[0].enabled
        ),
        (1, "tower", true)
    );

    // switched off: out of the settings, still known
    let (c, _) = out_of(|o| enable(&p, None, "tower", false, o));
    assert_eq!(c, 0);
    assert_eq!(
        settings(&p).strs("ollama_fallbacks"),
        ["http://10.0.0.99:11434"]
    );
    assert!(settings(&p).get("hosts").unwrap().get("tower").is_none());
    assert!(!store::load(&p).machines[0].enabled);
    // and back on, by its address
    let (c, _) = out_of(|o| enable(&p, None, "192.168.0.20", true, o));
    assert_eq!(c, 0);
    assert!(settings(&p)
        .strs("ollama_fallbacks")
        .contains(&"http://192.168.0.20:11434".to_string()));
    // adding it again changes nothing
    let (_, said) = out_of(|o| add(&p, None, "tower", None, None, o));
    assert!(
        said.contains("Switched on") && store::load(&p).machines.len() == 1,
        "{said}"
    );
    assert_eq!(store::load(&p).machines[0].addr, "192.168.0.20");
    assert_eq!(settings(&p).strs("ollama_fallbacks").len(), 2);

    let (c, _) = out_of(|o| remove(&p, None, "tower", o));
    assert_eq!(c, 0);
    assert!(store::load(&p).machines.is_empty());
    assert_eq!(
        settings(&p).strs("ollama_fallbacks"),
        ["http://10.0.0.99:11434"]
    );
    assert_eq!(out_of(|o| remove(&p, None, "tower", o)).0, 1);
}

#[test]
fn explicit_engine_address_update_migrates_only_unshared_fallbacks() {
    let p = paths("address-migration");
    std::fs::create_dir_all(&p.agent_data).unwrap();
    std::fs::write(
        p.agent_data.join("config.json"),
        "{\"ollama_fallbacks\":[\"http://10.0.0.99:11434\"]}",
    )
    .unwrap();
    let tower = Machine::new("tower", "192.168.0.20", "engine");
    store::update(&p, |f| f.machines.push(tower.clone())).unwrap();
    store::config_set(&p, None, &tower, true, store::now_s()).unwrap();
    let other = Machine::new("other", "192.168.0.20", "engine");
    store::update(&p, |f| f.machines.push(other.clone())).unwrap();
    store::config_set(&p, None, &other, true, store::now_s()).unwrap();

    let (code, said) = out_of(|o| add(&p, None, "192.168.0.21", Some("engine"), Some("tower"), o));
    assert_eq!(code, 0, "{said}");
    assert_eq!(
        settings(&p).strs("ollama_fallbacks"),
        [
            "http://10.0.0.99:11434",
            "http://192.168.0.20:11434",
            "http://192.168.0.21:11434"
        ]
    );
    assert_eq!(
        settings(&p)
            .get("hosts")
            .unwrap()
            .get("other")
            .unwrap()
            .strs("urls"),
        ["http://192.168.0.20:11434"]
    );

    assert_eq!(out_of(|o| remove(&p, None, "other", o)).0, 0);
    assert_eq!(
        settings(&p).strs("ollama_fallbacks"),
        ["http://10.0.0.99:11434", "http://192.168.0.21:11434"]
    );

    let (code, said) = out_of(|o| add(&p, None, "192.168.0.22", Some("engine"), Some("tower"), o));
    assert_eq!(code, 0, "{said}");
    assert_eq!(
        settings(&p).strs("ollama_fallbacks"),
        ["http://10.0.0.99:11434", "http://192.168.0.22:11434"]
    );
}

#[test]
fn a_public_address_is_refused_and_nothing_is_written() {
    let p = paths("public");
    for bad in [
        "8.8.8.8",
        "93.184.216.34",
        "example.com",
        "169.254.1.1",
        "0x7f000001",
        "10.0.0.1.evil.com",
    ] {
        let (c, said) = out_of(|o| add(&p, None, bad, Some("engine"), None, o));
        assert_eq!(c, 2, "{bad}: {said}");
    }
    assert!(!p.file("fleet.json").exists());
    assert!(!p.agent_data.join("config.json").exists());
    // a pinned settings file is never written; the line to add is printed
    let pin = tmp("pin").join("mine.json");
    std::fs::write(&pin, "{}").unwrap();
    let (c, said) = out_of(|o| {
        add(
            &p,
            Some(pin.clone()),
            "10.0.0.7",
            Some("engine"),
            Some("nas"),
            o,
        )
    });
    assert_eq!(c, 0);
    assert!(
        said.contains("leaves alone") && said.contains("http://10.0.0.7:11434"),
        "{said}"
    );
    assert_eq!(std::fs::read_to_string(&pin).unwrap(), "{}");
}

#[test]
fn an_agent_only_machine_does_not_touch_the_engines() {
    let p = paths("agentonly");
    let (c, _) = out_of(|o| add(&p, None, "10.0.0.8", Some("agent"), Some("laptop"), o));
    assert_eq!(c, 0);
    assert!(!p.agent_data.join("config.json").exists());
}

#[test]
fn the_command_line_parses() {
    let a = |x: &[&str]| parse_args(x);
    assert_eq!(a(&[]), Ok(FleetCmd::List));
    assert_eq!(a(&["scan", "--yes"]), Ok(FleetCmd::Scan { yes: true }));
    assert_eq!(
        a(&["add", "10.0.0.2", "--role", "both", "--name", "x"]),
        Ok(FleetCmd::Add {
            who: "10.0.0.2".into(),
            role: Some("both".into()),
            name: Some("x".into())
        })
    );
    assert!(a(&["add", "10.0.0.2", "--role", "king"]).is_err());
    assert_eq!(
        a(&["ssh", "tower", "--", "uname", "-a"]),
        Ok(FleetCmd::Ssh {
            who: "tower".into(),
            cmd: vec!["uname".into(), "-a".into()]
        })
    );
    assert!(a(&["ssh", "tower"]).is_err() && a(&["ssh", "tower", "--"]).is_err());
    assert_eq!(
        a(&["send", "tower", "hello there"]),
        Ok(FleetCmd::Send {
            who: "tower".into(),
            text: "hello there".into()
        })
    );
    let serve = |quiet, announce| Ok(FleetCmd::Serve { quiet, announce });
    assert_eq!(a(&["serve"]), serve(false, false));
    assert_eq!(a(&["serve", "--quiet"]), serve(true, false));
    assert_eq!(a(&["serve", "--announce"]), serve(false, true));
    assert_eq!(a(&["serve", "--announce", "--quiet"]), serve(true, true));
    assert!(a(&["serve", "--all"]).is_err());
    assert!(a(&["nope"]).is_err());
    assert!(matches!(
        crate::parse(&["fleet".to_string(), "ping".into(), "x".into()]),
        Ok(crate::Cmd::Fleet(FleetCmd::Ping(_)))
    ));
}

#[test]
fn the_first_scan_says_what_it_does_and_a_no_scans_nothing() {
    let p = paths("consent");
    let mut asked = 0;
    let (c, said) = out_of(|o| {
        scan_cmd(
            &p,
            false,
            &mut |_| {
                asked += 1;
                false
            },
            o,
        )
        .0
    });
    assert_eq!((c, asked), (0, 1));
    assert!(
        said.contains("What a scan does") && said.contains("300")
            || said.contains("third of a second"),
        "{said}"
    );
    assert!(said.contains("Nothing was scanned") && !p.file("fleet-scan.ok").exists());
}

// ---- ssh, with a fake ssh on PATH

#[cfg(unix)]
struct FakeSsh {
    dir: PathBuf,
    _override: ssh::TestSsh,
}

#[cfg(unix)]
impl std::ops::Deref for FakeSsh {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.dir
    }
}

#[cfg(unix)]
fn fake_ssh() -> FakeSsh {
    let dir = tmp("fake-ssh");
    let script = dir.join("ssh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
d="$(dirname "$0")"
echo "$@" >> "$d/log"
case "$*" in *10.0.0.33*) echo "Permission denied (publickey)." >&2; exit 255;; esac
case "$*" in
  *"-G "*) printf 'hostname 10.0.0.9\nport 22\nproxycommand none\nproxyjump none\n';;
  *"sh -s"*|*"sh -c"*)
    input=$(cat)
    printf '%s\n--END--\n' "$input" >> "$d/stdin"
    if [ "${#input}" -eq 64 ]; then printf 'The fixture received a 64-byte key payload.\n'; fi
    case "$input" in *__TEMPO_BOOTSTRAP_BEGIN__*) printf '__TEMPO_BOOTSTRAP_BEGIN__\nos=Linux\narch=x86_64\nuser=setupbox\nuser_home=/home/setupbox\nname=system-host\nlauncher=/home/setupbox/.local/bin/tempo-launch\nversion=tempo-launch 2.1.0\nhome_json={"home":"/home/setupbox/.local/state/perennos-tempo"}\nfleet_port=8770\n__TEMPO_BOOTSTRAP_END__\n';; esac
    case "$input" in *fleet.json*) printf 'key_hash=\nfleet_hex=__TEMPO_ABSENT__\n';; esac
    case "$input" in *"command -v curl"*) printf 'messenger=ready\n';; esac
    case "$input" in *nproc*) printf 'os=Linux x86_64\ncpus=8\nram_kb=16777216\ngpu=NVIDIA Test GPU, 8192 MiB\ndisk_kb=104857600\nlaunch=tempo-launch 2.0.1\nollama=ollama version is 0.5.1\n';; esac;;
  *) echo "ran: $*";;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakeSsh {
        dir,
        _override: ssh::test_ssh(script),
    }
}

#[cfg(unix)]
fn lines_with(dir: &std::path::Path, file: &str, needle: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(file))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .map(String::from)
        .collect()
}

#[cfg(unix)]
#[test]
fn ssh_runs_with_the_three_options_and_reports_what_the_other_side_said() {
    let dir = fake_ssh();
    let p = paths("sshrun");
    store::update(&p, |f| {
        f.machines
            .push(Machine::new("sshbox", "10.0.0.31", "engine"))
    })
    .unwrap();
    let (c, said) = out_of(|o| ssh_cmd(&p, "sshbox", &["uname".into(), "-a".into()], o));
    assert_eq!(c, 0);
    assert!(said.contains("ran:") && said.contains("uname -a"), "{said}");
    let l = lines_with(&dir, "log", "10.0.0.31");
    assert!(
        l[0].starts_with("-o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new 10.0.0.31 uname -a"),
        "{l:?}"
    );
    // not in the fleet and not in the ssh config: refused before ssh runs
    let (c, _) = out_of(|o| ssh_cmd(&p, "stranger", &["id".into()], o));
    assert_eq!(c, 2);
    assert!(lines_with(&dir, "log", "stranger").is_empty());
}

#[cfg(unix)]
#[test]
fn check_reads_the_machine_and_records_the_result_and_a_failure() {
    let _ssh = fake_ssh();
    let p = paths("sshcheck");
    store::update(&p, |f| {
        f.machines
            .push(Machine::new("checkbox", "10.0.0.32", "engine"));
        f.machines
            .push(Machine::new("badhost", "10.0.0.33", "engine"));
    })
    .unwrap();
    let (c, said) = out_of(|o| check_cmd(&p, "checkbox", o));
    assert_eq!(c, 0, "{said}");
    for want in [
        "Linux x86_64",
        "8 threads",
        "16.0 GB",
        "NVIDIA Test GPU",
        "100.0 GB",
        "tempo-launch 2.0.1",
        "0.5.1",
    ] {
        assert!(said.contains(want), "{want}: {said}");
    }
    let f = store::load(&p);
    let m = &f.machines[0];
    assert_eq!(m.ssh.bool("ok"), Some(true));
    assert_eq!(m.probe.f64("cpus"), Some(8.0));
    let (c, _) = out_of(|o| check_cmd(&p, "badhost", o));
    assert_eq!(c, 1);
    let m = &store::load(&p).machines[1];
    assert_eq!(m.ssh.bool("ok"), Some(false));
    assert!(m.ssh.str("note").unwrap().contains("Permission denied"));
}

#[cfg(unix)]
#[test]
fn setup_asks_and_keeps_the_key_on_stdin_when_pairing_cannot_be_verified() {
    let dir = fake_ssh();
    let p = paths("sshsetup");
    store::update(&p, |f| {
        f.machines
            .push(Machine::new("setupbox", "127.0.0.1", "engine"))
    })
    .unwrap();
    // a no: nothing runs after the look
    let mut asked = 0;
    let (c, said) = out_of(|o| {
        setup_cmd(
            &p,
            None,
            "setupbox",
            false,
            &mut |_| {
                asked += 1;
                false
            },
            o,
        )
    });
    assert_eq!((c, asked), (0, 1));
    assert!(
        said.contains("Perennos Tempo v2.1.0 will use /home/setupbox/.local/state/perennos-tempo"),
        "{said}"
    );
    assert!(said.contains("Nothing was changed"));
    // a yes
    let (c, said) = out_of(|o| setup_cmd(&p, None, "setupbox", true, &mut |_| true, o));
    assert_eq!(
        c, 1,
        "unverified pairing must not register the target: {said}"
    );
    let key = crypto::hex(&store::read_key(&p).unwrap());
    let stdin = std::fs::read_to_string(dir.join("stdin")).unwrap();
    let log = std::fs::read_to_string(dir.join("log")).unwrap();
    assert!(
        log.contains("fleet install-key --name "),
        "the remote install command is present without exposing its arguments"
    );
    assert!(stdin.contains(&key), "the key goes in SSH standard input");
    assert!(
        !log.contains(&key),
        "the key never appears on the command line"
    );
    let fleet = store::load(&p);
    assert!(
        fleet
            .find("setupbox")
            .is_some_and(|i| fleet.machines[i].id.is_empty()),
        "an unverified target stays unregistered"
    );
    let mode_ok = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p.file("fleet.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
                == 0o600
        }
    };
    assert!(mode_ok, "fleet.key is readable by its owner only");
}

#[test]
fn install_key_keeps_a_different_old_key_aside_and_names_the_peers() {
    let p = paths("installkey");
    let k1 = crypto::hex(&[1u8; 32]);
    let k2 = crypto::hex(&[2u8; 32]);
    let (c, _) = out_of(|o| {
        install_key(
            &p,
            Some("Remote Box"),
            &["main=10.0.0.4".into(), "evil=8.8.8.8".into()],
            &k1,
            o,
        )
    });
    assert_eq!(c, 0);
    assert_eq!(store::read_key(&p), Some([1u8; 32]));
    let f = store::load(&p);
    assert_eq!(f.me, "remote-box");
    assert_eq!(f.machines.len(), 1, "a public peer is not taken");
    let (c, _) = out_of(|o| install_key(&p, None, &[], &k2, o));
    assert_eq!(c, 0);
    assert_eq!(
        std::fs::read(p.file("fleet.key.old")).unwrap(),
        vec![1u8; 32]
    );
    assert_eq!(out_of(|o| install_key(&p, None, &[], "nothex", o)).0, 2);
}

// ---- the messenger

#[test]
fn a_frame_round_trips_and_a_wrong_key_or_a_flipped_byte_is_refused() {
    let f = Frame {
        kind: 'm',
        id: "i1".into(),
        from: "a".into(),
        to: "b".into(),
        topic: "note".into(),
        ts: 5,
        nonce: "n1".into(),
        created: 9,
        eph: false,
        body: vec![0, 1, 2, 255, b'\n', 10],
    };
    let sk = SigningKey::from_bytes(&[5; 32]);
    let vk = sk.verifying_key();
    let pins = |n: &str| (n == "a").then_some(vk);
    let wire = encode(&KEY, &sk, &f);
    assert_eq!(read_frame(&mut &wire[..], &KEY, &pins).unwrap(), f);
    assert_eq!(
        read_frame(&mut &wire[..], &[8; 32], &pins),
        Err(Bad::Refused("wrong key"))
    );
    let mut bad = wire.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert_eq!(
        read_frame(&mut &bad[..], &KEY, &pins),
        Err(Bad::Refused("wrong key"))
    );
    assert_eq!(read_frame(&mut &b""[..], &KEY, &pins), Err(Bad::Closed));
    // a length that is too big is refused before anything is read
    assert_eq!(
        read_frame(&mut &[0xff, 0xff, 0xff, 0xff][..], &KEY, &pins),
        Err(Bad::Refused("frame size"))
    );
    // the right fleet key, but a name nobody pinned, or an identity that is not the pinned one
    assert_eq!(
        read_frame(&mut &wire[..], &KEY, &|_| None),
        Err(Bad::Refused("not a pinned computer"))
    );
    let other = SigningKey::from_bytes(&[6; 32]).verifying_key();
    assert_eq!(
        read_frame(&mut &wire[..], &KEY, &|_| Some(other)),
        Err(Bad::Refused("wrong identity"))
    );
}

fn sample() -> (Frame, SigningKey) {
    let f = Frame {
        kind: 'm',
        id: "i1".into(),
        from: "a".into(),
        to: "b".into(),
        topic: "note".into(),
        ts: 5,
        nonce: "n1".into(),
        body: b"the plan is to meet at the old mill at noon".to_vec(),
        ..Frame::default()
    };
    (f, SigningKey::from_bytes(&[5; 32]))
}

#[test]
fn the_bytes_on_the_wire_never_contain_the_body_and_each_frame_has_its_own_seal() {
    let (f, sk) = sample();
    let wire = encode(&KEY, &sk, &f);
    let has = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    assert!(!has(&wire, &f.body), "plain text on the wire");
    assert!(
        !has(&wire, b"old mill"),
        "part of the plain text on the wire"
    );
    // the routing fields are readable (they are the associated data), the body is not
    assert!(has(&wire, br#""tp":"note""#));
    // a blob piece, which is just bytes, is sealed the same way
    let mut piece = f.clone();
    piece.body = vec![0xAB; 256 << 10];
    let wire = encode(&KEY, &sk, &piece);
    assert!(!has(&wire, &piece.body[..64]));
    assert_eq!(
        wire.len(),
        4 + 96 + wire_header_len(&wire) + 1 + (256 << 10) + 16
    );
    // the same frame twice: a new random nonce, so a different ciphertext
    assert_ne!(encode(&KEY, &sk, &f), encode(&KEY, &sk, &f));
    // and it opens for a pinned sender with the right key
    let vk = sk.verifying_key();
    let pins = |n: &str| (n == "a").then_some(vk);
    assert_eq!(
        read_frame(&mut &encode(&KEY, &sk, &f)[..], &KEY, &pins).unwrap(),
        f
    );
}

fn wire_header_len(wire: &[u8]) -> usize {
    wire[4 + 96..].iter().position(|b| *b == b'\n').unwrap()
}

#[test]
fn a_sealed_body_that_was_changed_does_not_open_and_is_counted_not_parsed() {
    let (f, sk) = sample();
    let vk = sk.verifying_key();
    let pins = |n: &str| (n == "a").then_some(vk);
    let before = crate::fleet::msg::unopened();
    // well formed, right MAC, right signature, one bit of the body changed: only the seal can tell
    let bad = crate::fleet::msg::build(&KEY, &sk, &f, true);
    assert_eq!(
        read_frame(&mut &bad[..], &KEY, &pins),
        Err(Bad::Refused("did not open"))
    );
    assert!(crate::fleet::msg::unopened() > before);
    // a flipped byte anywhere else is caught earlier, by the MAC
    let mut wire = encode(&KEY, &sk, &f);
    let at = wire.len() - 3;
    wire[at] ^= 1;
    assert_eq!(
        read_frame(&mut &wire[..], &KEY, &pins),
        Err(Bad::Refused("wrong key"))
    );
}

#[test]
fn a_header_is_bound_to_its_body_and_a_wrong_fleet_key_gets_nothing() {
    let (key_a, key_b) = ([1u8; 32], [2u8; 32]);
    let (mac_a, seal_a) = crypto::frame_keys(&key_a);
    let (mac_b, seal_b) = crypto::frame_keys(&key_b);
    // a rotated fleet key re-derives both, and the two never match each other
    assert!(mac_a != mac_b && seal_a != seal_b && mac_a != seal_a);
    let nonce = [3u8; 12];
    let ct = crypto::aead_seal(&seal_a, &nonce, b"header-1", b"hello");
    assert_eq!(
        crypto::aead_open(&seal_a, &nonce, b"header-1", &ct).as_deref(),
        Some(&b"hello"[..])
    );
    // the same sealed body under another header, nonce or key does not open
    assert!(crypto::aead_open(&seal_a, &nonce, b"header-2", &ct).is_none());
    assert!(crypto::aead_open(&seal_a, &[4u8; 12], b"header-1", &ct).is_none());
    assert!(crypto::aead_open(&seal_b, &nonce, b"header-1", &ct).is_none());
    // a frame made under one fleet key is refused under the other
    let (f, sk) = sample();
    let vk = sk.verifying_key();
    let wire = encode(&key_a, &sk, &f);
    assert_eq!(
        read_frame(&mut &wire[..], &key_b, &|_| Some(vk)),
        Err(Bad::Refused("wrong key"))
    );
}

fn vk_of(hex: &str) -> ed25519_dalek::VerifyingKey {
    let k = <[u8; 32]>::try_from(crypto::unhex(hex).unwrap().as_slice()).unwrap();
    ed25519_dalek::VerifyingKey::from_bytes(&k).unwrap()
}

fn raw_sk() -> SigningKey {
    SigningKey::from_bytes(&[9; 32])
}

fn raw_frame(to: &str, key: &[u8; 32], ts: u64, nonce: &str, topic: &str, id: &str) -> Vec<u8> {
    raw_frame_as("raw", &raw_sk(), to, key, ts, nonce, topic, id)
}

#[allow(clippy::too_many_arguments)]
fn raw_frame_as(
    from: &str,
    sk: &SigningKey,
    to: &str,
    key: &[u8; 32],
    ts: u64,
    nonce: &str,
    topic: &str,
    id: &str,
) -> Vec<u8> {
    encode(
        key,
        sk,
        &Frame {
            kind: 'm',
            id: id.into(),
            from: from.into(),
            to: to.into(),
            topic: topic.into(),
            ts,
            nonce: nonce.into(),
            body: b"hi".to_vec(),
            ..Frame::default()
        },
    )
}

/// Send bytes on a new connection; whether an answer frame came back.
fn answered(port: u16, wire: &[u8], who: &Rig) -> bool {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    s.write_all(wire).unwrap();
    let vk = vk_of(&who.node.identity_hex());
    read_frame(&mut s, &KEY, &|_| Some(vk)).is_ok()
}

fn fake_peer(owner: &Rig, name: &str, sk: &SigningKey) -> (Machine, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let id = crypto::hex(sk.verifying_key().as_bytes());
    (know(owner, name, port, &id), listener)
}

fn fake_replies(
    listener: TcpListener,
    requester: ed25519_dalek::VerifyingKey,
    signer: SigningKey,
    count: usize,
    reply: impl Fn(Frame, usize) -> Frame + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for i in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request = read_frame(&mut stream, &KEY, &|_| Some(requester)).unwrap();
            stream
                .write_all(&encode(&KEY, &signer, &reply(request, i)))
                .unwrap();
        }
    })
}

fn fake_ping(tag: &str, reply: impl Fn(Frame) -> Frame + Send + 'static) -> Result<u64, String> {
    let owner = rig(tag, KEY);
    let signer = SigningKey::from_bytes(&[71; 32]);
    let (peer, listener) = fake_peer(&owner, "remote", &signer);
    let replies = fake_replies(
        listener,
        vk_of(&owner.node.identity_hex()),
        signer,
        1,
        move |request, _| reply(request),
    );
    let result = owner.node.ping(&peer);
    replies.join().unwrap();
    result
}

fn fake_pings_with_replayed_nonce(tag: &str) -> (Result<u64, String>, Result<u64, String>) {
    let owner = rig(tag, KEY);
    let recipient = tag.to_string();
    let signer = SigningKey::from_bytes(&[72; 32]);
    let (peer, listener) = fake_peer(&owner, "remote", &signer);
    let replies = fake_replies(
        listener,
        vk_of(&owner.node.identity_hex()),
        signer,
        2,
        move |request, _| Frame {
            kind: 'o',
            id: request.id,
            from: "remote".into(),
            to: recipient.clone(),
            ts: store::now_ms(),
            nonce: "reused-pong-nonce".into(),
            ..Frame::default()
        },
    );
    let first = owner.node.ping(&peer);
    let second = owner.node.ping(&peer);
    replies.join().unwrap();
    (first, second)
}

#[test]
fn only_a_good_new_frame_for_this_node_is_answered() {
    let b = rig("recv", KEY);
    // the raw sender is a pinned computer of b's fleet
    let mut raw = Machine::new("raw", "127.0.0.1", "both");
    raw.id = crypto::hex(raw_sk().verifying_key().as_bytes());
    store::update(&b.paths, |f| f.machines.push(raw)).unwrap();
    let now = store::now_ms();
    assert!(answered(
        b.port,
        &raw_frame("recv", &KEY, now, "n-ok", "note", "m1"),
        &b
    ));
    // the same frame again is a replay
    assert!(!answered(
        b.port,
        &raw_frame("recv", &KEY, now, "n-ok", "note", "m1"),
        &b
    ));
    // a peer without the key is dropped without an answer
    assert!(!answered(
        b.port,
        &raw_frame("recv", &[9; 32], now, "n-2", "note", "m2"),
        &b
    ));
    // addressed to someone else
    assert!(!answered(
        b.port,
        &raw_frame("other", &KEY, now, "n-3", "note", "m3"),
        &b
    ));
    // a clock far off
    assert!(!answered(
        b.port,
        &raw_frame("recv", &KEY, now - 10 * 60_000, "n-4", "note", "m4"),
        &b
    ));
    assert!(!answered(
        b.port,
        &raw_frame("recv", &KEY, now + 10 * 60_000, "n-5", "note", "m5"),
        &b
    ));
    // an impostor: the fleet key, the pinned name, somebody else's identity
    let fake = SigningKey::from_bytes(&[10; 32]);
    assert!(!answered(
        b.port,
        &raw_frame_as("raw", &fake, "recv", &KEY, now, "n-6", "note", "m6"),
        &b
    ));
    // a computer nobody pinned, with the fleet key
    assert!(!answered(
        b.port,
        &raw_frame_as("stranger", &fake, "recv", &KEY, now, "n-7", "note", "m7"),
        &b
    ));
    // only the good one is in the inbox
    let ids: Vec<String> = b.node.inbox(None, 10).into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["m1"]);
}

#[test]
fn a_message_reaches_the_other_node_over_one_reused_connection() {
    let a = rig("alpha", KEY);
    let b = rig("beta", KEY);
    let to_b = link(&a, &b);
    for i in 0..5 {
        assert_eq!(
            a.node.send(&to_b, "note", format!("hello {i}").as_bytes()),
            Ok(true)
        );
    }
    let got = b.node.inbox(Some("note"), 10);
    assert_eq!(got.len(), 5);
    assert!(got.iter().all(|m| m.peer == "alpha"));
    assert_eq!(String::from_utf8_lossy(&got[0].body), "hello 4");
    assert_eq!(a.node.queued(None), 0);
    // a ping is a round trip on the same connection
    let us = a.node.ping(&to_b).unwrap();
    assert!(us > 0 && us < 2_000_000, "{us}");
    // binary bodies pass untouched
    let blob: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();
    assert_eq!(a.node.send(&to_b, "blob", &blob), Ok(true));
    assert_eq!(b.node.inbox(Some("blob"), 1)[0].body, blob);
    assert!(a
        .node
        .enqueue("beta", "blob", &vec![0; super::msg::MAX_BODY + 1])
        .is_err());
}

#[test]
fn nothing_is_lost_when_the_peer_is_down_and_a_retry_is_never_a_second_message() {
    let a = rig("sender", KEY);
    let port = free_port();
    let bpaths = paths("later");
    store::update(&bpaths, |f| f.me = "later".into()).unwrap();
    let bid = crypto::hex(
        super::pair::identity(&bpaths)
            .unwrap()
            .verifying_key()
            .as_bytes(),
    );
    let to_b = know(&a, "later", port, &bid);
    store::update(&bpaths, |f| {
        let mut m = Machine::new("sender", "127.0.0.1", "both");
        m.id = a.node.identity_hex();
        f.machines.push(m);
    })
    .unwrap();
    // the peer is down: the message is kept, tried, and scheduled again with backoff
    assert_eq!(a.node.send(&to_b, "note", b"for later"), Ok(false));
    assert_eq!(a.node.queued(Some("later")), 1);
    let id = lock_first_outbox(&a);
    assert_eq!(a.node.tries(&id), 1);
    assert_eq!(super::msg::backoff(1), 2);
    assert_eq!(super::msg::backoff(3), 8);
    assert_eq!(super::msg::backoff(20), 300, "backoff is capped");
    // not yet due: nothing is sent, no new try is counted
    assert_eq!(a.node.deliver(&to_b), Ok(0));
    assert_eq!(a.node.tries(&id), 1);

    // this program stops and starts again: the outbox is still there
    let paths = stop_for_restart(a).unwrap();
    let a2 = open_node(&paths, KEY).unwrap();
    assert_eq!(a2.queued(Some("later")), 1);

    // the peer comes up on that port
    let bnode = open_node(&bpaths, KEY).unwrap();
    let _h = serve::listen(&bnode, &[SocketAddr::from(([127, 0, 0, 1], port))]).unwrap();
    a2.retry_now();
    assert_eq!(a2.deliver(&to_b), Ok(1));
    assert_eq!(a2.queued(None), 0);
    assert_eq!(bnode.inbox(None, 10).len(), 1);

    // the answer was lost on the way back: the sender sends the same id again
    let db = rusqlite::Connection::open(paths.file("fleet.db")).unwrap();
    db.execute("UPDATE outbox SET delivered=0", []).unwrap();
    assert_eq!(
        a2.deliver(&to_b),
        Ok(1),
        "the receiver answers a duplicate so the sender stops"
    );
    assert_eq!(bnode.inbox(None, 10).len(), 1, "and keeps it once");
}

fn lock_first_outbox(a: &Rig) -> String {
    let db = rusqlite::Connection::open(a.paths.file("fleet.db")).unwrap();
    db.query_row("SELECT id FROM outbox LIMIT 1", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_signed_ack_from_another_pinned_member_is_refused() {
    let owner = rig("ack-sender", KEY);
    let peer_sk = SigningKey::from_bytes(&[73; 32]);
    let (peer, listener) = fake_peer(&owner, "peer", &peer_sk);
    let other_sk = SigningKey::from_bytes(&[74; 32]);
    know(
        &owner,
        "other",
        peer.port,
        &crypto::hex(other_sk.verifying_key().as_bytes()),
    );
    owner.node.enqueue("peer", "note", b"pending").unwrap();
    let to = owner.node.me.clone();
    let replies = fake_replies(
        listener,
        vk_of(&owner.node.identity_hex()),
        other_sk,
        1,
        move |request, _| Frame {
            kind: 'a',
            id: request.id,
            from: "other".into(),
            to: to.clone(),
            ts: store::now_ms(),
            nonce: "wrong-sender".into(),
            ..Frame::default()
        },
    );

    let result = owner.node.deliver(&peer);
    replies.join().unwrap();
    assert!(matches!(result, Err(e) if e == "unexpected answer"));
    assert_eq!(owner.node.queued(Some("peer")), 1);
}

#[test]
fn a_signed_ack_for_another_recipient_is_refused() {
    let owner = rig("ack-recipient", KEY);
    let peer_sk = SigningKey::from_bytes(&[75; 32]);
    let (peer, listener) = fake_peer(&owner, "peer", &peer_sk);
    owner.node.enqueue("peer", "note", b"pending").unwrap();
    let replies = fake_replies(
        listener,
        vk_of(&owner.node.identity_hex()),
        peer_sk,
        1,
        move |request, _| Frame {
            kind: 'a',
            id: request.id,
            from: "peer".into(),
            to: "someone-else".into(),
            ts: store::now_ms(),
            nonce: "wrong-recipient".into(),
            ..Frame::default()
        },
    );

    let result = owner.node.deliver(&peer);
    replies.join().unwrap();
    assert!(matches!(result, Err(e) if e == "unexpected answer"));
    assert_eq!(owner.node.queued(Some("peer")), 1);
}

#[test]
fn an_ack_for_another_queued_row_is_refused_without_marking_either_row() {
    let owner = rig("ack-row", KEY);
    let peer_sk = SigningKey::from_bytes(&[76; 32]);
    let (peer, listener) = fake_peer(&owner, "peer", &peer_sk);
    owner.node.enqueue("peer", "note", b"first").unwrap();
    let other_id = owner.node.enqueue("peer", "note", b"second").unwrap();
    let replies = fake_replies(
        listener,
        vk_of(&owner.node.identity_hex()),
        peer_sk,
        1,
        move |_, _| Frame {
            kind: 'a',
            id: other_id.clone(),
            from: "peer".into(),
            to: "ack-row".into(),
            ts: store::now_ms(),
            nonce: "wrong-row".into(),
            ..Frame::default()
        },
    );

    let result = owner.node.deliver(&peer);
    replies.join().unwrap();
    assert!(matches!(result, Err(e) if e == "unexpected answer"));
    assert_eq!(owner.node.queued(Some("peer")), 2);
}

#[test]
fn stale_and_replayed_signed_acks_are_refused() {
    let stale_owner = rig("ack-stale", KEY);
    let stale_sk = SigningKey::from_bytes(&[77; 32]);
    let (stale_peer, listener) = fake_peer(&stale_owner, "peer", &stale_sk);
    stale_owner
        .node
        .enqueue("peer", "note", b"pending")
        .unwrap();
    let replies = fake_replies(
        listener,
        vk_of(&stale_owner.node.identity_hex()),
        stale_sk,
        1,
        move |request, _| Frame {
            kind: 'a',
            id: request.id,
            from: "peer".into(),
            to: "ack-stale".into(),
            ts: store::now_ms() - 10 * 60_000,
            nonce: "stale-ack".into(),
            ..Frame::default()
        },
    );
    let stale = stale_owner.node.deliver(&stale_peer);
    replies.join().unwrap();
    assert!(matches!(stale, Err(e) if e.contains("clock")));
    assert_eq!(stale_owner.node.queued(Some("peer")), 1);

    let replay_owner = rig("ack-replay", KEY);
    let replay_sk = SigningKey::from_bytes(&[78; 32]);
    let (replay_peer, listener) = fake_peer(&replay_owner, "peer", &replay_sk);
    replay_owner.node.enqueue("peer", "note", b"first").unwrap();
    let replies = fake_replies(
        listener,
        vk_of(&replay_owner.node.identity_hex()),
        replay_sk,
        2,
        move |request, _| Frame {
            kind: 'a',
            id: request.id,
            from: "peer".into(),
            to: "ack-replay".into(),
            ts: store::now_ms(),
            nonce: "replayed-ack".into(),
            ..Frame::default()
        },
    );
    let first = replay_owner.node.deliver(&replay_peer);
    replay_owner
        .node
        .enqueue("peer", "note", b"second")
        .unwrap();
    let replayed = replay_owner.node.deliver(&replay_peer);
    replies.join().unwrap();
    assert_eq!(first, Ok(1));
    assert!(matches!(replayed, Err(e) if e.contains("replay")));
    assert_eq!(replay_owner.node.queued(Some("peer")), 1);
}

#[test]
fn signed_pongs_must_match_the_ping_and_pass_the_guard() {
    let wrong = fake_ping("pong-wrong", |_request| Frame {
        kind: 'o',
        id: "not-the-ping".into(),
        from: "remote".into(),
        to: "pong-wrong".into(),
        ts: store::now_ms(),
        nonce: "wrong-pong".into(),
        ..Frame::default()
    });
    assert!(matches!(wrong, Err(e) if e == "unexpected answer"));

    let stale = fake_ping("pong-stale", |request| Frame {
        kind: 'o',
        id: request.id,
        from: "remote".into(),
        to: "pong-stale".into(),
        ts: store::now_ms() - 10 * 60_000,
        nonce: "stale-pong".into(),
        ..Frame::default()
    });
    assert!(matches!(stale, Err(e) if e.contains("clock")));

    let valid = fake_ping("pong-valid", |request| Frame {
        kind: 'o',
        id: request.id,
        from: "remote".into(),
        to: "pong-valid".into(),
        ts: store::now_ms(),
        nonce: "valid-pong".into(),
        ..Frame::default()
    });
    assert!(valid.is_ok());

    let (first, replayed) = fake_pings_with_replayed_nonce("pong-replay");
    assert!(first.is_ok());
    assert!(matches!(replayed, Err(e) if e.contains("replay")));
}

#[test]
fn signed_acks_without_a_nonce_or_with_a_future_clock_leave_the_message_pending() {
    for (tag, nonce, offset, error) in [
        ("ack-empty", "", 0, "no nonce"),
        ("ack-future", "future-ack", 10 * 60_000, "clock"),
    ] {
        let owner = rig(tag, KEY);
        let signer = SigningKey::from_bytes(&[79; 32]);
        let (peer, listener) = fake_peer(&owner, "peer", &signer);
        owner.node.enqueue("peer", "note", b"pending").unwrap();
        let replies = fake_replies(
            listener,
            vk_of(&owner.node.identity_hex()),
            signer,
            1,
            move |request, _| Frame {
                kind: 'a',
                id: request.id,
                from: "peer".into(),
                to: tag.into(),
                ts: store::now_ms() + offset,
                nonce: nonce.into(),
                ..Frame::default()
            },
        );
        let result = owner.node.deliver(&peer);
        replies.join().unwrap();
        assert_eq!(result, Err(format!("refused: {error}")));
        assert_eq!(owner.node.queued(Some("peer")), 1);
    }
}

#[test]
fn losing_a_real_ack_retries_the_same_message_and_stores_it_once() {
    let owner = rig("lost-ack-sender", KEY);
    let receiver = rig("lost-ack-peer", KEY);
    know(
        &receiver,
        &owner.node.me,
        owner.port,
        &owner.node.identity_hex(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = know(
        &owner,
        &receiver.node.me,
        listener.local_addr().unwrap().port(),
        &receiver.node.identity_hex(),
    );
    let id = owner
        .node
        .enqueue(&peer.name, "note", b"keep once")
        .unwrap();
    let recipient = receiver.node.clone();
    let requester = vk_of(&owner.node.identity_hex());
    let expected_id = id.clone();
    let replies = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = read_frame(&mut stream, &KEY, &|_| Some(requester)).unwrap();
        assert_eq!(request.id, expected_id);
        assert!(recipient.store(
            &request.from,
            &request.id,
            &request.topic,
            request.created,
            &request.body
        ));
        // The message reached the receiver, but the connection ends before its ACK.
        drop(stream);
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        recipient.frames(std::io::BufReader::new(stream));
    });
    assert!(owner.node.deliver(&peer).is_err());
    assert_eq!(owner.node.queued(Some(&peer.name)), 1);
    assert_eq!(owner.node.tries(&id), 1);
    owner.node.retry_now();
    assert_eq!(owner.node.deliver(&peer), Ok(1));
    assert_eq!(owner.node.queued(Some(&peer.name)), 0);
    let got = receiver.node.inbox(Some("note"), 10);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].id.as_str(), got[0].body.as_slice()),
        (id.as_str(), &b"keep once"[..])
    );
    replies.join().unwrap();
}

#[test]
fn a_stale_connection_is_replaced_without_losing_the_message() {
    let a = rig("stale-a", KEY);
    let mut b = rig("stale-b", KEY);
    let to_b = link(&a, &b);
    assert_eq!(a.node.send(&to_b, "note", b"one"), Ok(true));
    // b goes away and comes back on the same port (a laptop that slept): a's open connection is dead
    b.handle.take().unwrap().stop();
    std::thread::sleep(Duration::from_millis(100));
    let h = serve::listen(&b.node, &[SocketAddr::from(([127, 0, 0, 1], b.port))]).unwrap();
    assert_eq!(a.node.send(&to_b, "note", b"two"), Ok(true));
    assert_eq!(b.node.inbox(Some("note"), 10).len(), 2);
    drop(h);
}

#[test]
fn heartbeats_update_fleet_json_and_the_fleet_page_reads_them() {
    let a = rig("hb-a", KEY);
    let b = rig("hb-b", KEY);
    // b's fleet knows a, and runs the real senders; a is only listening
    let to_a = link(&b, &a);
    let _ = to_a;
    let stop = serve::start(&b.node, &[SocketAddr::from(([127, 0, 0, 1], free_port()))]).unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let m = loop {
        let m = store::load(&a.paths).machines.remove(0);
        if m.last_seen > 0 || std::time::Instant::now() > end {
            break m;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(m.last_seen > 0, "the heartbeat arrived");
    assert!(m.health.f64("cpus").unwrap() >= 1.0);
    assert!(m.health.get("models").is_some() && m.health.get("queue").is_some());
    assert!(m.health.str("ver").is_some());
    assert_eq!(store::load(&a.paths).me, "hb-a", "a's own name is kept");
    // the loopback side serves the same facts
    let v = serve::view(&a.paths, Some(&a.node));
    assert_eq!(v.str("heading"), Some("Two machines, one field."));
    assert_eq!(v.arr("machines")[0].bool("up"), Some(true));
    let html = page::section(&a.paths, true);
    assert!(
        html.contains("answering") && html.contains("CPU load") || html.contains("memory in use"),
        "{html}"
    );
    drop(stop);
}

fn http_get(port: u16, path: &str, host: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut t = String::new();
    let _ = s.read_to_string(&mut t);
    crate::parse_response(t.as_bytes(), true).unwrap_or((0, t))
}

#[test]
fn the_http_side_serves_hello_and_a_loopback_only_fleet_json() {
    let a = rig("web", KEY);
    store::update(&a.paths, |f| {
        f.machines.push(Machine::new("x", "10.0.0.5", "engine"))
    })
    .unwrap();
    let (c, b) = http_get(a.port, "/hello", "whatever");
    assert_eq!(c, 200);
    // a stranger on the network learns that Tempo is here, not whose computer it is
    assert!(
        b.contains("\"app\":\"tempo-fleet\"") && !b.contains("name"),
        "{b}"
    );
    let host = format!("127.0.0.1:{}", a.port);
    let (c, b) = http_get(a.port, "/fleet.json", &host);
    assert_eq!(c, 200);
    assert!(b.contains("Two machines, one field."), "{b}");
    // a name the page chooses (DNS rebinding) is refused
    assert_eq!(http_get(a.port, "/fleet.json", "evil.example:80").0, 403);
    assert_eq!(http_get(a.port, "/nothing", &host).0, 404);
}

#[test]
fn hello_names_the_computer_only_to_its_own_fleet_or_a_joiner_with_an_invite() {
    let a = rig("web2", KEY);
    assert!(!http_get(a.port, "/hello", "x").1.contains("\"name\""));
    // a computer in this fleet (here the test's own address) gets the name
    store::update(&a.paths, |f| {
        f.machines
            .push(Machine::new("me-too", "127.0.0.1", "engine"))
    })
    .unwrap();
    let (_, b) = http_get(a.port, "/hello", "x");
    assert!(
        b.contains("\"name\":\"web2\"") && b.contains("\"invite\":false"),
        "{b}"
    );
    // and while an invite is open, a computer that is not in the fleet yet needs the name to join
    let b2 = rig("web3", KEY);
    pair::create_invite(&b2.paths, store::now_s()).unwrap();
    let (_, b) = http_get(b2.port, "/hello", "x");
    assert!(
        b.contains("\"name\":\"web3\"") && b.contains("\"invite\":true"),
        "{b}"
    );
}

fn fleet_of(addrs: &[&str]) -> store::Fleet {
    let mut f = store::Fleet::default();
    for (i, a) in addrs.iter().enumerate() {
        f.machines.push(Machine::new(&format!("m{i}"), a, "engine"));
    }
    f
}

#[test]
fn the_messenger_opens_only_the_addresses_the_fleet_needs() {
    use std::net::Ipv4Addr;
    // built from numbers, so no address is written out in the source (the repo's leak scan reads the text)
    let home = Ipv4Addr::new(10, 0, 1, 7);
    let tail = Ipv4Addr::new(100, 80, 1, 2);
    let wsl = Ipv4Addr::new(172, 22, 96, 1);
    let own = [home, tail, wsl, Ipv4Addr::new(169, 254, 3, 3)];
    let (lan_peer, tail_peer, tail_peer2) = (
        Ipv4Addr::new(10, 0, 1, 20).to_string(),
        Ipv4Addr::new(100, 64, 0, 9).to_string(),
        Ipv4Addr::new(100, 80, 4, 4).to_string(),
    );
    let ips = |f: &store::Fleet| -> Vec<String> {
        serve::listen_for(f, &own, 8770)
            .iter()
            .map(|a| a.ip().to_string())
            .collect()
    };
    let s = |v: &[Ipv4Addr]| -> Vec<String> {
        v.iter()
            .map(|a| a.to_string())
            .chain(["127.0.0.1".to_string()])
            .collect()
    };
    // computers on the tailnet only: the home Wi-Fi and the virtual network stay closed
    assert_eq!(ips(&fleet_of(&[&tail_peer, &tail_peer2])), s(&[tail]));
    // a computer on the home network: the home addresses open, the tailnet one does not
    assert_eq!(ips(&fleet_of(&[&lan_peer])), s(&[home, wsl]));
    // both kinds: both open; link-local is never listened on
    let both = ips(&fleet_of(&[&lan_peer, &tail_peer]));
    assert_eq!(both, s(&[home, tail, wsl]));
    // nobody yet (an invite is open) or a name that cannot be placed: keep both
    assert_eq!(ips(&fleet_of(&[])), both);
    assert_eq!(ips(&fleet_of(&["tower.local"])), both);
    // a disabled computer does not count
    let mut f = fleet_of(&[&lan_peer]);
    f.machines[0].enabled = false;
    assert_eq!(ips(&f), both);
    // loopback is always there, and never the wildcard
    for a in serve::listen_for(&fleet_of(&[&tail_peer]), &own, 8770) {
        assert!(!a.ip().is_unspecified());
    }
}

#[test]
fn an_address_that_cannot_be_listened_on_is_reported_not_skipped() {
    let a = rig("bindfail", KEY);
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap();
    let free: SocketAddr = ([127, 0, 0, 1], free_port()).into();
    let h = serve::listen(&a.node, &[busy, free]).unwrap();
    assert_eq!(h.addrs, vec![free]);
    assert_eq!(h.failed.len(), 1);
    assert_eq!(h.failed[0].0, busy);
    assert!(!h.failed[0].1.is_empty());
}

#[test]
fn the_listener_is_never_on_all_addresses() {
    for a in serve::listen_on(8770) {
        assert!(!a.ip().is_unspecified(), "{a}");
        assert!(crate::private_ip(a.ip()), "{a}");
    }
    assert!(serve::listen_on(8770).iter().any(|a| a.ip().is_loopback()));
}

#[test]
fn messenger_latency_and_throughput_on_this_computer() {
    let a = rig("perf-a", KEY);
    let b = rig("perf-b", KEY);
    let to_b = link(&a, &b);
    // latency: one message at a time, written to the outbox, sent, stored by the receiver, answered
    a.node.send(&to_b, "note", b"warm up").unwrap();
    let mut us: Vec<u64> = Vec::new();
    for i in 0..1000 {
        let t = std::time::Instant::now();
        assert_eq!(
            a.node
                .send(&to_b, "note", format!("message {i}").as_bytes()),
            Ok(true)
        );
        us.push(t.elapsed().as_micros() as u64);
    }
    us.sort_unstable();
    let (p50, p99) = (us[499], us[989]);
    // throughput: a thousand written first, then sent in batches on the open connection
    let t = std::time::Instant::now();
    for i in 0..1000 {
        a.node
            .enqueue("perf-b", "note", format!("bulk {i}").as_bytes())
            .unwrap();
    }
    let queued = t.elapsed();
    while a.node.queued(None) > 0 {
        a.node.deliver(&to_b).unwrap();
    }
    let total = t.elapsed();
    eprintln!(
        "fleet messenger on localhost: one at a time, 1000 messages: p50 {p50} us, p99 {p99} us; \
         1000 queued in {:.1} ms, all delivered in {:.1} ms ({:.0} messages/s)",
        queued.as_secs_f64() * 1000.0,
        total.as_secs_f64() * 1000.0,
        1000.0 / total.as_secs_f64()
    );
    assert_eq!(b.node.inbox(None, 5000).len(), 2001);
    assert!(p50 < 30_000, "p50 {p50} us");
}

// ---- proposals

#[test]
fn a_proposal_is_applied_only_after_a_yes_and_resolves_labels_to_names() {
    let p = paths("proposal");
    store::update(&p, |f| {
        f.machines.push(Machine::new("zeta", "10.0.0.9", "engine"));
        f.machines.push(Machine::new("alpha", "10.0.0.8", "engine"));
    })
    .unwrap();
    let args = parse(
        r#"{"add":[{"addr":"10.0.0.20","name":"nas","role":"engine"}],"disable":["machine-2"]}"#,
    )
    .unwrap();
    assert_eq!(
        write_proposal(&p, &args, "why", 1_793_000_000),
        Ok(APPLY_COMMAND)
    );
    // a no changes nothing
    let (c, said) = out_of(|o| apply_proposal(&p, None, &mut |_| false, o));
    assert_eq!(c, 0);
    assert!(
        said.contains("add      10.0.0.20") && said.contains("disable  zeta"),
        "{said}"
    );
    assert_eq!(store::load(&p).machines.len(), 2);
    assert!(p.file(PROPOSAL_FILE).exists());
    // a yes applies, and the proposal is spent
    let (c, _) = out_of(|o| apply_proposal(&p, None, &mut |_| true, o));
    assert_eq!(c, 0);
    let f = store::load(&p);
    assert_eq!(f.machines.len(), 3);
    assert!(
        !f.machines
            .iter()
            .find(|m| m.name == "zeta")
            .unwrap()
            .enabled
    );
    assert!(!p.file(PROPOSAL_FILE).exists());
    // refusals
    assert!(write_proposal(
        &p,
        &parse(r#"{"add":[{"addr":"1.1.1.1"}]}"#).unwrap(),
        "",
        1
    )
    .is_err());
    assert!(write_proposal(&p, &parse("{}").unwrap(), "", 1).is_err());
}

#[cfg(unix)]
#[test]
fn rotate_sends_the_new_key_over_ssh_then_switches_and_names_who_it_missed() {
    let dir = fake_ssh();
    let p = paths("rotate");
    store::init_key(&p).unwrap();
    let old = store::read_key(&p).unwrap();
    store::update(&p, |f| {
        f.machines
            .push(Machine::new("rotbox", "10.0.0.41", "engine"));
        f.machines
            .push(Machine::new("lostbox", "10.0.0.33", "engine"));
    })
    .unwrap();
    // a no changes nothing
    let (c, _) = out_of(|o| rotate_cmd(&p, false, &mut |_| false, o));
    assert_eq!((c, store::read_key(&p)), (0, Some(old)));
    let (c, said) = out_of(|o| rotate_cmd(&p, true, &mut |_| true, o));
    assert_eq!(c, 1, "one computer could not be reached: {said}");
    let new = store::read_key(&p).unwrap();
    assert_ne!(new, old);
    assert_eq!(
        std::fs::read(p.file("fleet.key.old")).unwrap(),
        old.to_vec()
    );
    assert!(
        said.contains("rotbox has the new key") && said.contains("lostbox could not be reached"),
        "{said}"
    );
    assert!(said.contains("fleet setup lostbox"), "{said}");
    let stdin = std::fs::read_to_string(dir.join("stdin")).unwrap();
    assert!(stdin.contains(&crypto::hex(&new)));
    assert!(!std::fs::read_to_string(dir.join("log"))
        .unwrap()
        .contains(&crypto::hex(&new)));
}

#[test]
fn a_running_messenger_picks_up_a_new_key_without_a_restart() {
    let a = rig("rot-a", KEY);
    assert_eq!(a.node.key(), KEY);
    crypto::write_private(&a.paths.file("fleet.key"), &[3u8; 32]).unwrap();
    // the file is looked at every two seconds
    let end = std::time::Instant::now() + Duration::from_secs(6);
    while a.node.key() != [3u8; 32] && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(a.node.key(), [3u8; 32]);
}

#[test]
fn callback_endpoint_keeps_its_actual_controller_port() {
    assert_eq!(peer_endpoint("10.0.0.2:9123"), Some(("10.0.0.2", 9123)));
    assert_eq!(peer_endpoint("10.0.0.2:0"), None);
    assert_eq!(peer_endpoint("8.8.8.8:9123"), None);
    assert_eq!(
        peer_endpoint("10.0.0.2"),
        Some(("10.0.0.2", store::fleet_port()))
    );
}

#[test]
fn checked_enrollment_update_preserves_malformed_duplicate_and_unfamiliar_registry_bytes() {
    let p = paths("checked-registry");
    std::fs::create_dir_all(&p.home).unwrap();
    for bytes in [
        "{broken",
        "{\"self\":\"main\",\"self\":\"other\",\"machines\":[]}",
        "{\"self\":\"main\",\"machines\":[],\"future_field\":true}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"8.8.8.8\"}]}",
        "{\"self\":\"main\",\"machines\":[],\"allow\":[12]}",
        "{\"self\":\"main\",\"machines\":[],\"version\":01}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"health\":{\"count\":00}}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"health\":{\"a\":1,\"a\":2}}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"probe\":\"bad\nline\"}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"probe\":\"\\uD800\"}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"probe\":\"\\uD800\\u0041\"}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"probe\":\"\\u+123\"}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"port\":65536}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"enabled\":\"yes\"}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\",\"id\":\"invalid\"}]}",
        "{\"self\":\"main\",\"machines\":[{\"name\":\"worker\",\"addr\":\"10.0.0.2\"},{\"name\":\"worker\",\"addr\":\"10.0.0.3\"}]}",
    ] {
        std::fs::write(p.file("fleet.json"), bytes).unwrap();
        let called = std::cell::Cell::new(false);
        let result = store::update_checked(&p, |_| {
            called.set(true);
            Ok(())
        });
        assert!(result.is_err(), "accepted {bytes}");
        assert!(!called.get());
        assert_eq!(
            std::fs::read_to_string(p.file("fleet.json")).unwrap(),
            bytes
        );
        assert!(!p.file("fleet.json.lock").exists());
    }
    let valid = "{ \"self\":\"main\", \"machines\":[], \"allow\":[] }";
    std::fs::write(p.file("fleet.json"), valid).unwrap();
    let result: Result<(), String> = store::update_checked(&p, |_| Err("declined".into()));
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(p.file("fleet.json")).unwrap(),
        valid
    );
    store::update_checked(&p, |fleet| {
        fleet
            .machines
            .push(Machine::new("worker", "10.0.0.2", "agent"));
        Ok(())
    })
    .unwrap();
    assert_eq!(store::load_checked(&p).unwrap().machines.len(), 1);
}

#[test]
fn checked_enrollment_update_preserves_escaped_non_bmp_nested_metadata() {
    let p = paths("checked-unicode-metadata");
    std::fs::create_dir_all(&p.home).unwrap();
    let input = r#"{"self":"main","machines":[{"name":"worker","addr":"10.0.0.2","health":{"nested":[{"label":"\ud83c\udfb5"}]},"probe":{"\ud83d\ude80":"\ud83d\ude00"},"tools":[{"label":"\udbff\udfff"}]}]}"#;
    std::fs::write(p.file("fleet.json"), input).unwrap();
    store::update_checked(&p, |fleet| {
        fleet
            .machines
            .push(Machine::new("other", "10.0.0.3", "agent"));
        Ok(())
    })
    .unwrap();
    let fleet = store::load_checked(&p).unwrap();
    let machine = &fleet.machines[0];
    assert_eq!(machine.health.arr("nested")[0].str("label"), Some("🎵"));
    assert_eq!(machine.probe.str("🚀"), Some("😀"));
    assert_eq!(
        machine.tools,
        crate::setup::json::J::Arr(vec![crate::setup::json::J::Obj(vec![(
            "label".into(),
            crate::setup::json::J::s("\u{10ffff}")
        )])])
    );
}

#[cfg(windows)]
#[test]
fn checked_enrollment_update_does_not_replace_an_unreadable_registry() {
    use std::os::windows::fs::OpenOptionsExt;
    let p = paths("checked-locked-registry");
    std::fs::create_dir_all(&p.home).unwrap();
    let bytes = "{\"self\":\"main\",\"machines\":[]}";
    std::fs::write(p.file("fleet.json"), bytes).unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(p.file("fleet.json"))
        .unwrap();
    let called = std::cell::Cell::new(false);
    assert!(store::update_checked(&p, |_| {
        called.set(true);
        Ok(())
    })
    .is_err());
    assert!(!called.get());
    drop(held);
    assert_eq!(
        std::fs::read_to_string(p.file("fleet.json")).unwrap(),
        bytes
    );
    assert!(!p.file("fleet.json.lock").exists());
}
