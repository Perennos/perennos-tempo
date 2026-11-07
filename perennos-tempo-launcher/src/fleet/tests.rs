use super::msg::{encode, read_frame, Bad, Frame, Node};
use super::serve::{self, Handle};
use super::store::{self, Machine, Paths};
use super::*;
use ed25519_dalek::SigningKey;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

pub fn tmp(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "tempo-fleet-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn paths(tag: &str) -> Paths {
    let d = tmp(tag);
    Paths {
        home: d.join("home"),
        agent_data: d.join("home").join("agent"),
    }
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

/// A node named `name` listening only (no senders) on this computer, with the shared `key`.
pub fn rig(name: &str, key: [u8; 32]) -> Rig {
    let paths = paths(name);
    store::update(&paths, |f| f.me = name.into()).unwrap();
    let node = Node::open(&paths, key).unwrap();
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
    assert_eq!(
        a(&["serve", "--quiet"]),
        Ok(FleetCmd::Serve { quiet: true })
    );
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
fn fake_ssh() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let dir = std::env::temp_dir().join(format!("tempo-fake-ssh-{}", std::process::id()));
    ONCE.call_once(|| {
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("ssh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
d="$(dirname "$0")"
echo "$@" >> "$d/log"
case "$*" in *10.0.0.33*) echo "Permission denied (publickey)." >&2; exit 255;; esac
case "$*" in
  *"sh -s"*)
    input=$(cat)
    printf '%s\n--END--\n' "$input" >> "$d/stdin"
    case "$input" in *install-key*) printf 'The fleet key is in place.\nidentity=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n';; esac
    case "$input" in *nproc*) printf 'os=Linux x86_64\ncpus=8\nram_kb=16777216\ngpu=NVIDIA Test GPU, 8192 MiB\ndisk_kb=104857600\nlaunch=tempo-launch 2.0.1\nollama=ollama version is 0.5.1\n';; esac;;
  *) echo "ran: $*";;
esac
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut parts = vec![dir.clone()];
        parts.extend(std::env::split_paths(&path));
        std::env::set_var("PATH", std::env::join_paths(parts).unwrap());
    });
    dir
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
    fake_ssh();
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
fn setup_shows_the_commands_asks_and_never_puts_the_key_on_a_command_line() {
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
        said.contains("OLLAMA_HOST=127.0.0.1:11434") && said.contains("setup --apply --yes"),
        "{said}"
    );
    assert!(said.contains("Nothing was run"));
    // a yes
    let (c, said) = out_of(|o| setup_cmd(&p, None, "setupbox", true, &mut |_| true, o));
    assert_eq!(c, 0, "{said}");
    let key = crypto::hex(&store::read_key(&p).unwrap());
    let stdin = std::fs::read_to_string(dir.join("stdin")).unwrap();
    assert!(stdin.contains("install-key --name setupbox"));
    assert!(stdin.contains(&key), "the key goes in ssh's input");
    assert!(
        !std::fs::read_to_string(dir.join("log"))
            .unwrap()
            .contains(&key),
        "never on a command line"
    );
    assert!(said.contains("is set up and linked here"));
    assert_eq!(
        store::load(&p).machines[0].id,
        "a".repeat(64),
        "its identity was pinned"
    );
    assert!(!said.contains("identity="));
    assert!(store::load(&p).machines[0].enabled);
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
    let paths = a.paths.clone();
    drop(a);
    let a2 = Node::open(&paths, KEY).unwrap();
    assert_eq!(a2.queued(Some("later")), 1);

    // the peer comes up on that port
    let bnode = Node::open(&bpaths, KEY).unwrap();
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
    assert!(
        b.contains("\"app\":\"tempo-fleet\"") && b.contains("\"name\":\"web\""),
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
    assert!(p50 < 20_000, "p50 {p50} us");
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
