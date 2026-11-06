//! Small shared helpers: time (std only), char-safe slicing, secret redaction, path guard, HTTP.
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
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

/// 32 hex characters nobody can guess, from the system's per-process random keys (no RNG crate).
/// For ids that act as a one-time key, such as an approval id.
pub fn secret_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..2)
        .map(|_| {
            format!(
                "{:016x}",
                std::collections::hash_map::RandomState::new()
                    .build_hasher()
                    .finish()
            )
        })
        .collect()
}

/// 32 random bytes as 64 hex characters: the system's random source where there is one, else
/// the per-process random keys `secret_id` uses.
pub fn new_key() -> String {
    #[cfg(unix)]
    {
        let mut b = [0u8; 32];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut b))
            .is_ok()
        {
            return b.iter().map(|x| format!("{x:02x}")).collect();
        }
    }
    format!("{}{}", secret_id(), secret_id())
}

/// Equal without telling, by its timing, how much of a secret was right.
pub fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Write `<dir>/control.key` whole (a temporary file renamed into place), readable by its owner
/// only on Unix.
pub fn write_key(dir: &Path, key: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("control.key");
    let tmp = dir.join(format!("control.key.{}.tmp", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    {
        let mut f = o
            .open(&tmp)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::io::Write::write_all(&mut f, key.as_bytes()).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
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
    r#"(?i)((?:api[_-]?key|token|secret|password|passwd|authorization)['"]?\s*[:=]\s*['"]?(?:(?:bearer|basic|digest|token)\s+)?)[^\s'",;]{6,}"#
);
// `Bearer x` or `Basic x` anywhere, and a whole Cookie / Set-Cookie line.
rx!(
    secret_scheme_rx,
    r"(?i)\b(bearer|basic)\s+([A-Za-z0-9._~+/=-]{8,})"
);
rx!(
    secret_cookie_rx,
    r"(?i)\b((?:set-)?cookie\s*[:=]\s*)[^\r\n]+"
);
// Token shapes: OpenAI/Anthropic-style sk-, Stripe live/test/restricted keys, GitHub (all prefixes and
// fine-grained PATs), Slack, Hugging Face, Google API keys, AWS access key ids and JWTs.
rx!(
    secret_tok_rx,
    r"\b(sk-[A-Za-z0-9_-]{20,}|(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|hf_[A-Za-z0-9]{20,}|AIza[0-9A-Za-z_-]{30,}|AKIA[0-9A-Z]{16}\b|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})"
);
// A whole PEM private-key block (its BEGIN/END lines included).
rx!(
    secret_pem_rx,
    r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)"
);
// user:password@ in a URL.
rx!(
    secret_userinfo_rx,
    r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s:@]+:[^/\s@]+@"
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
    let a = secret_pem_rx().replace_all(s, "[REDACTED PRIVATE KEY]");
    let b = secret_userinfo_rx().replace_all(&a, "${1}[REDACTED]@");
    let c = secret_kv_rx().replace_all(&b, "${1}[REDACTED]");
    let d = secret_cookie_rx().replace_all(&c, "${1}[REDACTED]");
    // "Bearer abc123.def" or "Basic dXNlcjpw==", but not "basic introduction": a credential
    // has a digit, a token character or a capital inside it, or is very long
    let e = secret_scheme_rx().replace_all(&d, |c: &regex::Captures| {
        let tok = &c[2];
        if tok.chars().skip(1).any(|ch| ch.is_ascii_uppercase())
            || tok
                .chars()
                .any(|ch| ch.is_ascii_digit() || "._~+/=-".contains(ch))
            || tok.len() >= 24
        {
            format!("{} [REDACTED]", &c[1])
        } else {
            c[0].to_string()
        }
    });
    secret_tok_rx().replace_all(&e, "[REDACTED]").into_owned()
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
    /// The agent's own state (conversations, its config, its index and logs): never writable by
    /// a tool, even inside a write root. A guest flag or a trust list lives there.
    pub keep: Vec<PathBuf>,
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
            keep: vec![],
        }
    }
    pub fn keeping(mut self, paths: &[PathBuf]) -> Guard {
        self.keep = paths.iter().map(|p| resolve(p)).collect();
        self
    }
    /// The real place to write for `path`: inside a root, not the agent's own state, and not a link
    /// (a dangling link would let the write land wherever it points). Use the returned path for the
    /// write itself: `a/link/..` means one thing to this check and another to the system.
    pub fn writable(&self, path: &str) -> Option<PathBuf> {
        let p = resolve(Path::new(path));
        let inside = self.in_roots(&p.to_string_lossy());
        let kept = self.keep.iter().any(|k| p.starts_with(k));
        let link = p
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink());
        (inside && !kept && !link).then_some(p)
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
    // a machine that is off should be noticed in seconds, not after the OS gives up on the
    // connection (about two minutes), so the next engine or host can be tried
    let b = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(timeout.min(CONNECT_TIMEOUT)))
        .tls_config(tls());
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
    get_text_checked(url, timeout, limit, &|_| Ok(()))
}

