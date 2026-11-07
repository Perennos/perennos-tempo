//! tempo-launch: the program behind every Perennos Tempo shortcut.
//!
//! A shortcut names an app. When the app's server is not answering on its port yet, tempo-launch
//! starts it with no window (its output goes to a log file), waits until it answers, then opens it
//! in the browser. Nothing here deletes anything: a log that grows too big is renamed aside, and the
//! record of a stopped app is overwritten, never removed.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod conductor;
pub mod home;
pub mod setup;
pub mod tempo_home;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const EXE: &str = std::env::consts::EXE_SUFFIX;
/// The scheduled task's name: the same one `tempo-vault schedule` uses, so there is only ever one.
pub const TASK_NAME: &str = "Perennos Tempo Vault Learn";
/// Marks the crontab line (the same mark `tempo-vault schedule` uses).
const CRON_MARK: &str = "# perennos-tempo-vault-learn";
/// The launchd agent's name on macOS (the file is `<label>.plist` in ~/Library/LaunchAgents).
pub const LAUNCHD_LABEL: &str = "com.perennos.tempo.vault-learn";
/// A log bigger than this is renamed aside before the next start.
const LOG_ROLL: u64 = 8 << 20;
/// Ports for vaults other than the default one ("Open as Tempo Vault" on any folder).
const VAULT_PORTS: std::ops::RangeInclusive<u16> = 8780..=8799;

pub const USAGE: &str = "Perennos Tempo launcher: the program behind the Perennos Tempo shortcuts.

usage: tempo-launch [command]

  (nothing)         Perennos Tempo: starts the agent, then opens the Field (or the Vault)
  field             the Tempo Field
  vault [FOLDER]    Tempo Vault on FOLDER (default: TEMPO_VAULT, else Documents\\Tempo Vault)
  room              Tempo Room, signed in as you
  agent             start the agent and open its chat page (with its key, from the agent's data folder)
  studio            Tempo Studio (mixing and mastering) has no page of its own: starts the agent and
                    opens its chat page; ask Tempo to analyse or master a file in your Music folder
  start             start the agent and every installed app without opening anything (sign-in); the very
                    first time it also writes and opens the setup page (the plan for this computer)
  stop              stop the apps tempo-launch started
  status [--json]   which apps are installed and which are running (and the remote ones)
  open ID           open an app on another machine listed in remote.json (starts nothing)
  learn             one learning pass over the vault (what the hourly task runs)
  schedule [--daily | --remove | --print]
                    the learning task (hourly unless --daily)
  logs              open the folder with the logs
  home [--json]     the Tempo folder in use and why, and any other Tempo folder that holds files
  home --merge DIR  move the files of another Tempo folder into the home; never overwrites,
                    never deletes (what is merged away goes to <home>\\trash\\<date>)
  setup [--apply [--yes]] [--with-d1] [--goal chat,code,notes,music,rooms,private]
        [--prefer preferred|mainstream|quality] [--preferred-prefix P] [--json]
                    look at this computer and your goals, show which models fit, and with
                    --apply (and your yes) fetch what is missing and set up the agent
  setup --apply-proposal
                    show what the proposal your AI wrote (see mcp) would change in the agent's
                    settings, and apply it only after your y/N
  setup --page      only write the plan to <home>\\setup.html (opened once at the first start)
  setup --ollama [--yes]
                    only fetch Tempo's own Ollama (checked against a pinned SHA-256) when this
                    computer has none, and start it; no models are fetched
  welcome           the first-run page: what the scan found, one computer or several, and how to
                    let your AI help (opened by itself the first time, then `tempo-launch welcome`)
  link URL          add another computer's Ollama (a tailnet or home-network address) to the
                    agent's ollama_fallbacks; nothing is contacted
  mcp [--scan]      a stdio MCP server for your AI (Claude Desktop, Claude Code, ...): read the scan,
                    the plan and the status, and write a proposal you apply yourself;
                    --scan prints the scan facts to paste into any AI
  conductor         Tempo Conductor: sets up bands (teams of AI members) for your own workflow,
                    runs them, and picks which of your engines plays each part (`band` works too)
  conductor templates
                    the ready-made bands (social-media, music-release, small-business, dev-team,
                    research, content-creator, study)
  conductor new (--template NAME | --describe \"TEXT\") [--name BAND] [--yes]
                    set up a band: shows who is in it, what they do, which engine plays each
                    part and when they run, asks y/N, then adds them to Room (if Room runs)
                    and schedules their flows
  conductor run BAND [FLOW] [--note TEXT]
                    walk a flow through its members; each step becomes a linked note in the Vault
  conductor engines every engine you have (this computer's Ollama and the ones you linked):
                    up or down, their models, and how strong each is
  conductor room BAND
                    add the band's members to Room as teammates (when Room was not running)
  conductor page [BAND]
                    write and open the Conductor page: engines, bands, last runs, apps
  conductor apply-proposal [NAME]
                    show the band your AI proposed (see mcp) and set it up only after your y/N
  version

A perennos-tempo: web link (the Store app registers it) is accepted in place of a command:
perennos-tempo:open, :agent, :vault, :room or :field. Nothing else, and no arguments.

remote.json (optional, in the launcher folder) lists apps on other machines of your
tailnet; `open ID` and `status` use it. Nothing is ever started on another machine.

Every app starts hidden, with no window; its output goes to logs\\<app>.log in
the Tempo home (`tempo-launch home` shows it; docs/TEMPO-HOME.md has the rule). TEMPO_NO_BROWSER=1 prints the
address instead of opening the browser.";

/// One Perennos Tempo app tempo-launch can start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct App {
    pub id: &'static str,
    pub title: &'static str,
    /// The program's name, without `.exe`.
    pub bin: &'static str,
    pub port: u16,
    pub health: &'static str,
    /// Its health answer holds one of these (whitespace removed), so another program on the port
    /// is never mistaken for it.
    pub signs: &'static [&'static str],
    /// How long it may take to answer after it starts.
    pub wait: Duration,
}

pub const AGENT: App = App {
    id: "agent",
    title: "Tempo Agent",
    bin: "tempo-local-agent",
    port: 8765,
    health: "/agent/health",
    signs: &["\"guest\":true", "\"stream\":true"],
    wait: Duration::from_secs(60),
};
pub const VAULT: App = App {
    id: "vault",
    title: "Tempo Vault",
    bin: "tempo-vault",
    port: 8766,
    health: "/vault/health",
    signs: &["\"app\":\"tempo-vault\""],
    wait: Duration::from_secs(30),
};
pub const ROOM: App = App {
    id: "room",
    title: "Tempo Room",
    bin: "tempo-room",
    port: 8767,
    health: "/room/health",
    signs: &["\"app\":\"tempo-room\""],
    wait: Duration::from_secs(30),
};
pub const FIELD: App = App {
    id: "field",
    title: "Tempo Field",
    bin: "tempo-dashboard",
    port: 7878,
    health: "/healthz",
    signs: &["\"read_only\":true"],
    wait: Duration::from_secs(30),
};
/// The apps that have a server and a port: what `start`, `stop` and `status` go through. Tempo
/// Studio is not here: it has no window or server of its own (the agent uses it as a tool), so there
/// is nothing to start, wait for or stop. `tempo-launch studio` opens the agent instead.
pub const APPS: [App; 4] = [AGENT, VAULT, ROOM, FIELD];
/// What `tempo-launch studio` says: how Tempo Studio is used, since it has nothing to open.
pub const STUDIO_LINE: &str =
    "Tempo Studio works through Tempo: ask Tempo to analyse or master a file in your Music folder.";
/// Tempo's own copy of Ollama (`setup --apply` fetches it when a computer has none). It is not in
/// `APPS`: its program lives in `<home>/runtime`, named by `<home>/runtime/ollama.json`, and it is
/// started with `OLLAMA_HOST` instead of arguments. Whatever Ollama answers on the port is used
/// as it is: Tempo starts its own only when none answers, and stops only what it started.
pub const OLLAMA: App = App {
    id: "ollama",
    title: "Ollama (Tempo's own copy)",
    bin: "ollama",
    port: 11434,
    health: "/api/version",
    signs: &["\"version\":"],
    wait: Duration::from_secs(30),
};

// ---- where things are

/// Everything tempo-launch needs to know about this machine. `from_env` in the program; tests
/// build their own.
pub struct Ctx {
    /// Logs, run records and the problem page: the one Tempo home (`tempo_home.rs`), or TEMPO_LAUNCH_HOME.
    pub home: PathBuf,
    /// The agent's conversations and memory (TEMPO_AGENT_DATA, else <home>/agent).
    pub agent_data: PathBuf,
    /// The default vault (TEMPO_VAULT, else Documents/Tempo Vault).
    pub vault: PathBuf,
    /// Where the apps are looked for, in order: next to tempo-launch, then the folders of TEMPO_BIN.
    pub bin_dirs: Vec<PathBuf>,
    /// false: print addresses instead of opening them.
    pub browser: bool,
    /// Every address opened (or printed), in order.
    pub opened: RefCell<Vec<String>>,
}

impl Ctx {
    pub fn from_env() -> Ctx {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let home = var("TEMPO_LAUNCH_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(default_home);
        let agent_data = var("TEMPO_AGENT_DATA")
            .map(|v| expand_home(Path::new(&v)))
            .unwrap_or_else(|| home.join("agent"));
        let vault = var("TEMPO_VAULT")
            .map(|v| expand_home(Path::new(&v)))
            .unwrap_or_else(default_vault);
        let mut bin_dirs = Vec::new();
        if let Some(d) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
        {
            bin_dirs.push(d);
        }
        // PATH is never searched: a program planted in a folder on it (or in the current folder,
        // which an empty or relative PATH entry means) would run as Tempo.
        if let Some(v) = var("TEMPO_BIN") {
            bin_dirs.extend(std::env::split_paths(&v).filter(|d| d.is_absolute()));
        }
        let browser = var("TEMPO_NO_BROWSER").is_none_or(|v| v == "0");
        Ctx {
            home,
            agent_data,
            vault,
            bin_dirs,
            browser,
            opened: RefCell::new(Vec::new()),
        }
    }

    /// The program for an app, if it is installed.
    pub fn find(&self, bin: &str) -> Option<PathBuf> {
        let name = format!("{bin}{EXE}");
        self.bin_dirs
            .iter()
            .map(|d| d.join(&name))
            .find(|p| p.is_file())
    }

    /// `<agent data>/config.json`, if it exists.
    pub fn agent_config_file(&self) -> Option<PathBuf> {
        Some(self.agent_data.join("config.json")).filter(|p| p.is_file())
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.logs_dir().join(format!("{name}.log"))
    }

    fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }

    /// One line in logs/launch.log, so what happened at sign-in can be read later.
    fn note(&self, line: &str) {
        if let Ok(mut f) = open_log(&self.log_path("launch")) {
            let _ = writeln!(f, "{} {line}", stamp(unix_now()));
        }
    }

    /// Open an address in the browser (or print it).
    pub fn open(&self, target: &str) -> Result<(), String> {
        // only what tempo-launch itself builds: an address on this computer or a file in its own
        // folder; the system opener would run anything else it is handed
        let own_file = Path::new(target).starts_with(&self.home)
            && !Path::new(target)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        // or exactly the address of an entry in remote.json that passed its checks
        let remote = !own_file
            && !loopback_url(target)
            && load_remote(&self.home).0.iter().any(|r| r.url == target);
        if !(loopback_url(target) || own_file || remote) {
            return Err("refused: tempo-launch only opens its own pages".into());
        }
        self.opened.borrow_mut().push(target.to_string());
        // an address may carry the agent's key in its fragment: it is never printed or put in an error
        let shown = hide_key(target);
        if !self.browser {
            println!("{shown}");
            return Ok(());
        }
        open_with_system(target).map_err(|e| e.replace(target, &shown))
    }

    // ---- run records: which program tempo-launch started, so `stop` ends only those

    fn record_path(&self, app: &str, port: u16) -> PathBuf {
        self.run_dir().join(format!("{app}-{port}.json"))
    }

    fn save_record(&self, r: &Record) {
        let _ = write_atomic(&self.record_path(&r.app, r.port), r.to_json().as_bytes());
    }

    pub fn records(&self) -> Vec<Record> {
        let mut out: Vec<Record> = fs::read_dir(self.run_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|t| Record::parse(&t))
            .collect();
        out.sort_by_key(|r| (r.app.clone(), r.port));
        out
    }
}

/// Write a file whole or not at all: into a sibling file first, then renamed over the real one. A
/// link at the real name is replaced, never written through.
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    fs::write(&tmp, data).and_then(|()| fs::rename(&tmp, path))
}

fn home_dir() -> Option<PathBuf> {
    let order: [&str; 2] = if cfg!(windows) {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    order
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The one Tempo home per machine (`tempo_home.rs`).
fn default_home() -> PathBuf {
    tempo_home::tempo_home().dir
}

fn documents_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(d) = win::documents() {
        return Some(d);
    }
    let home = home_dir()?;
    let docs = home.join("Documents");
    Some(if docs.is_dir() { docs } else { home })
}

fn default_vault() -> PathBuf {
    documents_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Tempo Vault")
}

/// `~`, `~/x` or `~\x` under the home folder; anything else unchanged.
pub fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let rest = if s == "~" {
        Some("")
    } else {
        s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\"))
    };
    match (rest, home_dir()) {
        (Some(""), Some(h)) => h,
        (Some(r), Some(h)) => h.join(r),
        _ => p.to_path_buf(),
    }
}

/// A folder from Explorer: `"%V"` on a drive root arrives as `C:"` (the backslash escaped the quote).
pub fn clean_folder_arg(a: &str) -> PathBuf {
    let t = a.trim().trim_end_matches('"');
    if t.len() == 2 && t.ends_with(':') {
        PathBuf::from(format!("{t}\\"))
    } else {
        PathBuf::from(t)
    }
}

fn same_place(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = c
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    norm(a) == norm(b)
}

// ---- time, for log lines and file names (UTC; no time zone database needed)

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// `2026-11-06 13:05:09Z`
pub fn stamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

fn file_stamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

// ---- logs

/// Open a log for appending. A log over the limit is renamed aside first, never deleted.
pub fn open_log(path: &Path) -> io::Result<File> {
    roll_if_big(path, LOG_ROLL);
    OpenOptions::new().create(true).append(true).open(path)
}

fn roll_if_big(path: &Path, limit: u64) {
    if let Some(d) = path.parent() {
        let _ = fs::create_dir_all(d);
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > limit) {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("log");
        let mut n = 0;
        loop {
            let tag = if n == 0 {
                String::new()
            } else {
                format!("-{n}")
            };
            let aside = path.with_file_name(format!("{stem}-{}{tag}.log", file_stamp(unix_now())));
            if !aside.exists() {
                // a log that is still open elsewhere stays where it is
                let _ = fs::rename(path, &aside);
                break;
            }
            n += 1;
        }
    }
}

/// The last `n` lines of a log, with anything that looks like a key hidden.
pub fn log_tail(path: &Path, n: usize) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(64 << 10);
    let mut buf = Vec::new();
    if io::Seek::seek(&mut f, io::SeekFrom::Start(from)).is_err()
        || f.read_to_end(&mut buf).is_err()
    {
        return String::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(n)..].join("\n");
    redact(&tail)
}

/// Hide the value after a name that says it is secret, in any spelling: `token=x`, `"token":"x"`,
/// `X-Api-Key: x`, `Authorization: Basic x`, `Cookie: a=b; c=d` (the rest of the line).
fn hide_named(line: &str) -> String {
    const STEMS: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "api_key",
        "api-key",
        "authorization",
        "cookie",
        "owner",
        "invite",
        "key",
    ];
    let b = line.as_bytes();
    let lower = line.to_ascii_lowercase();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';
    let is_quote = |c: u8| c == b'"' || c == b'\'';
    let stop = |c: u8| matches!(c, b' ' | b',' | b';' | b'"' | b'\'' | b'}' | b'&');
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < b.len() {
        if !is_id(b[i]) {
            let ch = line[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let start = i;
        while i < b.len() && is_id(b[i]) {
            i += 1;
        }
        out.push_str(&line[start..i]);
        let name = &lower[start..i];
        if !STEMS.iter().any(|k| name.ends_with(k)) {
            continue;
        }
        // an optional closing quote and spaces, then : or =
        let mut j = i;
        if j < b.len() && is_quote(b[j]) {
            j += 1;
        }
        while j < b.len() && b[j] == b' ' {
            j += 1;
        }
        if j >= b.len() || !matches!(b[j], b':' | b'=') {
            continue;
        }
        j += 1;
        while j < b.len() && b[j] == b' ' {
            j += 1;
        }
        let quote = (j < b.len() && is_quote(b[j])).then(|| b[j]);
        if quote.is_some() {
            j += 1;
        }
        let find_end = |from: usize| {
            match quote {
                Some(q) => b[from..].iter().position(|c| *c == q),
                None => b[from..].iter().position(|c| stop(*c)),
            }
            .map_or(b.len(), |e| from + e)
        };
        let mut v = j;
        let mut end = if name.ends_with("cookie") {
            b.len()
        } else {
            find_end(v)
        };
        // `Authorization: Bearer x`: the scheme word stays, the key after it goes
        if name.ends_with("authorization")
            && quote.is_none()
            && matches!(&lower[v..end], "bearer" | "basic" | "token" | "digest")
            && b.get(end) == Some(&b' ')
        {
            v = end + 1;
            end = find_end(v);
        }
        if end > v {
            out.push_str(&line[i..v]);
            out.push_str("[hidden]");
            i = end;
        }
    }
    out
}

/// Hide what looks like a key or a sign-in token: `sk-…`, `ghp_…`, `xox…-…`, `Bearer …`,
/// `owner=…`, `token=…`, and any unbroken run of 32 or more key characters.
pub fn redact(text: &str) -> String {
    const PREFIXES: &[&str] = &[
        "sk-",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xapp-",
        "glpat-",
        "AKIA",
    ];
    const NAMED: &[&str] = &[
        "owner=",
        "token=",
        "key=",
        "secret=",
        "password=",
        "invite=",
    ];
    let keyish = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    // base64 and its url-safe form also carry keys
    let tokenish = |c: char| keyish(c) || c == '+' || c == '=' || c == '/';
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let hidden_line = hide_named(line);
        let line = hidden_line.as_str();
        let mut hide_next = false;
        let mut words = Vec::new();
        for w in line.split(' ') {
            let mut w = w.to_string();
            let lower = w.to_ascii_lowercase();
            if hide_next && !w.is_empty() {
                // the scheme word stays; what follows it is the secret
                if !matches!(lower.as_str(), "bearer" | "basic" | "token") {
                    w = "[hidden]".into();
                    hide_next = false;
                }
            } else if matches!(lower.as_str(), "bearer" | "authorization:") {
                hide_next = true;
            }
            for n in NAMED {
                if let Some(at) = w.to_ascii_lowercase().find(n) {
                    let start = at + n.len();
                    let end = w[start..]
                        .find(|c: char| !keyish(c) && c != '.' && c != '~')
                        .map_or(w.len(), |e| start + e);
                    if end > start {
                        w.replace_range(start..end, "[hidden]");
                    }
                }
            }
            let core = w.trim_matches(|c: char| !tokenish(c));
            // a `/` only counts in a run that does not look like a path (no dots, no leading slash)
            let shape = core
                .chars()
                .all(|c| tokenish(c) && (c != '/' || !core.contains('.')))
                && !core.starts_with('/');
            let long = core.len() >= 32 && shape && core.chars().any(|c| c.is_ascii_digit());
            if !core.is_empty()
                && (long
                    || PREFIXES
                        .iter()
                        .any(|p| core.starts_with(p) && core.len() >= p.len() + 12))
            {
                w = w.replace(core, "[hidden]");
            }
            words.push(w);
        }
        out.push_str(&words.join(" "));
    }
    out
}

// ---- tiny HTTP and JSON (std only)

/// `GET http://127.0.0.1:PORT/PATH`: the status code and body, or None when nothing answers in time.
pub fn http_get(port: u16, path: &str, timeout: Duration) -> Option<(u16, String)> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    http_get_at(addr, &format!("127.0.0.1:{port}"), path, timeout)
}

/// `GET http://ADDR/PATH` with `host` as the Host header. No cookies, no credentials, and a
/// redirect is just another status code: it is never followed.
fn http_get_at(
    addr: SocketAddr,
    host: &str,
    path: &str,
    timeout: Duration,
) -> Option<(u16, String)> {
    let mut s = TcpStream::connect_timeout(&addr, timeout).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.set_write_timeout(Some(timeout)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nUser-Agent: tempo-launch/{VERSION}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).ok()?;
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(r) = parse_response(&buf, false) {
            return Some(r);
        }
        if Instant::now() > deadline || buf.len() > (1 << 20) {
            break;
        }
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    parse_response(&buf, true)
}

