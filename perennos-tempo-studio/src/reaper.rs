//! Plan A: REAPER.
//!
//! - `locate`: `--reaper` flag > env `TEMPO_REAPER` > config `reaper_exe` > standard install paths
//!   (and PATH). A structured "not found" lists every path tried.
//! - `session_script`: a defensive ReaScript (Lua) that builds a REAPER project from a mix plan:
//!   one track per stem with name, volume and media, stock Cockos FX (ReaEQ high-pass, gentle
//!   ReaComp on vocals and drums, ReaLimit on the master), render settings (WAV 24-bit, sample rate,
//!   whole project), saves the .rpp next to the outputs and, only when asked, renders and closes.
//!   It writes a JSON status file that `parse_status` reads back.
//! - `render_project`: headless `-renderproject` render, verified with the loudness meter; when the
//!   result is off target, a new copy of the project with every top-level track volume (pre-master,
//!   so the master limiter still holds the ceiling) moved by the miss is rendered again, at most
//!   twice. Plan C: if REAPER still misses, the built-in master fixes the best render.
//!
//! Every process goes through the `Runner` trait, so the tests drive all of this with a fake.
use crate::loudness::{self, Analysis};
use crate::master::{self, MasterSettings};
use crate::util::{free_path, jnum, r2, OutFile};
use crate::wav::WavReader;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Windows process creation flag: no console window for a background child.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }
}

// ---------------------------------------------------------------- locate

