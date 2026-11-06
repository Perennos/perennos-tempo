//! Test doubles: a scripted mock Ollama HTTP server and a mock MCP stdio server.
//! The cloud CI has no GPU and no engines, so every test talks to these.
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Chunked,
    Length,
    Close,
}

#[derive(Debug, Clone)]
pub enum Reply {
    /// NDJSON lines for one `/api/chat` call.
    Chat { lines: Vec<Value>, framing: Framing },
    /// A non-200 answer with this body.
    Status(u16, String),
}

/// A streamed assistant reply in a few pieces, then the `done` chunk with timings.
pub fn text_reply(text: &str) -> Reply {
    let mut lines: Vec<Value> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(7);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        lines.push(json!({"message": {"role": "assistant", "content": &rest[..cut]}, "done": false}));
        rest = &rest[cut..];
    }
    lines.push(done_chunk());
    Reply::Chat { lines, framing: Framing::Chunked }
}

/// An assistant reply that asks for tool calls (no content).
pub fn tool_reply(calls: &[(&str, Value)]) -> Reply {
    let calls: Vec<Value> = calls.iter().map(|(n, a)| json!({"function": {"name": n, "arguments": a}})).collect();
    Reply::Chat {
        lines: vec![
            json!({"message": {"role": "assistant", "content": "", "tool_calls": calls}, "done": false}),
            done_chunk(),
        ],
        framing: Framing::Chunked,
    }
}

fn done_chunk() -> Value {
    json!({"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop",
           "load_duration": 1_500_000_000u64, "prompt_eval_count": 42, "prompt_eval_duration": 250_000_000u64,
           "eval_count": 7, "eval_duration": 100_000_000u64})
}

pub struct MockOllama {
    pub url: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    resident: Arc<Mutex<Vec<String>>>,
}

impl MockOllama {
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock ollama");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let resident = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
        let (rq, rs) = (requests.clone(), resident.clone());
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let _ = serve_one(conn, &rq, &rs, &queue);
            }
        });
        MockOllama { url, requests, resident }
    }

    /// Models reported as loaded by `GET /api/ps`.
    pub fn set_resident(&self, names: &[&str]) {
        *self.resident.lock().unwrap() = names.iter().map(|s| s.to_string()).collect();
    }

    /// Bodies of every `POST /api/chat`, in order.
    pub fn chat_requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().iter().filter(|(p, _)| p == "/api/chat").map(|(_, b)| b.clone()).collect()
    }

    pub fn chat_models(&self) -> Vec<String> {
        self.chat_requests().iter().map(|b| b["model"].as_str().unwrap_or("").to_string()).collect()
    }
}

