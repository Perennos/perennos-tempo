//! `<home>/fleet.json`: the machines this person has, who is in the fleet and what each one last
//! said about itself. Changes go through `update`, which takes a lock file, so the CLI and the
//! running messenger never write over each other. The agent's settings are changed here too, with
//! the same private-address check as `tempo-launch link` and a backup of the old file.

use crate::setup::json::{parse, J};
use crate::setup::propose::{self, engine_url};
use crate::{allowed_remote_host, write_atomic, Ctx};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const FLEET_PORT: u16 = 8770;

pub fn fleet_port() -> u16 {
    std::env::var("TEMPO_FLEET_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .filter(|p| *p != 0)
        .unwrap_or(FLEET_PORT)
}

pub fn now_s() -> u64 {
    crate::unix_now()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The folders the fleet works in, copyable into threads (a `Ctx` is not).
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub agent_data: PathBuf,
}

impl Paths {
    pub fn of(ctx: &Ctx) -> Paths {
        Paths {
            home: ctx.home.clone(),
            agent_data: ctx.agent_data.clone(),
        }
    }
    pub fn ctx(&self) -> Ctx {
        Ctx {
            home: self.home.clone(),
            agent_data: self.agent_data.clone(),
            vault: self.home.join("vault"),
            bin_dirs: vec![],
            browser: false,
            opened: Default::default(),
        }
    }
    pub fn file(&self, name: &str) -> PathBuf {
        self.home.join(name)
    }
}

/// A name as other machines will see it: lower case letters, digits and `- _ .`.
pub fn clean_name(s: &str) -> String {
    let n: String = s
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    n.trim_matches(['-', '.']).chars().take(40).collect()
}

/// This computer's name.
pub fn hostname() -> String {
    let raw = ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| crate::setup::probe::run_with_timeout("hostname", &[], Duration::from_secs(2)))
        .unwrap_or_default();
    let n = clean_name(raw.split('.').next().unwrap_or(""));
    if n.is_empty() {
        "this-computer".into()
    } else {
        n
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// engine (serves models), agent (runs a Tempo agent) or both; a paired computer is chosen
    /// by its hardware: engine, client or runner.
    pub role: String,
    pub enabled: bool,
    pub last_seen: u64,
    /// What the last scan or probe found.
    pub probe: J,
    /// What its last heartbeat said.
    pub health: J,
    /// The last `fleet check`: ok, at, note.
    pub ssh: J,
    /// Its pinned identity (an Ed25519 public key, hex), once paired.
    pub id: String,
    /// What it offers to the fleet, from its heartbeat.
    pub tools: J,
}

impl Machine {
    pub fn new(name: &str, addr: &str, role: &str) -> Machine {
        Machine {
            name: name.into(),
            addr: addr.into(),
            port: fleet_port(),
            role: role.into(),
            enabled: true,
            last_seen: 0,
            probe: J::Null,
            health: J::Null,
            ssh: J::Null,
            id: String::new(),
            tools: J::Null,
        }
    }

    pub fn engine(&self) -> bool {
        matches!(self.role.as_str(), "engine" | "both")
    }

    pub fn engine_url(&self) -> String {
        format!("http://{}:{}", self.addr, propose::OLLAMA_DEFAULT_PORT)
    }

    fn to_j(&self) -> J {
        let mut o = vec![
            ("name".to_string(), J::s(&self.name)),
            ("addr".into(), J::s(&self.addr)),
            ("port".into(), J::n(f64::from(self.port))),
            ("role".into(), J::s(&self.role)),
            ("enabled".into(), J::Bool(self.enabled)),
            ("last_seen".into(), J::n(self.last_seen as f64)),
        ];
        if !self.id.is_empty() {
            o.push(("id".into(), J::s(&self.id)));
        }
        for (k, v) in [
            ("probe", &self.probe),
            ("health", &self.health),
            ("ssh", &self.ssh),
            ("tools", &self.tools),
        ] {
            if *v != J::Null {
                o.push((k.into(), v.clone()));
            }
        }
        J::Obj(o)
    }

    fn from_j(j: &J) -> Option<Machine> {
        let name = j.str("name")?.to_string();
        let addr = j.str("addr")?.to_string();
        if name.is_empty() || !allowed_remote_host(&addr) {
            return None;
        }
        let get = |k: &str| j.get(k).cloned().unwrap_or(J::Null);
        Some(Machine {
            name,
            addr,
            port: j.f64("port").map_or(fleet_port(), |p| p as u16).max(1),
            role: j.str("role").unwrap_or("engine").to_string(),
            enabled: j.bool("enabled").unwrap_or(true),
            last_seen: j.f64("last_seen").unwrap_or(0.0) as u64,
            probe: get("probe"),
            health: get("health"),
            ssh: get("ssh"),
            id: j.str("id").unwrap_or("").to_string(),
            tools: get("tools"),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fleet {
    /// This computer's own name in the fleet.
    pub me: String,
    pub machines: Vec<Machine>,
    /// What this computer lets the rest of the fleet run here (`comfy_image`, `mcp:server/tool`,
    /// `mcp:server/*`). Empty: nothing.
    pub allow: Vec<String>,
}

impl Fleet {
    pub fn find(&self, key: &str) -> Option<usize> {
        let k = key.trim().to_ascii_lowercase();
        self.machines
            .iter()
            .position(|m| m.name == k || m.addr.to_ascii_lowercase() == k)
    }

    /// This computer and every enabled machine.
    pub fn count(&self) -> usize {
        1 + self.machines.iter().filter(|m| m.enabled).count()
    }

    pub fn peers(&self) -> impl Iterator<Item = &Machine> {
        self.machines.iter().filter(|m| m.enabled)
    }

    pub fn to_j(&self) -> J {
        J::Obj(vec![
            ("version".into(), J::n(1.0)),
            ("self".into(), J::s(&self.me)),
            ("allow".into(), J::strings(&self.allow)),
            (
                "machines".into(),
                J::Arr(self.machines.iter().map(Machine::to_j).collect()),
            ),
        ])
    }

    pub fn from_j(j: &J) -> Fleet {
        Fleet {
            me: j.str("self").map(clean_name).unwrap_or_default(),
            allow: j.strs("allow"),
            machines: j
                .arr("machines")
                .iter()
                .filter_map(Machine::from_j)
                .collect(),
        }
    }
}

pub fn load(p: &Paths) -> Fleet {
    let mut f = std::fs::read_to_string(p.file("fleet.json"))
        .ok()
        .and_then(|t| parse(&t).ok())
        .map(|j| Fleet::from_j(&j))
        .unwrap_or_default();
    if f.me.is_empty() {
        f.me = hostname();
    }
    f
}

/// Read, change and write fleet.json while holding its lock. A lock older than ten seconds is
/// taken over (its owner died).
pub fn update<T>(p: &Paths, f: impl FnOnce(&mut Fleet) -> T) -> Result<T, String> {
    let lock = p.file("fleet.json.lock");
    let _ = std::fs::create_dir_all(&p.home);
    let start = Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => break,
            Err(_) => {
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|a| a > Duration::from_secs(10));
                if stale {
                    let _ = std::fs::remove_file(&lock);
                } else if start.elapsed() > Duration::from_secs(5) {
                    return Err("fleet.json is busy; try again".into());
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    let mut fleet = load(p);
    let out = f(&mut fleet);
    let r = write_atomic(&p.file("fleet.json"), fleet.to_j().pretty().as_bytes())
        .map_err(|e| format!("could not write fleet.json: {e}"));
    let _ = std::fs::remove_file(&lock);
    r.map(|()| out)
}

/// The fleet key: 32 bytes, made by `fleet init`.
pub fn read_key(p: &Paths) -> Option<[u8; 32]> {
    std::fs::read(p.file("fleet.key"))
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
}

/// Make the key if there is none. Returns whether one was made.
pub fn init_key(p: &Paths) -> Result<bool, String> {
    if read_key(p).is_some() {
        return Ok(false);
    }
    super::crypto::write_private(&p.file("fleet.key"), &super::crypto::random(32))
        .map_err(|e| format!("could not write fleet.key: {e}"))?;
    Ok(true)
}

// ---- the agent's settings

/// Put the machine into (or take it out of) `ollama_fallbacks` and `hosts`. `pinned` is the file
/// TEMPO_AGENT_CONFIG names: then nothing is written and the answer says what to add.
pub fn config_set(
    p: &Paths,
    pinned: Option<PathBuf>,
    m: &Machine,
    present: bool,
    now: u64,
) -> Result<String, String> {
    if !m.engine() && present {
        return Ok(String::new());
    }
    let url = engine_url(&m.engine_url())?;
    let path = pinned
        .clone()
        .unwrap_or_else(|| crate::setup::apply::agent_settings_path(&p.agent_data));
    let ctx = p.ctx();
    let (cfg, old) = propose::load_settings(&path)?;
    let mut list = cfg.strs("ollama_fallbacks");
    list.retain(|u| *u != url);
    let old_hosts = match cfg.get("hosts") {
        Some(J::Obj(h)) => h.clone(),
        _ => Vec::new(),
    };
    let mut hosts = old_hosts.clone();
    hosts.retain(|(n, _)| *n != m.name);
    if present {
        list.push(url.clone());
        hosts.push((
            m.name.clone(),
            J::Obj(vec![(
                "urls".into(),
                J::strings(std::slice::from_ref(&url)),
            )]),
        ));
    }
    if cfg.strs("ollama_fallbacks") == list && old_hosts == hosts {
        return Ok(String::new());
    }
    if pinned.is_some() {
        return Ok(format!(
            "TEMPO_AGENT_CONFIG names {}, which tempo-launch leaves alone. Set it to: \"ollama_fallbacks\": {}, \"hosts\": {}",
            path.display(),
            J::strings(&list).compact(),
            J::Obj(hosts).compact()
        ));
    }
    let values = vec![
        ("ollama_fallbacks".to_string(), J::strings(&list)),
        ("hosts".to_string(), J::Obj(hosts)),
    ];
    let trashed = propose::store(&ctx, &path, cfg, old, &values, now)?;
    Ok(match trashed {
        Some(t) => format!("The old settings were kept at {}.", t.display()),
        None => String::new(),
    })
}

/// A path under the home for the scan's last answer, so `fleet add` knows a name and a role.
pub fn found_file(p: &Path) -> PathBuf {
    p.join("fleet-found.json")
}
