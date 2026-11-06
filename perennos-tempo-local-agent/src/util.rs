//! Small shared helpers: time (std only), char-safe slicing, secret redaction, path guard, HTTP.
use regex::Regex;
use serde_json::Value;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const RESULT_CAP: usize = 6000;

pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (UTC).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day, hour, minute, second) in UTC.
pub fn civil(secs: f64) -> (i64, u32, u32, u32, u32, u32) {
    let s = secs.max(0.0) as i64;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    (
        y,
        m,
        d,
        (t / 3600) as u32,
        (t % 3600 / 60) as u32,
        (t % 60) as u32,
    )
}

pub fn now_str() -> String {
    let (y, mo, d, h, mi, s) = civil(unix_now());
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

pub fn date_ymd() -> String {
    let (y, mo, d, ..) = civil(unix_now());
    format!("{y:04}{mo:02}{d:02}")
}

pub fn time_hms() -> String {
    let (.., h, mi, s) = civil(unix_now());
    format!("{h:02}{mi:02}{s:02}")
}

pub fn today_long() -> String {
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, mo, d, ..) = civil(unix_now());
    format!("{d:02} {} {y}", M[(mo - 1) as usize])
}

/// A short unique-ish hex suffix without a RNG crate.
pub fn short_id() -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:04x}", (n ^ std::process::id().rotate_left(7)) & 0xffff)
}

pub fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn tail_chars(s: &str, n: usize) -> String {
    let c = s.chars().count();
    s.chars().skip(c.saturating_sub(n)).collect()
}

