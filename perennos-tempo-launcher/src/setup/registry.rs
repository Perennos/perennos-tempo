//! Live checks against the official Ollama registry. Tempo ships a list of candidate model names
//! (`catalog/candidates.json`) but never their sizes or licences: those come from the registry's
//! manifest and licence layer, fetched with `curl` on the user's computer (std has no TLS), and
//! cached for 7 days in `<home>/models/registry-cache.json`. A candidate that does not verify is
//! skipped, so a wrong or missing tag is harmless. Without curl, or offline, setup still plans
//! from the installed models and the verified entries that ship with Tempo, and says so.

use super::catalog::{Model, Proof, Source, HEADROOM_GB};
use super::json::{parse, J};
use super::probe::{run_with_timeout, Host, Installed};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const CANDIDATES: &str = include_str!("../../catalog/candidates.json");
pub const REGISTRY: &str = "https://registry.ollama.ai";
pub const CACHE_DAYS: u64 = 7;
/// All lookups together stop after this long; what did not answer is skipped.
pub const TOTAL_SECS: u64 = 20;
const WORKERS: usize = 8;
const DAY: u64 = 86_400;
const GB: f64 = 1e9;
const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MODEL_LAYER: &str = "application/vnd.ollama.image.model";
const LICENCE_LAYER: &str = "application/vnd.ollama.image.license";

// ---- the candidate list

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub roles: Vec<String>,
    pub params_b: Option<f64>,
    pub active_params_b: Option<f64>,
    pub about: String,
}

