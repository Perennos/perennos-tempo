//! `tempo-launch setup`: print the plan, and with consent fetch what is missing and write the
//! agent's settings. Nothing is fetched without `--apply` and a yes (or `--yes`).

use super::catalog;
use super::json::{parse, J};
use super::page::D1_LICENCE;
use super::plan::{self, Goals, Pick, Plan, Prefer, Status};
use super::probe::{self, Host, Machine};
use super::registry;
use super::runtime::{self, RuntimePlan};
use crate::{file_stamp, json_escape, stamp, write_atomic, Ctx};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const OLLAMA_PAGE: &str = "https://ollama.com/download";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub apply: bool,
    pub yes: bool,
    pub json: bool,
    pub with_d1: bool,
    /// `--prefer preferred|mainstream|quality`; unset means the owner's preferred models when such
    /// are installed (see `--preferred-prefix`).
    pub prefer: Prefer,
    /// Set when `--prefer` was given, so the apply command repeats it.
    pub prefer_set: bool,
    /// `--page`: write `<home>/setup.html` (the plan, for the first start to open); change nothing else.
    pub page: bool,
    /// `--ollama`: fetch and check only Tempo's own Ollama (when this computer has none), and start
    /// it once to see that it answers. No models are looked up or pulled, no settings are written.
    pub ollama: bool,
    /// `--apply-proposal`: show what the proposal an AI wrote (`tempo-launch mcp`) would change in
    /// the agent's settings, ask y/N, and only then write it. Always asks; takes no `--yes`.
    pub apply_proposal: bool,
    /// `--voice`: fetch whisper.cpp and a model that fits (and with `--piper`, Piper and a voice) so
    /// you can talk to Tempo. Asks y/N first. `--multilingual` picks the models that are not English-only.
    pub voice: bool,
    pub piper: bool,
    pub multilingual: bool,
    pub goals: Vec<String>,
    /// `--preferred-prefix P`: save P as this computer's preferred prefix in `<home>/setup.json` (an
    /// empty value clears it) and use it now.
    pub preferred_prefix: Option<String>,
}

pub fn parse_args(rest: &[&str]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--apply" => o.apply = true,
            "--yes" | "-y" => o.yes = true,
            "--json" => o.json = true,
            "--with-d1" => o.with_d1 = true,
            "--page" => o.page = true,
            "--ollama" => o.ollama = true,
            "--apply-proposal" => o.apply_proposal = true,
            "--voice" => o.voice = true,
            "--piper" => o.piper = true,
            "--multilingual" => o.multilingual = true,
            "--goal" => {
                i += 1;
                o.goals.push(
                    rest.get(i)
                        .filter(|v| !v.starts_with("--"))
                        .ok_or("--goal needs a value, like --goal chat,code")?
                        .to_string(),
                );
            }
            "--prefer" => {
                i += 1;
                let v = rest
                    .get(i)
                    .filter(|v| !v.starts_with("--"))
                    .ok_or("--prefer needs preferred, mainstream or quality")?;
                o.prefer = Prefer::parse(v)?;
                o.prefer_set = true;
            }
            a if a.starts_with("--prefer=") => {
                o.prefer = Prefer::parse(&a["--prefer=".len()..])?;
                o.prefer_set = true;
            }
            "--preferred-prefix" => {
                i += 1;
                let v = rest
                    .get(i)
                    .filter(|v| !v.starts_with("--"))
                    .ok_or("--preferred-prefix needs a value, like --preferred-prefix mine- (\"\" clears it)")?;
                o.preferred_prefix = Some(checked_prefix(v)?);
            }
            a if a.starts_with("--preferred-prefix=") => {
                o.preferred_prefix = Some(checked_prefix(&a["--preferred-prefix=".len()..])?);
            }
            a if a.starts_with("--goal=") => o.goals.push(a["--goal=".len()..].to_string()),
            other => return Err(format!("setup: unknown argument {other}")),
        }
        i += 1;
    }
    if (o.piper || o.multilingual) && !o.voice {
        return Err("setup: --piper and --multilingual go with --voice".into());
    }
    if o.voice
        && (o.apply
            || o.page
            || o.json
            || o.ollama
            || o.with_d1
            || o.apply_proposal
            || !o.goals.is_empty()
            || o.prefer_set
            || o.preferred_prefix.is_some())
    {
        return Err("setup: --voice stands alone; add --piper, --multilingual or --yes".into());
    }
    if o.apply_proposal
        && (o.apply
            || o.yes
            || o.page
            || o.json
            || o.ollama
            || o.with_d1
            || !o.goals.is_empty()
            || o.prefer_set
            || o.preferred_prefix.is_some())
    {
        return Err("setup: --apply-proposal stands alone, and always asks".into());
    }
    if o.ollama
        && (o.apply
            || o.page
            || o.json
            || o.with_d1
            || !o.goals.is_empty()
            || o.prefer_set
            || o.preferred_prefix.is_some())
    {
        return Err("setup: --ollama only fetches Ollama; use it alone, or with --yes".into());
    }
    if o.yes && !o.apply && !o.ollama && !o.voice {
        return Err("setup: --yes goes with --apply, --ollama or --voice".into());
    }
    if o.page && (o.apply || o.json) {
        return Err("setup: --page cannot be combined with --apply or --json".into());
    }
    Ok(o)
}

/// A preferred prefix as given: lower case; empty clears; otherwise 1 to 40 of a-z 0-9 . _ -.
fn checked_prefix(v: &str) -> Result<String, String> {
    let p = v.trim().to_lowercase();
    if p.is_empty() || probe::valid_prefix(&p) {
        Ok(p)
    } else {
        Err(format!(
            "--preferred-prefix {v:?}: use 1 to 40 of a-z, 0-9, dot, underscore and hyphen"
        ))
    }
}

/// Save the preferred prefix in `<home>/setup.json`, keeping its other keys (the old file goes to
/// the trash first, as with the agent's settings).
fn save_preferred_prefix(home: &Path, prefix: &str, now: u64) -> Result<(), String> {
    let path = home.join("setup.json");
    let old = std::fs::read_to_string(&path).ok();
    let mut j = match &old {
        Some(t) => match parse(t) {
            Ok(j @ J::Obj(_)) => j,
            _ => {
                return Err(format!(
                    "{} is not a JSON object, so setup left it alone",
                    path.display()
                ))
            }
        },
        None => J::Obj(Vec::new()),
    };
    j.set("preferred_prefix", J::s(prefix));
    if let Some(t) = &old {
        let dir = home.join("trash").join(&file_stamp(now)[..8]);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not make the trash folder: {e}"))?;
        std::fs::write(dir.join(format!("setup-{}.json", &file_stamp(now)[9..])), t)
            .map_err(|e| format!("could not keep the old setup.json: {e}"))?;
    }
    std::fs::create_dir_all(home).map_err(|e| format!("could not make {}: {e}", home.display()))?;
    write_atomic(&path, j.pretty().as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

pub fn ollama_port() -> u16 {
    ["OLLAMA_URL", "TEMPO_OLLAMA"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find_map(|u| {
            u.rsplit(':')
                .next()
                .and_then(|p| p.trim_end_matches('/').parse().ok())
        })
        .unwrap_or(11434)
}

// ---- fetching

/// `POST /api/pull` on this computer's Ollama, reading its progress lines. HTTP/1.0 so the answer
/// is a plain stream.
pub fn pull(port: u16, model: &str, progress: &mut dyn FnMut(&str)) -> Result<(), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .map_err(|e| format!("could not reach Ollama: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(120))).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let body = format!("{{\"model\":\"{}\",\"stream\":true}}", json_escape(model));
    let req = format!(
        "POST /api/pull HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())
        .map_err(|e| format!("could not ask Ollama: {e}"))?;
    let mut st = PullState::default();
    let (mut raw, mut chunk) = (Vec::new(), [0u8; 8192]);
    let mut head: Option<(u16, bool)> = None;
    let mut done = 0usize; // body bytes already turned into lines
    loop {
        let n = match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("Ollama stopped answering ({e})")),
        };
        raw.extend_from_slice(&chunk[..n]);
        if head.is_none() {
            if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let h = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                let code = h
                    .split_whitespace()
                    .nth(1)
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(0);
                head = Some((code, h.contains("chunked")));
                raw.drain(..end + 4);
            }
        }
        if let Some((code, chunked)) = head {
            if code != 200 {
                continue;
            }
            if !chunked {
                // whole lines only
                while let Some(nl) = raw[done..].iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&raw[done..done + nl]).into_owned();
                    done += nl + 1;
                    st.line(&line, progress);
                }
            }
        }
    }
    let Some((code, chunked)) = head else {
        return Err("Ollama gave no answer".into());
    };
    if code != 200 {
        let text = String::from_utf8_lossy(&raw);
        let msg = parse(&text)
            .ok()
            .and_then(|j| j.str("error").map(String::from))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!("Ollama answered {code}: {msg}"));
    }
    let rest: Vec<u8> = if chunked {
        crate::dechunk(&raw).unwrap_or_default()
    } else {
        raw[done..].to_vec()
    };
    for line in String::from_utf8_lossy(&rest).lines() {
        st.line(line, progress);
    }
    match (st.error, st.success) {
        (Some(e), _) => Err(e),
        (None, true) => Ok(()),
        (None, false) => Err("the download ended before Ollama said it was done".into()),
    }
}

#[derive(Default)]
struct PullState {
    status: String,
    pct: i64,
    success: bool,
    error: Option<String>,
}

impl PullState {
    fn line(&mut self, line: &str, progress: &mut dyn FnMut(&str)) {
        let Ok(j) = parse(line.trim()) else { return };
        if let Some(e) = j.str("error") {
            self.error = Some(e.to_string());
            return;
        }
        let status = j.str("status").unwrap_or("");
        if status == "success" {
            self.success = true;
        }
        let pct = match (j.f64("completed"), j.f64("total")) {
            (Some(c), Some(t)) if t > 0.0 => (c / t * 100.0) as i64,
            _ => -1,
        };
        // a line when the step changes, then every 10%
        let key = status.split_whitespace().next().unwrap_or("");
        let changed = key != self.status.split_whitespace().next().unwrap_or("")
            || status != self.status && pct < 0;
        if changed || (pct >= 0 && pct / 10 > self.pct / 10) {
            progress(&if pct >= 0 {
                format!("{status} {pct}%")
            } else {
                status.to_string()
            });
        }
        self.status = status.to_string();
        self.pct = pct.max(0);
    }
}

