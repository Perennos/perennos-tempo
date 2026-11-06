//! Minimal stdio MCP host: JSON-RPC 2.0 over newline-delimited stdio. Stdio servers only.
//! One reader thread per server; responses are matched by id. Env values are never printed.
use crate::config::Config;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerCfg {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// `{"mcpServers": {name: {command, args, env}}}`; entries without `command` are skipped.
pub fn load_server_configs(path: &Path) -> Result<BTreeMap<String, ServerCfg>, String> {
    let txt = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read mcp config {}: {e}", path.display()))?;
    parse_server_configs(&txt)
}

pub fn parse_server_configs(txt: &str) -> Result<BTreeMap<String, ServerCfg>, String> {
    let v: Value = serde_json::from_str(txt).map_err(|e| format!("bad mcp config JSON: {e}"))?;
    let mut out = BTreeMap::new();
    for (name, s) in v
        .get("mcpServers")
        .and_then(|m| m.as_object())
        .into_iter()
        .flatten()
    {
        let Some(command) = s
            .get("command")
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        let args = s
            .get("args")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .map(|x| {
                        x.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| x.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let env = s
            .get("env")
            .and_then(|e| e.as_object())
            .map(|e| {
                e.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_str()
                                .map(String::from)
                                .unwrap_or_else(|| v.to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.insert(
            name.clone(),
            ServerCfg {
                command: command.into(),
                args,
                env,
            },
        );
    }
    Ok(out)
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Value>>>>;

pub struct McpServer {
    pub name: String,
    child: Option<Child>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    pending: Pending,
    next_id: u64,
    pub tools: Vec<Value>,
    pub error: Option<String>,
}

fn write_line(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) -> Result<(), String> {
    let mut g = stdin.lock().map_err(|_| "stdin poisoned".to_string())?;
    writeln!(g, "{msg}")
        .and_then(|_| g.flush())
        .map_err(|e| format!("write: {e}"))
}

impl McpServer {
    pub fn start(name: &str, cfg: &ServerCfg, timeout: Duration) -> McpServer {
        let mut s = McpServer {
            name: name.into(),
            child: None,
            stdin: None,
            pending: Arc::default(),
            next_id: 1,
            tools: vec![],
            error: None,
        };
        if let Err(e) = s.boot(cfg, timeout) {
            s.error = Some(crate::util::head_chars(&e, 200));
            s.stop();
        }
        s
    }

    fn boot(&mut self, cfg: &ServerCfg, timeout: Duration) -> Result<(), String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {}", cfg.command, e.kind()))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or("no stdin")?));
        let stdout = child.stdout.take().ok_or("no stdout")?;
        self.child = Some(child);
        self.stdin = Some(stdin.clone());
        let pending = self.pending.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(m) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let is_resp = m.get("result").is_some() || m.get("error").is_some();
                if let (true, Some(id)) = (is_resp, m.get("id").and_then(|i| i.as_u64())) {
                    if let Some(tx) = pending.lock().ok().and_then(|p| p.get(&id).cloned()) {
                        let _ = tx.send(m);
                    }
                } else if m.get("method").is_some() && m.get("id").is_some() {
                    // server -> client request: decline
                    let _ = write_line(
                        &stdin,
                        &json!({"jsonrpc":"2.0","id":m["id"],"error":{"code":-32601,"message":"unsupported"}}),
                    );
                }
            }
        });
        self.rpc("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"perennos-tempo-local-agent","version":"1"}}), timeout)?;
        write_line(
            self.stdin.as_ref().ok_or("no stdin")?,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )?;
        let r = self.rpc("tools/list", json!({}), timeout)?;
        self.tools = r
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(())
    }

    pub fn rpc(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let (tx, rx) = mpsc::channel();
        self.pending.lock().map_err(|_| "poisoned")?.insert(id, tx);
        let sent = write_line(
            self.stdin.as_ref().ok_or("server not running")?,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        );
        let got = sent.and_then(|_| {
            rx.recv_timeout(timeout).map_err(|_| {
                format!(
                    "{} {} timed out after {:.1}s",
                    self.name,
                    method,
                    timeout.as_secs_f64()
                )
            })
        });
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&id);
        }
        let m = got?;
        if let Some(e) = m.get("error") {
            return Err(crate::util::head_chars(&e.to_string(), 300));
        }
        Ok(m["result"].clone())
    }

    pub fn call(&mut self, tool: &str, args: &Value, timeout: Duration) -> Result<String, String> {
        let r = self.rpc("tools/call", json!({"name": tool, "arguments": if args.is_null() { json!({}) } else { args.clone() }}), timeout)?;
        let text: Vec<&str> = r
            .get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
            .collect();
        let prefix = if r.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) {
            "ERROR: "
        } else {
            ""
        };
        Ok(format!("{prefix}{}", text.join("\n")))
    }

    /// Kills exactly our child process, never anything by name.
    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.stdin = None;
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An MCP tool offered to the model.
#[derive(Clone, Debug)]
pub struct Offered {
    pub server: String,
    pub tool: Value,
}

impl Offered {
    pub fn exposed_name(&self) -> String {
        exposed(&self.server, self.tool["name"].as_str().unwrap_or(""))
    }
}

pub fn exposed(server: &str, tool: &str) -> String {
    format!("{server}__{tool}").replace('-', "_")
}

pub struct McpHost {
    cfgs: BTreeMap<String, ServerCfg>,
    running: HashMap<String, McpServer>,
    catalog: Option<Vec<(String, Value)>>,
    blocked: BTreeMap<String, HashSet<String>>,
    confirm: Vec<(String, String)>,
    hints: BTreeMap<String, String>,
    skip: Vec<String>,
    start_timeout: Duration,
    pub call_timeout: Duration,
}

pub fn tok(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let low = s.to_lowercase();
    let mut cur = String::new();
    for ch in low.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            cur.push(ch);
        } else {
            if cur.len() >= 3 {
                out.insert(cur.clone());
            }
            cur.clear();
        }
    }
    out
}