/// How long a connection may take to open (a machine that is off fails this fast).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// TLS settings for every request: the certificates in `SSL_CERT_FILE`, when it names a PEM
/// bundle, replace the built-in list, as they do for curl, Python and OpenSSL (so a company's
/// own authority, or a TLS-inspecting proxy's, works); otherwise the built-in list.
pub fn tls() -> ureq::tls::TlsConfig {
    static ROOTS: std::sync::OnceLock<Option<ureq::tls::RootCerts>> = std::sync::OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        let pem = std::fs::read(std::env::var_os("SSL_CERT_FILE")?).ok()?;
        let certs: Vec<ureq::tls::Certificate<'static>> = ureq::tls::parse_pem(&pem)
            .filter_map(|i| match i {
                Ok(ureq::tls::PemItem::Certificate(c)) => Some(c),
                _ => None,
            })
            .collect();
        (!certs.is_empty()).then(|| ureq::tls::RootCerts::new_with_certs(&certs))
    });
    let b = ureq::tls::TlsConfig::builder();
    match roots {
        Some(r) => b.root_certs(r.clone()).build(),
        None => b.build(),
    }
}

/// Whether an address is on the open internet: not this computer, not a private or link-local
/// network (the cloud metadata address 169.254.169.254 is link-local), not carrier-grade NAT
/// (Tailscale), not multicast.
pub fn public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
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

/// A name resolver that refuses, at the moment of connecting, any address that is not on the
/// open internet. The addresses it checks are the ones the connection then uses, so a name that
/// answers with a public address for a first look and a private one for the connection (DNS
/// rebinding) gets nowhere. Hosts in `allow` (the owner's `fetch_allow_private`) pass, and so
/// does the proxy when one is set (through a proxy, the proxy looks names up).
#[derive(Debug)]
struct PublicOnly {
    allow: Vec<String>,
}

impl ureq::unversioned::resolver::Resolver for PublicOnly {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = ureq::unversioned::resolver::DefaultResolver::default()
            .resolve(uri, config, timeout)?;
        let host = uri
            .host()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']');
        let proxy = config
            .proxy()
            .is_some_and(|p| p.host().eq_ignore_ascii_case(host));
        if proxy || self.allow.iter().any(|a| a.eq_ignore_ascii_case(host)) {
            return Ok(addrs);
        }
        if addrs.iter().any(|a| !public_ip(a.ip())) {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refused: {host} is on this computer or a private network (fetch_url reaches the internet only; the owner can list a host in fetch_allow_private)"),
            )));
        }
        Ok(addrs)
    }
}

/// A page as fetched, with what a cache needs to ask again cheaply.
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub status: u16,
    pub text: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// `Cache-Control: max-age`, in seconds.
    pub max_age: Option<u64>,
    /// `Cache-Control: no-store`: the site asks not to keep a copy.
    pub no_store: bool,
}

/// Like `get_text_capped`, but `check` approves the address of the request and of every redirect
/// (up to 5) before it is made, so a public page cannot send the agent on to an internal one.
pub fn get_text_checked(
    url: &str,
    timeout: Duration,
    limit: u64,
    check: &dyn Fn(&str) -> Result<(), String>,
) -> Result<String, String> {
    get_checked(url, timeout, limit, check, None, &[]).map(|f| f.text)
}

