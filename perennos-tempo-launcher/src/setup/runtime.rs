//! Tempo's own copy of Ollama. When a computer has no Ollama, `setup --apply` can fetch one pinned
//! Ollama release (`catalog/runtime.json`), check its SHA-256 before anything else touches the
//! file, unpack it with the system's own `tar` and write down where it is. Only after the person
//! says yes. Nothing is installed system-wide, nothing is deleted (a bad file goes to the trash),
//! and an existing folder is never overwritten. Starting it is `lib.rs` (`ensure_owned_ollama`).
//! See docs/TEMPO-SETUP.md.

use super::json::{parse, J};
use super::probe::{Host, Machine, GIB};
use super::registry::Fetch;
use super::sha256::file_sha256;
use crate::{file_stamp, stamp, write_atomic};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const EMBEDDED: &str = include_str!("../../catalog/runtime.json");
pub const PRODUCT: &str = "Perennos Tempo";
pub const PURPOSE: &str = "owned-ollama-runtime";
/// Unpacking needs room next to the download, so the disk must hold this many times its size.
pub const DISK_FACTOR: f64 = 2.5;
const UNPACK_LIMIT: Duration = Duration::from_secs(30 * 60);

pub const ZSTD_HELP: &str = "install zstd (for example sudo apt install zstd), or install Ollama from ollama.com, then run setup again";

// ---- the pins

/// One file of the pinned release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    /// `windows-x86_64`, `macos`, `linux-aarch64`...
    pub key: String,
    pub file: String,
    /// Lower-case hex.
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pins {
    pub version: String,
    pub base_url: String,
    pub files: Vec<Pin>,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn parse_pins(text: &str) -> Result<Pins, String> {
    let j = parse(text)?;
    let version = j
        .str("version")
        .filter(|v| {
            !v.is_empty()
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.' || b.is_ascii_alphabetic())
        })
        .ok_or("the runtime list has no usable version")?
        .to_string();
    let base_url = j
        .str("base_url")
        .filter(|u| u.starts_with("https://") && u.ends_with('/'))
        .ok_or("the runtime list needs an https base_url ending in /")?
        .to_string();
    let J::Obj(files) = j.get("files").ok_or("the runtime list has no files")? else {
        return Err("files must be an object".into());
    };
    let mut out = Vec::new();
    for (key, f) in files {
        let file = f
            .str("file")
            .filter(|n| !n.is_empty() && !n.contains(['/', '\\', ':']) && !n.starts_with('.'))
            .ok_or_else(|| format!("{key}: no usable file name"))?;
        let sha = f
            .str("sha256")
            .filter(|s| is_hex64(s))
            .ok_or_else(|| format!("{key}: sha256 must be 64 lower-case hex characters"))?;
        let bytes = f
            .f64("bytes")
            .filter(|b| *b >= 1.0 && b.fract() == 0.0)
            .ok_or_else(|| format!("{key}: no byte count"))? as u64;
        out.push(Pin {
            key: key.clone(),
            file: file.to_string(),
            sha256: sha.to_string(),
            bytes,
        });
    }
    Ok(Pins {
        version,
        base_url,
        files: out,
    })
}

impl Pins {
    /// The list that ships inside the program. A test keeps it valid; if it ever were not, no
    /// computer would be offered a download.
    pub fn embedded() -> Pins {
        parse_pins(EMBEDDED).unwrap_or(Pins {
            version: String::new(),
            base_url: String::new(),
            files: Vec::new(),
        })
    }

    pub fn pin_for(&self, os: &str, arch: &str) -> Option<&Pin> {
        let key = key_for(os, arch)?;
        self.files.iter().find(|p| p.key == key)
    }

    pub fn url(&self, p: &Pin) -> String {
        format!("{}{}", self.base_url, p.file)
    }

    /// `<home>/runtime/ollama-<version>`
    pub fn folder(&self, home: &Path) -> PathBuf {
        home.join("runtime")
            .join(format!("ollama-{}", self.version))
    }
}

/// The catalog key for a computer; None where Ollama publishes nothing Tempo can use.
pub fn key_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("windows-x86_64"),
        ("windows", "aarch64") => Some("windows-aarch64"),
        ("macos", "x86_64" | "aarch64") => Some("macos"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        _ => None,
    }
}

// ---- the plan

/// Tempo's own copy, as recorded in `<home>/runtime/ollama.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owned {
    pub version: String,
    pub sha256: String,
    pub file: String,
    pub binary: PathBuf,
    pub installed_at: String,
}

/// What setup would fetch, and whether it can.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub pin: Pin,
    pub version: String,
    pub url: String,
    pub folder: PathBuf,
    pub port: u16,
    pub need_gb: f64,
    /// The models setup fetches in the same run (they share the free disk with Ollama).
    pub models_gb: f64,
    pub disk_free_gb: Option<f64>,
    /// None: the free disk is unknown.
    pub enough_disk: Option<bool>,
    /// Something that stops the install, in plain words (zstd missing, a folder in the way).
    pub blocker: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimePlan {
    /// Tempo's own copy is already here (it may or may not be running).
    Installed(Owned),
    /// No Ollama: Tempo can fetch its own.
    Missing(Offer),
    /// No Ollama, and nothing Tempo can fetch for this computer.
    Unsupported(String),
}

/// Whether the computer needs Tempo's own Ollama. None when an Ollama answers or the `ollama`
/// program is installed: that one is used, and Tempo's own is never offered.
pub fn plan_runtime(
    m: &Machine,
    host: &dyn Host,
    home: &Path,
    pins: &Pins,
    port: u16,
) -> Option<RuntimePlan> {
    if m.ollama.running || m.ollama.binary.is_some() {
        return None;
    }
    if let Some(o) = read_record(home) {
        return Some(RuntimePlan::Installed(o));
    }
    let Some(pin) = pins.pin_for(&m.os, &m.arch) else {
        return Some(RuntimePlan::Unsupported(format!(
            "Ollama was not found, and Tempo has no Ollama to fetch for {} on {}. Install Ollama from ollama.com, then run setup again.",
            m.os, m.arch
        )));
    };
    let need_gb = pin.bytes as f64 * DISK_FACTOR / GIB;
    let folder = pins.folder(home);
    let blocker = tools_blocker(host, &m.os).or_else(|| {
        folder.exists().then(|| {
            format!(
                "{} already exists and Tempo never overwrites a folder. Move it aside (to a trash folder), then run setup again.",
                folder.display()
            )
        })
    });
    Some(RuntimePlan::Missing(Offer {
        pin: pin.clone(),
        version: pins.version.clone(),
        url: pins.url(pin),
        folder,
        port,
        need_gb,
        models_gb: 0.0,
        disk_free_gb: m.disk_free_gb,
        enough_disk: m.disk_free_gb.map(|f| f >= need_gb),
        blocker,
    }))
}

/// Ollama and the models come out of the same free disk, so they are checked together: the
/// offer counts the models (and the margin the model plan keeps free), and the model plan
/// counts Ollama. Without this, each could say "enough" when both together do not fit.
pub fn share_disk(plan: &mut super::plan::Plan) {
    let models = plan.fetch_gb;
    let has_models = !plan.fetch.is_empty();
    let Some(RuntimePlan::Missing(o)) = &mut plan.runtime else {
        return;
    };
    o.models_gb = models;
    let Some(free) = o.disk_free_gb else {
        return;
    };
    let margin = if has_models {
        super::plan::DISK_MARGIN_GB
    } else {
        0.0
    };
    let total = o.need_gb + models + margin;
    o.enough_disk = Some(free >= total);
    if has_models && plan.refused.is_none() && free < total {
        plan.refused = Some(format!(
            "Not enough free disk for Ollama and the models together: about {:.1} GB for Tempo's own copy of Ollama, {models:.1} GB of models and {margin:.0} GB to keep free, but only {free:.1} GB is free. Free some space, then run setup again.",
            o.need_gb
        ));
    }
}