fn digest_of(port: u16, name: &str) -> Option<String> {
    let (200, body) = crate::http_get(port, "/api/tags", Duration::from_secs(3))? else {
        return None;
    };
    let want = name.to_lowercase();
    let want = want.strip_suffix(":latest").unwrap_or(&want);
    probe::parse_tags(&body)
        .into_iter()
        .find(|i| {
            let n = i.name.to_lowercase();
            n.strip_suffix(":latest").unwrap_or(&n) == want
        })
        .map(|i| i.digest)
}

// ---- the agent's settings

#[derive(Debug, Default, PartialEq)]
pub struct Written {
    pub path: PathBuf,
    pub changed: bool,
    pub trashed: Option<PathBuf>,
    pub set: Vec<String>,
    pub kept: Vec<String>,
}

/// The settings file the agent reads from its data folder, in the agent's and the launcher's
/// order: an older `agent-config.json` when there is one, else `config.json`.
pub fn agent_settings_path(agent_data: &Path) -> PathBuf {
    let older = agent_data.join("agent-config.json");
    if older.is_file() {
        older
    } else {
        agent_data.join("config.json")
    }
}

/// The file `TEMPO_AGENT_CONFIG` names, when it names one. It always wins over the agent's own
/// files (in the agent and the launcher), so setup leaves it alone.
pub fn pinned_agent_settings() -> Option<PathBuf> {
    pinned_from(std::env::var_os("TEMPO_AGENT_CONFIG"))
}