/// `get_text_checked` with more: with `public_only`, every connection is refused unless the
/// address it goes to is on the internet (or its host is in the list); `headers` go on the first
/// request (`If-None-Match`, `If-Modified-Since`), and a 304 answer comes back as it is.
pub fn get_checked(
    url: &str,
    timeout: Duration,
    limit: u64,
    check: &dyn Fn(&str) -> Result<(), String>,
    public_only: Option<&[String]>,
    headers: &[(&str, String)],
) -> Result<Fetched, String> {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(timeout.min(CONNECT_TIMEOUT)))
        .tls_config(tls())
        .max_redirects(0)
        .build();
    let a: ureq::Agent = match public_only {
        Some(allow) => ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::new(),
            PublicOnly {
                allow: allow.to_vec(),
            },
        ),
        None => config.into(),
    };
    let mut cur = url.to_string();
    for hop in 0..=5 {
        check(&cur)?;
        let mut req = a.get(&cur).header("User-Agent", "PerennosTempo/1");
        if hop == 0 {
            for (k, v) in headers {
                req = req.header(*k, v.as_str());
            }
        }
        let resp = req.call().map_err(|e| {
            let s = e.to_string();
            // the resolver's refusal, without ureq's "io:" in front
            s.find("refused: ").map(|i| s[i..].to_string()).unwrap_or(s)
        })?;
        let status = resp.status();
        if status.is_redirection() && status.as_u16() != 304 {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without a location")?
                .to_string();
            cur = join_location(&cur, &loc);
            continue;
        }
        let head = |k: &str| {
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let (etag, last_modified) = (head("etag"), head("last-modified"));
        let cc = head("cache-control")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let max_age = cc
            .split(',')
            .find_map(|d| d.trim().strip_prefix("max-age=")?.parse().ok());
        let no_store = cc.contains("no-store");
        let code = status.as_u16();
        let text = if code == 304 {
            String::new()
        } else {
            body_text(resp, limit)
        };
        if code >= 400 {
            return Err(format!("HTTP {code}"));
        }
        return Ok(Fetched {
            status: code,
            text,
            etag,
            last_modified,
            max_age,
            no_store,
        });
    }
    Err("too many redirects".into())
}

