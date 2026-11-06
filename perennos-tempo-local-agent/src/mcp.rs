//! Minimal stdio MCP host: JSON-RPC 2.0 over newline-delimited stdio. Stdio servers only.
//! One reader thread per server; responses are matched by id. Env values are never printed.
use crate::config::Config;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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
/// The `~/.claude.json` shape works too, when that file is passed explicitly: its global
/// `mcpServers`, then `projects[dir].mcpServers` and `dir/.mcp.json` for each of `project_dirs`
/// (env TEMPO_MCP_PROJECT_DIRS). Later entries win. Nothing is ever loaded implicitly.
pub fn load_server_configs(
    path: &Path,
    project_dirs: &[PathBuf],
) -> Result<BTreeMap<String, ServerCfg>, String> {
    let txt = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read mcp config {}: {e}", path.display()))?;
    parse_server_configs_for(&txt, project_dirs)
}

pub fn parse_server_configs(txt: &str) -> Result<BTreeMap<String, ServerCfg>, String> {
    parse_server_configs_for(txt, &[])
}

fn norm_dir(s: &str) -> String {
    let s = s.replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

pub fn parse_server_configs_for(
    txt: &str,
    project_dirs: &[PathBuf],
) -> Result<BTreeMap<String, ServerCfg>, String> {
    let v: Value = serde_json::from_str(txt).map_err(|e| format!("bad mcp config JSON: {e}"))?;
    let mut raw: BTreeMap<String, Value> = BTreeMap::new();
    let mut merge = |doc: &Value| {
        for (name, s) in doc
            .get("mcpServers")
            .and_then(|m| m.as_object())
            .into_iter()
            .flatten()
        {
            raw.insert(name.clone(), s.clone());
        }
    };
    merge(&v);
    let wanted: Vec<String> = project_dirs
        .iter()
        .map(|d| norm_dir(&d.to_string_lossy()))
        .collect();
    for (dir, p) in v
        .get("projects")
        .and_then(|p| p.as_object())
        .into_iter()
        .flatten()
    {
        if wanted.contains(&norm_dir(dir)) {
            merge(p);
        }
    }
    for d in project_dirs {
        if let Ok(t) = std::fs::read_to_string(d.join(".mcp.json")) {
            if let Ok(j) = serde_json::from_str::<Value>(&t) {
                merge(&j);
            }
        }
    }
    Ok(raw
        .iter()
        .filter_map(|(name, s)| server_cfg(s).map(|c| (name.clone(), c)))
        .collect())
}

fn server_cfg(s: &Value) -> Option<ServerCfg> {
    let command = s
        .get("command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())?;
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
    Some(ServerCfg {
        command: command.into(),
        args,
        env,
    })
}

/// Environment variables every program needs to find its files and run (upper case).
const BASE_ENV: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USER",
    "USERNAME",
    "LOGNAME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOMEDRIVE",
    "HOMEPATH",
    "OS",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "LANG",
    "LC_ALL",
    "TZ",
    "SHELL",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

type Pending = Arc<Mutex<HashMap<u64, Sender<Value>>>>;

pub struct McpServer {
    pub name: String,
    child: Option<Child>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    pending: Pending,
    next_id: u64,
    pub tools: Vec<Value>,
    pub error: Option<String>,
    /// When it was last started (a server that failed is tried again a minute later).
    pub started: std::time::Instant,
    /// The last lines it wrote to stderr, for the error message (and its log file).
    stderr_tail: Arc<Mutex<std::collections::VecDeque<String>>>,
    log: Option<std::path::PathBuf>,
}

/// How long a server that failed waits before it is started again.
pub const RETRY_AFTER: Duration = Duration::from_secs(60);

fn write_line(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) -> Result<(), String> {
    let mut g = stdin.lock().map_err(|_| "stdin poisoned".to_string())?;
    writeln!(g, "{msg}")
        .and_then(|_| g.flush())
        .map_err(|e| format!("write: {e}"))
}

impl McpServer {
    pub fn start(name: &str, cfg: &ServerCfg, timeout: Duration) -> McpServer {
        McpServer::start_logged(name, cfg, timeout, None)
    }

    /// Start it; what it writes to stderr is kept (the last lines) and appended to `log`.
    pub fn start_logged(
        name: &str,
        cfg: &ServerCfg,
        timeout: Duration,
        log: Option<std::path::PathBuf>,
    ) -> McpServer {
        let mut s = McpServer {
            name: name.into(),
            child: None,
            stdin: None,
            pending: Arc::default(),
            next_id: 1,
            tools: vec![],
            error: None,
            started: std::time::Instant::now(),
            stderr_tail: Arc::default(),
            log,
        };
        if let Err(e) = s.boot(cfg, timeout) {
            // give its last words a moment to arrive: they usually say why (ssh: host is down)
            std::thread::sleep(Duration::from_millis(150));
            let said = s.last_said();
            s.error = Some(crate::util::head_chars(
                &match said {
                    Some(l) => format!("{e} (it said: {l})"),
                    None => e,
                },
                300,
            ));
            s.stop();
        }
        s
    }

    /// The last line it wrote to stderr.
    pub fn last_said(&self) -> Option<String> {
        self.stderr_tail.lock().ok()?.back().cloned()
    }

    /// Still running (it may have stopped after it started: a remote machine went away).
    pub fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn boot(&mut self, cfg: &ServerCfg, timeout: Duration) -> Result<(), String> {
        let mut cmd = Command::new(&cfg.command);
        // A third-party server gets what any program needs to run, this product's own TEMPO_*
        // settings, and what its config lists: not the agent's other environment (keys, tokens).
        cmd.env_clear();
        for (k, v) in std::env::vars_os() {
            let name = k.to_string_lossy().to_ascii_uppercase();
            if BASE_ENV.contains(&name.as_str()) || name.starts_with("TEMPO_") {
                cmd.env(k, v);
            }
        }
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
        if let Some(err) = child.stderr.take() {
            let tail = self.stderr_tail.clone();
            let log = self.log.clone();
            std::thread::spawn(move || {
                let mut file = log.and_then(|p| {
                    if let Some(d) = p.parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    // a log past 1 MB starts over beside the old one (renamed, never removed)
                    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 1 << 20) {
                        let _ = std::fs::rename(&p, p.with_extension("log.old"));
                    }
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(p)
                        .ok()
                });
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    let line = crate::util::redact(&line);
                    if let Some(f) = file.as_mut() {
                        let _ = writeln!(f, "{} {line}", crate::util::now_str());
                    }
                    if let Ok(mut t) = tail.lock() {
                        if t.len() >= 20 {
                            t.pop_front();
                        }
                        t.push_back(crate::util::head_chars(&line, 300));
                    }
                }
            });
        }
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
        // wait for the answer, but not for a program that has already ended (an ssh to a machine
        // that is off ends at once; waiting the whole start timeout would only delay the turn)
        let deadline = std::time::Instant::now() + timeout;
        let got = sent.and_then(|_| loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(m) => break Ok(m),
                Err(mpsc::RecvTimeoutError::Timeout) if left.is_zero() => {
                    break Err(format!(
                        "{} {} timed out after {:.1}s",
                        self.name,
                        method,
                        timeout.as_secs_f64()
                    ))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(Ok(Some(st))) = self.child.as_mut().map(|c| c.try_wait()) {
                        break Err(format!(
                            "{} ended before answering {method} ({st})",
                            self.name
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(format!("{} {method}: no answer", self.name))
                }
            }
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
    trust: Vec<(String, String)>,
    hints: BTreeMap<String, String>,
    skip: Vec<String>,
    start_timeout: Duration,
    pub call_timeout: Duration,
    /// Where each server's stderr is kept (`<data>/logs/mcp-<name>.log`).
    logs: Option<std::path::PathBuf>,
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
            trust: cfg.mcp_trust.clone(),
            hints: cfg.mcp_hints.clone(),
            skip: cfg.mcp_skip.clone(),
            start_timeout: Duration::from_secs_f64(cfg.mcp_start_timeout_s),
            call_timeout: Duration::from_secs_f64(cfg.mcp_call_timeout_s),
            logs: Some(cfg.logs_dir()),
        }
    }

    pub fn empty(cfg: &Config) -> McpHost {
        McpHost::new(cfg, BTreeMap::new())
    }

    fn server(&mut self, name: &str) -> Option<&mut McpServer> {
        if !self.running.contains_key(name) {
            let cfg = self.cfgs.get(name)?.clone();
            let s = McpServer::start_logged(name, &cfg, self.start_timeout, self.log_for(name));
            self.running.insert(name.to_string(), s);
        }
        self.running.get_mut(name)
    }

    fn log_for(&self, name: &str) -> Option<std::path::PathBuf> {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.logs
            .as_ref()
            .map(|d| d.join(format!("mcp-{safe}.log")))
    }

    /// Start again, at most once a minute, a server that failed to start or has stopped since (a
    /// machine reached through `ssh` that was down or restarted). Its tools come back on the next
    /// turn. Called at the start of every turn.
    pub fn retry_failed(&mut self) {
        let names: Vec<String> = self
            .running
            .iter_mut()
            .filter(|(_, s)| s.started.elapsed() >= RETRY_AFTER)
            .filter_map(|(n, s)| (s.error.is_some() || !s.alive()).then(|| n.clone()))
            .collect();
        for n in names {
            let Some(cfg) = self.cfgs.get(&n).cloned() else {
                continue;
            };
            let s = McpServer::start_logged(&n, &cfg, self.start_timeout, self.log_for(&n));
            if s.error.is_none() {
                self.catalog = None; // its tools join the catalog again
            }
            self.running.insert(n, s);
        }
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

    /// Look up a catalog tool by its exposed `server__tool` name.
    pub fn find(&mut self, exposed_name: &str) -> Option<Offered> {
        self.load_catalog()
            .into_iter()
            .find(|(s, t)| exposed(s, t["name"].as_str().unwrap_or("")) == exposed_name)
            .map(|(server, tool)| Offered { server, tool })
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

    /// Whether a tool asks the owner first. A named (server, tool) always asks. (server, "*"),
    /// which every pack connector gets, asks for each of its tools unless the owner's own
    /// `mcp_trust` names that tool; a pack can never set that.
    pub fn needs_confirm(&self, server: &str, tool: &str) -> bool {
        let named =
            |list: &[(String, String)], t: &str| list.iter().any(|(s, x)| s == server && x == t);
        named(&self.confirm, tool) || (named(&self.confirm, "*") && !named(&self.trust, tool))
    }

    pub fn call(&mut self, server: &str, tool: &str, args: &Value) -> Result<String, String> {
        let timeout = self.call_timeout;
        let s = self
            .server(server)
            .ok_or_else(|| format!("unknown MCP server {server}"))?;
        if let Some(e) = &s.error {
            return Err(format!(
                "MCP server {server} is not running: {e} (tried again within a minute)"
            ));
        }
        let r = s.call(tool, args, timeout);
        if r.is_err() && !s.alive() {
            // it went away mid-call: say so, with its last words; it is started again later
            let said = s
                .last_said()
                .map(|l| format!(" (it said: {l})"))
                .unwrap_or_default();
            s.error = Some(format!("stopped{said}"));
        }
        r
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
    #[cfg(unix)]
    #[test]
    fn a_server_that_was_down_is_started_again_and_says_why() {
        let d = crate::util::testutil::tmpdir("mcp-retry");
        let flag = d.join("up");
        // like `ssh zeus tempo-vault mcp` while Zeus is off, then on
        let script = format!(
            r#"if [ -f '{}' ]; then read a; echo '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'; read b; read c; echo '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"vault_search","description":"search"}}]}}}}'; sleep 5; else echo 'ssh: connect to host zeus port 22: Connection refused' >&2; exit 255; fi"#,
            flag.display()
        );
        let cfg = ServerCfg {
            command: "sh".into(),
            args: vec!["-c".into(), script],
            env: BTreeMap::new(),
        };
        let mut c = crate::config::Config::defaults(Some(d.to_string_lossy().into_owned()));
        c.mcp_start_timeout_s = 5.0;
        let mut host = McpHost::new(&c, BTreeMap::from([("zeus".to_string(), cfg)]));
        assert!(host.route().is_empty());
        let errs = host.errors();
        assert!(
            errs[0].1.contains("it said: ssh: connect to host zeus"),
            "{errs:?}"
        );
        let log = std::fs::read_to_string(d.join("logs/mcp-zeus.log")).unwrap();
        assert!(log.contains("Connection refused"), "{log}");
        // within the minute it is not tried again; after it, it is, and its tools come back
        std::fs::write(&flag, "").unwrap();
        host.retry_failed();
        assert!(host.route().is_empty(), "not before a minute has passed");
        host.running.get_mut("zeus").unwrap().started -= RETRY_AFTER;
        host.retry_failed();
        assert!(
            host.route().contains_key(&exposed("zeus", "vault_search")),
            "{:?}",
            host.errors()
        );
        host.stop_all();
    }

    #[cfg(unix)]
    #[test]
    fn third_party_servers_do_not_inherit_the_agents_secrets() {
        std::env::set_var("TLA_LEAK_TEST_KEY", "hunter2-from-the-agent");
        let script = r#"read a; echo '{"jsonrpc":"2.0","id":1,"result":{}}'; read b; read c; echo "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"t\",\"description\":\"seen=[$TLA_LEAK_TEST_KEY] own=[$OWN] path=[${PATH:+yes}]\"}]}}"; sleep 3"#;
        let cfg = ServerCfg {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::from([("OWN".to_string(), "listed-in-config".to_string())]),
        };
        let s = McpServer::start("leak", &cfg, Duration::from_secs(10));
        assert!(s.error.is_none(), "{:?}", s.error);
        let d = s.tools[0]["description"].as_str().unwrap().to_string();
        assert!(
            !d.contains("hunter2"),
            "the agent's variable reached the server: {d}"
        );
        assert!(
            d.contains("own=[listed-in-config]") && d.contains("path=[yes]"),
            "{d}"
        );
    }

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
    fn claude_json_shape_with_project_dirs() {
        let d = crate::util::testutil::tmpdir("mcpproj");
        let proj = d.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(".mcp.json"),
            r#"{"mcpServers":{"d":{"command":"srv-d"}}}"#,
        )
        .unwrap();
        // Project keys in .claude.json may use backslashes and a trailing slash.
        let key = format!("{}/", proj.to_string_lossy().replace('/', "\\"));
        let doc = json!({
            "mcpServers": {"a": {"command": "srv-a"}},
            "projects": {key: {"mcpServers": {"b": {"command": "srv-b"}, "a": {"command": "srv-a2"}}},
                         "/elsewhere": {"mcpServers": {"c": {"command": "srv-c"}}}}
        });
        let file = d.join("claude.json");
        std::fs::write(&file, doc.to_string()).unwrap();
        let got = load_server_configs(&file, std::slice::from_ref(&proj)).unwrap();
        assert_eq!(got.keys().collect::<Vec<_>>(), ["a", "b", "d"]);
        assert_eq!(
            got["a"].command, "srv-a2",
            "project entries override global ones"
        );
        // Without project dirs only the global servers load.
        let global = load_server_configs(&file, &[]).unwrap();
        assert_eq!(global.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(global["a"].command, "srv-a");
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