/// Linux unpacks `.tar.zst`, which needs the `zstd` program.
fn tools_blocker(host: &dyn Host, os: &str) -> Option<String> {
    (os == "linux" && host.run("zstd", &["--version"]).is_none()).then(|| {
        format!("Tempo needs the zstd program to unpack Ollama on Linux, and it is not installed: {ZSTD_HELP}.")
    })
}

fn gb_text(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / GIB)
}

impl RuntimePlan {
    /// The facts in plain English, one per line. The terminal plan and the plan page both show
    /// exactly these.
    pub fn lines(&self) -> Vec<String> {
        match self {
            RuntimePlan::Installed(o) => vec![format!(
                "Ollama was not found, but Tempo's own copy ({}) is installed at {}. Tempo starts it for you when it is not running.",
                o.version,
                o.binary.display()
            )],
            RuntimePlan::Unsupported(why) => vec![why.clone()],
            RuntimePlan::Missing(o) => {
                let mut v = vec![
                    format!(
                        "No Ollama answers at http://127.0.0.1:{} and no ollama program was found on this computer.",
                        o.port
                    ),
                    format!(
                        "Tempo can fetch its own copy: {} (Ollama {}, {}, {} bytes).",
                        o.pin.file,
                        o.version,
                        gb_text(o.pin.bytes),
                        o.pin.bytes
                    ),
                    format!("From: {}", o.url),
                    format!(
                        "It must match SHA-256 {} before anything is unpacked; a file that does not match is refused and moved to the trash.",
                        o.pin.sha256
                    ),
                    {
                        let with_models = if o.models_gb > 0.0 {
                            format!(
                                ", plus {:.1} GB for the models and {:.0} GB kept free",
                                o.models_gb,
                                super::plan::DISK_MARGIN_GB
                            )
                        } else {
                            String::new()
                        };
                        match (o.disk_free_gb, o.enough_disk) {
                            (Some(f), Some(true)) => format!(
                                "Free disk: {f:.1} GB. It needs about {:.1} GB ({DISK_FACTOR} times the download, because unpacking needs room){with_models}: enough.",
                                o.need_gb
                            ),
                            (Some(f), _) => format!(
                                "Free disk: {f:.1} GB. It needs about {:.1} GB ({DISK_FACTOR} times the download, because unpacking needs room){with_models}: NOT enough.",
                                o.need_gb
                            ),
                            (None, _) => format!(
                                "Free disk: unknown. It needs about {:.1} GB ({DISK_FACTOR} times the download, because unpacking needs room){with_models}.",
                                o.need_gb
                            ),
                        }
                    },
                    format!(
                        "It would be unpacked into {} and started only for Tempo (OLLAMA_HOST=127.0.0.1:{}). Nothing is fetched unless you run setup --apply and say yes.",
                        o.folder.display(),
                        o.port
                    ),
                ];
                if let Some(b) = &o.blocker {
                    v.push(format!("Not possible yet: {b}"));
                }
                v
            }
        }
    }

    pub fn json(&self) -> J {
        let opt = |v: Option<f64>| v.map_or(J::Null, |g| J::n((g * 100.0).round() / 100.0));
        match self {
            RuntimePlan::Installed(o) => J::Obj(vec![
                ("state".into(), J::s("installed")),
                ("version".into(), J::s(&o.version)),
                ("binary".into(), J::s(&o.binary.to_string_lossy())),
            ]),
            RuntimePlan::Unsupported(why) => J::Obj(vec![
                ("state".into(), J::s("unsupported")),
                ("message".into(), J::s(why)),
            ]),
            RuntimePlan::Missing(o) => J::Obj(vec![
                ("state".into(), J::s("missing")),
                ("version".into(), J::s(&o.version)),
                ("file".into(), J::s(&o.pin.file)),
                ("url".into(), J::s(&o.url)),
                ("sha256".into(), J::s(&o.pin.sha256)),
                ("bytes".into(), J::n(o.pin.bytes as f64)),
                (
                    "download_gb".into(),
                    J::n((o.pin.bytes as f64 / GIB * 100.0).round() / 100.0),
                ),
                ("needs_gb".into(), J::n((o.need_gb * 10.0).round() / 10.0)),
                ("disk_free_gb".into(), opt(o.disk_free_gb)),
                ("enough_disk".into(), o.enough_disk.map_or(J::Null, J::Bool)),
                ("folder".into(), J::s(&o.folder.to_string_lossy())),
                ("port".into(), J::n(f64::from(o.port))),
                ("blocked".into(), o.blocker.as_deref().map_or(J::Null, J::s)),
            ]),
        }
    }
}

// ---- the record

pub fn record_path(home: &Path) -> PathBuf {
    home.join("runtime").join("ollama.json")
}

/// Tempo's own Ollama, if `<home>/runtime/ollama.json` says so and the program is really there.
/// The program must lie inside `<home>/runtime`: a record never makes Tempo run something elsewhere.
pub fn read_record(home: &Path) -> Option<Owned> {
    let j = parse(&std::fs::read_to_string(record_path(home)).ok()?).ok()?;
    if j.str("purpose") != Some(PURPOSE) {
        return None;
    }
    let binary = PathBuf::from(j.str("binary")?);
    let inside = binary.starts_with(home.join("runtime"))
        && !binary
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    (inside && binary.is_file()).then(|| Owned {
        version: j.str("version").unwrap_or("").to_string(),
        sha256: j.str("sha256").unwrap_or("").to_string(),
        file: j.str("file").unwrap_or("").to_string(),
        binary,
        installed_at: j.str("installed_at").unwrap_or("").to_string(),
    })
}

fn write_record(home: &Path, o: &Owned) -> Result<(), String> {
    let j = J::Obj(vec![
        ("product".into(), J::s(PRODUCT)),
        ("purpose".into(), J::s(PURPOSE)),
        ("version".into(), J::s(&o.version)),
        ("sha256".into(), J::s(&o.sha256)),
        ("file".into(), J::s(&o.file)),
        ("binary".into(), J::s(&o.binary.to_string_lossy())),
        ("installed_at".into(), J::s(&o.installed_at)),
    ]);
    write_atomic(&record_path(home), j.pretty().as_bytes())
        .map_err(|e| format!("could not write {}: {e}", record_path(home).display()))
}

// ---- unpacking

/// The program and arguments that unpack `file` into `dest`: the system's own tar on each OS.
pub fn tar_command(
    os: &str,
    system_root: Option<&str>,
    file: &Path,
    dest: &Path,
) -> Result<(String, Vec<String>), String> {
    let (prog, flags): (String, &[&str]) = match os {
        "windows" => {
            let root = system_root.unwrap_or(r"C:\Windows").trim_end_matches('\\');
            (format!(r"{root}\System32\tar.exe"), &["-xf"])
        }
        "macos" => ("/usr/bin/tar".into(), &["-xzf"]),
        "linux" => ("tar".into(), &["--zstd", "-xf"]),
        other => return Err(format!("Tempo cannot unpack Ollama on {other}")),
    };
    let mut args: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
    args.push(file.to_string_lossy().into_owned());
    args.push("-C".into());
    args.push(dest.to_string_lossy().into_owned());
    Ok((prog, args))
}