macro_rules! rx {
    ($name:ident, $pat:expr) => {
        pub fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

rx!(
    secret_path_rx,
    r"(?i)(\.claude\.json|\.token$|\.key$|\.env|secret|credential|executor-token|password|id_rsa|\.pem$)"
);
rx!(
    secret_kv_rx,
    r#"(?i)((?:api[_-]?key|token|secret|password|passwd|authorization)['"]?\s*[:=]\s*['"]?)[^\s'",;]{6,}"#
);
rx!(
    secret_tok_rx,
    r"\b(sk-[A-Za-z0-9_-]{20,}|ghp_[A-Za-z0-9]{20,}|AIza[0-9A-Za-z_-]{30,})"
);
rx!(
    delete_rx,
    r"(?i)(^|[\s;|&(])(rm|del|erase|rmdir|rd|Remove-Item|ri|shred|unlink)(\s|$)"
);
rx!(
    comfy_rx,
    r#"(?i)comfy(\.exe)?"?\s+(launch|start|stop|run)|ComfyUI[\\/]main\.py"#
);
rx!(protect_verb_rx, r"(?i)stop|kill|rm|unload|delete|restart");
rx!(
    confirm_rx,
    r"(?i)(>|\bSet-Content|\bOut-File|\bAdd-Content|\bNew-Item|\bMove-Item|\bCopy-Item|\bmv\b|\bcp\b|\bmkdir\b|\bInvoke-(WebRequest|RestMethod|Expression)|\bcurl\b|\bwget\b|\bpip\b|\bnpm\b|\bwinget\b|\bStart-Process|\bStop-Process|\btaskkill|\bshutdown|\bssh\b|\bscp\b|\bdocker\b|\bsystemctl\b|\bgit\s+(reset|clean|push|checkout)|\bollama\s+(rm|stop|pull|create)|\breg\s+(add|delete)|\bformat\b|\bSet-ExecutionPolicy|\bnetsh\b|\bsc(\.exe)?\s+(stop|delete))"
);

pub fn redact(s: &str) -> String {
    let a = secret_kv_rx().replace_all(s, "${1}[REDACTED]");
    secret_tok_rx().replace_all(&a, "[REDACTED]").into_owned()
}

/// Redact, then cap at `n` chars with a truncation note.
pub fn cap(s: &str, n: usize) -> String {
    let s = redact(s);
    let len = s.chars().count();
    if len <= n {
        s
    } else {
        format!("{}\n...[truncated {} chars]", head_chars(&s, n), len - n)
    }
}

pub fn is_secret_path(p: &str) -> bool {
    secret_path_rx().is_match(p)
}

/// Absolute, lexically normalised, with symlinks resolved on the deepest existing ancestor.
pub fn resolve(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut norm = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut base = norm.clone();
    loop {
        if let Ok(c) = base.canonicalize() {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (
            base.file_name().map(|n| n.to_os_string()),
            base.parent().map(|p| p.to_path_buf()),
        ) {
            (Some(n), Some(p)) => {
                tail.push(n);
                base = p;
            }
            _ => return norm,
        }
    }
}

/// Write-root guard shared by write_file, trash, edit_file.
#[derive(Clone, Debug, Default)]
pub struct Guard {
    pub roots: Vec<PathBuf>,
}

fn same_case(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

impl Guard {
    pub fn new(roots: &[String]) -> Guard {
        Guard {
            roots: roots.iter().map(|r| resolve(Path::new(r))).collect(),
        }
    }
    pub fn in_roots(&self, path: &str) -> bool {
        let p = resolve(Path::new(path));
        self.roots.iter().any(|r| {
            p == *r
                || same_case(&p).starts_with(&format!(
                    "{}{}",
                    same_case(r),
                    std::path::MAIN_SEPARATOR
                ))
        }) || self.roots.iter().any(|r| p.starts_with(r))
    }
    pub fn is_root(&self, path: &str) -> bool {
        let p = resolve(Path::new(path));
        self.roots.contains(&p)
    }
}

// ---------------------------------------------------------------- HTTP
pub fn agent(timeout: Duration, env_proxy: bool) -> ureq::Agent {
    let b = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout));
    let b = if env_proxy { b } else { b.proxy(None) };
    b.build().into()
}

fn body_text(mut resp: ureq::http::Response<ureq::Body>, limit: u64) -> String {
    let mut buf = Vec::new();
    let _ = resp
        .body_mut()
        .as_reader()
        .take(limit)
        .read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

pub fn http_json(url: &str, body: Option<&Value>, timeout: Duration) -> Result<Value, String> {
    let a = agent(timeout, false);
    let resp = match body {
        None => a.get(url).call(),
        Some(b) => a
            .post(url)
            .header("Content-Type", "application/json")
            .send(b.to_string()),
    }
    .map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = body_text(resp, 64 * 1024 * 1024);
    if status >= 400 {
        return Err(format!("HTTP {status}: {}", head_chars(&text, 160)));
    }
    serde_json::from_str(&text).map_err(|e| format!("bad JSON from {url}: {e}"))
}

pub fn get_text_capped(url: &str, timeout: Duration, limit: u64) -> Result<String, String> {
    let a = agent(timeout, true);
    let resp = a
        .get(url)
        .header("User-Agent", "PerennosTempo/1")
        .call()
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let text = body_text(resp, limit);
    if status >= 400 {
        return Err(format!("HTTP {status}"));
    }
    Ok(text)
}

/// POST a JSON body and return the response for streaming, or an error with the HTTP status text.
pub fn post_stream(
    url: &str,
    body: &Value,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let a = agent(timeout, false);
    let resp = a
        .post(url)
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("{e}"))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(format!(
            "HTTP {status}: {}",
            head_chars(&body_text(resp, 400), 160)
        ));
    }
    Ok(resp)
}

// ---------------------------------------------------------------- glob
/// fnmatch-style: `*`, `?`, `[abc]`, `[a-z]`, `[!x]`.
pub fn glob_match(pat: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|i| go(&p[1..], &s[i..])),
            Some('?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some('[') => {
                let Some(end) = p.iter().skip(2).position(|c| *c == ']').map(|i| i + 2) else {
                    return !s.is_empty() && s[0] == '[' && go(&p[1..], &s[1..]);
                };
                if s.is_empty() {
                    return false;
                }
                let (neg, set) = match p[1] {
                    '!' | '^' => (true, &p[2..end]),
                    _ => (false, &p[1..end]),
                };
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        hit |= set[i] <= s[0] && s[0] <= set[i + 2];
                        i += 3;
                    } else {
                        hit |= set[i] == s[0];
                        i += 1;
                    }
                }
                hit != neg && go(&p[end + 1..], &s[1..])
            }
            Some(c) => !s.is_empty() && s[0] == *c && go(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    go(&p, &t)
}

