//! MCP stdio host: loads the same `mcpServers` a Claude Code install uses, starts each
//! server as a child process, runs `initialize` / `tools/list` / `tools/call` over
//! newline-delimited JSON-RPC, and shrinks tool schemas into Ollama's tool format.
//! Env values from the config are passed to servers and never printed.
use crate::config::AgentConfig;
use crate::error::{Error, Result};
use crate::ollama::Tool;
use crate::text::truncate_chars;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// One stdio server entry: `{"command": ..., "args": [...], "env": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl ServerConfig {
    /// `None` for entries without a command (http/sse servers are not stdio).
    pub fn from_value(v: &Value) -> Option<ServerConfig> {
        let command = v.get("command")?.as_str().filter(|c| !c.is_empty())?.to_string();
        let args = v.get("args").and_then(Value::as_array).map(|a| a.iter().map(as_text).collect()).unwrap_or_default();
        let env = v
            .get("env")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), as_text(v))).collect())
            .unwrap_or_default();
        Some(ServerConfig { command, args, env })
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn norm_dir(s: &str) -> String {
    let s = s.replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// Stdio servers from `claude_json` (global `mcpServers`, then `projects[dir].mcpServers`
/// for each project dir) and then each project's `.mcp.json`; later entries win.
pub fn load_server_configs(claude_json: &Path, project_dirs: &[PathBuf]) -> BTreeMap<String, ServerConfig> {
    let mut raw: BTreeMap<String, Value> = BTreeMap::new();
    let mut merge = |v: &Value| {
        if let Some(m) = v.get("mcpServers").and_then(Value::as_object) {
            raw.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    };
    let wanted: Vec<String> = project_dirs.iter().map(|d| norm_dir(&d.to_string_lossy())).collect();
    if let Some(j) = read_json(claude_json) {
        merge(&j);
        if let Some(projects) = j.get("projects").and_then(Value::as_object) {
            for (dir, v) in projects {
                if wanted.contains(&norm_dir(dir)) {
                    merge(v);
                }
            }
        }
    }
    for d in project_dirs {
        if let Some(j) = read_json(&d.join(".mcp.json")) {
            merge(&j);
        }
    }
    raw.into_iter().filter_map(|(k, v)| ServerConfig::from_value(&v).map(|c| (k, c))).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl McpTool {
    pub fn from_value(v: &Value) -> Option<McpTool> {
        Some(McpTool {
            name: v.get("name")?.as_str()?.to_string(),
            description: v.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
            input_schema: v.get("inputSchema").cloned().unwrap_or(Value::Null),
        })
    }
}

/// Drop JSON-Schema metadata local models do not need (`title`, `$schema`, `examples`,
/// `default`) and cut descriptions to 120 characters. A *property* called `title` or
/// `default` is an object and is kept.
pub fn shrink_schema(schema: &Value) -> Value {
    fn walk(x: &Value) -> Value {
        match x {
            Value::Object(m) => {
                let mut out = Map::new();
                for (k, v) in m {
                    if matches!(k.as_str(), "title" | "$schema" | "examples" | "default") && !v.is_object() {
                        continue;
                    }
                    let nv = match (k.as_str(), v) {
                        ("description", Value::String(s)) => Value::String(truncate_chars(s, 120).to_string()),
                        ("properties", Value::Object(p)) => {
                            Value::Object(p.iter().map(|(pk, pv)| (pk.clone(), walk(pv))).collect())
                        }
                        _ => walk(v),
                    };
                    out.insert(k.clone(), nv);
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(walk).collect()),
            other => other.clone(),
        }
    }
    let empty = schema.is_null() || schema.as_object().is_some_and(Map::is_empty);
    if empty {
        json!({"type": "object", "properties": {}})
    } else {
        walk(schema)
    }
}

/// The function name the model sees for an MCP tool: `server__tool`, `-` turned into `_`.
pub fn route_name(server: &str, tool: &str) -> String {
    format!("{server}__{tool}").replace('-', "_")
}

pub fn to_ollama_tool(server: &str, tool: &McpTool) -> Tool {
    Tool::function(
        &route_name(server, &tool.name),
        truncate_chars(&tool.description, 300).trim(),
        shrink_schema(&tool.input_schema),
    )
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;
type Pending = Arc<Mutex<HashMap<u64, Sender<Value>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_line(w: &Mutex<Box<dyn Write + Send>>, msg: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    let mut g = lock(w);
    g.write_all(&line)?;
    g.flush()
}

/// One running MCP server. Requests are matched to replies by JSON-RPC id on a reader
/// thread, so calls may come from any thread.
pub struct McpServer {
    pub name: String,
    pub tools: Vec<McpTool>,
    /// Why the server is not usable, if it failed to start.
    pub error: Option<String>,
    writer: Writer,
    pending: Pending,
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
    child: Option<Child>,
}

impl McpServer {
    /// Start `cfg` as a child process and run the handshake. Never panics: a failure
    /// is recorded in `error` and the server has no tools.
    pub fn spawn(name: &str, cfg: &ServerConfig, timeout: Duration) -> McpServer {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args).envs(&cfg.env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Self::failed(name, format!("Io: {e}")),
        };
        match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => Self::connect(name, stdout, stdin, Some(child), timeout),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                Self::failed(name, "Io: no stdio pipes".to_string())
            }
        }
    }

    /// Run the handshake over any byte streams (a child's stdio, or pipes in tests).
    pub fn connect(
        name: &str,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        child: Option<Child>,
        timeout: Duration,
    ) -> McpServer {
        let mut s = McpServer {
            name: name.to_string(),
            tools: Vec::new(),
            error: None,
            writer: Arc::new(Mutex::new(Box::new(writer))),
            pending: Arc::default(),
            closed: Arc::default(),
            next_id: AtomicU64::new(1),
            child,
        };
        s.start_reader(reader);
        if let Err(e) = s.handshake(timeout) {
            s.error = Some(truncate_chars(&format!("{}: {e}", e.kind()), 200).to_string());
            s.stop();
        }
        s
    }

    fn failed(name: &str, error: String) -> McpServer {
        McpServer {
            name: name.to_string(),
            tools: Vec::new(),
            error: Some(error),
            writer: Arc::new(Mutex::new(Box::new(io::sink()))),
            pending: Arc::default(),
            closed: Arc::new(AtomicBool::new(true)),
            next_id: AtomicU64::new(1),
            child: None,
        }
    }

    fn start_reader(&self, reader: impl Read + Send + 'static) {
        let (pending, writer, closed) = (self.pending.clone(), self.writer.clone(), self.closed.clone());
        thread::spawn(move || {
            for line in BufReader::new(reader).split(b'\n') {
                let Ok(line) = line else { break };
                let Ok(m) = serde_json::from_slice::<Value>(&line) else { continue };
                if m.get("result").is_some() || m.get("error").is_some() {
                    let tx = m.get("id").and_then(Value::as_u64).and_then(|id| lock(&pending).remove(&id));
                    if let Some(tx) = tx {
                        let _ = tx.send(m);
                    }
                } else if let (Some(_), Some(id)) = (m.get("method"), m.get("id")) {
                    // A server-to-client request (sampling, roots...): decline politely.
                    let decline = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unsupported"}});
                    let _ = write_line(&writer, &decline);
                }
            }
            closed.store(true, Ordering::SeqCst);
            lock(&pending).clear(); // wake every waiter: the server is gone
        });
    }

    fn handshake(&mut self, timeout: Duration) -> Result<()> {
        let client = json!({"name": "perennos-tempo-agent", "version": env!("CARGO_PKG_VERSION")});
        self.rpc("initialize", json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": client}), timeout)?;
        write_line(&self.writer, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            let r = self.rpc("tools/list", params, timeout)?;
            self.tools.extend(r["tools"].as_array().into_iter().flatten().filter_map(McpTool::from_value));
            cursor = r["nextCursor"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(())
    }

    /// One JSON-RPC request; waits up to `timeout` for the matching reply.
    pub fn rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, tx);
        if self.closed.load(Ordering::SeqCst) {
            lock(&self.pending).remove(&id);
            return Err(Error::Mcp(format!("{} is not running", self.name)));
        }
        if let Err(e) = write_line(&self.writer, &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})) {
            lock(&self.pending).remove(&id);
            return Err(e.into());
        }
        let got = rx.recv_timeout(timeout);
        lock(&self.pending).remove(&id);
        match got {
            Ok(m) => match m.get("error") {
                Some(e) => Err(Error::Mcp(truncate_chars(&e.to_string(), 300).to_string())),
                None => Ok(m.get("result").cloned().unwrap_or(Value::Null)),
            },
            Err(RecvTimeoutError::Timeout) => {
                Err(Error::Timeout(format!("{} {method} timed out after {}s", self.name, timeout.as_secs_f64())))
            }
            Err(RecvTimeoutError::Disconnected) => Err(Error::Mcp(format!("{} closed during {method}", self.name))),
        }
    }

    /// `tools/call`; returns the text items joined by newlines, `ERROR: ` first if the
    /// server flagged the result as an error.
    pub fn call(&self, tool: &str, args: &Value, timeout: Duration) -> Result<String> {
        let args = if args.is_null() { json!({}) } else { args.clone() };
        let r = self.rpc("tools/call", json!({"name": tool, "arguments": args}), timeout)?;
        let text = r["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["type"] == "text")
            .map(|c| c["text"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(if r["isError"] == true { format!("ERROR: {text}") } else { text })
    }

    pub fn is_running(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    /// Close the server's stdin, give it a moment to exit, then kill the exact child
    /// (never anything by name).
    pub fn stop(&mut self) {
        *lock(&self.writer) = Box::new(io::sink());
        if let Some(mut c) = self.child.take() {
            let deadline = Instant::now() + Duration::from_millis(300);
            while matches!(c.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the agent loop needs from a tool provider.
pub trait ToolHost {
    /// Every routable MCP tool as (server, tool). May start servers on first use.
    fn catalog(&mut self) -> Vec<(String, McpTool)>;
    fn call(&mut self, server: &str, tool: &str, args: &Value, timeout: Duration) -> Result<String>;
    /// Servers that failed to start, with the reason.
    fn errors(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// No MCP servers (`--no-mcp`).
pub struct NoTools;

impl ToolHost for NoTools {
    fn catalog(&mut self) -> Vec<(String, McpTool)> {
        Vec::new()
    }
    fn call(&mut self, server: &str, _: &str, _: &Value, _: Duration) -> Result<String> {
        Err(Error::Mcp(format!("MCP is off in this session (asked for '{server}')")))
    }
}

/// Starts configured servers lazily: all of them (in parallel) the first time the
/// catalog is needed, or one by name when it is called directly.
pub struct McpHost {
    configs: BTreeMap<String, ServerConfig>,
    servers: BTreeMap<String, McpServer>,
    skip: Vec<String>,
    start_timeout: Duration,
    catalog: Option<Vec<(String, McpTool)>>,
}

impl McpHost {
    pub fn new(configs: BTreeMap<String, ServerConfig>, skip: Vec<String>, start_timeout: Duration) -> McpHost {
        McpHost { configs, servers: BTreeMap::new(), skip, start_timeout, catalog: None }
    }

    pub fn from_config(cfg: &AgentConfig) -> McpHost {
        let configs = load_server_configs(&cfg.claude_json_path(), &cfg.project_dirs());
        McpHost::new(configs, cfg.mcp_skip.clone(), Duration::from_secs(cfg.mcp_start_timeout_secs))
    }

    /// Configured and attached server names.
    pub fn server_names(&self) -> Vec<String> {
        self.configs.keys().chain(self.servers.keys()).cloned().collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Add an already connected server.
    pub fn attach(&mut self, server: McpServer) {
        self.catalog = None;
        self.servers.insert(server.name.clone(), server);
    }

    /// The named server, started on first use.
    pub fn server(&mut self, name: &str) -> Option<&McpServer> {
        if !self.servers.contains_key(name) {
            let s = McpServer::spawn(name, self.configs.get(name)?, self.start_timeout);
            self.servers.insert(name.to_string(), s);
        }
        self.servers.get(name)
    }

    fn start_all(&mut self, names: &[String]) {
        let todo: Vec<(&String, &ServerConfig)> =
            names.iter().filter(|n| !self.servers.contains_key(*n)).filter_map(|n| self.configs.get_key_value(n)).collect();
        let timeout = self.start_timeout;
        let started: Vec<McpServer> = thread::scope(|s| {
            let handles: Vec<_> = todo.iter().map(|(n, c)| s.spawn(move || McpServer::spawn(n, c, timeout))).collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for srv in started {
            self.servers.insert(srv.name.clone(), srv);
        }
    }
}

impl ToolHost for McpHost {
    fn catalog(&mut self) -> Vec<(String, McpTool)> {
        if self.catalog.is_none() {
            let names: Vec<String> = self.server_names().into_iter().filter(|n| !self.skip.contains(n)).collect();
            self.start_all(&names);
            let cat = names
                .iter()
                .filter_map(|n| self.servers.get(n))
                .flat_map(|s| s.tools.iter().map(|t| (s.name.clone(), t.clone())))
                .collect();
            self.catalog = Some(cat);
        }
        self.catalog.clone().unwrap_or_default()
    }

    fn call(&mut self, server: &str, tool: &str, args: &Value, timeout: Duration) -> Result<String> {
        let s = self.server(server).ok_or_else(|| Error::Mcp(format!("unknown MCP server '{server}'")))?;
        s.call(tool, args, timeout)
    }

    fn errors(&self) -> Vec<(String, String)> {
        self.servers.values().filter_map(|s| s.error.clone().map(|e| (s.name.clone(), e))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{mock_tools_page1, spawn_mock_mcp, TempDir};

    const T: Duration = Duration::from_secs(10);

    fn piped(name: &str) -> McpServer {
        let (rx, tx) = spawn_mock_mcp();
        McpServer::connect(name, rx, tx, None, T)
    }

    #[test]
    fn loads_global_project_and_dot_mcp_servers() {
        let dir = TempDir::new("mcpcfg");
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        // Project keys in .claude.json may use backslashes and a trailing slash.
        let proj_key = format!("{}/", proj.to_string_lossy().replace('/', "\\"));
        let cj = dir.path().join("claude.json");
        let doc = json!({
            "mcpServers": {"a": {"command": "srv-a", "args": ["--x", 2], "env": {"K": 1, "S": "v"}},
                           "remote": {"type": "http", "url": "http://localhost:1/mcp"}},
            "projects": {proj_key: {"mcpServers": {"b": {"command": "srv-b"}, "a": {"command": "srv-a2"}}},
                         "/elsewhere": {"mcpServers": {"c": {"command": "srv-c"}}}}
        });
        std::fs::write(&cj, doc.to_string()).unwrap();
        std::fs::write(proj.join(".mcp.json"), json!({"mcpServers": {"d": {"command": "srv-d"}}}).to_string()).unwrap();
        let got = load_server_configs(&cj, std::slice::from_ref(&proj));
        assert_eq!(got.keys().collect::<Vec<_>>(), ["a", "b", "d"]);
        assert_eq!(got["a"].command, "srv-a2", "project entries override global ones");
        let global = load_server_configs(&cj, &[]);
        assert_eq!(global["a"].args, ["--x", "2"]);
        assert_eq!(global["a"].env, BTreeMap::from([("K".to_string(), "1".to_string()), ("S".to_string(), "v".to_string())]));
        // No .claude.json at all: the project's .mcp.json still counts.
        let only = load_server_configs(&dir.path().join("missing.json"), &[proj]);
        assert_eq!(only.keys().collect::<Vec<_>>(), ["d"]);
    }

    #[test]
    fn shrinks_schemas_to_ollama_tools() {
        let echo = McpTool::from_value(&mock_tools_page1()[0]).unwrap();
        let s = shrink_schema(&echo.input_schema);
        assert!(s.get("$schema").is_none() && s.get("title").is_none());
        let text = &s["properties"]["text"];
        assert!(text.get("title").is_none() && text.get("default").is_none() && text.get("examples").is_none());
        assert_eq!(text["description"].as_str().unwrap().len(), 120);
        assert_eq!(s["properties"]["title"]["type"], "string", "a property named title stays");
        assert_eq!(s["required"], json!(["text"]));
        assert_eq!(shrink_schema(&Value::Null), json!({"type": "object", "properties": {}}));
        assert_eq!(shrink_schema(&json!({})), json!({"type": "object", "properties": {}}));
        // `default` holding an object is a value worth keeping.
        assert_eq!(shrink_schema(&json!({"default": {"a": 1}, "items": [{"title": "x"}]})), json!({"default": {"a": 1}, "items": [{}]}));

        let long = McpTool { name: "run-it".into(), description: format!("  {}", "w".repeat(400)), input_schema: Value::Null };
        let t = to_ollama_tool("comfy-media", &long);
        assert_eq!(t.function.name, "comfy_media__run_it");
        assert_eq!(t.function.description.len(), 298, "cut to 300 chars, then trimmed");
        assert_eq!(serde_json::to_value(&t).unwrap()["type"], "function");
    }

    #[test]
    fn talks_json_rpc_over_pipes() {
        let s = piped("mock");
        assert!(s.error.is_none(), "{:?}", s.error);
        let names: Vec<_> = s.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["echo", "fail", "boom", "slow", "check_decline", "image_render"], "both pages listed");
        assert_eq!(s.call("echo", &json!({"text": "hello"}), T).unwrap(), "hello");
        assert_eq!(s.call("fail", &Value::Null, T).unwrap(), "ERROR: bad input");
        assert!(matches!(s.call("boom", &json!({}), T), Err(Error::Mcp(e)) if e.contains("boom")));
        assert_eq!(s.call("check_decline", &json!({}), T).unwrap(), "declined", "server requests get -32601");
        assert!(matches!(s.call("slow", &json!({}), Duration::from_millis(100)), Err(Error::Timeout(_))));
        assert_eq!(s.call("echo", &json!({"text": "still up"}), T).unwrap(), "still up", "a timeout does not wedge it");
        let mut s = s;
        s.stop();
        assert!(s.call("echo", &json!({"text": "x"}), T).is_err());
    }

    #[test]
    fn a_server_that_hangs_up_fails_fast() {
        let (rx, tx) = std::io::pipe().unwrap();
        drop(tx); // EOF at once: no handshake possible
        let started = Instant::now();
        let s = McpServer::connect("gone", rx, io::sink(), None, Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(5), "no waiting for the timeout");
        assert!(s.error.as_deref().unwrap().starts_with("Mcp: gone"), "{:?}", s.error);
        assert!(!s.is_running() && s.tools.is_empty());
    }

    /// Not a real test: the body of the child process that `spawns_a_real_process` runs.
    #[test]
    fn mock_mcp_child() {
        if std::env::var_os("TEMPO_AGENT_MOCK_MCP").is_none() {
            return;
        }
        let (stdin, stdout) = (io::stdin(), io::stdout());
        crate::testutil::serve_mock_mcp(stdin.lock(), stdout.lock());
        std::process::exit(0);
    }

    fn child_config() -> ServerConfig {
        ServerConfig {
            command: std::env::current_exe().unwrap().to_string_lossy().into_owned(),
            args: ["--exact", "mcp::tests::mock_mcp_child", "--nocapture", "--quiet", "--test-threads=1"]
                .map(String::from)
                .to_vec(),
            env: BTreeMap::from([("TEMPO_AGENT_MOCK_MCP".to_string(), "1".to_string())]),
        }
    }

    #[test]
    fn spawns_a_real_process() {
        let mut s = McpServer::spawn("child", &child_config(), T);
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(s.tools.len(), 6);
        assert_eq!(s.call("echo", &json!({"text": "over stdio"}), T).unwrap(), "over stdio");
        s.stop();
        assert!(!s.is_running());

        let missing = ServerConfig { command: "tempo-agent-no-such-binary".into(), args: vec![], env: BTreeMap::new() };
        let bad = McpServer::spawn("missing", &missing, T);
        assert!(bad.error.as_deref().unwrap().starts_with("Io:") && bad.tools.is_empty());
    }

    #[test]
    fn host_builds_catalog_and_routes_calls() {
        let configs = BTreeMap::from([
            ("child".to_string(), child_config()),
            ("broken".to_string(), ServerConfig { command: "tempo-agent-no-such-binary".into(), args: vec![], env: BTreeMap::new() }),
            ("skipped".to_string(), child_config()),
        ]);
        let mut host = McpHost::new(configs, vec!["skipped".into()], T);
        host.attach(piped("piped"));
        let cat = host.catalog();
        let servers: BTreeSet<_> = cat.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(servers, BTreeSet::from(["child", "piped"]));
        assert_eq!(cat.len(), 12);
        assert_eq!(host.errors().iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["broken"]);
        assert_eq!(host.call("piped", "echo", &json!({"text": "p"}), T).unwrap(), "p");
        // Skipped servers stay out of the catalog but start when called by name.
        assert_eq!(host.call("skipped", "echo", &json!({"text": "s"}), T).unwrap(), "s");
        assert!(matches!(host.call("nope", "echo", &json!({}), T), Err(Error::Mcp(_))));
        assert!(NoTools.catalog().is_empty() && NoTools.call("x", "y", &Value::Null, T).is_err());
    }
}
