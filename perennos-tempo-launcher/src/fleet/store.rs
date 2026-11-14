//! `<home>/fleet.json`: the machines this person has, who is in the fleet and what each one last
//! said about itself. Changes go through `update`, which takes a lock file, so the CLI and the
//! running messenger never write over each other. The agent's settings are changed here too, with
//! the same private-address check as `tempo-launch link` and a backup of the old file.

use crate::setup::json::{parse, J};
use crate::setup::propose::{self, engine_url};
use crate::{allowed_remote_host, apply_json_atomic, Ctx, FileLock};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// When the owner added this computer; old entries without it stay unknown until first seen.
    pub joined_at: u64,
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
            joined_at: now_s(),
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
            ("joined_at".into(), J::n(self.joined_at as f64)),
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
            joined_at: j.f64("joined_at").unwrap_or(0.0) as u64,
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
    /// The most bytes one file may be, sent or taken, in one blob stream. 0 means the default.
    pub blob_cap: u64,
}

pub const GIB: u64 = 1 << 30;
/// What one transfer may be unless the owner says otherwise (`tempo-launch fleet cap GIB`).
pub const DEFAULT_BLOB_CAP: u64 = 4 * GIB;

impl Fleet {
    pub fn blob_cap(&self) -> u64 {
        if self.blob_cap == 0 {
            DEFAULT_BLOB_CAP
        } else {
            self.blob_cap
        }
    }

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
        let mut o = vec![
            ("version".to_string(), J::n(1.0)),
            ("self".into(), J::s(&self.me)),
            ("allow".into(), J::strings(&self.allow)),
        ];
        if self.blob_cap != 0 {
            o.push((
                "blob_cap_gib".into(),
                J::n(self.blob_cap as f64 / GIB as f64),
            ));
        }
        o.push((
            "machines".into(),
            J::Arr(self.machines.iter().map(Machine::to_j).collect()),
        ));
        J::Obj(o)
    }

    pub fn from_j(j: &J) -> Fleet {
        Fleet {
            me: j.str("self").map(clean_name).unwrap_or_default(),
            allow: j.strs("allow"),
            blob_cap: j
                .f64("blob_cap_gib")
                .filter(|g| *g > 0.0)
                .map_or(0, |g| (g * GIB as f64) as u64),
            machines: j
                .arr("machines")
                .iter()
                .filter_map(Machine::from_j)
                .collect(),
        }
    }
}

pub fn load(p: &Paths) -> Fleet {
    let mut f = match load_checked(p) {
        Ok(fleet) => fleet,
        Err(error) => {
            eprintln!("fleet.json could not be read. {error}");
            Fleet::default()
        }
    };
    if f.me.is_empty() {
        f.me = hostname();
    }
    f
}

/// Read, change and write fleet.json while holding the OS lock. An orphaned lock file is reusable.
pub fn update<T>(p: &Paths, f: impl FnOnce(&mut Fleet) -> T) -> Result<T, String> {
    let _lock = FileLock::acquire(&p.file("fleet.json.lock"), Duration::from_secs(5))
        .map_err(|e| format!("could not lock fleet.json: {e}"))?;
    let mut fleet = load_checked_locked(p)?;
    let path = p.file("fleet.json");
    let previous = read_fleet_bytes(&path)?;
    let out = f(&mut fleet);
    apply_json_atomic(
        &path,
        previous.as_deref(),
        fleet.to_j().pretty().as_bytes(),
        Some(&p.file("fleet.json.last-good")),
        validate_fleet_bytes,
    )
    .map_err(|e| format!("could not write fleet.json: {e}"))?;
    Ok(out)
}

/// Enrollment-only update: validate and edit while holding the existing registry lock.
pub fn update_checked<T>(
    p: &Paths,
    f: impl FnOnce(&mut Fleet) -> Result<T, String>,
) -> Result<T, String> {
    let _lock = FileLock::acquire(&p.file("fleet.json.lock"), Duration::from_secs(5))
        .map_err(|e| format!("could not lock fleet.json: {e}"))?;
    let mut fleet = load_checked_locked(p)?;
    let path = p.file("fleet.json");
    let previous = read_fleet_bytes(&path)?;
    let out = f(&mut fleet)?;
    apply_json_atomic(
        &path,
        previous.as_deref(),
        fleet.to_j().pretty().as_bytes(),
        Some(&p.file("fleet.json.last-good")),
        validate_fleet_bytes,
    )
    .map_err(|e| format!("could not write fleet.json: {e}"))?;
    Ok(out)
}