fn segs_match(pat: &[&str], rel: &[&str]) -> bool {
    match pat.first() {
        None => rel.is_empty(),
        Some(&"**") => (0..=rel.len()).any(|i| segs_match(&pat[1..], &rel[i..])),
        Some(p) => !rel.is_empty() && glob_match(p, rel[0]) && segs_match(&pat[1..], &rel[1..]),
    }
}

pub const SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", "__pycache__"];

/// All files under `dir` (not following symlinked dirs), skipping SKIP_DIRS.
pub fn walk_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 40 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            if !SKIP_DIRS.iter().any(|s| e.file_name() == *s) {
                walk_files(&p, out, depth + 1);
            }
        } else if ft.is_file() || ft.is_symlink() {
            out.push(p);
        }
    }
}

/// Files below `base` whose relative path matches `pattern` (`**` allowed). Newest first.
pub fn glob_in(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let pat = pattern.replace('\\', "/");
    let psegs: Vec<&str> = pat
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut all = Vec::new();
    if psegs.len() == 1 && psegs[0] != "**" {
        if let Ok(rd) = std::fs::read_dir(base) {
            all = rd.flatten().map(|e| e.path()).collect();
        }
    } else {
        walk_files(base, &mut all, 0);
    }
    let mut found: Vec<PathBuf> = all
        .into_iter()
        .filter(|p| p.is_file())
        .filter(|p| {
            let rel = p.strip_prefix(base).unwrap_or(p);
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let refs: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
            !refs.iter().any(|s| SKIP_DIRS.contains(s)) && segs_match(&psegs, &refs)
        })
        .collect();
    sort_newest(&mut found);
    found
}

