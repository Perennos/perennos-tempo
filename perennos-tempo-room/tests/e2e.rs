//! The real binary, end to end: `serve --port 0` prints its address on stderr, `/room/health`
//! answers, the CLI invites through the running server, and an MCP session over stdio posts into
//! a room as Tempo, which the server shows at once.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_tempo-room");

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tempo-room-e2e-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http(addr: &str, req: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let code = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (
        code,
        out.split_once("\r\n\r\n")
            .map(|x| x.1.to_string())
            .unwrap_or_default(),
    )
}

#[test]
fn serve_health_cli_and_mcp() {
    let d = tmpdir("serve");
    let data = d.to_str().unwrap().to_string();
    let mut child = Command::new(EXE)
        .args([
            "serve",
            "--port",
            "0",
            "--data",
            &data,
            "--agent",
            "http://127.0.0.1:9",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let err = child.stderr.take().unwrap();
    let _guard = Kill(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut lines = vec![];
    let addr = loop {
        let l = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("serve printed nothing");
        lines.push(l.clone());
        if let Some(rest) = l.strip_prefix("[Perennos Tempo Room on http://") {
            break rest.trim_end_matches("/]").to_string();
        }
    };
    let owner_line = loop {
        let l = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if l.contains("#owner=") {
            break l;
        }
    };
    assert!(
        owner_line
            .trim()
            .starts_with(&format!("http://{addr}/#owner=")),
        "the owner link is printed once at first start: {owner_line}"
    );
    // Health.
    let (c, body) = http(
        &addr,
        &format!("GET /room/health HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(c, 200);
    let h: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(h["ok"], true);
    assert_eq!(h["app"], "tempo-room");
    assert_eq!(h["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(h["rooms"], 0);
    assert_eq!(h["agent"], "down", "nothing listens on port 9");
    // The CLI goes through the running server.
    let out = Command::new(EXE)
        .args(["invite", "family", "--data", &data, "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let inv: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(inv["room"], "family");
    assert!(inv["link"]
        .as_str()
        .unwrap()
        .starts_with(&format!("http://{addr}/#join=")));
    let link = Command::new(EXE)
        .args(["owner-link", "--data", &data])
        .output()
        .unwrap();
    let new_link = String::from_utf8_lossy(&link.stdout).to_string();
    assert!(new_link.contains(&format!("http://{addr}/#owner=")));
    // `owner-link` asked the running server for a new link: the one printed at start is dead,
    // the new one signs in once, and the cookie it gives is not the link.
    let token_of = |t: &str| {
        t.split("#owner=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    let (first, fresh) = (token_of(&owner_line), token_of(&new_link));
    let login = |t: &str| {
        let b = json!({ "token": t }).to_string();
        let mut s = TcpStream::connect(&addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(format!("POST /room/owner HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}", b.len()).as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    };
    assert!(
        login(&first).starts_with("HTTP/1.1 403"),
        "the first link was replaced"
    );
    let ok = login(&fresh);
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(
        ok.contains("Set-Cookie: tr_owner=") && !ok.contains(&fresh),
        "the cookie is a session, not the link: {ok}"
    );
    assert!(
        login(&fresh).starts_with("HTTP/1.1 403"),
        "a link works once"
    );
    // An MCP session over stdio.
    let mut mcp = Command::new(EXE)
        .args(["mcp", "--data", &data])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = mcp.stdin.take().unwrap();
    let mut output = BufReader::new(mcp.stdout.take().unwrap());
    let mut ask = |msg: Value| -> Value {
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
        let mut l = String::new();
        output.read_line(&mut l).unwrap();
        serde_json::from_str(&l).unwrap()
    };
    let init = ask(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "tempo-room");
    let tools = ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 3);
    let list = ask(
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "room_list", "arguments": {}}}),
    );
    assert!(list["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("#family"));
    let posted = ask(
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "room_post", "arguments": {"room": "family", "text": "Dinner is at 7."}}}),
    );
    assert_eq!(
        posted["result"]["content"][0]["text"],
        "posted to #family as Tempo"
    );
    assert_eq!(posted["result"]["isError"], false);
    let read = ask(
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "room_read", "arguments": {"room": "family"}}}),
    );
    assert!(read["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Tempo: Dinner is at 7."));
    drop(input);
    let t0 = Instant::now();
    while mcp.try_wait().unwrap().is_none() {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "mcp ends with its input"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The server shows the post (as the owner, from loopback).
    // (the CLI and MCP use this run's control key, an owner-only file; the config holds no token)
    let key = std::fs::read_to_string(d.join("control.key")).unwrap();
    let (c, body) = http(&addr, &format!("GET /room/messages?room=family HTTP/1.1\r\nHost: {addr}\r\nX-Tempo-Control: {}\r\n\r\n", key.trim()));
    assert_eq!(c, 200);
    let m: Value = serde_json::from_str(&body).unwrap();
    let last = m["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        (
            last["text"].as_str(),
            last["kind"].as_str(),
            last["via"].as_str()
        ),
        (Some("Dinner is at 7."), Some("tempo"), Some("mcp"))
    );
}

#[test]
fn a_second_start_does_not_print_the_owner_token() {
    let d = tmpdir("second");
    let data = d.to_str().unwrap().to_string();
    for round in 0..2 {
        let mut child = Command::new(EXE)
            .args([
                "serve",
                "--port",
                "0",
                "--data",
                &data,
                "--agent",
                "http://127.0.0.1:9",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let err = child.stderr.take().unwrap();
        let _guard = Kill(child);
        let mut r = BufReader::new(err);
        let mut text = String::new();
        for _ in 0..3 {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            text.push_str(&l);
        }
        assert_eq!(text.contains("#owner="), round == 0, "{text}");
    }
}