impl McpHost {
    pub fn new(cfg: &Config, servers: BTreeMap<String, ServerCfg>) -> McpHost {
        McpHost {
            cfgs: servers,
            running: HashMap::new(),
            catalog: None,
            blocked: cfg
                .mcp_blocked
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
                .collect(),
            confirm: cfg.mcp_confirm.clone(),
            hints: cfg.mcp_hints.clone(),
            skip: cfg.mcp_skip.clone(),
            start_timeout: Duration::from_secs_f64(cfg.mcp_start_timeout_s),
            call_timeout: Duration::from_secs_f64(cfg.mcp_call_timeout_s),
        }
    }

    pub fn empty(cfg: &Config) -> McpHost {
        McpHost::new(cfg, BTreeMap::new())
    }

    fn server(&mut self, name: &str) -> Option<&mut McpServer> {
        if !self.running.contains_key(name) {
            let cfg = self.cfgs.get(name)?.clone();
            self.running.insert(
                name.to_string(),
                McpServer::start(name, &cfg, self.start_timeout),
            );
        }
        self.running.get_mut(name)
    }

    pub fn load_catalog(&mut self) -> Vec<(String, Value)> {
        if self.catalog.is_none() {
            let mut cat = Vec::new();
            let names: Vec<String> = self
                .cfgs
                .keys()
                .filter(|n| !self.skip.contains(n))
                .cloned()
                .collect();
            for n in names {
                let blocked = self.blocked.get(&n).cloned().unwrap_or_default();
                if let Some(s) = self.server(&n) {
                    for t in &s.tools {
                        if !blocked.contains(t["name"].as_str().unwrap_or("")) {
                            cat.push((n.clone(), t.clone()));
                        }
                    }
                }
            }
            self.catalog = Some(cat);
        }
        self.catalog.clone().unwrap_or_default()
    }

    /// Startup errors per server, for the status line (never contains env values).
    pub fn errors(&self) -> Vec<(String, String)> {
        self.running
            .values()
            .filter_map(|s| s.error.clone().map(|e| (s.name.clone(), e)))
            .collect()
    }

    /// Route table: exposed name -> (server, tool) for every catalog tool.
    pub fn route(&mut self) -> HashMap<String, (String, String)> {
        self.load_catalog()
            .into_iter()
            .map(|(s, t)| {
                (
                    exposed(&s, t["name"].as_str().unwrap_or("")),
                    (s.clone(), t["name"].as_str().unwrap_or("").to_string()),
                )
            })
            .collect()
    }