pub fn mtime_of(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn sort_newest(v: &mut [PathBuf]) {
    v.sort_by(|a, b| {
        mtime_of(b)
            .partial_cmp(&mtime_of(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Expand an absolute glob such as `/data/notes/*.md` (the part before the first wildcard segment is the base).
pub fn glob_files(full: &str) -> Vec<PathBuf> {
    let norm = full.replace('\\', "/");
    let segs: Vec<&str> = norm.split('/').collect();
    let first = segs
        .iter()
        .position(|s| s.contains(['*', '?', '[']))
        .unwrap_or(segs.len());
    if first == segs.len() {
        let p = PathBuf::from(full);
        return if p.is_file() { vec![p] } else { vec![] };
    }
    let mut base = segs[..first].join("/");
    if base.is_empty() && norm.starts_with('/') {
        base = "/".into();
    }
    if base.is_empty() {
        base = ".".into();
    }
    glob_in(Path::new(&base), &segs[first..].join("/"))
}

#[cfg(test)]
pub mod testutil {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    type HandlerFn = Arc<dyn Fn(&Req) -> (u16, String) + Send + Sync>;

    #[derive(Clone, Debug)]
    pub struct Req {
        pub method: String,
        pub path: String,
        pub body: String,
    }

    /// In-process HTTP mock on 127.0.0.1:0. Records every request.
    pub struct Mock {
        pub url: String,
        pub reqs: Arc<Mutex<Vec<Req>>>,
    }

    impl Mock {
        pub fn start(h: impl Fn(&Req) -> (u16, String) + Send + Sync + 'static) -> Mock {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            let reqs = Arc::new(Mutex::new(Vec::new()));
            let (r2, h): (_, HandlerFn) = (reqs.clone(), Arc::new(h));
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    let (r2, h) = (r2.clone(), h.clone());
                    std::thread::spawn(move || serve(s, r2, h));
                }
            });
            Mock { url, reqs }
        }
        pub fn bodies(&self, path: &str) -> Vec<serde_json::Value> {
            self.reqs
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.path == path)
                .map(|r| serde_json::from_str(&r.body).unwrap_or(serde_json::Value::Null))
                .collect()
        }
    }

    fn serve(s: std::net::TcpStream, reqs: Arc<Mutex<Vec<Req>>>, h: HandlerFn) {
        let mut rd = BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        if rd.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut it = line.split_whitespace();
        let (method, path) = (
            it.next().unwrap_or("").to_string(),
            it.next().unwrap_or("").to_string(),
        );
        let mut len = 0usize;
        loop {
            let mut hl = String::new();
            if rd.read_line(&mut hl).unwrap_or(0) == 0 || hl.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = hl.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; len];
        let _ = rd.read_exact(&mut body);
        let req = Req {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
        };
        reqs.lock().unwrap().push(req.clone());
        let (code, out) = h(&req);
        let resp = format!(
            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
            out.len()
        );
        let mut w = s;
        let _ = w.write_all(resp.as_bytes());
        let _ = w.flush();
    }

    /// Mock handler for /api/chat that pops scripted responses (status, body) in order.
    /// Other paths answer 404. After the script runs out it answers 500.
    pub fn chat_script(
        script: Vec<(u16, String)>,
    ) -> impl Fn(&Req) -> (u16, String) + Send + Sync + 'static {
        let q = Mutex::new(std::collections::VecDeque::from(script));
        move |r: &Req| {
            if r.path == "/api/chat" {
                q.lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, "script exhausted".into()))
            } else {
                (404, "{}".into())
            }
        }
    }

    pub fn ndjson(lines: &[serde_json::Value]) -> String {
        lines.iter().map(|l| l.to_string() + "\n").collect()
    }

    pub fn text_resp(text: &str) -> (u16, String) {
        (
            200,
            ndjson(&[
                serde_json::json!({"message": {"role": "assistant", "content": text}, "done": false}),
                serde_json::json!({"message": {"role": "assistant", "content": ""}, "done": true, "prompt_eval_count": 12, "eval_count": 3}),
            ]),
        )
    }

    pub fn call_resp(name: &str, args: serde_json::Value) -> (u16, String) {
        (
            200,
            ndjson(&[
                serde_json::json!({"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": name, "arguments": args}}]}, "done": false}),
                serde_json::json!({"done": true}),
            ]),
        )
    }

    pub fn err_resp() -> (u16, String) {
        (
            200,
            ndjson(&[serde_json::json!({"error": "CUDA error: illegal memory access"})]),
        )
    }

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A fresh scratch dir (never removed; the OS temp dir is cleaned by the OS).
    pub fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tla-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_and_caps() {
        assert_eq!(redact("api_key = abcdef123456"), "api_key = [REDACTED]");
        // Built at run time so the repository's leak scan never sees a token-shaped literal.
        let token = format!("sk-{}1", "abcdefghijklmnopqrstuvwxyz");
        let red = redact(&format!("x {token} y"));
        assert!(red.contains("[REDACTED]") && !red.contains(&token));
        let long = "a".repeat(7000);
        let c = cap(&long, RESULT_CAP);
        assert!(c.contains("[truncated 1000 chars]"));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0.0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(1_793_923_200.0 + 3661.0), (2026, 11, 6, 1, 1, 1));
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*.py", "a.py"));
        assert!(!glob_match("*.py", "a.pyc"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("[a-c]x", "bx"));
        assert!(segs_match(&["**", "*.rs"], &["a", "b", "c.rs"]));
        assert!(segs_match(&["**", "*.rs"], &["c.rs"]));
    }
}