pub fn parse_candidates(text: &str) -> Result<Vec<Candidate>, String> {
    let j = parse(text)?;
    j.arr("candidates")
        .iter()
        .map(|c| {
            let name = c
                .str("name")
                .filter(|n| split_name(n).is_some())
                .ok_or("a candidate has no valid family:tag name")?;
            Ok(Candidate {
                name: name.to_string(),
                roles: c.strs("roles"),
                params_b: c.f64("params_b").filter(|p| *p > 0.0),
                active_params_b: c.f64("active_params_b").filter(|p| *p > 0.0),
                about: c.str("about").unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// `family:tag` into its two parts (tag defaults to `latest`); None when it holds anything but
/// letters, digits, `.`, `_` and `-`, so a name can never bend the registry address.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let (fam, tag) = name.split_once(':').unwrap_or((name, "latest"));
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    (ok(fam) && ok(tag)).then_some((fam, tag))
}

pub fn model_page(family: &str) -> String {
    format!("https://ollama.com/library/{family}")
}

// ---- manifest and licence parsing (pure)

#[derive(Debug, PartialEq)]
pub struct Manifest {
    /// The sum of every layer (model, projector, template, ...).
    pub bytes: u64,
    pub licence_digest: Option<String>,
}

fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let j = parse(text)?;
    let layers = j.arr("layers");
    if !layers
        .iter()
        .any(|l| l.str("mediaType") == Some(MODEL_LAYER))
    {
        return Err("the manifest has no model layer".into());
    }
    let mut bytes = 0.0;
    for l in layers {
        bytes += l
            .f64("size")
            .filter(|s| *s >= 0.0)
            .ok_or("a layer has no size")?;
    }
    if bytes < 1.0 {
        return Err("the manifest has no size".into());
    }
    let licence_digest = layers
        .iter()
        .find(|l| l.str("mediaType") == Some(LICENCE_LAYER))
        .and_then(|l| l.str("digest"))
        .filter(|d| valid_digest(d))
        .map(String::from);
    Ok(Manifest {
        bytes: bytes as u64,
        licence_digest,
    })
}

/// A short licence name from the first lines of a licence text; None when it is not recognised.
pub fn detect_licence(text: &str) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(12)
        .collect();
    let head = lines.join(" ");
    let low = head.to_ascii_lowercase();
    let first = lines.first()?.to_ascii_lowercase();
    if low.contains("apache license") {
        let v = if low.contains("version 2.0") {
            " 2.0"
        } else {
            ""
        };
        return Some(format!("Apache License{v}"));
    }
    if first.starts_with("mit license")
        || first == "mit"
        || low.contains("permission is hereby granted, free of charge")
    {
        return Some("MIT".into());
    }
    if low.contains("gemma terms of use") || (low.contains("gemma") && low.contains("terms of use"))
    {
        return Some("Gemma Terms of Use".into());
    }
    if let Some(i) = low.find("community license") {
        if let Some(j) = low[..i].rfind("llama") {
            let version = low[j + 5..i].split_whitespace().next().unwrap_or("");
            let version: String = version
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            return Some(if version.is_empty() {
                "Llama Community License".into()
            } else {
                format!("Llama {} Community License", version.trim_matches('.'))
            });
        }
    }
    if low.contains("qwen research license") {
        return Some("Qwen Research License".into());
    }
    if low.contains("qwen license") || low.contains("tongyi qianwen") {
        return Some("Qwen License".into());
    }
    if low.contains("deepseek license") {
        return Some("DeepSeek License".into());
    }
    if low.contains("mistral ai research license") || low.contains("mistral research license") {
        return Some("Mistral Research License".into());
    }
    if low.contains("gnu general public license") {
        return Some("GNU General Public License".into());
    }
    if low.contains("creative commons") {
        return Some("Creative Commons".into());
    }
    if low.contains("bsd") && low.contains("redistribution and use") {
        return Some("BSD".into());
    }
    let f = lines.first()?;
    let fl = f.to_ascii_lowercase();
    (f.len() <= 60 && (fl.contains("licen") || fl.contains("terms of")))
        .then(|| f.trim_end_matches(['.', ':']).to_string())
}

// ---- fetching

#[derive(Clone, Debug, PartialEq)]
pub enum Fetched {
    Body(String),
    /// The registry answered 404: the name does not exist.
    NotFound,
    /// No answer, a refusal, a timeout, an oversized answer: nothing is learned.
    Failed,
}

/// One HTTPS GET. `follow`: follow redirects (https only), needed for licence blobs, which the
/// registry hands to its storage host; manifests never follow one.
/// What one request to an address answered, when it is not followed anywhere.
#[derive(Clone, Debug, PartialEq)]
pub enum Probe {
    /// 200: the file is here, at this address.
    Here,
    /// 301, 302, 303, 307 or 308, with where it points (an absolute address).
    Moved(String),
    /// 401 or 403: a login or a licence is wanted.
    Denied(u16),
    NotFound,
    Other(u16),
}

pub trait Fetch: Send + Sync {
    fn get(&self, url: &str, accept: Option<&str>, follow: bool) -> Fetched;

    /// One request (a HEAD) that never follows a redirect, with a bearer token in the
    /// `Authorization` header when one is given. For callers that must decide, hop by hop, who
    /// gets to see a token.
    fn probe(&self, _url: &str, _bearer: Option<&str>) -> Result<Probe, String> {
        Err("this fetcher cannot look at one address at a time".into())
    }

    /// `download`, but at exactly this address (a redirect is an error, never followed) and
    /// with a bearer token when one is given.
    fn download_hop(
        &self,
        _url: &str,
        _part: &Path,
        _max_secs: u64,
        _progress: &mut dyn FnMut(u64),
        _bearer: Option<&str>,
    ) -> Result<(), String> {
        Err("this fetcher cannot fetch from one address at a time".into())
    }

    /// Download a big file (an https address) into `part`, carrying on from what is already in
    /// it when the server allows. `max_secs` is the most it may take. `progress` is told, about
    /// twice a second, how many bytes `part` holds. Most callers only need the lookups above, so
    /// a fetcher that cannot download says so.
    fn download(
        &self,
        _url: &str,
        _part: &Path,
        _max_secs: u64,
        _progress: &mut dyn FnMut(u64),
    ) -> Result<(), String> {
        Err("this fetcher cannot download files".into())
    }
}

pub struct Curl {
    pub program: String,
}

impl Fetch for Curl {
    fn get(&self, url: &str, accept: Option<&str>, follow: bool) -> Fetched {
        if !url.starts_with("https://") {
            return Fetched::Failed;
        }
        let mut args: Vec<&str> = vec![
            "-sS",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            "8",
            "--max-filesize",
            "1000000",
            "-w",
            "\n%{http_code}",
        ];
        if follow {
            args.extend(["-L", "--max-redirs", "3"]);
        }
        if let Some(a) = accept {
            args.extend(["-H", a]);
        }
        args.push(url);
        // the output is capped at 1 MiB by run_with_timeout; the limit here is curl's 8 s plus slack
        let Some(out) = run_with_timeout(&self.program, &args, Duration::from_secs(11)) else {
            return Fetched::Failed;
        };
        let Some((body, code)) = out.rsplit_once('\n') else {
            return Fetched::Failed;
        };
        match code.trim() {
            "200" => Fetched::Body(body.to_string()),
            "404" => Fetched::NotFound,
            _ => Fetched::Failed,
        }
    }

    fn download(
        &self,
        url: &str,
        part: &Path,
        max_secs: u64,
        progress: &mut dyn FnMut(u64),
    ) -> Result<(), String> {
        if !url.starts_with("https://") {
            return Err("only https addresses are fetched".into());
        }
        self.run_download(
            curl_download_args(max_secs, part, url),
            None,
            part,
            progress,
        )
    }

    fn probe(&self, url: &str, bearer: Option<&str>) -> Result<Probe, String> {
        if !url.starts_with("https://") {
            return Err("only https addresses are fetched".into());
        }
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut args: Vec<String> = [
            "-sS",
            "-I",
            "--proto",
            "=https",
            "--max-redirs",
            "0",
            "--connect-timeout",
            "30",
            "--max-time",
            "60",
            "-o",
            null,
            "-w",
            "%{http_code} %{redirect_url}",
        ]
        .iter()
        .map(|a| a.to_string())
        .collect();
        if bearer.is_some() {
            args.extend(["-K".into(), "-".into()]);
        }
        args.push(url.to_string());
        let out = self.run_capture(&args, bearer)?;
        let (code, to) = out.trim().split_once(' ').unwrap_or((out.trim(), ""));
        Ok(match code.parse::<u16>().unwrap_or(0) {
            200 | 206 => Probe::Here,
            301 | 302 | 303 | 307 | 308 if to.starts_with("https://") => Probe::Moved(to.into()),
            301 | 302 | 303 | 307 | 308 => {
                return Err(
                    "the address moved somewhere that is not https, so it was not followed".into(),
                )
            }
            c @ (401 | 403) => Probe::Denied(c),
            404 => Probe::NotFound,
            c => Probe::Other(c),
        })
    }

    fn download_hop(
        &self,
        url: &str,
        part: &Path,
        max_secs: u64,
        progress: &mut dyn FnMut(u64),
        bearer: Option<&str>,
    ) -> Result<(), String> {
        if !url.starts_with("https://") {
            return Err("only https addresses are fetched".into());
        }
        // the same as `download`, without `--location`: a redirect is an error here
        let mut args: Vec<String> = curl_download_args(max_secs, part, url)
            .into_iter()
            .filter(|a| a != "--location")
            .collect();
        let at = args.iter().position(|a| a == "--max-redirs").unwrap_or(0);
        if args.get(at + 1).is_some() {
            args[at + 1] = "0".into();
        }
        if bearer.is_some() {
            // the token goes in on stdin as a config line: never on a command line, where any
            // program on this computer could read it
            args.insert(0, "-".into());
            args.insert(0, "-K".into());
        }
        self.run_download(args, bearer, part, progress)
    }
}

impl Curl {
    /// curl, with `bearer` (if any) given as a config line on stdin; what it printed.
    fn run_capture(&self, args: &[String], bearer: Option<&str>) -> Result<String, String> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(args)
            .stdin(if bearer.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start curl: {e}"))?;
        if let (Some(t), Some(mut si)) = (bearer, child.stdin.take()) {
            let _ = std::io::Write::write_all(&mut si, bearer_config(t).as_bytes());
        }
        let o = child
            .wait_with_output()
            .map_err(|e| format!("could not wait for curl: {e}"))?;
        if !o.status.success() {
            let said = String::from_utf8_lossy(&o.stderr);
            return Err(format!(
                "curl stopped ({}): {}",
                o.status,
                said.lines().next().unwrap_or("").trim()
            ));
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn run_download(
        &self,
        args: Vec<String>,
        bearer: Option<&str>,
        part: &Path,
        progress: &mut dyn FnMut(u64),
    ) -> Result<(), String> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(args)
            .stdin(if bearer.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start curl: {e}"))?;
        if let (Some(t), Some(mut si)) = (bearer, child.stdin.take()) {
            let _ = std::io::Write::write_all(&mut si, bearer_config(t).as_bytes());
        }
        // curl only writes short error lines to stderr; read them aside so it never blocks
        let mut err = child.stderr.take();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(e) = err.as_mut() {
                let _ = std::io::Read::read_to_end(&mut std::io::Read::take(e, 16 << 10), &mut buf);
            }
            String::from_utf8_lossy(&buf).trim().to_string()
        });
        let held = || std::fs::metadata(part).map_or(0, |m| m.len());
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) => {
                    progress(held());
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(format!("could not wait for curl: {e}")),
            }
        };
        let said = reader.join().unwrap_or_default();
        progress(held());
        if status.success() {
            return Ok(());
        }
        let said = if said.is_empty() {
            String::new()
        } else {
            format!(": {said}")
        };
        Err(format!("curl stopped ({status}){said}"))
    }
}