/// The `ollama` (or `ollama.exe`) program inside an unpacked folder: the nearest to the top, so
/// `lib/ollama/` (a folder) or a runner deeper down is never taken for it. Links are not followed.
pub fn find_binary(dir: &Path) -> Option<PathBuf> {
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..5 {
        let mut next = Vec::new();
        for d in &level {
            let mut entries: Vec<_> = std::fs::read_dir(d).ok()?.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().to_lowercase();
                if ft.is_file() && (name == "ollama" || name == "ollama.exe") {
                    return Some(e.path());
                }
                if ft.is_dir() {
                    next.push(e.path());
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    None
}

/// Move something into `<home>/trash/<date>/`, under a name that is not taken. Never deletes.
pub fn to_trash(home: &Path, path: &Path, now: u64) -> Result<PathBuf, String> {
    let fs_ = file_stamp(now);
    let dir = home.join("trash").join(&fs_[..8]);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make the trash folder: {e}"))?;
    let name = path
        .file_name()
        .map_or("file".into(), |n| n.to_string_lossy().into_owned());
    let mut dest = dir.join(&name);
    let mut n = 0;
    while dest.exists() {
        n += 1;
        let tag = if n == 1 {
            fs_[9..].to_string()
        } else {
            format!("{}-{n}", &fs_[9..])
        };
        dest = dir.join(format!("{name}-{tag}"));
    }
    std::fs::rename(path, &dest)
        .map_err(|e| format!("could not move {} to the trash: {e}", path.display()))?;
    Ok(dest)
}

// ---- the install

fn in_the_way(folder: &Path) -> String {
    format!(
        "{} already exists, and it is not Tempo's own copy of Ollama (there is no record for it). Tempo never overwrites a folder. Move it aside (to a trash folder), then run setup again.",
        folder.display()
    )
}

/// The longest a download may take: 200 KB/s at the slowest, and never under 15 minutes. (A link
/// stuck below 1 KB/s for a minute is dropped earlier, and a second run carries on from the part.)
pub fn download_limit_secs(bytes: u64) -> u64 {
    (bytes / 200_000).max(900)
}

/// Fetch (or find), check, unpack and record Tempo's own Ollama. `say` gets progress lines.
/// On any failure nothing half-done is left under the final name, and the reason is one plain
/// sentence for the person.
pub fn install(
    home: &Path,
    host: &dyn Host,
    fetch: &dyn Fetch,
    pins: &Pins,
    pin: &Pin,
    now: u64,
    say: &mut dyn FnMut(&str),
) -> Result<Owned, String> {
    let folder = pins.folder(home);
    if folder.exists() {
        return match read_record(home).filter(|o| o.binary.starts_with(&folder)) {
            Some(o) => {
                say("Tempo's own copy of Ollama is already installed; it is left as it is.");
                Ok(o)
            }
            None => Err(in_the_way(&folder)),
        };
    }
    // before a 1.4 GB download, not after it
    if let Some(b) = tools_blocker(host, &host.os()) {
        return Err(b);
    }
    let file = fetch_checked(home, fetch, pins, pin, now, say)?;
    unpack(home, host, pins, pin, &file, now, say)
}

/// `<home>/runtime/downloads/<file>`, once its SHA-256 matches the pin.
fn fetch_checked(
    home: &Path,
    fetch: &dyn Fetch,
    pins: &Pins,
    pin: &Pin,
    now: u64,
    say: &mut dyn FnMut(&str),
) -> Result<PathBuf, String> {
    let dir = home.join("runtime").join("downloads");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let done = dir.join(&pin.file);
    let part = dir.join(format!("{}.part", pin.file));
    let sum = |p: &Path, say: &mut dyn FnMut(&str)| -> Result<String, String> {
        say(&format!("Checking SHA-256 of {} ...", pin.file));
        file_sha256(p, &mut |_| {}).map_err(|e| format!("could not read {}: {e}", p.display()))
    };
    // a file fetched and checked by an earlier run
    if done.is_file() {
        if sum(&done, say)? == pin.sha256 {
            say(&format!("{} was already fetched and checked.", pin.file));
            return Ok(done);
        }
        let t = to_trash(home, &done, now)?;
        say(&format!(
            "An older {} did not match its SHA-256, so it was moved to {}.",
            pin.file,
            t.display()
        ));
    }
    let have = std::fs::metadata(&part).map_or(0, |m| m.len());
    if have > pin.bytes {
        let t = to_trash(home, &part, now)?;
        say(&format!(
            "A leftover {}.part was bigger than the file should be, so it was moved to {}.",
            pin.file,
            t.display()
        ));
    }
    let have = std::fs::metadata(&part).map_or(0, |m| m.len());
    if have != pin.bytes {
        say(&format!(
            "Fetching {} ({}) from {}{}",
            pin.file,
            gb_text(pin.bytes),
            pins.url(pin),
            if have > 0 {
                format!(", carrying on from {}", gb_text(have))
            } else {
                String::new()
            }
        ));
        let mut decile = have * 10 / pin.bytes;
        fetch
            .download(
                &pins.url(pin),
                &part,
                download_limit_secs(pin.bytes),
                &mut |n| {
                    let d = n.min(pin.bytes) * 10 / pin.bytes;
                    if d > decile {
                        decile = d;
                        say(&format!("  {}% ({})", d * 10, gb_text(n)));
                    }
                },
            )
            .map_err(|e| {
                format!(
                    "Could not fetch {}: {e}. What arrived is kept, and running setup again carries on from there.",
                    pin.file
                )
            })?;
    }
    let got = sum(&part, say)?;
    if got != pin.sha256 {
        let t = to_trash(home, &part, now)?;
        return Err(format!(
            "The file fetched for {} did not match its pinned SHA-256 (wanted {}, got {got}), so Tempo refused it. It was moved to {} and nothing was unpacked or installed.",
            pin.file,
            pin.sha256,
            t.display()
        ));
    }
    std::fs::rename(&part, &done)
        .map_err(|e| format!("could not rename {}: {e}", part.display()))?;
    say("SHA-256 matches the pin.");
    Ok(done)
}

fn unpack(
    home: &Path,
    host: &dyn Host,
    pins: &Pins,
    pin: &Pin,
    file: &Path,
    now: u64,
    say: &mut dyn FnMut(&str),
) -> Result<Owned, String> {
    let folder = pins.folder(home);
    let partial = folder.with_file_name(format!("ollama-{}.partial", pins.version));
    if partial.exists() {
        let t = to_trash(home, &partial, now)?;
        say(&format!(
            "A leftover unpack folder was moved to {}.",
            t.display()
        ));
    }
    std::fs::create_dir_all(&partial)
        .map_err(|e| format!("could not make {}: {e}", partial.display()))?;
    let (prog, args) = tar_command(
        &host.os(),
        host.env("SystemRoot").as_deref(),
        file,
        &partial,
    )?;
    say(&format!("Unpacking into {} ...", folder.display()));
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    host.run_tool(&prog, &argv, UNPACK_LIMIT).map_err(|e| {
        format!(
            "Unpacking {} failed: {e}. The half-unpacked folder {} is left, and the next run moves it to the trash first.",
            pin.file,
            partial.display()
        )
    })?;
    let found = find_binary(&partial).ok_or_else(|| {
        format!(
            "{} unpacked, but no ollama program was inside it. It is left in {} and nothing was installed.",
            pin.file,
            partial.display()
        )
    })?;
    let rel = found
        .strip_prefix(&partial)
        .map_err(|_| "the ollama program is outside its folder".to_string())?
        .to_path_buf();
    make_executable(&found);
    // rename onto an existing empty folder would succeed on some systems: never take that chance
    if folder.exists() {
        return Err(in_the_way(&folder));
    }
    std::fs::rename(&partial, &folder)
        .map_err(|e| format!("could not move the unpacked folder into place: {e}"))?;
    let owned = Owned {
        version: pins.version.clone(),
        sha256: pin.sha256.clone(),
        file: pin.file.clone(),
        binary: folder.join(rel),
        installed_at: stamp(now),
    };
    write_record(home, &owned)?;
    say(&format!("Installed {}.", owned.binary.display()));
    Ok(owned)
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::metadata(p) {
        let mode = m.permissions().mode();
        if mode & 0o111 == 0 {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode | 0o755));
        }
    }
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

#[cfg(test)]
pub mod tests {
    use super::super::probe::tests::Fake;
    use super::super::registry::Fetched;
    use super::super::sha256::sha256_hex;
    use super::*;
    use std::sync::Mutex;

    pub fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-runtime-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The bytes a pretend release holds, and pins that match them.
    pub const CONTENT: &[u8] =
        b"pretend ollama archive: not a real archive, the fake tar does not read it";

    pub fn test_pins() -> Pins {
        let sha = sha256_hex(CONTENT);
        let pin = |key: &str, file: &str| Pin {
            key: key.into(),
            file: file.into(),
            sha256: sha.clone(),
            bytes: CONTENT.len() as u64,
        };
        Pins {
            version: "0.40.6".into(),
            base_url: "https://example.test/ollama/v0.40.6/".into(),
            files: vec![
                pin("windows-x86_64", "ollama-windows-amd64.zip"),
                pin("macos", "ollama-darwin.tgz"),
                pin("linux-x86_64", "ollama-linux-amd64.tar.zst"),
            ],
        }
    }

    /// A fetcher that "downloads" CONTENT (or something else) with no network, resuming a part.
    pub struct FakeDl {
        pub body: Vec<u8>,
        pub fail: Option<&'static str>,
        pub calls: Mutex<Vec<(String, PathBuf, u64)>>,
    }

    impl Default for FakeDl {
        fn default() -> Self {
            FakeDl::new()
        }
    }

    impl FakeDl {
        pub fn new() -> FakeDl {
            FakeDl {
                body: CONTENT.to_vec(),
                fail: None,
                calls: Mutex::new(Vec::new()),
            }
        }
        pub fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Fetch for FakeDl {
        fn get(&self, _: &str, _: Option<&str>, _: bool) -> Fetched {
            Fetched::Failed
        }
        fn download(
            &self,
            url: &str,
            part: &Path,
            max_secs: u64,
            progress: &mut dyn FnMut(u64),
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((url.into(), part.into(), max_secs));
            if let Some(f) = self.fail {
                // half of it arrives, then the link drops
                std::fs::write(part, &self.body[..self.body.len() / 2]).unwrap();
                return Err(f.into());
            }
            let have = std::fs::metadata(part).map_or(0, |m| m.len()) as usize;
            let mut data = std::fs::read(part).unwrap_or_default();
            data.extend_from_slice(&self.body[have.min(self.body.len())..]);
            std::fs::write(part, &data).unwrap();
            progress(data.len() as u64 / 2);
            progress(data.len() as u64);
            Ok(())
        }
    }

    fn linux() -> Fake {
        let mut f = Fake {
            os: "linux",
            arch: "x86_64",
            tar_makes: Some("bin/ollama"),
            ..Fake::default()
        };
        f.cmds
            .insert("zstd --version".into(), "zstd 1.5.5\n".into());
        f
    }

    fn machine(os: &str, arch: &str, disk: Option<f64>) -> Machine {
        Machine {
            preferred_prefix: String::new(),
            os: os.into(),
            arch: arch.into(),
            cpus: 4,
            ram_total_gb: Some(16.0),
            ram_avail_gb: Some(8.0),
            gpu: super::super::probe::Gpu::None,
            disk_free_gb: disk,
            ollama: Default::default(),
            local: vec![],
        }
    }

    fn run_install(
        home: &Path,
        host: &Fake,
        dl: &FakeDl,
        pins: &Pins,
    ) -> (Result<Owned, String>, Vec<String>) {
        let pin = pins.pin_for(host.os, host.arch).unwrap().clone();
        let mut said = Vec::new();
        let r = install(home, host, dl, pins, &pin, 1_793_000_000, &mut |s| {
            said.push(s.to_string())
        });
        (r, said)
    }

    // ---- the pins

    #[test]
    fn the_shipped_pins_are_exactly_the_upstream_ones() {
        let p = parse_pins(EMBEDDED).unwrap();
        assert_eq!(p.version, "0.40.6");
        assert_eq!(
            p.base_url,
            "https://github.com/ollama/ollama/releases/download/v0.40.6/"
        );
        let want = [
            (
                "windows-x86_64",
                "ollama-windows-amd64.zip",
                "e359637575302f288d76c0007c408a4be9c7376d59c4f28684d63b232a44b427",
                1471164488u64,
            ),
            (
                "windows-aarch64",
                "ollama-windows-arm64.zip",
                "82cace1a134f88e7e7092dcf79304eff5950b88dbfabc03f19be09f8be20ee65",
                210482819,
            ),
            (
                "macos",
                "ollama-darwin.tgz",
                "b2b55434865a668decb494110d7fd970eed3ecc8837be2872f672ccbb95df974",
                174459907,
            ),
            (
                "linux-x86_64",
                "ollama-linux-amd64.tar.zst",
                "c164ab8b1b28e2a247c29719b8e404c423c5b83ae64c3ab4e59d50c7ff7fc5e0",
                1438364593,
            ),
            (
                "linux-aarch64",
                "ollama-linux-arm64.tar.zst",
                "0a279d9428ad17b06fdb7dddf3b059b2c148433585fa3b30676a102fc459bb9f",
                1560414226,
            ),
        ];
        assert_eq!(p.files.len(), want.len());
        for (key, file, sha, bytes) in want {
            let f = p.files.iter().find(|f| f.key == key).unwrap();
            assert_eq!(
                (f.file.as_str(), f.sha256.as_str(), f.bytes),
                (file, sha, bytes)
            );
        }
        assert_eq!(Pins::embedded(), p);
        assert_eq!(
            p.url(p.pin_for("macos", "aarch64").unwrap()),
            "https://github.com/ollama/ollama/releases/download/v0.40.6/ollama-darwin.tgz"
        );
    }

    #[test]
    fn a_bad_pin_list_is_refused() {
        let good = |sha: &str, file: &str| {
            format!(
                r#"{{"version":"1.0.0","base_url":"https://x.test/","files":{{"macos":{{"file":"{file}","sha256":"{sha}","bytes":5}}}}}}"#
            )
        };
        let sha = "a".repeat(64);
        assert!(parse_pins(&good(&sha, "a.tgz")).is_ok());
        assert!(parse_pins(&good(&sha[..63], "a.tgz")).is_err());
        assert!(parse_pins(&good(&sha.to_uppercase(), "a.tgz")).is_err());
        assert!(parse_pins(&good(&sha, "../a.tgz")).is_err());
        assert!(parse_pins(&good(&sha, "x/a.tgz")).is_err());
        assert!(parse_pins(&good(&sha, "a.tgz").replace("https://", "http://")).is_err());
        assert!(parse_pins("{").is_err());
    }

    #[test]
    fn every_supported_computer_has_a_pin_and_others_have_none() {
        let p = Pins::embedded();
        for (os, arch) in [
            ("windows", "x86_64"),
            ("windows", "aarch64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
        ] {
            assert!(p.pin_for(os, arch).is_some(), "{os} {arch}");
        }
        for (os, arch) in [
            ("freebsd", "x86_64"),
            ("linux", "riscv64"),
            ("linux", "x86"),
            ("windows", "x86"),
            ("macos", "powerpc"),
        ] {
            assert!(p.pin_for(os, arch).is_none(), "{os} {arch}");
        }
    }

    // ---- the plan

    #[test]
    fn no_plan_when_an_ollama_answers_or_is_installed() {
        let home = tmp("plan-none");
        let host = linux();
        let pins = Pins::embedded();
        let mut m = machine("linux", "x86_64", Some(100.0));
        m.ollama.running = true;
        assert_eq!(plan_runtime(&m, &host, &home, &pins, 11434), None);
        m.ollama.running = false;
        m.ollama.binary = Some(PathBuf::from("/usr/bin/ollama"));
        assert_eq!(plan_runtime(&m, &host, &home, &pins, 11434), None);
    }

    #[test]
    fn the_plan_shows_the_exact_file_version_size_and_free_disk() {
        let home = tmp("plan-missing");
        let host = linux();
        let pins = Pins::embedded();
        let m = machine("linux", "x86_64", Some(100.0));
        let Some(RuntimePlan::Missing(o)) = plan_runtime(&m, &host, &home, &pins, 11434) else {
            panic!("expected an offer")
        };
        assert_eq!(o.pin.file, "ollama-linux-amd64.tar.zst");
        assert_eq!(o.version, "0.40.6");
        assert_eq!(o.pin.bytes, 1438364593);
        assert_eq!(o.enough_disk, Some(true));
        assert_eq!(o.blocker, None);
        assert_eq!(o.folder, home.join("runtime").join("ollama-0.40.6"));
        let text = RuntimePlan::Missing(o).lines().join("\n");
        for must in [
            "No Ollama answers at http://127.0.0.1:11434 and no ollama program was found",
            "ollama-linux-amd64.tar.zst (Ollama 0.40.6, 1.34 GB, 1438364593 bytes)",
            "https://github.com/ollama/ollama/releases/download/v0.40.6/ollama-linux-amd64.tar.zst",
            "c164ab8b1b28e2a247c29719b8e404c423c5b83ae64c3ab4e59d50c7ff7fc5e0",
            "Free disk: 100.0 GB. It needs about 3.3 GB (2.5 times the download",
            "enough",
            "Nothing is fetched unless you run setup --apply and say yes",
        ] {
            assert!(text.contains(must), "{must}\n{text}");
        }
    }

    #[test]
    fn ollama_and_the_models_are_checked_against_the_same_free_disk() {
        let home = tmp("plan-share");
        let host = linux();
        let pins = Pins::embedded();
        let need = 1438364593f64 * 2.5 / GIB;
        let plan_with = |free: f64, models: f64| {
            let m = machine("linux", "x86_64", Some(free));
            let mut p = super::super::plan::Plan {
                goals: vec![],
                summary: vec![],
                picks: vec![],
                fetch: vec![],
                fetch_gb: models,
                unknown_sizes: vec![],
                disk_free_gb: Some(free),
                refused: None,
                notes: vec![],
                runtime: plan_runtime(&m, &host, &home, &pins, 11434),
            };
            if models > 0.0 {
                p.fetch.push(super::super::plan::Fetch {
                    name: "qwen3:4b".into(),
                    catalog_name: "qwen3:4b".into(),
                    gb: Some(models),
                    licence: "Apache-2.0".into(),
                    proof: String::new(),
                    link: None,
                    opt_in: false,
                });
            }
            share_disk(&mut p);
            p
        };
        let offer = |p: &super::super::plan::Plan| match &p.runtime {
            Some(RuntimePlan::Missing(o)) => o.clone(),
            other => panic!("{other:?}"),
        };
        // each fits alone, but not both with the margin: neither says "enough"
        let p = plan_with(need + 2.0, 2.5);
        assert_eq!(offer(&p).enough_disk, Some(false));
        assert_eq!(offer(&p).models_gb, 2.5);
        let why = p.refused.as_deref().unwrap();
        assert!(why.contains("Ollama and the models together"), "{why}");
        let text = RuntimePlan::Missing(offer(&p)).lines().join("\n");
        assert!(
            text.contains("plus 2.5 GB for the models and 1 GB kept free: NOT enough"),
            "{text}"
        );
        // room for both and the margin
        let p = plan_with(need + 2.5 + 1.0 + 0.1, 2.5);
        assert_eq!(offer(&p).enough_disk, Some(true));
        assert_eq!(p.refused, None);
        // no models to fetch: Ollama alone, no margin added
        let p = plan_with(need + 0.1, 0.0);
        assert_eq!(offer(&p).enough_disk, Some(true));
        assert_eq!(p.refused, None);
    }

    #[test]
    fn free_disk_must_cover_two_and_a_half_times_the_download() {
        let home = tmp("plan-disk");
        let host = linux();
        let pins = Pins::embedded();
        let need = 1438364593f64 * 2.5 / GIB;
        let enough = |disk: Option<f64>| {
            let m = machine("linux", "x86_64", disk);
            match plan_runtime(&m, &host, &home, &pins, 11434) {
                Some(RuntimePlan::Missing(o)) => o.enough_disk,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(enough(Some(need + 0.01)), Some(true));
        assert_eq!(enough(Some(need - 0.01)), Some(false));
        // one times the download is not enough: unpacking needs room too
        assert_eq!(enough(Some(1438364593f64 / GIB + 0.5)), Some(false));
        assert_eq!(enough(None), None);
        let m = machine("linux", "x86_64", Some(2.0));
        let t = plan_runtime(&m, &host, &home, &pins, 11434)
            .unwrap()
            .lines()
            .join("\n");
        assert!(t.contains("NOT enough"), "{t}");
        let m = machine("linux", "x86_64", None);
        let t = plan_runtime(&m, &host, &home, &pins, 11434)
            .unwrap()
            .lines()
            .join("\n");
        assert!(t.contains("Free disk: unknown"), "{t}");
    }

    #[test]
    fn an_unsupported_computer_is_told_so_plainly() {
        let home = tmp("plan-unsup");
        let host = Fake {
            os: "freebsd",
            arch: "x86_64",
            ..Fake::default()
        };
        let m = machine("freebsd", "x86_64", Some(100.0));
        let p = plan_runtime(&m, &host, &home, &Pins::embedded(), 11434).unwrap();
        let RuntimePlan::Unsupported(msg) = &p else {
            panic!("{p:?}")
        };
        assert!(msg.contains("Tempo has no Ollama to fetch for freebsd on x86_64"));
        assert!(msg.contains("Install Ollama from ollama.com"));
        assert_eq!(p.json().str("state"), Some("unsupported"));
    }

    #[test]
    fn linux_without_zstd_is_blocked_in_the_plan_with_the_plain_instruction() {
        let home = tmp("plan-zstd");
        let host = Fake {
            os: "linux",
            arch: "x86_64",
            ..Fake::default()
        };
        let m = machine("linux", "x86_64", Some(100.0));
        let p = plan_runtime(&m, &host, &home, &Pins::embedded(), 11434).unwrap();
        let t = p.lines().join("\n");
        assert!(t.contains(ZSTD_HELP), "{t}");
        // macOS and Windows do not need zstd
        let host = Fake {
            os: "macos",
            arch: "aarch64",
            ..Fake::default()
        };
        let m = machine("macos", "aarch64", Some(100.0));
        let Some(RuntimePlan::Missing(o)) =
            plan_runtime(&m, &host, &home, &Pins::embedded(), 11434)
        else {
            panic!()
        };
        assert_eq!(o.blocker, None);
    }

    #[test]
    fn the_plan_json_has_the_same_facts() {
        let home = tmp("plan-json");
        let m = machine("linux", "x86_64", Some(100.0));
        let p = plan_runtime(&m, &linux(), &home, &Pins::embedded(), 11434).unwrap();
        let j = p.json();
        assert_eq!(j.str("state"), Some("missing"));
        assert_eq!(j.str("version"), Some("0.40.6"));
        assert_eq!(j.str("file"), Some("ollama-linux-amd64.tar.zst"));
        assert_eq!(
            j.str("sha256"),
            Some("c164ab8b1b28e2a247c29719b8e404c423c5b83ae64c3ab4e59d50c7ff7fc5e0")
        );
        assert_eq!(j.f64("bytes"), Some(1438364593.0));
        assert_eq!(j.f64("needs_gb"), Some(3.3));
        assert_eq!(j.f64("disk_free_gb"), Some(100.0));
        assert_eq!(j.get("enough_disk"), Some(&J::Bool(true)));
        assert!(j
            .str("url")
            .unwrap()
            .ends_with("/v0.40.6/ollama-linux-amd64.tar.zst"));
        assert_eq!(j.get("blocked"), Some(&J::Null));
    }

    // ---- the tar command lines

    #[test]
    fn the_tar_command_line_is_exact_on_each_system() {
        let f = Path::new("/h/runtime/downloads/x");
        let d = Path::new("/h/runtime/ollama-0.40.6.partial");
        assert_eq!(
            tar_command("linux", None, f, d).unwrap(),
            (
                "tar".to_string(),
                vec![
                    "--zstd".to_string(),
                    "-xf".into(),
                    "/h/runtime/downloads/x".into(),
                    "-C".into(),
                    "/h/runtime/ollama-0.40.6.partial".into()
                ]
            )
        );
        assert_eq!(
            tar_command("macos", None, f, d).unwrap(),
            (
                "/usr/bin/tar".to_string(),
                vec![
                    "-xzf".to_string(),
                    "/h/runtime/downloads/x".into(),
                    "-C".into(),
                    "/h/runtime/ollama-0.40.6.partial".into()
                ]
            )
        );
        let wf = Path::new(r"C:\h\runtime\downloads\ollama-windows-amd64.zip");
        let wd = Path::new(r"C:\h\runtime\ollama-0.40.6.partial");
        let (prog, args) = tar_command("windows", Some(r"D:\Win"), wf, wd).unwrap();
        assert_eq!(prog, r"D:\Win\System32\tar.exe");
        assert_eq!(
            args,
            vec![
                "-xf",
                r"C:\h\runtime\downloads\ollama-windows-amd64.zip",
                "-C",
                r"C:\h\runtime\ollama-0.40.6.partial"
            ]
        );
        // no SystemRoot: the usual place; a trailing backslash is not doubled
        assert_eq!(
            tar_command("windows", None, wf, wd).unwrap().0,
            r"C:\Windows\System32\tar.exe"
        );
        assert_eq!(
            tar_command("windows", Some(r"C:\Windows\"), wf, wd)
                .unwrap()
                .0,
            r"C:\Windows\System32\tar.exe"
        );
        assert!(tar_command("freebsd", None, f, d).is_err());
    }

    #[test]
    fn the_curl_download_line_is_https_only_resumes_and_has_a_long_limit() {
        let a = super::super::registry::curl_download_args(
            7355,
            Path::new("/h/x.part"),
            "https://example.test/x",
        );
        let s = a.join(" ");
        assert!(s.contains("-C -"), "{s}");
        assert!(s.contains("--proto =https --proto-redir =https"), "{s}");
        assert!(s.contains("--max-time 7355"), "{s}");
        assert!(s.contains("--fail"), "{s}");
        assert!(
            s.ends_with("--output /h/x.part https://example.test/x"),
            "{s}"
        );
        assert_eq!(download_limit_secs(1471164488), 7355);
        assert_eq!(download_limit_secs(1000), 900);
        // the real curl wrapper refuses a plain-http address before it starts anything
        let c = super::super::registry::Curl {
            program: "no-such-curl-program".into(),
        };
        assert!(c
            .download("http://example.test/x", Path::new("/x"), 5, &mut |_| {})
            .unwrap_err()
            .contains("only https"));
        assert!(c
            .download("https://example.test/x", Path::new("/x"), 5, &mut |_| {})
            .unwrap_err()
            .contains("could not start curl"));
    }

    // ---- the install

    #[test]
    fn a_good_download_is_checked_unpacked_found_renamed_and_recorded() {
        let home = tmp("ok");
        let host = linux();
        let dl = FakeDl::new();
        let pins = test_pins();
        let (r, said) = run_install(&home, &host, &dl, &pins);
        let o = r.unwrap();
        let folder = home.join("runtime/ollama-0.40.6");
        assert_eq!(o.binary, folder.join("bin/ollama"));
        assert!(o.binary.is_file());
        // fetched into <file>.part, the pinned url, a long limit
        let calls = dl.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            "https://example.test/ollama/v0.40.6/ollama-linux-amd64.tar.zst"
        );
        assert_eq!(
            calls[0].1,
            home.join("runtime/downloads/ollama-linux-amd64.tar.zst.part")
        );
        assert_eq!(calls[0].2, 900);
        // the checked file is kept under its own name; no .part and no .partial left
        assert!(home
            .join("runtime/downloads/ollama-linux-amd64.tar.zst")
            .is_file());
        assert!(!home
            .join("runtime/downloads/ollama-linux-amd64.tar.zst.part")
            .exists());
        assert!(!home.join("runtime/ollama-0.40.6.partial").exists());
        // the one tar command, after the check, into the .partial folder
        let tools = host.tools.lock().unwrap().clone();
        assert_eq!(
            tools,
            vec![format!(
                "tar --zstd -xf {} -C {}",
                home.join("runtime/downloads/ollama-linux-amd64.tar.zst")
                    .display(),
                home.join("runtime/ollama-0.40.6.partial").display()
            )]
        );
        let text = said.join("\n");
        assert!(text.contains("Checking SHA-256") && text.contains("SHA-256 matches the pin."));
        assert!(
            text.contains("100% (") && text.contains("Fetching ollama-linux-amd64.tar.zst"),
            "{text}"
        );
        // the record
        let rec = std::fs::read_to_string(home.join("runtime/ollama.json")).unwrap();
        let j = parse(&rec).unwrap();
        assert_eq!(j.str("product"), Some("Perennos Tempo"));
        assert_eq!(j.str("purpose"), Some("owned-ollama-runtime"));
        assert_eq!(j.str("version"), Some("0.40.6"));
        assert_eq!(j.str("sha256"), Some(pins.files[2].sha256.as_str()));
        assert_eq!(j.str("file"), Some("ollama-linux-amd64.tar.zst"));
        assert_eq!(
            j.str("binary"),
            Some(folder.join("bin/ollama").to_str().unwrap())
        );
        assert_eq!(j.str("installed_at"), Some("2026-10-26 07:33:20Z"));
        assert_eq!(read_record(&home), Some(o));
        // no temp file from the atomic write is left next to it
        let left: Vec<_> = std::fs::read_dir(home.join("runtime"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty());
        assert!(!home.join("trash").exists(), "nothing needed the trash");
    }

    #[test]
    fn a_sha_mismatch_moves_the_file_to_the_trash_and_refuses() {
        let home = tmp("mismatch");
        let host = linux();
        let dl = FakeDl {
            body: b"something else entirely, same size or not".to_vec(),
            ..FakeDl::new()
        };
        let mut pins = test_pins();
        for f in &mut pins.files {
            f.bytes = dl.body.len() as u64;
        }
        let (r, _) = run_install(&home, &host, &dl, &pins);
        let e = r.unwrap_err();
        assert!(e.contains("did not match its pinned SHA-256"), "{e}");
        assert!(e.contains(&sha256_hex(&dl.body)), "says what it got: {e}");
        assert!(e.contains("nothing was unpacked or installed"), "{e}");
        // moved, never deleted: the bytes are in trash/<date>/
        let day = home.join("trash/20261026");
        let moved = day.join("ollama-linux-amd64.tar.zst.part");
        assert!(e.contains(moved.to_str().unwrap()), "{e}");
        assert_eq!(std::fs::read(&moved).unwrap(), dl.body);
        // nothing was touched after that: no download folder file, no unpack, no record
        assert!(!home
            .join("runtime/downloads/ollama-linux-amd64.tar.zst.part")
            .exists());
        assert!(!home
            .join("runtime/downloads/ollama-linux-amd64.tar.zst")
            .exists());
        assert!(host.tools.lock().unwrap().is_empty(), "tar never ran");
        assert!(!home.join("runtime/ollama-0.40.6").exists());
        assert!(!home.join("runtime/ollama-0.40.6.partial").exists());
        assert!(!home.join("runtime/ollama.json").exists());
    }

    #[test]
    fn a_checked_file_that_changed_on_disk_is_trashed_and_fetched_again() {
        let home = tmp("stale");
        let dir = home.join("runtime/downloads");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ollama-linux-amd64.tar.zst"), b"damaged").unwrap();
        let dl = FakeDl::new();
        let (r, said) = run_install(&home, &linux(), &dl, &test_pins());
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(dl.count(), 1);
        assert!(said.join("\n").contains("did not match its SHA-256"));
        let moved = home.join("trash/20261026/ollama-linux-amd64.tar.zst");
        assert_eq!(std::fs::read(moved).unwrap(), b"damaged");
    }

    #[test]
    fn a_file_checked_by_an_earlier_run_is_not_fetched_again() {
        let home = tmp("reuse");
        let dir = home.join("runtime/downloads");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ollama-linux-amd64.tar.zst"), CONTENT).unwrap();
        let dl = FakeDl::new();
        let (r, said) = run_install(&home, &linux(), &dl, &test_pins());
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(dl.count(), 0);
        assert!(said.join("\n").contains("already fetched and checked"));
    }

    #[test]
    fn a_stopped_download_is_resumed_from_its_part() {
        let home = tmp("resume");
        let dir = home.join("runtime/downloads");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("ollama-linux-amd64.tar.zst.part");
        std::fs::write(&part, &CONTENT[..20]).unwrap();
        let dl = FakeDl::new();
        let (r, said) = run_install(&home, &linux(), &dl, &test_pins());
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(dl.count(), 1, "curl is asked to carry on, with -C -");
        assert!(said.join("\n").contains("carrying on from"), "{said:?}");
        // a download that drops leaves its part for the next run
        let home = tmp("drop");
        let dl = FakeDl {
            fail: Some("curl stopped (exit status: 18)"),
            ..FakeDl::new()
        };
        let (r, _) = run_install(&home, &linux(), &dl, &test_pins());
        let e = r.unwrap_err();
        assert!(
            e.contains("Could not fetch ollama-linux-amd64.tar.zst"),
            "{e}"
        );
        assert!(e.contains("carries on from there"), "{e}");
        assert!(home
            .join("runtime/downloads/ollama-linux-amd64.tar.zst.part")
            .is_file());
        assert!(!home.join("runtime/ollama-0.40.6").exists());
    }

    #[test]
    fn a_part_bigger_than_the_file_is_trashed_not_resumed() {
        let home = tmp("big-part");
        let dir = home.join("runtime/downloads");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("ollama-linux-amd64.tar.zst.part");
        std::fs::write(&part, vec![7u8; CONTENT.len() + 5]).unwrap();
        let dl = FakeDl::new();
        let (r, _) = run_install(&home, &linux(), &dl, &test_pins());
        assert!(r.is_ok(), "{r:?}");
        let moved = home.join("trash/20261026/ollama-linux-amd64.tar.zst.part");
        assert_eq!(
            std::fs::metadata(moved).unwrap().len(),
            CONTENT.len() as u64 + 5
        );
    }

    #[test]
    fn zstd_missing_stops_before_any_download_with_the_plain_words() {
        let home = tmp("nozstd");
        let host = Fake {
            os: "linux",
            arch: "x86_64",
            tar_makes: Some("bin/ollama"),
            ..Fake::default()
        };
        let dl = FakeDl::new();
        let (r, _) = run_install(&home, &host, &dl, &test_pins());
        let e = r.unwrap_err();
        assert!(
            e.contains("install zstd (for example sudo apt install zstd), or install Ollama from ollama.com, then run setup again"),
            "{e}"
        );
        assert_eq!(dl.count(), 0, "1.4 GB is not fetched for nothing");
        assert!(host.tools.lock().unwrap().is_empty());
        assert!(!home.join("runtime/downloads").exists());
    }

    #[test]
    fn an_existing_folder_is_never_overwritten() {
        let home = tmp("exists");
        let folder = home.join("runtime/ollama-0.40.6");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("mine.txt"), "keep me").unwrap();
        let host = linux();
        let dl = FakeDl::new();
        let (r, _) = run_install(&home, &host, &dl, &test_pins());
        let e = r.unwrap_err();
        assert!(
            e.contains("already exists") && e.contains("never overwrites"),
            "{e}"
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("mine.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(dl.count(), 0);
        assert!(host.tools.lock().unwrap().is_empty());
        assert!(!home.join("runtime/ollama.json").exists());
        // the plan says so too
        let m = machine("linux", "x86_64", Some(100.0));
        let Some(RuntimePlan::Missing(o)) = plan_runtime(&m, &host, &home, &test_pins(), 11434)
        else {
            panic!()
        };
        assert!(o.blocker.unwrap().contains("never overwrites"));
    }

    #[test]
    fn an_existing_empty_folder_is_not_replaced_by_a_rename() {
        // the folder appears while the archive is being unpacked (rename would take an empty one)
        let home = tmp("race");
        let folder = home.join("runtime/ollama-0.40.6");
        let host = Fake {
            tar_makes: Some("bin/ollama"),
            ..linux()
        };
        let pins = test_pins();
        let pin = pins.files[2].clone();
        let dl = FakeDl::new();
        let dir = home.join("runtime/downloads");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&pin.file), CONTENT).unwrap();
        // run unpack directly with the folder already there
        std::fs::create_dir_all(&folder).unwrap();
        let e = unpack(
            &home,
            &host,
            &pins,
            &pin,
            &dir.join(&pin.file),
            1,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(e.contains("never overwrites"), "{e}");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
        assert_eq!(dl.count(), 0);
    }

    #[test]
    fn a_leftover_partial_goes_to_the_trash_first() {
        let home = tmp("leftover");
        let partial = home.join("runtime/ollama-0.40.6.partial");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("old.bin"), "half").unwrap();
        let dl = FakeDl::new();
        let (r, said) = run_install(&home, &linux(), &dl, &test_pins());
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(
            std::fs::read_to_string(home.join("trash/20261026/ollama-0.40.6.partial/old.bin"))
                .unwrap(),
            "half"
        );
        assert!(said
            .join("\n")
            .contains("leftover unpack folder was moved to"));
        // and the new folder holds only the fresh unpack
        assert!(!home.join("runtime/ollama-0.40.6/old.bin").exists());
    }

    #[test]
    fn a_failed_unpack_installs_nothing_and_says_what_tar_said() {
        let home = tmp("tarfail");
        let host = Fake {
            tar_fails: Some("tar: Unexpected EOF in archive"),
            ..linux()
        };
        let (r, _) = run_install(&home, &host, &FakeDl::new(), &test_pins());
        let e = r.unwrap_err();
        assert!(
            e.contains("Unpacking ollama-linux-amd64.tar.zst failed"),
            "{e}"
        );
        assert!(e.contains("Unexpected EOF"), "{e}");
        assert!(!home.join("runtime/ollama-0.40.6").exists());
        assert!(home.join("runtime/ollama-0.40.6.partial").is_dir());
        assert!(!home.join("runtime/ollama.json").exists());
    }

    #[test]
    fn an_archive_with_no_ollama_program_installs_nothing() {
        let home = tmp("nobin");
        let host = Fake {
            tar_makes: Some("README.txt"),
            ..linux()
        };
        let (r, _) = run_install(&home, &host, &FakeDl::new(), &test_pins());
        assert!(r.unwrap_err().contains("no ollama program was inside"));
        assert!(!home.join("runtime/ollama-0.40.6").exists());
        assert!(!home.join("runtime/ollama.json").exists());
    }

    #[test]
    fn windows_and_macos_run_their_own_tar() {
        for (os, arch, file, tar_first, rel) in [
            (
                "windows",
                "x86_64",
                "ollama-windows-amd64.zip",
                "-xf",
                "ollama.exe",
            ),
            ("macos", "aarch64", "ollama-darwin.tgz", "-xzf", "ollama"),
        ] {
            let home = tmp("os-tar");
            let mut host = Fake {
                os,
                arch,
                tar_makes: Some(rel),
                ..Fake::default()
            };
            host.vars.insert("SystemRoot".into(), r"C:\Windows".into());
            let dl = FakeDl::new();
            let (r, _) = run_install(&home, &host, &dl, &test_pins());
            let o = r.unwrap();
            assert!(o.binary.ends_with(rel), "{:?}", o.binary);
            let t = host.tools.lock().unwrap().clone();
            assert_eq!(t.len(), 1);
            let prog = if os == "windows" {
                r"C:\Windows\System32\tar.exe"
            } else {
                "/usr/bin/tar"
            };
            let want = format!(
                "{prog} {tar_first} {} -C {}",
                home.join("runtime/downloads").join(file).display(),
                home.join("runtime/ollama-0.40.6.partial").display()
            );
            assert_eq!(t[0], want);
        }
    }

    #[test]
    fn an_installed_copy_is_found_again_and_not_fetched_twice() {
        let home = tmp("again");
        let host = linux();
        let dl = FakeDl::new();
        let (r, _) = run_install(&home, &host, &dl, &test_pins());
        let first = r.unwrap();
        let (r, said) = run_install(&home, &host, &dl, &test_pins());
        assert_eq!(r.unwrap(), first);
        assert_eq!(dl.count(), 1);
        assert_eq!(host.tools.lock().unwrap().len(), 1);
        assert!(said.join("\n").contains("already installed"));
        // the plan now says it is installed, even though no Ollama answers
        let m = machine("linux", "x86_64", Some(100.0));
        let p = plan_runtime(&m, &host, &home, &test_pins(), 11434).unwrap();
        assert!(matches!(p, RuntimePlan::Installed(_)));
        assert!(p.lines()[0].contains("Tempo's own copy (0.40.6) is installed"));
    }

    #[test]
    fn the_binary_is_the_nearest_ollama_file_and_never_a_folder_or_a_link() {
        let d = tmp("find");
        std::fs::create_dir_all(d.join("lib/ollama/runners")).unwrap();
        std::fs::write(d.join("lib/ollama/runners/ollama"), "deep").unwrap();
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("bin/ollama"), "top").unwrap();
        assert_eq!(find_binary(&d), Some(d.join("bin/ollama")));
        let e = tmp("find-none");
        std::fs::create_dir_all(e.join("lib/ollama")).unwrap();
        assert_eq!(
            find_binary(&e),
            None,
            "a folder named ollama is not the program"
        );
        let w = tmp("find-win");
        std::fs::write(w.join("OLLAMA.EXE"), "x").unwrap();
        assert_eq!(find_binary(&w), Some(w.join("OLLAMA.EXE")));
    }

    #[cfg(unix)]
    #[test]
    fn the_unpacked_program_is_made_runnable() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp("exec");
        let (r, _) = run_install(&home, &linux(), &FakeDl::new(), &test_pins());
        let mode = std::fs::metadata(r.unwrap().binary)
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "{mode:o}");
    }

    #[test]
    fn a_record_only_counts_when_it_is_ours_and_its_program_is_inside_the_runtime_folder() {
        let home = tmp("record");
        let rt = home.join("runtime");
        std::fs::create_dir_all(rt.join("ollama-0.40.6")).unwrap();
        let prog = rt.join("ollama-0.40.6/ollama");
        std::fs::write(&prog, "x").unwrap();
        let outside = home.join("elsewhere");
        std::fs::write(&outside, "x").unwrap();
        let rec = |purpose: &str, bin: &Path| {
            let j = J::Obj(vec![
                ("product".into(), J::s("Perennos Tempo")),
                ("purpose".into(), J::s(purpose)),
                ("binary".into(), J::s(&bin.to_string_lossy())),
            ]);
            std::fs::write(rt.join("ollama.json"), j.pretty()).unwrap();
            read_record(&home)
        };
        assert!(rec(PURPOSE, &prog).is_some());
        assert!(rec("something-else", &prog).is_none());
        assert!(
            rec(PURPOSE, &outside).is_none(),
            "outside the runtime folder"
        );
        assert!(rec(PURPOSE, &rt.join("ollama-0.40.6/../../elsewhere")).is_none());
        assert!(rec(PURPOSE, &rt.join("gone")).is_none(), "program missing");
        std::fs::write(rt.join("ollama.json"), "{ nope").unwrap();
        assert!(read_record(&home).is_none());
    }

    #[test]
    fn trash_names_never_collide() {
        let home = tmp("trash");
        let mut got = Vec::new();
        for i in 0..3 {
            let f = home.join("a.part");
            std::fs::write(&f, format!("{i}")).unwrap();
            got.push(to_trash(&home, &f, 1_793_000_000).unwrap());
        }
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 3);
        assert!(got
            .iter()
            .all(|p| p.starts_with(home.join("trash/20261026"))));
    }

    // ---- the real end-to-end check (needs the network and 200 MB): see docs, run with --ignored

    /// A computer that is macOS to the program (so its real command line, `/usr/bin/tar -xzf`, is
    /// the one that runs) but whose tools are this machine's real ones.
    struct PretendMac;

    impl Host for PretendMac {
        fn os(&self) -> String {
            "macos".into()
        }
        fn arch(&self) -> String {
            "aarch64".into()
        }
        fn cpus(&self) -> usize {
            1
        }
        fn env(&self, _: &str) -> Option<String> {
            None
        }
        fn read(&self, _: &Path) -> Option<String> {
            None
        }
        fn run(&self, prog: &str, args: &[&str]) -> Option<String> {
            super::super::probe::run_with_timeout(prog, args, Duration::from_secs(4))
        }
        fn win_memory(&self) -> Option<(u64, u64)> {
            None
        }
        fn win_disk_free(&self, _: &Path) -> Option<u64> {
            None
        }
        fn ggufs(&self, _: &Path) -> Vec<(PathBuf, u64)> {
            Vec::new()
        }
        fn run_tool(&self, prog: &str, args: &[&str], limit: Duration) -> Result<(), String> {
            super::super::probe::run_checked(prog, args, limit)
        }
        fn ollama_binary(&self) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    #[ignore = "fetches the real 174 MB macOS file; set TEMPO_E2E_DIR to an empty scratch folder"]
    fn real_macos_file_through_real_curl_real_sha256_and_real_tar() {
        let root = PathBuf::from(std::env::var("TEMPO_E2E_DIR").expect("TEMPO_E2E_DIR"));
        let home = root.join("home");
        let pins = Pins::embedded();
        let pin = pins.pin_for("macos", "aarch64").unwrap().clone();
        assert_eq!(pin.bytes, 174_459_907);
        let curl = super::super::registry::find_curl(&PretendMac).expect("curl");
        let fetch = super::super::registry::Curl { program: curl };
        let mut lines = Vec::new();
        let r = install(
            &home,
            &PretendMac,
            &fetch,
            &pins,
            &pin,
            1_793_000_000,
            &mut |s| {
                println!("{s}");
                lines.push(s.to_string())
            },
        );
        let o = r.expect("install");
        let on_disk = super::super::sha256::file_sha256(
            &home.join("runtime/downloads").join(&pin.file),
            &mut |_| {},
        )
        .unwrap();
        println!("sha256 {on_disk}  {}", pin.file);
        assert_eq!(on_disk, pin.sha256);
        assert_eq!(
            on_disk,
            "b2b55434865a668decb494110d7fd970eed3ecc8837be2872f672ccbb95df974"
        );
        println!("binary found: {}", o.binary.display());
        assert!(o.binary.is_file());
        assert!(o.binary.starts_with(home.join("runtime/ollama-0.40.6")));
        assert_eq!(read_record(&home), Some(o));
        println!("{}", std::fs::read_to_string(record_path(&home)).unwrap());
    }
}
