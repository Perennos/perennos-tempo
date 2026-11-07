//! End to end: the real `tempo-vault` binary, as the owner and the agent use it.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tempo-vault"))
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tempo-vault-e2e-{tag}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::create_dir_all(&d);
    d
}

fn run(args: &[&str], vault: &Path) -> (bool, String, String) {
    let o = bin()
        .args(args)
        .arg("--vault")
        .arg(vault)
        .env_remove("TEMPO_VAULT")
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).to_string(),
        String::from_utf8_lossy(&o.stderr).to_string(),
    )
}

#[test]
fn cli_round_trip() {
    let d = tmp("cli");
    let v = d.join("Vault");
    let (ok, out, err) = run(&["init"], &v);
    assert!(ok && out.contains("vault ready"), "{out}{err}");
    assert!(
        run(
            &[
                "new",
                "Projects/Studio",
                "--text",
                "Masters with [[REAPER]]. #audio\n- [ ] render 📅 2026-11-07"
            ],
            &v
        )
        .0
    );
    let (ok, out, _) = run(&["search", "masters", "--json"], &v);
    let hits: Value = serde_json::from_str(&out).unwrap();
    assert!(ok && hits[0]["path"] == "Projects/Studio.md");
    let (_, out, _) = run(&["tasks", "--json"], &v);
    assert!(out.contains("render"), "{out}");
    let (_, out, _) = run(&["unresolved"], &v);
    assert!(out.contains("[[REAPER]]"), "{out}");
    let (ok, out, _) = run(&["read", "Projects/Studio", "--html"], &v);
    assert!(
        ok && out.contains("class=\"internal unresolved\"") && out.contains("data-tag=\"audio\""),
        "{out}"
    );
    let (ok, _, err) = run(&["read", "../../etc/passwd"], &v);
    assert!(!ok && err.contains("not a usable vault path"), "{err}");
    let (ok, _, err) = run(&["frobnicate"], &v);
    assert!(!ok && err.contains("unknown command"));
    let (ok, out, _) = run(&["schedule", "--print"], &v);
    assert!(ok && out.contains("learn --vault"), "{out}");
}

#[test]
fn query_like_a_database() {
    let d = tmp("query");
    let v = d.join("Vault");
    assert!(run(&["init"], &v).0);
    for (name, status, rank) in [
        ("Tempo", "Doing", 2),
        ("Room", "Todo", 1),
        ("Studio", "Done", 3),
    ] {
        let text = format!("---\nstatus: {status}\nrank: {rank}\n---\n# {name}\n");
        assert!(run(&["new", &format!("Projects/{name}"), "--text", &text], &v).0);
    }
    let (ok, out, err) = run(
        &[
            "query",
            "TABLE status, rank FROM \"Projects\" WHERE status != \"Done\" SORT rank DESC",
        ],
        &v,
    );
    assert!(ok, "{err}");
    let (tempo, room) = (
        out.find("[[Tempo]]").unwrap(),
        out.find("[[Room]]").unwrap(),
    );
    assert!(
        tempo < room && !out.contains("Studio") && out.contains("| note | status | rank |"),
        "{out}"
    );
    std::fs::write(v.join("Projects.base"), "views:\n  - type: board\n    name: Board\n    groupBy: status\n    lanes: [Todo, Doing, Done]\n    filters: file.inFolder(\"Projects\")\n").unwrap();
    let (ok, out, err) = run(&["query", "--base", "Projects.base", "--json"], &v);
    assert!(ok, "{err}");
    let j: Value = serde_json::from_str(&out).unwrap();
    let lanes: Vec<&str> = j[0]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["group"].as_str().unwrap())
        .collect();
    assert_eq!(lanes, ["Todo", "Doing", "Done"], "{out}");
    let (ok, _, err) = run(&["query", "TABLE FROM WHERE ((("], &v);
    assert!(!ok && !err.is_empty());
}

#[test]
fn mcp_session_over_stdio() {
    let d = tmp("mcp");
    let v = d.join("Vault");
    assert!(run(&["init"], &v).0);
    let mut child = bin()
        .args(["mcp", "--write", "--vault"])
        .arg(&v)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut si = child.stdin.take().unwrap();
    let mut so = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |msg: Value| -> Value {
        writeln!(si, "{msg}").unwrap();
        si.flush().unwrap();
        let mut line = String::new();
        so.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let init = ask(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "tempo-vault");
    let tools = ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "vault_daily_append"));
    let r = ask(
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "vault_daily_append", "arguments": {"text": "Tempo was here", "heading": "Log", "date": "2026-11-06"}}}),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    let r = ask(
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "vault_search", "arguments": {"query": "tempo was here"}}}),
    );
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Daily/2026-11-06.md"),
        "{r}"
    );
    drop(si);
    assert!(
        child.wait().unwrap().success(),
        "the server ends cleanly when its input closes"
    );
}

#[test]
fn serve_answers_on_loopback() {
    let d = tmp("serve");
    let v = d.join("Vault");
    assert!(run(&["init"], &v).0);
    let mut child = bin()
        .args(["serve", "--port", "0", "--read-only", "--vault"])
        .arg(&v)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    err.read_line(&mut line).unwrap();
    let addr = line
        .split("http://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or_else(|| panic!("no address in {line:?}"))
        .to_string();
    let get = |path: &str, host: &str| -> String {
        let mut s = TcpStream::connect(&addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    };
    let h = get("/vault/health", &addr);
    assert!(
        h.starts_with("HTTP/1.1 200")
            && h.contains("\"read_only\":true")
            && h.contains("\"notes\":4"),
        "{h}"
    );
    assert!(get("/", &addr).contains("Perennos Tempo Vault"));
    assert!(
        get("/vault/health", "attacker.example").starts_with("HTTP/1.1 403"),
        "DNS rebinding is refused"
    );
    let _ = child.kill(); // the test's own server
    let _ = child.wait();
}