fn serve_one(
    conn: TcpStream,
    requests: &Mutex<Vec<(String, Value)>>,
    resident: &Mutex<Vec<String>>,
    queue: &Mutex<VecDeque<Reply>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let mut length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let mut w = conn;
    match (method.as_str(), path.as_str()) {
        ("GET", "/api/ps") | ("GET", "/api/tags") => {
            let models: Vec<Value> =
                resident.lock().unwrap().iter().map(|n| json!({"name": format!("{n}:latest")})).collect();
            write_status(&mut w, 200, &json!({ "models": models }).to_string())
        }
        ("POST", _) => {
            let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
            requests.lock().unwrap().push((path.clone(), parsed));
            match queue.lock().unwrap().pop_front() {
                Some(Reply::Chat { lines, framing }) => write_lines(&mut w, &lines, framing),
                Some(Reply::Status(code, body)) => write_status(&mut w, code, &body),
                None => write_status(&mut w, 500, r#"{"error":"mock: no scripted reply left"}"#),
            }
        }
        _ => write_status(&mut w, 404, r#"{"error":"not found"}"#),
    }
}

fn write_status(w: &mut TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    write!(w, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())?;
    w.flush()
}

fn write_lines(w: &mut TcpStream, lines: &[Value], framing: Framing) -> std::io::Result<()> {
    let rows: Vec<String> = lines.iter().map(|l| format!("{l}\n")).collect();
    match framing {
        Framing::Chunked => {
            write!(w, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n")?;
            for r in &rows {
                write!(w, "{:x}\r\n{r}\r\n", r.len())?;
                w.flush()?;
            }
            write!(w, "0\r\n\r\n")?;
        }
        Framing::Length => {
            let all = rows.concat();
            write!(w, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\n\r\n{all}", all.len())?;
        }
        Framing::Close => {
            write!(w, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n{}", rows.concat())?;
        }
    }
    w.flush()
}

/// A scratch folder removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!(
            "tempo-agent-{tag}-{}-{nanos}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A mock MCP server running in a thread, connected by two in-process pipes.
/// Returns (what the client reads, what the client writes).
pub fn spawn_mock_mcp() -> (std::io::PipeReader, std::io::PipeWriter) {
    let (client_rx, server_tx) = std::io::pipe().unwrap();
    let (server_rx, client_tx) = std::io::pipe().unwrap();
    thread::spawn(move || serve_mock_mcp(BufReader::new(server_rx), server_tx));
    (client_rx, client_tx)
}

pub fn mock_tools_page1() -> Value {
    json!([
        {"name": "echo", "description": "Echo text back.",
         "inputSchema": {"$schema": "http://json-schema.org/draft-07/schema#", "title": "EchoArgs", "type": "object",
                         "properties": {"text": {"type": "string", "title": "Text", "description": "d".repeat(300),
                                                 "default": "hi", "examples": ["a"]},
                                        "title": {"type": "string", "description": "a property named title"}},
                         "required": ["text"]}},
        {"name": "fail", "description": "Always reports a tool error."},
        {"name": "boom", "description": "Answers with a JSON-RPC error."},
        {"name": "slow", "description": "Never answers."},
        {"name": "check_decline", "description": "Says whether the client declined the server request."}
    ])
}

/// Speaks MCP over newline-delimited JSON-RPC. Before answering `tools/list` it sends a
/// server-to-client request and a line that is not JSON, to probe the client.
pub fn serve_mock_mcp(reader: impl BufRead, mut w: impl Write) {
    let mut initialized = false;
    let mut declined = false;
    let _ = writeln!(w); // a blank line first; clients must skip it
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
        let method = m.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        if method.is_empty() {
            if m["id"] == json!("srv-1") && m["error"]["code"] == json!(-32601) {
                declined = true;
            }
            continue;
        }
        if method == "notifications/initialized" {
            initialized = true;
            continue;
        }
        let Some(id) = m.get("id").cloned() else { continue };
        let text = |t: String| Ok(json!({"content": [{"type": "text", "text": t}, {"type": "image", "data": "AA=="}]}));
        let result: Result<Value, &str> = match method.as_str() {
            "initialize" => Ok(json!({"protocolVersion": m["params"]["protocolVersion"],
                                      "capabilities": {"tools": {}}, "serverInfo": {"name": "mock", "version": "0"}})),
            "tools/list" if !initialized => Err("tools/list before notifications/initialized"),
            "tools/list" => {
                let _ = writeln!(w, "{}", json!({"jsonrpc": "2.0", "id": "srv-1", "method": "roots/list"}));
                let _ = writeln!(w, "this line is not JSON");
                if m["params"]["cursor"] == json!("p2") {
                    Ok(json!({"tools": [{"name": "image_render", "description": "Render an image from a prompt.",
                                          "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string"}}}}]}))
                } else {
                    Ok(json!({"tools": mock_tools_page1(), "nextCursor": "p2"}))
                }
            }
            "tools/call" => {
                let args = &m["params"]["arguments"];
                match m["params"]["name"].as_str().unwrap_or("") {
                    "echo" => text(args["text"].as_str().unwrap_or("").to_string()),
                    "image_render" => text(format!("rendered: {}", args["prompt"].as_str().unwrap_or(""))),
                    "fail" => Ok(json!({"content": [{"type": "text", "text": "bad input"}], "isError": true})),
                    "boom" => Err("boom"),
                    "slow" => continue,
                    "check_decline" => text(if declined { "declined" } else { "not declined" }.to_string()),
                    _ => Err("unknown tool"),
                }
            }
            _ => Err("method not found"),
        };
        let msg = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": e}}),
        };
        let _ = writeln!(w, "{msg}");
        let _ = w.flush();
    }
}