/// The one line of curl config that carries a token. A token with a quote, a backslash or a
/// line break in it is never written (the caller checked; this is the second lock).
fn bearer_config(token: &str) -> String {
    let t: String = token
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .collect();
    format!("header = \"Authorization: Bearer {t}\"\n")
}

/// curl's arguments for one big download: https only (also after a redirect), carry on from a
/// partial file (`-C -`), give up on a link stuck under 1 KB/s for a minute, retry a few times.
pub fn curl_download_args(max_secs: u64, part: &Path, url: &str) -> Vec<String> {
    let max = max_secs.to_string();
    [
        "-sS",
        "--fail",
        "--location",
        "--max-redirs",
        "5",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "30",
        "--max-time",
        &max,
        "--speed-limit",
        "1024",
        "--speed-time",
        "60",
        "--retry",
        "3",
        "--retry-delay",
        "5",
        "-C",
        "-",
        "--output",
    ]
    .iter()
    .map(|a| a.to_string())
    .chain([part.to_string_lossy().into_owned(), url.to_string()])
    .collect()
}

/// A curl that answers `--version`: on Windows the one Windows ships first, then any on the path.
pub fn find_curl(host: &dyn Host) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    if host.os() == "windows" {
        let root = host
            .env("SystemRoot")
            .unwrap_or_else(|| r"C:\Windows".into());
        names.push(format!(r"{root}\System32\curl.exe"));
    }
    names.push("curl".into());
    names
        .into_iter()
        .find(|n| host.run(n, &["--version"]).is_some())
}

pub fn real_fetcher(host: &dyn Host) -> Option<Arc<dyn Fetch>> {
    find_curl(host).map(|program| Arc::new(Curl { program }) as Arc<dyn Fetch>)
}

// ---- one lookup

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    /// Unix seconds when the registry answered.
    pub at: u64,
    /// None: the registry says this name does not exist.
    pub bytes: Option<u64>,
    pub licence: String,
    pub licence_known: bool,
}

pub enum Lookup {
    Answer(Entry),
    /// Could not find out (network, refusal, odd answer): never cached.
    Unknown,
}

fn see_licence(family: &str) -> String {
    format!("see the licence at {}", model_page(family))
}

pub fn lookup(f: &dyn Fetch, name: &str, now: u64) -> Lookup {
    let Some((fam, tag)) = split_name(name) else {
        return Lookup::Unknown;
    };
    let url = format!("{REGISTRY}/v2/library/{fam}/manifests/{tag}");
    let manifest = match f.get(&url, Some(&format!("Accept: {MANIFEST_ACCEPT}")), false) {
        Fetched::NotFound => {
            return Lookup::Answer(Entry {
                name: name.into(),
                at: now,
                bytes: None,
                licence: String::new(),
                licence_known: false,
            })
        }
        Fetched::Failed => return Lookup::Unknown,
        Fetched::Body(b) => match parse_manifest(&b) {
            Ok(m) => m,
            Err(_) => return Lookup::Unknown,
        },
    };
    let detected = manifest.licence_digest.as_ref().and_then(|d| {
        match f.get(
            &format!("{REGISTRY}/v2/library/{fam}/blobs/{d}"),
            None,
            true,
        ) {
            Fetched::Body(t) => detect_licence(&t),
            _ => None,
        }
    });
    Lookup::Answer(Entry {
        name: name.into(),
        at: now,
        bytes: Some(manifest.bytes),
        licence_known: detected.is_some(),
        licence: detected.unwrap_or_else(|| see_licence(fam)),
    })
}

// ---- the cache

pub fn cache_path(home: &Path) -> std::path::PathBuf {
    home.join("models").join("registry-cache.json")
}

pub fn parse_cache(text: &str) -> Vec<Entry> {
    let Ok(j) = parse(text) else { return vec![] };
    j.arr("entries")
        .iter()
        .filter_map(|e| {
            Some(Entry {
                name: e.str("name")?.to_string(),
                at: e.f64("at").filter(|a| *a >= 0.0)? as u64,
                bytes: e.f64("bytes").filter(|b| *b > 0.0).map(|b| b as u64),
                licence: e.str("licence").unwrap_or("").to_string(),
                licence_known: e.bool("licence_known").unwrap_or(false),
            })
        })
        .collect()
}

pub fn cache_json(entries: &[Entry]) -> String {
    J::Obj(vec![
        ("version".into(), J::n(1.0)),
        (
            "entries".into(),
            J::Arr(
                entries
                    .iter()
                    .map(|e| {
                        let mut o = vec![
                            ("name".to_string(), J::s(&e.name)),
                            ("at".to_string(), J::n(e.at as f64)),
                        ];
                        if let Some(b) = e.bytes {
                            o.push(("bytes".into(), J::n(b as f64)));
                            o.push(("licence".into(), J::s(&e.licence)));
                            o.push(("licence_known".into(), J::Bool(e.licence_known)));
                        }
                        J::Obj(o)
                    })
                    .collect(),
            ),
        ),
    ])
    .pretty()
}

/// Age in whole days when the entry is still good (under 7 days, not from the future).
pub fn fresh_days(e: &Entry, now: u64) -> Option<u64> {
    if e.at > now + DAY {
        return None;
    }
    let age = now.saturating_sub(e.at);
    (age < CACHE_DAYS * DAY).then_some(age / DAY)
}