pub fn load_checked(p: &Paths) -> Result<Fleet, String> {
    if read_fleet_bytes(&p.file("fleet.json"))?.is_none()
        && read_fleet_bytes(&p.file("fleet.json.last-good"))?.is_none()
    {
        return Ok(empty_fleet());
    }
    let _lock = FileLock::acquire(&p.file("fleet.json.lock"), Duration::from_secs(5))
        .map_err(|e| format!("could not lock fleet.json: {e}"))?;
    load_checked_locked(p)
}

fn load_checked_locked(p: &Paths) -> Result<Fleet, String> {
    let path = p.file("fleet.json");
    let backup = p.file("fleet.json.last-good");
    let current = read_fleet_bytes(&path)?;
    if let Some(bytes) = current.as_deref() {
        if let Ok(text) = std::str::from_utf8(bytes) {
            if let Ok(json) = parse_registry_text(text) {
                let fleet = validate_registry(&json).map_err(|error| {
                    format!(
                        "fleet.json contains valid JSON that fails validation; the file was left unchanged and fleet status is unknown. {error}"
                    )
                })?;
                if let Err(error) = ensure_last_good(&backup, bytes) {
                    eprintln!(
                        "The fleet is readable, but its recovery copy could not be refreshed: {error}"
                    );
                }
                return Ok(fleet);
            }
        }
    }
    let last_good = match read_fleet_bytes(&backup) {
        Ok(Some(bytes)) => bytes,
        Ok(None) if current.is_none() => return Ok(empty_fleet()),
        Ok(None) => {
            return Err("fleet.json is unreadable and no working recovery copy is available; the file was left unchanged, so this computer's status is unknown".into())
        }
        Err(error) => {
            return Err(format!("fleet.json could not be read and its recovery copy is unavailable: {error}"))
        }
    };
    let fleet = parse_fleet_bytes(&last_good)
        .map_err(|_| "fleet.json is unreadable and its recovery copy is not valid; both files were left unchanged".to_string())?;
    if let Some(rejected) = current.as_deref() {
        archive_rejected_fleet(&path, rejected)?;
    }
    apply_json_atomic(
        &path,
        current.as_deref(),
        &last_good,
        None,
        validate_fleet_bytes,
    )
    .map_err(|e| format!("fleet.json could not be restored from its last saved copy: {e}"))?;
    eprintln!("fleet.json could not be read, so the last saved copy was restored.");
    Ok(fleet)
}

fn archive_rejected_fleet(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| "fleet.json has no parent folder for its rejected copy".to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("could not timestamp the rejected fleet.json copy: {error}"))?
        .as_millis();
    for collision in 0..100u8 {
        let name = format!("fleet.json.rejected-{stamp:020}-{collision:02}Z");
        let destination = parent.join(&name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&destination) {
            Ok(mut file) => {
                file.write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| {
                        format!("could not preserve rejected fleet.json as {name}: {error}")
                    })?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not preserve rejected fleet.json as {name}: {error}"
                ))
            }
        }
    }
    Err("could not preserve rejected fleet.json because its UTC archive name collided".into())
}

fn empty_fleet() -> Fleet {
    Fleet {
        me: hostname(),
        ..Fleet::default()
    }
}