/// A complete HTTP response in `buf` (or, `at_eof`, whatever arrived): status code and body.
pub fn parse_response(buf: &[u8], at_eof: bool) -> Option<(u16, String)> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?;
    if !status.starts_with("HTTP/") {
        return None;
    }
    let code: u16 = status.split_whitespace().nth(1)?.parse().ok()?;
    let (mut len, mut chunked) = (None, false);
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "content-length" {
                len = v.trim().parse::<usize>().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let body = &buf[end + 4..];
    let body: Vec<u8> = if let Some(n) = len {
        if body.len() >= n {
            body[..n].to_vec()
        } else if at_eof {
            body.to_vec()
        } else {
            return None;
        }
    } else if chunked {
        match dechunk(body) {
            Some(b) => b,
            None if at_eof => body.to_vec(),
            None => return None,
        }
    } else if at_eof {
        body.to_vec()
    } else {
        return None;
    };
    Some((code, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = b.windows(2).position(|w| w == b"\r\n")?;
        let size_txt = std::str::from_utf8(&b[..nl]).ok()?;
        let size = usize::from_str_radix(size_txt.split(';').next()?.trim(), 16).ok()?;
        b = &b[nl + 2..];
        if size == 0 {
            return Some(out);
        }
        if b.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

/// The text after `"key":` (first match, at any depth), whitespace skipped.
fn json_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let mut from = 0;
    while let Some(i) = text[from..].find(&pat) {
        let after = from + i + pat.len();
        if let Some(rest) = text[after..].trim_start().strip_prefix(':') {
            return Some(rest.trim_start());
        }
        from = after;
    }
    None
}

/// The string value of `"key"` in a JSON text. Enough for health answers and our own records.
pub fn json_str(text: &str, key: &str) -> Option<String> {
    let mut it = json_value(text, key)?.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = it.next() {
        match c {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = it.by_ref().take(4).collect();
                    let n = u32::from_str_radix(&hex, 16).ok()?;
                    if (0xD800..0xDC00).contains(&n) {
                        let mut look = it.clone();
                        let low = (look.next() == Some('\\') && look.next() == Some('u'))
                            .then(|| look.by_ref().take(4).collect::<String>())
                            .and_then(|h| u32::from_str_radix(&h, 16).ok())
                            .filter(|l| (0xDC00..0xE000).contains(l));
                        match low.and_then(|l| {
                            char::from_u32(0x10000 + ((n - 0xD800) << 10) + (l - 0xDC00))
                        }) {
                            Some(ch) => {
                                out.push(ch);
                                it = look;
                            }
                            None => out.push('\u{FFFD}'),
                        }
                    } else {
                        out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// The whole-number value of `"key"`.
pub fn json_num(text: &str, key: &str) -> Option<u64> {
    let v = json_value(text, key)?;
    let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

pub fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---- health

/// Whether a health answer is the app's own.
pub fn signed(body: &str, signs: &[&str]) -> bool {
    let flat: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    signs.iter().any(|s| flat.contains(s))
}

/// The app's health answer on `port`, if the app is what answers there.
pub fn answers(app: &App, port: u16) -> Option<String> {
    // the agent checks its model hosts before it answers
    let t = Duration::from_secs(if app.id == "agent" { 15 } else { 3 });
    match http_get(port, app.health, t) {
        Some((200, body)) if signed(&body, app.signs) => Some(body),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub enum Probe {
    /// The app answers (its health body).
    Ours(String),
    /// Something else holds the port.
    Other,
    Free,
}

/// What is on `port`. Only before starting anything: checking for a free port binds it briefly.
pub fn probe(app: &App, port: u16) -> Probe {
    if let Some(b) = answers(app, port) {
        return Probe::Ours(b);
    }
    if port_free(port) {
        Probe::Free
    } else {
        Probe::Other
    }
}

pub fn port_free(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

// ---- starting apps

/// How to start one app.
#[derive(Clone, Debug)]
pub struct Launch {
    pub app: App,
    pub port: u16,
    pub args: Vec<String>,
    /// The folder a vault serves.
    pub dir: Option<PathBuf>,
    pub cwd: PathBuf,
    pub env: Vec<(String, PathBuf)>,
}

impl Launch {
    /// The log's name: the app, plus the port for a vault on another folder.
    pub fn log_name(&self) -> String {
        if self.port == self.app.port {
            self.app.id.to_string()
        } else {
            format!("{}-{}", self.app.id, self.port)
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Whether a health answer is this launch's app: for a vault, the one on this folder.
    pub fn accepts(&self, body: &str) -> bool {
        match (&self.dir, self.app.id) {
            (Some(d), "vault") => {
                d.file_name().map(|n| n.to_string_lossy().into_owned()) == json_str(body, "name")
            }
            _ => true,
        }
    }

    fn answered(&self) -> bool {
        answers(&self.app, self.port).is_some_and(|b| self.accepts(&b))
    }
}

/// How the agent is told which settings file to use. `env_set`: TEMPO_AGENT_CONFIG is already set,
/// and it always wins. Otherwise an `agent-config.json` is passed as that variable (as before), and
/// failing that the `config.json` written by `tempo-launch setup` is passed as `--config`, until
/// the agent reads `<agent data>/config.json` by itself.
fn agent_config_args(ctx: &Ctx, env_set: bool) -> (Vec<String>, Option<(String, PathBuf)>) {
    let mut args = vec!["serve".to_string()];
    if env_set {
        return (args, None);
    }
    let old = ctx.agent_data.join("agent-config.json");
    if old.is_file() {
        return (args, Some(("TEMPO_AGENT_CONFIG".into(), old)));
    }
    if let Some(c) = ctx.agent_config_file() {
        args.push("--config".into());
        args.push(c.to_string_lossy().into_owned());
    }
    (args, None)
}

/// The usual way to start `app`. A vault needs its folder and port (see `vault_launch`).
pub fn launch_for(ctx: &Ctx, app: App) -> Launch {
    // every app is told the home, so they all agree with this program even when it was chosen by
    // TEMPO_LAUNCH_HOME
    let mut env = vec![
        ("TEMPO_HOME".to_string(), ctx.home.clone()),
        ("TEMPO_AGENT_DATA".to_string(), ctx.agent_data.clone()),
    ];
    // the agent connects the vault as a tool when it knows where it is
    if ctx.vault.is_dir() {
        env.push(("TEMPO_VAULT".into(), ctx.vault.clone()));
    }
    let port = app.port.to_string();
    let (args, cwd): (Vec<String>, PathBuf) = match app.id {
        "agent" => {
            let (mut args, cfg_env) =
                agent_config_args(ctx, std::env::var_os("TEMPO_AGENT_CONFIG").is_some());
            env.extend(cfg_env);
            args.extend(["--port".into(), port]);
            (args, ctx.agent_data.clone())
        }
        "vault" => (
            vec![
                "serve".into(),
                "--vault".into(),
                ctx.vault.to_string_lossy().into_owned(),
                "--port".into(),
                port,
            ],
            ctx.home.clone(),
        ),
        "room" => (
            vec!["serve".into(), "--port".into(), port],
            ctx.home.clone(),
        ),
        _ => (vec![], ctx.home.clone()),
    };
    Launch {
        app,
        port: app.port,
        args,
        dir: (app.id == "vault").then(|| ctx.vault.clone()),
        cwd,
        env,
    }
}

/// The vault for `folder` (default: the default vault): the port it is on, or a free one.
pub fn vault_launch(ctx: &Ctx, folder: Option<&Path>) -> Launch {
    let dir = folder.map_or_else(|| ctx.vault.clone(), Path::to_path_buf);
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    let mut l = launch_for(ctx, VAULT);
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
    let named = |body: &str| json_str(body, "name") == name;
    let port = ctx
        .records()
        .into_iter()
        // already open?
        .filter(|r| r.app == "vault" && r.pid != 0 && same_place(Path::new(&r.dir), &dir))
        .find(|r| answers(&VAULT, r.port).is_some_and(|b| named(&b)))
        .map(|r| r.port)
        .or_else(|| {
            answers(&VAULT, VAULT.port)
                .filter(|b| named(b))
                .map(|_| VAULT.port)
        })
        // the default vault gets the usual port while it is free; any other folder the next free one
        .or_else(|| (same_place(&dir, &ctx.vault) && port_free(VAULT.port)).then_some(VAULT.port))
        .unwrap_or_else(|| pick_port(VAULT_PORTS).unwrap_or(*VAULT_PORTS.end()));
    l.port = port;
    l.args = vec![
        "serve".into(),
        "--vault".into(),
        dir.to_string_lossy().into_owned(),
        "--port".into(),
        port.to_string(),
    ];
    l.dir = Some(dir);
    l
}

/// The first free port in the range.
pub fn pick_port(range: std::ops::RangeInclusive<u16>) -> Option<u16> {
    range.into_iter().find(|p| port_free(*p))
}

#[derive(Debug, PartialEq)]
pub enum Up {
    Already,
    Started(u32),
}

/// Make sure the app answers: it already does, or it is started hidden. With `wait`, wait until it
/// answers.
pub fn ensure(ctx: &Ctx, l: &Launch, wait: bool) -> Result<Up, Problem> {
    ensure_exe(ctx, l, ctx.find(l.app.bin), wait)
}

/// `ensure`, with the program already chosen (None: it is not installed).
fn ensure_exe(ctx: &Ctx, l: &Launch, exe: Option<PathBuf>, wait: bool) -> Result<Up, Problem> {
    match probe(&l.app, l.port) {
        Probe::Ours(b) if l.accepts(&b) => return Ok(Up::Already),
        Probe::Ours(_) | Probe::Other => return Err(Problem::busy(&l.app, l.port)),
        Probe::Free => {}
    }
    let exe = exe.ok_or_else(|| Problem::missing(&l.app))?;
    let mut child = spawn(ctx, l, &exe)?;
    let pid = child.id();
    ctx.save_record(&Record {
        app: l.app.id.into(),
        pid,
        port: l.port,
        exe: exe.to_string_lossy().into_owned(),
        img: String::new(),
        dir: l
            .dir
            .as_ref()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
        at: unix_now(),
    });
    ctx.note(&format!(
        "{}: started pid {pid} on port {}",
        l.log_name(),
        l.port
    ));
    if wait {
        wait_ready(ctx, l, &mut child)?;
        remember_image(ctx, l.app.id, l.port);
    }
    Ok(Up::Started(pid))
}

/// How to start Tempo's own Ollama on `port`, and its program: `ollama serve` with
/// `OLLAMA_HOST=127.0.0.1:<port>` (this computer only). OLLAMA_MODELS is left alone, so models go
/// where a normal Ollama keeps them. None when `setup` has not installed one.
pub fn owned_ollama_launch(ctx: &Ctx, port: u16) -> Option<(Launch, PathBuf)> {
    let owned = setup::runtime::read_record(&ctx.home)?;
    let l = Launch {
        app: OLLAMA,
        port,
        args: vec!["serve".into()],
        dir: None,
        cwd: ctx.home.join("runtime"),
        env: vec![(
            "OLLAMA_HOST".into(),
            PathBuf::from(format!("127.0.0.1:{port}")),
        )],
    };
    Some((l, owned.binary))
}

/// Make sure an Ollama answers on `port`, starting Tempo's own copy if there is one and none
/// answers. Any Ollama that already answers is left exactly as it is, and a port held by something
/// else is an error. `Ok(None)`: Tempo has no copy of its own.
pub fn ensure_owned_ollama(ctx: &Ctx, port: u16, wait: bool) -> Result<Option<Up>, Problem> {
    let Some((l, exe)) = owned_ollama_launch(ctx, port) else {
        return Ok(None);
    };
    ensure_exe(ctx, &l, Some(exe), wait).map(Some)
}

/// Whether an `OLLAMA_URL` / `TEMPO_OLLAMA` names this computer. Tempo's own Ollama only ever
/// serves this computer, so it is not started for a setting that points somewhere else.
pub fn ollama_url_is_local(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let auth = rest.split('/').next().unwrap_or("");
    if auth.contains('@') {
        return false;
    }
    let host = match auth.strip_prefix('[') {
        Some(r) => r.split(']').next().unwrap_or(""),
        None => auth.split(':').next().unwrap_or(""),
    };
    // a bare port (`11434`) means this computer; a longer number is a whole address written as
    // one number (`2130706433`), which is not worth guessing about
    host.is_empty()
        || (host.len() <= 5 && host.bytes().all(|b| b.is_ascii_digit()))
        || matches!(
            host.to_ascii_lowercase().as_str(),
            "127.0.0.1" | "localhost" | "::1"
        )
}

fn ollama_setting_is_local() -> bool {
    ["OLLAMA_URL", "TEMPO_OLLAMA"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .all(|u| ollama_url_is_local(&u))
}

/// What `start` says when there is no Ollama at all: none answers, none is installed, and Tempo
/// has none of its own.
pub const OLLAMA_MISSING: &str = "Ollama is not installed yet, so Tempo can't answer. Run: tempo-launch setup --apply (or install it from https://ollama.com/download).";

fn ollama_missing_line(o: &setup::probe::Ollama, has_owned: bool) -> Option<&'static str> {
    (!o.running && o.binary.is_none() && !has_owned).then_some(OLLAMA_MISSING)
}

/// Start Tempo's own Ollama when it is installed and none answers; a problem is a log line.
fn start_owned_ollama(ctx: &Ctx, wait: bool) {
    start_owned_ollama_at(ctx, setup::apply::ollama_port(), wait);
}

fn start_owned_ollama_at(ctx: &Ctx, port: u16, wait: bool) {
    if !ollama_setting_is_local() {
        return;
    }
    if let Err(p) = ensure_owned_ollama(ctx, port, wait) {
        ctx.note(&format!("{}: {}", p.title, p.what));
    }
}

/// Once the app answers, write down the program the system says the process runs, so `stop` can
/// tell it from an unrelated program that got the same process id later.
fn remember_image(ctx: &Ctx, app: &str, port: u16) {
    let p = ctx.record_path(app, port);
    if let Some(mut r) = fs::read_to_string(&p).ok().and_then(|t| Record::parse(&t)) {
        if let Some(img) = (r.pid != 0).then(|| image_of(r.pid)).flatten() {
            r.img = img.to_string_lossy().into_owned();
            ctx.save_record(&r);
        }
    }
}

fn spawn(ctx: &Ctx, l: &Launch, exe: &Path) -> Result<Child, Problem> {
    let log_path = ctx.log_path(&l.log_name());
    let io_problem = |e: io::Error| Problem::cannot_start(&l.app, &e.to_string());
    let mut log = open_log(&log_path).map_err(io_problem)?;
    let _ = writeln!(
        log,
        "\n--- {} tempo-launch {VERSION}: {} {}",
        stamp(unix_now()),
        exe.display(),
        l.args.join(" ")
    );
    let err = log.try_clone().map_err(io_problem)?;
    let _ = fs::create_dir_all(&l.cwd);
    let mut cmd = Command::new(exe);
    cmd.args(&l.args)
        .current_dir(&l.cwd)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err);
    for (k, v) in &l.env {
        cmd.env(k, v);
    }
    spawn_hidden(&mut cmd).map_err(io_problem)
}

/// Start a program with no window that keeps running after tempo-launch exits. On Windows it gets
/// a console nobody sees, and the programs it starts (the agent's tools) share it, so none of them
/// opens a window either.
pub fn spawn_hidden(cmd: &mut Command) -> io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // out of the caller's job when allowed, so closing a terminal does not end the app
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(c) = cmd.spawn() {
            return Ok(c);
        }
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// Run a program hidden and wait for it.
fn run_hidden(cmd: &mut Command) -> io::Result<std::process::Output> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.stdin(Stdio::null()).output()
}

fn wait_ready(ctx: &Ctx, l: &Launch, child: &mut Child) -> Result<(), Problem> {
    let start = Instant::now();
    let log = ctx.log_path(&l.log_name());
    loop {
        // only ask; never bind the port while the app is trying to
        if l.answered() {
            return Ok(());
        }
        if let Ok(Some(st)) = child.try_wait() {
            // a second launch at the same moment: the first one's app answers now
            if l.answered() {
                return Ok(());
            }
            return Err(Problem::exited(&l.app, st, log));
        }
        if start.elapsed() > l.app.wait {
            return Err(Problem::slow(&l.app, log));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

// ---- run records

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub app: String,
    /// 0 once stopped.
    pub pid: u32,
    pub port: u16,
    /// The program tempo-launch started.
    pub exe: String,
    /// The program the system said the process ran once it answered (empty until then).
    pub img: String,
    pub dir: String,
    pub at: u64,
}

impl Record {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"app\":\"{}\",\"pid\":{},\"port\":{},\"exe\":\"{}\",\"img\":\"{}\",\"dir\":\"{}\",\"at\":{}}}\n",
            json_escape(&self.app),
            self.pid,
            self.port,
            json_escape(&self.exe),
            json_escape(&self.img),
            json_escape(&self.dir),
            self.at
        )
    }

    pub fn parse(t: &str) -> Option<Record> {
        Some(Record {
            app: json_str(t, "app")?,
            pid: json_num(t, "pid")? as u32,
            port: json_num(t, "port")? as u16,
            exe: json_str(t, "exe").unwrap_or_default(),
            img: json_str(t, "img").unwrap_or_default(),
            dir: json_str(t, "dir").unwrap_or_default(),
            at: json_num(t, "at").unwrap_or(0),
        })
    }
}

/// Whether `pid` still runs the program the record names (process ids are reused).
fn is_ours(r: &Record) -> bool {
    // process 0 is "none"; 1 is the system's first process; above i32::MAX a number turns negative
    // in `kill`, where -1 means every process
    if r.pid <= 1 || r.pid > i32::MAX as u32 {
        return false;
    }
    let Some(img) = image_of(r.pid) else {
        return false;
    };
    let norm = |s: &str| {
        let s = s.trim_end_matches(" (deleted)");
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    if !r.img.is_empty() {
        return norm(&img.to_string_lossy()) == norm(&r.img);
    }
    let name = |p: &Path| {
        p.file_name()
            .map(|n| norm(&n.to_string_lossy()))
            .unwrap_or_default()
    };
    let n = name(&img);
    !n.is_empty() && n == name(Path::new(&r.exe))
}

#[cfg(unix)]
fn image_of(pid: u32) -> Option<PathBuf> {
    if let Ok(p) = fs::read_link(format!("/proc/{pid}/exe")) {
        return Some(p);
    }
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

#[cfg(windows)]
fn image_of(pid: u32) -> Option<PathBuf> {
    win::image_of(pid)
}

#[cfg(not(any(unix, windows)))]
fn image_of(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn terminate(pid: u32) -> bool {
    let kill = |target: String| {
        Command::new("kill")
            .args(["-TERM", "--", &target])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // its process group first (the agent's tool programs), then the program itself
    kill(format!("-{pid}")) | kill(pid.to_string())
}

#[cfg(windows)]
fn terminate(pid: u32) -> bool {
    win::terminate(pid)
}

#[cfg(not(any(unix, windows)))]
fn terminate(_pid: u32) -> bool {
    false
}

// ---- problems: shown as a page when someone clicked a shortcut, logged otherwise

#[derive(Debug)]
pub struct Problem {
    pub title: String,
    pub what: String,
    pub tips: Vec<String>,
    pub log: Option<PathBuf>,
}

impl Problem {
    fn missing(app: &App) -> Problem {
        Problem {
            title: format!("{} is not installed", app.title),
            what: format!(
                "tempo-launch looked for {}{EXE} next to itself and in TEMPO_BIN.",
                app.bin
            ),
            tips: vec![format!(
                "Run the Perennos Tempo installer again and keep {} selected. Your notes and settings stay as they are.",
                app.title
            )],
            log: None,
        }
    }

    fn busy(app: &App, port: u16) -> Problem {
        Problem {
            title: format!("{} could not start", app.title),
            what: format!(
                "Another program is using port {port}, the one {} needs.",
                app.title
            ),
            tips: vec![
                "Close the program that uses it, or restart the computer.".into(),
                format!("Then open {} again.", app.title),
            ],
            log: None,
        }
    }

    fn cannot_start(app: &App, why: &str) -> Problem {
        Problem {
            title: format!("{} could not start", app.title),
            what: format!("Windows would not start it: {why}"),
            tips: vec!["Run the Perennos Tempo installer again to repair it.".into()],
            log: None,
        }
    }

    fn exited(app: &App, st: ExitStatus, log: PathBuf) -> Problem {
        let code = st
            .code()
            .map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
        Problem {
            title: format!("{} stopped while starting", app.title),
            what: format!("It exited with {code} before it was ready."),
            tips: vec![
                "The last lines it wrote are below; they usually say why.".into(),
                format!("Open {} again once that is fixed.", app.title),
            ],
            log: Some(log),
        }
    }

    fn slow(app: &App, log: PathBuf) -> Problem {
        Problem {
            title: format!("{} is taking longer than usual", app.title),
            what: format!(
                "It started but did not answer within {} seconds.",
                app.wait.as_secs()
            ),
            tips: vec![
                format!(
                    "It may still be starting: open {} again in a minute.",
                    app.title
                ),
                "The last lines it wrote are below.".into(),
            ],
            log: Some(log),
        }
    }

    fn other(title: &str, what: String) -> Problem {
        Problem {
            title: title.into(),
            what,
            tips: vec![],
            log: None,
        }
    }
}

const MARK_SVG: &str = r##"<svg class="mark" viewBox="0 0 64 64" aria-hidden="true"><rect width="64" height="64" rx="14" fill="#23262d"/><path d="M18 46V18h11a9 9 0 0 1 0 18H24" fill="none" stroke="#ECE6D8" stroke-width="5" stroke-linecap="round" stroke-linejoin="round"/><circle cx="44" cy="44" r="5" fill="#E8552F"/></svg>"##;

/// The page shown when an app could not be opened: what happened, what to try, the log's last lines.
pub fn problem_page(p: &Problem, tail: &str, when: u64) -> String {
    let tips: String = p
        .tips
        .iter()
        .map(|t| format!("<li>{}</li>", html_escape(t)))
        .collect();
    let log = match &p.log {
        Some(path) => format!(
            "{}<p class=\"small\">The whole log: <code>{}</code></p>",
            if tail.trim().is_empty() {
                String::new()
            } else {
                format!("<pre>{}</pre>", html_escape(tail))
            },
            html_escape(&path.to_string_lossy())
        ),
        None => String::new(),
    };
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} · Perennos Tempo</title>
<style>
:root{{color-scheme:dark}}
body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#16181d;color:#ece6d8;font:15px/1.6 "Segoe UI",system-ui,-apple-system,sans-serif}}
main{{width:min(640px,100%);box-sizing:border-box;padding:48px 28px}}
.mark{{width:44px;height:44px;margin-bottom:30px}}
h1{{font:500 27px/1.25 "Iowan Old Style","Palatino Linotype",Palatino,Georgia,serif;margin:0 0 10px;letter-spacing:.005em}}
p,li{{color:#cfc9bd}}
p{{margin:0 0 14px}}
ul{{margin:0 0 24px;padding-left:20px}}
li{{margin:4px 0}}
pre{{background:#0e1013;border:1px solid #2a2c31;border-radius:10px;padding:14px 16px;overflow:auto;max-height:340px;font:12.5px/1.5 ui-monospace,"Cascadia Mono",Consolas,monospace;color:#a9aaad;white-space:pre-wrap;word-break:break-word}}
code{{font:12.5px ui-monospace,"Cascadia Mono",Consolas,monospace;color:#d9dde2}}
.small{{font-size:12.5px;color:#8a8b8f}}
.dot{{display:inline-block;width:7px;height:7px;border-radius:50%;background:#e8552f;margin-right:8px;vertical-align:1px}}
</style></head>
<body><main>
{mark}
<h1>{title}</h1>
<p>{what}</p>
<ul>{tips}</ul>
{log}
<p class="small"><span class="dot"></span>Nothing was changed or lost. {when}</p>
</main></body></html>
"#,
        title = html_escape(&p.title),
        mark = MARK_SVG,
        what = html_escape(&p.what),
        tips = tips,
        log = log,
        when = html_escape(&stamp(when)),
    )
}

impl Ctx {
    /// Write the problem page and open it.
    fn show_problem(&self, p: &Problem) {
        let tail = p
            .log
            .as_deref()
            .map(|l| log_tail(l, 30))
            .unwrap_or_default();
        let page = self.home.join("problem.html");
        if write_atomic(&page, problem_page(p, &tail, unix_now()).as_bytes()).is_ok() {
            let _ = self.open(&page.to_string_lossy());
        }
    }
}

// ---- apps on other machines (remote.json)

/// One app on another machine, from `<home>/remote.json`. tempo-launch only looks at it and opens
/// its address; it never starts or stops anything there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub id: String,
    pub title: String,
    pub url: String,
    pub health: String,
    /// The health answer holds this (whitespace removed).
    pub sign: String,
}

/// `[a-z0-9@._-]`, 1 to 64 characters.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@._-".contains(&b))
}

/// An address that is a tailnet or private one: 10/8, 172.16/12, 192.168/16, 100.64/10, 127/8,
/// IPv6 fd00::/8 and ::1.
pub fn private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(a) => {
            let o = a.octets();
            o[0] == 10
                || o[0] == 127
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 100 && (64..=127).contains(&o[1]))
        }
        std::net::IpAddr::V6(a) => a.is_loopback() || a.octets()[0] == 0xfd,
    }
}

/// Whether `host` (no port; an IPv6 address without brackets) is on a tailnet or private network,
/// or is a name that cannot be the public internet.
pub fn allowed_remote_host(host: &str) -> bool {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return private_ip(ip);
    }
    let ok_chars = host
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
    let labels_ok = host
        .split('.')
        .all(|l| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-'));
    if host.is_empty() || host.len() > 253 || !ok_chars || !labels_ok {
        return false;
    }
    if !host.contains('.') {
        // `tower`; never digits only or hex (`2130706433`, `0x7f000001` are addresses)
        return host.as_bytes()[0].is_ascii_lowercase();
    }
    [".ts.net", ".local", ".lan", ".internal", ".home.arpa"]
        .iter()
        .any(|s| host.ends_with(s) && host.len() > s.len())
}

/// `http://host[:port][/]` or `https://...` on an allowed host: (is_https, host, port).
pub fn parse_remote_url(url: &str) -> Result<(bool, String, u16), String> {
    let (https, rest) = if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        return Err("the address must start with http:// or https://".into());
    };
    if url.len() > 200 || url.bytes().any(|b| b <= b' ' || b >= 0x7f || b == b'\\') {
        return Err("the address holds a space, a control or a strange character".into());
    }
    let auth = rest.strip_suffix('/').unwrap_or(rest);
    if auth.contains(['/', '?', '#']) {
        return Err("the address must be host and port only, with no path or query".into());
    }
    if auth.contains('@') {
        return Err("the address must not hold a user name or password".into());
    }
    let (host, port) = if let Some(r) = auth.strip_prefix('[') {
        let (h, after) = r
            .split_once(']')
            .ok_or("the IPv6 address has no closing ]")?;
        (h, after.strip_prefix(':'))
    } else {
        match auth.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (auth, None),
        }
    };
    let port = match port {
        None => {
            if https {
                443
            } else {
                80
            }
        }
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("the port is out of range")?,
        Some(_) => return Err("the port is not a number".into()),
    };
    if !allowed_remote_host(host) {
        return Err(format!(
            "{host} is not a tailnet or private address (allowed: 10/8, 172.16/12, 192.168/16, 100.64/10, 127/8, fd00::/8, ::1, a single-label name like tower, or a name ending in .ts.net, .local, .lan, .internal, .home.arpa)"
        ));
    }
    Ok((https, host.to_string(), port))
}

/// One entry, checked. Errors say what is wrong in plain words.
fn check_remote(
    id: &str,
    title: &str,
    url: &str,
    health: &str,
    sign: &str,
) -> Result<Remote, String> {
    if !valid_id(id) {
        return Err("the id must be 1 to 64 characters from a-z 0-9 @ . _ -".into());
    }
    let bad_text = |s: &str| s.is_empty() || s.len() > 200 || s.chars().any(char::is_control);
    if bad_text(title) || bad_text(sign) {
        return Err("title and sign must be short text".into());
    }
    if !health.starts_with('/')
        || health.len() > 200
        || health.bytes().any(|b| b <= b' ' || b >= 0x7f || b == b'#')
    {
        return Err("health must be a path starting with /".into());
    }
    parse_remote_url(url)?;
    Ok(Remote {
        id: id.into(),
        title: title.into(),
        url: url.into(),
        health: health.into(),
        sign: sign.chars().filter(|c| !c.is_whitespace()).collect(),
    })
}

/// The `{...}` objects of the first array in `text` (strings and nesting respected).
fn json_objects(text: &str) -> Option<Vec<&str>> {
    let b = text.as_bytes();
    let mut i = text.find('[')? + 1;
    let mut out = Vec::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        match b.get(i)? {
            b']' => return Some(out),
            b'{' => {
                let (start, mut depth, mut in_str, mut esc) = (i, 0, false, false);
                loop {
                    let c = *b.get(i)?;
                    i += 1;
                    if in_str {
                        match (esc, c) {
                            (true, _) => esc = false,
                            (_, b'\\') => esc = true,
                            (_, b'"') => in_str = false,
                            _ => {}
                        }
                    } else if c == b'"' {
                        in_str = true;
                    } else if c == b'{' {
                        depth += 1;
                    } else if c == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                out.push(&text[start..i]);
            }
            _ => return None,
        }
    }
}

/// The remote apps in `<home>/remote.json` and one plain warning line for each thing wrong. A
/// missing file is no remote apps and no warning; a broken file is a warning, never a crash.
pub fn load_remote(home: &Path) -> (Vec<Remote>, Vec<String>) {
    let path = home.join("remote.json");
    let mut text = String::new();
    match File::open(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (vec![], vec![]),
        Err(e) => return (vec![], vec![format!("remote.json could not be read ({e})")]),
        Ok(f) => {
            if let Err(e) = f.take(65_537).read_to_string(&mut text) {
                return (vec![], vec![format!("remote.json could not be read ({e})")]);
            }
        }
    }
    let broken = |why: &str| (vec![], vec![format!("remote.json is ignored: {why}")]);
    if text.len() > 65_536 {
        return broken("it is over 64 KB");
    }
    let Some(list) = json_value(&text, "remote").filter(|v| v.starts_with('[')) else {
        return broken("it has no \"remote\" list");
    };
    let Some(objects) = json_objects(list) else {
        return broken("the \"remote\" list is not a list of {...} entries");
    };
    let (mut apps, mut warnings) = (Vec::<Remote>::new(), Vec::new());
    for o in objects {
        let get = |k: &str| json_str(o, k).unwrap_or_default();
        let id = get("id");
        match check_remote(
            &id,
            &get("title"),
            &get("url"),
            &get("health"),
            &get("sign"),
        ) {
            Ok(_) if apps.iter().any(|a| a.id == id) => warnings.push(format!(
                "remote.json: \"{id}\" is listed twice; the first is used"
            )),
            Ok(r) => apps.push(r),
            Err(e) => warnings.push(format!("remote.json: entry \"{id}\" is ignored: {e}")),
        }
    }
    (apps, warnings)
}

/// `answering`, `not answering` or `not checked (https)`. Plain http only (std only), short
/// timeouts, 200 and the sign in the body; a name is resolved on a thread so a dead resolver
/// cannot hold `status`, and only private addresses are ever connected to.
pub fn remote_state(r: &Remote) -> &'static str {
    let Ok((https, host, port)) = parse_remote_url(&r.url) else {
        return "not answering";
    };
    if https {
        return "not checked (https)";
    }
    let t = Duration::from_millis(1500);
    let (tx, rx) = std::sync::mpsc::channel();
    let h = host.clone();
    std::thread::spawn(move || {
        let _ = tx.send((h.as_str(), port).to_socket_addrs().ok());
    });
    let addrs = rx.recv_timeout(t).ok().flatten().into_iter().flatten();
    let hosthdr = if port == 80 {
        host
    } else {
        format!("{host}:{port}")
    };
    // the first private address only: a name that resolves to a public one is never connected to
    let ok = addrs
        .into_iter()
        .find(|a| private_ip(a.ip()))
        .and_then(|a| http_get_at(a, &hosthdr, &r.health, t))
        .is_some_and(|(code, body)| code == 200 && signed(&body, &[&r.sign]));
    if ok {
        "answering"
    } else {
        "not answering"
    }
}

/// `tempo-launch open ID`: the address in the browser. Nothing is started anywhere.
fn open_remote(ctx: &Ctx, id: &str) -> Result<i32, Problem> {
    let (apps, warnings) = load_remote(&ctx.home);
    for w in &warnings {
        eprintln!("{w}");
    }
    let Some(r) = apps.iter().find(|r| r.id == id) else {
        return Err(Problem::other(
            &format!("No remote app called {id}"),
            format!(
                "remote.json in {} lists: {}",
                ctx.home.display(),
                if apps.is_empty() {
                    "nothing".to_string()
                } else {
                    apps.iter()
                        .map(|r| r.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        ));
    };
    ctx.open(&r.url).map(|_| 0).map_err(|e| {
        Problem::other(
            &format!("{} did not open", r.title),
            format!("{e}. Open {} yourself.", r.url),
        )
    })
}

// ---- commands

#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// Open Perennos Tempo (no command given).
    Home,
    /// `home [--json]`: which Tempo folder is in use, and why.
    ShowHome {
        json: bool,
    },
    /// `home --merge FOLDER`: move another Tempo folder's files into the home.
    MergeHome(PathBuf),
    Field,
    Vault(Option<PathBuf>),
    Room,
    Agent,
    /// Tempo Studio has no page of its own: the agent, then its page, and how to use Studio.
    Studio,
    Start,
    Stop,
    Status {
        json: bool,
    },
    Learn,
    Schedule {
        remove: bool,
        print: bool,
        daily: bool,
    },
    Logs,
    /// Open an app on another machine, listed in remote.json.
    OpenRemote(String),
    /// `setup`: recommend models for this computer and your goals; `--apply` fetches them.
    Setup(setup::apply::Opts),
    /// `welcome`: write and open the first-run page.
    Welcome,
    /// `link URL`: add another computer's Ollama to the agent's `ollama_fallbacks`.
    Link(String),
    /// `mcp`: the setup helper as an MCP server on stdio; `--scan` prints the scan facts.
    Mcp {
        scan: bool,
    },
    /// `conductor ...` (also `band ...`): Tempo Conductor.
    Conductor(conductor::ConductorCmd),
    Help,
    Version,
}

/// The verb of a `perennos-tempo:` web link, or None if `arg` is not one. Only a short list of
/// verbs is accepted and everything after the verb (a path, a query, a fragment) is ignored, so a
/// web page that fires the link can open an app that is installed and nothing else.
pub fn protocol_verb(arg: &str) -> Option<Result<&'static str, String>> {
    const SCHEME: &str = "perennos-tempo:";
    let head = arg.get(..SCHEME.len())?;
    if !head.eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let rest = arg[SCHEME.len()..].trim_start_matches('/');
    let verb = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    Some(match verb.as_str() {
        "" | "open" => Ok(""),
        "agent" => Ok("agent"),
        "vault" => Ok("vault"),
        "room" => Ok("room"),
        "field" => Ok("field"),
        other => Err(format!("perennos-tempo: link not allowed: {other}")),
    })
}

/// The arguments the program really runs with. A startup task cannot pass a parameter, so the
/// Store package starts a copy of this program named `tempo-start.exe`: that name means `start`.
pub fn effective_args(args: &[String], program: Option<&str>) -> Vec<String> {
    let stem = program.map(str::to_ascii_lowercase);
    if args.is_empty() && stem.as_deref() == Some("tempo-start") {
        return vec!["start".to_string()];
    }
    args.to_vec()
}

pub fn parse(args: &[String]) -> Result<Cmd, String> {
    if let Some(v) = args.first().and_then(|a| protocol_verb(a)) {
        return match v? {
            "" => Ok(Cmd::Home),
            verb => parse(&[verb.to_string()]),
        };
    }
    let Some(cmd) = args.first().map(String::as_str) else {
        return Ok(Cmd::Home);
    };
    let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    let only = |allowed: &[&str]| -> Result<(), String> {
        match rest.iter().find(|r| !allowed.contains(r)) {
            Some(r) => Err(format!("{cmd}: unknown argument {r}")),
            None => Ok(()),
        }
    };
    Ok(match cmd {
        "home" => match rest.as_slice() {
            [] => Cmd::ShowHome { json: false },
            ["--json"] => Cmd::ShowHome { json: true },
            ["--merge", d] if !d.trim().is_empty() => Cmd::MergeHome(clean_folder_arg(d)),
            _ => return Err("home takes --json, or --merge FOLDER".into()),
        },
        "open" => match rest.as_slice() {
            [] => Cmd::Home,
            [id] if valid_id(id) => Cmd::OpenRemote(id.to_string()),
            _ => return Err("open takes one id from remote.json".into()),
        },
        "field" => {
            only(&[])?;
            Cmd::Field
        }
        "vault" => match rest.as_slice() {
            [] => Cmd::Vault(None),
            [d] if !d.starts_with("--") && !d.trim().is_empty() => {
                Cmd::Vault(Some(clean_folder_arg(d)))
            }
            _ => return Err("vault takes one folder".into()),
        },
        "room" => {
            only(&[])?;
            Cmd::Room
        }
        "agent" => {
            only(&[])?;
            Cmd::Agent
        }
        "studio" => {
            only(&[])?;
            Cmd::Studio
        }
        "start" => {
            only(&[])?;
            Cmd::Start
        }
        "stop" => {
            only(&[])?;
            Cmd::Stop
        }
        "status" => {
            only(&["--json"])?;
            Cmd::Status {
                json: rest.contains(&"--json"),
            }
        }
        "learn" => {
            only(&[])?;
            Cmd::Learn
        }
        "schedule" => {
            only(&["--install", "--remove", "--print", "--daily"])?;
            Cmd::Schedule {
                remove: rest.contains(&"--remove"),
                print: rest.contains(&"--print"),
                daily: rest.contains(&"--daily"),
            }
        }
        "logs" => {
            only(&[])?;
            Cmd::Logs
        }
        "setup" => Cmd::Setup(setup::apply::parse_args(&rest)?),
        "welcome" => {
            only(&[])?;
            Cmd::Welcome
        }
        "link" => match rest.as_slice() {
            [u] if !u.starts_with("--") => Cmd::Link(u.to_string()),
            _ => return Err("link takes one address, like http://192.168.0.20:11434".into()),
        },
        "mcp" => {
            only(&["--scan"])?;
            Cmd::Mcp {
                scan: rest.contains(&"--scan"),
            }
        }
        "conductor" | "band" => Cmd::Conductor(conductor::parse_args(&rest)?),
        "help" | "--help" | "-h" | "/?" => Cmd::Help,
        "version" | "--version" | "-V" => Cmd::Version,
        other => return Err(format!("unknown command: {other}")),
    })
}

/// The program: parse, run, and turn a problem into a page (shortcuts) or a log line (sign-in,
/// the scheduled task).
pub fn run(args: &[String]) -> i32 {
    #[cfg(windows)]
    win::attach_console();
    let program = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));
    let cmd = match parse(&effective_args(args, program.as_deref())) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tempo-launch: {e}\n\n{USAGE}");
            return 2;
        }
    };
    run_cmd(&Ctx::from_env(), cmd)
}

pub fn run_cmd(ctx: &Ctx, cmd: Cmd) -> i32 {
    let clicked = matches!(
        cmd,
        Cmd::Home | Cmd::Field | Cmd::Vault(_) | Cmd::Room | Cmd::Agent | Cmd::Studio
    );
    // (OpenRemote is typed in a terminal, so a failure is printed, not shown as a page)
    let r = match cmd {
        Cmd::Home => home(ctx),
        Cmd::ShowHome { json } => Ok(show_home(ctx, json)),
        Cmd::MergeHome(dir) => merge_home(ctx, &dir),
        Cmd::Field => open_field(ctx),
        Cmd::Vault(dir) => open_vault(ctx, dir.as_deref()),
        Cmd::Room => open_room(ctx),
        Cmd::Agent => agent(ctx),
        Cmd::Studio => studio(ctx),
        Cmd::Start => Ok(start(ctx)),
        Cmd::Stop => Ok(stop(ctx)),
        Cmd::Status { json } => {
            println!("{}", status(ctx, json));
            Ok(0)
        }
        Cmd::Learn => learn(ctx),
        Cmd::OpenRemote(id) => open_remote(ctx, &id),
        Cmd::Setup(o) => Ok(setup::run_setup(ctx, &o)),
        Cmd::Welcome => {
            setup::run_welcome(ctx, &setup::probe::RealHost, setup::apply::ollama_port())
                .map(|_| 0)
                .map_err(|e| Problem::other("The welcome page could not be opened", e))
        }
        Cmd::Link(url) => Ok(setup::propose::link(
            ctx,
            setup::propose::real_pinned(),
            &url,
            unix_now(),
            &mut io::stdout(),
        )),
        Cmd::Mcp { scan } => Ok(if scan {
            setup::mcp::print_scan(ctx)
        } else {
            setup::mcp::run_stdio(ctx)
        }),
        Cmd::Conductor(b) => Ok(conductor::run(ctx, b)),
        Cmd::Schedule {
            remove,
            print,
            daily,
        } => schedule(ctx, remove, print, daily),
        Cmd::Logs => {
            let d = ctx.logs_dir();
            let _ = fs::create_dir_all(&d);
            ctx.open(&d.to_string_lossy())
                .map(|_| 0)
                .map_err(|e| Problem::other("The logs folder could not be opened", e))
        }
        Cmd::Help => {
            println!("{USAGE}");
            Ok(0)
        }
        Cmd::Version => {
            println!("tempo-launch {VERSION}");
            Ok(0)
        }
    };
    match r {
        Ok(code) => code,
        Err(p) => {
            ctx.note(&format!("problem: {}: {}", p.title, p.what));
            eprintln!("{}: {}", p.title, p.what);
            if clicked {
                ctx.show_problem(&p);
            }
            1
        }
    }
}

/// Start apps that are installed and not running yet, without waiting for them.
fn start_in_background(ctx: &Ctx, apps: &[App]) {
    for app in apps {
        if ctx.find(app.bin).is_some() {
            if let Err(p) = ensure(ctx, &launch_for(ctx, *app), false) {
                ctx.note(&format!("{}: {}", p.title, p.what));
            }
        }
    }
}

fn home(ctx: &Ctx) -> Result<i32, Problem> {
    // the very first time, before any settings exist: the plan for this computer, as a page, once
    if let Some(p) = setup::first_start(ctx, &setup::probe::RealHost, setup::apply::ollama_port()) {
        ctx.note(&format!("setup: plan page opened: {}", p.display()));
    }
    // Tempo's own Ollama (when it has one and none answers), then the agent: the other apps use
    // it, and it takes a moment to connect its tools
    start_owned_ollama(ctx, false);
    start_in_background(ctx, &[AGENT]);
    if ctx.find(FIELD.bin).is_some() {
        open_field(ctx)
    } else {
        open_vault(ctx, None)
    }
}

/// `home [--json]`: the folder in use, the step that chose it, and any other Tempo folder with files.
fn show_home(ctx: &Ctx, json: bool) -> i32 {
    let env = |k: &str| std::env::var_os(k);
    let windows = cfg!(windows);
    let user = tempo_home::user_home_from(windows, &env);
    let (step, why) = if env("TEMPO_LAUNCH_HOME").is_some_and(|v| !v.is_empty()) {
        // only this program reads it; the apps it starts are told this folder as TEMPO_HOME
        (
            "env:TEMPO_LAUNCH_HOME".to_string(),
            "TEMPO_LAUNCH_HOME is set".to_string(),
        )
    } else {
        home::step_of(&tempo_home::tempo_home().why)
    };
    let cands = home::candidates(windows, &env, user.as_deref());
    let others = home::others(&ctx.home, &cands);
    println!("{}", home::show(&ctx.home, &step, &why, &others, json));
    0
}

/// `home --merge FOLDER`: nothing is overwritten or deleted.
fn merge_home(ctx: &Ctx, other: &Path) -> Result<i32, Problem> {
    let other = expand_home(other);
    match home::merge(&ctx.home, &other, &home::today()) {
        Ok(m) => {
            println!("{}", m.text(&ctx.home, &other));
            ctx.note(&format!(
                "home: merged {}: {} moved, {} copied, {} left, {} skipped, {} problems",
                other.display(),
                m.moved.len(),
                m.copied.len(),
                m.left.len(),
                m.skipped.len(),
                m.errors.len()
            ));
            Ok(i32::from(!m.ok()))
        }
        Err(e) => Err(Problem::other("The folders were not merged", e)),
    }
}

fn open_page(ctx: &Ctx, title: &str, url: &str) -> Result<i32, Problem> {
    ctx.open(url).map(|_| 0).map_err(|e| {
        Problem::other(
            &format!("{title} is running but the browser did not open"),
            format!("{e}. Open {url} yourself."),
        )
    })
}

fn open_field(ctx: &Ctx) -> Result<i32, Problem> {
    let l = launch_for(ctx, FIELD);
    ensure(ctx, &l, true)?;
    open_page(ctx, FIELD.title, &l.url())
}

fn open_vault(ctx: &Ctx, folder: Option<&Path>) -> Result<i32, Problem> {
    let l = vault_launch(ctx, folder);
    let dir = l.dir.clone().unwrap_or_else(|| ctx.vault.clone());
    if !dir.exists() {
        // a new vault: Tempo Vault makes the folder, its settings and a first note
        let exe = ctx
            .find(VAULT.bin)
            .ok_or_else(|| Problem::missing(&VAULT))?;
        let out = run_hidden(Command::new(&exe).arg("init").arg(&dir))
            .map_err(|e| Problem::cannot_start(&VAULT, &e.to_string()))?;
        if !out.status.success() {
            return Err(Problem::other(
                "The vault could not be made",
                format!(
                    "tempo-vault init {} said: {}",
                    dir.display(),
                    redact(String::from_utf8_lossy(&out.stderr).trim())
                ),
            ));
        }
        ctx.note(&format!("vault: made {}", dir.display()));
    }
    ensure(ctx, &l, true)?;
    // the Vault's Tempo tab talks to the agent with a key its page supplies, from the fragment
    open_keyed_page(ctx, &VAULT, &l.url(), agent_key(ctx).as_deref())
}

fn open_room(ctx: &Ctx) -> Result<i32, Problem> {
    // Tempo answers in the room through the agent
    start_in_background(ctx, &[AGENT]);
    let l = launch_for(ctx, ROOM);
    ensure(ctx, &l, true)?;
    let url = ctx
        .find(ROOM.bin)
        .and_then(|exe| run_hidden(Command::new(exe).args(["owner-link", "--json"])).ok())
        .filter(|o| o.status.success())
        .and_then(|o| json_str(&String::from_utf8_lossy(&o.stdout), "link"))
        .filter(|u| loopback_url(u))
        .unwrap_or_else(|| l.url());
    open_page(ctx, ROOM.title, &url)
}

/// Whether `u` is a plain address on this computer (the owner link carries a sign-in key, so it
/// must never lead anywhere else).
pub fn loopback_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("http://") else {
        return false;
    };
    if u.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return false;
    }
    // the part before the first / ? or # is the host: nothing else may hide in it (no `user@`)
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((name, port)) = host.split_once(':') else {
        return false;
    };
    matches!(name, "127.0.0.1" | "localhost")
        && !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
}

/// The agent's key: `TEMPO_AGENT_KEY` if set (an agent on another machine), else the file the
/// agent writes at every start, `<agent data>/control.key`. Only a plain token is accepted.
fn agent_key(ctx: &Ctx) -> Option<String> {
    agent_key_with(ctx, std::env::var("TEMPO_AGENT_KEY").ok())
}

fn agent_key_with(ctx: &Ctx, env: Option<String>) -> Option<String> {
    let raw = env
        .filter(|k| !k.trim().is_empty())
        .or_else(|| fs::read_to_string(ctx.agent_data.join("control.key")).ok())?;
    let k = raw.trim();
    (!k.is_empty() && k.len() <= 512 && k.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| k.to_string())
}

/// The agent's chat page: `http://127.0.0.1:<port>/`, with `#key=<key>` when there is a key. The
/// page reads the key from the fragment, which the browser never sends anywhere. The key is
/// percent-encoded so nothing in it can change the address.
pub fn agent_page_url(base: &str, key: Option<&str>) -> String {
    let Some(k) = key else {
        return base.to_string();
    };
    let mut enc = String::with_capacity(k.len());
    for b in k.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{base}#key={enc}")
}

/// `target` with the value of a `#key=` fragment hidden.
fn hide_key(target: &str) -> String {
    match target.find("#key=") {
        Some(i) => format!("{}#key=<hidden>", &target[..i]),
        None => target.to_string(),
    }
}

fn agent(ctx: &Ctx) -> Result<i32, Problem> {
    let l = agent_up(ctx)?;
    open_agent_page(ctx, &l.url(), agent_key(ctx).as_deref())
}

/// Start the agent if it is not running (Tempo's own Ollama first, when it has one) and say where it
/// is. Nothing else is started.
fn agent_up(ctx: &Ctx) -> Result<Launch, Problem> {
    start_owned_ollama(ctx, false);
    let l = launch_for(ctx, AGENT);
    ensure(ctx, &l, true)?;
    println!("{} is running on 127.0.0.1:{}", AGENT.title, l.port);
    Ok(l)
}

/// `studio`: Tempo Studio is a command-line program and a tool the agent uses; it has no window or
/// server to open. So this does what helps: the agent, its page, and one plain line on how to use
/// Studio. The line is printed (in a terminal) and written to launch.log (from a shortcut nothing
/// is shown, so that is where it can be read).
fn studio(ctx: &Ctx) -> Result<i32, Problem> {
    let l = agent_up(ctx)?;
    println!("{STUDIO_LINE}");
    ctx.note(&format!("studio: {STUDIO_LINE}"));
    open_agent_page(ctx, &l.url(), agent_key(ctx).as_deref())
}

fn open_agent_page(ctx: &Ctx, base: &str, key: Option<&str>) -> Result<i32, Problem> {
    open_keyed_page(ctx, &AGENT, base, key)
}

/// Open an app's page with the agent's key in the fragment (the agent's own page, and the Vault's
/// Tempo tab). The message names the plain address: the key never reaches the problem page or the log.
fn open_keyed_page(ctx: &Ctx, app: &App, base: &str, key: Option<&str>) -> Result<i32, Problem> {
    ctx.open(&agent_page_url(base, key))
        .map(|_| 0)
        .map_err(|e| {
            Problem::other(
                &format!("{} is running but the browser did not open", app.title),
                format!(
                    "{e}. Open {base} yourself; the agent's key is in {}.",
                    ctx.agent_data.join("control.key").display()
                ),
            )
        })
}

/// Sign-in: start everything that is installed, open nothing, write down how it went.
fn start(ctx: &Ctx) -> i32 {
    start_at(ctx, setup::apply::ollama_port())
}

fn start_at(ctx: &Ctx, ollama_port: u16) -> i32 {
    // the first time, before any settings exist: the plan for this computer, as a page, once
    if let Some(page) = setup::first_start(ctx, &setup::probe::RealHost, ollama_port) {
        ctx.note(&format!("setup: plan written to {}", page.display()));
    }
    // Tempo's own Ollama, when it has one and none answers, before the agent: the agent's models
    // come from it. (Waits until it answers, up to its limit.)
    start_owned_ollama_at(ctx, ollama_port, true);
    if let Some(line) = ollama_missing_line(
        &setup::probe::probe_ollama(&setup::probe::RealHost, ollama_port),
        owned_ollama_launch(ctx, ollama_port).is_some(),
    ) {
        println!("{line}");
        ctx.note(line);
    }
    let mut waiting = Vec::new();
    for app in APPS {
        if ctx.find(app.bin).is_none() {
            continue;
        }
        let l = launch_for(ctx, app);
        if app.id == "vault" && !ctx.vault.is_dir() {
            ctx.note(&format!(
                "vault: no folder at {}; not started",
                ctx.vault.display()
            ));
            continue;
        }
        match ensure(ctx, &l, false) {
            Ok(Up::Started(_)) => waiting.push(l),
            Ok(Up::Already) => {}
            Err(p) => ctx.note(&format!("{}: {}", p.title, p.what)),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut failed = 0;
    for l in waiting {
        while !l.answered() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
        if l.answered() {
            remember_image(ctx, l.app.id, l.port);
            ctx.note(&format!("{}: answering on port {}", l.log_name(), l.port));
        } else {
            failed += 1;
            ctx.note(&format!(
                "{}: did not answer; see logs/{}.log",
                l.log_name(),
                l.log_name()
            ));
        }
    }
    i32::from(failed > 0)
}

fn stop(ctx: &Ctx) -> i32 {
    let mut stopped = 0;
    for mut r in ctx.records() {
        if !is_ours(&r) {
            continue;
        }
        let title = APPS
            .iter()
            .chain([&OLLAMA])
            .find(|a| a.id == r.app)
            .map_or(r.app.as_str(), |a| a.title)
            .to_string();
        if terminate(r.pid) {
            // wait for the port to be let go, so a start right after works
            let until = Instant::now() + Duration::from_secs(5);
            while !port_free(r.port) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(100));
            }
            println!("stopped {title} (pid {})", r.pid);
            ctx.note(&format!("{}: stopped pid {}", r.app, r.pid));
            r.pid = 0;
            r.at = unix_now();
            ctx.save_record(&r);
            stopped += 1;
        } else {
            println!("could not stop {title} (pid {})", r.pid);
        }
    }
    if stopped == 0 {
        println!("nothing to stop: no app tempo-launch started is running");
    }
    0
}

/// Which apps are installed and running.
pub fn status(ctx: &Ctx, json: bool) -> String {
    status_with(ctx, json, setup::apply::ollama_port())
}

/// The Ollama row: Tempo's own copy, running only when Tempo started the process that answers.
/// An Ollama Tempo did not start is reported as such and is never counted as Tempo's.
fn ollama_row(
    ctx: &Ctx,
    records: &[Record],
    port: u16,
) -> (String, String, bool, bool, u16, Option<u32>, String) {
    let installed = setup::runtime::read_record(&ctx.home).is_some();
    let body = answers(&OLLAMA, port);
    let pid = records
        .iter()
        .find(|r| r.app == OLLAMA.id && r.port == port && r.pid != 0)
        .filter(|r| body.is_some() && is_ours(r))
        .map(|r| r.pid);
    let detail = match (&body, pid) {
        (Some(b), Some(_)) => format!("Ollama {}", json_str(b, "version").unwrap_or_default()),
        (Some(_), None) => format!(
            "another Ollama answers on port {port} (Tempo did not start it, and leaves it alone)"
        ),
        _ => String::new(),
    };
    (
        OLLAMA.id.to_string(),
        OLLAMA.title.to_string(),
        installed,
        pid.is_some(),
        port,
        pid,
        detail,
    )
}

pub fn status_with(ctx: &Ctx, json: bool, ollama_port: u16) -> String {
    let records = ctx.records();
    let mut rows = Vec::new();
    for app in APPS {
        let installed = ctx.find(app.bin).is_some();
        let body = answers(&app, app.port);
        let pid = records
            .iter()
            .find(|r| r.app == app.id && r.port == app.port && r.pid != 0)
            .filter(|r| body.is_some() && is_ours(r))
            .map(|r| r.pid);
        let detail = match (app.id, &body) {
            ("vault", Some(b)) => format!(
                "\"{}\", {} notes",
                json_str(b, "name").unwrap_or_default(),
                json_num(b, "notes").unwrap_or(0)
            ),
            ("room", Some(b)) => format!(
                "{} rooms, agent {}",
                json_num(b, "rooms").unwrap_or(0),
                json_str(b, "agent").unwrap_or_else(|| "?".into())
            ),
            _ => String::new(),
        };
        rows.push((
            app.id.to_string(),
            app.title.to_string(),
            installed,
            body.is_some(),
            app.port,
            pid,
            detail,
        ));
    }
    rows.push(ollama_row(ctx, &records, ollama_port));
    // vaults on other folders
    for r in records
        .iter()
        .filter(|r| r.app == "vault" && r.port != VAULT.port && r.pid != 0)
    {
        if let Some(b) = answers(&VAULT, r.port) {
            let detail = format!("{}, {} notes", r.dir, json_num(&b, "notes").unwrap_or(0));
            rows.push((
                "vault".into(),
                VAULT.title.into(),
                true,
                true,
                r.port,
                Some(r.pid),
                detail,
            ));
        }
    }
    let (remote, warnings) = load_remote(&ctx.home);
    let states: Vec<&str> = std::thread::scope(|s| {
        let hs: Vec<_> = remote.iter().map(|r| s.spawn(|| remote_state(r))).collect();
        hs.into_iter()
            .map(|h| h.join().unwrap_or("not answering"))
            .collect()
    });
    if json {
        let apps: Vec<String> = rows
            .iter()
            .map(|(id, title, inst, up, port, pid, detail)| {
                format!(
                    "{{\"id\":\"{id}\",\"title\":\"{title}\",\"installed\":{inst},\"running\":{up},\"port\":{port},\"pid\":{},\"detail\":\"{}\"}}",
                    pid.map_or("null".to_string(), |p| p.to_string()),
                    json_escape(detail)
                )
            })
            .collect();
        let remote_json: Vec<String> = remote
            .iter()
            .zip(&states)
            .map(|(r, st)| {
                format!(
                    "{{\"id\":\"{}\",\"title\":\"{}\",\"url\":\"{}\",\"state\":\"{st}\"}}",
                    json_escape(&r.id),
                    json_escape(&r.title),
                    json_escape(&r.url)
                )
            })
            .collect();
        let warn_json: Vec<String> = warnings
            .iter()
            .map(|w| format!("\"{}\"", json_escape(w)))
            .collect();
        return format!(
            "{{\"version\":\"{VERSION}\",\"home\":\"{}\",\"vault\":\"{}\",\"agent_data\":\"{}\",\"apps\":[{}],\"remote\":[{}],\"remote_warnings\":[{}]}}",
            json_escape(&ctx.home.to_string_lossy()),
            json_escape(&ctx.vault.to_string_lossy()),
            json_escape(&ctx.agent_data.to_string_lossy()),
            apps.join(","),
            remote_json.join(","),
            warn_json.join(",")
        );
    }
    let mut out = format!("Perennos Tempo {VERSION}\n");
    for (_, title, inst, up, port, pid, detail) in &rows {
        let state = match (inst, up) {
            (_, true) => "running",
            (true, false) => "stopped",
            (false, false) => "not installed",
        };
        let mut line = format!("  {title:<12} {state:<14}");
        if *up {
            line.push_str(&format!(" http://127.0.0.1:{port}/"));
            if let Some(p) = pid {
                line.push_str(&format!("  pid {p}"));
            }
        }
        // (a detail is also shown for an app that is not Tempo's own: another Ollama on the port)
        if !detail.is_empty() {
            line.push_str(&format!("  {detail}"));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    for (r, st) in remote.iter().zip(&states) {
        out.push_str(&format!("  {}  {st}  ({}, {})\n", r.title, r.id, r.url));
    }
    for w in &warnings {
        out.push_str(&format!("  {w}\n"));
    }
    out.push_str(&format!("  vault folder  {}\n", ctx.vault.display()));
    out.push_str(&format!("  logs          {}", ctx.logs_dir().display()));
    out
}

/// One learning pass: what the scheduled task runs. Quiet: its output goes to logs/learn.log.
fn learn(ctx: &Ctx) -> Result<i32, Problem> {
    let exe = ctx
        .find(VAULT.bin)
        .ok_or_else(|| Problem::missing(&VAULT))?;
    if !ctx.vault.is_dir() {
        ctx.note(&format!(
            "learn: no vault at {}; nothing to learn from",
            ctx.vault.display()
        ));
        return Ok(0);
    }
    let _ = fs::create_dir_all(&ctx.agent_data);
    let log_path = ctx.log_path("learn");
    let log = open_log(&log_path).map_err(|e| Problem::cannot_start(&VAULT, &e.to_string()))?;
    let err = log
        .try_clone()
        .map_err(|e| Problem::cannot_start(&VAULT, &e.to_string()))?;
    let mut cmd = Command::new(&exe);
    cmd.arg("learn")
        .arg("--vault")
        .arg(&ctx.vault)
        .arg("--agent-data")
        .arg(&ctx.agent_data);
    // Tempo Learn's model cycle too, when it is installed
    if ctx.find("tempo-learn").is_some() {
        cmd.arg("--with-model-cycle");
    }
    cmd.stdin(Stdio::null()).stdout(log).stderr(err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let st = cmd
        .status()
        .map_err(|e| Problem::cannot_start(&VAULT, &e.to_string()))?;
    ctx.note(&format!("learn: finished with {st}"));
    Ok(st.code().unwrap_or(1))
}

/// The command a Windows scheduled task runs.
pub fn task_command(exe: &Path) -> Result<String, String> {
    let p = exe.display().to_string();
    // the scheduler expands %NAME% in a command, and a quote or a line break would end the path
    if p.contains(['"', '%', '\n', '\r']) {
        return Err(format!(
            "the task command cannot hold a quote, a % or a line break; {p} has one. Install Perennos Tempo in a folder without them"
        ));
    }
    Ok(format!("\"{p}\" learn"))
}

/// `exe` as one word for cron's shell: single quotes expand nothing, and cron turns an unescaped
/// % into a line break, so it is escaped.
fn cron_word(exe: &Path) -> Result<String, String> {
    let p = exe.display().to_string();
    if p.contains(['\n', '\r', '\0']) {
        return Err(format!("the program's folder holds a line break: {p}"));
    }
    // an ordinary path stays in the double quotes it always had
    if !p.contains(['"', '$', '`', '\\', '%', '\'', '!']) {
        return Ok(format!("\"{p}\""));
    }
    Ok(format!(
        "'{}'",
        p.replace('\'', "'\\''").replace('%', "\\%")
    ))
}

/// The program a scheduled task should run. Inside the Store package the real path is a versioned
/// folder that changes with every update, so the task runs the package's command name instead:
/// `%LOCALAPPDATA%\Microsoft\WindowsApps\tempo-launch.exe` (declared in the manifest).
pub fn schedule_exe(packaged: bool, local_app_data: Option<&Path>, current: &Path) -> PathBuf {
    match (packaged, local_app_data) {
        (true, Some(l)) => l
            .join("Microsoft")
            .join("WindowsApps")
            .join("tempo-launch.exe"),
        _ => current.to_path_buf(),
    }
}

/// True when this program runs inside an MSIX package (the Store app). Then the package's
/// execution aliases give the command names and its startup task starts Tempo at sign-in, so
/// nothing here writes PATH or a Run key (the installer, which does, does not run in a package).
pub fn packaged() -> bool {
    #[cfg(windows)]
    {
        win::packaged()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The scheduler line or task command for `learn`, and how often it runs.
pub fn schedule_plan(exe: &Path, daily: bool) -> Result<(String, Vec<String>), String> {
    // a line break would start a second crontab line; no scheduler needs one in a program's path
    if exe.to_string_lossy().chars().any(char::is_control) {
        return Err("the program's folder name has a line break or another control character; install Perennos Tempo in a plainer folder".into());
    }
    if cfg!(windows) {
        // a windowless program: the task never shows a window
        let tr = task_command(exe)?;
        if tr.len() > 261 {
            return Err("the task command is longer than Windows allows (261 characters); install Perennos Tempo in a shorter folder".into());
        }
        let mut args: Vec<String> = ["/Create", "/F", "/TN", TASK_NAME, "/TR", &tr]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if daily {
            args.extend(["/SC", "DAILY", "/ST", "03:17"].map(String::from));
        } else {
            args.extend(["/SC", "HOURLY"].map(String::from));
        }
        Ok((tr, args))
    } else if cfg!(target_os = "macos") {
        // launchd, not cron: it runs a job missed while the Mac slept, and needs no extra permission
        let plist = launchd_plist(exe, daily, &default_home().join("logs").join("learn.log"));
        Ok((plist.clone(), vec![plist]))
    } else {
        let when = if daily { "17 3 * * *" } else { "17 * * * *" };
        let line = format!("{when} {} learn {CRON_MARK}", cron_word(exe)?);
        Ok((line.clone(), vec![line]))
    }
}

/// The current crontab, from what `crontab -l` said. "No crontab" is an empty one; any other
/// failure is an error, so the new crontab never replaces one that could not be read.
pub fn crontab_lines(ok: bool, stdout: &str, stderr: &str) -> Result<Vec<String>, String> {
    if ok {
        Ok(stdout.lines().map(str::to_string).collect())
    } else if stderr.to_ascii_lowercase().contains("no crontab") {
        Ok(vec![])
    } else {
        Err(format!(
            "crontab -l failed ({}); the existing schedule was left as it is",
            stderr.trim()
        ))
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The macOS launchd agent that runs `tempo-launch learn`: every hour, or every day at 03:17.
/// Pure text, so it is tested on every system.
pub fn launchd_plist(exe: &Path, daily: bool, log: &Path) -> String {
    let when = if daily {
        "<key>StartCalendarInterval</key>\n  <dict><key>Hour</key><integer>3</integer><key>Minute</key><integer>17</integer></dict>"
    } else {
        "<key>StartInterval</key>\n  <integer>3600</integer>"
    };
    let log = xml_escape(&log.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n  <key>Label</key>\n  <string>{LAUNCHD_LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>learn</string>\n  </array>\n  {when}\n  <key>ProcessType</key>\n  <string>Background</string>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n</dict>\n</plist>\n",
        xml_escape(&exe.to_string_lossy())
    )
}

/// Installs (or, with `remove`, takes out) the launchd agent. A removed plist is moved into
/// `<home>/trash`, never deleted.
fn schedule_launchd(ctx: &Ctx, remove: bool, plist: &str) -> Result<(), String> {
    let home = home_dir().ok_or("no home folder")?;
    let dir = home.join("Library").join("LaunchAgents");
    let path = dir.join(format!("{LAUNCHD_LABEL}.plist"));
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|u| !u.is_empty() && u.bytes().all(|b| b.is_ascii_digit()))
        .ok_or("cannot read the user id (id -u)")?;
    let domain = format!("gui/{uid}");
    let launchctl = |args: &[&str]| {
        Command::new("launchctl")
            .args(args)
            .stdin(Stdio::null())
            .output()
    };
    // out of the way first; "not loaded" is fine
    let _ = launchctl(&["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
    if remove {
        if path.is_file() {
            let trash = ctx.home.join("trash");
            fs::create_dir_all(&trash).map_err(|e| e.to_string())?;
            fs::rename(
                &path,
                trash.join(format!("{LAUNCHD_LABEL}-{}.plist", unix_now())),
            )
            .map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // launchd does not make the folder of the log it writes
    let _ = fs::create_dir_all(default_home().join("logs"));
    fs::write(&path, plist).map_err(|e| e.to_string())?;
    let out = launchctl(&["bootstrap", &domain, &path.to_string_lossy()])
        .map_err(|e| format!("cannot run launchctl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "launchctl bootstrap ended with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn schedule(ctx: &Ctx, remove: bool, print: bool, daily: bool) -> Result<i32, Problem> {
    let fail = |w: String| Problem::other("The learning schedule was not changed", w);
    let exe = std::env::current_exe().map_err(|e| fail(e.to_string()))?;
    let exe = schedule_exe(
        packaged(),
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .as_deref(),
        &exe,
    );
    let (what, args) = schedule_plan(&exe, daily).map_err(fail)?;
    if print {
        println!("{what}");
        return Ok(0);
    }
    if cfg!(windows) {
        let args: Vec<String> = if remove {
            ["/Delete", "/F", "/TN", TASK_NAME]
                .map(String::from)
                .to_vec()
        } else {
            args
        };
        let out = run_hidden(Command::new("schtasks").args(&args))
            .map_err(|e| fail(format!("cannot run schtasks: {e}")))?;
        if !out.status.success() {
            // schtasks writes some of its errors to stdout
            let said = [&out.stderr, &out.stdout]
                .iter()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| "no message".into());
            // removing a task that is not there is fine
            if remove {
                println!("no scheduled task \"{TASK_NAME}\"");
                return Ok(0);
            }
            return Err(fail(format!("schtasks ended with {}: {said}", out.status)));
        }
    } else if cfg!(target_os = "macos") {
        let plist = args.first().map(String::as_str).unwrap_or_default();
        schedule_launchd(ctx, remove, plist).map_err(fail)?;
    } else {
        let cur = Command::new("crontab")
            .arg("-l")
            .stdin(Stdio::null())
            .output();
        let mut lines: Vec<String> = match cur {
            Ok(o) => crontab_lines(
                o.status.success(),
                &String::from_utf8_lossy(&o.stdout),
                &String::from_utf8_lossy(&o.stderr),
            )
            .map_err(fail)?,
            Err(e) => return Err(fail(format!("cannot run crontab: {e}"))),
        };
        lines.retain(|l| !l.contains(CRON_MARK));
        if !remove {
            lines.extend(args);
        }
        let mut child = Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| fail(format!("cannot run crontab: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let mut text = lines.join("\n");
            text.push('\n');
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| fail(e.to_string()))?;
        }
        if !child.wait().is_ok_and(|s| s.success()) {
            return Err(fail("crontab refused the new schedule".into()));
        }
    }
    let done = if remove {
        format!("removed the learning schedule \"{TASK_NAME}\"")
    } else {
        format!(
            "Tempo learns from {} {} (no window)",
            ctx.vault.display(),
            if daily {
                "every day at 03:17"
            } else {
                "every hour"
            }
        )
    };
    ctx.note(&format!("schedule: {done}"));
    println!("{done}");
    Ok(0)
}

// ---- the browser

fn open_with_system(target: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        win::shell_open(target)
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("cannot run open: {e}"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("cannot run xdg-open: {e}"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(format!("no way to open {target} on this system"))
    }
}

// ---- Windows: the few system calls std does not wrap

#[cfg(windows)]
mod win {
    use std::ffi::{c_void, OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;

    type Handle = *mut c_void;

    #[repr(C)]
    struct Guid {
        a: u32,
        b: u16,
        c: u16,
        d: [u8; 8],
    }

    // {FDD39AD0-238F-46AF-ADB4-6C85480369C7}: the Documents folder, wherever it really is
    const DOCUMENTS: Guid = Guid {
        a: 0xFDD3_9AD0,
        b: 0x238F,
        c: 0x46AF,
        d: [0xAD, 0xB4, 0x6C, 0x85, 0x48, 0x03, 0x69, 0xC7],
    };

    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
        fn GetStdHandle(which: u32) -> Handle;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn QueryFullProcessImageNameW(h: Handle, flags: u32, buf: *mut u16, len: *mut u32) -> i32;
        fn TerminateProcess(h: Handle, code: u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn GetCurrentPackageFullName(len: *mut u32, name: *mut u16) -> i32;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: Handle,
            op: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> Handle;
        fn SHGetKnownFolderPath(
            id: *const Guid,
            flags: u32,
            token: Handle,
            path: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
        fn CoTaskMemFree(p: *mut c_void);
    }

    /// Inside an MSIX package? A call with no buffer answers ERROR_INSUFFICIENT_BUFFER (122) for a
    /// packaged program and APPMODEL_ERROR_NO_PACKAGE (15700) for any other.
    pub fn packaged() -> bool {
        const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
        let mut len = 0u32;
        unsafe {
            GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) == ERROR_INSUFFICIENT_BUFFER
        }
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    /// Started from a terminal: print there. Started from a shortcut: there is no console, and
    /// none is made.
    pub fn attach_console() {
        const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
        const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            if h.is_null() || h as isize == -1 {
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
    }

    /// Open a web address or a file the way double-clicking it would.
    pub fn shell_open(target: &str) -> Result<(), String> {
        let op = wide(OsStr::new("open"));
        let file = wide(OsStr::new(target));
        let r = unsafe {
            // COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE, as ShellExecute asks for
            CoInitializeEx(std::ptr::null_mut(), 0x2 | 0x4);
            ShellExecuteW(
                std::ptr::null_mut(),
                op.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // SW_SHOWNORMAL
            )
        };
        if r as isize > 32 {
            Ok(())
        } else {
            Err(format!("Windows could not open it (error {})", r as isize))
        }
    }

    pub fn documents() -> Option<PathBuf> {
        let mut p: *mut u16 = std::ptr::null_mut();
        let hr = unsafe { SHGetKnownFolderPath(&DOCUMENTS, 0, std::ptr::null_mut(), &mut p) };
        if p.is_null() {
            return None;
        }
        let out = (hr == 0).then(|| {
            let mut n = 0;
            unsafe {
                while *p.add(n) != 0 {
                    n += 1;
                }
                PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p, n)))
            }
        });
        unsafe { CoTaskMemFree(p.cast()) };
        out
    }

    pub fn image_of(pid: u32) -> Option<PathBuf> {
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 32_768];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            (ok != 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len as usize])))
        }
    }

    pub fn terminate(pid: u32) -> bool {
        const PROCESS_TERMINATE: u32 = 0x0001;
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return false;
            }
            let ok = TerminateProcess(h, 1) != 0;
            CloseHandle(h);
            ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-launch-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx_in(root: &Path) -> Ctx {
        Ctx {
            home: root.join("home"),
            agent_data: root.join("home").join("agent"),
            vault: root.join("Tempo Vault"),
            bin_dirs: vec![root.join("bin")],
            browser: false,
            opened: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(parse(&[]), Ok(Cmd::Home));
        assert_eq!(parse(&s(&["vault"])), Ok(Cmd::Vault(None)));
        assert_eq!(
            parse(&s(&["vault", "D:\\Notes"])),
            Ok(Cmd::Vault(Some(PathBuf::from("D:\\Notes"))))
        );
        // Explorer's "%V" on a drive root
        assert_eq!(
            parse(&s(&["vault", "D:\""])),
            Ok(Cmd::Vault(Some(PathBuf::from("D:\\"))))
        );
        assert_eq!(
            parse(&s(&["status", "--json"])),
            Ok(Cmd::Status { json: true })
        );
        assert_eq!(
            parse(&s(&["schedule", "--daily"])),
            Ok(Cmd::Schedule {
                remove: false,
                print: false,
                daily: true
            })
        );
        for (a, c) in [
            ("field", Cmd::Field),
            ("room", Cmd::Room),
            ("agent", Cmd::Agent),
            ("studio", Cmd::Studio),
            ("start", Cmd::Start),
            ("stop", Cmd::Stop),
            ("learn", Cmd::Learn),
            ("welcome", Cmd::Welcome),
            ("logs", Cmd::Logs),
            ("--help", Cmd::Help),
            ("--version", Cmd::Version),
        ] {
            assert_eq!(parse(&s(&[a])), Ok(c), "{a}");
        }
        assert!(parse(&s(&["nope"])).is_err());
        assert!(parse(&s(&["room", "--wat"])).is_err());
        assert!(parse(&s(&["vault", "a", "b"])).is_err());
    }

    #[test]
    fn studio_takes_no_arguments_and_is_not_an_app_with_a_server() {
        assert_eq!(
            parse(&s(&["studio", "--wat"])),
            Err("studio: unknown argument --wat".to_string())
        );
        assert!(parse(&s(&["studio", "D:\\Music"])).is_err());
        assert!(parse(&s(&["studio", "--json"])).is_err());
        assert!(parse(&s(&["studio", "agent"])).is_err());
        // a web page can fire the verbs of the link list and nothing else: studio is not one
        assert!(parse(&s(&["perennos-tempo:studio"])).is_err());
        // it is a command, not an entry of APPS: start, stop and status go through APPS and would
        // look for a program, a port and a health answer that Studio does not have
        assert!(APPS
            .iter()
            .all(|a| a.id != "studio" && a.bin != "tempo-studio"));
        // the help text names it, with how to use it
        assert!(
            USAGE.lines().any(|l| l.trim_start().starts_with("studio ")),
            "{USAGE}"
        );
        assert!(USAGE.contains("ask Tempo to analyse or master a file in your Music folder"));
        // the line it says is plain: no key, no address, and the product's name in full
        assert!(STUDIO_LINE.starts_with("Tempo Studio works through Tempo"));
        assert!(!STUDIO_LINE.contains("127.0.0.1") && !STUDIO_LINE.contains("key"));
    }

    #[test]
    fn reads_http_answers() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"ok\": true}xyz";
        assert_eq!(
            parse_response(ok, false),
            Some((200, "{\"ok\": true}xyz".into()))
        );
        let partial = b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n{\"ok\"";
        assert_eq!(parse_response(partial, false), None, "waits for the rest");
        assert_eq!(parse_response(partial, true), Some((200, "{\"ok\"".into())));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n6\r\n:true}\r\n0\r\n\r\n";
        assert_eq!(
            parse_response(chunked, false),
            Some((200, "{\"a\":true}".into()))
        );
        assert_eq!(parse_response(b"SSH-2.0-OpenSSH\r\n\r\n", true), None);
        assert_eq!(
            parse_response(b"HTTP/1.1 404 Not Found\r\n\r\n", true),
            Some((404, String::new()))
        );
    }

    #[test]
    fn http_get_does_not_wait_for_a_kept_alive_connection() {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut b = [0u8; 1024];
            let _ = c.read(&mut b);
            let body = "{\"ok\":true,\"app\":\"tempo-vault\",\"name\":\"Tempo Vault\"}";
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            std::thread::sleep(Duration::from_secs(5)); // never closes in time
        });
        let t = Instant::now();
        let (code, body) = http_get(port, "/vault/health", Duration::from_secs(3)).unwrap();
        assert_eq!(code, 200);
        assert!(signed(&body, VAULT.signs));
        assert_eq!(json_str(&body, "name").as_deref(), Some("Tempo Vault"));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(
            http_get(1, "/", Duration::from_millis(300)).is_none(),
            "nothing on port 1"
        );
    }

    #[test]
    fn health_signs_ignore_spacing_and_other_programs() {
        assert!(signed(
            "{\"app\": \"tempo-vault\", \"ok\": true}",
            VAULT.signs
        ));
        assert!(signed("{\n  \"guest\" : true\n}", AGENT.signs));
        assert!(
            signed("{\"stream\":true}", AGENT.signs),
            "an agent from before guest mode"
        );
        assert!(!signed("{\"app\":\"tempo-vaults\"}", ROOM.signs));
        assert!(!signed("<html>hello</html>", FIELD.signs));
    }

    #[test]
    fn json_values() {
        let t =
            r#"{"name":"Caf\u00e9 \"Notes\" \\ \ud83c\udfb5","notes":42,"n2" : 7, "a":{"pid":9}}"#;
        assert_eq!(json_str(t, "name").as_deref(), Some("Café \"Notes\" \\ 🎵"));
        assert_eq!(json_num(t, "notes"), Some(42));
        assert_eq!(json_num(t, "n2"), Some(7));
        assert_eq!(json_num(t, "pid"), Some(9));
        assert_eq!(json_str(t, "notes"), None, "not a string");
        assert_eq!(json_str(t, "missing"), None);
        assert_eq!(
            json_str(r#"{"x":"\ud800 lone"}"#, "x").as_deref(),
            Some("\u{FFFD} lone")
        );
        assert_eq!(json_escape("a\"b\\c\n\u{1}"), "a\\\"b\\\\c\\n\\u0001");
    }

    #[test]
    fn records_round_trip_windows_paths() {
        let r = Record {
            app: "vault".into(),
            pid: 4242,
            port: 8781,
            exe: r"C:\Users\Riley\AppData\Local\Programs\Perennos Tempo\bin\tempo-vault.exe".into(),
            img: String::new(),
            dir: r#"D:\Notes "2026""#.into(),
            at: 1_762_430_709,
        };
        assert_eq!(Record::parse(&r.to_json()), Some(r.clone()));
        let root = tmp("rec");
        let ctx = ctx_in(&root);
        ctx.save_record(&r);
        let mut r2 = r.clone();
        r2.pid = 0;
        ctx.save_record(&r2); // overwritten, not added
        assert_eq!(ctx.records(), vec![r2]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn utc_stamps() {
        assert_eq!(stamp(0), "1970-01-01 00:00:00Z");
        assert_eq!(stamp(951_782_400), "2000-02-29 00:00:00Z");
        assert_eq!(stamp(1_700_000_000), "2023-11-14 22:13:20Z");
        assert_eq!(file_stamp(1_700_000_000), "20231114-221320");
    }

    #[test]
    fn a_big_log_is_renamed_never_deleted() {
        let root = tmp("log");
        let p = root.join("logs").join("vault.log");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "x".repeat(100)).unwrap();
        roll_if_big(&p, 10);
        roll_if_big(&p, 10); // nothing left to roll
        fs::write(&p, "y".repeat(100)).unwrap();
        roll_if_big(&p, 10); // same second: a second name, not an overwrite
        let names: Vec<String> = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let total: u64 = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert_eq!(total, 200, "both logs are kept");
        let mut f = open_log(&p).unwrap();
        writeln!(f, "fresh").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "fresh\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn keys_are_hidden_in_what_is_shown() {
        let key = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0kLmN");
        let gh = format!("ghp_{}", "0123456789abcdefABCDEF0123");
        let text = format!(
            "using {key} now\nAuthorization: Bearer {}\nopen http://127.0.0.1:8767/#owner={} please\n{gh}, ok\nsha 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08\nplain words stay",
            "abc.def", "Zx9-Qw8_Er7Ty6"
        );
        let r = redact(&text);
        assert!(
            !r.contains(&key)
                && !r.contains("abc.def")
                && !r.contains("Zx9-Qw8_Er7Ty6")
                && !r.contains(&gh),
            "{r}"
        );
        assert!(!r.contains("9f86d081884c7d659a2f"), "{r}");
        assert!(r.contains("using [hidden] now"), "{r}");
        assert!(r.contains("#owner=[hidden] please"), "{r}");
        assert!(r.contains("plain words stay"));
    }

    #[test]
    fn problem_page_is_escaped_and_hides_keys() {
        let root = tmp("page");
        let log = root.join("vault.log");
        let key = format!("sk-{}", "Q1w2E3r4T5y6U7i8O9p0AaSs");
        fs::write(
            &log,
            format!("line 1\n<script>alert(1)</script>\nkey {key}\nerror: port 8766 in use\n"),
        )
        .unwrap();
        let p = Problem::exited(&VAULT, exit_status(3), log.clone());
        let page = problem_page(&p, &log_tail(&log, 30), 1_700_000_000);
        assert!(page.contains("Tempo Vault stopped while starting"));
        assert!(page.contains("error: port 8766 in use"));
        assert!(page.contains("&lt;script&gt;") && !page.contains("<script>"));
        assert!(!page.contains(&key));
        assert!(page.contains("2023-11-14 22:13:20Z"));
        assert!(page.contains("Nothing was changed or lost."));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }

    #[test]
    fn ports_and_folders() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = held.local_addr().unwrap().port();
        assert!(!port_free(p));
        let picked = pick_port(p..=p.saturating_add(20)).unwrap();
        assert_ne!(picked, p);
        assert_eq!(clean_folder_arg("  C:\\Notes "), PathBuf::from("C:\\Notes"));
        assert_eq!(clean_folder_arg("E:"), PathBuf::from("E:\\"));
        let root = tmp("same");
        assert!(same_place(&root, &root.join(".")));
        assert!(same_place(
            &root,
            Path::new(&format!("{}/", root.display()))
        ));
        assert!(!same_place(&root, &root.join("other")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_usual_launches() {
        let root = tmp("launch");
        let ctx = ctx_in(&root);
        let a = launch_for(&ctx, AGENT);
        assert_eq!(a.args, s(&["serve", "--port", "8765"]));
        assert_eq!(a.cwd, ctx.agent_data);
        assert!(a
            .env
            .iter()
            .any(|(k, v)| k == "TEMPO_AGENT_DATA" && *v == ctx.agent_data));
        assert!(
            !a.env.iter().any(|(k, _)| k == "TEMPO_VAULT"),
            "no vault folder yet"
        );
        fs::create_dir_all(&ctx.vault).unwrap();
        let a = launch_for(&ctx, AGENT);
        assert!(a
            .env
            .iter()
            .any(|(k, v)| k == "TEMPO_VAULT" && *v == ctx.vault));
        let v = vault_launch(&ctx, None);
        if port_free(VAULT.port) {
            assert_eq!(v.port, 8766);
            assert_eq!(v.log_name(), "vault");
            assert_eq!(v.url(), "http://127.0.0.1:8766/");
        } else {
            // another vault holds the usual port: this one opens next to it
            assert!(VAULT_PORTS.contains(&v.port), "{}", v.port);
        }
        assert!(v.accepts("{\"app\":\"tempo-vault\",\"name\":\"Tempo Vault\"}"));
        assert!(!v.accepts("{\"app\":\"tempo-vault\",\"name\":\"Someone Else\"}"));
        let other = root.join("Other Notes");
        let v2 = vault_launch(&ctx, Some(&other));
        assert!(VAULT_PORTS.contains(&v2.port), "{}", v2.port);
        assert_eq!(v2.log_name(), format!("vault-{}", v2.port));
        assert!(v2.args.contains(&other.to_string_lossy().into_owned()));
        assert_eq!(launch_for(&ctx, ROOM).args, s(&["serve", "--port", "8767"]));
        assert!(launch_for(&ctx, FIELD).args.is_empty());
        let (line, args) = schedule_plan(Path::new("/opt/tempo/tempo-launch"), false).unwrap();
        if cfg!(windows) {
            assert!(args.contains(&"HOURLY".to_string()));
        } else if cfg!(target_os = "macos") {
            assert!(line.contains(LAUNCHD_LABEL) && line.contains("<integer>3600</integer>"));
        } else {
            assert_eq!(
                line,
                "17 * * * * \"/opt/tempo/tempo-launch\" learn # perennos-tempo-vault-learn"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn launchd_plist_text_is_exact_and_escaped() {
        let exe = Path::new("/Applications/Perennos & Tempo/tempo-launch");
        let log = Path::new("/Users/riley/.tempo/logs/learn.log");
        let hourly = launchd_plist(exe, false, log);
        assert!(hourly.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"));
        assert!(
            hourly.contains("<key>Label</key>\n  <string>com.perennos.tempo.vault-learn</string>")
        );
        assert!(hourly.contains(
            "<array>\n    <string>/Applications/Perennos &amp; Tempo/tempo-launch</string>\n    <string>learn</string>\n  </array>"
        ));
        assert!(hourly.contains("<key>StartInterval</key>\n  <integer>3600</integer>"));
        assert!(!hourly.contains("StartCalendarInterval"));
        assert!(hourly.contains(
            "<key>StandardOutPath</key>\n  <string>/Users/riley/.tempo/logs/learn.log</string>"
        ));
        assert!(hourly.ends_with("</dict>\n</plist>\n"));
        let daily = launchd_plist(exe, true, log);
        assert!(daily
            .contains("<key>Hour</key><integer>3</integer><key>Minute</key><integer>17</integer>"));
        assert!(!daily.contains("StartInterval"));
    }

    #[test]
    fn a_missing_app_is_a_clear_problem() {
        let root = tmp("missing");
        let ctx = ctx_in(&root);
        let p = ensure(&ctx, &vault_launch(&ctx, Some(&root.join("x"))), true).unwrap_err();
        assert_eq!(p.title, "Tempo Vault is not installed");
        assert!(p.tips[0].contains("installer"));
        let st = status(&ctx, false);
        assert!(st.contains("Tempo Vault"), "{st}");
        let j = status(&ctx, true);
        assert!(
            j.starts_with("{\"version\":") && j.contains("\"apps\":["),
            "{j}"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Stands in for tempo-vault in the end-to-end test: `init DIR` makes the folder, `serve`
    /// answers health on its port until it is stopped. Does nothing in a normal test run.
    #[test]
    fn fake_product() {
        let Ok(args) = std::env::var("TEMPO_LAUNCH_FAKE_ARGS") else {
            return;
        };
        let a: Vec<&str> = args.split('\u{1f}').collect();
        let val = |k: &str| {
            a.iter()
                .position(|x| *x == k)
                .and_then(|i| a.get(i + 1))
                .copied()
        };
        if a.first() == Some(&"init") {
            fs::create_dir_all(a[1]).unwrap();
            fs::write(Path::new(a[1]).join("Welcome.md"), "# Welcome\n").unwrap();
            std::process::exit(0);
        }
        let port: u16 = val("--port").unwrap().parse().unwrap();
        let name = Path::new(val("--vault").unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        println!("fake tempo-vault serving \"{name}\" on {port}");
        for c in l.incoming().flatten() {
            let mut c = c;
            let mut b = [0u8; 2048];
            let _ = c.read(&mut b);
            let body = format!(
                "{{\"ok\":true,\"app\":\"tempo-vault\",\"name\":\"{}\",\"notes\":1}}",
                json_escape(&name)
            );
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn starts_hidden_waits_opens_and_stops() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("e2e");
        let ctx = ctx_in(&root);
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let me = std::env::current_exe().unwrap();
        let script = format!(
            "#!/bin/sh\nIFS=\"$(printf '\\037')\"\nTEMPO_LAUNCH_FAKE_ARGS=\"$*\" exec \"{}\" tests::fake_product --exact --nocapture --test-threads=1\n",
            me.display()
        );
        let fake = bin.join("tempo-vault");
        fs::write(&fake, script).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        // a folder that is not a vault yet: it is made first, then served, then opened
        let notes = root.join("Field Notes");
        assert_eq!(run_cmd(&ctx, Cmd::Vault(Some(notes.clone()))), 0);
        assert!(notes.join("Welcome.md").is_file(), "init made the vault");
        let opened = ctx.opened.borrow().clone();
        assert_eq!(opened.len(), 1);
        let port: u16 = opened[0]
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches('/')
            .parse()
            .unwrap();
        assert!(VAULT_PORTS.contains(&port));
        assert!(answers(&VAULT, port).is_some());
        let recs = ctx.records();
        assert_eq!(recs.len(), 1);
        assert!(is_ours(&recs[0]), "the record names the running program");

        // opening it again finds it on the same port and starts nothing
        assert_eq!(run_cmd(&ctx, Cmd::Vault(Some(notes.clone()))), 0);
        assert_eq!(ctx.opened.borrow().len(), 2);
        assert_eq!(ctx.opened.borrow()[1], opened[0]);
        assert_eq!(ctx.records().len(), 1);
        assert!(fs::read_to_string(ctx.log_path(&format!("vault-{port}")))
            .unwrap()
            .contains("fake tempo-vault serving \"Field Notes\""));
        assert!(status(&ctx, false).contains("Field Notes"));

        // stop ends it and lets the port go; the record stays, marked stopped
        assert_eq!(run_cmd(&ctx, Cmd::Stop), 0);
        assert!(answers(&VAULT, port).is_none());
        assert!(port_free(port));
        let recs = ctx.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].pid, 0);
        let launch_log = fs::read_to_string(ctx.log_path("launch")).unwrap();
        assert!(
            launch_log.contains("started pid") && launch_log.contains("stopped pid"),
            "{launch_log}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_crash_while_starting_shows_its_last_words() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("crash");
        let ctx = ctx_in(&root);
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("tempo-vault");
        fs::write(
            &fake,
            "#!/bin/sh\necho \"cannot read the vault: permission denied\" >&2\nexit 3\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let notes = root.join("Locked");
        fs::create_dir_all(&notes).unwrap();
        assert_eq!(run_cmd(&ctx, Cmd::Vault(Some(notes))), 1);
        let page = fs::read_to_string(ctx.home.join("problem.html")).unwrap();
        assert!(
            page.contains("Tempo Vault stopped while starting"),
            "{page}"
        );
        assert!(page.contains("exited with code 3"));
        assert!(page.contains("cannot read the vault: permission denied"));
        assert_eq!(
            ctx.opened.borrow().last().unwrap(),
            &ctx.home.join("problem.html").to_string_lossy()
        );
        let _ = fs::remove_dir_all(root);
    }

    // ---- security review (SEC-AGENT-LAUNCHER)

    #[test]
    fn programs_are_found_only_next_to_tempo_launch_or_in_tempo_bin() {
        let root = tmp("bins");
        let planted = root.join("planted");
        let pinned = root.join("pinned");
        fs::create_dir_all(&planted).unwrap();
        fs::create_dir_all(&pinned).unwrap();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let old_bin = std::env::var_os("TEMPO_BIN");
        let mut paths = vec![
            planted.clone(),
            PathBuf::from("relative-dir"),
            PathBuf::new(),
        ];
        paths.extend(std::env::split_paths(&old_path));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var(
            "TEMPO_BIN",
            std::env::join_paths([pinned.clone(), PathBuf::from("rel-bin")]).unwrap(),
        );
        let dirs = Ctx::from_env().bin_dirs;
        std::env::set_var("PATH", old_path);
        match old_bin {
            Some(v) => std::env::set_var("TEMPO_BIN", v),
            None => std::env::remove_var("TEMPO_BIN"),
        }
        assert!(
            dirs.contains(&pinned),
            "TEMPO_BIN is still honoured: {dirs:?}"
        );
        assert!(!dirs.contains(&planted), "PATH is never searched: {dirs:?}");
        assert!(
            dirs.iter().all(|d| d.is_absolute()),
            "a relative folder would mean the current folder: {dirs:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn stop_never_signals_a_process_id_it_could_not_have_started() {
        let me = Record {
            app: "vault".into(),
            pid: 1,
            port: 8766,
            exe: String::new(),
            img: image_of(1)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            dir: String::new(),
            at: 0,
        };
        // process 1 (and "-1", which would mean every process) is never ours, even when a record
        // names exactly the program that runs there
        assert!(!is_ours(&me));
        let mut big = me.clone();
        big.pid = u32::MAX;
        big.img = "/bin/sh".into();
        assert!(!is_ours(&big));
    }

    #[test]
    fn the_owner_link_must_stay_on_this_computer() {
        assert!(loopback_url("http://127.0.0.1:8767/#owner=abc"));
        assert!(loopback_url("http://localhost:8767/"));
        for bad in [
            "http://127.0.0.1:8767@evil.example/#owner=abc",
            "http://localhost:80@evil.example/",
            "http://127.0.0.1:8767.evil.example/",
            "http://127.0.0.1:/x",
            "http://127.0.0.1:8767\\@evil.example/",
            "http://127.0.0.1:8767/ calc.exe",
            "http://127.0.0.1:8767/\n",
            "https://127.0.0.1:8767/",
            "file:///C:/Windows/System32/calc.exe",
            "",
        ] {
            assert!(!loopback_url(bad), "{bad}");
        }
    }

    #[test]
    fn only_our_own_addresses_and_files_are_opened() {
        let root = tmp("open");
        let mut ctx = ctx_in(&root);
        ctx.browser = true; // the real thing: nothing foreign may reach the system opener
        for bad in [
            "calc.exe",
            "C:\\Windows\\System32\\calc.exe",
            "/bin/sh",
            "ms-msdt:/id PCWDiagnostic",
            "file:///etc/passwd",
            "https://example.com/",
            "http://127.0.0.1:8767@evil.example/",
            "--help",
        ] {
            let e = ctx.open(bad).unwrap_err();
            assert!(e.contains("refused"), "{bad}: {e}");
        }
        let _ = fs::remove_dir_all(root);
    }

    fn write_remote(ctx: &Ctx, text: &str) {
        fs::create_dir_all(&ctx.home).unwrap();
        fs::write(ctx.home.join("remote.json"), text).unwrap();
    }

    /// A one-answer HTTP server on 127.0.0.1; returns its port.
    fn mock(reply: &'static str) -> u16 {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { break };
                let mut b = [0u8; 2048];
                let n = c.read(&mut b).unwrap_or(0);
                let req = String::from_utf8_lossy(&b[..n]).to_lowercase();
                // nothing private may be sent: no cookie, no authorization
                let body = if req.contains("cookie") || req.contains("authorization") {
                    "HTTP/1.1 500 X\r\nContent-Length: 0\r\n\r\n".to_string()
                } else {
                    reply.to_string()
                };
                let _ = c.write_all(body.as_bytes());
            }
        });
        port
    }

    #[test]
    fn remote_hosts_are_tailnet_private_or_unroutable_names_only() {
        for good in [
            "http://tower:8766",
            "http://tower",
            "http://tower:8766/",
            "https://tower.tail1234.ts.net",
            "http://10.1.2.3:80",
            "http://172.16.0.1:80",
            "http://172.31.255.255:80",
            "http://192.168.0.5:8766",
            "http://127.0.0.1:8766",
            "http://[::1]:8766",
            "http://[fd7a:115c:a1e0::1]:8766",
            "http://nas.local:1",
            "http://pi.lan",
            "http://x.internal",
            "http://x.home.arpa",
        ] {
            assert!(parse_remote_url(good).is_ok(), "{good}");
        }
        // both ends of the tailnet range (100.64/10), put together at run time: the public
        // repository's leak scan refuses tailnet addresses written out in a file
        for (ip, port) in [
            (std::net::Ipv4Addr::new(100, 64, 0, 1), 8766),
            (std::net::Ipv4Addr::new(100, 127, 255, 254), 1),
        ] {
            let good = format!("http://{ip}:{port}");
            assert!(parse_remote_url(&good).is_ok(), "{good}");
        }
        for bad in [
            "http://example.com",
            "http://8.8.8.8:80",
            "http://100.63.255.255",
            "http://100.128.0.1",
            "http://172.32.0.1",
            "http://172.15.0.1",
            "http://192.169.0.1",
            "http://11.0.0.1",
            "http://[2001:4860::1]:80",
            "http://[fe80::1]:80",
            "http://[::ffff:8.8.8.8]:80",
            "http://2130706433",
            "http://0x7f000001",
            "http://1.2",
            "http://ts.net",
            "http://evil.ts.net.example.com",
            "http://tower.example.com",
            "http://Tower:8766",
            "ftp://tower",
            "tower:8766",
            "file:///etc/passwd",
            "http://",
            "http://tower:0",
            "http://tower:99999",
            "http://tower:abc",
        ] {
            assert!(parse_remote_url(bad).is_err(), "{bad}");
        }
        let e = parse_remote_url("http://example.com").unwrap_err();
        assert!(e.contains("not a tailnet or private address"), "{e}");
    }

    #[test]
    fn remote_urls_refuse_userinfo_query_path_and_odd_text() {
        for bad in [
            "http://user@tower:8766",
            "http://user:pw@tower",
            "http://tower:8766@evil.example",
            "http://tower@8.8.8.8",
            "http://tower:8766?x=1",
            "http://tower:8766/?x=1",
            "http://tower:8766/vault",
            "http://tower:8766#top",
            "http://tower 8766",
            "http://tower\\evil",
            "http://tower:8766\n",
        ] {
            assert!(parse_remote_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn remote_entries_need_a_good_id_health_and_sign() {
        let ok = |id: &str, health: &str, sign: &str| {
            check_remote(id, "Vault", "http://tower:8766", health, sign)
        };
        assert!(ok("vault@tower", "/vault/health", "\"app\":\"tempo-vault\"").is_ok());
        assert!(ok("a.b_c-9", "/h", "x").is_ok());
        for id in ["", "Vault", "va ult", "a/b", "a:b", &"a".repeat(65)] {
            assert!(ok(id, "/h", "x").is_err(), "{id:?}");
        }
        assert!(ok(&"a".repeat(64), "/h", "x").is_ok());
        assert!(ok("a", "health", "x").is_err());
        assert!(ok("a", "/h h", "x").is_err());
        assert!(ok("a", "/h", "").is_err());
        assert!(check_remote("a", "", "http://tower", "/h", "x").is_err());
        assert!(check_remote("a", "T", "http://8.8.8.8", "/h", "x").is_err());
    }

    #[test]
    fn a_missing_or_broken_remote_file_is_no_apps_and_never_a_crash() {
        let root = tmp("remote-file");
        let ctx = ctx_in(&root);
        assert_eq!(load_remote(&ctx.home), (vec![], vec![]), "missing file");
        let st = status(&ctx, false);
        assert!(!st.contains("remote.json"), "{st}");
        for broken in [
            "",
            "not json",
            "{\"remote\":",
            "{\"remote\":{}}",
            "{\"remote\":[1,2]}",
            "{\"remote\":[{\"id\":\"a\"",
        ] {
            write_remote(&ctx, broken);
            let (apps, warns) = load_remote(&ctx.home);
            assert!(apps.is_empty(), "{broken}");
            assert_eq!(warns.len(), 1, "{broken}: {warns:?}");
            assert!(warns[0].starts_with("remote.json is ignored"), "{warns:?}");
            let st = status(&ctx, false);
            assert!(st.contains("remote.json is ignored"), "{st}");
            assert!(status(&ctx, true).contains("\"remote_warnings\":[\"remote.json"));
        }
        // a bad entry is dropped with its own line; the good one stays; duplicates are refused
        write_remote(
            &ctx,
            r#"{"remote":[
              {"id":"vault@tower","title":"Tempo Vault on Tower","url":"http://tower:8766","health":"/vault/health","sign":"\"app\":\"tempo-vault\""},
              {"id":"evil","title":"Evil","url":"http://example.com","health":"/h","sign":"x"},
              {"id":"vault@tower","title":"Again","url":"http://tower:1","health":"/h","sign":"x"}
            ]}"#,
        );
        let (apps, warns) = load_remote(&ctx.home);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].title, "Tempo Vault on Tower");
        assert_eq!(apps[0].sign, "\"app\":\"tempo-vault\"");
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert!(warns[0].contains("evil") && warns[0].contains("not a tailnet or private"));
        let _ = fs::remove_dir_all(root);
    }

    fn remote_at(url: &str, sign: &str) -> Remote {
        check_remote("v@mock", "Mock Vault", url, "/vault/health", sign).unwrap()
    }

    #[test]
    fn a_remote_app_answers_only_with_200_and_its_sign_over_plain_http() {
        let good = mock(
            "HTTP/1.1 200 OK\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{ \"ok\": true, \"app\": \"tempo-vault\" }  ",
        );
        let sign = "\"app\":\"tempo-vault\"";
        assert_eq!(
            remote_state(&remote_at(&format!("http://127.0.0.1:{good}"), sign)),
            "answering"
        );
        // another program on that port
        assert_eq!(
            remote_state(&remote_at(
                &format!("http://127.0.0.1:{good}"),
                "\"app\":\"tempo-room\""
            )),
            "not answering"
        );
        // a redirect is never followed, even to the right place
        let redir = mock(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 40\r\n\r\n{\"app\":\"tempo-vault\"} xxxxxxxxxxxxxxxxxxx",
        );
        assert_eq!(
            remote_state(&remote_at(&format!("http://127.0.0.1:{redir}"), sign)),
            "not answering"
        );
        // nothing listens
        let dead = {
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        assert_eq!(
            remote_state(&remote_at(&format!("http://127.0.0.1:{dead}"), sign)),
            "not answering"
        );
        // https is not probed (no TLS in std)
        assert_eq!(
            remote_state(&remote_at(&format!("https://127.0.0.1:{good}"), sign)),
            "not checked (https)"
        );
    }

    #[test]
    fn status_lists_remote_apps_after_the_local_ones() {
        let port = mock(
            "HTTP/1.1 200 OK\r\nContent-Length: 24\r\nConnection: close\r\n\r\n{\"app\":\"tempo-vault\"}  ",
        );
        let root = tmp("remote-status");
        let ctx = ctx_in(&root);
        write_remote(
            &ctx,
            &format!(
                r#"{{"remote":[
                {{"id":"vault@mock","title":"Tempo Vault on Tower","url":"http://127.0.0.1:{port}","health":"/vault/health","sign":"\"app\":\"tempo-vault\""}},
                {{"id":"room@tower","title":"Tempo Room on Tower","url":"https://tower.tail1.ts.net","health":"/room/health","sign":"x"}}
                ]}}"#
            ),
        );
        let st = status(&ctx, false);
        let field = st.find("Tempo Field").unwrap();
        let vault = st
            .find("Tempo Vault on Tower  answering")
            .unwrap_or_else(|| panic!("{st}"));
        let room = st
            .find("Tempo Room on Tower  not checked (https)")
            .unwrap_or_else(|| panic!("{st}"));
        assert!(field < vault && vault < room, "{st}");
        let j = status(&ctx, true);
        assert!(
            j.contains("\"id\":\"vault@mock\"") && j.contains("\"state\":\"answering\""),
            "{j}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_opener_takes_a_validated_remote_url_and_nothing_else_foreign() {
        let root = tmp("remote-open");
        let mut ctx = ctx_in(&root);
        ctx.browser = false;
        let url = "http://tower:8766";
        // no remote.json: the same address is refused
        assert!(ctx.open(url).unwrap_err().contains("refused"));
        write_remote(
            &ctx,
            r#"{"remote":[
              {"id":"vault@tower","title":"V","url":"http://tower:8766","health":"/vault/health","sign":"x"},
              {"id":"evil","title":"E","url":"https://example.com","health":"/h","sign":"x"}
            ]}"#,
        );
        assert!(ctx.open(url).is_ok());
        assert_eq!(ctx.opened.borrow().as_slice(), [url]);
        // exactly that address: a different port, path, scheme, a lookalike or an entry that
        // failed validation is still refused
        for bad in [
            "http://tower:8767",
            "http://tower:8766/x",
            "https://tower:8766",
            "http://tower:8766@evil.example/",
            "http://tower:8766 ",
            "https://example.com",
            "http://example.com",
            "http://tower",
        ] {
            let e = ctx.open(bad).unwrap_err();
            assert!(e.contains("refused"), "{bad}: {e}");
        }
        // `open ID` goes through the same opener; an unknown id is a clear problem
        assert_eq!(
            parse(&s(&["open", "vault@tower"])),
            Ok(Cmd::OpenRemote("vault@tower".into()))
        );
        assert_eq!(parse(&s(&["open"])), Ok(Cmd::Home));
        assert!(parse(&s(&["open", "A B"])).is_err());
        assert_eq!(run_cmd(&ctx, Cmd::OpenRemote("vault@tower".into())), 0);
        assert_eq!(run_cmd(&ctx, Cmd::OpenRemote("evil".into())), 1);
        assert_eq!(run_cmd(&ctx, Cmd::OpenRemote("nope".into())), 1);
        assert_eq!(ctx.opened.borrow().len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn long_and_quoted_keys_are_hidden_too() {
        let b64 = "dGhpcy1pcy1hLXNlY3JldC10b2tlbi12YWx1ZS0xMjM0NTY3ODkw+/Qz==";
        let json_tok = "Zq8Xw3Lk9Pp2Vn7Bm4Tr6Yc1Hd5Gf0Js";
        let text = format!(
            "{{\"token\":\"{json_tok}\",\"user\":\"riley\"}}\nsecret {b64} end\nX-Api-Key: short123\nCookie: session=Abc123def456\n\"password\": \"hunter2hunter2\"\nAuthorization: Basic dXNlcjpwYXNz\nclient_secret=abc123XYZ\nplain words stay\nlistening on 127.0.0.1:8765"
        );
        let r = redact(&text);
        for secret in [
            json_tok,
            b64,
            "short123",
            "Abc123def456",
            "hunter2hunter2",
            "dXNlcjpwYXNz",
            "abc123XYZ",
        ] {
            assert!(!r.contains(secret), "{secret} leaked:\n{r}");
        }
        assert!(
            r.contains("plain words stay") && r.contains("listening on 127.0.0.1:8765"),
            "{r}"
        );
        assert!(
            r.contains("\"user\":\"riley\""),
            "ordinary fields stay: {r}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_problem_page_never_writes_through_a_link() {
        let root = tmp("link");
        let ctx = ctx_in(&root);
        fs::create_dir_all(&ctx.home).unwrap();
        let victim = root.join("victim.txt");
        fs::write(&victim, "precious").unwrap();
        std::os::unix::fs::symlink(&victim, ctx.home.join("problem.html")).unwrap();
        ctx.show_problem(&Problem::other("x", "y".into()));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert!(fs::read_to_string(ctx.home.join("problem.html"))
            .unwrap()
            .contains("Perennos Tempo"));
        // and a run record is replaced whole, with no half-written file left beside it
        let r = Record {
            app: "vault".into(),
            pid: 7,
            port: 8766,
            exe: "e".into(),
            img: String::new(),
            dir: String::new(),
            at: 1,
        };
        ctx.save_record(&r);
        ctx.save_record(&Record {
            pid: 0,
            ..r.clone()
        });
        assert_eq!(ctx.records(), vec![Record { pid: 0, ..r }]);
        let left: Vec<_> = fs::read_dir(ctx.run_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_crontab_line_survives_odd_folder_names() {
        if cfg!(windows) {
            return;
        }
        for odd in [
            "/opt/Tempo $(touch pwned)/tempo-launch",
            "/opt/it's `id`/tempo-launch",
            "/opt/100%/tempo-launch",
            "/opt/a b\"c/tempo-launch",
        ] {
            let (line, _) = schedule_plan(Path::new(odd), false).unwrap();
            if cfg!(target_os = "macos") {
                // launchd: the path is one XML string in ProgramArguments, no shell reads it
                let want = format!("<string>{}</string>", xml_escape(odd));
                assert!(line.contains(&want), "{line}");
                assert!(
                    !line.contains(&format!("<string>{odd}</string>")) || !odd.contains('"'),
                    "{line}"
                );
                continue;
            }
            // one line, no bare %, and the path sits inside single quotes where cron's sh expands nothing
            assert_eq!(line.lines().count(), 1, "{line}");
            assert!(!line.replace("\\%", "").contains('%'), "{line}");
            assert!(line.contains(" '/opt/"), "{line}");
            assert!(
                !line.contains("\"/opt"),
                "double quotes expand $ and backticks: {line}"
            );
        }
        // a name that would start a second crontab line is refused
        assert!(schedule_plan(Path::new("/opt/x\n* * * * * evil/tempo-launch"), false).is_err());
    }

    #[test]
    fn the_task_command_refuses_what_the_scheduler_would_expand() {
        assert_eq!(
            task_command(Path::new(
                "C:\\Program Files\\Perennos Tempo\\bin\\tempo-launch.exe"
            ))
            .unwrap(),
            "\"C:\\Program Files\\Perennos Tempo\\bin\\tempo-launch.exe\" learn"
        );
        assert!(task_command(Path::new("C:\\100%\\tempo-launch.exe")).is_err());
        assert!(task_command(Path::new("C:\\a\"b\\tempo-launch.exe")).is_err());
        assert!(task_command(Path::new("C:\\a\nb\\tempo-launch.exe")).is_err());
    }

    #[test]
    fn an_unreadable_crontab_is_never_overwritten() {
        // no crontab yet: fine, start an empty one
        assert_eq!(
            crontab_lines(false, "", "no crontab for riley\n"),
            Ok(vec![])
        );
        assert_eq!(
            crontab_lines(true, "0 1 * * * a\n5 2 * * * b\n", ""),
            Ok(vec!["0 1 * * * a".to_string(), "5 2 * * * b".to_string()])
        );
        // any other failure: stop, the owner's lines are not ours to replace
        assert!(crontab_lines(false, "", "crontab: permission denied").is_err());
        assert!(crontab_lines(false, "", "").is_err());
    }

    // ---- the agent's page and its key (ONE-HOME, contract B)

    #[test]
    fn the_agent_page_opens_with_the_key_in_the_fragment_only() {
        let root = tmp("agentkey");
        let ctx = ctx_in(&root);
        fs::create_dir_all(&ctx.agent_data).unwrap();
        fs::write(ctx.agent_data.join("control.key"), "Ab-12_xyz.~k\n").unwrap();
        let key = agent_key_with(&ctx, None);
        assert_eq!(key.as_deref(), Some("Ab-12_xyz.~k"));
        assert_eq!(
            open_agent_page(&ctx, "http://127.0.0.1:8765/", key.as_deref()).unwrap(),
            0
        );
        let opened = ctx.opened.borrow().clone();
        assert_eq!(opened, ["http://127.0.0.1:8765/#key=Ab-12_xyz.~k"]);
        assert!(!opened[0].contains('?'), "never a query string");
        assert!(
            loopback_url(&opened[0]),
            "the opener accepts exactly this form"
        );
        // the environment's key (an agent on another machine, tunnelled) wins over the file
        assert_eq!(
            agent_key_with(&ctx, Some("from-env-1".into())).as_deref(),
            Some("from-env-1")
        );
        // no key and no file: the plain address
        let bare = ctx_in(&tmp("agentnokey"));
        assert_eq!(agent_key_with(&bare, None), None);
        open_agent_page(&bare, "http://127.0.0.1:8765/", None).unwrap();
        assert_eq!(bare.opened.borrow().as_slice(), ["http://127.0.0.1:8765/"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_key_that_is_not_a_plain_token_is_not_used_and_cannot_change_the_address() {
        let ctx = ctx_in(&tmp("agentbad"));
        for bad in ["two words", "tab\there", "caf\u{e9}", ""] {
            assert_eq!(agent_key_with(&ctx, Some(bad.into())), None, "{bad:?}");
        }
        // odd but printable characters are percent-encoded, so the host and path cannot move
        let u = agent_page_url("http://127.0.0.1:8765/", Some("a/b@c?d#e+f=g\\h%"));
        assert_eq!(
            u,
            "http://127.0.0.1:8765/#key=a%2Fb%40c%3Fd%23e%2Bf%3Dg%5Ch%25"
        );
        assert!(loopback_url(&u));
    }

    #[test]
    fn the_loopback_check_still_refuses_look_alikes_with_a_key_fragment() {
        for bad in [
            "http://127.0.0.1:8765@evil.example/#key=k",
            "http://evil.example#@127.0.0.1:8765/#key=k",
            "http://127.0.0.1.evil.example:8765/#key=k",
            "http://127.0.0.1/#key=k",
            "https://127.0.0.1:8765/#key=k",
            "http://127.0.0.1:8765/#key=k k",
        ] {
            assert!(!loopback_url(bad), "{bad}");
        }
        assert!(loopback_url("http://localhost:8765/#key=k"));
    }

    #[test]
    fn the_key_never_reaches_stdout_the_problem_page_or_the_log() {
        let root = tmp("agentleak");
        let ctx = ctx_in(&root);
        let secret = "SECRET-key-0123456789";
        // refused address: the problem names the plain address and the key file, not the key
        let p = open_agent_page(&ctx, "http://evil.example/", Some(secret)).unwrap_err();
        assert!(
            !p.what.contains(secret) && !p.title.contains(secret),
            "{}",
            p.what
        );
        assert!(p.what.contains("control.key"));
        let page = problem_page(&p, "", 0);
        assert!(!page.contains(secret));
        assert!(
            ctx.opened.borrow().is_empty(),
            "a refused address is not recorded"
        );
        // what is printed or put in an error shows the key hidden
        assert_eq!(
            hide_key("http://127.0.0.1:8765/#key=SECRET-key-0123456789"),
            "http://127.0.0.1:8765/#key=<hidden>"
        );
        assert_eq!(hide_key("http://127.0.0.1:8765/"), "http://127.0.0.1:8765/");
        // nothing in the log folder holds it
        ctx.note("agent: page opened");
        let logs = fs::read_to_string(ctx.log_path("launch")).unwrap_or_default();
        assert!(!logs.contains(secret));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_apps_are_told_the_one_home() {
        let ctx = ctx_in(&tmp("homeenv"));
        for app in APPS {
            let l = launch_for(&ctx, app);
            assert!(
                l.env
                    .iter()
                    .any(|(k, v)| k == "TEMPO_HOME" && *v == ctx.home),
                "{}",
                app.id
            );
        }
    }

    #[test]
    fn a_web_link_is_one_of_five_verbs_and_nothing_more() {
        assert_eq!(parse(&s(&["perennos-tempo:open"])), Ok(Cmd::Home));
        assert_eq!(parse(&s(&["perennos-tempo:"])), Ok(Cmd::Home));
        assert_eq!(
            parse(&s(&["PERENNOS-TEMPO://vault/"])),
            Ok(Cmd::Vault(None))
        );
        assert_eq!(parse(&s(&["perennos-tempo:room?x=1"])), Ok(Cmd::Room));
        assert_eq!(parse(&s(&["perennos-tempo:agent#a"])), Ok(Cmd::Agent));
        assert_eq!(parse(&s(&["perennos-tempo:field"])), Ok(Cmd::Field));
        // a folder in the link is not used; these verbs are not offered to a web page
        assert_eq!(
            parse(&s(&["perennos-tempo:vault/D:/x"])),
            Ok(Cmd::Vault(None))
        );
        for bad in ["stop", "schedule", "home", "learn", "open-remote", "logs"] {
            assert!(
                parse(&s(&[&format!("perennos-tempo:{bad}")])).is_err(),
                "{bad}"
            );
        }
        assert!(parse(&s(&["perennos-tempo:vault", "x"])).is_ok());
    }

    #[test]
    fn the_sign_in_copy_means_start() {
        assert_eq!(effective_args(&[], Some("tempo-start")), s(&["start"]));
        assert_eq!(effective_args(&[], Some("Tempo-Start")), s(&["start"]));
        assert!(effective_args(&[], Some("tempo-launch")).is_empty());
        assert_eq!(
            effective_args(&s(&["vault"]), Some("tempo-start")),
            s(&["vault"])
        );
        assert!(effective_args(&[], None).is_empty());
    }

    #[test]
    fn a_scheduled_task_in_the_package_runs_the_alias() {
        let cur = Path::new("C:\\Program Files\\WindowsApps\\X_1.0.0.0\\bin\\tempo-launch.exe");
        assert_eq!(schedule_exe(false, Some(Path::new("C:\\L")), cur), cur);
        assert_eq!(schedule_exe(true, None, cur), cur);
        assert_eq!(
            schedule_exe(true, Some(Path::new("C:\\L")), cur),
            Path::new("C:\\L")
                .join("Microsoft")
                .join("WindowsApps")
                .join("tempo-launch.exe")
        );
    }

    #[test]
    fn parses_the_home_command() {
        assert_eq!(parse(&s(&["home"])), Ok(Cmd::ShowHome { json: false }));
        assert_eq!(
            parse(&s(&["home", "--json"])),
            Ok(Cmd::ShowHome { json: true })
        );
        assert_eq!(
            parse(&s(&["home", "--merge", "D:\\old"])),
            Ok(Cmd::MergeHome(PathBuf::from("D:\\old")))
        );
        assert!(parse(&s(&["home", "--merge"])).is_err());
        assert!(parse(&s(&["home", "--wat"])).is_err());
        assert!(parse(&s(&["home", "--json", "--merge", "x"])).is_err());
    }

    #[test]
    fn the_vault_page_opens_with_the_agent_key_in_the_fragment_only() {
        let root = tmp("vaultkey");
        let ctx = ctx_in(&root);
        fs::create_dir_all(&ctx.agent_data).unwrap();
        fs::write(ctx.agent_data.join("control.key"), "vk-123_abc\n").unwrap();
        let key = agent_key_with(&ctx, None);
        open_keyed_page(&ctx, &VAULT, "http://127.0.0.1:8766/", key.as_deref()).unwrap();
        // the environment's key wins, as for the agent page
        let env = agent_key_with(&ctx, Some("env-key-9".into()));
        open_keyed_page(&ctx, &VAULT, "http://127.0.0.1:8781/", env.as_deref()).unwrap();
        // no key: the plain address
        let bare = ctx_in(&tmp("vaultnokey"));
        open_keyed_page(
            &bare,
            &VAULT,
            "http://127.0.0.1:8766/",
            agent_key_with(&bare, None).as_deref(),
        )
        .unwrap();
        let opened = ctx.opened.borrow().clone();
        assert_eq!(
            opened,
            [
                "http://127.0.0.1:8766/#key=vk-123_abc",
                "http://127.0.0.1:8781/#key=env-key-9"
            ]
        );
        assert!(opened.iter().all(|u| !u.contains('?') && loopback_url(u)));
        assert_eq!(bare.opened.borrow().as_slice(), ["http://127.0.0.1:8766/"]);
        // a refused address: the problem names the plain address and the key file, never the key
        let p = open_keyed_page(
            &ctx,
            &VAULT,
            "http://evil.example/",
            Some("SECRET-vault-key-1"),
        )
        .unwrap_err();
        assert!(
            !p.what.contains("SECRET")
                && p.title.contains("Tempo Vault")
                && p.what.contains("control.key")
        );
        assert!(!problem_page(&p, "", 0).contains("SECRET"));
        let _ = fs::remove_dir_all(root);
    }

    // ---- setup

    #[test]
    fn setup_command_parses() {
        assert!(matches!(parse(&s(&["setup"])), Ok(Cmd::Setup(o)) if !o.apply));
        assert!(
            matches!(parse(&s(&["setup", "--apply", "--yes", "--with-d1", "--goal", "code"])),
            Ok(Cmd::Setup(o)) if o.apply && o.yes && o.with_d1 && o.goals == vec!["code"])
        );
        assert!(parse(&s(&["setup", "--nope"])).is_err());
    }

    #[test]
    fn the_agent_gets_config_json_from_setup_as_dash_dash_config() {
        let root = tmp("agentcfg");
        let ctx = ctx_in(&root);
        // no settings yet: plain `serve`
        assert_eq!(agent_config_args(&ctx, false), (s(&["serve"]), None));
        fs::create_dir_all(&ctx.agent_data).unwrap();
        let cfg = ctx.agent_data.join("config.json");
        fs::write(&cfg, "{}").unwrap();
        let (args, env) = agent_config_args(&ctx, false);
        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--config".into(),
                cfg.to_string_lossy().into_owned()
            ]
        );
        assert!(env.is_none());
        // TEMPO_AGENT_CONFIG already set: it wins, nothing is added
        assert_eq!(agent_config_args(&ctx, true), (s(&["serve"]), None));
        // the older agent-config.json keeps working as it did (as the variable)
        let old = ctx.agent_data.join("agent-config.json");
        fs::write(&old, "{}").unwrap();
        assert_eq!(
            agent_config_args(&ctx, false),
            (s(&["serve"]), Some(("TEMPO_AGENT_CONFIG".into(), old)))
        );
        // and the launch built from it puts --config before --port
        fs::remove_file(ctx.agent_data.join("agent-config.json")).unwrap();
        let l = launch_for(&ctx, AGENT);
        if std::env::var_os("TEMPO_AGENT_CONFIG").is_none() {
            assert_eq!(l.args[..2], ["serve".to_string(), "--config".to_string()]);
            assert_eq!(l.args[3..], ["--port".to_string(), "8765".to_string()]);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn first_start_opens_the_welcome_page_once_and_fetches_nothing() {
        use setup::probe::tests::Fake;
        let root = tmp("firststart");
        let ctx = ctx_in(&root);
        let host = Fake {
            os: "windows",
            arch: "x86_64",
            cpus: 4,
            win_mem: Some((8 << 30, 4 << 30)),
            win_disk: Some(100 << 30),
            ..Fake::default()
        };
        let page = setup::first_start(&ctx, &host, 1).unwrap();
        assert_eq!(page, ctx.home.join("welcome.html"));
        let html = fs::read_to_string(&page).unwrap();
        assert!(html.contains("Just this computer") && html.contains("Several computers"));
        assert!(html.contains("tempo-launch setup --apply"));
        assert!(html.contains("tempo-launch link http://"));
        assert!(html.contains("8 GB RAM, no GPU: qwen3:4b (2.5 GB)"));
        assert!(html.contains("claude mcp add tempo-setup --"));
        assert!(html.contains("claude_desktop_config.json") && html.contains("mcpServers"));
        assert!(html.contains("only the scan facts") || html.contains("Only the scan facts"));
        // Tempo Conductor: every template, the describe example, and the AI's two band tools
        assert!(html.contains("Let the Conductor set up a band for your work"));
        assert!(html.contains("tempo-launch conductor engines"));
        for t in conductor::templates::all() {
            assert!(html.contains(t.id), "{}", t.id);
        }
        assert!(html.contains("tempo-launch conductor new --describe"));
        assert!(html.contains("bakery&#39;s Instagram") || html.contains("bakery's Instagram"));
        assert!(html.contains("conductor_templates") && html.contains("conductor_propose"));
        assert!(html.contains("tempo-launch conductor apply-proposal"));
        // offline and strict: no outside address, no script, no form, a locked-down policy
        assert!(html.contains("default-src 'none'"));
        for bad in [
            "<script",
            "<link",
            "<form",
            "<iframe",
            "src=\"http",
            "href=\"http",
            "url(http",
        ] {
            assert!(!html.contains(bad), "{bad}");
        }
        // the plan page is there too, and the welcome page was the one opened
        let plan_html = fs::read_to_string(ctx.home.join("setup.html")).unwrap();
        assert!(plan_html.contains("USD 10 million") && plan_html.contains("--with-d1"));
        assert!(plan_html.contains("never downloaded"));
        assert_eq!(ctx.opened.borrow().len(), 1);
        assert!(ctx.opened.borrow()[0].ends_with("welcome.html"));
        // the second start: nothing more
        assert!(setup::first_start(&ctx, &host, 1).is_none());
        assert_eq!(ctx.opened.borrow().len(), 1);
        // a plan page the installer wrote with `setup --page` is kept as it is
        let root3 = tmp("firststart3");
        let ctx3 = ctx_in(&root3);
        let mut out = Vec::new();
        let o = setup::apply::parse_args(&["--page"]).unwrap();
        let code = setup::apply::run(
            &ctx3,
            &host,
            &o,
            1,
            1_793_000_000,
            &mut out,
            &mut |_| false,
            &mut |_| false,
        );
        assert_eq!(code, 0);
        let written = fs::read_to_string(ctx3.home.join("setup.html")).unwrap();
        assert!(ctx3.opened.borrow().is_empty());
        assert!(setup::first_start(&ctx3, &host, 1).is_some());
        assert_eq!(ctx3.opened.borrow().len(), 1);
        assert_eq!(
            fs::read_to_string(ctx3.home.join("setup.html")).unwrap(),
            written
        );
        assert!(setup::first_start(&ctx3, &host, 1).is_none());
        let _ = fs::remove_dir_all(root3);
        // with settings already there it never runs at all
        let root2 = tmp("firststart2");
        let ctx2 = ctx_in(&root2);
        fs::create_dir_all(&ctx2.agent_data).unwrap();
        fs::write(ctx2.agent_data.join("config.json"), "{}").unwrap();
        assert!(setup::first_start(&ctx2, &host, 1).is_none());
        assert!(!ctx2.home.join("welcome.html").exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(root2);
    }

    #[test]
    fn welcome_link_and_mcp_parse_and_the_link_is_checked() {
        assert_eq!(parse(&s(&["welcome"])), Ok(Cmd::Welcome));
        assert!(parse(&s(&["welcome", "x"])).is_err());
        assert_eq!(
            parse(&s(&["link", "http://192.168.0.20:11434"])),
            Ok(Cmd::Link("http://192.168.0.20:11434".into()))
        );
        assert!(parse(&s(&["link"])).is_err() && parse(&s(&["link", "a", "b"])).is_err());
        assert_eq!(parse(&s(&["mcp"])), Ok(Cmd::Mcp { scan: false }));
        assert_eq!(parse(&s(&["mcp", "--scan"])), Ok(Cmd::Mcp { scan: true }));
        assert!(parse(&s(&["mcp", "--write"])).is_err());
        let Ok(Cmd::Setup(o)) = parse(&s(&["setup", "--apply-proposal"])) else {
            panic!()
        };
        assert!(o.apply_proposal);
        assert!(parse(&s(&["setup", "--apply-proposal", "--yes"])).is_err());
        assert!(parse(&s(&["setup", "--apply-proposal", "--apply"])).is_err());

        let root = tmp("link");
        let ctx = ctx_in(&root);
        let run = |url: &str, pinned: Option<PathBuf>| {
            let mut out = Vec::new();
            let code = setup::propose::link(&ctx, pinned, url, 1_793_000_000, &mut out);
            (code, String::from_utf8(out).unwrap())
        };
        // a public address is refused, and nothing is written
        for bad in [
            "http://example.com:11434",
            "https://8.8.8.8",
            "http://user@10.0.0.2",
            "10.0.0.2",
            "http://10.0.0.2/x",
        ] {
            assert_eq!(run(bad, None).0, 2, "{bad}");
        }
        assert!(!ctx.agent_data.join("config.json").exists());
        // a private one is added, with the default port, into a new file
        let (code, said) = run("http://192.168.0.20", None);
        assert_eq!(code, 0, "{said}");
        let cfg = ctx.agent_data.join("config.json");
        let j = setup::json::parse(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(j.strs("ollama_fallbacks"), ["http://192.168.0.20:11434"]);
        // a second one is appended, other keys and the old file are kept, a repeat changes nothing
        fs::write(
            &cfg,
            r#"{"models":["a:1b"],"ollama_fallbacks":["http://10.0.0.5:11434"]}"#,
        )
        .unwrap();
        assert_eq!(run("http://tower.local:11434/", None).0, 0);
        let j = setup::json::parse(&fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(j.strs("models"), ["a:1b"]);
        assert_eq!(
            j.strs("ollama_fallbacks"),
            ["http://10.0.0.5:11434", "http://tower.local:11434"]
        );
        assert!(ctx.home.join("trash").is_dir());
        let before = fs::read_to_string(&cfg).unwrap();
        let (code, said) = run("http://tower.local:11434", None);
        assert_eq!(code, 0);
        assert!(said.contains("already"));
        assert_eq!(fs::read_to_string(&cfg).unwrap(), before);
        // a tailnet address works; a file TEMPO_AGENT_CONFIG names is left alone
        assert_eq!(run("http://10.20.30.40:11434", None).0, 0);
        let pinned = root.join("pinned.json");
        fs::write(&pinned, "{}").unwrap();
        let (code, said) = run("http://10.1.1.1:11434", Some(pinned.clone()));
        assert_eq!(code, 0);
        assert!(said.contains("\"ollama_fallbacks\": [\"http://10.1.1.1:11434\"]"));
        assert_eq!(fs::read_to_string(&pinned).unwrap(), "{}");
        // a settings file that is not an object is never overwritten
        fs::write(&cfg, "[1]").unwrap();
        assert_eq!(run("http://10.1.1.1:11434", None).0, 1);
        assert_eq!(fs::read_to_string(&cfg).unwrap(), "[1]");
        let _ = fs::remove_dir_all(root);
    }

    // ---- Tempo's own Ollama

    /// Run by the fake `ollama` script (see `own_ollama`): answers /api/version on OLLAMA_HOST.
    #[test]
    fn fake_ollama() {
        if std::env::var_os("TEMPO_FAKE_OLLAMA").is_none() {
            return;
        }
        let host = std::env::var("OLLAMA_HOST").unwrap();
        let l = TcpListener::bind(host.as_str()).unwrap();
        for c in l.incoming().flatten() {
            let mut c = c;
            let mut b = [0u8; 2048];
            let _ = c.read(&mut b);
            let body = "{\"version\":\"0.40.6\"}";
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    }

    /// The fake agent (and the check that none is there) uses the agent's real port, so the tests
    /// that do take turns.
    static AGENT_PORT_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run by the fake `tempo-local-agent` script: answers the agent's health on its own port.
    #[test]
    fn fake_agent() {
        if std::env::var_os("TEMPO_FAKE_AGENT").is_none() {
            return;
        }
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, AGENT.port)).unwrap();
        for c in l.incoming().flatten() {
            let mut c = c;
            let mut b = [0u8; 2048];
            let _ = c.read(&mut b);
            let body = "{\"ok\":true,\"guest\":true}";
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    }

    /// Tempo's own Ollama "installed" under `<home>/runtime`, as `setup --apply` leaves it: a fake
    /// `ollama` script that notes how it was started and then serves /api/version. Returns the
    /// program's path.
    #[cfg(unix)]
    fn own_ollama(ctx: &Ctx, root: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = ctx.home.join("runtime").join("ollama-0.40.6");
        fs::create_dir_all(&dir).unwrap();
        let me = std::env::current_exe().unwrap();
        let note = root.join("ollama");
        let script = format!(
            "#!/bin/sh\necho \"$*\" > \"{n}.args\"\necho \"$OLLAMA_HOST\" > \"{n}.host\"\necho \"${{OLLAMA_MODELS-unset}}\" > \"{n}.models\"\nTEMPO_FAKE_OLLAMA=1 exec \"{me}\" tests::fake_ollama --exact --nocapture --test-threads=1\n",
            n = note.display(),
            me = me.display()
        );
        let bin = dir.join("ollama");
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            ctx.home.join("runtime").join("ollama.json"),
            format!(
                "{{\"product\":\"Perennos Tempo\",\"purpose\":\"owned-ollama-runtime\",\"version\":\"0.40.6\",\"binary\":\"{}\"}}",
                json_escape(&bin.to_string_lossy())
            ),
        )
        .unwrap();
        bin
    }

    fn free_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn the_start_command_is_serve_with_only_ollama_host_set() {
        let root = tmp("own-launch");
        let ctx = ctx_in(&root);
        assert!(
            owned_ollama_launch(&ctx, 11434).is_none(),
            "no record, no launch"
        );
        let bin = ctx.home.join("runtime/ollama-0.40.6/ollama");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, "x").unwrap();
        fs::write(
            ctx.home.join("runtime/ollama.json"),
            format!(
                "{{\"purpose\":\"owned-ollama-runtime\",\"binary\":\"{}\"}}",
                json_escape(&bin.to_string_lossy())
            ),
        )
        .unwrap();
        let (l, exe) = owned_ollama_launch(&ctx, 11434).unwrap();
        assert_eq!(exe, bin);
        assert_eq!(l.args, vec!["serve"]);
        assert_eq!(l.port, 11434);
        // OLLAMA_HOST on this computer only; OLLAMA_MODELS (and everything else) is not touched
        assert_eq!(
            l.env,
            vec![("OLLAMA_HOST".to_string(), PathBuf::from("127.0.0.1:11434"))]
        );
        assert_eq!(l.log_name(), "ollama");
        let (l, _) = owned_ollama_launch(&ctx, 11500).unwrap();
        assert_eq!(l.env[0].1, PathBuf::from("127.0.0.1:11500"));
        assert_eq!(l.log_name(), "ollama-11500");
        assert_eq!(l.app.title, "Ollama (Tempo's own copy)");
        // the record of a program outside <home>/runtime is not used
        fs::write(
            ctx.home.join("runtime/ollama.json"),
            "{\"purpose\":\"owned-ollama-runtime\",\"binary\":\"/bin/sh\"}",
        )
        .unwrap();
        assert!(owned_ollama_launch(&ctx, 11434).is_none());
    }

    #[test]
    fn the_ollama_setting_must_name_this_computer() {
        for ok in [
            "http://127.0.0.1:11434",
            "http://localhost:11434/",
            "127.0.0.1:11434",
            "http://[::1]:11434",
            "11434",
            "",
        ] {
            assert!(ollama_url_is_local(ok), "{ok}");
        }
        for bad in [
            "http://192.168.0.5:11434",
            "http://gpu-box.tailnet.ts.net:11434",
            "http://127.0.0.1@evil.example:11434",
            "https://example.com",
            // a whole address written as one number is not a port: 10.0.0.1 and 127.0.0.1
            "http://167772161:11434",
            "http://2130706433:11434",
            "123456",
        ] {
            assert!(!ollama_url_is_local(bad), "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn own_ollama_starts_hidden_shows_in_status_and_is_stopped_by_its_pid_record() {
        let root = tmp("own-run");
        let ctx = ctx_in(&root);
        let bin = own_ollama(&ctx, &root);
        let port = free_port();
        // not running yet: installed, stopped
        let s = status_with(&ctx, false, port);
        let row = s
            .lines()
            .find(|l| l.contains("Ollama (Tempo's own copy)"))
            .unwrap();
        assert!(row.contains("stopped"), "{s}");
        let up = ensure_owned_ollama(&ctx, port, true).unwrap().unwrap();
        let Up::Started(pid) = up else {
            panic!("{up:?}")
        };
        // it was started as `ollama serve`, told its address, and nothing else
        let note = root.join("ollama");
        let noted = |ext: &str| {
            fs::read_to_string(format!("{}.{ext}", note.display()))
                .unwrap()
                .trim()
                .to_string()
        };
        assert_eq!(noted("args"), "serve");
        assert_eq!(noted("host"), format!("127.0.0.1:{port}"));
        assert_eq!(noted("models"), "unset");
        // (its log is named after the port when that is not the usual one)
        assert!(fs::read_to_string(ctx.log_path(&format!("ollama-{port}")))
            .unwrap()
            .contains(&bin.display().to_string()));
        // the record names it by pid
        let recs = ctx.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            (recs[0].app.as_str(), recs[0].pid, recs[0].port),
            ("ollama", pid, port)
        );
        assert!(is_ours(&recs[0]));
        // status shows it, with its pid; json too
        let s = status_with(&ctx, false, port);
        let row = s
            .lines()
            .find(|l| l.contains("Ollama (Tempo's own copy)"))
            .unwrap();
        assert!(
            row.contains("running") && row.contains(&format!("pid {pid}")),
            "{s}"
        );
        assert!(
            row.contains(&format!("http://127.0.0.1:{port}/")) && row.contains("Ollama 0.40.6"),
            "{s}"
        );
        let j = status_with(&ctx, true, port);
        assert!(
            j.contains("\"id\":\"ollama\"") && j.contains(&format!("\"pid\":{pid}")),
            "{j}"
        );
        // asking again starts nothing
        assert_eq!(
            ensure_owned_ollama(&ctx, port, true).unwrap(),
            Some(Up::Already)
        );
        assert_eq!(ctx.records()[0].pid, pid);
        // stop ends it by its pid record and lets the port go
        assert_eq!(run_cmd(&ctx, Cmd::Stop), 0);
        assert!(answers(&OLLAMA, port).is_none());
        assert!(port_free(port));
        assert_eq!(ctx.records()[0].pid, 0);
        let s = status_with(&ctx, false, port);
        assert!(
            s.lines()
                .any(|l| l.contains("Ollama (Tempo's own copy)") && l.contains("stopped")),
            "{s}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_ollama_tempo_did_not_start_is_never_replaced_stopped_or_called_ours() {
        let root = tmp("own-foreign");
        let ctx = ctx_in(&root);
        own_ollama(&ctx, &root);
        // some other Ollama answers on the port
        let port = mock("HTTP/1.1 200 OK\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"version\":\"0.6.1\"}");
        assert!(answers(&OLLAMA, port).is_some());
        // Tempo's own copy is not started over it, and no record is made
        assert_eq!(
            ensure_owned_ollama(&ctx, port, true).unwrap(),
            Some(Up::Already)
        );
        assert!(!root.join("ollama.args").exists(), "the program never ran");
        assert!(ctx.records().is_empty());
        // status says so, and does not call it Tempo's
        let s = status_with(&ctx, false, port);
        let row = s
            .lines()
            .find(|l| l.contains("Ollama (Tempo's own copy)"))
            .unwrap();
        assert!(!row.contains("running") && !row.contains("pid"), "{s}");
        assert!(
            row.contains("another Ollama answers on port") && row.contains("leaves it alone"),
            "{s}"
        );
        // stop has nothing to stop, and the other Ollama still answers
        assert_eq!(run_cmd(&ctx, Cmd::Stop), 0);
        assert!(answers(&OLLAMA, port).is_some());
        // even a stale record whose pid now belongs to some other program is not signalled
        let mut other = Command::new("sleep").arg("30").spawn().unwrap();
        ctx.save_record(&Record {
            app: "ollama".into(),
            pid: other.id(),
            port,
            exe: root.join("ollama").to_string_lossy().into_owned(),
            img: String::new(),
            dir: String::new(),
            at: 0,
        });
        assert_eq!(run_cmd(&ctx, Cmd::Stop), 0);
        assert!(
            other.try_wait().unwrap().is_none(),
            "an unrelated process is left running"
        );
        assert!(answers(&OLLAMA, port).is_some());
        other.kill().unwrap(); // our own child, by its pid
        let _ = other.wait();
        // a port held by something that is not an Ollama is a plain problem, not a start
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy = held.local_addr().unwrap().port();
        assert!(ensure_owned_ollama(&ctx, busy, false).is_err());
    }

    #[test]
    fn start_says_one_clear_line_only_when_there_is_no_ollama_at_all() {
        use setup::probe::Ollama;
        let none = Ollama::default();
        assert_eq!(ollama_missing_line(&none, false), Some(OLLAMA_MISSING));
        assert_eq!(
            OLLAMA_MISSING,
            "Ollama is not installed yet, so Tempo can't answer. Run: tempo-launch setup --apply (or install it from https://ollama.com/download)."
        );
        assert!(!OLLAMA_MISSING.contains('\n'));
        // Tempo's own copy, the system's program, or one that answers: nothing to say
        assert_eq!(ollama_missing_line(&none, true), None);
        let installed = Ollama {
            binary: Some(PathBuf::from("/x/ollama")),
            ..Ollama::default()
        };
        assert_eq!(ollama_missing_line(&installed, false), None);
        let running = Ollama {
            running: true,
            ..Ollama::default()
        };
        assert_eq!(ollama_missing_line(&running, false), None);
    }

    #[cfg(unix)]
    #[test]
    fn start_brings_up_tempos_own_ollama_before_the_agent() {
        use std::os::unix::fs::PermissionsExt;
        let _turn = AGENT_PORT_TURN.lock().unwrap_or_else(|e| e.into_inner());
        if !port_free(AGENT.port) {
            return; // something else is on the agent's port: this test cannot run here
        }
        let root = tmp("own-start");
        let ctx = ctx_in(&root);
        own_ollama(&ctx, &root);
        // not the first start: no plan page, no probes
        fs::create_dir_all(&ctx.home).unwrap();
        fs::write(ctx.home.join("setup.page-opened"), "x").unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let me = std::env::current_exe().unwrap();
        let agent = bin.join("tempo-local-agent");
        fs::write(
            &agent,
            format!(
                "#!/bin/sh\nif [ -f \"{r}/ollama.args\" ]; then echo yes; else echo no; fi > \"{r}/agent.saw-ollama\"\nTEMPO_FAKE_AGENT=1 exec \"{me}\" tests::fake_agent --exact --nocapture --test-threads=1\n",
                r = root.display(),
                me = me.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        let port = free_port();
        assert_eq!(start_at(&ctx, port), 0);
        // Ollama was already answering when the agent started
        assert_eq!(
            fs::read_to_string(root.join("agent.saw-ollama"))
                .unwrap()
                .trim(),
            "yes"
        );
        let log = fs::read_to_string(ctx.log_path("launch")).unwrap();
        let o = log.find(&format!("ollama-{port}: started pid")).unwrap();
        let a = log.find("agent: started pid").unwrap();
        assert!(o < a, "{log}");
        assert!(answers(&OLLAMA, port).is_some() && answers(&AGENT, AGENT.port).is_some());
        // stop ends both, by their records
        assert_eq!(run_cmd(&ctx, Cmd::Stop), 0);
        assert!(answers(&OLLAMA, port).is_none() && answers(&AGENT, AGENT.port).is_none());
    }

    /// Ends what tempo-launch started when a test ends, even by a failed assertion, so a fake agent
    /// never keeps the agent's port for the tests that follow.
    #[cfg(unix)]
    struct StopOnDrop<'a>(&'a Ctx);

    #[cfg(unix)]
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            stop(self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn studio_opens_the_agent_page_and_starts_nothing_else() {
        use std::os::unix::fs::PermissionsExt;
        let _turn = AGENT_PORT_TURN.lock().unwrap_or_else(|e| e.into_inner());
        if !port_free(AGENT.port) {
            return; // something else is on the agent's port: this test cannot run here
        }
        let root = tmp("studio");
        let ctx = ctx_in(&root);
        let _stop = StopOnDrop(&ctx);
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, text: String| {
            let p = bin.join(name);
            fs::write(&p, text).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        };
        // the agent is the fake that answers its health; Tempo Studio and the other apps are
        // installed too, and each leaves a mark if it is ever run
        let me = std::env::current_exe().unwrap();
        script(
            AGENT.bin,
            format!(
                "#!/bin/sh\nTEMPO_FAKE_AGENT=1 exec \"{}\" tests::fake_agent --exact --nocapture --test-threads=1\n",
                me.display()
            ),
        );
        let others = [VAULT.bin, ROOM.bin, FIELD.bin, "tempo-studio"];
        for name in others {
            script(
                name,
                format!("#!/bin/sh\necho ran > \"{}/ran-{name}\"\n", root.display()),
            );
        }
        fs::create_dir_all(&ctx.agent_data).unwrap();
        fs::write(ctx.agent_data.join("control.key"), "studio-key_1\n").unwrap();
        let page = agent_page_url(
            &format!("http://127.0.0.1:{}/", AGENT.port),
            agent_key(&ctx).as_deref(),
        );

        // the agent is not running: it is started, and its page (not another) is opened
        assert_eq!(run_cmd(&ctx, Cmd::Studio), 0);
        assert_eq!(ctx.opened.borrow().as_slice(), std::slice::from_ref(&page));
        assert!(page.starts_with("http://127.0.0.1:8765/") && loopback_url(&page));
        assert!(answers(&AGENT, AGENT.port).is_some());
        // nothing else was started: one record, the agent's, and none of the other programs ran
        let recs = ctx.records();
        assert_eq!(recs.len(), 1, "{recs:?}");
        assert_eq!(recs[0].app, "agent");
        assert!(is_ours(&recs[0]));
        let pid = recs[0].pid;
        for name in others {
            assert!(!root.join(format!("ran-{name}")).exists(), "{name} ran");
        }
        // the plain line is recorded, and the agent's key is not
        let launch_log = fs::read_to_string(ctx.log_path("launch")).unwrap();
        assert!(
            launch_log.contains(&format!("studio: {STUDIO_LINE}")),
            "{launch_log}"
        );
        assert!(!launch_log.contains("studio-key_1"), "{launch_log}");

        // again: the agent is found where it is, nothing is started, the page opens again
        assert_eq!(run_cmd(&ctx, Cmd::Studio), 0);
        assert_eq!(ctx.opened.borrow().as_slice(), [page.clone(), page]);
        let recs = ctx.records();
        assert_eq!((recs.len(), recs[0].pid), (1, pid));
        for name in others {
            assert!(!root.join(format!("ran-{name}")).exists(), "{name} ran");
        }

        // stop ends the agent, the only thing there is to end
        assert_eq!(run_cmd(&ctx, Cmd::Stop), 0);
        assert!(answers(&AGENT, AGENT.port).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn studio_without_the_agent_is_a_plain_problem_and_opens_no_agent_page() {
        let _turn = AGENT_PORT_TURN.lock().unwrap_or_else(|e| e.into_inner());
        if !port_free(AGENT.port) {
            return; // something else is on the agent's port: this test cannot run here
        }
        let root = tmp("studio-missing");
        let ctx = ctx_in(&root);
        // as from a shortcut: a page says why (the only thing opened), and nothing is started
        assert_eq!(run_cmd(&ctx, Cmd::Studio), 1);
        let page = fs::read_to_string(ctx.home.join("problem.html")).unwrap();
        assert!(page.contains("Tempo Agent is not installed"), "{page}");
        let opened = ctx.opened.borrow().clone();
        assert_eq!(opened, [ctx.home.join("problem.html").to_string_lossy()]);
        assert!(ctx.records().is_empty());
        // Studio is not described as ready when the agent that would use it is not there
        let launch_log = fs::read_to_string(ctx.log_path("launch")).unwrap();
        assert!(
            launch_log.contains("problem: Tempo Agent is not installed")
                && !launch_log.contains(STUDIO_LINE),
            "{launch_log}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_lists_the_ollama_row_even_when_there_is_none() {
        let root = tmp("own-none");
        let ctx = ctx_in(&root);
        let port = free_port();
        let s = status_with(&ctx, false, port);
        assert!(
            s.lines()
                .any(|l| l.contains("Ollama (Tempo's own copy)") && l.contains("not installed")),
            "{s}"
        );
        // nothing to start without a record
        assert_eq!(ensure_owned_ollama(&ctx, port, false).unwrap(), None);
    }
}