/// A redirect's Location against the address it came from.
pub fn join_location(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(p) = loc.strip_prefix("//") {
        return format!("{scheme}://{p}");
    }
    if loc.starts_with('/') {
        return format!("{scheme}://{auth}{loc}");
    }
    let dir = rest[auth.len()..].rsplit_once('/').map_or("", |(d, _)| d);
    format!("{scheme}://{auth}{dir}/{loc}")
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
/// fnmatch-style: `*`, `?`, `[abc]`, `[a-z]`, `[!x]`. Each (pattern position, text position) is tried
/// once, so a pattern such as `*a*a*a*a*b` cannot take exponential time on a long name.
pub fn glob_match(pat: &str, s: &str) -> bool {
    fn go(
        p: &[char],
        s: &[char],
        pi: usize,
        si: usize,
        dead: &mut HashSet<(usize, usize)>,
    ) -> bool {
        if dead.contains(&(pi, si)) {
            return false;
        }
        let rest = &s[si.min(s.len())..];
        let ok = match p.get(pi) {
            None => rest.is_empty(),
            Some('*') => (si..=s.len()).any(|i| go(p, s, pi + 1, i, dead)),
            Some('?') => !rest.is_empty() && go(p, s, pi + 1, si + 1, dead),
            Some('[') => {
                let tail = &p[pi..];
                match tail.iter().skip(2).position(|c| *c == ']').map(|i| i + 2) {
                    None => !rest.is_empty() && rest[0] == '[' && go(p, s, pi + 1, si + 1, dead),
                    Some(_) if rest.is_empty() => false,
                    Some(end) => {
                        let (neg, set) = match tail[1] {
                            '!' | '^' => (true, &tail[2..end]),
                            _ => (false, &tail[1..end]),
                        };
                        let mut hit = false;
                        let mut i = 0;
                        while i < set.len() {
                            if i + 2 < set.len() && set[i + 1] == '-' {
                                hit |= set[i] <= rest[0] && rest[0] <= set[i + 2];
                                i += 3;
                            } else {
                                hit |= set[i] == rest[0];
                                i += 1;
                            }
                        }
                        hit != neg && go(p, s, pi + end + 1, si + 1, dead)
                    }
                }
            }
            Some(c) => !rest.is_empty() && rest[0] == *c && go(p, s, pi + 1, si + 1, dead),
        };
        if !ok {
            dead.insert((pi, si));
        }
        ok
    }
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    go(&p, &t, 0, 0, &mut HashSet::new())
}

fn segs_match(pat: &[&str], rel: &[&str]) -> bool {
    fn go(
        pat: &[&str],
        rel: &[&str],
        pi: usize,
        ri: usize,
        dead: &mut HashSet<(usize, usize)>,
    ) -> bool {
        if dead.contains(&(pi, ri)) {
            return false;
        }
        let ok = match pat.get(pi) {
            None => ri >= rel.len(),
            Some(&"**") => (ri..=rel.len()).any(|i| go(pat, rel, pi + 1, i, dead)),
            Some(p) => {
                ri < rel.len() && glob_match(p, rel[ri]) && go(pat, rel, pi + 1, ri + 1, dead)
            }
        };
        if !ok {
            dead.insert((pi, ri));
        }
        ok
    }
    go(pat, rel, 0, 0, &mut HashSet::new())
}

pub const SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", "__pycache__"];

/// All files under `dir` (not following symlinked dirs), skipping SKIP_DIRS.
pub fn walk_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    // a model can point grep or glob at a whole drive: stop at 200 000 files rather than walk for hours
    if depth > 40 || out.len() >= 200_000 {
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

    #[derive(Clone, Debug)]
    pub struct Req {
        pub method: String,
        pub path: String,
        pub body: String,
        /// Request headers, names in lower case.
        pub headers: Vec<(String, String)>,
    }

    impl Req {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }

    type FullFn = Arc<dyn Fn(&Req) -> (u16, Vec<(String, String)>, String) + Send + Sync>;

    /// In-process HTTP mock on 127.0.0.1:0. Records every request.
    pub struct Mock {
        pub url: String,
        pub reqs: Arc<Mutex<Vec<Req>>>,
    }

    impl Mock {
        pub fn start(h: impl Fn(&Req) -> (u16, String) + Send + Sync + 'static) -> Mock {
            Mock::start_with_headers(move |r| {
                let (code, body) = h(r);
                (code, vec![], body)
            })
        }

        /// A mock whose handler also sets response headers.
        pub fn start_with_headers(
            h: impl Fn(&Req) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
        ) -> Mock {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            let reqs = Arc::new(Mutex::new(Vec::new()));
            let (r2, h): (_, FullFn) = (reqs.clone(), Arc::new(h));
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

    fn serve(s: std::net::TcpStream, reqs: Arc<Mutex<Vec<Req>>>, h: FullFn) {
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
        let mut headers = vec![];
        loop {
            let mut hl = String::new();
            if rd.read_line(&mut hl).unwrap_or(0) == 0 || hl.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = hl.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap_or(0);
                }
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let mut body = vec![0u8; len];
        let _ = rd.read_exact(&mut body);
        let req = Req {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
            headers,
        };
        reqs.lock().unwrap().push(req.clone());
        let (code, extra, out) = h(&req);
        let extra: String = extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let resp = format!(
            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{out}",
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
        // More token shapes, all built at run time for the same reason.
        let a = "abcdefghijKLMNOPQRSTuvwxyz0123456789";
        let tokens = [
            format!("sk_live_{a}"),
            format!("rk_test_{a}"),
            format!("gho_{a}"),
            format!("ghs_{a}"),
            format!("github_pat_11{a}"),
            format!("xoxb-1234-{a}"),
            format!("hf_{a}"),
            format!("AKIA{}", "ABCDEFGHIJKLMNOP"),
            format!("eyJ{a}.eyJ{a}.{a}"),
        ];
        for t in &tokens {
            let red = redact(&format!("value {t} end"));
            assert_eq!(red, "value [REDACTED] end", "{t}");
        }
        let pem = format!("before\n-----BEGIN {}PRIVATE KEY-----\nMIIEv{a}\n{a}==\n-----END {}PRIVATE KEY-----\nafter", "RSA ", "RSA ");
        assert_eq!(redact(&pem), "before\n[REDACTED PRIVATE KEY]\nafter");
        let cut = format!("-----BEGIN {}PRIVATE KEY-----\nMIIEv{a}", "");
        assert_eq!(
            redact(&cut),
            "[REDACTED PRIVATE KEY]",
            "a block cut off by a cap is still hidden"
        );
        assert_eq!(
            redact("clone https://alex:hunter2pass@git.example.com/repo.git now"),
            "clone https://[REDACTED]@git.example.com/repo.git now"
        );
        // Ordinary text is left alone.
        for plain in [
            "see https://example.com/path?q=1",
            "akia is a word",
            "the sk_ prefix alone",
            "eyJ alone",
        ] {
            assert_eq!(redact(plain), plain);
        }
        let long = "a".repeat(7000);
        let c = cap(&long, RESULT_CAP);
        assert!(c.contains("[truncated 1000 chars]"));
    }

    #[test]
    fn plain_words_after_basic_or_bearer_are_kept() {
        for prose in [
            "A basic introduction to Rust.",
            "The bearer bond, basic configuration, Basic Settings",
            "Basic authentication sends a username and a password.",
        ] {
            assert_eq!(redact(prose), prose);
        }
        assert!(
            !redact("curl -H 'Authorization: Bearer abcdefghijkl12'").contains("abcdefghijkl12")
        );
        assert!(!redact("use Basic dXNlcjpwYXNz to log in").contains("dXNlcjpwYXNz"));
    }

    #[test]
    fn a_connection_goes_only_to_an_address_checked_when_it_is_made() {
        // the check before (`check`) lets everything through here; the resolver alone refuses
        let m = testutil::Mock::start(|_| (200, "inside".into()));
        let port = m.url.rsplit(':').next().unwrap().to_string();
        let local = format!("http://localhost:{port}/x");
        let e = get_checked(
            &local,
            Duration::from_secs(5),
            1000,
            &|_| Ok(()),
            Some(&[]),
            &[],
        )
        .unwrap_err();
        assert!(
            e.starts_with("refused: localhost is on this computer"),
            "{e}"
        );
        // a host the owner lists passes, and without pinning nothing is checked
        let ok = get_checked(
            &local,
            Duration::from_secs(5),
            1000,
            &|_| Ok(()),
            Some(&["localhost".to_string()]),
            &[],
        )
        .unwrap();
        assert_eq!((ok.status, ok.text.as_str()), (200, "inside"));
        assert_eq!(
            get_text_checked(&local, Duration::from_secs(5), 1000, &|_| Ok(())).unwrap(),
            "inside"
        );
        assert!(public_ip("1.1.1.1".parse().unwrap()) && !public_ip("::1".parse().unwrap()));
    }

    #[test]
    fn authorization_headers_and_cookies_are_redacted_whole() {
        let r = redact("Authorization: Bearer abcDEF123456.ghi\nauthorization=Basic dXNlcjpwYXNzd29yZA==\nCookie: sid=Abc123Def; theme=dark\nSet-Cookie: sid=Zzz999Yyy; Path=/\nProxy-Authorization: Bearer qqq111www222");
        for leak in [
            "abcDEF123456",
            "dXNlcjpwYXNzd29yZA",
            "Abc123Def",
            "Zzz999Yyy",
            "qqq111www222",
        ] {
            assert!(!r.contains(leak), "{leak} leaked: {r}");
        }
        assert!(r.contains("Bearer") || r.contains("[REDACTED]"));
    }

    #[test]
    fn redirect_locations_are_joined_to_where_they_came_from() {
        assert_eq!(join_location("http://h/a/c", "b?x=1"), "http://h/a/b?x=1");
        assert_eq!(join_location("http://h:8/a/c", "/z"), "http://h:8/z");
        assert_eq!(join_location("https://h/a", "//other/p"), "https://other/p");
        assert_eq!(
            join_location("http://h/a", "http://169.254.169.254/"),
            "http://169.254.169.254/"
        );
        assert_eq!(join_location("http://h", "x"), "http://h/x");
    }

    #[test]
    fn secret_ids_are_long_and_never_repeat() {
        let ids: std::collections::HashSet<String> = (0..2000).map(|_| secret_id()).collect();
        assert_eq!(ids.len(), 2000);
        assert!(ids
            .iter()
            .all(|i| i.len() == 32 && i.chars().all(|c| c.is_ascii_hexdigit())));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0.0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(1_793_923_200.0 + 3661.0), (2026, 11, 6, 1, 1, 1));
    }

    #[test]
    fn hostile_patterns_finish_quickly() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let name = "a".repeat(80);
            let r1 = glob_match("*a*a*a*a*a*a*a*a*a*a*a*a*b", &name);
            let segs: Vec<String> = (0..40).map(|_| "d".to_string()).collect();
            let rel: Vec<&str> = segs.iter().map(String::as_str).collect();
            let r2 = segs_match(&["**", "**", "**", "**", "**", "**", "**", "x"], &rel);
            let _ = tx.send((r1, r2));
        });
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert_eq!(got, Ok((false, false)), "glob_match took exponential time");
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
