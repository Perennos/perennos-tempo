//! Tools anywhere: what each computer offers the fleet, and calling it from another.
//!
//! A computer offers only what its own owner allowed in `fleet.json` (`allow`: `comfy_image`,
//! `mcp:SERVER/TOOL`, `mcp:SERVER/*`); with nothing allowed it offers nothing. The offer rides on
//! its heartbeat. A call goes controller, messenger, node: the controller's agent posts it to
//! `fleet serve` on this computer (`POST /call`, with a local token), the call travels as a signed
//! `call` message, the node checks the allow list again and runs it, and the answer comes back as
//! `callres`, with any file in 256 KiB `blob` pieces that are put together and checked against
//! their SHA-256. Both computers write a line to `logs/fleet-calls.log` for every call. Arguments
//! are capped; nothing about a call's content is logged. A call is never retried by itself: a
//! tool may write.

use super::msg::{lock, Handler, Node};
use super::store::{self, Paths};
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_ARGS: usize = 32 << 10;
pub const CHUNK: usize = 256 << 10;
const MAX_FILE: usize = 64 << 20;
const MAX_TEXT: usize = 60_000;

// ---- the allow list

/// Whether `tool` (`comfy_image` or `mcp:SERVER/TOOL`) is on the list.
pub fn allowed(allow: &[String], tool: &str) -> bool {
    allow.iter().any(|a| {
        a == tool
            || a.strip_suffix("/*")
                .is_some_and(|s| tool.starts_with(s) && tool[s.len()..].starts_with('/'))
    })
}

/// An entry the owner may write: `comfy_image`, `mcp:server/tool` or `mcp:server/*`.
pub fn allow_entry_ok(e: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 80
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    };
    e == "comfy_image"
        || e.strip_prefix("mcp:")
            .and_then(|r| r.split_once('/'))
            .is_some_and(|(s, t)| ok(s) && (t == "*" || ok(t)))
}

// ---- what this computer has

fn agent_settings(p: &Paths) -> Option<J> {
    let path = crate::setup::apply::pinned_agent_settings()
        .unwrap_or_else(|| crate::setup::apply::agent_settings_path(&p.agent_data));
    parse(&std::fs::read_to_string(path).ok()?).ok()
}

fn comfy_base(p: &Paths) -> String {
    agent_settings(p)
        .and_then(|j| {
            j.str("comfy_url")
                .map(|u| u.trim_end_matches('/').to_string())
        })
        .filter(|u| crate::parse_remote_url(u).is_ok())
        .unwrap_or_else(|| "http://127.0.0.1:8188".into())
}

fn get_json(base: &str, path: &str) -> Option<J> {
    let (https, host, port) = crate::parse_remote_url(base).ok()?;
    if https {
        return None;
    }
    let a = std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))
        .ok()?
        .find(|a| crate::private_ip(a.ip()))?;
    let (c, b) = crate::http_get_at(
        a,
        &format!("{host}:{port}"),
        path,
        Duration::from_millis(900),
    )?;
    (c == 200).then(|| parse(&b).ok()).flatten()
}

/// ComfyUI at `base`: whether it answers, the checkpoints it has and the memory of its card.
pub fn comfy_state(base: &str) -> J {
    let Some(stats) = get_json(base, "/system_stats") else {
        return J::Obj(vec![("up".into(), J::Bool(false))]);
    };
    let gpu_gb = stats
        .arr("devices")
        .iter()
        .filter_map(|d| d.f64("vram_total"))
        .fold(0.0, f64::max)
        / 1e9;
    let checkpoints: Vec<String> = get_json(base, "/object_info/CheckpointLoaderSimple")
        .and_then(|j| {
            let names = j
                .get("CheckpointLoaderSimple")?
                .get("input")?
                .get("required")?
                .get("ckpt_name")?;
            match names {
                J::Arr(a) => match a.first() {
                    Some(J::Arr(n)) => Some(
                        n.iter()
                            .filter_map(|x| {
                                if let J::Str(s) = x {
                                    Some(s.clone())
                                } else {
                                    None
                                }
                            })
                            .take(30)
                            .collect(),
                    ),
                    _ => None,
                },
                _ => None,
            }
        })
        .unwrap_or_default();
    J::Obj(vec![
        ("up".into(), J::Bool(true)),
        ("gpu_gb".into(), J::n((gpu_gb * 10.0).round() / 10.0)),
        ("checkpoints".into(), J::strings(&checkpoints)),
    ])
}