// ---- resolving all candidates

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub candidates: usize,
    pub live: usize,
    pub cached: usize,
    /// The registry says the name does not exist.
    pub absent: usize,
    /// Not answered in time or at all.
    pub unreachable: usize,
    /// Of those, how many were still waiting when the total time ran out.
    pub timed_out: usize,
    pub no_curl: bool,
    /// Live lookups were not allowed (first start).
    pub cache_only: bool,
}

impl Report {
    /// What to tell the user about how much was verified.
    pub fn notes(&self) -> Vec<String> {
        let mut n = Vec::new();
        let ok = self.live + self.cached;
        if self.no_curl && self.unreachable > 0 {
            n.push("curl was not found on this computer, so Tempo could not check models with the Ollama registry (Windows 10 and later, macOS and most Linux have curl). Setup planned only from models already installed here and the verified models that ship with Tempo.".into());
        } else if self.cache_only && self.unreachable > 0 {
            n.push(format!("{} candidate models have not been checked with the Ollama registry yet. Run `tempo-launch setup` to check them.", self.unreachable));
        } else if self.live == 0 && self.unreachable > 0 && self.timed_out == 0 {
            n.push("The Ollama registry could not be reached (offline?), so candidate models could not be verified. Setup planned only from cached checks, models already installed here and the verified models that ship with Tempo.".into());
        } else if self.unreachable > 0 {
            n.push(format!(
                "{} candidate models could not be checked in time and were skipped.",
                self.unreachable
            ));
        }
        if ok > 0 {
            n.push(format!(
                "{ok} of {} candidate models are verified with the Ollama registry ({} just now, {} from the cache, kept {CACHE_DAYS} days).",
                self.candidates, self.live, self.cached
            ));
        }
        n
    }
}

pub struct Resolved {
    /// (name, entry with bytes, days old: None = checked just now)
    pub found: Vec<(Entry, Option<u64>)>,
    pub report: Report,
}

pub fn resolve(
    fetcher: Option<Arc<dyn Fetch>>,
    cands: &[Candidate],
    home: &Path,
    now: u64,
    live_allowed: bool,
) -> Resolved {
    resolve_within(
        fetcher,
        cands,
        home,
        now,
        live_allowed,
        Duration::from_secs(TOTAL_SECS),
    )
}

pub fn resolve_within(
    fetcher: Option<Arc<dyn Fetch>>,
    cands: &[Candidate],
    home: &Path,
    now: u64,
    live_allowed: bool,
    budget: Duration,
) -> Resolved {
    let old = std::fs::read_to_string(cache_path(home))
        .map(|t| parse_cache(&t))
        .unwrap_or_default();
    let mut report = Report {
        candidates: cands.len(),
        cache_only: !live_allowed,
        ..Report::default()
    };
    let mut found: Vec<(Entry, Option<u64>)> = Vec::new();
    let mut todo: VecDeque<String> = VecDeque::new();
    for c in cands {
        match old
            .iter()
            .find(|e| e.name == c.name)
            .and_then(|e| fresh_days(e, now).map(|d| (e, d)))
        {
            Some((e, d)) => {
                if e.bytes.is_some() {
                    report.cached += 1;
                    found.push((e.clone(), Some(d)));
                } else {
                    report.absent += 1;
                }
            }
            None => todo.push_back(c.name.clone()),
        }
    }
    let mut learned: Vec<Entry> = Vec::new();
    match (&fetcher, live_allowed) {
        (Some(f), true) if !todo.is_empty() => {
            let n = todo.len();
            let answers = run_pool(f.clone(), todo, now, budget);
            report.unreachable += n - answers.len();
            report.timed_out = n - answers.len();
            for a in answers {
                match a {
                    Lookup::Answer(e) => {
                        if e.bytes.is_some() {
                            report.live += 1;
                            found.push((e.clone(), None));
                        } else {
                            report.absent += 1;
                        }
                        learned.push(e);
                    }
                    Lookup::Unknown => report.unreachable += 1,
                }
            }
        }
        (None, true) if !todo.is_empty() => {
            report.no_curl = true;
            report.unreachable += todo.len();
        }
        _ => report.unreachable += todo.len(),
    }
    if !learned.is_empty() {
        // fresh answers, plus older entries that are still good; failures to write are ignored
        let mut keep: Vec<Entry> = old
            .into_iter()
            .filter(|e| fresh_days(e, now).is_some() && !learned.iter().any(|l| l.name == e.name))
            .collect();
        keep.extend(learned);
        let path = cache_path(home);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::write_atomic(&path, cache_json(&keep).as_bytes());
    }
    Resolved { found, report }
}