#[derive(Clone, Debug, PartialEq)]
pub struct Tried {
    pub source: String,
    pub path: String,
    pub why: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Located {
    pub exe: PathBuf,
    pub source: String,
    /// Candidates checked before this one (for example a `--reaper` path that does not exist).
    pub skipped: Vec<Tried>,
}

impl Located {
    pub fn to_json(&self) -> Value {
        json!({
            "found": true,
            "exe": self.exe.display().to_string(),
            "source": self.source,
            "skipped": self.skipped.iter().map(tried_json).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NotFound {
    pub tried: Vec<Tried>,
}

fn tried_json(t: &Tried) -> Value {
    json!({"source": t.source, "path": t.path, "why": t.why})
}

impl NotFound {
    pub fn message(&self) -> String {
        let list: Vec<String> = self
            .tried
            .iter()
            .map(|t| format!("{} ({}: {})", t.path, t.source, t.why))
            .collect();
        format!(
            "REAPER not found. Tried: {}. Set --reaper, env TEMPO_REAPER or reaper_exe in the config.",
            list.join("; ")
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "found": false,
            "tried": self.tried.iter().map(tried_json).collect::<Vec<_>>(),
            "message": self.message(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Missing,
}

/// Everything `locate` looks at, so tests can describe any machine.
pub struct LocateEnv<'a> {
    pub flag: Option<&'a str>,
    pub env: Option<String>,
    pub config: Option<&'a str>,
    pub os: Os,
    pub path_var: Option<String>,
    pub program_files: Option<String>,
    pub probe: &'a dyn Fn(&str) -> Kind,
}

fn join_os(dir: &str, name: &str, os: Os) -> String {
    let sep = if os == Os::Windows { '\\' } else { '/' };
    let d = dir.trim_end_matches(['\\', '/']);
    if os == Os::Windows {
        format!("{d}{sep}{}", name.replace('/', "\\"))
    } else {
        format!("{d}{sep}{name}")
    }
}

fn exe_names(os: Os) -> &'static [&'static str] {
    match os {
        Os::Windows => &["reaper.exe"],
        Os::Mac => &["REAPER", "reaper"],
        Os::Linux => &["reaper"],
    }
}

/// The REAPER program inside a folder the user named (an install folder or a macOS .app).
fn inside_dir(dir: &str, os: Os) -> Vec<String> {
    match os {
        Os::Windows => vec![join_os(dir, "reaper.exe", os)],
        Os::Mac => vec![
            join_os(dir, "Contents/MacOS/REAPER", os),
            join_os(dir, "REAPER.app/Contents/MacOS/REAPER", os),
        ],
        Os::Linux => vec![join_os(dir, "reaper", os)],
    }
}

/// Standard install locations, in the order they are tried.
pub fn standard_paths(os: Os, program_files: Option<&str>) -> Vec<String> {
    match os {
        Os::Windows => {
            let mut v = Vec::new();
            if let Some(pf) = program_files.filter(|p| !p.eq_ignore_ascii_case(r"C:\Program Files"))
            {
                v.push(join_os(pf, r"REAPER (x64)\reaper.exe", os));
                v.push(join_os(pf, r"REAPER\reaper.exe", os));
            }
            v.push(r"C:\Program Files\REAPER (x64)\reaper.exe".into());
            v.push(r"C:\Program Files\REAPER\reaper.exe".into());
            v.push(r"C:\Program Files (x86)\REAPER\reaper.exe".into());
            v
        }
        Os::Mac => vec![
            "/Applications/REAPER.app/Contents/MacOS/REAPER".into(),
            "/Applications/REAPER64.app/Contents/MacOS/REAPER".into(),
        ],
        Os::Linux => vec!["/opt/REAPER/reaper".into()],
    }
}

fn search_path(e: &LocateEnv) -> Option<String> {
    let pv = e.path_var.as_deref()?;
    let sep = if e.os == Os::Windows { ';' } else { ':' };
    for dir in pv.split(sep).filter(|d| !d.trim().is_empty()) {
        for n in exe_names(e.os) {
            let c = join_os(dir.trim(), n, e.os);
            if (e.probe)(&c) == Kind::File {
                return Some(c);
            }
        }
    }
    None
}

pub fn locate(e: &LocateEnv) -> Result<Located, NotFound> {
    let mut tried = Vec::new();
    let user = [
        ("--reaper", e.flag.map(String::from)),
        ("env TEMPO_REAPER", e.env.clone()),
        ("config reaper_exe", e.config.map(String::from)),
    ];
    for (source, p) in user {
        let Some(p) = p else { continue };
        let p = p.trim().to_string();
        if p.is_empty() {
            continue;
        }
        match (e.probe)(&p) {
            Kind::File => {
                return Ok(Located {
                    exe: PathBuf::from(p),
                    source: source.into(),
                    skipped: tried,
                })
            }
            Kind::Dir => {
                if let Some(c) = inside_dir(&p, e.os)
                    .into_iter()
                    .find(|c| (e.probe)(c) == Kind::File)
                {
                    return Ok(Located {
                        exe: PathBuf::from(c),
                        source: source.into(),
                        skipped: tried,
                    });
                }
                tried.push(Tried {
                    source: source.into(),
                    path: p,
                    why: "a folder without the REAPER program in it".into(),
                });
            }
            Kind::Missing => tried.push(Tried {
                source: source.into(),
                path: p,
                why: "does not exist".into(),
            }),
        }
    }
    let path_first = e.os == Os::Linux;
    let path_try = |tried: &mut Vec<Tried>| -> Option<Located> {
        if let Some(c) = search_path(e) {
            return Some(Located {
                exe: PathBuf::from(c),
                source: "PATH".into(),
                skipped: tried.clone(),
            });
        }
        tried.push(Tried {
            source: "PATH".into(),
            path: exe_names(e.os).join(" or "),
            why: if e.path_var.is_some() {
                "not on PATH".into()
            } else {
                "PATH is not set".into()
            },
        });
        None
    };
    if path_first {
        if let Some(l) = path_try(&mut tried) {
            return Ok(l);
        }
    }
    for p in standard_paths(e.os, e.program_files.as_deref()) {
        if (e.probe)(&p) == Kind::File {
            return Ok(Located {
                exe: PathBuf::from(p),
                source: "standard install path".into(),
                skipped: tried,
            });
        }
        tried.push(Tried {
            source: "standard install path".into(),
            path: p,
            why: "not there".into(),
        });
    }
    if !path_first {
        if let Some(l) = path_try(&mut tried) {
            return Ok(l);
        }
    }
    Err(NotFound { tried })
}

/// `locate` on this machine: real environment and file system.
pub fn locate_here(flag: Option<&str>, config: Option<&str>) -> Result<Located, NotFound> {
    let probe = |p: &str| match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => Kind::Dir,
        Ok(_) => Kind::File,
        Err(_) => Kind::Missing,
    };
    locate(&LocateEnv {
        flag,
        env: std::env::var("TEMPO_REAPER").ok(),
        config,
        os: Os::current(),
        path_var: std::env::var("PATH").ok(),
        program_files: std::env::var("ProgramFiles").ok(),
        probe: &probe,
    })
}

// ---------------------------------------------------------------- commands and the runner

#[derive(Clone, Debug, PartialEq)]
pub struct Cmd {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// A background child: on Windows it gets CREATE_NO_WINDOW. Opening the REAPER GUI on
    /// purpose is not background.
    pub background: bool,
}

impl Cmd {
    pub fn display(&self) -> String {
        let mut parts = vec![quote_arg(&self.program.to_string_lossy())];
        parts.extend(self.args.iter().map(|a| quote_arg(a)));
        parts.join(" ")
    }
}

fn quote_arg(a: &str) -> String {
    if a.contains([' ', '"', '\t']) || a.is_empty() {
        format!("\"{}\"", a.replace('"', "\\\""))
    } else {
        a.to_string()
    }
}

fn pstr(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The `.app` bundle around a macOS REAPER binary.
fn mac_bundle(exe: &Path) -> Option<String> {
    let s = exe.to_string_lossy();
    s.find(".app/").map(|i| s[..i + 4].to_string())
}

/// Start the REAPER GUI on purpose with a project, media file or script (visible, detached).
/// macOS goes through `open -a` so the app is launched and focused the normal way.
pub fn open_cmd(exe: &Path, file: &Path, os: Os) -> Cmd {
    match (os, mac_bundle(exe)) {
        (Os::Mac, Some(bundle)) => Cmd {
            program: PathBuf::from("/usr/bin/open"),
            args: vec!["-a".into(), bundle, pstr(file)],
            background: false,
        },
        _ => Cmd {
            program: exe.to_path_buf(),
            args: vec![pstr(file)],
            background: false,
        },
    }
}

/// Headless render: a new REAPER instance renders the project with its saved settings and exits.
pub fn render_cmd(exe: &Path, rpp: &Path) -> Cmd {
    Cmd {
        program: exe.to_path_buf(),
        args: vec![
            "-newinst".into(),
            "-nosplash".into(),
            "-renderproject".into(),
            pstr(rpp),
        ],
        background: true,
    }
}

pub trait Proc {
    fn id(&self) -> u32;
    /// `Some(exit code)` once it has exited (-1 when killed by a signal).
    fn try_wait(&mut self) -> Result<Option<i32>, String>;
}

pub trait Runner {
    fn start(&self, cmd: &Cmd) -> Result<Box<dyn Proc>, String>;
}

/// Real processes. stdin/stdout/stderr are never shared (the MCP server owns stdout).
pub struct SystemRunner;

struct SysProc {
    child: Option<std::process::Child>,
}

impl Proc for SysProc {
    fn id(&self) -> u32 {
        self.child.as_ref().map(|c| c.id()).unwrap_or(0)
    }

    fn try_wait(&mut self) -> Result<Option<i32>, String> {
        match self.child.as_mut() {
            Some(c) => c
                .try_wait()
                .map(|s| s.map(|s| s.code().unwrap_or(-1)))
                .map_err(|e| e.to_string()),
            None => Ok(Some(0)),
        }
    }
}

impl Drop for SysProc {
    /// A REAPER that is still running is left running; a thread waits for it so it never
    /// lingers as a zombie. Nothing is ever killed.
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            if let Ok(None) = c.try_wait() {
                std::thread::spawn(move || {
                    let _ = c.wait();
                });
            }
        }
    }
}

impl Runner for SystemRunner {
    fn start(&self, cmd: &Cmd) -> Result<Box<dyn Proc>, String> {
        let mut c = std::process::Command::new(&cmd.program);
        c.args(&cmd.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            if cmd.background {
                c.creation_flags(CREATE_NO_WINDOW);
            }
        }
        let child = c
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", cmd.program.display()))?;
        Ok(Box::new(SysProc { child: Some(child) }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Wait {
    pub timeout: Duration,
    pub poll: Duration,
}

impl Default for Wait {
    fn default() -> Self {
        Wait {
            timeout: Duration::from_secs(1800),
            poll: Duration::from_millis(500),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitEnd {
    Ready,
    Exited(i32),
    TimedOut,
}

/// Wait until `ready()` holds, the process exits, or the time runs out.
pub fn wait_for(
    p: &mut dyn Proc,
    w: Wait,
    ready: &mut dyn FnMut() -> bool,
) -> Result<WaitEnd, String> {
    let start = Instant::now();
    loop {
        if ready() {
            return Ok(WaitEnd::Ready);
        }
        if let Some(code) = p.try_wait()? {
            return Ok(if ready() {
                WaitEnd::Ready
            } else {
                WaitEnd::Exited(code)
            });
        }
        if start.elapsed() >= w.timeout {
            return Ok(WaitEnd::TimedOut);
        }
        std::thread::sleep(w.poll);
    }
}

// ---------------------------------------------------------------- Lua

/// A double-quoted Lua string literal for any text: quotes, backslashes, newlines and every
/// other control byte are escaped (decimal `\ddd`), UTF-8 passes through unchanged.
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn lua_num(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.4}")
    } else {
        "0".into()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackPlan {
    pub name: String,
    pub file: PathBuf,
    /// Track volume in dB (stem gain + mix trim + master gain). Above +12 dB the rest goes on the item.
    pub volume_db: f64,
    pub highpass_hz: Option<f64>,
    pub compress: bool,
    pub comp_threshold_db: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MasterChain {
    /// The ReaLimit brickwall ceiling (a little under the true-peak target).
    pub limiter_ceiling_db: f64,
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionPlan {
    pub sample_rate: u32,
    pub tracks: Vec<TrackPlan>,
    pub master: Option<MasterChain>,
    pub project_path: PathBuf,
    pub render_path: PathBuf,
    pub status_path: PathBuf,
    pub render: bool,
    pub close: bool,
}

/// ReaLimit sits this far under the true-peak target (it may limit sample peaks only).
pub const REALIMIT_MARGIN_DB: f64 = 0.5;

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64()).filter(|x| x.is_finite())
}

impl SessionPlan {
    /// Build a session from a mix plan (`mix` writes one) for a script at `script`. The project,
    /// render and status files sit next to the script: `<name>.rpp`, `<name>.wav`,
    /// `<name>.status.json` (free names, never overwriting).
    pub fn from_plan_json(
        plan: &Value,
        plan_dir: &Path,
        script: &Path,
    ) -> Result<SessionPlan, String> {
        let stems = plan
            .get("stems")
            .and_then(|s| s.as_array())
            .filter(|s| !s.is_empty())
            .ok_or("the plan has no stems")?;
        let trim = num(plan, "trim_db").unwrap_or(0.0);
        let master_v = plan.get("master").filter(|m| m.is_object());
        let mut master = None;
        let mut master_gain = 0.0;
        if let Some(m) = master_v {
            let target = num(m, "target_lufs").ok_or("plan master has no target_lufs")?;
            let ceiling = num(m, "ceiling_dbtp").ok_or("plan master has no ceiling_dbtp")?;
            MasterSettings::new(target, ceiling)?;
            // Pre-limiter gain: what the built-in master measured, else target minus the mix.
            master_gain = num(m, "gain_db")
                .or_else(|| {
                    plan.get("mix")
                        .and_then(|x| num(x, "integrated_lufs"))
                        .map(|l| target - l)
                })
                .unwrap_or(0.0);
            master = Some(MasterChain {
                limiter_ceiling_db: ceiling - REALIMIT_MARGIN_DB,
                target_lufs: target,
                ceiling_dbtp: ceiling,
            });
        }
        let mut tracks = Vec::new();
        for (i, s) in stems.iter().enumerate() {
            let p = s
                .get("path")
                .and_then(|x| x.as_str())
                .filter(|x| !x.trim().is_empty())
                .ok_or_else(|| format!("stem {} has no path", i + 1))?;
            let file = if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                plan_dir.join(p)
            };
            let name = s
                .get("name")
                .and_then(|x| x.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    file.file_stem()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("Stem {}", i + 1))
                });
            let gain = num(s, "gain_db").unwrap_or(0.0);
            let lufs = num(s, "lufs");
            tracks.push(TrackPlan {
                name,
                file,
                volume_db: gain + trim + master_gain,
                highpass_hz: num(s, "highpass_hz").filter(|f| *f > 10.0 && *f < 2000.0),
                compress: s.get("compress").and_then(|x| x.as_bool()).unwrap_or(false),
                comp_threshold_db: lufs.map(|l| (l + 4.0).clamp(-40.0, -3.0)).unwrap_or(-18.0),
            });
        }
        let base = script.with_extension("");
        let with = |ext: &str| PathBuf::from(format!("{}{ext}", base.to_string_lossy()));
        Ok(SessionPlan {
            sample_rate: plan
                .get("sample_rate")
                .and_then(|x| x.as_u64())
                .unwrap_or(48000) as u32,
            tracks,
            master,
            project_path: free_path(&with(".rpp"))?,
            render_path: free_path(&with(".wav"))?,
            status_path: free_path(&with(".status.json"))?,
            render: false,
            close: false,
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "project": self.project_path.display().to_string(),
            "render_file": self.render_path.display().to_string(),
            "status_file": self.status_path.display().to_string(),
            "sample_rate": self.sample_rate,
            "render": self.render,
            "close": self.close,
            "tracks": self.tracks.iter().map(|t| json!({
                "name": t.name,
                "file": t.file.display().to_string(),
                "volume_db": r2(t.volume_db),
                "highpass_hz": t.highpass_hz,
                "compress": t.compress,
            })).collect::<Vec<_>>(),
            "master": self.master.as_ref().map(|m| json!({
                "limiter_ceiling_db": r2(m.limiter_ceiling_db),
                "target_lufs": m.target_lufs,
                "ceiling_dbtp": m.ceiling_dbtp,
            })),
        })
    }
}

/// First line after the banner: the planned paths as JSON, for people and tools.
pub const SESSION_TAG: &str = "-- tempo-studio-session: ";

/// The generated ReaScript.
pub fn session_script(s: &SessionPlan) -> String {
    let mut p = String::new();
    p.push_str("-- Perennos Tempo studio session, generated by tempo-studio. Run it in REAPER (Actions > ReaScript: run,\n");
    p.push_str("-- or start REAPER with this file). It builds a new project tab; it never overwrites a file.\n");
    let tag = json!({
        "status": pstr(&s.status_path),
        "project": pstr(&s.project_path),
        "render": pstr(&s.render_path),
    });
    p.push_str(&format!("{SESSION_TAG}{tag}\n\n"));
    p.push_str("local P = {\n");
    p.push_str(&format!("  status = {},\n", lua_str(&pstr(&s.status_path))));
    p.push_str(&format!(
        "  project = {},\n",
        lua_str(&pstr(&s.project_path))
    ));
    p.push_str(&format!(
        "  render_file = {},\n",
        lua_str(&pstr(&s.render_path))
    ));
    p.push_str(&format!("  sample_rate = {},\n", s.sample_rate));
    p.push_str(&format!("  do_render = {},\n", s.render));
    p.push_str(&format!("  do_close = {},\n", s.close));
    p.push_str("  tracks = {\n");
    for t in &s.tracks {
        let hp = t
            .highpass_hz
            .map(|f| format!(", hp_hz = {}", lua_num(f)))
            .unwrap_or_default();
        p.push_str(&format!(
            "    {{ name = {}, file = {}, vol_db = {}{hp}, comp = {}, comp_thresh_db = {} }},\n",
            lua_str(&t.name),
            lua_str(&pstr(&t.file)),
            lua_num(t.volume_db),
            t.compress,
            lua_num(t.comp_threshold_db),
        ));
    }
    p.push_str("  },\n");
    match &s.master {
        Some(m) => p.push_str(&format!(
            "  master = {{ limiter = true, ceiling_db = {} }},\n",
            lua_num(m.limiter_ceiling_db)
        )),
        None => p.push_str("  master = nil,\n"),
    }
    p.push_str("}\n");
    p.push_str(LUA_BODY);
    p
}

/// Write the script to `want` (a free name; never overwriting). Returns the path written.
pub fn write_script(s: &SessionPlan, want: &Path) -> Result<PathBuf, String> {
    let (out, mut f) = OutFile::create(want, false, &[])?;
    f.write_all(session_script(s).as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("{e} {}", out.left_note()))?;
    drop(f);
    out.commit()
}

/// The fixed part of the script. Every optional API call is checked first (`has`), risky blocks
/// run in `pcall`, and the status file is always written at the end.
pub const LUA_BODY: &str = r#"
local S = {
  tool = "perennos-tempo-studio", version = 1, done = false, ok = false,
  steps = {}, warnings = {}, fx_missing = {}, tracks = 0, items = 0, rendered = false,
}

local function add(list, v) list[#list + 1] = v end
local function step(s) add(S.steps, s) end
local function warn(s) add(S.warnings, s) end
local function has(name) return type(reaper) == "table" and type(reaper[name]) == "function" end
local function db2amp(db) return 10 ^ (db / 20) end

local function jstr(s)
  s = tostring(s):gsub('[%c"\\]', function(c)
    if c == '"' then return '\\"' end
    if c == '\\' then return '\\\\' end
    if c == '\n' then return '\\n' end
    if c == '\r' then return '\\r' end
    if c == '\t' then return '\\t' end
    return string.format('\\u%04x', c:byte())
  end)
  return '"' .. s .. '"'
end

local function jval(v)
  local t = type(v)
  if t == "nil" then return "null" end
  if t == "boolean" then return v and "true" or "false" end
  if t == "number" then
    if v ~= v or v == math.huge or v == -math.huge then return "null" end
    if math.type and math.type(v) == "integer" then return tostring(v) end
    return string.format("%.10g", v)
  end
  if t == "table" then
    if next(v) == nil or #v > 0 then
      local parts = {}
      for i = 1, #v do parts[i] = jval(v[i]) end
      return "[" .. table.concat(parts, ",") .. "]"
    end
    local keys = {}
    for k in pairs(v) do keys[#keys + 1] = tostring(k) end
    table.sort(keys)
    local parts = {}
    for i, k in ipairs(keys) do parts[i] = jstr(k) .. ":" .. jval(v[k]) end
    return "{" .. table.concat(parts, ",") .. "}"
  end
  return jstr(tostring(v))
end

local function file_exists(p)
  if has("file_exists") then
    local ok, r = pcall(reaper.file_exists, p)
    if ok then return r == true end
  end
  local f = io.open(p, "rb")
  if f then f:close() return true end
  return false
end

-- Never overwrite: name-2.ext, name-3.ext, ...
local function free_name(path)
  if not file_exists(path) then return path end
  local base, ext = path:match("^(.-)(%.[^%.\\/]*)$")
  if not base then base, ext = path, "" end
  for n = 2, 9999 do
    local p = base .. "-" .. n .. ext
    if not file_exists(p) then return p end
  end
  error("no free file name next to " .. path)
end

local function split_path(p)
  local dir, name = p:match("^(.*)[\\/]([^\\/]*)$")
  if not dir then dir, name = ".", p end
  if dir == "" then dir = "/" end
  local stem = name:match("^(.*)%.[^%.]*$") or name
  return dir, stem
end

local function write_status()
  local text = jval(S)
  local tmp = free_name(P.status .. ".tmp")
  local f = io.open(tmp, "w")
  if f then
    f:write(text)
    f:close()
    if os.rename(tmp, P.status) then return end
  end
  f = io.open(P.status, "w")
  if f then f:write(text) f:close() end
end

local function add_fx(tr, names, what)
  if not has("TrackFX_AddByName") then
    add(S.fx_missing, what)
    warn("TrackFX_AddByName is not available; " .. what .. " not added")
    return -1
  end
  for _, n in ipairs(names) do
    local ok, idx = pcall(reaper.TrackFX_AddByName, tr, n, false, -1)
    if ok and type(idx) == "number" and idx >= 0 then
      step("fx:" .. n)
      return idx
    end
  end
  add(S.fx_missing, what)
  warn("could not add " .. what .. " (tried " .. table.concat(names, ", ") .. ")")
  return -1
end

local function find_param(tr, fx, pats)
  if fx < 0 or not (has("TrackFX_GetNumParams") and has("TrackFX_GetParamName")) then return -1 end
  local ok, n = pcall(reaper.TrackFX_GetNumParams, tr, fx)
  if not ok or type(n) ~= "number" then return -1 end
  for i = 0, n - 1 do
    local ok2, _, name = pcall(reaper.TrackFX_GetParamName, tr, fx, i, "")
    name = ok2 and tostring(name or ""):lower() or ""
    for _, pat in ipairs(pats) do
      if name:find(pat, 1, true) then return i end
    end
  end
  return -1
end

-- Set a parameter so the plug-in shows `target` (a bisection on the normalised value, so it
-- works whatever the plug-in's internal mapping is, as long as it is monotonic).
local function set_by_display(tr, fx, p, target)
  if fx < 0 or p < 0 then return false end
  if not (has("TrackFX_SetParamNormalized") and has("TrackFX_GetFormattedParamValue")) then return false end
  local function shown(v)
    reaper.TrackFX_SetParamNormalized(tr, fx, p, v)
    local _, txt = reaper.TrackFX_GetFormattedParamValue(tr, fx, p, "")
    txt = tostring(txt or "")
    local x = tonumber(txt:match("[-+]?%d+%.?%d*"))
    if x and txt:lower():match("%d%s*k") then x = x * 1000 end
    return x
  end
  local ok, res = pcall(function()
    local lo, hi = 0.0, 1.0
    local vlo, vhi = shown(lo), shown(hi)
    if not vlo or not vhi then return false end
    local rising = vhi >= vlo
    for _ = 1, 40 do
      local mid = (lo + hi) / 2
      local v = shown(mid)
      if not v then return false end
      if (v < target) == rising then lo = mid else hi = mid end
    end
    local final = shown((lo + hi) / 2)
    return final ~= nil and math.abs(final - target) <= math.max(0.05 * math.abs(target), 0.5)
  end)
  return ok and res == true
end

local function bypass(tr, fx, why)
  if has("TrackFX_SetEnabled") then pcall(reaper.TrackFX_SetEnabled, tr, fx, false) end
  warn(why .. "; the plug-in was added but bypassed")
end

local function setup_highpass(tr, hz, name)
  local fx = add_fx(tr, { "ReaEQ", "ReaEQ (Cockos)", "VST: ReaEQ (Cockos)" }, "ReaEQ (high-pass) on " .. name)
  if fx < 0 then return end
  local typed = false
  if has("TrackFX_SetNamedConfigParm") then
    pcall(reaper.TrackFX_SetNamedConfigParm, tr, fx, "BANDENABLED0", "1")
    local ok, r = pcall(reaper.TrackFX_SetNamedConfigParm, tr, fx, "BANDTYPE0", "4")
    typed = ok and r == true
  end
  if not typed then
    bypass(tr, fx, "ReaEQ band 1 could not be made a high-pass on " .. name)
    return
  end
  local p = find_param(tr, fx, { "freq" })
  if p < 0 then p = 0 end
  if set_by_display(tr, fx, p, hz) then
    step("highpass:" .. name .. ":" .. hz)
  else
    bypass(tr, fx, "ReaEQ high-pass frequency could not be set to " .. hz .. " Hz on " .. name)
  end
end

local function setup_comp(tr, thresh_db, name)
  local fx = add_fx(tr, { "ReaComp", "ReaComp (Cockos)", "VST: ReaComp (Cockos)" }, "ReaComp on " .. name)
  if fx < 0 then return end
  local ok_t = set_by_display(tr, fx, find_param(tr, fx, { "thresh" }), thresh_db)
  local ok_r = set_by_display(tr, fx, find_param(tr, fx, { "ratio" }), 2.0)
  if not (ok_t and ok_r) then
    bypass(tr, fx, "ReaComp threshold or ratio could not be set on " .. name)
    return
  end
  if not set_by_display(tr, fx, find_param(tr, fx, { "attack" }), 10) then warn("ReaComp attack left at its default on " .. name) end
  if not set_by_display(tr, fx, find_param(tr, fx, { "release" }), 120) then warn("ReaComp release left at its default on " .. name) end
  step("comp:" .. name)
end

local function insert_by_source(tr, file)
  if not (has("AddMediaItemToTrack") and has("AddTakeToMediaItem") and has("PCM_Source_CreateFromFile")
      and has("SetMediaItemTake_Source") and has("SetMediaItemInfo_Value")) then return false end
  local ok = pcall(function()
    local src = reaper.PCM_Source_CreateFromFile(file)
    if not src then error("no source") end
    local item = reaper.AddMediaItemToTrack(tr)
    local take = reaper.AddTakeToMediaItem(item)
    reaper.SetMediaItemTake_Source(take, src)
    reaper.SetMediaItemInfo_Value(item, "D_POSITION", 0)
    if has("GetMediaSourceLength") then
      local len = reaper.GetMediaSourceLength(src)
      if type(len) == "number" and len > 0 then reaper.SetMediaItemInfo_Value(item, "D_LENGTH", len) end
    end
  end)
  if ok then step("media_by_source:" .. file) end
  return ok
end

local function count_items(tr)
  if has("CountTrackMediaItems") then return reaper.CountTrackMediaItems(tr) end
  return -1
end

local function add_track(idx, t)
  reaper.InsertTrackAtIndex(idx, false)
  local tr = reaper.GetTrack(0, idx)
  if not tr then error("GetTrack returned no track at " .. idx) end
  reaper.GetSetMediaTrackInfo_String(tr, "P_NAME", t.name, true)
  local track_db = math.min(t.vol_db, 12)
  reaper.SetMediaTrackInfo_Value(tr, "D_VOL", db2amp(track_db))
  S.tracks = S.tracks + 1
  if not file_exists(t.file) then
    warn("media file not found: " .. t.file)
  else
    local before = count_items(tr)
    local inserted = false
    if has("InsertMedia") and has("SetOnlyTrackSelected") and has("SetEditCurPos") then
      reaper.SetOnlyTrackSelected(tr)
      if has("Main_OnCommand") then reaper.Main_OnCommand(40914, 0) end -- set first selected track as last touched
      reaper.SetEditCurPos(0, false, false)
      local ok = pcall(reaper.InsertMedia, t.file, 0)
      inserted = ok and (before < 0 or count_items(tr) > before)
    end
    if not inserted then inserted = insert_by_source(tr, t.file) end
    if inserted then
      S.items = S.items + 1
      local item_db = t.vol_db - track_db
      if item_db > 0.001 and has("GetTrackMediaItem") and has("SetMediaItemInfo_Value") then
        local item = reaper.GetTrackMediaItem(tr, 0)
        if item then reaper.SetMediaItemInfo_Value(item, "D_VOL", db2amp(item_db)) end
      end
    else
      warn("could not put " .. t.file .. " on track " .. t.name)
    end
  end
  if t.hp_hz then
    local ok, err = pcall(setup_highpass, tr, t.hp_hz, t.name)
    if not ok then warn("high-pass on " .. t.name .. ": " .. tostring(err)) end
  end
  if t.comp then
    local ok, err = pcall(setup_comp, tr, t.comp_thresh_db, t.name)
    if not ok then warn("compressor on " .. t.name .. ": " .. tostring(err)) end
  end
  step("track:" .. t.name)
end

local function setup_limiter(ceiling_db)
  if not has("GetMasterTrack") then warn("GetMasterTrack is not available; no limiter") add(S.fx_missing, "ReaLimit") return end
  local m = reaper.GetMasterTrack(0)
  local fx = add_fx(m, { "ReaLimit", "ReaLimit (Cockos)", "VST: ReaLimit (Cockos)" }, "ReaLimit")
  if fx < 0 then return end
  if set_by_display(m, fx, find_param(m, fx, { "ceiling" }), ceiling_db) then
    step("limiter:" .. ceiling_db)
  else
    warn("ReaLimit ceiling could not be set to " .. ceiling_db .. " dB; it is at its default")
  end
end

local function set_render()
  if not has("GetSetProjectInfo_String") then
    warn("GetSetProjectInfo_String is not available; render settings not set")
    return
  end
  P.render_file = free_name(P.render_file)
  local dir, stem = split_path(P.render_file)
  local function sets(key, val)
    local ok, r = pcall(reaper.GetSetProjectInfo_String, 0, key, val, true)
    return ok and r ~= false
  end
  local okd = sets("RENDER_FILE", dir)
  local okp = sets("RENDER_PATTERN", stem)
  if not (okd and okp) then warn("render file could not be set to " .. P.render_file) end
  if not sets("RENDER_FORMAT", "ZXZhdxgAAQ==") and not sets("RENDER_FORMAT", "evaw") then
    warn("render format could not be set to WAV")
  end
  if has("GetSetProjectInfo") then
    pcall(reaper.GetSetProjectInfo, 0, "RENDER_SETTINGS", 0, true)  -- master mix
    pcall(reaper.GetSetProjectInfo, 0, "RENDER_BOUNDSFLAG", 1, true) -- entire project
    pcall(reaper.GetSetProjectInfo, 0, "RENDER_CHANNELS", 2, true)
    pcall(reaper.GetSetProjectInfo, 0, "RENDER_SRATE", P.sample_rate, true)
  end
  S.render_file = P.render_file
  step("render_settings")
end

local function save()
  P.project = free_name(P.project)
  if has("Main_SaveProjectEx") then
    local ok = pcall(reaper.Main_SaveProjectEx, 0, P.project, 0)
    if ok and file_exists(P.project) then
      S.project = P.project
      step("saved")
      return
    end
  end
  if has("Main_OnCommand") then
    warn("Main_SaveProjectEx did not save " .. P.project .. "; asking REAPER to save (it may show a dialog)")
    reaper.Main_OnCommand(40026, 0) -- File: Save project
  else
    warn("the project could not be saved")
  end
end

local function render()
  if not has("Main_OnCommand") then warn("Main_OnCommand is not available; not rendered") return end
  reaper.Main_OnCommand(42230, 0) -- render with the project's render settings, auto-close
  S.rendered = file_exists(P.render_file)
  if S.rendered then step("rendered") else warn("render finished but " .. P.render_file .. " is not there") end
end

local function build()
  if has("GetAppVersion") then S.app = reaper.GetAppVersion() end
  P.status = free_name(P.status)
  if has("Main_OnCommand") then
    reaper.Main_OnCommand(40859, 0) -- File: New project tab
    step("new_project")
  else
    warn("Main_OnCommand is not available; using the current project")
  end
  if has("GetSetProjectInfo") then
    pcall(reaper.GetSetProjectInfo, 0, "PROJECT_SRATE", P.sample_rate, true)
    pcall(reaper.GetSetProjectInfo, 0, "PROJECT_SRATE_USE", 1, true)
  end
  local first = 0
  if has("CountTracks") then
    first = reaper.CountTracks(0)
    if first > 0 then warn("the new project already had " .. first .. " tracks (a default template?); ours follow them") end
  end
  if has("PreventUIRefresh") then reaper.PreventUIRefresh(1) end
  if has("Undo_BeginBlock") then reaper.Undo_BeginBlock() end
  for i, t in ipairs(P.tracks) do
    local ok, err = pcall(add_track, first + i - 1, t)
    if not ok then warn("track " .. t.name .. ": " .. tostring(err)) end
  end
  if P.master then
    local ok, err = pcall(setup_limiter, P.master.ceiling_db)
    if not ok then warn("limiter: " .. tostring(err)) end
  end
  if has("Undo_EndBlock") then reaper.Undo_EndBlock("Perennos Tempo studio session", -1) end
  if has("PreventUIRefresh") then reaper.PreventUIRefresh(-1) end
  if has("TrackList_AdjustWindows") then reaper.TrackList_AdjustWindows(false) end
  if has("UpdateArrange") then reaper.UpdateArrange() end
  set_render()
  save()
  if P.do_render then render() end
  if P.do_close and has("Main_OnCommand") then
    reaper.Main_OnCommand(40860, 0) -- File: Close current project tab
    step("closed")
  end
end

local ok, err = pcall(build)
if not ok then S.error = tostring(err) end
S.ok = ok
S.done = true
write_status()
"#;

// ---------------------------------------------------------------- status file

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub done: bool,
    pub ok: bool,
    pub app: Option<String>,
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
    pub fx_missing: Vec<String>,
    pub tracks: u64,
    pub items: u64,
    pub project: Option<PathBuf>,
    pub render_file: Option<PathBuf>,
    pub rendered: bool,
    pub error: Option<String>,
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(|x| {
                    x.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| x.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse what the script wrote. Missing fields are tolerated; text that is not a JSON object is
/// an error (it may be half written: the caller polls again).
pub fn parse_status(text: &str) -> Result<Status, String> {
    let t = text.trim_start_matches('\u{feff}').trim();
    if t.is_empty() {
        return Err("the status file is empty".into());
    }
    let v: Value =
        serde_json::from_str(t).map_err(|e| format!("status file is not valid JSON: {e}"))?;
    if !v.is_object() {
        return Err("status file is not a JSON object".into());
    }
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(String::from)
    };
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    Ok(Status {
        done: b("done"),
        ok: b("ok"),
        app: s("app"),
        steps: strings(&v, "steps"),
        warnings: strings(&v, "warnings"),
        fx_missing: strings(&v, "fx_missing"),
        tracks: n("tracks"),
        items: n("items"),
        project: s("project").map(PathBuf::from),
        render_file: s("render_file").map(PathBuf::from),
        rendered: b("rendered"),
        error: s("error"),
    })
}

/// `Ok(None)` while the file is missing or not complete JSON yet.
pub fn read_status(path: &Path) -> Option<Status> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| parse_status(&t).ok())
}

impl Status {
    pub fn to_json(&self) -> Value {
        json!({
            "done": self.done,
            "ok": self.ok,
            "app": self.app,
            "steps": self.steps,
            "warnings": self.warnings,
            "fx_missing": self.fx_missing,
            "tracks": self.tracks,
            "items": self.items,
            "project": self.project.as_ref().map(|p| p.display().to_string()),
            "render_file": self.render_file.as_ref().map(|p| p.display().to_string()),
            "rendered": self.rendered,
            "error": self.error,
        })
    }
}

// ---------------------------------------------------------------- RPP (REAPER project text)

/// Quote a value the way REAPER writes it: "..." unless the text has a double quote, then '...'
/// or `...`.
pub fn rpp_quote(s: &str) -> Result<String, String> {
    if s.contains(['\n', '\r']) {
        return Err("a project value cannot contain a line break".into());
    }
    for q in ['"', '\'', '`'] {
        if !s.contains(q) {
            return Ok(format!("{q}{s}{q}"));
        }
    }
    Err(format!("cannot quote {s} for a REAPER project"))
}

/// First value after a key on an RPP line, unquoted.
fn rpp_value(rest: &str) -> String {
    let r = rest.trim();
    if let Some(q) = r.chars().next().filter(|c| matches!(c, '"' | '\'' | '`')) {
        let inner = &r[1..];
        return inner.split(q).next().unwrap_or("").to_string();
    }
    r.split_whitespace().next().unwrap_or("").to_string()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RppInfo {
    pub render_file: Option<String>,
    pub render_pattern: Option<String>,
    pub tracks: usize,
    pub top_level_volumes: Vec<f64>,
}

/// Walk the project text, calling `f(open_blocks, line)` for every line (`open_blocks` is the
/// stack of block names the line sits in, outermost first).
fn walk_rpp(text: &str, mut f: impl FnMut(&[String], &str)) {
    let mut stack: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t == ">" {
            f(&stack, line);
            stack.pop();
            continue;
        }
        if let Some(rest) = t.strip_prefix('<') {
            f(&stack, line);
            stack.push(rest.split_whitespace().next().unwrap_or("").to_string());
            continue;
        }
        f(&stack, line);
    }
}

fn is_top(stack: &[String], name: &str) -> bool {
    stack.len() == 2 && stack[0] == "REAPER_PROJECT" && stack[1] == name
}

pub fn rpp_info(text: &str) -> RppInfo {
    let text = text.trim_start_matches('\u{feff}');
    let mut info = RppInfo::default();
    let mut depth = 0i64;
    let mut track_top = false;
    walk_rpp(text, |stack, line| {
        let t = line.trim();
        if stack.len() == 1 && stack[0] == "REAPER_PROJECT" {
            if let Some(r) = t.strip_prefix("RENDER_FILE ") {
                info.render_file = Some(rpp_value(r));
            } else if let Some(r) = t.strip_prefix("RENDER_PATTERN ") {
                info.render_pattern = Some(rpp_value(r));
            } else if t.starts_with("<TRACK") {
                info.tracks += 1;
                track_top = depth == 0;
            }
        }
        if is_top(stack, "TRACK") {
            if let Some(r) = t.strip_prefix("VOLPAN ") {
                if track_top {
                    if let Ok(v) = rpp_value(r).parse::<f64>() {
                        info.top_level_volumes.push(v);
                    }
                }
            } else if let Some(r) = t.strip_prefix("ISBUS ") {
                depth += r
                    .split_whitespace()
                    .nth(1)
                    .and_then(|x| x.parse::<i64>().ok())
                    .unwrap_or(0);
            }
        }
    });
    info
}

/// WAV 24-bit render configuration, as REAPER stores it.
pub const RENDER_CFG_WAV24: &str = "ZXZhdxgAAQ==";

/// A copy of the project that renders the master mix of the whole project to `render_dir` /
/// `render_stem`.wav as WAV 24-bit, with every top-level track's volume (VOLPAN, before the master
/// FX) moved by `gain_db`.
pub fn rpp_render_copy(
    text: &str,
    render_dir: &str,
    render_stem: &str,
    gain_db: f64,
) -> Result<String, String> {
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if !text.starts_with("<REAPER_PROJECT") {
        return Err("not a REAPER project (.rpp): it does not start with <REAPER_PROJECT".into());
    }
    let scale = crate::util::db_to_amp(gain_db);
    let mut out = String::with_capacity(text.len() + 256);
    let mut depth = 0i64;
    let mut track_top = false;
    let mut scaled = 0usize;
    let mut skipping_cfg = false;
    let mut header_done = false;
    let mut err = None;
    walk_rpp(text, |stack, line| {
        let t = line.trim();
        if skipping_cfg {
            if t == ">" && stack.len() == 2 {
                skipping_cfg = false;
            }
            return;
        }
        if stack.len() == 1 && stack[0] == "REAPER_PROJECT" {
            if t.starts_with("RENDER_FILE ")
                || t.starts_with("RENDER_PATTERN ")
                || t.starts_with("RENDER_STEMS ")
            {
                return;
            }
            if t.starts_with("<RENDER_CFG") {
                skipping_cfg = true;
                return;
            }
            if let Some(r) = t.strip_prefix("RENDER_RANGE ") {
                let mut parts: Vec<&str> = r.split_whitespace().collect();
                if !parts.is_empty() {
                    parts[0] = "1";
                }
                out.push_str(&format!("  RENDER_RANGE {}\n", parts.join(" ")));
                return;
            }
            if t.starts_with("<TRACK") {
                track_top = depth == 0;
            }
        }
        if is_top(stack, "TRACK") {
            if let Some(r) = t.strip_prefix("VOLPAN ") {
                if track_top {
                    let mut parts: Vec<String> = r.split_whitespace().map(String::from).collect();
                    if let Some(v) = parts.first().and_then(|x| x.parse::<f64>().ok()) {
                        parts[0] = format!("{}", v * scale);
                        let indent = &line[..line.len() - line.trim_start().len()];
                        out.push_str(&format!("{indent}VOLPAN {}\n", parts.join(" ")));
                        scaled += 1;
                        return;
                    }
                }
            } else if let Some(r) = t.strip_prefix("ISBUS ") {
                depth += r
                    .split_whitespace()
                    .nth(1)
                    .and_then(|x| x.parse::<i64>().ok())
                    .unwrap_or(0);
            }
        }
        out.push_str(line);
        out.push('\n');
        if !header_done && t.starts_with("<REAPER_PROJECT") {
            header_done = true;
            let q = |s: &str| rpp_quote(s);
            match (q(render_dir), q(render_stem)) {
                (Ok(d), Ok(p)) => {
                    out.push_str(&format!(
                        "  RENDER_FILE {d}\n  RENDER_PATTERN {p}\n  RENDER_STEMS 0\n"
                    ));
                    out.push_str(&format!("  <RENDER_CFG\n    {RENDER_CFG_WAV24}\n  >\n"));
                }
                (Err(e), _) | (_, Err(e)) => err = Some(e),
            }
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    if gain_db.abs() > 1e-9 && scaled == 0 {
        return Err("the project has no top-level track volume to adjust".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------- render and verify

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
}

impl Target {
    pub fn within(&self, a: &Analysis) -> bool {
        let l = &a.loudness;
        l.integrated
            .is_some_and(|x| (x - self.target_lufs).abs() <= master::LUFS_TOL)
            && l.true_peak
                .is_none_or(|t| t <= self.ceiling_dbtp + master::TP_TOL)
    }
}

#[derive(Clone, Debug)]
pub struct RenderOpts {
    pub target: Option<Target>,
    /// Where renders go (numbered free names); default `<project>-render.wav` next to the project.
    pub out: Option<PathBuf>,
    pub wait: Wait,
    pub max_rerenders: usize,
    /// Plan C: fix a render that is still off target with the built-in master.
    pub builtin_fallback: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            target: None,
            out: None,
            wait: Wait::default(),
            max_rerenders: 2,
            builtin_fallback: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Attempt {
    pub project: PathBuf,
    pub render: PathBuf,
    pub gain_db: f64,
    pub analysis: Option<Analysis>,
    pub error: Option<String>,
}

impl Attempt {
    fn to_json(&self, target: Option<&Target>) -> Value {
        let l = self.analysis.as_ref().map(|a| &a.loudness);
        json!({
            "project": self.project.display().to_string(),
            "render": self.render.display().to_string(),
            "track_gain_db": r2(self.gain_db),
            "integrated_lufs": jnum(l.and_then(|l| l.integrated)),
            "true_peak_dbtp": jnum(l.and_then(|l| l.true_peak)),
            "within_target": match (target, &self.analysis) {
                (Some(t), Some(a)) => json!(t.within(a)),
                _ => Value::Null,
            },
            "error": self.error,
        })
    }
}

#[derive(Clone, Debug)]
pub struct RenderReport {
    pub attempts: Vec<Attempt>,
    pub final_path: Option<PathBuf>,
    pub within_target: Option<bool>,
    pub fallback: Option<master::MasterReport>,
    pub notes: Vec<String>,
    pub target: Option<Target>,
}

impl RenderReport {
    pub fn to_json(&self) -> Value {
        json!({
            "engine": "reaper",
            "final_path": self.final_path.as_ref().map(|p| p.display().to_string()),
            "within_target": self.within_target,
            "target": self.target.map(|t| json!({"target_lufs": t.target_lufs, "ceiling_dbtp": t.ceiling_dbtp})),
            "attempts": self.attempts.iter().map(|a| a.to_json(self.target.as_ref())).collect::<Vec<_>>(),
            "builtin_fallback": self.fallback.as_ref().map(|m| m.to_json()),
            "final": self.final_path.as_ref().and_then(|p| {
                self.attempts.iter().filter_map(|a| a.analysis.as_ref()).find(|a| &a.path == p).map(|a| a.to_json())
                    .or_else(|| self.fallback.as_ref().map(|m| m.output.to_json()))
            }),
            "notes": self.notes,
        })
    }
}

/// After REAPER exits, wait until the render is a complete WAV whose size has settled. A file
/// that never appears is given up on after `grace`; one that is still growing (a render handed to
/// a REAPER that was already running) is waited for up to `limit`.
fn wait_render_file(path: &Path, grace: Duration, limit: Duration, poll: Duration) -> bool {
    let start = Instant::now();
    let mut last = None;
    loop {
        if let Ok(m) = std::fs::metadata(path) {
            let len = m.len();
            if last == Some(len) && WavReader::open(path).is_ok() {
                return true;
            }
            last = Some(len);
        }
        let el = start.elapsed();
        if (last.is_none() && el >= grace) || el >= limit {
            return path.is_file() && WavReader::open(path).is_ok();
        }
        std::thread::sleep(poll);
    }
}

/// A render that REAPER named a little differently (for example with a numeric suffix).
fn find_alt_render(want: &Path, since: SystemTime) -> Option<PathBuf> {
    let dir = want.parent()?;
    let stem = want.file_stem()?.to_string_lossy().to_lowercase();
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"))
                && p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().to_lowercase().starts_with(&stem))
                && std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t >= since)
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

fn render_once(
    runner: &dyn Runner,
    exe: &Path,
    source_text: &str,
    project: &Path,
    want_render: &Path,
    gain_db: f64,
    w: Wait,
) -> Attempt {
    let mut a = Attempt {
        project: PathBuf::new(),
        render: PathBuf::new(),
        gain_db,
        analysis: None,
        error: None,
    };
    let fail = |mut a: Attempt, e: String| {
        a.error = Some(e);
        a
    };
    let render = match free_path(want_render) {
        Ok(p) => p,
        Err(e) => return fail(a, e),
    };
    a.render = render.clone();
    let dir = render
        .parent()
        .map(pstr)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| ".".into());
    let stem = render
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "render".into());
    let copy = match rpp_render_copy(source_text, &dir, &stem, gain_db) {
        Ok(c) => c,
        Err(e) => return fail(a, e),
    };
    let pstem = project
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let (out, mut f) = match OutFile::create(
        &project.with_file_name(format!("{pstem}-tempo-render.rpp")),
        false,
        &[],
    ) {
        Ok(x) => x,
        Err(e) => return fail(a, e),
    };
    if let Err(e) = f.write_all(copy.as_bytes()).and_then(|_| f.sync_all()) {
        return fail(a, format!("{e} {}", out.left_note()));
    }
    drop(f);
    let copy_path = match out.commit() {
        Ok(p) => p,
        Err(e) => return fail(a, e),
    };
    a.project = copy_path.clone();
    let started = SystemTime::now() - Duration::from_secs(2);
    let mut p = match runner.start(&render_cmd(exe, &copy_path)) {
        Ok(p) => p,
        Err(e) => return fail(a, e),
    };
    match wait_for(p.as_mut(), w, &mut || false) {
        Ok(WaitEnd::TimedOut) => {
            return fail(
                a,
                format!(
                    "REAPER did not finish rendering within {} s (process {} is left running)",
                    w.timeout.as_secs(),
                    p.id()
                ),
            )
        }
        Err(e) => return fail(a, e),
        Ok(_) => {}
    }
    let grace = w.timeout.min(Duration::from_secs(10));
    let got = if wait_render_file(&render, grace, w.timeout, w.poll) {
        render.clone()
    } else if let Some(alt) = find_alt_render(&render, started) {
        alt
    } else {
        return fail(
            a,
            format!(
                "REAPER exited but wrote no render at {} (is the project's render set to the master mix?)",
                render.display()
            ),
        );
    };
    a.render = got.clone();
    match loudness::analyze_file(&got) {
        Ok(an) => a.analysis = Some(an),
        Err(e) => a.error = Some(e),
    }
    a
}

/// Render `project` headlessly and verify it; off-target renders are redone (at most
/// `max_rerenders` times) from a new project copy with every top-level track moved by the miss.
/// `first` is a render that already exists for this project at gain 0 (from a session run).
pub fn render_project(
    runner: &dyn Runner,
    exe: &Path,
    project: &Path,
    first: Option<PathBuf>,
    o: &RenderOpts,
) -> Result<RenderReport, String> {
    let text =
        std::fs::read_to_string(project).map_err(|e| format!("{}: {e}", project.display()))?;
    rpp_render_copy(&text, ".", "probe", 0.0)?;
    let pstem = project
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let want = o
        .out
        .clone()
        .unwrap_or_else(|| project.with_file_name(format!("{pstem}-render.wav")));
    let mut notes = Vec::new();
    let mut attempts: Vec<Attempt> = Vec::new();
    let mut gain = 0.0;
    let from_session = first.is_some();
    if let Some(r) = first {
        let analysis = loudness::analyze_file(&r);
        attempts.push(Attempt {
            project: project.to_path_buf(),
            render: r,
            gain_db: 0.0,
            error: analysis.as_ref().err().cloned(),
            analysis: analysis.ok(),
        });
    }
    loop {
        if let Some(last) = attempts.last() {
            match (&last.analysis, o.target) {
                // The session's own render is unreadable: render the project headlessly instead.
                (None, _) if from_session && attempts.len() == 1 => {
                    notes.push(format!(
                        "the session render could not be read ({}); rendering headlessly",
                        last.error.clone().unwrap_or_default()
                    ));
                }
                (None, _) | (Some(_), None) => break,
                (Some(a), Some(t)) => {
                    if t.within(a) {
                        break;
                    }
                    let Some(l) = a.loudness.integrated else {
                        notes.push("the render is silent; not adjusting".into());
                        break;
                    };
                    gain += t.target_lufs - l;
                    notes.push(format!(
                        "render {} read {l:.2} LUFS; rendering again with {gain:+.2} dB on every top-level track",
                        attempts.len()
                    ));
                }
            }
        }
        if attempts.len() > o.max_rerenders {
            break;
        }
        attempts.push(render_once(
            runner, exe, &text, project, &want, gain, o.wait,
        ));
    }
    let rendered: Vec<usize> = (0..attempts.len())
        .filter(|&i| attempts[i].analysis.is_some())
        .collect();
    if rendered.is_empty() {
        let why = attempts
            .iter()
            .filter_map(|a| a.error.clone())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("REAPER render failed: {why}"));
    }
    let Some(t) = o.target else {
        let last = &attempts[rendered[rendered.len() - 1]];
        return Ok(RenderReport {
            final_path: Some(last.render.clone()),
            within_target: None,
            attempts: attempts.clone(),
            fallback: None,
            notes,
            target: None,
        });
    };
    let key = |i: usize| {
        let an = attempts[i].analysis.as_ref().expect("rendered");
        let miss = an
            .loudness
            .integrated
            .map(|l| (l - t.target_lufs).abs())
            .unwrap_or(f64::INFINITY);
        (!t.within(an), miss)
    };
    let best_i = rendered
        .iter()
        .copied()
        .min_by(|&a, &b| {
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("at least one render");
    let best = &attempts[best_i];
    let best_an = best.analysis.as_ref().expect("rendered");
    if t.within(best_an) {
        return Ok(RenderReport {
            final_path: Some(best.render.clone()),
            within_target: Some(true),
            attempts: attempts.clone(),
            fallback: None,
            notes,
            target: Some(t),
        });
    }
    notes.push(format!(
        "REAPER's best render reads {} LUFS, {} dBTP (target {} LUFS, ceiling {} dBTP)",
        crate::util::fmt_db(best_an.loudness.integrated),
        crate::util::fmt_db(best_an.loudness.true_peak),
        t.target_lufs,
        t.ceiling_dbtp
    ));
    let mut fallback = None;
    let mut final_path = Some(best.render.clone());
    let mut within = false;
    if o.builtin_fallback {
        let s = MasterSettings::new(t.target_lufs, t.ceiling_dbtp)?;
        match master::master_file(&best.render, &master::default_out(&best.render), false, &s) {
            Ok(m) => {
                notes.push(format!(
                    "plan C: the built-in master fixed the best render: {}",
                    m.path.display()
                ));
                within = m.within_target;
                final_path = Some(m.path.clone());
                fallback = Some(m);
            }
            Err(e) => notes.push(format!("plan C (built-in master) failed too: {e}")),
        }
    }
    Ok(RenderReport {
        final_path,
        within_target: Some(within),
        attempts: attempts.clone(),
        fallback,
        notes,
        target: Some(t),
    })
}

// ---------------------------------------------------------------- the session pipeline

#[derive(Clone, Debug)]
pub struct SessionRun {
    pub script: PathBuf,
    pub plan: SessionPlan,
    pub status: Status,
    pub pid: u32,
}

impl SessionRun {
    pub fn to_json(&self) -> Value {
        json!({
            "script": self.script.display().to_string(),
            "session": self.plan.to_json(),
            "status": self.status.to_json(),
            "pid": self.pid,
        })
    }
}

/// Write the session script, start REAPER with it (visible) and wait for the status file.
pub fn run_session(
    runner: &dyn Runner,
    exe: &Path,
    os: Os,
    plan: &SessionPlan,
    script_want: &Path,
    w: Wait,
) -> Result<SessionRun, String> {
    let script = write_script(plan, script_want)?;
    let mut p = runner.start(&open_cmd(exe, &script, os))?;
    let status_path = plan.status_path.clone();
    let mut done = || read_status(&status_path).is_some_and(|s| s.done);
    let start = Instant::now();
    loop {
        let left = w.timeout.saturating_sub(start.elapsed());
        match wait_for(
            p.as_mut(),
            Wait {
                timeout: left,
                poll: w.poll,
            },
            &mut done,
        )? {
            WaitEnd::Ready => break,
            WaitEnd::Exited(code) if code != 0 => {
                return Err(format!(
                    "REAPER exited with code {code} before the session script finished (script {})",
                    script.display()
                ))
            }
            // Exit 0 can be a launcher that handed the script to a running REAPER: keep polling.
            WaitEnd::Exited(_) | WaitEnd::TimedOut => {
                if start.elapsed() >= w.timeout {
                    return Err(timeout_msg(&script, &status_path, w, p.id()));
                }
                std::thread::sleep(w.poll);
            }
        }
    }
    let status = read_status(&status_path).ok_or("the status file went away")?;
    Ok(SessionRun {
        script,
        plan: plan.clone(),
        status,
        pid: p.id(),
    })
}

fn timeout_msg(script: &Path, status: &Path, w: Wait, pid: u32) -> String {
    format!(
        "no status from the REAPER session script after {} s (REAPER may be showing a dialog; process {pid} is left running). \
         Script {}; status would be {}",
        w.timeout.as_secs(),
        script.display(),
        status.display()
    )
}

/// Plan A end to end: session (build, save, render) in a visible REAPER, then verify the render
/// and re-render headlessly when it misses the target.
pub fn session_and_render(
    runner: &dyn Runner,
    exe: &Path,
    os: Os,
    plan: &SessionPlan,
    script_want: &Path,
    target: Option<Target>,
    w: Wait,
) -> Result<Value, String> {
    let mut plan = plan.clone();
    plan.render = true;
    let run = run_session(runner, exe, os, &plan, script_want, w)?;
    let st = &run.status;
    if let Some(e) = &st.error {
        return Err(format!("the REAPER session script failed: {e}"));
    }
    let project = st
        .project
        .clone()
        .filter(|p| p.is_file())
        .ok_or("the REAPER session did not save its project")?;
    if st.items < plan.tracks.len() as u64 {
        return Err(format!(
            "REAPER put {} of {} stems on tracks: {}",
            st.items,
            plan.tracks.len(),
            st.warnings.join("; ")
        ));
    }
    let first = st
        .render_file
        .clone()
        .filter(|p| st.rendered && p.is_file());
    let mut notes = Vec::new();
    if first.is_none() {
        notes
            .push("the session did not render; rendering the saved project headlessly".to_string());
    }
    if !st.fx_missing.is_empty() {
        notes.push(format!(
            "FX that could not be added: {}",
            st.fx_missing.join(", ")
        ));
    }
    let opts = RenderOpts {
        target,
        out: Some(plan.render_path.clone()),
        wait: w,
        ..RenderOpts::default()
    };
    let rep = render_project(runner, exe, &project, first, &opts)?;
    let mut j = rep.to_json();
    j["session"] = run.to_json();
    if let Some(a) = j["notes"].as_array_mut() {
        for n in notes {
            a.insert(0, json!(n));
        }
    }
    Ok(j)
}

#[cfg(test)]
pub mod testfake {
    //! A stand-in for REAPER behind the `Runner` trait, shared by the tests of this crate.
    use super::*;
    use crate::loudness::testsig::{db, fade, sine};
    use crate::wav::{write_new, OutFormat};
    use std::cell::RefCell;

    /// Stands in for REAPER. `-renderproject` writes a WAV whose loudness is `unity_lufs` plus
    /// the first top-level track's volume (capped at `cap_lufs`, like a limiter would). A .lua
    /// writes the status, a project and (when asked) the render.
    pub struct FakeReaper {
        pub calls: RefCell<Vec<Cmd>>,
        pub unity_lufs: f64,
        pub cap_lufs: f64,
        pub render_ok: bool,
        pub status_override: Option<String>,
        pub exit_code: i32,
    }

    impl FakeReaper {
        pub fn new(unity_lufs: f64) -> FakeReaper {
            FakeReaper {
                calls: RefCell::new(Vec::new()),
                unity_lufs,
                cap_lufs: 0.0,
                render_ok: true,
                status_override: None,
                exit_code: 0,
            }
        }
    }

    pub struct FakeProc(pub i32);
    impl Proc for FakeProc {
        fn id(&self) -> u32 {
            4242
        }
        fn try_wait(&mut self) -> Result<Option<i32>, String> {
            Ok(Some(self.0))
        }
    }

    pub fn tone_at(path: &Path, lufs: f64) {
        // Stereo 1 kHz tone: integrated loudness equals its peak level in dBFS.
        let sig = fade(sine(48000, 2, 3.0, 1000.0, db(lufs), 0.0), 48000, 2, 0.01);
        write_new(path, 48000, 2, OutFormat::Pcm24, &sig).unwrap();
    }

    pub const FAKE_RPP: &str = "<REAPER_PROJECT 0.1 \"7.22/fake\" 1\n  <TRACK {1}\n    NAME a\n    VOLPAN 1 0 -1 -1 1\n    ISBUS 0 0\n  >\n>\n";

    impl Runner for FakeReaper {
        fn start(&self, cmd: &Cmd) -> Result<Box<dyn Proc>, String> {
            self.calls.borrow_mut().push(cmd.clone());
            if let Some(i) = cmd.args.iter().position(|a| a == "-renderproject") {
                let text = std::fs::read_to_string(&cmd.args[i + 1]).map_err(|e| e.to_string())?;
                let info = rpp_info(&text);
                let out = PathBuf::from(info.render_file.unwrap())
                    .join(format!("{}.wav", info.render_pattern.unwrap()));
                if self.render_ok {
                    let vol = info.top_level_volumes.first().copied().unwrap_or(1.0);
                    let l = (self.unity_lufs + 20.0 * vol.log10()).min(self.cap_lufs);
                    tone_at(&out, l);
                }
                return Ok(Box::new(FakeProc(self.exit_code)));
            }
            let script = cmd.args.last().unwrap();
            if !script.ends_with(".lua") {
                // Opening a project or media file: nothing to do.
                return Ok(Box::new(FakeProc(self.exit_code)));
            }
            let text = std::fs::read_to_string(script).map_err(|e| e.to_string())?;
            let tag: Value = serde_json::from_str(
                text.lines()
                    .find_map(|l| l.strip_prefix(SESSION_TAG))
                    .unwrap(),
            )
            .unwrap();
            let (status, project, render) = (
                tag["status"].as_str().unwrap(),
                tag["project"].as_str().unwrap(),
                tag["render"].as_str().unwrap(),
            );
            std::fs::write(project, FAKE_RPP).unwrap();
            let do_render = text.contains("do_render = true");
            if do_render && self.render_ok {
                tone_at(Path::new(render), self.unity_lufs.min(self.cap_lufs));
            }
            let n = text.matches("    { name = ").count();
            let st = self.status_override.clone().unwrap_or_else(|| {
                json!({"done": true, "ok": true, "steps": ["new_project"], "warnings": [], "fx_missing": [],
                       "tracks": n, "items": n, "project": project, "render_file": render, "rendered": do_render && self.render_ok})
                .to_string()
            });
            std::fs::write(status, st).unwrap();
            Ok(Box::new(FakeProc(self.exit_code)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testfake::*;
    use super::*;
    use crate::util::testutil::tmpdir;

    // ------------------------------------------------ locate and commands

    fn probe_set(
        files: &'static [&'static str],
        dirs: &'static [&'static str],
    ) -> impl Fn(&str) -> Kind {
        move |p: &str| {
            if files.contains(&p) {
                Kind::File
            } else if dirs.contains(&p) {
                Kind::Dir
            } else {
                Kind::Missing
            }
        }
    }

    fn env<'a>(os: Os, probe: &'a dyn Fn(&str) -> Kind) -> LocateEnv<'a> {
        LocateEnv {
            flag: None,
            env: None,
            config: None,
            os,
            path_var: None,
            program_files: None,
            probe,
        }
    }

    #[test]
    fn locate_order_flag_env_config_then_standard_paths() {
        let pr = probe_set(
            &[
                "/x/flag-reaper",
                "/x/env-reaper",
                "/x/cfg-reaper",
                "/opt/REAPER/reaper",
            ],
            &[],
        );
        let mut e = env(Os::Linux, &pr);
        e.flag = Some("/x/flag-reaper");
        e.env = Some("/x/env-reaper".into());
        e.config = Some("/x/cfg-reaper");
        assert_eq!(locate(&e).unwrap().source, "--reaper");
        e.flag = None;
        assert_eq!(locate(&e).unwrap().source, "env TEMPO_REAPER");
        e.env = None;
        assert_eq!(locate(&e).unwrap().source, "config reaper_exe");
        e.config = None;
        let l = locate(&e).unwrap();
        assert_eq!(
            (l.exe.to_str().unwrap(), l.source.as_str()),
            ("/opt/REAPER/reaper", "standard install path")
        );
        // A flag that points nowhere is reported and the search goes on.
        e.flag = Some("/nope/reaper");
        let l = locate(&e).unwrap();
        assert_eq!(l.skipped[0].why, "does not exist");
        assert_eq!(l.skipped[0].source, "--reaper");
    }

    #[test]
    fn locate_per_os_standard_paths_folders_and_path() {
        // Windows: Program Files (x64) first, %ProgramFiles% on another drive before it.
        let pr = probe_set(&[r"C:\Program Files\REAPER\reaper.exe"], &[]);
        let e = env(Os::Windows, &pr);
        assert_eq!(
            locate(&e).unwrap().exe,
            PathBuf::from(r"C:\Program Files\REAPER\reaper.exe")
        );
        assert_eq!(
            standard_paths(Os::Windows, Some(r"D:\Program Files"))[0],
            r"D:\Program Files\REAPER (x64)\reaper.exe"
        );
        assert_eq!(
            standard_paths(Os::Windows, None)[0],
            r"C:\Program Files\REAPER (x64)\reaper.exe"
        );
        // Windows PATH uses ';' and reaper.exe.
        let pr = probe_set(&[r"E:\Tools\REAPER\reaper.exe"], &[]);
        let mut e = env(Os::Windows, &pr);
        e.path_var = Some(r"C:\Windows;E:\Tools\REAPER\".into());
        let l = locate(&e).unwrap();
        assert_eq!(
            (l.exe.to_str().unwrap(), l.source.as_str()),
            (r"E:\Tools\REAPER\reaper.exe", "PATH")
        );
        // macOS: the app bundle binary; a .app folder given by the user is looked inside.
        let pr = probe_set(
            &["/Applications/REAPER.app/Contents/MacOS/REAPER"],
            &["/Applications/REAPER.app"],
        );
        let mut e = env(Os::Mac, &pr);
        assert_eq!(
            locate(&e).unwrap().exe,
            PathBuf::from("/Applications/REAPER.app/Contents/MacOS/REAPER")
        );
        e.flag = Some("/Applications/REAPER.app");
        let l = locate(&e).unwrap();
        assert_eq!(
            (l.source.as_str(), l.exe.to_str().unwrap()),
            ("--reaper", "/Applications/REAPER.app/Contents/MacOS/REAPER")
        );
        // Linux: PATH before /opt.
        let pr = probe_set(&["/usr/local/bin/reaper", "/opt/REAPER/reaper"], &[]);
        let mut e = env(Os::Linux, &pr);
        e.path_var = Some("/usr/bin:/usr/local/bin".into());
        assert_eq!(locate(&e).unwrap().source, "PATH");
        // A Windows install folder given in the config.
        let pr = probe_set(&[r"D:\REAPER\reaper.exe"], &[r"D:\REAPER"]);
        let mut e = env(Os::Windows, &pr);
        e.config = Some(r"D:\REAPER");
        assert_eq!(
            locate(&e).unwrap().exe,
            PathBuf::from(r"D:\REAPER\reaper.exe")
        );
    }

    #[test]
    fn not_found_lists_every_path_tried() {
        let pr = probe_set(&[], &[]);
        let mut e = env(Os::Windows, &pr);
        e.flag = Some(r"C:\wrong\reaper.exe");
        e.path_var = Some(r"C:\Windows".into());
        let nf = locate(&e).unwrap_err();
        let paths: Vec<&str> = nf.tried.iter().map(|t| t.path.as_str()).collect();
        assert!(paths.contains(&r"C:\wrong\reaper.exe"));
        assert!(paths.contains(&r"C:\Program Files\REAPER (x64)\reaper.exe"));
        assert!(paths.contains(&r"C:\Program Files\REAPER\reaper.exe"));
        assert!(nf.tried.iter().any(|t| t.source == "PATH"));
        let j = nf.to_json();
        assert_eq!(j["found"], false);
        assert!(j["message"].as_str().unwrap().contains("TEMPO_REAPER"));
        let pr = probe_set(&[], &[]);
        let e = env(Os::Mac, &pr);
        let nf = locate(&e).unwrap_err();
        assert!(nf
            .tried
            .iter()
            .any(|t| t.path == "/Applications/REAPER.app/Contents/MacOS/REAPER"));
        assert!(nf.tried.iter().any(|t| t.why == "PATH is not set"));
        let e = env(Os::Linux, &pr);
        assert!(locate(&e)
            .unwrap_err()
            .tried
            .iter()
            .any(|t| t.path == "/opt/REAPER/reaper"));
    }

    #[test]
    fn command_lines_per_os() {
        // Windows: the exe itself; a render is a background child (CREATE_NO_WINDOW), opening is not.
        let win = Path::new(r"C:\Program Files\REAPER (x64)\reaper.exe");
        let r = render_cmd(win, Path::new(r"D:\Music\song.rpp"));
        assert_eq!(r.program, win);
        assert_eq!(
            r.args,
            vec![
                "-newinst",
                "-nosplash",
                "-renderproject",
                r"D:\Music\song.rpp"
            ]
        );
        assert!(r.background);
        let o = open_cmd(win, Path::new(r"D:\out\session.lua"), Os::Windows);
        assert_eq!(
            (o.program.as_path(), o.args.clone(), o.background),
            (win, vec![r"D:\out\session.lua".to_string()], false)
        );
        assert_eq!(
            o.display(),
            r#""C:\Program Files\REAPER (x64)\reaper.exe" D:\out\session.lua"#
        );
        // macOS: opening goes through `open -a <bundle>`; rendering runs the binary in the bundle.
        let mac = Path::new("/Applications/REAPER.app/Contents/MacOS/REAPER");
        let o = open_cmd(mac, Path::new("/Users/x/song.rpp"), Os::Mac);
        assert_eq!(o.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(
            o.args,
            vec!["-a", "/Applications/REAPER.app", "/Users/x/song.rpp"]
        );
        let r = render_cmd(mac, Path::new("/Users/x/song.rpp"));
        assert_eq!(r.program, mac);
        assert_eq!(r.args[2], "-renderproject");
        // Linux.
        let lin = Path::new("/opt/REAPER/reaper");
        assert_eq!(
            open_cmd(lin, Path::new("/m/a.wav"), Os::Linux).args,
            vec!["/m/a.wav"]
        );
        assert_eq!(render_cmd(lin, Path::new("/m/p.rpp")).args.len(), 4);
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }

    // ------------------------------------------------ Lua

    #[test]
    fn lua_escaping_covers_quotes_backslashes_newlines_and_controls() {
        assert_eq!(
            lua_str(r"C:\Users\me\song.wav"),
            r#""C:\\Users\\me\\song.wav""#
        );
        assert_eq!(lua_str("say \"hi\""), r#""say \"hi\"""#);
        assert_eq!(lua_str("a\nb\rc\td"), r#""a\nb\rc\td""#);
        assert_eq!(
            lua_str("nul\0bell\u{7}del\u{7f}"),
            r#""nul\000bell\007del\127""#
        );
        assert_eq!(lua_str("Beyoncé — ]]--[[ end"), "\"Beyoncé — ]]--[[ end\"");
        assert_eq!(lua_str(""), "\"\"");
        // Property: the inside has no raw newline and every quote is escaped.
        for s in ["\"", "\\\"", "x\\", "\n\n\"\\\r", "'single'", "\\\\\\"] {
            let q = lua_str(s);
            let inner = &q[1..q.len() - 1];
            assert!(!inner.contains('\n') && !inner.contains('\r'), "{q}");
            let b = inner.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                assert_ne!(b[i], b'"', "unescaped quote in {q}");
                i += 1;
            }
        }
    }

    fn plan_json(dir: &Path) -> Value {
        json!({
            "kind": "mix_plan",
            "sample_rate": 44100,
            "trim_db": -2.0,
            "stems": [
                {"path": dir.join("Kick \"In\".wav").display().to_string(), "name": "Kick \"In\"", "gain_db": -3.0, "highpass_hz": null, "compress": true, "lufs": -20.0},
                {"path": "vox\\lead.wav", "name": "Lead\nVox", "gain_db": 1.5, "highpass_hz": 90.0, "compress": true, "lufs": -30.0},
                {"path": dir.join("pad.wav").display().to_string(), "name": "pad", "gain_db": 20.0, "highpass_hz": 120.0, "compress": false}
            ],
            "mix": {"integrated_lufs": -18.0},
            "master": {"preset": "streaming", "target_lufs": -14.0, "ceiling_dbtp": -1.0, "gain_db": 3.5}
        })
    }

    #[test]
    fn session_plan_from_mix_plan_json() {
        let d = tmpdir("splan");
        let s = SessionPlan::from_plan_json(&plan_json(&d), &d, &d.join("song.lua")).unwrap();
        assert_eq!(s.sample_rate, 44100);
        assert_eq!(s.tracks.len(), 3);
        // volume = stem gain + trim + master gain (pre-limiter)
        assert!((s.tracks[0].volume_db - (-3.0 - 2.0 + 3.5)).abs() < 1e-9);
        assert_eq!(s.tracks[1].highpass_hz, Some(90.0));
        assert_eq!(s.tracks[1].file, d.join("vox\\lead.wav"));
        assert!((s.tracks[1].comp_threshold_db + 26.0).abs() < 1e-9);
        assert_eq!(s.tracks[2].comp_threshold_db, -18.0);
        let m = s.master.as_ref().unwrap();
        assert!((m.limiter_ceiling_db + 1.5).abs() < 1e-9);
        assert_eq!(s.project_path, d.join("song.rpp"));
        assert_eq!(s.render_path, d.join("song.wav"));
        assert_eq!(s.status_path, d.join("song.status.json"));
        // Without gain_db the master gain comes from the mix loudness.
        let mut pj = plan_json(&d);
        pj["master"].as_object_mut().unwrap().remove("gain_db");
        let s2 = SessionPlan::from_plan_json(&pj, &d, &d.join("x.lua")).unwrap();
        assert!((s2.tracks[0].volume_db - (-3.0 - 2.0 + 4.0)).abs() < 1e-9);
        // Errors.
        assert!(SessionPlan::from_plan_json(&json!({"stems": []}), &d, &d.join("a.lua")).is_err());
        assert!(SessionPlan::from_plan_json(
            &json!({"stems": [{"name": "x"}]}),
            &d,
            &d.join("a.lua")
        )
        .unwrap_err()
        .contains("no path"));
        let bad = json!({"stems": [{"path": "/a.wav"}], "master": {"target_lufs": -14.0, "ceiling_dbtp": 3.0}});
        assert!(SessionPlan::from_plan_json(&bad, &d, &d.join("a.lua")).is_err());
        // Existing outputs are never reused.
        std::fs::write(d.join("taken.rpp"), "x").unwrap();
        let s3 = SessionPlan::from_plan_json(&plan_json(&d), &d, &d.join("taken.lua")).unwrap();
        assert_eq!(s3.project_path, d.join("taken-2.rpp"));
    }

    #[test]
    fn session_script_structure_escaping_and_fx_names() {
        let d = tmpdir("script");
        let mut s = SessionPlan::from_plan_json(&plan_json(&d), &d, &d.join("song.lua")).unwrap();
        s.render = true;
        let lua = session_script(&s);
        // Paths and names are escaped Lua literals.
        assert!(lua.contains(&format!(
            "file = {}",
            lua_str(&pstr(&d.join("Kick \"In\".wav")))
        )));
        assert!(lua.contains(r#"name = "Lead\nVox""#));
        assert!(lua.contains(r#"name = "Kick \"In\"""#));
        assert!(lua.contains("vox\\\\lead.wav"));
        assert!(lua.contains("hp_hz = 90.0000"));
        assert!(lua.contains("do_render = true") && lua.contains("do_close = false"));
        assert!(lua.contains("master = { limiter = true, ceiling_db = -1.5000 }"));
        // The planned paths are in the machine-readable tag line.
        let tag = lua
            .lines()
            .find_map(|l| l.strip_prefix(SESSION_TAG))
            .unwrap();
        let tag: Value = serde_json::from_str(tag).unwrap();
        assert_eq!(tag["status"], pstr(&s.status_path));
        // Stock Cockos FX and the well-known API calls.
        for needle in [
            "\"ReaEQ\"",
            "\"ReaComp\"",
            "\"ReaLimit\"",
            "BANDTYPE0",
            "InsertTrackAtIndex",
            "GetTrack(0, idx)",
            "\"P_NAME\"",
            "\"D_VOL\"",
            "SetOnlyTrackSelected",
            "SetEditCurPos",
            "InsertMedia",
            "TrackFX_AddByName",
            "GetMasterTrack",
            "\"RENDER_FILE\"",
            "\"RENDER_PATTERN\"",
            "\"RENDER_FORMAT\"",
            RENDER_CFG_WAV24,
            "\"RENDER_SRATE\"",
            "\"RENDER_BOUNDSFLAG\"",
            "Main_SaveProjectEx",
            "Main_OnCommand(42230, 0)",
            "Main_OnCommand(40859, 0)",
            "write_status()",
        ] {
            assert!(lua.contains(needle), "missing {needle}");
        }
        // Every optional call is guarded: each reaper.X( used in the body has a has("X") check,
        // except the core track calls that run inside pcall(add_track).
        let core = [
            "InsertTrackAtIndex",
            "GetTrack",
            "GetSetMediaTrackInfo_String",
            "SetMediaTrackInfo_Value",
        ];
        let mut i = 0;
        let b = LUA_BODY;
        while let Some(k) = b[i..].find("reaper.") {
            let start = i + k + 7;
            let name: String = b[start..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            i = start + name.len();
            if core.contains(&name.as_str()) {
                continue;
            }
            assert!(
                b.contains(&format!("has(\"{name}\")")),
                "reaper.{name} is used without a has() check"
            );
        }
        // Balanced structure (a cheap syntax check; the Lua 5.4 stub run is in the report).
        let opens = lua.matches("function").count()
            + lua.matches(" then").count()
            + lua.matches(" do\n").count();
        assert!(opens > 50);
        // Writing the script never overwrites.
        let p1 = write_script(&s, &d.join("song.lua")).unwrap();
        let p2 = write_script(&s, &d.join("song.lua")).unwrap();
        assert_eq!((p1, p2), (d.join("song.lua"), d.join("song-2.lua")));
    }

    // ------------------------------------------------ status

    #[test]
    fn status_file_parsing() {
        let full = r#"{"app":"7.22/win64","done":true,"error":null,"fx_missing":["ReaLimit"],"items":3,"ok":true,
            "project":"D:\\out\\song.rpp","render_file":"D:\\out\\song.wav","rendered":true,
            "steps":["new_project","track:Kick"],"tool":"perennos-tempo-studio","tracks":3,"version":1,
            "warnings":["ReaLimit ceiling could not be set"]}"#;
        let s = parse_status(full).unwrap();
        assert!(s.done && s.ok && s.rendered);
        assert_eq!((s.tracks, s.items), (3, 3));
        assert_eq!(s.fx_missing, vec!["ReaLimit"]);
        assert_eq!(s.project, Some(PathBuf::from(r"D:\out\song.rpp")));
        assert_eq!(s.app.as_deref(), Some("7.22/win64"));
        assert_eq!(s.error, None);
        // What the Lua encoder writes for an empty list and a failed run.
        let failed = parse_status(r#"{"done":true,"ok":false,"error":"[string] attempt to index nil","steps":[],"warnings":[]}"#).unwrap();
        assert!(!failed.ok);
        assert!(failed.error.unwrap().contains("index nil"));
        // Written by the generated script's own JSON encoder under Lua 5.4 (stub run, bare API):
        // escaped quotes and backslashes come back exactly.
        let from_lua = r#"{"done":true,"fx_missing":["ReaComp on Kick \"In\"","ReaEQ (high-pass) on Lead Vox\\1","ReaLimit"],"items":0,"ok":true,"rendered":false,"steps":["track:Kick \"In\"","track:Lead Vox\\1"],"tool":"perennos-tempo-studio","tracks":3,"version":1,"warnings":["could not put D:/out/Kick \"In\".wav on track Kick \"In\"","the project could not be saved"]}"#;
        let s = parse_status(from_lua).unwrap();
        assert_eq!(s.fx_missing[0], "ReaComp on Kick \"In\"");
        assert_eq!(s.fx_missing[1], "ReaEQ (high-pass) on Lead Vox\\1");
        assert_eq!((s.tracks, s.items, s.rendered), (3, 0, false));
        assert_eq!(s.warnings.len(), 2);
        // Missing fields are defaults; a BOM is fine.
        let s = parse_status("\u{feff}{\"done\":true}").unwrap();
        assert!(s.done && s.steps.is_empty() && s.project.is_none());
        // Half-written, empty or not an object: errors (the caller keeps polling).
        assert!(parse_status(r#"{"done":tr"#).is_err());
        assert!(parse_status("").unwrap_err().contains("empty"));
        assert!(parse_status("[1,2]").is_err());
        let d = tmpdir("status");
        assert!(read_status(&d.join("none.json")).is_none());
        std::fs::write(d.join("half.json"), r#"{"done": tr"#).unwrap();
        assert!(read_status(&d.join("half.json")).is_none());
    }

    // ------------------------------------------------ RPP

    const RPP: &str = "<REAPER_PROJECT 0.1 \"7.22/linux-x86_64\" 1730000000\n  RIPPLE 0\n  RENDER_FILE \"/old/dir\"\n  RENDER_PATTERN \"old name\"\n  RENDER_RANGE 2 0 0 18 1000\n  RENDER_STEMS 3\n  <RENDER_CFG\n    bDNwbSAAAAABAAAA\n  >\n  MASTER_VOLUME 1 0 -1 -1 1\n  <TRACK {A}\n    NAME Bus\n    VOLPAN 1 0 -1 -1 1\n    ISBUS 1 1\n  >\n  <TRACK {B}\n    NAME Child\n    VOLPAN 0.5 0 -1 -1 1\n    ISBUS 2 -1\n    <ITEM\n      VOLPAN 1 0 1 -1\n    >\n  >\n  <TRACK {C}\n    NAME Solo\n    VOLPAN 2 0 -1 -1 1\n    ISBUS 0 0\n  >\n>\n";

    #[test]
    fn rpp_copy_sets_render_target_and_moves_top_level_tracks_only() {
        let info = rpp_info(RPP);
        assert_eq!(info.render_file.as_deref(), Some("/old/dir"));
        assert_eq!(info.render_pattern.as_deref(), Some("old name"));
        assert_eq!(info.tracks, 3);
        assert_eq!(info.top_level_volumes, vec![1.0, 2.0]);
        let c = rpp_render_copy(RPP, "/new/dir", "song-render", 6.0).unwrap();
        let ci = rpp_info(&c);
        assert_eq!(ci.render_file.as_deref(), Some("/new/dir"));
        assert_eq!(ci.render_pattern.as_deref(), Some("song-render"));
        let g = db_to(6.0);
        assert!(
            (ci.top_level_volumes[0] - g).abs() < 1e-9
                && (ci.top_level_volumes[1] - 2.0 * g).abs() < 1e-9
        );
        // The folder child, the item volume and the master fader are untouched.
        assert!(c.contains("    VOLPAN 0.5 0 -1 -1 1\n"));
        assert!(c.contains("      VOLPAN 1 0 1 -1\n"));
        assert!(c.contains("MASTER_VOLUME 1 0 -1 -1 1"));
        // WAV 24-bit, master mix, whole project; the old settings are gone.
        assert!(c.contains(&format!("<RENDER_CFG\n    {RENDER_CFG_WAV24}\n  >")));
        assert!(!c.contains("bDNwbSAAAAABAAAA") && !c.contains("old name"));
        assert!(c.contains("RENDER_STEMS 0") && !c.contains("RENDER_STEMS 3"));
        assert!(c.contains("RENDER_RANGE 1 0 0 18 1000"));
        assert_eq!(c.matches("<TRACK").count(), 3);
        assert_eq!(
            c.matches('<').count(),
            c.lines().filter(|l| l.trim() == ">").count()
        );
        // Quoting and errors.
        assert_eq!(rpp_quote("a b").unwrap(), "\"a b\"");
        assert_eq!(rpp_quote("say \"x\"").unwrap(), "'say \"x\"'");
        assert!(rpp_quote("a\nb").is_err());
        assert!(rpp_render_copy("hello", "/d", "s", 0.0)
            .unwrap_err()
            .contains("not a REAPER project"));
        // A byte-order mark in front changes nothing.
        let bom =
            rpp_render_copy(&format!("\u{feff}{RPP}"), "/new/dir", "song-render", 6.0).unwrap();
        assert_eq!(bom, c);
        let no_tracks = "<REAPER_PROJECT 0.1\n>\n";
        assert!(rpp_render_copy(no_tracks, "/d", "s", 0.0).is_ok());
        assert!(rpp_render_copy(no_tracks, "/d", "s", 1.0)
            .unwrap_err()
            .contains("no top-level track"));
    }

    fn db_to(x: f64) -> f64 {
        crate::util::db_to_amp(x)
    }

    fn fast() -> Wait {
        Wait {
            timeout: Duration::from_secs(5),
            poll: Duration::from_millis(5),
        }
    }

    fn project(dir: &Path) -> PathBuf {
        let p = dir.join("song.rpp");
        std::fs::write(&p, FAKE_RPP).unwrap();
        p
    }

    fn opts(target: Option<Target>) -> RenderOpts {
        RenderOpts {
            target,
            wait: fast(),
            ..RenderOpts::default()
        }
    }

    const STREAMING: Target = Target {
        target_lufs: -14.0,
        ceiling_dbtp: -1.0,
    };

    #[test]
    fn verify_loop_re_renders_once_to_hit_the_target() {
        let d = tmpdir("loop1");
        let rpp = project(&d);
        let fake = FakeReaper::new(-17.0);
        let rep = render_project(
            &fake,
            Path::new("/fake/reaper"),
            &rpp,
            None,
            &opts(Some(STREAMING)),
        )
        .unwrap();
        assert_eq!(rep.attempts.len(), 2, "{:#}", rep.to_json());
        assert_eq!(rep.within_target, Some(true));
        assert!((rep.attempts[1].gain_db - 3.0).abs() < 0.05);
        let fin = rep.final_path.clone().unwrap();
        assert_eq!(fin, d.join("song-render-2.wav"));
        let l = loudness::analyze_file(&fin)
            .unwrap()
            .loudness
            .integrated
            .unwrap();
        assert!((l + 14.0).abs() <= 0.5, "{l}");
        // Both renders used a fresh project copy and a headless background command.
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls
            .iter()
            .all(|c| c.background && c.args[2] == "-renderproject"));
        assert_ne!(calls[0].args[3], calls[1].args[3]);
        assert!(
            d.join("song-tempo-render.rpp").is_file()
                && d.join("song-tempo-render-2.rpp").is_file()
        );
        assert_eq!(
            std::fs::read_to_string(&rpp).unwrap(),
            FAKE_RPP,
            "the user's project is untouched"
        );
        assert!(rep.fallback.is_none());
        assert_eq!(rep.to_json()["attempts"][1]["within_target"], true);
    }

    #[test]
    fn verify_loop_stops_after_two_re_renders_then_plan_c_fixes_it() {
        let d = tmpdir("loop2");
        let rpp = project(&d);
        let mut fake = FakeReaper::new(-20.0);
        fake.cap_lufs = -17.0; // like a limiter that will not go louder
        let rep = render_project(
            &fake,
            Path::new("/fake/reaper"),
            &rpp,
            None,
            &opts(Some(STREAMING)),
        )
        .unwrap();
        assert_eq!(rep.attempts.len(), 3, "first render + 2 re-renders");
        assert_eq!(fake.calls.borrow().len(), 3);
        let m = rep.fallback.as_ref().expect("plan C ran");
        assert!(m.within_target);
        assert_eq!(rep.within_target, Some(true));
        assert_eq!(rep.final_path.as_ref(), Some(&m.path));
        assert!(rep.notes.iter().any(|n| n.contains("plan C")));
        // Without plan C the best REAPER render is reported as off target.
        let mut o = opts(Some(STREAMING));
        o.builtin_fallback = false;
        let rep = render_project(&fake, Path::new("/fake/reaper"), &rpp, None, &o).unwrap();
        assert_eq!(rep.within_target, Some(false));
        assert!(rep.notes.iter().any(|n| n.contains("best render")));
    }

    #[test]
    fn render_without_target_and_render_failures() {
        let d = tmpdir("loop3");
        let rpp = project(&d);
        let fake = FakeReaper::new(-12.0);
        let rep =
            render_project(&fake, Path::new("/fake/reaper"), &rpp, None, &opts(None)).unwrap();
        assert_eq!(rep.attempts.len(), 1);
        assert_eq!(rep.within_target, None);
        // REAPER that writes nothing: a clear error, no re-render.
        let mut bad = FakeReaper::new(-12.0);
        bad.render_ok = false;
        let mut o = opts(Some(STREAMING));
        o.wait.timeout = Duration::from_millis(50);
        let e = render_project(&bad, Path::new("/fake/reaper"), &rpp, None, &o).unwrap_err();
        assert!(e.contains("wrote no render"), "{e}");
        assert_eq!(bad.calls.borrow().len(), 1);
        // Not a project.
        let junk = d.join("junk.rpp");
        std::fs::write(&junk, "not a project").unwrap();
        assert!(
            render_project(&fake, Path::new("/fake/reaper"), &junk, None, &opts(None))
                .unwrap_err()
                .contains("not a REAPER project")
        );
    }

    #[test]
    fn session_pipeline_with_fake_reaper() {
        let d = tmpdir("sess");
        for n in ["Kick \"In\".wav", "pad.wav"] {
            tone_at(&d.join(n), -20.0);
        }
        std::fs::create_dir_all(d.join("vox")).unwrap();
        tone_at(&d.join("vox\\lead.wav"), -20.0);
        let plan = SessionPlan::from_plan_json(&plan_json(&d), &d, &d.join("session.lua")).unwrap();
        // The session render reads -16: one headless re-render brings it to -14.
        let fake = FakeReaper::new(-16.0);
        let j = session_and_render(
            &fake,
            Path::new("/fake/reaper"),
            Os::Linux,
            &plan,
            &d.join("session.lua"),
            Some(STREAMING),
            fast(),
        )
        .unwrap();
        assert_eq!(j["within_target"], true, "{j:#}");
        assert_eq!(j["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(j["session"]["status"]["items"], 3);
        let calls = fake.calls.borrow();
        assert!(
            !calls[0].background,
            "the session opens the REAPER GUI on purpose"
        );
        assert!(calls[0].args[0].ends_with("session.lua"));
        assert!(calls[1].background);
        let script = std::fs::read_to_string(d.join("session.lua")).unwrap();
        assert!(script.contains("do_render = true"));
    }

    #[test]
    fn session_failures_are_clear() {
        let d = tmpdir("sessbad");
        let plan = SessionPlan::from_plan_json(
            &json!({"stems": [{"path": d.join("a.wav").display().to_string()}]}),
            &d,
            &d.join("s.lua"),
        )
        .unwrap();
        // Script error reported by the status file.
        let mut fake = FakeReaper::new(-14.0);
        fake.status_override = Some(r#"{"done":true,"ok":false,"error":"boom"}"#.into());
        let e = session_and_render(
            &fake,
            Path::new("/r"),
            Os::Linux,
            &plan,
            &d.join("s.lua"),
            None,
            fast(),
        )
        .unwrap_err();
        assert!(e.contains("boom"), "{e}");
        // REAPER exits with an error before any status.
        let mut fake = FakeReaper::new(-14.0);
        fake.exit_code = 3;
        fake.status_override = Some("{\"done\":tr".into());
        let e = run_session(
            &fake,
            Path::new("/r"),
            Os::Linux,
            &plan,
            &d.join("s.lua"),
            fast(),
        )
        .unwrap_err();
        assert!(e.contains("code 3"), "{e}");
        // A launcher that exits 0 but no status ever comes: timeout, with the paths.
        let mut fake = FakeReaper::new(-14.0);
        fake.status_override = Some("{\"done\":false}".into());
        let w = Wait {
            timeout: Duration::from_millis(60),
            poll: Duration::from_millis(5),
        };
        let e = run_session(
            &fake,
            Path::new("/r"),
            Os::Linux,
            &plan,
            &d.join("s.lua"),
            w,
        )
        .unwrap_err();
        assert!(
            e.contains("no status") && e.contains("process 4242 is left running"),
            "{e}"
        );
        // Stems that did not make it onto tracks.
        let mut fake = FakeReaper::new(-14.0);
        let p = plan.project_path.clone();
        fake.status_override = Some(json!({"done": true, "ok": true, "items": 0, "project": p.display().to_string(), "warnings": ["media file not found"]}).to_string());
        let plan2 = SessionPlan::from_plan_json(
            &json!({"stems": [{"path": d.join("a.wav").display().to_string()}]}),
            &d,
            &d.join("t.lua"),
        )
        .unwrap();
        std::fs::write(&p, FAKE_RPP).unwrap();
        let e = session_and_render(
            &fake,
            Path::new("/r"),
            Os::Linux,
            &plan2,
            &d.join("t.lua"),
            None,
            fast(),
        )
        .unwrap_err();
        assert!(
            e.contains("0 of 1") && e.contains("media file not found"),
            "{e}"
        );
    }
}