    /// Up to min(room, 8) tools ranked by keyword fit to the prompt.
    pub fn select(&mut self, prompt: &str, n_builtin: usize, tool_cap: usize) -> Vec<Offered> {
        let room = tool_cap.saturating_sub(n_builtin);
        let qt = tok(prompt);
        let low = prompt.to_lowercase();
        let mut ranked: Vec<(usize, Offered)> = Vec::new();
        for (srv, t) in self.load_catalog() {
            let name = t["name"].as_str().unwrap_or("").replace('_', " ");
            let desc: String = t
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            let words = tok(&format!("{name} {desc}"));
            let mut sc = 3 * qt.intersection(&tok(&name)).count() + qt.intersection(&words).count();
            if self
                .hints
                .get(&srv)
                .and_then(|h| Regex::new(h).ok())
                .is_some_and(|r| r.is_match(&low))
            {
                sc += 1;
            }
            if sc >= 3 {
                ranked.push((
                    sc,
                    Offered {
                        server: srv,
                        tool: t,
                    },
                ));
            }
        }
        ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
        ranked
            .into_iter()
            .take(room.min(8))
            .map(|(_, o)| o)
            .collect()
    }

    pub fn needs_confirm(&self, server: &str, tool: &str) -> bool {
        self.confirm.iter().any(|(s, t)| s == server && t == tool)
    }

    pub fn call(&mut self, server: &str, tool: &str, args: &Value) -> Result<String, String> {
        let timeout = self.call_timeout;
        let s = self
            .server(server)
            .ok_or_else(|| format!("unknown MCP server {server}"))?;
        if let Some(e) = &s.error {
            return Err(format!("MCP server {server} is not running: {e}"));
        }
        s.call(tool, args, timeout)
    }

    pub fn stop_all(&mut self) {
        for s in self.running.values_mut() {
            s.stop();
        }
    }
}