fn read_fleet_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "could not inspect {}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ))
        }
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 1024 * 1024 {
        return Err(format!(
            "{} is not a bounded regular file; it was kept unchanged",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(format!(
                "{} is a reparse point; it was kept unchanged",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| {
            format!(
                "could not read {}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        })?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            format!(
                "could not read {}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        })?;
    if bytes.len() > 1024 * 1024 {
        return Err(format!(
            "{} exceeded its read limit; it was kept unchanged",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    Ok(Some(bytes))
}

fn parse_fleet_bytes(bytes: &[u8]) -> Result<Fleet, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "fleet.json is not UTF-8; it was kept unchanged".to_string())?;
    let json = parse_registry_text(text)?;
    validate_registry(&json)
}

fn validate_fleet_bytes(bytes: &[u8]) -> std::io::Result<()> {
    parse_fleet_bytes(bytes)
        .map(|_| ())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn ensure_last_good(path: &Path, current: &[u8]) -> Result<(), String> {
    if read_fleet_bytes(path)
        .ok()
        .flatten()
        .is_some_and(|bytes| validate_fleet_bytes(&bytes).is_ok())
    {
        return Ok(());
    }
    let previous = read_fleet_bytes(path)?;
    apply_json_atomic(
        path,
        previous.as_deref(),
        current,
        None,
        validate_fleet_bytes,
    )
    .map_err(|e| format!("could not save the fleet recovery copy: {e}"))
}

/// Reject malformed string escapes that the legacy general-purpose parser accepts lossily.
pub fn parse_registry_text(text: &str) -> Result<J, String> {
    fn valid_number(token: &[u8]) -> bool {
        let mut n = 0;
        if token.get(n) == Some(&b'-') {
            n += 1;
        }
        match token.get(n) {
            Some(b'0') => {
                n += 1;
                if token.get(n).is_some_and(u8::is_ascii_digit) {
                    return false;
                }
            }
            Some(b'1'..=b'9') => {
                n += 1;
                while token.get(n).is_some_and(u8::is_ascii_digit) {
                    n += 1;
                }
            }
            _ => return false,
        }
        if token.get(n) == Some(&b'.') {
            n += 1;
            let start = n;
            while token.get(n).is_some_and(u8::is_ascii_digit) {
                n += 1;
            }
            if start == n {
                return false;
            }
        }
        if matches!(token.get(n), Some(b'e' | b'E')) {
            n += 1;
            if matches!(token.get(n), Some(b'+' | b'-')) {
                n += 1;
            }
            let start = n;
            while token.get(n).is_some_and(u8::is_ascii_digit) {
                n += 1;
            }
            if start == n {
                return false;
            }
        }
        n == token.len()
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut quoted = false;
    while i < bytes.len() {
        let c = bytes[i];
        i += 1;
        if !quoted {
            if c == b'"' {
                quoted = true;
            } else if c == b'-' || c.is_ascii_digit() {
                let start = i - 1;
                while bytes
                    .get(i)
                    .is_some_and(|b| b.is_ascii_digit() || b".+-eE".contains(b))
                {
                    i += 1;
                }
                if !valid_number(&bytes[start..i]) {
                    return Err("fleet.json has an invalid number; it was kept unchanged".into());
                }
            }
            continue;
        }
        if c == b'"' {
            quoted = false;
            continue;
        }
        if c < 0x20 {
            return Err("fleet.json contains an unescaped control; it was kept unchanged".into());
        }
        if c != b'\\' {
            continue;
        }
        let e = *bytes.get(i).ok_or("fleet.json has an incomplete escape")?;
        i += 1;
        if e == b'u' {
            fn hex4(bytes: &[u8]) -> Option<u16> {
                let digits = bytes.get(..4)?;
                if !digits.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                std::str::from_utf8(digits)
                    .ok()
                    .and_then(|s| u16::from_str_radix(s, 16).ok())
            }
            let u = hex4(&bytes[i..]).ok_or("fleet.json has an invalid Unicode escape")?;
            i += 4;
            if (0xd800..=0xdbff).contains(&u) {
                if bytes.get(i..i + 2) != Some(&b"\\u"[..]) {
                    return Err("fleet.json has an unpaired Unicode surrogate".into());
                }
                i += 2;
                let low = hex4(&bytes[i..]).ok_or("fleet.json has an invalid Unicode escape")?;
                i += 4;
                if !(0xdc00..=0xdfff).contains(&low) {
                    return Err("fleet.json has an unpaired Unicode surrogate".into());
                }
            } else if (0xdc00..=0xdfff).contains(&u) {
                return Err("fleet.json has an unpaired Unicode surrogate".into());
            }
        } else if !b"\"\\/bfnrt".contains(&e) {
            return Err("fleet.json has an invalid escape".into());
        }
    }
    parse(text).map_err(|_| "fleet.json is malformed; it was kept unchanged".into())
}

/// Validate a parsed registry before enrollment can rely on it or rewrite it.
pub fn validate_registry(json: &J) -> Result<Fleet, String> {
    fn unique_objects(j: &J) -> bool {
        match j {
            J::Obj(fields) => {
                let mut seen = std::collections::HashSet::new();
                fields
                    .iter()
                    .all(|(key, value)| seen.insert(key) && unique_objects(value))
            }
            J::Arr(items) => items.iter().all(unique_objects),
            _ => true,
        }
    }
    fn fields_ok(j: &J, allowed: &[&str]) -> bool {
        let J::Obj(fields) = j else {
            return false;
        };
        let mut seen = std::collections::HashSet::new();
        fields
            .iter()
            .all(|(k, _)| allowed.contains(&k.as_str()) && seen.insert(k))
    }
    if !unique_objects(json)
        || !fields_ok(
            json,
            &["version", "self", "allow", "blob_cap_gib", "machines"],
        )
        || !matches!(json.get("machines"), Some(J::Arr(_)))
        || !matches!(json.get("self"), Some(J::Str(s)) if !s.is_empty() && clean_name(s) == s.as_str())
    {
        return Err("fleet.json has unfamiliar or invalid fields; it was kept unchanged".into());
    }
    if json
        .get("allow")
        .is_some_and(|v| !matches!(v, J::Arr(a) if a.iter().all(|x| matches!(x,J::Str(_)))))
    {
        return Err("fleet.json has an invalid allow list; it was kept unchanged".into());
    }
    if json
        .get("version")
        .is_some_and(|_| json.f64("version") != Some(1.0))
        || json.get("blob_cap_gib").is_some_and(|_| {
            json.f64("blob_cap_gib")
                .is_none_or(|n| !n.is_finite() || n < 0.0 || n > (u64::MAX / GIB) as f64)
        })
    {
        return Err("fleet.json has invalid version or capacity; it was kept unchanged".into());
    }
    let allowed = [
        "name",
        "addr",
        "port",
        "role",
        "enabled",
        "last_seen",
        "joined_at",
        "id",
        "probe",
        "health",
        "ssh",
        "tools",
    ];
    let mut names = std::collections::HashSet::new();
    for item in json.arr("machines") {
        let Some(member) = Machine::from_j(item) else {
            return Err("fleet.json has an invalid member; it was kept unchanged".into());
        };
        if !fields_ok(item, &allowed)
            || clean_name(&member.name) != member.name
            || (!member.id.is_empty()
                && (member.id.len() != 64 || !member.id.bytes().all(|b| b.is_ascii_hexdigit())))
            || !names.insert(member.name.to_ascii_lowercase())
            || item
                .get("enabled")
                .is_some_and(|v| !matches!(v, J::Bool(_)))
            || ["role", "id"]
                .iter()
                .any(|k| item.get(k).is_some_and(|v| !matches!(v, J::Str(_))))
            || item.get("port").is_some_and(|_| {
                item.f64("port")
                    .is_none_or(|n| !(1.0..=65535.0).contains(&n) || n.fract() != 0.0)
            })
            || ["last_seen", "joined_at"].iter().any(|key| {
                item.get(key).is_some_and(|_| {
                    item.f64(key).is_none_or(|n| {
                        !(0.0..=9007199254740991.0).contains(&n) || n.fract() != 0.0
                    })
                })
            })
        {
            return Err(
                "fleet.json has an invalid or unfamiliar member; it was kept unchanged".into(),
            );
        }
    }
    Ok(Fleet::from_j(json))
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
    config_set_inner(p, pinned, m, present, None, now)
}

/// Drop the old engine fallback only when no other active peer still owns it.
pub fn config_set_after_address_change(
    p: &Paths,
    pinned: Option<PathBuf>,
    m: &Machine,
    previous: &Machine,
    now: u64,
) -> Result<String, String> {
    config_set_inner(p, pinned, m, true, Some(previous), now)
}

fn config_set_inner(
    p: &Paths,
    pinned: Option<PathBuf>,
    m: &Machine,
    present: bool,
    previous: Option<&Machine>,
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
    let previous_url = previous
        .filter(|old| old.engine() && old.addr != m.addr)
        .map(|old| engine_url(&old.engine_url()))
        .transpose()?;
    let previous_shared = previous
        .filter(|old| old.engine() && old.addr != m.addr)
        .is_some_and(|old| {
            load(p).machines.iter().any(|peer| {
                peer.name != old.name
                    && peer.enabled
                    && peer.engine()
                    && peer.engine_url() == old.engine_url()
            })
        });
    if let Some(previous_url) = previous_url.as_ref() {
        if !previous_shared {
            list.retain(|u| u != previous_url);
        }
    }
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
