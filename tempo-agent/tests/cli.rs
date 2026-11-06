//! End-to-end: run the `tempo-agent` binary against a tiny mock Ollama.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// Answers `/api/ps` with no models and every `/api/chat` with `reply`, streamed in
/// two pieces. Returns the base URL and the chat request bodies it saw.
fn mock_ollama(reply: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut len = 0;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap() == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let out = if path == "/api/chat" {
                log.lock().unwrap().push(String::from_utf8(body).unwrap());
                let (a, b) = reply.split_at(reply.len() / 2);
                format!(
                    "{}\n{}\n{}\n",
                    serde_json::json!({"message": {"role": "assistant", "content": a}, "done": false}),
                    serde_json::json!({"message": {"role": "assistant", "content": b}, "done": false}),
                    serde_json::json!({"message": {"role": "assistant", "content": ""}, "done": true, "eval_count": 2})
                )
            } else {
                r#"{"models":[]}"#.to_string()
            };
            let mut w = conn;
            write!(w, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{out}", out.len()).unwrap();
        }
    });
    (url, seen)
}

fn home(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("tempo-agent-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn agent(url: &str, home: &PathBuf, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tempo-agent"))
        .args(args)
        .env("OLLAMA_URL", url)
        .env("TEMPO_AGENT_HOME", home)
        .env_remove("TEMPO_AGENT_CONFIG")
        .env_remove("TEMPO_OLLAMA")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn help_version_and_bad_options() {
    let h = home("help");
    let out = agent("http://127.0.0.1:9", &h, &["--help"], "");
    assert!(out.status.success() && text(&out.stdout).starts_with("usage: tempo-agent"));
    let out = agent("http://127.0.0.1:9", &h, &["-V"], "");
    assert!(text(&out.stdout).starts_with("tempo-agent 0."));
    let out = agent("http://127.0.0.1:9", &h, &["--lane", "sideways"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("--lane must be auto, fast or heavy"));
    let out = agent("http://127.0.0.1:9", &h, &["--bogus"], "");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn one_shot_prompt_streams_the_answer() {
    let (url, seen) = mock_ollama("pong from the mock");
    let h = home("oneshot");
    let out = agent(&url, &h, &["--no-mcp", "--fast-model", "tiny", "ping"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "pong from the mock\n");
    let err = text(&out.stderr);
    assert!(err.contains(&format!("[status] engine {url}")) && err.contains("[status] lane: fast (tiny)"), "{err}");
    let req: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
    assert_eq!(req["model"], "tiny");
    assert_eq!(req["messages"][1]["content"], "ping");
    let wfs: Vec<_> = std::fs::read_dir(h.join("workflows")).unwrap().flatten().collect();
    assert_eq!(wfs.len(), 1, "a workflow file is created under TEMPO_AGENT_HOME");
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn reads_turns_from_stdin_and_url_flag_wins() {
    let (url, seen) = mock_ollama("pong");
    let h = home("repl");
    let wf = h.join("plan.md");
    let wf_arg = wf.to_string_lossy().into_owned();
    // OLLAMA_URL points nowhere; --url must take precedence.
    let out = Command::new(env!("CARGO_BIN_EXE_tempo-agent"))
        .args(["-q", "--no-mcp", "--url", &url, "--workflow", &wf_arg])
        .env("OLLAMA_URL", "http://127.0.0.1:9")
        .env("TEMPO_AGENT_HOME", &h)
        .env_remove("TEMPO_AGENT_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin.take().unwrap().write_all(b"hi\n\nhello again\nexit\nnot sent\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert!(out.status.success());
    assert_eq!(text(&out.stdout), "pong\npong\n");
    assert_eq!(text(&out.stderr), "", "quiet mode prints answers only");
    assert_eq!(seen.lock().unwrap().len(), 2);
    let log = std::fs::read_to_string(&wf).unwrap();
    assert!(log.contains("turn done: hi") && log.contains("turn done: hello again"));
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn unreachable_engine_fails_cleanly() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let h = home("down");
    let out = agent(&url, &h, &["--no-mcp", "--lane", "heavy", "--model", "m1", "--model", "m2", "hello"], "");
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("[status] engine m1 failed") && err.contains("[status] engine m2 failed"), "{err}");
    assert!(err.contains("[error] all engines failed in the heavy lane"), "{err}");
    let _ = std::fs::remove_dir_all(&h);
}