/// Trim an MCP input schema: drop titles/examples/defaults, cap descriptions at 120 chars.
pub fn shrink(schema: &Value) -> Value {
    fn walk(x: &Value) -> Value {
        match x {
            Value::Object(m) => {
                let mut out = serde_json::Map::new();
                for (k, v) in m {
                    if matches!(k.as_str(), "title" | "$schema" | "examples" | "default")
                        && !v.is_object()
                    {
                        continue; // metadata only; a PROPERTY named 'title' is an object and is kept
                    }
                    if k == "description" && v.is_string() {
                        out.insert(
                            k.clone(),
                            Value::String(v.as_str().unwrap_or("").chars().take(120).collect()),
                        );
                    } else {
                        out.insert(k.clone(), walk(v));
                    }
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(walk).collect()),
            other => other.clone(),
        }
    }
    if schema.is_null() {
        json!({"type": "object", "properties": {}})
    } else {
        walk(schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When TLA_MCP_MOCK is set this "test" is re-invoked as a stdio MCP server (see `mock_cfg`).
    #[test]
    fn mock_server_main() {
        if std::env::var("TLA_MCP_MOCK").is_err() {
            return;
        }
        let out = std::io::stdout();
        let send = |v: Value| {
            let mut o = out.lock();
            let _ = writeln!(o, "{v}");
            let _ = o.flush();
        };
        // the test harness prints "test name ... " without a newline: end that line first
        let _ = writeln!(out.lock());
        let mut decline: Option<Value> = None;
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            let Ok(m) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if m.get("id") == Some(&json!(900)) && m.get("error").is_some() {
                decline = Some(m["error"]["code"].clone());
                continue;
            }
            let Some(method) = m.get("method").and_then(|x| x.as_str()) else {
                continue;
            };
            let id = m.get("id").cloned();
            match (method, id) {
                ("initialize", Some(id)) => {
                    send(
                        json!({"jsonrpc":"2.0","id":900,"method":"sampling/createMessage","params":{}}),
                    );
                    send(
                        json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}),
                    );
                }
                ("tools/list", Some(id)) => {
                    send(json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                    {"name":"echo","description":"echo the text back","inputSchema":{"type":"object","title":"T","properties":{"text":{"type":"string","description":"x","default":"d"}}}},
                    {"name":"slow","description":"sleeps","inputSchema":{"type":"object"}},
                    {"name":"declined","description":"report decline","inputSchema":{"type":"object"}},
                    {"name":"fail","description":"always errors","inputSchema":{"type":"object"}},
                    {"name":"blocked-tool","description":"should be blocked","inputSchema":{"type":"object"}}]}}))
                }
                ("tools/call", Some(id)) => {
                    let name = m["params"]["name"].as_str().unwrap_or("");
                    match name {
                        "echo" => send(
                            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("echo: {}", m["params"]["arguments"]["text"].as_str().unwrap_or(""))}]}}),
                        ),
                        "slow" => {
                            std::thread::sleep(Duration::from_millis(1500));
                            send(
                                json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"late"}]}}),
                            );
                        }
                        "declined" => send(
                            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":format!("decline code {}", decline.clone().unwrap_or(Value::Null))}]}}),
                        ),
                        "fail" => send(
                            json!({"jsonrpc":"2.0","id":id,"result":{"isError":true,"content":[{"type":"text","text":"nope"}]}}),
                        ),
                        _ => send(
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"unknown tool"}}),
                        ),
                    }
                }
                _ => {}
            }
        }
        std::process::exit(0);
    }

    fn mock_cfg() -> ServerCfg {
        ServerCfg {
            command: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            args: vec![
                "--exact".into(),
                "mcp::tests::mock_server_main".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            env: [("TLA_MCP_MOCK".to_string(), "1".to_string())].into(),
        }
    }

    #[test]
    fn config_parsing_skips_entries_without_command() {
        let c = parse_server_configs(r#"{"mcpServers":{"a":{"command":"x","args":["1"],"env":{"K":"v"}},"b":{"url":"http://z"},"c":{}}}"#).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c["a"].args, ["1"]);
        assert_eq!(c["a"].env["K"], "v");
        assert!(parse_server_configs("{").is_err());
        assert!(parse_server_configs("{}").unwrap().is_empty());
    }

    #[test]
    fn initialize_list_call_decline_and_timeout() {
        let mut s = McpServer::start("mock", &mock_cfg(), Duration::from_secs(20));
        assert_eq!(s.error, None);
        assert_eq!(s.tools.len(), 5);
        let r = s
            .call("echo", &json!({"text": "hi"}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(r, "echo: hi");
        assert_eq!(
            s.call("fail", &json!({}), Duration::from_secs(5)).unwrap(),
            "ERROR: nope"
        );
        // server->client request 900 was declined with -32601
        assert_eq!(
            s.call("declined", &json!({}), Duration::from_secs(5))
                .unwrap(),
            "decline code -32601"
        );
        assert!(s
            .call("nope", &json!({}), Duration::from_secs(5))
            .unwrap_err()
            .contains("unknown tool"));
        let e = s
            .call("slow", &json!({}), Duration::from_millis(200))
            .unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        s.stop();
        assert!(s.call("echo", &json!({}), Duration::from_secs(1)).is_err());
    }

    #[test]
    fn missing_command_records_error() {
        let cfg = ServerCfg {
            command: "/definitely/not/a/binary".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let s = McpServer::start("bad", &cfg, Duration::from_secs(1));
        assert!(s.error.is_some());
    }

    #[test]
    fn host_catalog_blocked_select_route_and_call() {
        let mut c = Config::defaults(None);
        c.mcp_blocked
            .insert("my-srv".into(), vec!["blocked-tool".into()]);
        c.mcp_confirm.push(("my-srv".into(), "echo".into()));
        c.mcp_start_timeout_s = 20.0;
        let mut host = McpHost::new(&c, [("my-srv".to_string(), mock_cfg())].into());
        let cat = host.load_catalog();
        assert_eq!(cat.len(), 4);
        assert!(!cat.iter().any(|(_, t)| t["name"] == "blocked-tool"));
        let route = host.route();
        assert_eq!(
            route["my_srv__echo"],
            ("my-srv".to_string(), "echo".to_string())
        );
        // "echo the text" scores: name hit 3 + words
        let sel = host.select("please echo the text back", 13, 40);
        assert_eq!(sel[0].exposed_name(), "my_srv__echo");
        assert!(host.select("unrelated words entirely", 13, 40).is_empty());
        assert!(host.select("echo", 40, 40).is_empty()); // no room
        assert!(host.needs_confirm("my-srv", "echo"));
        assert!(!host.needs_confirm("my-srv", "fail"));
        assert_eq!(
            host.call("my-srv", "echo", &json!({"text": "z"})).unwrap(),
            "echo: z"
        );
        assert!(host.call("zzz", "echo", &json!({})).is_err());
        host.stop_all();
    }

    #[test]
    fn shrink_trims_metadata_keeps_property_named_title() {
        let s = json!({"type":"object","title":"X","$schema":"s","description":"d".repeat(300),
            "properties":{"title":{"type":"string","default":"zz","description":"keep"},"n":{"type":"integer","examples":[1]}}});
        let r = shrink(&s);
        assert!(r.get("title").is_none() && r.get("$schema").is_none());
        assert_eq!(r["description"].as_str().unwrap().len(), 120);
        assert!(r["properties"]["title"].is_object());
        assert!(r["properties"]["title"].get("default").is_none());
        assert!(r["properties"]["n"].get("examples").is_none());
        assert_eq!(
            shrink(&Value::Null),
            json!({"type":"object","properties":{}})
        );
    }
}