fn run_pool(f: Arc<dyn Fetch>, names: VecDeque<String>, now: u64, budget: Duration) -> Vec<Lookup> {
    let n = names.len();
    let jobs = Arc::new(Mutex::new(names));
    let (tx, rx) = mpsc::channel();
    let deadline = Instant::now() + budget;
    for _ in 0..WORKERS.min(n) {
        let (jobs, tx, f) = (jobs.clone(), tx.clone(), f.clone());
        std::thread::spawn(move || {
            while Instant::now() < deadline {
                let Some(name) = jobs.lock().ok().and_then(|mut q| q.pop_front()) else {
                    break;
                };
                if tx.send(lookup(&*f, &name, now)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut out = Vec::new();
    while let Ok(l) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        out.push(l);
    }
    out
}

// ---- into the catalog

fn gb_of(bytes: u64) -> f64 {
    ((bytes as f64 / GB) * 100.0).round() / 100.0
}

fn same(a: &str, b: &str) -> bool {
    let n = |t: &str| {
        let t = t.to_lowercase();
        t.strip_suffix(":latest").unwrap_or(&t).to_string()
    };
    n(a) == n(b)
}

fn roles_of(c: &Candidate) -> Vec<String> {
    c.roles
        .iter()
        .map(|r| {
            if r == "code" {
                "writing-code"
            } else {
                r.as_str()
            }
            .to_string()
        })
        .collect()
}

fn model_for(c: &Candidate, gb: Option<f64>, licence: String, proof: Proof) -> Model {
    let need = gb.map(|g| ((g + HEADROOM_GB) * 10.0).round() / 10.0);
    Model {
        name: c.name.clone(),
        source: Source::Ollama,
        gb,
        params_b: c.params_b,
        active_params_b: c.active_params_b,
        roles: roles_of(c),
        licence,
        min_ram_gb: need,
        min_vram_gb: need,
        opt_in: false,
        pull: None,
        link: Some(model_page(split_name(&c.name).map_or("", |s| s.0))),
        notes: c.about.clone(),
        proof,
    }
}

/// Put what the registry said into the catalog: a shipped entry of the same name gets the real
/// size and licence; a verified candidate is added; an unverified candidate is added only when
/// it is already installed here (then Ollama's own size is used and the licence says "see the
/// licence"). Anything else is dropped, so an unverified model is never recommended.
pub fn merge(
    catalog: &mut Vec<Model>,
    cands: &[Candidate],
    found: &[(Entry, Option<u64>)],
    installed: &[Installed],
) {
    for c in cands {
        let fam = split_name(&c.name).map_or("", |s| s.0);
        let hit = found
            .iter()
            .find(|(e, _)| e.name == c.name)
            .and_then(|(e, age)| e.bytes.map(|b| (e, b, *age)));
        let slot = catalog
            .iter()
            .position(|m| m.source == Source::Ollama && same(&m.name, &c.name));
        match (hit, slot) {
            (Some((e, b, age)), Some(i)) => {
                let m = &mut catalog[i];
                let g = gb_of(b);
                let need = ((g + HEADROOM_GB) * 10.0).round() / 10.0;
                m.gb = Some(g);
                m.min_ram_gb = Some(need);
                m.min_vram_gb = Some(need);
                m.licence = e.licence.clone();
                m.proof = Proof::Registry { cached_days: age };
                m.link.get_or_insert_with(|| model_page(fam));
            }
            (Some((e, b, age)), None) => catalog.push(model_for(
                c,
                Some(gb_of(b)),
                e.licence.clone(),
                Proof::Registry { cached_days: age },
            )),
            (None, Some(_)) => {}
            (None, None) => {
                if let Some(i) = installed.iter().find(|i| same(&i.name, &c.name)) {
                    let gb = (i.size_bytes > 0).then(|| gb_of(i.size_bytes));
                    catalog.push(model_for(c, gb, see_licence(fam), Proof::Installed));
                }
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A registry answering from a table (no network). Names not in the table do not exist.
    pub struct MockReg {
        pub gb: HashMap<String, f64>,
        pub calls: Mutex<Vec<(String, Option<String>, bool)>>,
        pub delay: Duration,
        pub offline: bool,
        pub no_licence_blob: bool,
    }

    pub fn digest_of(family: &str) -> String {
        let hex: String = family.bytes().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex:0>64}")
    }

    pub fn licence_text(family: &str) -> &'static str {
        if family.starts_with("gemma") || family.starts_with("embeddinggemma") {
            "Gemma Terms of Use\n\nLast modified: February 21, 2024\n\nBy using..."
        } else if family.starts_with("llama") {
            "LLAMA 3.2 COMMUNITY LICENSE AGREEMENT\nLlama 3.2 Version Release Date: September 25, 2024"
        } else if family.starts_with("phi") {
            "MIT License\n\nCopyright (c) Microsoft Corporation.\n"
        } else {
            "                                 Apache License\n                           Version 2.0, January 2004\n"
        }
    }

    pub fn manifest_json(family: &str, gb: f64, licence: bool) -> String {
        let model = (gb * 1e9) as u64;
        let lic = if licence {
            format!(
                r#",{{"mediaType":"application/vnd.ollama.image.license","digest":"{}","size":11357}}"#,
                digest_of(family)
            )
        } else {
            String::new()
        };
        format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"sha256:{c}","size":487}},"layers":[{{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:{c}","size":{model}}},{{"mediaType":"application/vnd.ollama.image.template","digest":"sha256:{c}","size":1500}}{lic},{{"mediaType":"application/vnd.ollama.image.params","digest":"sha256:{c}","size":98}}]}}"#,
            c = "a".repeat(64)
        )
    }

    impl MockReg {
        pub fn new(table: &[(&str, f64)]) -> MockReg {
            MockReg {
                gb: table.iter().map(|(n, g)| (n.to_string(), *g)).collect(),
                calls: Mutex::new(vec![]),
                delay: Duration::ZERO,
                offline: false,
                no_licence_blob: false,
            }
        }
        pub fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Fetch for MockReg {
        fn get(&self, url: &str, accept: Option<&str>, follow: bool) -> Fetched {
            self.calls
                .lock()
                .unwrap()
                .push((url.into(), accept.map(String::from), follow));
            std::thread::sleep(self.delay);
            if self.offline {
                return Fetched::Failed;
            }
            let rest = url
                .strip_prefix("https://registry.ollama.ai/v2/library/")
                .unwrap();
            let (fam, what) = rest.split_once('/').unwrap();
            if let Some(tag) = what.strip_prefix("manifests/") {
                return match self.gb.get(&format!("{fam}:{tag}")).or(if tag == "latest" {
                    self.gb.get(fam)
                } else {
                    None
                }) {
                    Some(g) => Fetched::Body(manifest_json(fam, *g, true)),
                    None => Fetched::NotFound,
                };
            }
            if what.starts_with("blobs/")
                && !self.no_licence_blob
                && what.ends_with(&digest_of(fam))
            {
                return Fetched::Body(licence_text(fam).into());
            }
            Fetched::Failed
        }
    }

    /// Fixture sizes in GB (test values, never shipped). qwen3:32b and gpt-oss:120b are left out
    /// on purpose: they "do not exist" in this registry.
    pub fn table() -> Vec<(&'static str, f64)> {
        vec![
            ("qwen3:4b", 2.5),
            ("qwen3:1.7b", 1.4),
            ("qwen3:0.6b", 0.52),
            ("qwen3:8b", 5.2),
            ("qwen3:14b", 9.3),
            ("qwen3:30b", 19.0),
            ("qwen3-coder:30b", 19.0),
            ("qwen2.5-coder:1.5b", 0.99),
            ("qwen2.5-coder:3b", 1.9),
            ("qwen2.5-coder:7b", 4.7),
            ("qwen2.5-coder:14b", 9.0),
            ("gemma3:270m", 0.29),
            ("gemma3:1b", 0.8),
            ("gemma3:4b", 3.3),
            ("gemma3:12b", 8.1),
            ("gemma3:27b", 17.0),
            ("gemma3n:e2b", 5.6),
            ("gemma3n:e4b", 7.5),
            ("llama3.2:1b", 1.3),
            ("llama3.2:3b", 2.0),
            ("llama3.1:8b", 4.9),
            ("phi4-mini:3.8b", 2.5),
            ("phi4:14b", 9.1),
            ("mistral:7b", 4.4),
            ("mistral-nemo:12b", 7.1),
            ("mistral-small:24b", 14.0),
            ("deepseek-r1:1.5b", 1.1),
            ("deepseek-r1:7b", 4.7),
            ("deepseek-r1:8b", 5.2),
            ("deepseek-r1:14b", 9.0),
            ("deepseek-r1:32b", 20.0),
            ("gpt-oss:20b", 14.0),
            ("granite3.3:2b", 1.5),
            ("granite3.3:8b", 4.9),
            ("smollm2:135m", 0.09),
            ("smollm2:360m", 0.73),
            ("smollm2:1.7b", 1.8),
            ("embeddinggemma", 0.62),
            ("nomic-embed-text", 0.27),
            ("mxbai-embed-large", 0.67),
            ("qwen3-embedding:0.6b", 0.64),
            ("qwen3-embedding:4b", 2.5),
            ("qwen3-embedding:8b", 4.7),
        ]
    }

    pub fn cands() -> Vec<Candidate> {
        parse_candidates(CANDIDATES).unwrap()
    }

    pub fn tmp(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-reg-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const NOW: u64 = 1_793_000_000;

    #[test]
    fn candidate_list_is_valid_and_carries_no_sizes_or_licences() {
        let c = cands();
        assert!(c.len() >= 40, "{}", c.len());
        let mut names: Vec<&str> = c.iter().map(|x| x.name.as_str()).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "duplicate candidate");
        for x in &c {
            assert!(split_name(&x.name).is_some(), "{}", x.name);
            assert!(!x.roles.is_empty() && x.params_b.is_some() && !x.about.is_empty());
            assert!(x
                .roles
                .iter()
                .all(|r| ["fast", "heavy", "code", "vision", "embed"].contains(&r.as_str())));
        }
        for fam in [
            "qwen3",
            "qwen3-coder",
            "qwen2.5-coder",
            "gemma3",
            "gemma3n",
            "llama3.2",
            "llama3.1",
            "phi4",
            "phi4-mini",
            "mistral",
            "mistral-small",
            "deepseek-r1",
            "gpt-oss",
            "embeddinggemma",
            "nomic-embed-text",
            "mxbai-embed-large",
            "qwen3-embedding",
        ] {
            assert!(
                c.iter().any(|x| x.name.split(':').next() == Some(fam)),
                "{fam}"
            );
        }
        assert!(
            c.iter().any(|x| x.name.starts_with("granite"))
                && c.iter().any(|x| x.name.starts_with("smollm2"))
        );
        assert!(
            CANDIDATES.to_lowercase().contains("never")
                && !CANDIDATES.contains("\"gb\"")
                && !CANDIDATES.contains("\"licence\":")
        );
    }

    #[test]
    fn names_that_could_bend_the_address_are_refused() {
        assert_eq!(split_name("qwen3:4b"), Some(("qwen3", "4b")));
        assert_eq!(
            split_name("nomic-embed-text"),
            Some(("nomic-embed-text", "latest"))
        );
        for bad in [
            "", "a/b:1", "a:b/c", "a b:1", "../x:1", "x:1?y=2", "x:", ":1", "x:1#",
        ] {
            assert_eq!(split_name(bad), None, "{bad}");
        }
        let m = MockReg::new(&[]);
        assert!(matches!(lookup(&m, "../etc:1", NOW), Lookup::Unknown));
        assert_eq!(m.count(), 0);
    }

    #[test]
    fn manifest_with_model_template_licence_and_params_sums_every_layer() {
        let m = parse_manifest(&manifest_json("qwen3", 2.5, true)).unwrap();
        assert_eq!(m.bytes, 2_500_000_000 + 1500 + 11357 + 98);
        assert_eq!(m.licence_digest, Some(digest_of("qwen3")));
    }

    #[test]
    fn manifest_of_a_vision_model_counts_the_projector_layer() {
        let t = r#"{"schemaVersion":2,"layers":[
          {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:1","size":2489757856},
          {"mediaType":"application/vnd.ollama.image.projector","digest":"sha256:2","size":851251104},
          {"mediaType":"application/vnd.ollama.image.template","digest":"sha256:3","size":358},
          {"mediaType":"application/vnd.ollama.image.license","digest":"sha256:4444444444444444444444444444444444444444444444444444444444444444","size":8431},
          {"mediaType":"application/vnd.ollama.image.params","digest":"sha256:5","size":77}]}"#;
        let m = parse_manifest(t).unwrap();
        assert_eq!(m.bytes, 2489757856 + 851251104 + 358 + 8431 + 77);
        assert!(m.bytes as f64 / 1e9 > 3.3 && (m.bytes as f64 / 1e9) < 3.4);
    }

    #[test]
    fn manifest_without_a_licence_layer_has_no_digest() {
        let m = parse_manifest(&manifest_json("x", 1.0, false)).unwrap();
        assert_eq!(m.licence_digest, None);
        assert!(m.bytes > 1_000_000_000);
    }

    #[test]
    fn broken_manifests_are_errors_not_guesses() {
        assert!(parse_manifest("{ nope").is_err());
        assert!(parse_manifest(r#"{"layers":[]}"#).is_err());
        // no model layer: not a model
        assert!(parse_manifest(
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.template","size":5}]}"#
        )
        .is_err());
        // a layer without a size
        assert!(parse_manifest(
            r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:1"}]}"#
        )
        .is_err());
        // a licence digest that is not a sha256 is ignored (it goes into a URL)
        let t = r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","size":9},
            {"mediaType":"application/vnd.ollama.image.license","digest":"sha256:../../x","size":3}]}"#;
        assert_eq!(parse_manifest(t).unwrap().licence_digest, None);
    }

    #[test]
    fn licences_are_named_from_their_first_lines() {
        let d = |t: &str| detect_licence(t);
        assert_eq!(
            d(licence_text("qwen3")).as_deref(),
            Some("Apache License 2.0")
        );
        assert_eq!(
            d("Apache License\nVersion 2.0, January 2004\nhttp://www.apache.org/licenses/")
                .as_deref(),
            Some("Apache License 2.0")
        );
        assert_eq!(
            d("MIT License\n\nCopyright (c) 2024 X").as_deref(),
            Some("MIT")
        );
        assert_eq!(
            d("Copyright 2024\n\nPermission is hereby granted, free of charge, to any person")
                .as_deref(),
            Some("MIT")
        );
        assert_eq!(
            d(licence_text("gemma3")).as_deref(),
            Some("Gemma Terms of Use")
        );
        assert_eq!(
            d(licence_text("llama3.2")).as_deref(),
            Some("Llama 3.2 Community License")
        );
        assert_eq!(
            d("LLAMA 3.1 COMMUNITY LICENSE AGREEMENT\nLlama 3.1 Version Release Date").as_deref(),
            Some("Llama 3.1 Community License")
        );
        assert_eq!(
            d("Qwen RESEARCH LICENSE AGREEMENT\nQwen Research License").as_deref(),
            Some("Qwen Research License")
        );
        assert_eq!(
            d("Tongyi Qianwen LICENSE AGREEMENT").as_deref(),
            Some("Qwen License")
        );
        assert_eq!(
            d("DeepSeek License Agreement\nVersion 1.0").as_deref(),
            Some("DeepSeek License")
        );
        assert_eq!(
            d("\n\n  Acme Open Model Licence.\nblah blah").as_deref(),
            Some("Acme Open Model Licence")
        );
        // not recognised: None, so the page link is shown instead
        assert_eq!(
            d("lorem ipsum dolor sit amet, consectetur adipiscing elit sed do eiusmod tempor"),
            None
        );
        assert_eq!(d(""), None);
        assert_eq!(d("   \n \n"), None);
    }

    #[test]
    fn lookup_reads_size_and_licence_with_the_right_requests() {
        let m = MockReg::new(&[("gemma3:4b", 3.3)]);
        let Lookup::Answer(e) = lookup(&m, "gemma3:4b", NOW) else {
            panic!()
        };
        assert_eq!(e.bytes.map(|b| b / 1_000_000), Some(3300));
        assert_eq!(
            (e.licence.as_str(), e.licence_known),
            ("Gemma Terms of Use", true)
        );
        let calls = m.calls.lock().unwrap();
        assert_eq!(
            calls[0].0,
            "https://registry.ollama.ai/v2/library/gemma3/manifests/4b"
        );
        assert_eq!(
            calls[0].1.as_deref(),
            Some("Accept: application/vnd.docker.distribution.manifest.v2+json")
        );
        assert!(!calls[0].2, "manifests never follow redirects");
        assert!(calls[1]
            .0
            .starts_with("https://registry.ollama.ai/v2/library/gemma3/blobs/sha256:"));
        assert!(calls[1].2);
        assert!(calls.iter().all(|c| c.0.starts_with("https://")));
    }

    #[test]
    fn lookup_without_a_readable_licence_says_see_the_licence() {
        let mut m = MockReg::new(&[("qwen3:4b", 2.5)]);
        m.no_licence_blob = true;
        let Lookup::Answer(e) = lookup(&m, "qwen3:4b", NOW) else {
            panic!()
        };
        assert!(!e.licence_known);
        assert_eq!(
            e.licence,
            "see the licence at https://ollama.com/library/qwen3"
        );
        assert!(e.bytes.is_some());
    }

    #[test]
    fn missing_names_are_absent_and_network_trouble_is_unknown() {
        let m = MockReg::new(&[]);
        let Lookup::Answer(e) = lookup(&m, "nope:1b", NOW) else {
            panic!()
        };
        assert_eq!(e.bytes, None);
        let mut m = MockReg::new(&[("a:1b", 1.0)]);
        m.offline = true;
        assert!(matches!(lookup(&m, "a:1b", NOW), Lookup::Unknown));
    }

    #[test]
    fn cache_round_trips_and_expires_after_seven_days() {
        let e = Entry {
            name: "a:1b".into(),
            at: NOW,
            bytes: Some(5),
            licence: "MIT".into(),
            licence_known: true,
        };
        let gone = Entry {
            name: "b:1b".into(),
            at: NOW,
            bytes: None,
            licence: String::new(),
            licence_known: false,
        };
        let back = parse_cache(&cache_json(&[e.clone(), gone.clone()]));
        assert_eq!(back, vec![e.clone(), gone]);
        assert_eq!(fresh_days(&e, NOW), Some(0));
        assert_eq!(fresh_days(&e, NOW + 3 * DAY + 5), Some(3));
        assert_eq!(fresh_days(&e, NOW + 7 * DAY - 1), Some(6));
        assert_eq!(fresh_days(&e, NOW + 7 * DAY), None);
        // a clock set far back makes the entry look like it is from the future: not trusted
        assert_eq!(fresh_days(&e, NOW - 2 * DAY), None);
        assert!(parse_cache("{ broken").is_empty());
        assert!(parse_cache(r#"{"entries":[{"name":"x"},{"at":1}]}"#).is_empty());
    }

    #[test]
    fn a_second_run_is_answered_from_the_cache_with_no_network() {
        let home = tmp("cache");
        let m = Arc::new(MockReg::new(&table()));
        let first = resolve_within(
            Some(m.clone()),
            &cands(),
            &home,
            NOW,
            true,
            Duration::from_secs(10),
        );
        assert_eq!(first.report.live, table().len());
        assert!(first.report.absent > 0 && first.report.unreachable == 0);
        let calls = m.count();
        assert!(cache_path(&home).is_file());
        // later, offline and with no curl at all: everything still known comes from the cache
        let second = resolve_within(
            None,
            &cands(),
            &home,
            NOW + 3 * DAY,
            true,
            Duration::from_secs(10),
        );
        assert_eq!(m.count(), calls);
        assert_eq!(second.report.cached, table().len());
        assert!(!second.report.no_curl && second.report.live == 0);
        assert!(second.found.iter().all(|(_, age)| *age == Some(3)));
        // after 7 days it is asked again
        let third = resolve_within(
            Some(m.clone()),
            &cands(),
            &home,
            NOW + 8 * DAY,
            true,
            Duration::from_secs(10),
        );
        assert_eq!(third.report.live, table().len());
        assert!(m.count() > calls);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn no_curl_falls_back_cleanly_and_says_so() {
        let home = tmp("nocurl");
        let r = resolve(None, &cands(), &home, NOW, true);
        assert!(r.found.is_empty() && r.report.no_curl);
        let notes = r.report.notes().join(" ");
        assert!(notes.contains("curl was not found") && notes.contains("already installed"));
        assert!(!cache_path(&home).exists());
        // first start never uses the network, and does not call it curl trouble
        let m = Arc::new(MockReg::new(&table()));
        let r = resolve(Some(m.clone()), &cands(), &home, NOW, false);
        assert_eq!((m.count(), r.found.len()), (0, 0));
        assert!(r.report.notes().join(" ").contains("have not been checked"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn offline_falls_back_cleanly_and_caches_nothing() {
        let home = tmp("offline");
        let mut m = MockReg::new(&table());
        m.offline = true;
        let r = resolve_within(
            Some(Arc::new(m)),
            &cands(),
            &home,
            NOW,
            true,
            Duration::from_secs(10),
        );
        assert!(r.found.is_empty());
        assert_eq!(r.report.unreachable, cands().len());
        assert!(r.report.notes().join(" ").contains("could not be reached"));
        assert!(
            !cache_path(&home).exists(),
            "a failure must not be remembered as 'absent'"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_total_time_is_bounded() {
        let home = tmp("slow");
        let mut m = MockReg::new(&table());
        m.delay = Duration::from_millis(400);
        let t = Instant::now();
        let r = resolve_within(
            Some(Arc::new(m)),
            &cands(),
            &home,
            NOW,
            true,
            Duration::from_millis(500),
        );
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "{:?}",
            t.elapsed()
        );
        assert!(r.report.unreachable > 0);
        assert!(r.report.notes().join(" ").contains("in time"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn lookups_run_in_parallel() {
        let home = tmp("par");
        let mut m = MockReg::new(&table());
        m.delay = Duration::from_millis(100);
        let t = Instant::now();
        // 47 candidates x 2 requests x 100 ms would be 9 s one after the other
        let r = resolve_within(
            Some(Arc::new(m)),
            &cands(),
            &home,
            NOW,
            true,
            Duration::from_secs(10),
        );
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
        assert_eq!(r.report.unreachable, 0);
        let _ = std::fs::remove_dir_all(&home);
    }

    fn installed(name: &str, bytes: u64) -> Installed {
        Installed {
            name: name.into(),
            size_bytes: bytes,
            digest: String::new(),
        }
    }

    #[test]
    fn merge_refreshes_shipped_adds_verified_and_drops_the_rest() {
        let mut cat =
            super::super::catalog::parse_catalog(super::super::catalog::EMBEDDED).unwrap();
        let before = cat.len();
        let c = cands();
        let found = vec![
            (
                Entry {
                    name: "qwen3:4b".into(),
                    at: NOW,
                    bytes: Some(2_600_000_000),
                    licence: "Apache License 2.0".into(),
                    licence_known: true,
                },
                None,
            ),
            (
                Entry {
                    name: "embeddinggemma".into(),
                    at: NOW,
                    bytes: Some(621_000_000),
                    licence: "Gemma Terms of Use".into(),
                    licence_known: true,
                },
                Some(2),
            ),
            (
                Entry {
                    name: "phi4:14b".into(),
                    at: NOW,
                    bytes: Some(9_100_000_000),
                    licence: "MIT".into(),
                    licence_known: true,
                },
                None,
            ),
        ];
        merge(
            &mut cat,
            &c,
            &found,
            &[
                installed("mistral:7b", 4_400_000_000),
                installed("llama3.1:8b:x", 5),
            ],
        );
        let by = |n: &str| cat.iter().find(|m| m.name == n);
        // shipped entries keep their place and roles but get the real size and licence
        assert_eq!(by("qwen3:4b").unwrap().gb, Some(2.6));
        assert_eq!(by("qwen3:4b").unwrap().min_ram_gb, Some(4.1));
        assert_eq!(
            by("qwen3:4b").unwrap().proof,
            Proof::Registry { cached_days: None }
        );
        assert!(by("qwen3:4b").unwrap().has_role("thinking"));
        assert_eq!(by("embeddinggemma").unwrap().gb, Some(0.62));
        assert_eq!(by("embeddinggemma").unwrap().licence, "Gemma Terms of Use");
        assert_eq!(
            by("embeddinggemma").unwrap().proof,
            Proof::Registry {
                cached_days: Some(2)
            }
        );
        // a verified candidate is added with the registry's numbers
        let p = by("phi4:14b").unwrap();
        assert_eq!(
            (p.gb, p.licence.as_str(), p.params_b),
            (Some(9.1), "MIT", Some(14.0))
        );
        assert_eq!(p.link.as_deref(), Some("https://ollama.com/library/phi4"));
        // unverified and not installed: not there at all
        assert!(by("qwen3:32b").is_none() && by("gpt-oss:120b").is_none());
        // unverified but installed: kept, with Ollama's size and an honest licence
        let i = by("mistral:7b").unwrap();
        assert_eq!((i.gb, i.proof.clone()), (Some(4.4), Proof::Installed));
        assert!(i.licence.starts_with("see the licence at"));
        // 3 verified (2 refreshed) + 1 installed: the shipped five plus phi4 and mistral
        assert_eq!(cat.len(), before + 2);
    }

    #[test]
    fn proof_texts_say_where_the_numbers_came_from() {
        assert_eq!(
            Proof::Registry { cached_days: None }.text().unwrap(),
            "Verified with the Ollama registry today"
        );
        assert_eq!(
            Proof::Registry {
                cached_days: Some(4)
            }
            .text()
            .unwrap(),
            "From the cache, 4 days old (verified with the Ollama registry)"
        );
        assert!(Proof::Registry {
            cached_days: Some(1)
        }
        .text()
        .unwrap()
        .contains("1 day old"));
        assert!(Proof::Installed.text().unwrap().contains("not checked"));
        assert!(Proof::Own.text().is_none());
    }

    #[test]
    fn a_real_curl_that_cannot_connect_fails_cleanly() {
        // no network in tests: a closed local port over https is refused at once
        let c = Curl {
            program: "curl".into(),
        };
        assert_eq!(c.get("http://127.0.0.1:1/x", None, false), Fetched::Failed);
        assert_eq!(c.get("https://127.0.0.1:1/x", None, false), Fetched::Failed);
        let missing = Curl {
            program: "no-such-curl-program".into(),
        };
        assert_eq!(
            missing.get("https://registry.ollama.ai/", None, false),
            Fetched::Failed
        );
    }
}