#[derive(Clone, Debug)]
pub struct McpServer {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The MCP servers the agent is set up with (`{"mcpServers": {...}}`, the file `mcp_config` names).
pub fn mcp_servers(p: &Paths) -> BTreeMap<String, McpServer> {
    let path = std::env::var("TEMPO_MCP_CONFIG")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| agent_settings(p).and_then(|j| j.str("mcp_config").map(String::from)));
    let doc = path
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| parse(&t).ok());
    let mut out = BTreeMap::new();
    if let Some(J::Obj(o)) = doc.as_ref().and_then(|d| d.get("mcpServers")) {
        for (name, s) in o {
            if let Some(command) = s.str("command") {
                out.insert(
                    name.clone(),
                    McpServer {
                        command: command.into(),
                        args: s.strs("args"),
                        env: match s.get("env") {
                            Some(J::Obj(e)) => e
                                .iter()
                                .filter_map(|(k, v)| {
                                    if let J::Str(v) = v {
                                        Some((k.clone(), v.clone()))
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                            _ => vec![],
                        },
                    },
                );
            }
        }
    }
    out
}

struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<String>,
    id: u64,
}

impl Session {
    fn start(s: &McpServer) -> Result<Session, String> {
        let mut c = Command::new(&s.command);
        c.args(&s.args)
            .envs(s.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000);
        }
        let mut child = c
            .spawn()
            .map_err(|e| format!("could not start {} ({e})", s.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let out = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let mut me = Session {
            child,
            stdin,
            rx,
            id: 0,
        };
        me.rpc(
            "initialize",
            J::Obj(vec![
                ("protocolVersion".into(), J::s("2024-11-05")),
                ("capabilities".into(), J::Obj(vec![])),
                (
                    "clientInfo".into(),
                    J::Obj(vec![
                        ("name".into(), J::s("tempo-fleet")),
                        ("version".into(), J::s(crate::VERSION)),
                    ]),
                ),
            ]),
            Duration::from_secs(20),
        )?;
        let _ = writeln!(
            me.stdin,
            "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}"
        );
        Ok(me)
    }

    fn rpc(&mut self, method: &str, params: J, limit: Duration) -> Result<J, String> {
        self.id += 1;
        let req = J::Obj(vec![
            ("jsonrpc".into(), J::s("2.0")),
            ("id".into(), J::n(self.id as f64)),
            ("method".into(), J::s(method)),
            ("params".into(), params),
        ]);
        writeln!(self.stdin, "{}", req.compact()).map_err(|e| e.to_string())?;
        let end = Instant::now() + limit;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let line = self
                .rx
                .recv_timeout(left)
                .map_err(|_| format!("{method} took too long"))?;
            let Ok(j) = parse(&line) else { continue };
            if j.f64("id") == Some(self.id as f64) {
                if let Some(e) = j.get("error") {
                    return Err(e
                        .str("message")
                        .unwrap_or("the tool server said no")
                        .to_string());
                }
                return Ok(j.get("result").cloned().unwrap_or(J::Null));
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // only the child this program started
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub desc: String,
    pub schema: J,
    pub read_only: bool,
}

pub fn mcp_list(s: &McpServer) -> Result<Vec<McpTool>, String> {
    let mut se = Session::start(s)?;
    let r = se.rpc("tools/list", J::Obj(vec![]), Duration::from_secs(20))?;
    Ok(r.arr("tools")
        .iter()
        .filter_map(|t| {
            Some(McpTool {
                name: t.str("name")?.to_string(),
                desc: t
                    .str("description")
                    .unwrap_or("")
                    .chars()
                    .take(300)
                    .collect(),
                schema: t.get("inputSchema").cloned().unwrap_or(J::Null),
                read_only: t.get("annotations").and_then(|a| a.bool("readOnlyHint")) == Some(true),
            })
        })
        .collect())
}

pub fn mcp_call(s: &McpServer, tool: &str, args: &J) -> Result<String, String> {
    let mut se = Session::start(s)?;
    let r = se.rpc(
        "tools/call",
        J::Obj(vec![
            ("name".into(), J::s(tool)),
            ("arguments".into(), args.clone()),
        ]),
        Duration::from_secs(300),
    )?;
    let text: Vec<&str> = r
        .arr("content")
        .iter()
        .filter(|c| c.str("type") == Some("text"))
        .filter_map(|c| c.str("text"))
        .collect();
    let prefix = if r.bool("isError") == Some(true) {
        "ERROR: "
    } else {
        ""
    };
    Ok(format!("{prefix}{}", text.join("\n")))
}

type McpCache = Mutex<Option<(Instant, String, J)>>;
static MCP_CACHE: McpCache = Mutex::new(None);

/// What this computer offers. `all`: everything it has (what pairing shows the main computer: the
/// servers by name, no tools listed, nothing started). Otherwise only what the owner allowed, with
/// each tool's description and parameters (servers are asked at most every ten minutes).
pub fn advert(p: &Paths, all: bool) -> Option<J> {
    let fleet = store::load(p);
    let mut o: Vec<(String, J)> = vec![];
    let comfy = comfy_state(&comfy_base(p));
    if comfy.bool("up") == Some(true) && (all || allowed(&fleet.allow, "comfy_image")) {
        o.push(("comfy".into(), comfy));
    }
    let servers = mcp_servers(p);
    let mut mcp: Vec<(String, J)> = vec![];
    for (name, s) in &servers {
        if all {
            mcp.push((name.clone(), J::Arr(vec![])));
        } else if fleet
            .allow
            .iter()
            .any(|a| a.starts_with(&format!("mcp:{name}/")))
        {
            let stamp = format!("{name}{}", fleet.allow.join(","));
            let cached = lock(&MCP_CACHE)
                .as_ref()
                .filter(|(t, k, _)| t.elapsed() < Duration::from_secs(600) && *k == stamp)
                .map(|x| x.2.clone());
            let tools = cached.unwrap_or_else(|| {
                let list = mcp_list(s).unwrap_or_default();
                let j = J::Arr(
                    list.into_iter()
                        .filter(|t| allowed(&fleet.allow, &format!("mcp:{name}/{}", t.name)))
                        .take(40)
                        .map(|t| {
                            let schema = if t.schema.compact().len() > 2048 {
                                J::Null
                            } else {
                                t.schema.clone()
                            };
                            J::Obj(vec![
                                ("n".into(), J::s(&t.name)),
                                ("ro".into(), J::Bool(t.read_only)),
                                ("d".into(), J::s(&t.desc)),
                                ("s".into(), schema),
                            ])
                        })
                        .collect(),
                );
                *lock(&MCP_CACHE) = Some((Instant::now(), stamp, j.clone()));
                j
            });
            mcp.push((name.clone(), tools));
        }
    }
    if !mcp.is_empty() {
        o.push(("mcp".into(), J::Obj(mcp)));
    }
    let ctx = crate::Ctx::from_env();
    for (k, bin) in [("studio", "tempo-studio"), ("vault", "tempo-vault")] {
        if ctx.find(bin).is_some() {
            o.push((k.into(), J::Bool(true)));
        }
    }
    (!o.is_empty()).then_some(J::Obj(o))
}

// ---- running a call on this computer

pub struct RunOut {
    pub text: String,
    pub files: Vec<PathBuf>,
}

pub trait CallRunner: Send + Sync {
    fn comfy(&self, args: &J) -> Result<RunOut, String>;
    fn mcp(&self, server: &str, tool: &str, args: &J) -> Result<String, String>;
}

/// ComfyUI through this computer's own agent (it keeps the GPU rules: the lease, unloading its
/// models first), and MCP tools through the servers in the agent's settings.
pub struct RealRunner(pub Paths);

impl CallRunner for RealRunner {
    fn comfy(&self, args: &J) -> Result<RunOut, String> {
        let bin = crate::Ctx::from_env()
            .find("tempo-local-agent")
            .ok_or("the Tempo agent is not installed on this computer")?;
        let mut c = Command::new(bin);
        let cfg = crate::setup::apply::pinned_agent_settings()
            .unwrap_or_else(|| crate::setup::apply::agent_settings_path(&self.0.agent_data));
        if cfg.is_file() {
            c.arg("--config").arg(cfg);
        }
        c.args(["fleet-run", "comfy_image"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000);
        }
        let mut child = c
            .spawn()
            .map_err(|e| format!("could not start the agent ({e})"))?;
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(args.compact().as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let said = String::from_utf8_lossy(&out.stdout);
        let j = said
            .lines()
            .rev()
            .find_map(|l| parse(l).ok())
            .ok_or("the agent gave no answer")?;
        let text = j.str("text").unwrap_or("").to_string();
        if j.bool("ok") != Some(true) {
            return Err(text);
        }
        Ok(RunOut {
            text,
            files: j.strs("files").into_iter().map(PathBuf::from).collect(),
        })
    }

    fn mcp(&self, server: &str, tool: &str, args: &J) -> Result<String, String> {
        let s = mcp_servers(&self.0)
            .remove(server)
            .ok_or_else(|| format!("no MCP server called {server} here"))?;
        mcp_call(&s, tool, args)
    }
}

pub fn log_call(p: &Paths, line: &str) {
    if let Ok(mut f) = crate::open_log(&p.home.join("logs").join("fleet-calls.log")) {
        let _ = writeln!(f, "{} {line}", crate::stamp(store::now_s()));
    }
}

fn safe_name(n: &str) -> String {
    let base = n.rsplit(['/', '\\']).next().unwrap_or("file");
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._- ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s
        .trim_matches(['.', ' '])
        .chars()
        .take(100)
        .collect::<String>();
    if s.is_empty() {
        "file".into()
    } else {
        s
    }
}

pub struct CallHandler {
    paths: Paths,
    runner: Arc<dyn CallRunner>,
}

impl CallHandler {
    pub fn new(paths: Paths, runner: Arc<dyn CallRunner>) -> CallHandler {
        CallHandler { paths, runner }
    }
}

impl Handler for CallHandler {
    fn on(&self, node: &Arc<Node>, from: &str, topic: &str, _id: &str, body: &[u8]) {
        if topic != "call" {
            return;
        }
        let Some(j) = std::str::from_utf8(body).ok().and_then(|t| parse(t).ok()) else {
            return;
        };
        let (node, runner, paths, from) = (
            node.clone(),
            self.runner.clone(),
            self.paths.clone(),
            from.to_string(),
        );
        std::thread::spawn(move || run_call(&node, &*runner, &paths, &from, &j));
    }
}

fn run_call(node: &Arc<Node>, runner: &dyn CallRunner, p: &Paths, from: &str, j: &J) {
    let _busy = super::route::Busy::new();
    let started = Instant::now();
    let call = j.str("call").unwrap_or("").to_string();
    let tool = j.str("tool").unwrap_or("").to_string();
    let args = j.get("args").cloned().unwrap_or(J::Obj(vec![]));
    let fleet = store::load(p);
    let result: Result<RunOut, String> = if !fleet.peers().any(|m| m.name == from) {
        Err("refused: this computer does not know you".into())
    } else if !allowed(&fleet.allow, &tool) {
        Err(format!("refused: this computer doesn't offer {tool} to the fleet. Its owner can allow it with `tempo-launch fleet allow {tool}` on that computer."))
    } else if args.compact().len() > MAX_ARGS {
        Err(format!(
            "refused: the arguments are over {} KiB",
            MAX_ARGS >> 10
        ))
    } else if tool == "comfy_image" {
        runner.comfy(&args)
    } else if let Some((s, t)) = tool.strip_prefix("mcp:").and_then(|r| r.split_once('/')) {
        runner.mcp(s, t, &args).map(|text| RunOut {
            text,
            files: vec![],
        })
    } else {
        Err(format!(
            "refused: {tool} is not something the fleet can run"
        ))
    };
    let mut files = Vec::new();
    let (ok, mut text) = match &result {
        Ok(o) => (true, o.text.clone()),
        Err(e) => (false, e.clone()),
    };
    if let Ok(o) = &result {
        for path in o.files.iter().take(4) {
            match std::fs::read(path) {
                Ok(b) if b.len() <= MAX_FILE => files.push((safe_name(&path.to_string_lossy()), b)),
                _ => text
                    .push_str(" (a file was left behind: it could not be read or is over 64 MiB)"),
            }
        }
    }
    let Some(m) = fleet.peers().find(|m| m.name == from).cloned() else {
        return;
    };
    let mut metas = Vec::new();
    for (name, bytes) in &files {
        let xfer = super::crypto::hex(&super::crypto::random(6));
        let n = bytes.len().div_ceil(CHUNK).max(1);
        for (i, c) in bytes.chunks(CHUNK).enumerate() {
            let _ = node.enqueue(&m.name, &format!("blob:{xfer}:{i}:{n}"), c);
        }
        metas.push(J::Obj(vec![
            ("name".into(), J::s(name)),
            ("size".into(), J::n(bytes.len() as f64)),
            ("sha".into(), J::s(&sha256_hex(bytes))),
            ("xfer".into(), J::s(&xfer)),
            ("n".into(), J::n(n as f64)),
        ]));
    }
    let ms = started.elapsed().as_millis() as u64;
    let res = J::Obj(vec![
        ("call".into(), J::s(&call)),
        ("ok".into(), J::Bool(ok)),
        (
            "text".into(),
            J::s(&text.chars().take(MAX_TEXT).collect::<String>()),
        ),
        ("files".into(), J::Arr(metas)),
        ("ms".into(), J::n(ms as f64)),
        ("node".into(), J::s(&node.me)),
    ]);
    log_call(
        p,
        &format!(
            "ran for={from} tool={tool} args={}B ok={ok} ms={ms} files={}",
            args.compact().len(),
            files.len()
        ),
    );
    let _ = node.enqueue(&m.name, "callres", res.compact().as_bytes());
    // blobs first, then the answer: all of it, not one batch
    for _ in 0..200 {
        if node.queued(Some(&m.name)) == 0 || node.deliver(&m).is_err() {
            break;
        }
    }
}

// ---- calling from this computer

pub struct CallOut {
    pub ok: bool,
    pub text: String,
    pub files: Vec<PathBuf>,
    pub ms: u64,
    pub node: String,
}

fn free_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = name
        .rsplit_once('.')
        .map_or((name, String::new()), |(s, e)| (s, format!(".{e}")));
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// (for tests of the checks)
#[cfg(test)]
pub fn collect_for_test(
    node: &Node,
    metas: &[J],
    dir: &std::path::Path,
) -> Result<Vec<PathBuf>, String> {
    collect(node, metas, dir, Duration::from_millis(200))
}

/// Put the pieces of each file together, check size and SHA-256, and write it into `dir`.
fn collect(
    node: &Node,
    metas: &[J],
    dir: &std::path::Path,
    patience: Duration,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for m in metas {
        let (xfer, n) = (
            m.str("xfer").unwrap_or(""),
            m.f64("n").unwrap_or(0.0) as usize,
        );
        let end = Instant::now() + patience;
        let mut data: Vec<u8> = Vec::new();
        'wait: loop {
            data.clear();
            for i in 0..n {
                match node.inbox(Some(&format!("blob:{xfer}:{i}:{n}")), 1).pop() {
                    Some(c) => data.extend(c.body),
                    None => {
                        if Instant::now() > end {
                            return Err("a piece of the file never arrived".into());
                        }
                        std::thread::sleep(Duration::from_millis(20));
                        continue 'wait;
                    }
                }
            }
            break;
        }
        if data.len() as f64 != m.f64("size").unwrap_or(-1.0)
            || sha256_hex(&data) != m.str("sha").unwrap_or("")
        {
            return Err(
                "the file arrived damaged (its SHA-256 does not match), so it was not kept".into(),
            );
        }
        let path = free_path(dir, &safe_name(m.str("name").unwrap_or("file")));
        std::fs::write(&path, &data)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        for i in 0..n {
            node.forget(&format!("blob:{xfer}:{i}:{n}"));
        }
        out.push(path);
    }
    Ok(out)
}

pub fn call(
    node: &Arc<Node>,
    who: &str,
    tool: &str,
    args: &J,
    out_dir: &std::path::Path,
    limit: Duration,
) -> Result<CallOut, String> {
    let started = Instant::now();
    if args.compact().len() > MAX_ARGS {
        return Err(format!("the arguments are over {} KiB", MAX_ARGS >> 10));
    }
    let fleet = store::load(&node.paths);
    let m = fleet
        .peers()
        .find(|m| m.name == who)
        .cloned()
        .ok_or_else(|| format!("{who} is not in the fleet, or is switched off"))?;
    let id = super::crypto::hex(&super::crypto::random(8));
    let body = J::Obj(vec![
        ("call".into(), J::s(&id)),
        ("tool".into(), J::s(tool)),
        ("args".into(), args.clone()),
    ]);
    node.send(&m, "call", body.compact().as_bytes())?;
    let r = node
        .wait_for("callres", "call", &id, limit)
        .ok_or_else(|| format!("{who} did not answer in time"))?;
    let j = parse(&String::from_utf8_lossy(&r.body)).map_err(|e| e.to_string())?;
    let metas: Vec<J> = j.arr("files").to_vec();
    let files = collect(node, &metas, out_dir, Duration::from_secs(60))?;
    let out = CallOut {
        ok: j.bool("ok") == Some(true),
        text: j.str("text").unwrap_or("").to_string(),
        files,
        ms: j.f64("ms").unwrap_or(0.0) as u64,
        node: m.name.clone(),
    };
    log_call(
        &node.paths,
        &format!(
            "sent to={who} tool={tool} args={}B ok={} ms={} total_ms={} files={}",
            args.compact().len(),
            out.ok,
            out.ms,
            started.elapsed().as_millis(),
            out.files.len()
        ),
    );
    Ok(out)
}

/// `POST /call` on this computer's messenger: the agent's door to the fleet's tools.
pub fn http_call(node: &Arc<Node>, body: &[u8]) -> (&'static str, String) {
    let Some(j) = std::str::from_utf8(body).ok().and_then(|t| parse(t).ok()) else {
        return ("400 Bad Request", "{\"error\":\"not JSON\"}".into());
    };
    let dir = j
        .str("out_dir")
        .map_or_else(|| node.paths.home.join("fleet").join("in"), PathBuf::from);
    let secs = j.f64("timeout_s").unwrap_or(900.0).clamp(5.0, 1800.0) as u64;
    match call(
        node,
        j.str("node").unwrap_or(""),
        j.str("tool").unwrap_or(""),
        j.get("args").unwrap_or(&J::Obj(vec![])),
        &dir,
        Duration::from_secs(secs),
    ) {
        Ok(o) => (
            "200 OK",
            J::Obj(vec![
                ("ok".into(), J::Bool(o.ok)),
                ("text".into(), J::s(&o.text)),
                (
                    "files".into(),
                    J::strings(
                        &o.files
                            .iter()
                            .map(|f| f.to_string_lossy().into_owned())
                            .collect::<Vec<_>>(),
                    ),
                ),
                ("node".into(), J::s(&o.node)),
                ("ms".into(), J::n(o.ms as f64)),
            ])
            .compact(),
        ),
        Err(e) => (
            "200 OK",
            J::Obj(vec![
                ("ok".into(), J::Bool(false)),
                ("text".into(), J::s(&e)),
            ])
            .compact(),
        ),
    }
}

/// The token the agent sends with `POST /call` (made once, readable by this user only).
pub fn local_token(p: &Paths) -> String {
    let f = p.file("fleet-local.token");
    if let Ok(t) = std::fs::read_to_string(&f) {
        if t.trim().len() == 64 {
            return t.trim().to_string();
        }
    }
    let t = super::crypto::hex(&super::crypto::random(32));
    let _ = super::crypto::write_private(&f, t.as_bytes());
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allow_list_is_exact_or_a_whole_server() {
        let a: Vec<String> = ["comfy_image", "mcp:notes/search", "mcp:files/*"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            allowed(&a, "comfy_image")
                && allowed(&a, "mcp:notes/search")
                && allowed(&a, "mcp:files/anything")
        );
        assert!(
            !allowed(&a, "mcp:notes/write")
                && !allowed(&a, "mcp:filesx/a")
                && !allowed(&a, "run_command")
        );
        assert!(
            !allowed(&[], "comfy_image"),
            "nothing is offered until the owner says so"
        );
        for good in ["comfy_image", "mcp:a/b", "mcp:a.b-c/*"] {
            assert!(allow_entry_ok(good), "{good}");
        }
        for bad in ["", "*", "mcp:a", "mcp:/x", "mcp:a/b c", "run_command"] {
            assert!(!allow_entry_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_remote_file_name_cannot_leave_the_folder() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\x\\y.png"), "y.png");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("a b.png"), "a b.png");
    }
}