/// An empty value names nothing (the agent reads it the same way).
fn pinned_from(v: Option<std::ffi::OsString>) -> Option<PathBuf> {
    v.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Put `values` into the agent's settings file (`agent_settings_path`): a key the user set is
/// left alone; a key setup wrote earlier (and nobody changed since) is refreshed; a missing key is
/// added. The old file is copied to `<home>/trash/<date>/` before it is replaced. Nothing is
/// deleted.
pub fn merge_config(
    home: &Path,
    agent_data: &Path,
    values: &[(&str, J)],
    now: u64,
) -> Result<Written, String> {
    let path = agent_settings_path(agent_data);
    let old_text = std::fs::read_to_string(&path).ok();
    let mut cfg = match &old_text {
        Some(t) => match parse(t) {
            Ok(j @ J::Obj(_)) => j,
            _ => {
                return Err(format!(
                    "{} is not a JSON object, so setup left it alone. Fix or move it, then run setup again.",
                    path.display()
                ))
            }
        },
        None => J::Obj(Vec::new()),
    };
    let snapshot = cfg.get("configured_values").cloned();
    let mut w = Written {
        path: path.clone(),
        ..Written::default()
    };
    let mut written = Vec::new();
    for (k, v) in values {
        let ours = match (cfg.get(k), snapshot.as_ref().and_then(|s| s.get(k))) {
            (None, _) => true,
            (Some(cur), Some(was)) => cur == was,
            _ => false,
        };
        if !ours {
            w.kept.push((*k).to_string());
            continue;
        }
        written.push(((*k).to_string(), v.clone()));
        if cfg.get(k) != Some(v) {
            w.set.push((*k).to_string());
        }
    }
    if w.set.is_empty() {
        return Ok(w);
    }
    for (k, v) in &written {
        cfg.set(k, v.clone());
    }
    cfg.set("configured_by", J::s("tempo-launch setup"));
    cfg.set("configured_at", J::s(&stamp(now)));
    cfg.set("configured_values", J::Obj(written));
    if let Some(t) = &old_text {
        let fs = file_stamp(now);
        let dir = home.join("trash").join(&fs[..8]);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not make the trash folder: {e}"))?;
        // the backup keeps the file's own name: config.json, or the older agent-config.json
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.json".into());
        let mut dest = dir.join(&name);
        if dest.exists() {
            let stem = name.strip_suffix(".json").unwrap_or(&name);
            dest = dir.join(format!("{stem}-{}.json", &fs[9..]));
        }
        std::fs::write(&dest, t).map_err(|e| format!("could not save the old settings: {e}"))?;
        w.trashed = Some(dest);
    }
    write_atomic(&path, cfg.pretty().as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    w.changed = true;
    Ok(w)
}

/// The settings the plan implies, for the picks that are usable now (installed, or just fetched).
pub fn config_values(
    p: &Plan,
    usable: &dyn Fn(&Pick) -> bool,
    port: u16,
) -> Vec<(&'static str, J)> {
    let ok = |role: &str| p.pick(role).filter(|k| usable(k));
    let mut v = Vec::new();
    let heavy = ok("heavy");
    if let Some(h) = heavy {
        v.push(("models", J::strings(std::slice::from_ref(&h.model))));
    }
    if let Some(f) = ok("fast").filter(|f| heavy.is_none_or(|h| h.model != f.model)) {
        v.push(("fast_models", J::strings(std::slice::from_ref(&f.model))));
    }
    if let Some(e) = ok("embed") {
        v.push(("embed_model", J::s(&e.model)));
    }
    if !v.is_empty() {
        v.push(("ollama_url", J::s(&format!("http://127.0.0.1:{port}"))));
    }
    v
}

// ---- the whole command

pub fn parts(ctx: &Ctx) -> Vec<&'static str> {
    [
        ("tempo-local-agent", "chat"),
        ("tempo-vault", "notes"),
        ("tempo-room", "rooms"),
        ("tempo-studio", "music"),
    ]
    .iter()
    .filter(|(bin, _)| ctx.find(bin).is_some())
    .map(|(_, g)| *g)
    .collect()
}

pub struct Computed {
    pub machine: Machine,
    pub plan: Plan,
    pub warning: Option<String>,
    pub registry: registry::Report,
}

/// Look at the computer, check the candidate models with the Ollama registry (`live`: false uses
/// only what is cached, with no network), and plan.
pub fn compute(
    ctx: &Ctx,
    host: &dyn Host,
    o: &Opts,
    live: bool,
    port: u16,
    now: u64,
) -> Result<Computed, String> {
    let g = Goals::build(&parts(ctx), &o.goals)?;
    let mut machine = probe::probe(host, &ctx.home, port);
    if let Some(p) = &o.preferred_prefix {
        save_preferred_prefix(&ctx.home, p, now)?;
        machine.preferred_prefix = p.clone();
    }
    let (mut cat, warning) = catalog::load(&ctx.home);
    let cands = registry::parse_candidates(registry::CANDIDATES).unwrap_or_default();
    let res = registry::resolve(
        if live { host.fetcher() } else { None },
        &cands,
        &ctx.home,
        now,
        live,
    );
    registry::merge(&mut cat, &cands, &res.found, &machine.ollama.models);
    let mut plan = plan::plan_with(&machine, &g, o.with_d1, &cat, o.prefer);
    plan.runtime = runtime::plan_runtime(&machine, host, &ctx.home, &host.runtime_pins(), port);
    runtime::share_disk(&mut plan);
    let mut notes = res.report.notes();
    notes.append(&mut plan.notes);
    plan.notes = notes;
    Ok(Computed {
        machine,
        plan,
        warning,
        registry: res.report,
    })
}

pub fn apply_command(o: &Opts) -> String {
    let mut c = String::from("tempo-launch setup --apply");
    if o.with_d1 {
        c.push_str(" --with-d1");
    }
    if !o.goals.is_empty() {
        c.push_str(&format!(" --goal {}", o.goals.join(",")));
    }
    if o.prefer_set {
        c.push_str(match o.prefer {
            Prefer::Mainstream => " --prefer mainstream",
            Prefer::Quality => " --prefer quality",
            _ => " --prefer preferred",
        });
    }
    c
}

fn plan_json(c: &Computed) -> J {
    let p = &c.plan;
    let m = &c.machine;
    let opt = |v: Option<f64>| v.map_or(J::Null, J::n);
    J::Obj(vec![
        (
            "goals".into(),
            J::Arr(p.goals.iter().map(|g| J::s(g)).collect()),
        ),
        (
            "machine".into(),
            J::Obj(vec![
                ("os".into(), J::s(&m.os)),
                ("arch".into(), J::s(&m.arch)),
                ("cpus".into(), J::n(m.cpus as f64)),
                ("ram_total_gb".into(), opt(m.ram_total_gb)),
                ("ram_available_gb".into(), opt(m.ram_avail_gb)),
                ("disk_free_gb".into(), opt(m.disk_free_gb)),
                ("ollama_running".into(), J::Bool(m.ollama.running)),
                (
                    "ollama_found".into(),
                    J::Bool(m.ollama.binary.is_some() || m.ollama.running),
                ),
                (
                    "gpu".into(),
                    match &m.gpu {
                        probe::Gpu::None => J::s("none"),
                        probe::Gpu::Nvidia { name, total_gb, .. } => J::Obj(vec![
                            ("name".into(), J::s(name)),
                            ("memory_gb".into(), J::n((total_gb * 10.0).round() / 10.0)),
                        ]),
                        probe::Gpu::Apple { chip } => J::Obj(vec![
                            ("name".into(), J::s(chip)),
                            ("shared_memory".into(), J::Bool(true)),
                        ]),
                    },
                ),
            ]),
        ),
        (
            "picks".into(),
            J::Arr(
                p.picks
                    .iter()
                    .map(|k| {
                        J::Obj(vec![
                            ("role".into(), J::s(k.role)),
                            ("model".into(), J::s(&k.model)),
                            ("installed".into(), J::Bool(k.status == Status::Installed)),
                            ("gb".into(), opt(k.gb)),
                            ("licence".into(), J::s(&k.licence)),
                            ("verified".into(), J::s(&k.proof)),
                            ("why".into(), J::s(&k.why)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "fetch".into(),
            J::Arr(
                p.fetch
                    .iter()
                    .map(|f| {
                        J::Obj(vec![
                            ("name".into(), J::s(&f.name)),
                            ("gb".into(), opt(f.gb)),
                            ("licence".into(), J::s(&f.licence)),
                            ("verified".into(), J::s(&f.proof)),
                            ("link".into(), f.link.as_deref().map_or(J::Null, J::s)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("fetch_gb".into(), J::n((p.fetch_gb * 10.0).round() / 10.0)),
        ("refused".into(), p.refused.as_deref().map_or(J::Null, J::s)),
        // Tempo's own Ollama: null when an Ollama answers or is installed
        (
            "ollama_runtime".into(),
            p.runtime.as_ref().map_or(J::Null, RuntimePlan::json),
        ),
        (
            "registry".into(),
            J::Obj(vec![
                ("candidates".into(), J::n(c.registry.candidates as f64)),
                ("verified_now".into(), J::n(c.registry.live as f64)),
                ("from_cache".into(), J::n(c.registry.cached as f64)),
                ("not_in_registry".into(), J::n(c.registry.absent as f64)),
                ("not_checked".into(), J::n(c.registry.unreachable as f64)),
                ("curl_missing".into(), J::Bool(c.registry.no_curl)),
            ]),
        ),
        ("notes".into(), J::strings(&p.notes)),
    ])
}

fn d1_block(p: &Plan) -> Option<String> {
    let f = p.fetch.iter().find(|f| f.opt_in)?;
    Some(format!(
        "Licence for {}: {D1_LICENCE}\n  Read it here before you say yes: {}\n",
        f.name,
        f.link
            .as_deref()
            .unwrap_or("https://huggingface.co/LiquidAI/d1-2B-GGUF")
    ))
}

fn ollama_help(m: &Machine) -> String {
    if m.ollama.binary.is_some() {
        "Ollama is installed but not running. Open Ollama (or run `ollama serve`), then run setup again.".into()
    } else {
        format!("Ollama is not installed. Install it from {OLLAMA_PAGE} (one installer), open it once, then run setup again. Tempo never installs it for you.")
    }
}

fn wait_for_ollama(host: &dyn Host, port: u16) -> bool {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if probe::probe_ollama(host, port).running {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Make an Ollama answer when none does: the installed one, Tempo's own copy when it is already
/// here, or (only with a yes) Tempo's own copy fetched first. Returns whether an Ollama answers
/// now, the exit code to raise (0 = fine), and what happened for `--json`. Says why when not.
#[allow(clippy::too_many_arguments)]
fn bring_up_ollama(
    ctx: &Ctx,
    host: &dyn Host,
    o: &Opts,
    machine: &Machine,
    runtime_plan: &Option<RuntimePlan>,
    port: u16,
    now: u64,
    say: &mut dyn FnMut(&str),
    confirm: &mut dyn FnMut(&str) -> bool,
    start_ollama: &mut dyn FnMut(&Path) -> bool,
) -> (bool, i32, J) {
    let result = |what: &str, binary: Option<&Path>| {
        let mut r = vec![("result".to_string(), J::s(what))];
        if let Some(b) = binary {
            r.push(("binary".into(), J::s(&b.to_string_lossy())));
        }
        J::Obj(r)
    };
    let start_and_wait = |bin: &Path, start: &mut dyn FnMut(&Path) -> bool| {
        start(bin) && wait_for_ollama(host, port)
    };
    let owned = match runtime_plan {
        None => {
            // the installed Ollama
            if let Some(bin) = &machine.ollama.binary {
                say("Starting Ollama...");
                if start_and_wait(bin, start_ollama) {
                    return (true, 0, J::Null);
                }
            }
            say(&format!("\n{}", ollama_help(machine)));
            return (false, 1, J::Null);
        }
        Some(RuntimePlan::Unsupported(why)) => {
            say(&format!("\n{why}"));
            return (false, 1, result("unsupported", None));
        }
        Some(RuntimePlan::Installed(own)) => own.clone(),
        Some(RuntimePlan::Missing(offer)) => {
            if let Some(b) = &offer.blocker {
                say(&format!("\n{b}"));
                return (false, 1, result("blocked", None));
            }
            if offer.enough_disk == Some(false) {
                say(&format!(
                    "\nNot enough free disk for Tempo's own copy of Ollama: it needs about {:.1} GB ({} times the {:.2} GB download, because unpacking needs room){} and {:.1} GB is free. Free some space, then run setup again.",
                    offer.need_gb,
                    runtime::DISK_FACTOR,
                    offer.pin.bytes as f64 / probe::GIB,
                    if offer.models_gb > 0.0 {
                        format!(
                            ", plus {:.1} GB for the models and {:.0} GB kept free,",
                            offer.models_gb,
                            plan::DISK_MARGIN_GB
                        )
                    } else {
                        String::new()
                    },
                    offer.disk_free_gb.unwrap_or(0.0)
                ));
                return (false, 1, result("no-disk", None));
            }
            let Some(fetcher) = host.fetcher() else {
                say("\nTempo needs curl to fetch its own copy of Ollama, and curl was not found on this computer. Install Ollama from https://ollama.com/download, then run setup again.");
                return (false, 1, result("no-curl", None));
            };
            let q = format!(
                "\nFetch Tempo's own copy of Ollama {}: {} ({:.2} GB, checked against its pinned SHA-256 before it is unpacked), unpack it into {} and run it only for Tempo, with {} free? [y/N] ",
                offer.version,
                offer.pin.file,
                offer.pin.bytes as f64 / probe::GIB,
                offer.folder.display(),
                offer
                    .disk_free_gb
                    .map_or("unknown disk".into(), |d| format!("{d:.1} GB"))
            );
            if !(o.yes || confirm(&q)) {
                say("Nothing was fetched.");
                return (false, 0, result("declined", None));
            }
            say("");
            match runtime::install(
                &ctx.home,
                host,
                fetcher.as_ref(),
                &host.runtime_pins(),
                &offer.pin,
                now,
                say,
            ) {
                Ok(own) => own,
                Err(e) => {
                    say(&format!("\n{e}"));
                    return (false, 1, result("failed", None));
                }
            }
        }
    };
    say("Starting Tempo's own copy of Ollama...");
    if start_and_wait(&owned.binary, start_ollama) {
        say(&format!(
            "  Ollama {} answers on port {port}.",
            owned.version
        ));
        return (true, 0, result("started", Some(&owned.binary)));
    }
    let log = if port == crate::OLLAMA.port {
        "ollama".to_string()
    } else {
        format!("ollama-{port}")
    };
    say(&format!(
        "\nTempo's own copy of Ollama did not answer. Its log is {}",
        ctx.log_path(&log).display()
    ));
    (false, 1, result("not-answering", Some(&owned.binary)))
}

/// Run `tempo-launch setup`. Returns the exit code.
#[allow(clippy::too_many_arguments)]
pub fn run(
    ctx: &Ctx,
    host: &dyn Host,
    o: &Opts,
    port: u16,
    now: u64,
    out: &mut dyn Write,
    confirm: &mut dyn FnMut(&str) -> bool,
    start_ollama: &mut dyn FnMut(&Path) -> bool,
) -> i32 {
    if o.apply_proposal {
        return super::propose::apply_proposal(
            ctx,
            super::propose::real_pinned(),
            now,
            out,
            confirm,
        );
    }
    if o.voice {
        let f = host.fetcher();
        return super::voice::run(
            ctx,
            host,
            &super::voice::Catalog::embedded(),
            f.as_deref(),
            port,
            now,
            o.piper,
            o.multilingual,
            o.yes,
            out,
            confirm,
        );
    }
    if o.ollama {
        return run_ollama_only(ctx, host, o, port, now, out, confirm, start_ollama);
    }
    let c = match compute(ctx, host, o, true, port, now) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tempo-launch: setup: {e}");
            return 2;
        }
    };
    let mut say = |s: &str| {
        if o.json {
            eprintln!("{s}");
        } else {
            let _ = writeln!(out, "{s}");
        }
    };
    if let Some(w) = &c.warning {
        say(&format!("Warning: {w}"));
    }
    if o.page {
        match super::write_page(ctx, &c.plan, o) {
            Some(p) => say(&format!("Wrote the plan to {}", p.display())),
            None => {
                say("Could not write the setup page.");
                return 1;
            }
        }
        return 0;
    }
    if !o.json {
        say(plan::render(&c.plan).trim_end());
    }
    let mut report = plan_json(&c);
    let mut code = 0;
    let mut pulled: Vec<J> = Vec::new();
    let mut wrote = J::Null;
    let mut runtime_result = J::Null;
    if !o.apply {
        say(&format!(
            "\nNothing was fetched or changed. To go ahead: {}",
            apply_command(o)
        ));
    } else if let Some(r) = &c.plan.refused {
        say(&format!("\nStopped: {r}"));
        code = 1;
    } else {
        let mut fetched_ok: Vec<String> = Vec::new();
        let mut fetch_now = !c.plan.fetch.is_empty();
        if !c.machine.ollama.running && (fetch_now || c.plan.runtime.is_some()) {
            let (up, failed, result) = bring_up_ollama(
                ctx,
                host,
                o,
                &c.machine,
                &c.plan.runtime,
                port,
                now,
                &mut say,
                confirm,
                start_ollama,
            );
            code = code.max(failed);
            runtime_result = result;
            fetch_now = fetch_now && up;
        }
        if fetch_now {
            if let Some(b) = d1_block(&c.plan) {
                say(&format!("\n{}", b.trim_end()));
            }
            let unknown = if c.plan.unknown_sizes.is_empty() {
                String::new()
            } else {
                format!(" plus {} of unknown size", c.plan.unknown_sizes.join(", "))
            };
            let q = format!(
                "\nFetch {} model(s), {:.1} GB{unknown}, with {} free? [y/N] ",
                c.plan.fetch.len(),
                c.plan.fetch_gb,
                c.plan
                    .disk_free_gb
                    .map_or("unknown disk".into(), |d| format!("{d:.1} GB"))
            );
            if o.yes || confirm(&q) {
                for f in &c.plan.fetch {
                    say(&format!("Fetching {} ...", f.name));
                    let res = pull(port, &f.name, &mut |l| say(&format!("  {l}")));
                    match res {
                        Ok(()) => {
                            let digest = digest_of(port, &f.name);
                            say(&format!(
                                "  done{}",
                                digest
                                    .as_ref()
                                    .map_or(String::new(), |d| format!(" (digest {d})"))
                            ));
                            fetched_ok.push(f.name.clone());
                            pulled.push(J::Obj(vec![
                                ("name".into(), J::s(&f.name)),
                                ("digest".into(), digest.as_deref().map_or(J::Null, J::s)),
                            ]));
                        }
                        Err(e) => {
                            say(&format!("  could not fetch {}: {e}", f.name));
                            if f.opt_in {
                                say("  d1 may need its own runtime instead of Ollama; everything else is unaffected.");
                            }
                            code = 1;
                        }
                    }
                }
                if !pulled.is_empty() {
                    record_applied(&ctx.home, &pulled, now);
                }
            } else {
                say("Nothing was fetched.");
            }
        }
        let usable = |k: &Pick| k.status == Status::Installed || fetched_ok.contains(&k.model);
        let values = config_values(&c.plan, &usable, port);
        if values.is_empty() {
            say("No model is installed yet, so the agent's settings were not changed.");
        } else if let Some(pinned) = pinned_agent_settings() {
            say(&format!(
                "TEMPO_AGENT_CONFIG points at {}, and that file always wins, so setup left the agent's settings alone. To use this plan, put these in it:",
                pinned.display()
            ));
            // JSON lines, ready to paste into that object
            for (i, (k, v)) in values.iter().enumerate() {
                let comma = if i + 1 < values.len() { "," } else { "" };
                say(&format!("  \"{k}\": {}{comma}", v.pretty().trim_end()));
            }
            wrote = J::Obj(vec![
                ("path".into(), J::s(&pinned.to_string_lossy())),
                ("changed".into(), J::Bool(false)),
                ("pinned".into(), J::Bool(true)),
                ("set".into(), J::Arr(Vec::new())),
                (
                    "kept".into(),
                    J::Arr(values.iter().map(|(k, _)| J::s(k)).collect()),
                ),
            ]);
        } else {
            match merge_config(&ctx.home, &ctx.agent_data, &values, now) {
                Ok(w) => {
                    if w.changed {
                        say(&format!(
                            "Wrote the agent's settings: {} ({}).",
                            w.path.display(),
                            w.set.join(", ")
                        ));
                        if let Some(t) = &w.trashed {
                            say(&format!("  The old file was kept in {}", t.display()));
                        }
                    } else {
                        say("The agent's settings are already up to date.");
                    }
                    if !w.kept.is_empty() {
                        say(&format!(
                            "  Kept your own setting for: {}",
                            w.kept.join(", ")
                        ));
                    }
                    wrote = J::Obj(vec![
                        ("path".into(), J::s(&w.path.to_string_lossy())),
                        ("changed".into(), J::Bool(w.changed)),
                        ("set".into(), J::strings(&w.set)),
                        ("kept".into(), J::strings(&w.kept)),
                    ]);
                }
                Err(e) => {
                    say(&format!("Could not write the agent's settings: {e}"));
                    code = 1;
                }
            }
        }
    }
    if o.json {
        report.set("applied", J::Bool(o.apply));
        report.set("ollama_runtime_result", runtime_result);
        report.set("pulled", J::Arr(pulled));
        report.set("config", wrote);
        let _ = write!(out, "{}", report.pretty());
    }
    code
}

/// `setup --ollama`: only Ollama. Looks at the computer (no registry, no models), and when no
/// Ollama answers, starts the installed one, or fetches (checked against the pinned SHA-256) and
/// starts Tempo's own. Never pulls a model and never writes the agent's settings.
#[allow(clippy::too_many_arguments)]
fn run_ollama_only(
    ctx: &Ctx,
    host: &dyn Host,
    o: &Opts,
    port: u16,
    now: u64,
    out: &mut dyn Write,
    confirm: &mut dyn FnMut(&str) -> bool,
    start_ollama: &mut dyn FnMut(&Path) -> bool,
) -> i32 {
    let mut say = |s: &str| {
        let _ = writeln!(out, "{s}");
    };
    let machine = probe::probe(host, &ctx.home, port);
    if machine.ollama.running {
        say("An Ollama already answers on this computer. Nothing was fetched.");
        return 0;
    }
    let rt = runtime::plan_runtime(&machine, host, &ctx.home, &host.runtime_pins(), port);
    let (up, code, _) = bring_up_ollama(
        ctx,
        host,
        o,
        &machine,
        &rt,
        port,
        now,
        &mut say,
        confirm,
        start_ollama,
    );
    if up {
        say("Ollama is ready. No models were fetched.");
    }
    code
}

fn record_applied(home: &Path, pulled: &[J], now: u64) {
    let rec = J::Obj(vec![
        ("at".into(), J::s(&stamp(now))),
        ("pulled".into(), J::Arr(pulled.to_vec())),
    ]);
    let _ = write_atomic(
        &home.join("setup").join("applied.json"),
        rec.pretty().as_bytes(),
    );
}

/// The real `y/N` question on the terminal; anything but y or yes is no (and no terminal is no).
pub fn ask_terminal(q: &str) -> bool {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).is_ok()
        && matches!(s.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::super::probe::tests::Fake;
    use super::*;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-setup-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(root: &Path) -> Ctx {
        Ctx {
            home: root.join("home"),
            agent_data: root.join("home").join("agent"),
            vault: root.join("vault"),
            bin_dirs: vec![],
            browser: false,
            opened: RefCell::new(Vec::new()),
        }
    }

    /// A 16 GB Windows-shaped computer with no GPU and 50 GB free, as the probe sees it.
    fn pc(disk_gb: u64) -> Fake {
        Fake {
            os: "windows",
            arch: "x86_64",
            cpus: 8,
            win_mem: Some((16 << 30, 10 << 30)),
            win_disk: Some(disk_gb << 30),
            ..Fake::default()
        }
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Ok,
        Fail,
        Chunked,
    }

    struct Mock {
        port: u16,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Mock {
        fn pulls(&self) -> Vec<String> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.starts_with("PULL "))
                .cloned()
                .collect()
        }
    }

    /// A mock Ollama: answers version and tags, records every pull request, downloads nothing.
    fn mock(installed: &[&str], mode: Mode) -> Mock {
        serve_mock(
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
            installed,
            mode,
        )
    }

    /// The same mock Ollama on a listener the caller made (so a test can start it late).
    fn serve_mock(l: std::net::TcpListener, installed: &[&str], mode: Mode) -> Mock {
        let port = l.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let have: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(
            installed.iter().map(|s| s.to_string()).collect(),
        ));
        let lg = log.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let (log, have) = (lg.clone(), have.clone());
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (mut head_end, mut want) = (None, 0usize);
                    loop {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if head_end.is_none() {
                            if let Some(e) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                head_end = Some(e + 4);
                                let h = String::from_utf8_lossy(&buf[..e]).to_lowercase();
                                want = h
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse().ok())
                                    .unwrap_or(0);
                            }
                        }
                        if head_end.is_some_and(|h| buf.len() >= h + want) {
                            break;
                        }
                    }
                    let h = head_end.unwrap_or(buf.len());
                    let head = String::from_utf8_lossy(&buf[..h]).into_owned();
                    let body = String::from_utf8_lossy(&buf[h..]).into_owned();
                    let reply = |s: &mut std::net::TcpStream, b: &str| {
                        let _ = write!(
                            s,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                            b.len()
                        );
                    };
                    if head.starts_with("GET /api/version") {
                        reply(&mut s, r#"{"version":"0.6.1"}"#);
                    } else if head.starts_with("GET /api/tags") {
                        let m: Vec<String> = have
                            .lock()
                            .unwrap()
                            .iter()
                            .map(|n| {
                                format!(
                                    r#"{{"name":"{n}","size":1000,"digest":"sha256:fake-{n}"}}"#
                                )
                            })
                            .collect();
                        reply(&mut s, &format!("{{\"models\":[{}]}}", m.join(",")));
                    } else if head.starts_with("POST /api/pull") {
                        log.lock().unwrap().push(format!("PULL {body}"));
                        let name = parse(&body)
                            .ok()
                            .and_then(|j| j.str("model").map(String::from))
                            .unwrap_or_default();
                        let lines = match mode {
                            Mode::Fail => "{\"status\":\"pulling manifest\"}\n{\"error\":\"pull model manifest: file does not exist\"}\n".to_string(),
                            _ => {
                                have.lock().unwrap().push(name);
                                "{\"status\":\"pulling manifest\"}\n{\"status\":\"pulling abc\",\"total\":100,\"completed\":50}\n{\"status\":\"pulling abc\",\"total\":100,\"completed\":100}\n{\"status\":\"success\"}\n".to_string()
                            }
                        };
                        if let Mode::Chunked = mode {
                            let _ = write!(s, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{lines}\r\n0\r\n\r\n", lines.len());
                        } else {
                            let _ = write!(s, "HTTP/1.0 200 OK\r\n\r\n{lines}");
                        }
                    }
                });
            }
        });
        Mock { port, log }
    }

    struct Ran {
        code: i32,
        out: String,
        question: String,
        cfg: Option<String>,
    }

    fn go(root: &Path, host: &Fake, o: &Opts, port: u16, yes: bool) -> Ran {
        let c = ctx(root);
        let mut out = Vec::new();
        let mut question = String::new();
        let code = run(
            &c,
            host,
            o,
            port,
            1_793_000_000,
            &mut out,
            &mut |q| {
                question = q.to_string();
                yes
            },
            &mut |_| false,
        );
        Ran {
            code,
            question,
            out: String::from_utf8_lossy(&out).into_owned(),
            cfg: std::fs::read_to_string(c.agent_data.join("config.json")).ok(),
        }
    }

    fn opts(a: &[&str]) -> Opts {
        parse_args(a).unwrap()
    }

    #[test]
    fn arguments() {
        assert_eq!(opts(&[]), Opts::default());
        let o = opts(&[
            "--apply",
            "--yes",
            "--with-d1",
            "--goal",
            "chat,code",
            "--json",
        ]);
        assert!(o.apply && o.yes && o.with_d1 && o.json);
        assert_eq!(o.goals, vec!["chat,code"]);
        assert_eq!(opts(&["--goal=music"]).goals, vec!["music"]);
        assert!(parse_args(&["--goal"]).is_err());
        assert!(parse_args(&["--yes"]).is_err());
        assert!(opts(&["--page"]).page);
        assert!(parse_args(&["--page", "--apply"]).is_err());
        assert!(parse_args(&["--bogus"]).is_err());
        assert_eq!(
            apply_command(&opts(&["--with-d1", "--goal", "code"])),
            "tempo-launch setup --apply --with-d1 --goal code"
        );
    }

    #[test]
    fn plain_setup_changes_nothing_and_asks_nothing() {
        let root = tmp("plain");
        let m = mock(&[], Mode::Ok);
        let r = go(&root, &pc(50), &opts(&[]), m.port, true);
        assert_eq!(r.code, 0);
        assert!(
            r.out.contains("8 GB")
                || r.out.contains(
                    "16 GB RAM, no GPU: qwen3:4b (2.5 GB) for thinking and code, runs on CPU"
                )
        );
        assert!(r
            .out
            .contains("Nothing was fetched or changed. To go ahead: tempo-launch setup --apply"));
        assert!(m.pulls().is_empty());
        assert!(r.cfg.is_none());
    }

    #[test]
    fn apply_with_yes_pulls_each_missing_model_once_and_writes_settings() {
        let root = tmp("yes");
        let m = mock(&[], Mode::Ok);
        let r = go(&root, &pc(50), &opts(&["--apply", "--yes"]), m.port, false);
        assert_eq!(r.code, 0, "{}", r.out);
        let pulls = m.pulls();
        assert_eq!(pulls.len(), 2, "{pulls:?}");
        assert!(
            pulls[0].contains("\"model\":\"qwen3:4b\"") && pulls[0].contains("\"stream\":true")
        );
        assert!(pulls[1].contains("embeddinggemma"));
        assert!(r.out.contains("Fetching qwen3:4b") && r.out.contains("pulling manifest"));
        assert!(r.out.contains("(digest sha256:fake-qwen3:4b)"));
        let cfg = parse(&r.cfg.unwrap()).unwrap();
        assert_eq!(cfg.strs("models"), vec!["qwen3:4b"]);
        assert_eq!(cfg.str("embed_model"), Some("embeddinggemma"));
        assert_eq!(
            cfg.str("ollama_url"),
            Some(format!("http://127.0.0.1:{}", m.port).as_str())
        );
        assert_eq!(cfg.str("configured_by"), Some("tempo-launch setup"));
        assert!(cfg.str("configured_at").unwrap().ends_with('Z'));
        // the same model for both lanes: no separate fast list
        assert!(cfg.get("fast_models").is_none());
        let applied = std::fs::read_to_string(root.join("home/setup/applied.json")).unwrap();
        assert!(applied.contains("sha256:fake-qwen3:4b"));
    }

    #[test]
    fn re_running_is_safe_nothing_more_is_pulled_and_the_file_is_untouched() {
        let root = tmp("rerun");
        let m = mock(&[], Mode::Ok);
        let o = opts(&["--apply", "--yes"]);
        go(&root, &pc(50), &o, m.port, false);
        let first = std::fs::read_to_string(root.join("home/agent/config.json")).unwrap();
        // the mock now has both models installed, as Ollama would
        let host = {
            let mut h = pc(50);
            h.ollama_bin = None;
            h
        };
        let r = go(&root, &host, &o, m.port, false);
        assert_eq!(r.code, 0);
        assert_eq!(m.pulls().len(), 2, "no new pulls");
        assert!(r.out.contains("already up to date"));
        assert_eq!(r.cfg.unwrap(), first);
        assert!(
            !root.join("home/trash").exists(),
            "an unchanged file is not backed up again"
        );
    }

    #[test]
    fn declining_fetches_nothing_and_writes_no_settings() {
        let root = tmp("no");
        let m = mock(&[], Mode::Ok);
        let r = go(&root, &pc(50), &opts(&["--apply"]), m.port, false);
        assert_eq!(r.code, 0);
        assert!(r.question.contains("Fetch 2 model(s), 2.5 GB plus embeddinggemma of unknown size, with 50.0 GB free? [y/N]"), "{}", r.question);
        assert!(r.out.contains("Nothing was fetched."));
        assert!(m.pulls().is_empty());
        assert!(r.cfg.is_none());
        // saying yes at the question does fetch
        let r = go(&root, &pc(50), &opts(&["--apply"]), m.port, true);
        assert_eq!(m.pulls().len(), 2);
        assert!(r.cfg.is_some());
    }

    #[test]
    fn a_failed_pull_is_reported_and_that_model_is_left_out_of_the_settings() {
        let root = tmp("fail");
        let m = mock(&[], Mode::Fail);
        let r = go(&root, &pc(50), &opts(&["--apply", "--yes"]), m.port, false);
        assert_eq!(r.code, 1);
        assert!(r
            .out
            .contains("could not fetch qwen3:4b: pull model manifest: file does not exist"));
        assert!(r.cfg.is_none(), "nothing usable, so no settings");
        assert!(r.out.contains("No model is installed yet"));
    }

    #[test]
    fn a_chunked_answer_is_understood() {
        let root = tmp("chunk");
        let m = mock(&[], Mode::Chunked);
        let r = go(&root, &pc(50), &opts(&["--apply", "--yes"]), m.port, false);
        assert_eq!(r.code, 0, "{}", r.out);
        assert!(r.cfg.unwrap().contains("qwen3:4b"));
    }

    #[test]
    fn already_installed_models_are_used_without_a_pull() {
        let root = tmp("have");
        let m = mock(&["qwen3:4b", "embeddinggemma:latest"], Mode::Ok);
        let r = go(&root, &pc(50), &opts(&["--apply"]), m.port, false);
        assert_eq!(r.code, 0);
        assert!(m.pulls().is_empty());
        assert!(r
            .out
            .contains("Nothing: everything chosen is already here."));
        let cfg = parse(&r.cfg.unwrap()).unwrap();
        assert_eq!(cfg.strs("models"), vec!["qwen3:4b"]);
        assert_eq!(cfg.str("embed_model"), Some("embeddinggemma:latest"));
    }

    #[test]
    fn ollama_missing_says_where_to_get_it_and_fetches_nothing() {
        let root = tmp("noollama");
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let r = go(&root, &pc(50), &opts(&["--apply", "--yes"]), port, true);
        assert_eq!(r.code, 1);
        // Tempo can fetch its own Ollama, but without curl it cannot: it says where to get one
        assert!(r
            .out
            .contains("Tempo needs curl to fetch its own copy of Ollama"));
        assert!(r
            .out
            .contains("Install Ollama from https://ollama.com/download"));
        assert!(r.cfg.is_none());
        assert!(!root.join("home/runtime").exists(), "nothing was fetched");
        // installed but not running: it says to open it (the test starter does not start it)
        let mut h = pc(50);
        h.ollama_bin = Some(PathBuf::from("/x/ollama"));
        let r = go(&root, &h, &opts(&["--apply", "--yes"]), port, true);
        assert!(r.out.contains("installed but not running"));
    }

    #[test]
    fn disk_too_small_stops_before_any_pull() {
        let root = tmp("disk");
        let m = mock(&[], Mode::Ok);
        let r = go(&root, &pc(2), &opts(&["--apply", "--yes"]), m.port, true);
        assert_eq!(r.code, 1);
        assert!(r.out.contains("Stopped: Not enough free disk"));
        assert!(m.pulls().is_empty() && r.cfg.is_none());
    }

    #[test]
    fn d1_shows_its_licence_and_link_before_the_pull_and_is_opt_in() {
        let root = tmp("d1");
        let m = mock(&[], Mode::Ok);
        let r = go(
            &root,
            &pc(50),
            &opts(&["--apply", "--yes", "--with-d1"]),
            m.port,
            false,
        );
        assert_eq!(r.code, 0, "{}", r.out);
        let lic = r.out.find("USD 10 million").unwrap();
        let link = r.out.find("https://huggingface.co/LiquidAI/d1-2B-GGUF/blob/4a181094d1b6addfaa3a0956f78e7ab2aa406a0c/LICENSE").unwrap();
        let fetching = r
            .out
            .find("Fetching hf.co/LiquidAI/d1-2B-GGUF:Q4_K_M")
            .unwrap();
        assert!(lic < fetching && link < fetching);
        assert!(m
            .pulls()
            .iter()
            .any(|p| p.contains("hf.co/LiquidAI/d1-2B-GGUF:Q4_K_M")));
        // without the flag d1 is never pulled
        let root = tmp("nod1");
        let m = mock(&[], Mode::Ok);
        go(&root, &pc(50), &opts(&["--apply", "--yes"]), m.port, false);
        assert!(m.pulls().iter().all(|p| !p.contains("d1")));
    }

    #[test]
    fn json_output_is_one_parsable_object() {
        let root = tmp("json");
        let m = mock(&[], Mode::Ok);
        let r = go(&root, &pc(50), &opts(&["--json"]), m.port, false);
        let j = parse(&r.out).unwrap();
        assert_eq!(j.get("applied"), Some(&J::Bool(false)));
        assert_eq!(j.arr("picks").len(), 3);
        assert_eq!(j.arr("fetch").len(), 2);
        assert_eq!(j.get("machine").unwrap().f64("ram_total_gb"), Some(16.0));
    }

    #[test]
    fn a_bad_goal_is_a_usage_error() {
        let root = tmp("badgoal");
        let r = go(&root, &pc(50), &opts(&["--goal", "nonsense"]), 1, false);
        assert_eq!(r.code, 2);
    }

    // ---- the config merge

    fn vals(models: &str) -> Vec<(&'static str, J)> {
        vec![
            ("models", J::strings(&[models.to_string()])),
            ("embed_model", J::s("embeddinggemma")),
            ("ollama_url", J::s("http://127.0.0.1:11434")),
        ]
    }

    #[test]
    fn merge_never_overwrites_a_user_key_and_backs_the_old_file_up_to_trash() {
        let root = tmp("merge");
        let (home, data) = (root.join("home"), root.join("home/agent"));
        std::fs::create_dir_all(&data).unwrap();
        let old = r#"{"models":["my-own:7b"],"temperature":0.9,"fetch_allow_hosts":["a.example"]}"#;
        std::fs::write(data.join("config.json"), old).unwrap();
        let w = merge_config(&home, &data, &vals("qwen3:4b"), 1_793_000_000).unwrap();
        assert!(w.changed);
        assert_eq!(w.kept, vec!["models"]);
        assert_eq!(w.set, vec!["embed_model", "ollama_url"]);
        let cfg = parse(&std::fs::read_to_string(data.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg.strs("models"), vec!["my-own:7b"]);
        assert_eq!(cfg.f64("temperature"), Some(0.9));
        assert_eq!(cfg.strs("fetch_allow_hosts"), vec!["a.example"]);
        assert_eq!(cfg.str("embed_model"), Some("embeddinggemma"));
        // the old file is in trash/<date>/, byte for byte
        let t = w.trashed.unwrap();
        assert!(t.starts_with(home.join("trash")) && t.ends_with("config.json"));
        assert_eq!(std::fs::read_to_string(&t).unwrap(), old);
        // a second run changes nothing and makes no second backup
        let w2 = merge_config(&home, &data, &vals("qwen3:4b"), 1_793_000_100).unwrap();
        assert!(!w2.changed && w2.trashed.is_none());
        assert_eq!(std::fs::read_dir(t.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn what_setup_wrote_is_refreshed_unless_the_user_changed_it() {
        let root = tmp("refresh");
        let (home, data) = (root.join("home"), root.join("home/agent"));
        merge_config(&home, &data, &vals("qwen3:4b"), 1_793_000_000).unwrap();
        // a better model next time: setup's own key is refreshed
        let w = merge_config(&home, &data, &vals("better:8b"), 1_793_086_400).unwrap();
        assert_eq!(w.set, vec!["models"]);
        let p = data.join("config.json");
        assert!(std::fs::read_to_string(&p).unwrap().contains("better:8b"));
        // the user edits it: from then on it is theirs
        let text = std::fs::read_to_string(&p)
            .unwrap()
            .replacen("better:8b", "mine:1b", 1);
        std::fs::write(&p, text).unwrap();
        let w = merge_config(&home, &data, &vals("best:9b"), 1_793_172_800).unwrap();
        assert_eq!(w.kept, vec!["models"]);
        assert!(std::fs::read_to_string(&p).unwrap().contains("mine:1b"));
        // one backup (the refresh); the last run changed nothing, so it made none
        assert_eq!(std::fs::read_dir(home.join("trash")).unwrap().count(), 1);
    }

    #[test]
    fn an_older_agent_config_json_is_the_file_setup_fills_because_the_agent_reads_it_first() {
        let root = tmp("older");
        let (home, data) = (root.join("home"), root.join("home/agent"));
        std::fs::create_dir_all(&data).unwrap();
        assert_eq!(agent_settings_path(&data), data.join("config.json"));
        let old = r#"{"temperature":0.5,"command_deny":["rm"]}"#;
        std::fs::write(data.join("agent-config.json"), old).unwrap();
        assert_eq!(agent_settings_path(&data), data.join("agent-config.json"));
        let w = merge_config(&home, &data, &vals("qwen3:4b"), 1_793_000_000).unwrap();
        assert!(w.changed);
        assert_eq!(w.path, data.join("agent-config.json"));
        // no config.json next to it that the agent would never read
        assert!(!data.join("config.json").exists());
        let cfg = parse(&std::fs::read_to_string(data.join("agent-config.json")).unwrap()).unwrap();
        assert_eq!(cfg.strs("models"), vec!["qwen3:4b"]);
        assert_eq!(cfg.f64("temperature"), Some(0.5));
        assert_eq!(cfg.strs("command_deny"), vec!["rm"]);
        // the backup keeps the file's own name, byte for byte
        let t = w.trashed.unwrap();
        assert!(t.ends_with("agent-config.json"), "{}", t.display());
        assert_eq!(std::fs::read_to_string(&t).unwrap(), old);
        // a second backup the same day is named after it too
        let w = merge_config(&home, &data, &vals("better:8b"), 1_793_000_100).unwrap();
        let t2 = w.trashed.unwrap();
        let n = t2.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            n.starts_with("agent-config-") && n.ends_with(".json"),
            "{n}"
        );
    }

    #[test]
    fn a_pinned_settings_file_is_named_only_by_a_non_empty_variable() {
        assert_eq!(pinned_from(None), None);
        assert_eq!(pinned_from(Some("".into())), None);
        assert_eq!(
            pinned_from(Some("/srv/agent.json".into())),
            Some(PathBuf::from("/srv/agent.json"))
        );
    }

    #[test]
    fn a_second_backup_the_same_day_gets_its_own_name() {
        let root = tmp("sameday");
        let (home, data) = (root.join("home"), root.join("home/agent"));
        merge_config(&home, &data, &vals("a:1b"), 1_793_000_000).unwrap();
        merge_config(&home, &data, &vals("b:1b"), 1_793_000_010).unwrap();
        merge_config(&home, &data, &vals("c:1b"), 1_793_000_020).unwrap();
        let day = std::fs::read_dir(home.join("trash"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read_dir(day).unwrap().count(), 2);
    }

    #[test]
    fn a_broken_config_file_is_left_exactly_as_it_is() {
        let root = tmp("broken");
        let (home, data) = (root.join("home"), root.join("home/agent"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("config.json"), "{ not json").unwrap();
        let e = merge_config(&home, &data, &vals("x:1b"), 1).unwrap_err();
        assert!(e.contains("left it alone"));
        assert_eq!(
            std::fs::read_to_string(data.join("config.json")).unwrap(),
            "{ not json"
        );
        assert!(!home.join("trash").exists());
    }

    #[test]
    fn pull_reports_progress_in_steps_not_every_line() {
        let m = mock(&[], Mode::Ok);
        let mut lines = Vec::new();
        pull(m.port, "qwen3:4b", &mut |l| lines.push(l.to_string())).unwrap();
        assert_eq!(lines[0], "pulling manifest");
        assert!(lines.iter().any(|l| l == "pulling abc 50%"));
        assert!(lines.len() <= 4, "{lines:?}");
        // nothing listening: a clear error
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(pull(port, "x", &mut |_| {})
            .unwrap_err()
            .contains("could not reach Ollama"));
    }

    #[test]
    fn prefer_argument_is_parsed_and_repeated_in_the_apply_command() {
        assert_eq!(opts(&[]).prefer, Prefer::Auto);
        assert_eq!(opts(&["--prefer", "mainstream"]).prefer, Prefer::Mainstream);
        assert_eq!(opts(&["--prefer=preferred"]).prefer, Prefer::Preferred);
        assert_eq!(
            opts(&["--preferred-prefix", "Mine-"])
                .preferred_prefix
                .as_deref(),
            Some("mine-")
        );
        assert_eq!(
            opts(&["--preferred-prefix="]).preferred_prefix.as_deref(),
            Some("")
        );
        assert!(parse_args(&["--preferred-prefix", "bad prefix!"]).is_err());
        assert!(parse_args(&["--prefer"]).is_err());
        assert!(parse_args(&["--prefer", "both"]).is_err());
        assert_eq!(
            apply_command(&opts(&["--prefer", "mainstream"])),
            "tempo-launch setup --apply --prefer mainstream"
        );
        assert_eq!(apply_command(&opts(&[])), "tempo-launch setup --apply");
    }

    fn with_registry(host: Fake) -> Fake {
        use super::super::registry::tests::{table, MockReg};
        Fake {
            fetcher: Some(Arc::new(MockReg::new(&table()))),
            ..host
        }
    }

    #[test]
    fn setup_without_curl_still_plans_from_the_shipped_entries_and_says_so() {
        let root = tmp("nocurl");
        let m = mock(&[], Mode::Ok);
        let r = go(&root, &pc(50), &opts(&[]), m.port, false);
        assert_eq!(r.code, 0);
        assert!(r.out.contains("curl was not found"), "{}", r.out);
        assert!(r.out.contains("qwen3:4b (2.5 GB)"));
        assert!(r.out.contains("From the verified catalog, not re-checked"));
        assert!(m.pulls().is_empty());
    }

    #[test]
    fn setup_with_the_registry_shows_verified_size_licence_and_proof_in_text_and_json() {
        let root = tmp("reg");
        let m = mock(&[], Mode::Ok);
        let host = with_registry(pc(50));
        let r = go(&root, &host, &opts(&[]), m.port, false);
        assert_eq!(r.code, 0, "{}", r.out);
        assert!(
            r.out.contains("qwen3:8b (5.2 GB) for thinking and code"),
            "{}",
            r.out
        );
        assert!(r
            .out
            .contains("Licence: Apache License 2.0. Verified with the Ollama registry today."));
        assert!(r.out.contains("qwen3:8b (5.2 GB, licence: Apache License 2.0; Verified with the Ollama registry today)"));
        assert!(!r.out.contains("curl was not found"));
        // the second run is from the cache, even with no registry at all
        let r2 = go(&root, &pc(50), &opts(&[]), m.port, false);
        assert!(
            r2.out
                .contains("From the cache, 0 days old (verified with the Ollama registry)"),
            "{}",
            r2.out
        );
        assert!(!r2.out.contains("curl was not found"));
        // --json carries the same
        let r3 = go(&root, &host, &opts(&["--json"]), m.port, false);
        let j = parse(&r3.out).unwrap();
        let heavy = j
            .arr("picks")
            .iter()
            .find(|k| k.str("role") == Some("heavy"))
            .unwrap();
        assert_eq!(heavy.str("licence"), Some("Apache License 2.0"));
        assert!(heavy.str("verified").unwrap().contains("From the cache"));
        assert!(heavy
            .str("why")
            .unwrap()
            .contains("Licence: Apache License 2.0"));
        let f = j
            .arr("fetch")
            .iter()
            .find(|f| f.str("name") == Some("qwen3:8b"))
            .unwrap();
        assert_eq!(f.str("licence"), Some("Apache License 2.0"));
        assert!(f.f64("gb").unwrap() > 5.0);
        let reg = j.get("registry").unwrap();
        assert_eq!(reg.f64("curl_missing"), None);
        assert_eq!(reg.get("curl_missing"), Some(&J::Bool(false)));
        assert!(reg.f64("from_cache").unwrap() > 30.0);
    }

    #[test]
    fn mainstream_flag_overrides_an_installed_preferred_model_end_to_end() {
        let root = tmp("unc");
        let m = mock(&["mine-sample-8b:latest"], Mode::Ok);
        let host = with_registry(pc(50));
        // no prefix set: an installed model is just an installed model
        let none = go(&root, &host, &opts(&[]), m.port, false);
        assert!(
            !none.out.contains("one of your preferred models"),
            "{}",
            none.out
        );
        // the owner sets it once: it is saved for this computer and used at once
        let auto = go(
            &root,
            &host,
            &opts(&["--preferred-prefix", "mine-"]),
            m.port,
            false,
        );
        assert!(
            auto.out.contains("mine-sample-8b:latest")
                && auto.out.contains("one of your preferred models"),
            "{}",
            auto.out
        );
        let saved = std::fs::read_to_string(ctx(&root).home.join("setup.json")).unwrap();
        assert_eq!(
            parse(&saved).unwrap().str("preferred_prefix"),
            Some("mine-")
        );
        let main = go(
            &root,
            &host,
            &opts(&["--prefer", "mainstream", "--preferred-prefix", "mine-"]),
            m.port,
            false,
        );
        assert!(
            main.out.contains("qwen3:8b")
                && main.out.contains("left out because of --prefer mainstream"),
            "{}",
            main.out
        );
    }

    // ---- Tempo's own copy of Ollama

    use super::super::runtime::tests::{test_pins, FakeDl, CONTENT};

    /// A Windows-shaped computer with no Ollama, pretend pins, and a pretend download.
    fn bare(disk_gb: u64, dl: FakeDl) -> (Fake, Arc<FakeDl>) {
        let dl = Arc::new(dl);
        let host = Fake {
            tar_makes: Some("ollama.exe"),
            pins: Some(test_pins()),
            fetcher: Some(dl.clone()),
            ..pc(disk_gb)
        };
        (host, dl)
    }

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// `go`, with the start-Ollama callback chosen by the test.
    fn go_with(
        root: &Path,
        host: &Fake,
        o: &Opts,
        port: u16,
        yes: bool,
        start: &mut dyn FnMut(&Path) -> bool,
    ) -> Ran {
        let c = ctx(root);
        let mut out = Vec::new();
        let mut question = String::new();
        let code = run(
            &c,
            host,
            o,
            port,
            1_793_000_000,
            &mut out,
            &mut |q| {
                question.push_str(q);
                yes
            },
            start,
        );
        Ran {
            code,
            question,
            out: String::from_utf8_lossy(&out).into_owned(),
            cfg: std::fs::read_to_string(c.agent_data.join("config.json")).ok(),
        }
    }

    /// Starting the program "starts" a mock Ollama on `port`, as the real one would.
    fn starts_a_mock_on<'a>(
        port: u16,
        started: &'a Mutex<Vec<PathBuf>>,
        pulls: &'a Mutex<Vec<Mock>>,
    ) -> impl FnMut(&Path) -> bool + 'a {
        move |bin| {
            started.lock().unwrap().push(bin.to_path_buf());
            let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            pulls.lock().unwrap().push(serve_mock(l, &[], Mode::Ok));
            true
        }
    }

    #[test]
    fn without_any_ollama_the_plan_shows_the_exact_file_version_size_and_disk_in_text_json_and_page(
    ) {
        let root = tmp("own-plan");
        let (host, dl) = bare(50, FakeDl::new());
        let port = free_port();
        let r = go(&root, &host, &opts(&[]), port, true);
        assert_eq!(r.code, 0, "{}", r.out);
        for must in [
            "Ollama (the program that runs the models)",
            "No Ollama answers at http://127.0.0.1:",
            "ollama-windows-amd64.zip (Ollama 0.40.6,",
            "From: https://example.test/ollama/v0.40.6/ollama-windows-amd64.zip",
            &format!("SHA-256 {}", test_pins().files[0].sha256),
            "Free disk: 50.0 GB. It needs about 0.0 GB (2.5 times the download",
            "Nothing is fetched unless you run setup --apply and say yes",
        ] {
            assert!(r.out.contains(must), "{must}\n{}", r.out);
        }
        // plain setup fetches and writes nothing
        assert_eq!(dl.count(), 0);
        assert!(!root.join("home/runtime").exists());
        // --json carries the same facts
        let r = go(&root, &host, &opts(&["--json"]), port, true);
        let j = parse(&r.out).unwrap();
        let rt = j.get("ollama_runtime").unwrap();
        assert_eq!(rt.str("state"), Some("missing"));
        assert_eq!(rt.str("version"), Some("0.40.6"));
        assert_eq!(rt.str("file"), Some("ollama-windows-amd64.zip"));
        assert_eq!(rt.str("sha256"), Some(test_pins().files[0].sha256.as_str()));
        assert_eq!(rt.f64("bytes"), Some(CONTENT.len() as f64));
        assert_eq!(rt.f64("disk_free_gb"), Some(50.0));
        assert_eq!(rt.get("enough_disk"), Some(&J::Bool(true)));
        // the page shows every line the terminal plan shows
        let c = compute(&ctx(&root), &host, &opts(&[]), false, port, 1_793_000_000).unwrap();
        let page = super::super::page::setup_page(&c.plan, "tempo-launch setup --apply");
        let lines = c.plan.runtime.as_ref().unwrap().lines();
        assert!(lines.len() >= 6);
        for l in lines {
            assert!(page.contains(&crate::html_escape(&l)), "{l}");
        }
        assert!(page.contains("<h2>Ollama</h2>"));
    }

    #[test]
    fn an_unsupported_computer_is_told_so_in_the_plan_and_apply_stops() {
        let root = tmp("own-unsup");
        let host = Fake {
            os: "freebsd",
            arch: "x86_64",
            cpus: 4,
            ..Fake::default()
        };
        let port = free_port();
        let r = go(&root, &host, &opts(&[]), port, true);
        assert!(
            r.out
                .contains("Tempo has no Ollama to fetch for freebsd on x86_64"),
            "{}",
            r.out
        );
        let r = go(&root, &host, &opts(&["--apply", "--yes"]), port, true);
        assert_eq!(r.code, 1);
        assert!(
            r.out.contains("Install Ollama from ollama.com"),
            "{}",
            r.out
        );
        assert!(!root.join("home/runtime").exists());
    }

    #[test]
    fn an_ollama_that_answers_or_is_installed_means_no_runtime_is_offered_or_touched() {
        // one answers: nothing about Tempo's own copy, and a record of one is not started
        let root = tmp("own-foreign");
        let m = mock(&["qwen3:4b", "embeddinggemma:latest"], Mode::Ok);
        let (host, dl) = bare(50, FakeDl::new());
        let own = root.join("home/runtime/ollama-0.40.6");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("ollama.exe"), "x").unwrap();
        std::fs::write(
            root.join("home/runtime/ollama.json"),
            format!(
                "{{\"purpose\":\"owned-ollama-runtime\",\"binary\":{:?}}}",
                own.join("ollama.exe").to_string_lossy()
            ),
        )
        .unwrap();
        let started = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--apply", "--yes"]),
            m.port,
            true,
            &mut |b| {
                started.lock().unwrap().push(b.to_path_buf());
                true
            },
        );
        assert_eq!(r.code, 0, "{}", r.out);
        assert!(!r.out.contains("Tempo can fetch its own copy"), "{}", r.out);
        assert!(
            started.lock().unwrap().is_empty(),
            "a running Ollama is never replaced"
        );
        assert_eq!(dl.count(), 0);
        let j = parse(&go(&root, &host, &opts(&["--json"]), m.port, true).out).unwrap();
        assert_eq!(j.get("ollama_runtime"), Some(&J::Null));
        // one is installed (not running): it is the one started, nothing is offered
        let root = tmp("own-sysbin");
        let (mut host, dl) = bare(50, FakeDl::new());
        host.ollama_bin = Some(PathBuf::from("/usr/bin/ollama"));
        let r = go(&root, &host, &opts(&[]), free_port(), true);
        assert!(!r.out.contains("Tempo can fetch its own copy"), "{}", r.out);
        assert_eq!(dl.count(), 0);
    }

    #[test]
    fn consent_no_asks_the_question_with_the_facts_and_fetches_nothing() {
        let root = tmp("own-no");
        let (host, dl) = bare(50, FakeDl::new());
        let port = free_port();
        let started = Mutex::new(Vec::new());
        let keep = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--apply"]),
            port,
            false,
            &mut starts_a_mock_on(port, &started, &keep),
        );
        assert_eq!(r.code, 0, "{}", r.out);
        assert!(
            r.question.contains("Fetch Tempo's own copy of Ollama 0.40.6: ollama-windows-amd64.zip (0.00 GB, checked against its pinned SHA-256 before it is unpacked), unpack it into")
                && r.question.contains("and run it only for Tempo, with 50.0 GB free? [y/N] "),
            "{}",
            r.question
        );
        assert!(r.out.contains("Nothing was fetched."));
        assert_eq!(dl.count(), 0);
        assert!(started.lock().unwrap().is_empty());
        assert!(!root.join("home/runtime").exists());
        assert!(r.cfg.is_none());
        // setup without --apply never asks at all
        let r = go(&root, &host, &opts(&[]), port, true);
        assert!(r.question.is_empty());
        assert_eq!(dl.count(), 0);
    }

    #[test]
    fn consent_yes_fetches_checks_unpacks_records_starts_it_and_then_the_models_pull_from_it() {
        let root = tmp("own-yes");
        let (host, dl) = bare(50, FakeDl::new());
        let port = free_port();
        let started = Mutex::new(Vec::new());
        let keep = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--apply"]),
            port,
            true,
            &mut starts_a_mock_on(port, &started, &keep),
        );
        assert_eq!(r.code, 0, "{}", r.out);
        // the Ollama question came first, the model question after it
        let first = r.question.find("Fetch Tempo's own copy of Ollama").unwrap();
        let second = r.question.find("model(s)").unwrap();
        assert!(first < second, "{}", r.question);
        assert_eq!(dl.count(), 1);
        let home = root.join("home");
        let bin = home
            .join("runtime")
            .join("ollama-0.40.6")
            .join("ollama.exe");
        assert!(bin.is_file());
        // started right after unpacking, with the program from the record
        assert_eq!(*started.lock().unwrap(), vec![bin.clone()]);
        assert!(
            r.out.contains("Starting Tempo's own copy of Ollama"),
            "{}",
            r.out
        );
        // the record
        let rec =
            parse(&std::fs::read_to_string(home.join("runtime/ollama.json")).unwrap()).unwrap();
        assert_eq!(rec.str("purpose"), Some("owned-ollama-runtime"));
        assert_eq!(rec.str("binary"), Some(bin.to_str().unwrap()));
        // and the models came through it
        assert!(r.out.contains("Fetching qwen3:4b"), "{}", r.out);
        assert!(parse(&r.cfg.unwrap())
            .unwrap()
            .strs("models")
            .contains(&"qwen3:4b".to_string()));
        // the order in the output: checked, unpacked, started, then models
        let at = |s: &str| r.out.find(s).unwrap_or_else(|| panic!("{s}\n{}", r.out));
        assert!(at("SHA-256 matches the pin.") < at("Unpacking into"));
        assert!(at("Unpacking into") < at("Starting Tempo's own copy of Ollama"));
        assert!(at("Starting Tempo's own copy of Ollama") < at("Fetching qwen3:4b"));
    }

    #[test]
    fn dash_dash_yes_asks_nothing_and_json_reports_what_happened() {
        let root = tmp("own-json");
        let (host, dl) = bare(50, FakeDl::new());
        let port = free_port();
        let started = Mutex::new(Vec::new());
        let keep = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--apply", "--yes", "--json"]),
            port,
            false,
            &mut starts_a_mock_on(port, &started, &keep),
        );
        assert_eq!(r.code, 0);
        assert!(r.question.is_empty(), "{}", r.question);
        assert_eq!(dl.count(), 1);
        let j = parse(&r.out).unwrap();
        let res = j.get("ollama_runtime_result").unwrap();
        assert_eq!(res.str("result"), Some("started"));
        assert!(res.str("binary").unwrap().ends_with("ollama.exe"));
    }

    #[test]
    fn ollama_only_fetches_and_starts_ollama_and_never_touches_models_or_settings() {
        let root = tmp("own-only");
        let (host, dl) = bare(50, FakeDl::new());
        let port = free_port();
        let started = Mutex::new(Vec::new());
        let keep = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--ollama", "--yes"]),
            port,
            false,
            &mut starts_a_mock_on(port, &started, &keep),
        );
        assert_eq!(r.code, 0, "{}", r.out);
        assert!(r.question.is_empty(), "{}", r.question);
        assert_eq!(dl.count(), 1);
        assert!(root.join("home/runtime/ollama-0.40.6/ollama.exe").is_file());
        assert!(r.out.contains("SHA-256 matches the pin."), "{}", r.out);
        assert!(r.out.contains("No models were fetched."), "{}", r.out);
        assert!(!r.out.contains("Fetching qwen"), "{}", r.out);
        assert!(r.cfg.is_none(), "no settings written");
        assert!(!root.join("home/setup/applied.json").exists());
    }

    #[test]
    fn ollama_only_without_yes_asks_and_declining_fetches_nothing() {
        let root = tmp("own-only-no");
        let (host, dl) = bare(50, FakeDl::new());
        let port = free_port();
        let started = Mutex::new(Vec::new());
        let keep = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--ollama"]),
            port,
            false,
            &mut starts_a_mock_on(port, &started, &keep),
        );
        assert_eq!(r.code, 0, "{}", r.out);
        assert!(r.question.contains("Fetch Tempo's own copy of Ollama"));
        assert_eq!(dl.count(), 0);
        assert!(!root.join("home/runtime").exists());
    }

    #[test]
    fn ollama_only_leaves_an_ollama_that_answers_alone() {
        let root = tmp("own-only-there");
        let m = mock(&["qwen3:4b"], Mode::Ok);
        let (host, dl) = bare(50, FakeDl::new());
        let r = go(&root, &host, &opts(&["--ollama", "--yes"]), m.port, false);
        assert_eq!(r.code, 0, "{}", r.out);
        assert!(r.out.contains("already answers"), "{}", r.out);
        assert_eq!(dl.count(), 0);
        assert!(!root.join("home/runtime").exists());
    }

    #[test]
    fn ollama_flag_cannot_be_mixed_with_the_model_options() {
        for bad in [
            &["--ollama", "--apply"][..],
            &["--ollama", "--page"],
            &["--ollama", "--with-d1"],
            &["--ollama", "--goal", "chat"],
            &["--ollama", "--json"],
        ] {
            assert!(parse_args(bad).is_err(), "{bad:?}");
        }
        assert!(opts(&["--ollama", "--yes"]).ollama);
        assert!(parse_args(&["--yes"]).is_err());
    }

    #[test]
    fn a_wrong_checksum_through_setup_trashes_the_file_and_nothing_runs() {
        let root = tmp("own-sha");
        let (host, dl) = bare(
            50,
            FakeDl {
                body: vec![b'x'; CONTENT.len()],
                ..FakeDl::new()
            },
        );
        let port = free_port();
        let started = Mutex::new(Vec::new());
        let keep = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--apply", "--yes"]),
            port,
            true,
            &mut starts_a_mock_on(port, &started, &keep),
        );
        assert_eq!(r.code, 1);
        assert!(
            r.out.contains("did not match its pinned SHA-256"),
            "{}",
            r.out
        );
        assert!(r.out.contains("so Tempo refused it"), "{}", r.out);
        assert_eq!(dl.count(), 1);
        let moved = root.join("home/trash/20261026/ollama-windows-amd64.zip.part");
        assert_eq!(std::fs::read(moved).unwrap(), vec![b'x'; CONTENT.len()]);
        assert!(host.tools.lock().unwrap().is_empty(), "never unpacked");
        assert!(started.lock().unwrap().is_empty(), "never started");
        assert!(!root.join("home/runtime/ollama.json").exists());
        assert!(r.cfg.is_none());
    }

    #[test]
    fn linux_without_zstd_says_what_to_install_and_fetches_nothing() {
        let root = tmp("own-zstd");
        let dl = Arc::new(FakeDl::new());
        let mut host = Fake {
            os: "linux",
            arch: "x86_64",
            cpus: 8,
            tar_makes: Some("bin/ollama"),
            pins: Some(test_pins()),
            fetcher: Some(dl.clone()),
            ..Fake::default()
        };
        host.files.insert(
            "/proc/meminfo".into(),
            "MemTotal: 16384000 kB\nMemAvailable: 9000000 kB\n".into(),
        );
        let port = free_port();
        // the plan already says it
        let r = go(&root, &host, &opts(&[]), port, true);
        assert!(
            r.out.contains("Not possible yet:") && r.out.contains(runtime::ZSTD_HELP),
            "{}",
            r.out
        );
        let r = go(&root, &host, &opts(&["--apply", "--yes"]), port, true);
        assert_eq!(r.code, 1);
        assert!(r.out.contains(runtime::ZSTD_HELP), "{}", r.out);
        assert_eq!(
            dl.count(),
            0,
            "nothing fetched for a computer that cannot unpack it"
        );
        assert!(r.question.is_empty(), "no point asking");
    }

    #[test]
    fn too_little_disk_for_unpacking_stops_before_any_fetch() {
        let root = tmp("own-disk");
        let dl = Arc::new(FakeDl::new());
        let mut pins = test_pins();
        for f in &mut pins.files {
            f.bytes = 2 << 30; // 2 GiB: it would need 5 GiB
        }
        let host = Fake {
            pins: Some(pins),
            fetcher: Some(dl.clone()),
            tar_makes: Some("ollama.exe"),
            ..pc(4)
        };
        let port = free_port();
        let r = go(&root, &host, &opts(&[]), port, true);
        assert!(r.out.contains("NOT enough"), "{}", r.out);
        let r = go(&root, &host, &opts(&["--apply", "--yes"]), port, true);
        assert_eq!(r.code, 1);
        // the models need room too, so Ollama and the models are checked together
        assert!(
            r.out.contains(
                "Not enough free disk for Ollama and the models together: about 5.0 GB for Tempo's own copy of Ollama, 2.5 GB of models and 1 GB to keep free, but only 4.0 GB is free."
            ),
            "{}",
            r.out
        );
        assert_eq!(dl.count(), 0);
    }

    #[test]
    fn an_installed_own_copy_is_started_not_fetched_again() {
        let root = tmp("own-again");
        let (host, dl) = bare(50, FakeDl::new());
        let port = free_port();
        let started = Mutex::new(Vec::new());
        let keep = Mutex::new(Vec::new());
        go_with(
            &root,
            &host,
            &opts(&["--apply", "--yes"]),
            port,
            true,
            &mut starts_a_mock_on(port, &started, &keep),
        );
        assert_eq!(dl.count(), 1);
        // a new session: nothing answers on a new port; the record is there
        let port2 = free_port();
        let started2 = Mutex::new(Vec::new());
        let keep2 = Mutex::new(Vec::new());
        let r = go_with(
            &root,
            &host,
            &opts(&["--apply", "--yes"]),
            port2,
            true,
            &mut starts_a_mock_on(port2, &started2, &keep2),
        );
        assert_eq!(r.code, 0, "{}", r.out);
        assert_eq!(dl.count(), 1, "not fetched again");
        assert_eq!(started2.lock().unwrap().len(), 1);
        assert!(
            r.out.contains("Tempo's own copy (0.40.6) is installed at"),
            "{}",
            r.out
        );
        assert!(!r.out.contains("Fetch Tempo's own copy"));
    }
}
