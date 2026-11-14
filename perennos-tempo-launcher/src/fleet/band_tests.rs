//! Joining and tools, on in-process nodes that talk over this computer's own network.

use super::tests::{link, open_node, paths, rig, Rig};
use super::tools::{self, CallHandler, CallRunner, RunOut};
use super::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

const KEY: [u8; 32] = [7; 32];

fn addr_of(r: &Rig) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], r.port))
}

fn engine_probe() -> J {
    parse(
        r#"{"gpu":{"class":"discrete NVIDIA","memory_gb":24.0},"memory_gb_total":64.0,"logical_cpus":16}"#,
    )
    .unwrap()
}

fn laptop_probe() -> J {
    parse(r#"{"gpu":{"class":"none"},"memory_gb_total":16.0,"logical_cpus":8,"laptop":true}"#)
        .unwrap()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A main computer with the fleet key, listening, and a new computer's folders with its own key.
fn main_and_new(tag: &str) -> (Rig, Paths) {
    let main = rig(&format!("main-{tag}"), KEY);
    (main, paths(&format!("new-{tag}")))
}

#[test]
fn a_computer_joins_with_the_code_and_both_sides_pin_each_other() {
    let (main, new) = main_and_new("e2e");
    let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
    let j = pair::join(
        &new,
        addr_of(&main),
        &inv.code,
        "studio-box",
        0,
        &engine_probe(),
    )
    .unwrap();
    pair::keep(&new, &j, "127.0.0.1").unwrap();

    // the new computer has the fleet key and the main computer's identity
    assert_eq!(store::read_key(&new), Some(KEY));
    let nf = store::load(&new);
    assert_eq!(nf.me, "studio-box");
    assert_eq!(nf.machines[0].name, main.node.me);
    assert_eq!(nf.machines[0].id, main.node.identity_hex());
    // the main computer has it, with the part its hardware earned, pinned, and its engine in the settings
    let mf = store::load(&main.paths);
    let m = &mf.machines[0];
    assert_eq!(
        (m.name.as_str(), m.role.as_str(), m.addr.as_str()),
        ("studio-box", "engine", "127.0.0.1")
    );
    assert_eq!(
        m.id,
        crypto::hex(pair::identity(&new).unwrap().verifying_key().as_bytes())
    );
    let cfg = parse(&std::fs::read_to_string(main.paths.agent_data.join("config.json")).unwrap())
        .unwrap();
    assert!(cfg
        .strs("ollama_fallbacks")
        .contains(&"http://127.0.0.1:11434".to_string()));
    // the code is spent
    assert!(!pair::invite_open(&main.paths));
    assert_eq!(j.role, "engine");

    // and they can talk, signed, both ways (the new computer listens too)
    let fresh = open_node(&new, j.key).unwrap();
    let port = free_port();
    let _h = serve::listen(&fresh, &[SocketAddr::from(([127, 0, 0, 1], port))]).unwrap();
    store::update(&main.paths, |f| f.machines[0].port = port).unwrap();
    let to_new = store::load(&main.paths).machines.remove(0);
    assert_eq!(main.node.send(&to_new, "note", b"welcome aboard"), Ok(true));
    assert_eq!(fresh.inbox(Some("note"), 1)[0].body, b"welcome aboard");
    store::update(&new, |f| f.machines[0].port = main.port).unwrap();
    let to_main = store::load(&new).machines.remove(0);
    assert_eq!(fresh.send(&to_main, "note", b"glad to be here"), Ok(true));
    assert_eq!(main.node.inbox(Some("note"), 1)[0].peer, "studio-box");
}

#[test]
fn a_laptop_is_a_client_and_the_same_computer_joining_again_keeps_its_place() {
    let (main, new) = main_and_new("laptop");
    for _ in 0..2 {
        let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
        let j = pair::join(
            &new,
            addr_of(&main),
            &inv.code,
            "laptop",
            0,
            &laptop_probe(),
        )
        .unwrap();
        assert_eq!(j.role, "client");
    }
    let mf = store::load(&main.paths);
    assert_eq!(mf.machines.len(), 1);
    assert_eq!(mf.machines[0].role, "client");
    // a client is not an engine: the agent's settings are left alone
    assert!(!main.paths.agent_data.join("config.json").exists());
    // a different computer with the same name gets its own
    let other = paths("other-laptop");
    let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
    let j = pair::join(
        &other,
        addr_of(&main),
        &inv.code,
        "laptop",
        0,
        &laptop_probe(),
    )
    .unwrap();
    assert_eq!(j.you_are, "laptop-2");
}

#[test]
fn a_wrong_code_fails_and_three_wrong_tries_burn_the_invitation() {
    let (main, new) = main_and_new("wrong");
    let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
    let try_code = |c: &str| pair::join(&new, addr_of(&main), c, "box", 0, &engine_probe());
    let e = try_code("1-tiger-violet").unwrap_err();
    assert!(e.contains("not accepted"), "{e}");
    assert!(
        store::load(&main.paths).machines.is_empty(),
        "nothing was added"
    );
    assert!(
        pair::invite_open(&main.paths),
        "one wrong try does not end it"
    );
    // the right one still works after a slip
    assert!(try_code(&inv.code).is_ok());

    // three wrong tries and the invitation is gone, even for the right code
    let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
    for _ in 0..3 {
        assert!(try_code("2-oak-pine").is_err());
    }
    let e = try_code(&inv.code).unwrap_err();
    assert!(e.contains("no invitation"), "{e}");
}

#[test]
fn a_tampered_ml_kem_ciphertext_fails_closed() {
    let (main, new) = main_and_new("kem-tamper");
    let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
    let addr = addr_of(&main);
    let join = |see: &dyn Fn(&mut Vec<u8>)| {
        pair::join_with(&new, addr, &inv.code, "box", 0, &engine_probe(), see)
    };
    // one bit of the ciphertext changed after it was opened: the right code, the wrong key
    let e = join(&|ct| ct[0] ^= 1).unwrap_err();
    assert!(e.contains("did not line up"), "{e}");
    assert!(
        store::load(&main.paths).machines.is_empty(),
        "nothing was added"
    );
    assert!(
        store::read_key(&new).is_none(),
        "no fleet key was handed over"
    );
    // a ciphertext of the wrong shape is refused outright
    let e = join(&|ct| ct.truncate(10)).unwrap_err();
    assert!(e.contains("not ML-KEM-768"), "{e}");
    assert!(store::load(&main.paths).machines.is_empty());
    // the slips cost tries (two of three) but did not spend the code: an honest run still works
    assert!(pair::invite_open(&main.paths));
    assert!(join(&|_| {}).is_ok());
    assert_eq!(store::load(&main.paths).machines.len(), 1);
}

#[test]
fn an_expired_or_used_code_fails() {
    let (main, new) = main_and_new("old");
    // ten minutes and a second ago
    let inv = pair::create_invite(&main.paths, store::now_s() - pair::VALID_S - 1).unwrap();
    let e = pair::join(&new, addr_of(&main), &inv.code, "box", 0, &engine_probe()).unwrap_err();
    assert!(e.contains("ran out"), "{e}");
    assert!(!pair::invite_open(&main.paths));
    // single use
    let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
    assert!(pair::join(&new, addr_of(&main), &inv.code, "box", 0, &engine_probe()).is_ok());
    let e = pair::join(
        &paths("again"),
        addr_of(&main),
        &inv.code,
        "box2",
        0,
        &engine_probe(),
    )
    .unwrap_err();
    assert!(e.contains("no invitation"), "{e}");
    // no invitation at all
    assert!(pair::join(&new, addr_of(&main), "1-a-b", "x", 0, &engine_probe()).is_err());
    // never to a public address
    let public = SocketAddr::from(([8, 8, 8, 8], 8770));
    assert!(pair::join(&new, public, "1-a-b", "x", 0, &engine_probe())
        .unwrap_err()
        .contains("private"));
}

#[test]
fn the_hello_page_says_when_a_code_is_waiting() {
    let main = rig("hello", KEY);
    let hello = |r: &Rig| {
        let mut s = TcpStream::connect(addr_of(r)).unwrap();
        write!(
            s,
            "GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut t = String::new();
        let _ = s.read_to_string(&mut t);
        t
    };
    assert!(hello(&main).contains("\"invite\":false"));
    pair::create_invite(&main.paths, store::now_s()).unwrap();
    assert!(hello(&main).contains("\"invite\":true"));
    let found = probe_hello(&[std::net::Ipv4Addr::LOCALHOST], main.port);
    assert_eq!(found.len(), 1);
    assert!(found[0].invite && found[0].name == "hello");
}

#[test]
fn an_impostor_with_the_fleet_key_is_not_let_in() {
    let (main, new) = main_and_new("impostor");
    let inv = pair::create_invite(&main.paths, store::now_s()).unwrap();
    let j = pair::join(
        &new,
        addr_of(&main),
        &inv.code,
        "real-box",
        0,
        &engine_probe(),
    )
    .unwrap();
    pair::keep(&new, &j, "127.0.0.1").unwrap();
    // someone who got hold of the fleet key (a copied file) and says it is the new computer
    let imp = paths("imp");
    crypto::write_private(&imp.file("fleet.key"), &KEY).unwrap();
    store::update(&imp, |f| {
        f.me = "real-box".into();
        let mut m = Machine::new(&main.node.me, "127.0.0.1", "both");
        m.port = main.port;
        m.id = main.node.identity_hex(); // it can read the main computer's answers
        f.machines.push(m);
    })
    .unwrap();
    let inode = open_node(&imp, KEY).unwrap();
    let to_main = store::load(&imp).machines.remove(0);
    assert!(inode.send(&to_main, "note", b"let me in").is_err() || inode.queued(None) == 1);
    assert!(
        main.node.inbox(None, 5).is_empty(),
        "the message was not accepted"
    );
    // the real one, with its own identity, is
    let real = open_node(&new, j.key).unwrap();
    store::update(&new, |f| f.machines[0].port = main.port).unwrap();
    let m = store::load(&new).machines.remove(0);
    assert_eq!(real.send(&m, "note", b"it is me"), Ok(true));
    assert_eq!(main.node.inbox(None, 5).len(), 1);
}

// ---- tools

/// A fake ComfyUI: answers what the fleet looks at, and counts the prompts it is given.
fn fake_comfy() -> (u16, Arc<Mutex<u32>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let hits = Arc::new(Mutex::new(0u32));
    let h = hits.clone();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let mut b = [0u8; 4096];
            let n = s.read(&mut b).unwrap_or(0);
            let req = String::from_utf8_lossy(&b[..n]).into_owned();
            let body = if req.starts_with("POST /prompt") {
                *h.lock().unwrap() += 1;
                "{\"prompt_id\":\"p1\"}"
            } else if req.contains("/system_stats") {
                "{\"devices\":[{\"name\":\"fake\",\"vram_total\":25769803776}]}"
            } else if req.contains("/object_info/CheckpointLoaderSimple") {
                "{\"CheckpointLoaderSimple\":{\"input\":{\"required\":{\"ckpt_name\":[[\"dream.safetensors\",\"photo.safetensors\"]]}}}}"
            } else {
                "{}"
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, hits)
}

/// Writes the node's agent settings so its ComfyUI is the fake one.
fn point_comfy_at(p: &Paths, port: u16) {
    std::fs::create_dir_all(&p.agent_data).unwrap();
    std::fs::write(
        p.agent_data.join("config.json"),
        format!("{{\"comfy_url\":\"http://127.0.0.1:{port}\"}}"),
    )
    .unwrap();
}

fn picture() -> Vec<u8> {
    // three pieces and a bit, and not the same byte twice in a row
    (0..700_001u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// Makes the picture the way the agent would: asks ComfyUI (the fake) for it and leaves the file.
struct FakeAgent {
    comfy: u16,
    out: PathBuf,
    asked: Mutex<Vec<String>>,
}

impl CallRunner for FakeAgent {
    fn comfy(&self, args: &J) -> Result<RunOut, String> {
        self.asked
            .lock()
            .unwrap()
            .push(args.str("prompt").unwrap_or("").to_string());
        let mut s = TcpStream::connect(("127.0.0.1", self.comfy)).map_err(|e| e.to_string())?;
        write!(
            s,
            "POST /prompt HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        )
        .unwrap();
        let mut t = String::new();
        let _ = s.read_to_string(&mut t);
        std::fs::create_dir_all(&self.out).unwrap();
        let f = self.out.join("tempo-agent_00001_.png");
        std::fs::write(&f, picture()).unwrap();
        Ok(RunOut {
            text: "image path: tempo-agent_00001_.png".into(),
            files: vec![f],
        })
    }
    fn mcp(&self, _: &str, _: &str, _: &J) -> Result<String, String> {
        Err("no tools here".into())
    }
}

fn node_with_tools(name: &str, allow: &[&str], runner: Arc<dyn CallRunner>) -> Rig {
    let r = rig(name, KEY);
    store::update(&r.paths, |f| {
        f.allow = allow.iter().map(|s| s.to_string()).collect()
    })
    .unwrap();
    msg_handlers(&r, runner);
    r
}

fn msg_handlers(r: &Rig, runner: Arc<dyn CallRunner>) {
    super::msg::lock(&r.node.handlers).push(Arc::new(CallHandler::new(r.paths.clone(), runner)));
}

#[test]
fn a_node_offers_only_what_its_owner_allowed() {
    let (port, _) = fake_comfy();
    let p = paths("advert");
    point_comfy_at(&p, port);
    // nothing allowed: nothing offered, but pairing's look still sees ComfyUI is there
    assert!(tools::advert(&p, false).is_none());
    let all = tools::advert(&p, true).unwrap();
    assert_eq!(all.get("comfy").unwrap().bool("up"), Some(true));
    store::update(&p, |f| f.allow = vec!["comfy_image".into()]).unwrap();
    let a = tools::advert(&p, false).unwrap();
    let c = a.get("comfy").unwrap();
    assert_eq!(c.bool("up"), Some(true));
    assert_eq!(c.f64("gpu_gb"), Some(25.8));
    assert_eq!(
        c.strs("checkpoints"),
        ["dream.safetensors", "photo.safetensors"]
    );
    // switched off again
    store::update(&p, |f| f.allow.clear()).unwrap();
    assert!(tools::advert(&p, false).is_none());
    // ComfyUI not running: not offered even when allowed
    let q = paths("advert-down");
    point_comfy_at(&q, free_port());
    store::update(&q, |f| f.allow = vec!["comfy_image".into()]).unwrap();
    assert!(tools::advert(&q, false).is_none());
}

#[test]
fn the_offer_reaches_the_main_computer_on_the_heartbeat() {
    let (port, _) = fake_comfy();
    let main = rig("offer-main", KEY);
    let node = rig("offer-node", KEY);
    link(&main, &node);
    point_comfy_at(&node.paths, port);
    store::update(&node.paths, |f| f.allow = vec!["comfy_image".into()]).unwrap();
    let _senders = serve::start(
        &node.node,
        &[SocketAddr::from(([127, 0, 0, 1], free_port()))],
    )
    .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(10);
    let offer = loop {
        let t = store::load(&main.paths).machines[0].tools.clone();
        if t != J::Null || std::time::Instant::now() > end {
            break t;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        offer.get("comfy").and_then(|c| c.bool("up")),
        Some(true),
        "{offer:?}"
    );
    assert!(serve::view(&main.paths, None)
        .compact()
        .contains("dream.safetensors"));
}

#[test]
fn a_picture_made_on_another_node_arrives_whole() {
    let (comfy, hits) = fake_comfy();
    let out = tests::tmp("pic");
    let agent = Arc::new(FakeAgent {
        comfy,
        out: out.join("node-output"),
        asked: Mutex::new(vec![]),
    });
    let node = node_with_tools("studio-box", &["comfy_image"], agent.clone());
    let main = rig("desk", KEY);
    link(&main, &node);
    let mine = out.join("my-images");
    let args = parse(r#"{"prompt":"a lighthouse at dusk","width":512}"#).unwrap();
    let r = tools::call(
        &main.node,
        "studio-box",
        "comfy_image",
        &args,
        &mine,
        Duration::from_secs(20),
    )
    .unwrap();
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.node, "studio-box");
    assert_eq!(*hits.lock().unwrap(), 1, "ComfyUI on the node was asked");
    assert_eq!(
        agent.asked.lock().unwrap().as_slice(),
        ["a lighthouse at dusk"]
    );
    assert_eq!(r.files.len(), 1);
    assert!(r.files[0].starts_with(&mine));
    let got = std::fs::read(&r.files[0]).unwrap();
    assert_eq!(got.len(), picture().len());
    assert_eq!(
        crate::setup::sha256::sha256_hex(&got),
        crate::setup::sha256::sha256_hex(&picture())
    );
    // the pieces do not pile up in the inbox
    assert!(
        main.node.inbox(Some("callres"), 5).len() == 1
            && main
                .node
                .inbox(None, 50)
                .iter()
                .all(|m| !m.topic.starts_with("blob:"))
    );
    // a second call keeps the first picture (a new name)
    let r2 = tools::call(
        &main.node,
        "studio-box",
        "comfy_image",
        &args,
        &mine,
        Duration::from_secs(20),
    )
    .unwrap();
    assert_ne!(r.files[0], r2.files[0]);
    // both computers wrote a line, and neither wrote what was asked for
    for p in [&main.paths, &node.paths] {
        let log = std::fs::read_to_string(p.home.join("logs").join("fleet-calls.log")).unwrap();
        assert!(
            log.contains("comfy_image") && log.contains("ok=true"),
            "{log}"
        );
        assert!(!log.contains("lighthouse"), "{log}");
    }
}

#[test]
fn a_damaged_picture_is_not_kept() {
    let out = tests::tmp("damaged");
    let node = rig("damage-node", KEY);
    let main = rig("damage-main", KEY);
    link(&main, &node);
    // the answer names a file whose pieces do not add up to its hash
    let meta = parse(r#"{"name":"x.png","size":3,"sha":"00","xfer":"zz","n":1}"#).unwrap();
    main.node
        .store("damage-node", "blob1", "blob:zz:0:1", 0, b"abc");
    let e = tools::collect_for_test(&main.node, &[meta], &out).unwrap_err();
    assert!(e.contains("damaged"), "{e}");
    assert!(std::fs::read_dir(&out).map(|d| d.count()).unwrap_or(0) == 0);
}

#[test]
fn a_node_refuses_what_its_owner_did_not_allow() {
    let (comfy, hits) = fake_comfy();
    let out = tests::tmp("refuse");
    let agent = Arc::new(FakeAgent {
        comfy,
        out: out.join("o"),
        asked: Mutex::new(vec![]),
    });
    let node = node_with_tools("careful-box", &[], agent.clone());
    let main = rig("refuse-main", KEY);
    link(&main, &node);
    let args = parse(r#"{"prompt":"x"}"#).unwrap();
    let call = |tool: &str| {
        tools::call(
            &main.node,
            "careful-box",
            tool,
            &args,
            &out,
            Duration::from_secs(10),
        )
        .unwrap()
    };
    let r = call("comfy_image");
    assert!(!r.ok);
    assert!(
        r.text.contains("doesn't offer comfy_image") && r.text.contains("fleet allow comfy_image"),
        "{}",
        r.text
    );
    assert_eq!(*hits.lock().unwrap(), 0, "nothing ran");
    // allowed one thing is not allowed everything
    store::update(&node.paths, |f| f.allow = vec!["mcp:notes/search".into()]).unwrap();
    for tool in ["mcp:notes/delete", "run_command"] {
        let r = call(tool);
        assert!(!r.ok && r.text.contains("doesn't offer"), "{}", r.text);
    }
    // big arguments are refused before anything is sent
    let big = J::Obj(vec![(
        "prompt".into(),
        J::s(&"x".repeat(tools::MAX_ARGS + 1)),
    )]);
    assert!(tools::call(
        &main.node,
        "careful-box",
        "comfy_image",
        &big,
        &out,
        Duration::from_secs(5)
    )
    .is_err());
    // a computer that is not in the fleet cannot be called
    assert!(tools::call(
        &main.node,
        "nobody",
        "comfy_image",
        &args,
        &out,
        Duration::from_secs(5)
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn a_remote_mcp_tool_runs_on_the_node_and_the_answer_comes_back() {
    let dir = tests::tmp("mcp");
    let script = dir.join("fake-mcp");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id";;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo a word","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"annotations":{"readOnlyHint":true}},{"name":"wipe","description":"Wipes things","inputSchema":{"type":"object"}}]}}\n' "$id";;
    *'"method":"tools/call"'*) text=$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: %s"}]}}\n' "$id" "$text";;
  esac
done
"#,
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = dir.join("mcp.json");
    std::fs::write(
        &cfg,
        format!(
            "{{\"mcpServers\":{{\"fakesrv\":{{\"command\":\"{}\"}}}}}}",
            script.display()
        ),
    )
    .unwrap();
    let node = rig("mcp-node", KEY);
    std::fs::create_dir_all(&node.paths.agent_data).unwrap();
    std::fs::write(
        node.paths.agent_data.join("config.json"),
        format!("{{\"mcp_config\":\"{}\"}}", cfg.display()),
    )
    .unwrap();
    store::update(&node.paths, |f| f.allow = vec!["mcp:fakesrv/echo".into()]).unwrap();
    msg_handlers(&node, Arc::new(tools::RealRunner(node.paths.clone())));
    let main = rig("mcp-main", KEY);
    link(&main, &node);

    // what it offers: only the allowed tool, with its description and parameters
    let a = tools::advert(&node.paths, false).unwrap();
    let list = a.get("mcp").unwrap().get("fakesrv").unwrap();
    let J::Arr(l) = list else { panic!("{list:?}") };
    assert_eq!(l.len(), 1);
    assert_eq!((l[0].str("n"), l[0].bool("ro")), (Some("echo"), Some(true)));
    assert!(l[0].str("d").unwrap().contains("Echo"));
    assert!(l[0].get("s").unwrap().compact().contains("\"text\""));

    let args = parse(r#"{"text":"hello band"}"#).unwrap();
    let r = tools::call(
        &main.node,
        "mcp-node",
        "mcp:fakesrv/echo",
        &args,
        &dir,
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.text, "echo: hello band");
    // the tool that was not allowed is refused even though the server has it
    let r = tools::call(
        &main.node,
        "mcp-node",
        "mcp:fakesrv/wipe",
        &args,
        &dir,
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(!r.ok && r.text.contains("doesn't offer"), "{}", r.text);
}

#[test]
fn the_agents_door_wants_this_computer_and_its_token() {
    let (comfy, _) = fake_comfy();
    let out = tests::tmp("door");
    let agent = Arc::new(FakeAgent {
        comfy,
        out: out.join("o"),
        asked: Mutex::new(vec![]),
    });
    let node = node_with_tools("door-node", &["comfy_image"], agent);
    let main = rig("door-main", KEY);
    link(&main, &node);
    let token = tools::local_token(&main.paths);
    let body = format!(
        "{{\"node\":\"door-node\",\"tool\":\"comfy_image\",\"args\":{{\"prompt\":\"x\"}},\"out_dir\":{}}}",
        J::s(&out.join("mine").to_string_lossy()).compact()
    );
    let post = |tok: &str, host: &str| {
        let mut s = TcpStream::connect(addr_of(&main)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(
            s,
            "POST /call HTTP/1.1\r\nHost: {host}\r\nX-Tempo-Token: {tok}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut t = String::new();
        let _ = s.read_to_string(&mut t);
        crate::parse_response(t.as_bytes(), true).unwrap_or((0, t))
    };
    let host = format!("127.0.0.1:{}", main.port);
    assert_eq!(post("wrong", &host).0, 403);
    assert_eq!(post(&token, "evil.example").0, 403);
    let (c, b) = post(&token, &host);
    assert_eq!(c, 200, "{b}");
    let j = parse(&b).unwrap();
    assert_eq!(j.bool("ok"), Some(true));
    assert_eq!(j.strs("files").len(), 1);
    assert!(std::path::Path::new(&j.strs("files")[0]).is_file());
    assert_eq!(j.str("node"), Some("door-node"));
    // the token and the identity are private to this user
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in ["fleet-local.token", "fleet-identity"] {
            assert_eq!(
                std::fs::metadata(main.paths.file(f))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{f}"
            );
        }
    }
}

#[test]
fn allow_and_deny_edit_the_list_and_refuse_odd_entries() {
    let p = paths("allow");
    let mut out = Vec::new();
    assert_eq!(
        allow_cmd(
            &p,
            &["comfy_image".into(), "mcp:notes/*".into()],
            true,
            &mut out
        ),
        0
    );
    assert_eq!(store::load(&p).allow, ["comfy_image", "mcp:notes/*"]);
    assert_eq!(allow_cmd(&p, &["comfy_image".into()], false, &mut out), 0);
    assert_eq!(store::load(&p).allow, ["mcp:notes/*"]);
    assert!(parse_args(&["allow", "run_command"]).is_err());
    assert_eq!(
        parse_args(&["join", "--code", "1-a-b", "--yes"]),
        Ok(FleetCmd::Join {
            code: Some("1-a-b".into()),
            controller: None,
            yes: true
        })
    );
    assert_eq!(parse_args(&["invite"]), Ok(FleetCmd::Invite));
    assert_eq!(
        parse_args(&["rotate", "--yes"]),
        Ok(FleetCmd::Rotate { yes: true })
    );
}

#[test]
fn join_runs_what_a_new_computer_needs_in_order() {
    let p = paths("configure");
    let ctx = p.ctx();
    store::update(&p, |f| {
        f.machines.push(Machine::new("boss", "10.0.0.2", "both"))
    })
    .unwrap();
    crypto::write_private(&p.file("fleet.key"), &KEY).unwrap();
    let before = store::load(&p);
    let key_before = store::read_key(&p);
    let mut did: Vec<String> = vec![];
    let mut out = Vec::new();
    configure_self(&ctx, "client", &mut out, &mut |why, argv, _| {
        did.push(format!(
            "{why}: {}",
            argv.last().cloned().unwrap_or_default()
        ));
        true
    });
    let said = String::from_utf8(out).unwrap();
    assert!(
        did.is_empty(),
        "join must not run model/setup commands: {did:?}"
    );
    assert_eq!(
        store::load(&p),
        before,
        "the existing fleet link was preserved"
    );
    assert_eq!(
        store::read_key(&p),
        key_before,
        "the fleet key was preserved"
    );
    assert!(said.contains("Ollama, models and sign-in settings were left as they are"));
    assert!(said.contains("The fleet messenger start was requested"));
    assert!(said.contains("Check received health on your main computer"));
}
