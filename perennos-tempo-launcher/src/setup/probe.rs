//! What this computer is: memory, graphics card, free disk, Ollama and the Perennos models already
//! on it. Everything the probe touches goes through `Host`, so tests feed it canned answers; the
//! real `RealHost` runs programs hidden on Windows and never waits more than a few seconds.

use super::json::parse;
use crate::http_get;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The outside world, as the probe sees it.
pub trait Host {
    fn os(&self) -> String;
    fn arch(&self) -> String;
    fn cpus(&self) -> usize;
    fn env(&self, key: &str) -> Option<String>;
    fn read(&self, path: &Path) -> Option<String>;
    /// Run a program and return its output; None when it is missing, fails or is too slow.
    fn run(&self, prog: &str, args: &[&str]) -> Option<String>;
    /// Windows only: (total, available) physical memory in bytes.
    fn win_memory(&self) -> Option<(u64, u64)>;
    /// Windows only: free bytes for the current user on the disk holding `path`.
    fn win_disk_free(&self, path: &Path) -> Option<u64>;
    /// `.gguf` files in `dir`, two folders deep, with their sizes.
    fn ggufs(&self, dir: &Path) -> Vec<(PathBuf, u64)>;
    /// Run a program to its end (a long job, such as unpacking an archive) and say how it went:
    /// Ok, or what it said. Never shows a window. `limit` is the most it may take.
    fn run_tool(&self, _prog: &str, _args: &[&str], _limit: Duration) -> Result<(), String> {
        Err("this host cannot run programs".into())
    }
    /// The pinned Ollama release Tempo may fetch for itself (tests give their own).
    fn runtime_pins(&self) -> super::runtime::Pins {
        super::runtime::Pins::embedded()
    }
    /// The `ollama` program, if it is installed.
    fn ollama_binary(&self) -> Option<PathBuf>;
    /// HTTPS lookups for the Ollama registry: None when this computer has no curl (or in tests).
    fn fetcher(&self) -> Option<std::sync::Arc<dyn super::registry::Fetch>> {
        None
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Gpu {
    None,
    Nvidia {
        name: String,
        total_gb: f64,
        free_gb: f64,
    },
    /// Apple Silicon: the graphics chip shares the computer's memory.
    Apple {
        chip: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    pub name: String,
    pub size_bytes: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ollama {
    pub binary: Option<PathBuf>,
    pub running: bool,
    pub version: Option<String>,
    pub models: Vec<Installed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Umbra2,
    Umbra3,
    /// Some other UMBRA model.
    Umbra,
    D1,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Where {
    /// An Ollama model of this name.
    OllamaTag(String),
    /// A GGUF file under `<home>/models/`.
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Local {
    pub family: Family,
    pub at: Where,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub os: String,
    pub arch: String,
    pub cpus: usize,
    pub ram_total_gb: Option<f64>,
    pub ram_avail_gb: Option<f64>,
    pub gpu: Gpu,
    pub disk_free_gb: Option<f64>,
    pub ollama: Ollama,
    pub local: Vec<Local>,
    /// Installed models whose names start with this are the owner's own preferred ones ("" for none):
    /// `TEMPO_PREFERRED_PREFIX`, else `preferred_prefix` in `<home>/setup.json`.
    pub preferred_prefix: String,
}

// ---- parsers (pure)

/// `/proc/meminfo`: (total, available) in GB.
pub fn parse_meminfo(text: &str) -> Option<(f64, f64)> {
    let kb = |key: &str| -> Option<f64> {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|r| r.trim_start_matches(':').split_whitespace().next())
            .and_then(|n| n.parse::<f64>().ok())
    };
    let total = kb("MemTotal")?;
    let avail = kb("MemAvailable").or_else(|| kb("MemFree"))?;
    Some((total * 1024.0 / GIB, avail * 1024.0 / GIB))
}

/// macOS `vm_stat`: available GB = free + inactive + speculative pages.
pub fn parse_vm_stat(text: &str) -> Option<f64> {
    let page: f64 = text
        .lines()
        .next()
        .and_then(|l| l.split("page size of").nth(1))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(4096.0);
    let pages = |key: &str| -> f64 {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|n| n.trim().trim_end_matches('.').parse().ok())
            .unwrap_or(0.0)
    };
    let n = pages("Pages free") + pages("Pages inactive") + pages("Pages speculative");
    (n > 0.0).then_some(n * page / GIB)
}

/// `nvidia-smi --query-gpu=name,memory.total,memory.free --format=csv,noheader,nounits` (MiB);
/// the card with the most memory when there are several.
pub fn parse_nvidia(text: &str) -> Option<Gpu> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < 3 {
                return None;
            }
            let total: f64 = f[f.len() - 2].parse().ok()?;
            let free: f64 = f[f.len() - 1].parse().ok()?;
            let name = f[..f.len() - 2].join(", ");
            (!name.is_empty() && total > 0.0).then_some(Gpu::Nvidia {
                name,
                total_gb: total / 1024.0,
                free_gb: free / 1024.0,
            })
        })
        .max_by(|a, b| match (a, b) {
            (Gpu::Nvidia { total_gb: x, .. }, Gpu::Nvidia { total_gb: y, .. }) => x.total_cmp(y),
            _ => std::cmp::Ordering::Equal,
        })
}

/// `df -Pk PATH`: available GB.
pub fn parse_df(text: &str) -> Option<f64> {
    let line = text.lines().nth(1)?;
    let f: Vec<&str> = line.split_whitespace().collect();
    let kb: f64 = f.get(3)?.parse().ok()?;
    Some(kb * 1024.0 / GIB)
}

/// Words of a model name, lower case: `hf.co/LiquidAI/d1-2B-GGUF:Q4` gives hf co liquidai d1 2b gguf q4.
fn words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Which Perennos model a name or file name is: a word starting `umbra`, or the word `d1`.
pub fn family_of(name: &str) -> Option<Family> {
    let w = words(name);
    if w.iter().any(|x| x.starts_with("umbra")) {
        let compact = name.to_lowercase().replace(['-', '_', ' ', ':'], "");
        return Some(if compact.contains("umbra3") {
            Family::Umbra3
        } else if compact.contains("umbra2") {
            Family::Umbra2
        } else {
            Family::Umbra
        });
    }
    w.iter().any(|x| x == "d1").then_some(Family::D1)
}

/// `/api/version` and `/api/tags` bodies.
pub fn parse_tags(text: &str) -> Vec<Installed> {
    let Ok(j) = parse(text) else {
        return Vec::new();
    };
    j.arr("models")
        .iter()
        .filter_map(|m| {
            Some(Installed {
                name: m.str("name").or_else(|| m.str("model"))?.to_string(),
                size_bytes: m.f64("size").unwrap_or(0.0) as u64,
                digest: m.str("digest").unwrap_or("").to_string(),
            })
        })
        .collect()
}

pub fn probe_ollama(host: &dyn Host, port: u16) -> Ollama {
    let t = Duration::from_millis(1500);
    let mut o = Ollama {
        binary: host.ollama_binary(),
        ..Ollama::default()
    };
    if let Some((200, body)) = http_get(port, "/api/version", t) {
        if let Ok(j) = parse(&body) {
            o.version = j.str("version").map(String::from);
            o.running = o.version.is_some();
        }
    }
    if o.running {
        if let Some((200, body)) = http_get(port, "/api/tags", t) {
            o.models = parse_tags(&body);
        }
    }
    o
}

pub fn probe_memory(host: &dyn Host) -> (Option<f64>, Option<f64>) {
    match host.os().as_str() {
        "linux" => host
            .read(Path::new("/proc/meminfo"))
            .and_then(|t| parse_meminfo(&t))
            .map_or((None, None), |(t, a)| (Some(t), Some(a))),
        "macos" => {
            let total = host
                .run("sysctl", &["-n", "hw.memsize"])
                .and_then(|t| t.trim().parse::<f64>().ok())
                .map(|b| b / GIB);
            let avail = host.run("vm_stat", &[]).and_then(|t| parse_vm_stat(&t));
            (total, avail)
        }
        "windows" => host.win_memory().map_or((None, None), |(t, a)| {
            (Some(t as f64 / GIB), Some(a as f64 / GIB))
        }),
        _ => (None, None),
    }
}

pub fn probe_gpu(host: &dyn Host) -> Gpu {
    if let Some(g) = host
        .run(
            "nvidia-smi",
            &[
                "--query-gpu=name,memory.total,memory.free",
                "--format=csv,noheader,nounits",
            ],
        )
        .and_then(|t| parse_nvidia(&t))
    {
        return g;
    }
    if host.os() == "macos" && host.arch() == "aarch64" {
        let chip = host
            .run("sysctl", &["-n", "machdep.cpu.brand_string"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Apple Silicon".into());
        return Gpu::Apple { chip };
    }
    Gpu::None
}

/// Free disk where Tempo keeps its files (the nearest folder that exists).
pub fn probe_disk(host: &dyn Host, home: &Path) -> Option<f64> {
    let mut dir = home;
    while !dir.exists() {
        dir = dir.parent()?;
    }
    match host.os().as_str() {
        "windows" => host.win_disk_free(dir).map(|b| b as f64 / GIB),
        "linux" | "macos" => host
            .run("/bin/df", &["-Pk", &dir.to_string_lossy()])
            .and_then(|t| parse_df(&t)),
        _ => None,
    }
}

pub fn probe_local(host: &dyn Host, ollama: &Ollama, home: &Path) -> Vec<Local> {
    let mut out: Vec<Local> = ollama
        .models
        .iter()
        .filter_map(|m| {
            Some(Local {
                family: family_of(&m.name)?,
                at: Where::OllamaTag(m.name.clone()),
                size_bytes: m.size_bytes,
            })
        })
        .collect();
    for (path, size) in host.ggufs(&home.join("models")) {
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if let Some(family) = file.as_deref().and_then(family_of) {
            out.push(Local {
                family,
                at: Where::File(path),
                size_bytes: size,
            });
        }
    }
    out
}

pub fn probe(host: &dyn Host, home: &Path, ollama_port: u16) -> Machine {
    let (ram_total_gb, ram_avail_gb) = probe_memory(host);
    let ollama = probe_ollama(host, ollama_port);
    let local = probe_local(host, &ollama, home);
    Machine {
        os: host.os(),
        arch: host.arch(),
        cpus: host.cpus(),
        ram_total_gb,
        ram_avail_gb,
        gpu: probe_gpu(host),
        disk_free_gb: probe_disk(host, home),
        ollama,
        local,
        preferred_prefix: preferred_prefix(host, home),
    }
}

/// A preferred prefix is 1 to 40 of a-z 0-9 . _ - (anything else is ignored).
pub fn valid_prefix(p: &str) -> bool {
    (1..=40).contains(&p.len())
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// The owner's preferred prefix: the environment first, then the saved setting, else none.
pub fn preferred_prefix(host: &dyn Host, home: &Path) -> String {
    let saved = || {
        let text = host.read(&home.join("setup.json"))?;
        super::json::parse(&text)
            .ok()?
            .str("preferred_prefix")
            .map(str::to_string)
    };
    host.env("TEMPO_PREFERRED_PREFIX")
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .or_else(saved)
        .filter(|p| valid_prefix(p))
        .unwrap_or_default()
}

// ---- the real thing

pub struct RealHost;

/// Run a program hidden, with a time limit. Only a child this function started is ever killed.
pub fn run_with_timeout(prog: &str, args: &[&str], limit: Duration) -> Option<String> {
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(&mut out, 1 << 20), &mut buf);
        let _ = tx.send(buf);
    });
    let start = Instant::now();
    let buf = loop {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(50)) {
            break Some(b);
        }
        if start.elapsed() > limit {
            break None;
        }
    };
    match buf {
        Some(b) => {
            let ok = child.wait().map(|s| s.success()).unwrap_or(false);
            ok.then(|| String::from_utf8_lossy(&b).into_owned())
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// Run a program hidden until it ends or `limit` passes (then only that child is killed). Ok when
/// it exits 0; otherwise the last lines it said on stderr.
pub fn run_checked(prog: &str, args: &[&str], limit: Duration) -> Result<(), String> {
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start {prog}: {e}"))?;
    let mut err = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(e) = err.as_mut() {
            let _ = std::io::Read::read_to_end(&mut std::io::Read::take(e, 16 << 10), &mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{prog} took longer than {} seconds",
                    limit.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("could not wait for {prog}: {e}")),
        }
    };
    let said = reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let mut tail: Vec<&str> = said.lines().rev().take(3).collect();
    tail.reverse();
    Err(format!("{prog} stopped ({status}): {}", tail.join(" ")))
}

fn walk_ggufs(dir: &Path, depth: u32, out: &mut Vec<(PathBuf, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() && depth > 0 {
            walk_ggufs(&p, depth - 1, out);
        } else if ft.is_file()
            && p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("gguf"))
        {
            out.push((p, e.metadata().map(|m| m.len()).unwrap_or(0)));
        }
    }
}

impl Host for RealHost {
    fn os(&self) -> String {
        std::env::consts::OS.into()
    }
    fn arch(&self) -> String {
        std::env::consts::ARCH.into()
    }
    fn cpus(&self) -> usize {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    }
    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
    fn run(&self, prog: &str, args: &[&str]) -> Option<String> {
        run_with_timeout(prog, args, Duration::from_secs(4))
    }
    fn win_memory(&self) -> Option<(u64, u64)> {
        #[cfg(windows)]
        return winffi::memory();
        #[cfg(not(windows))]
        None
    }
    fn win_disk_free(&self, path: &Path) -> Option<u64> {
        #[cfg(windows)]
        return winffi::disk_free(path);
        #[cfg(not(windows))]
        {
            let _ = path;
            None
        }
    }
    fn run_tool(&self, prog: &str, args: &[&str], limit: Duration) -> Result<(), String> {
        run_checked(prog, args, limit)
    }
    fn ggufs(&self, dir: &Path) -> Vec<(PathBuf, u64)> {
        let mut v = Vec::new();
        walk_ggufs(dir, 2, &mut v);
        v.sort();
        v
    }
    fn ollama_binary(&self) -> Option<PathBuf> {
        let exe = format!("ollama{}", std::env::consts::EXE_SUFFIX);
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(Path::new(&l).join("Programs").join("Ollama"));
        }
        // a relative PATH entry means "the current folder": never run a program from there
        dirs.into_iter()
            .filter(|d| d.is_absolute())
            .map(|d| d.join(&exe))
            .find(|p| p.is_file())
    }
    fn fetcher(&self) -> Option<std::sync::Arc<dyn super::registry::Fetch>> {
        super::registry::real_fetcher(self)
    }
}

#[cfg(windows)]
mod winffi {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32;
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            avail: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }

    pub fn memory() -> Option<(u64, u64)> {
        let mut m = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `m` is a valid, correctly sized MEMORYSTATUSEX with `length` set, as the call requires.
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) };
        (ok != 0 && m.total_phys > 0).then_some((m.total_phys, m.avail_phys))
    }

    pub fn disk_free(path: &Path) -> Option<u64> {
        let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: `w` is NUL-terminated and the three outputs are valid u64s.
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
        (ok != 0).then_some(avail)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A canned computer.
    #[derive(Default)]
    pub struct Fake {
        pub os: &'static str,
        pub arch: &'static str,
        pub cpus: usize,
        pub files: HashMap<String, String>,
        pub cmds: HashMap<String, String>,
        pub ggufs: Vec<(PathBuf, u64)>,
        pub win_mem: Option<(u64, u64)>,
        pub win_disk: Option<u64>,
        pub ollama_bin: Option<PathBuf>,
        pub fetcher: Option<std::sync::Arc<dyn crate::setup::registry::Fetch>>,
        /// Every program `run_tool` was asked to run, as `program arg arg`.
        pub tools: std::sync::Mutex<Vec<String>>,
        /// A fake `tar`: unpacking makes this file (a path inside the `-C` folder).
        pub tar_makes: Option<&'static str>,
        /// A fake `tar` that fails with this text.
        pub tar_fails: Option<&'static str>,
        /// Environment variables the host reports.
        pub vars: HashMap<String, String>,
        /// The pinned Ollama release (default: the one that ships).
        pub pins: Option<crate::setup::runtime::Pins>,
    }

    impl Host for Fake {
        fn os(&self) -> String {
            self.os.into()
        }
        fn arch(&self) -> String {
            self.arch.into()
        }
        fn cpus(&self) -> usize {
            self.cpus
        }
        fn env(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn read(&self, p: &Path) -> Option<String> {
            self.files.get(p.to_str()?).cloned()
        }
        fn run_tool(&self, prog: &str, args: &[&str], _: Duration) -> Result<(), String> {
            self.tools
                .lock()
                .unwrap()
                .push(format!("{prog} {}", args.join(" ")));
            if let Some(why) = self.tar_fails {
                return Err(why.to_string());
            }
            if let (Some(rel), Some(i)) = (self.tar_makes, args.iter().position(|a| *a == "-C")) {
                let f = Path::new(args[i + 1]).join(rel);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, "#!/bin/sh\n").unwrap();
            }
            Ok(())
        }
        fn run(&self, prog: &str, args: &[&str]) -> Option<String> {
            self.cmds
                .get(&format!("{prog} {}", args.join(" ")))
                .cloned()
        }
        fn win_memory(&self) -> Option<(u64, u64)> {
            self.win_mem
        }
        fn win_disk_free(&self, _: &Path) -> Option<u64> {
            self.win_disk
        }
        fn ggufs(&self, _: &Path) -> Vec<(PathBuf, u64)> {
            self.ggufs.clone()
        }
        fn ollama_binary(&self) -> Option<PathBuf> {
            self.ollama_bin.clone()
        }
        fn runtime_pins(&self) -> crate::setup::runtime::Pins {
            self.pins
                .clone()
                .unwrap_or_else(crate::setup::runtime::Pins::embedded)
        }
        fn fetcher(&self) -> Option<std::sync::Arc<dyn crate::setup::registry::Fetch>> {
            self.fetcher.clone()
        }
    }

    const MEMINFO: &str = "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    9000000 kB\nBuffers: 1 kB\n";
    const SMI: &str = "NVIDIA GeForce RTX 3080, 16384, 15000\n";

    #[test]
    fn meminfo_nvidia_df_and_vm_stat_parse() {
        let (t, a) = parse_meminfo(MEMINFO).unwrap();
        assert!((t - 15.625).abs() < 0.01 && (a - 8.583).abs() < 0.01);
        assert!(parse_meminfo("nothing").is_none());
        match parse_nvidia(SMI).unwrap() {
            Gpu::Nvidia {
                name,
                total_gb,
                free_gb,
            } => {
                assert_eq!(name, "NVIDIA GeForce RTX 3080");
                assert_eq!(total_gb, 16.0);
                assert!((free_gb - 14.648).abs() < 0.01);
            }
            g => panic!("{g:?}"),
        }
        // two cards: the bigger one
        let two = "NVIDIA GTX 1050 Ti, 4096, 4000\nNVIDIA RTX 3060, 12288, 12000\n";
        assert!(
            matches!(parse_nvidia(two), Some(Gpu::Nvidia { total_gb, .. }) if total_gb == 12.0)
        );
        assert!(parse_nvidia("No devices were found").is_none());
        assert!(parse_nvidia("").is_none());
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 100 50 52428800 50% /\n";
        assert_eq!(parse_df(df), Some(50.0));
        let vm = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:      1000.\nPages inactive:  2000.\nPages speculative: 1000.\n";
        let g = parse_vm_stat(vm).unwrap();
        assert!((g - 4000.0 * 16384.0 / GIB).abs() < 1e-9);
    }

    #[test]
    fn family_names() {
        assert_eq!(family_of("umbra-2:q4"), Some(Family::Umbra2));
        assert_eq!(family_of("UMBRA-3-big"), Some(Family::Umbra3));
        assert_eq!(family_of("umbra-experiment"), Some(Family::Umbra));
        assert_eq!(
            family_of("hf.co/LiquidAI/d1-2B-GGUF:Q4_K_M"),
            Some(Family::D1)
        );
        assert_eq!(family_of("qwen3:4b"), None);
        assert_eq!(family_of("hid1den:7b"), None);
    }

    #[test]
    fn linux_probe_from_canned_answers() {
        let mut f = Fake {
            os: "linux",
            arch: "x86_64",
            cpus: 8,
            ..Fake::default()
        };
        f.files.insert("/proc/meminfo".into(), MEMINFO.into());
        f.cmds.insert(
            "nvidia-smi --query-gpu=name,memory.total,memory.free --format=csv,noheader,nounits"
                .into(),
            SMI.into(),
        );
        f.ggufs = vec![(PathBuf::from("/h/models/umbra-2-q4.gguf"), 1_100_000_000)];
        // the home does not need to exist: the nearest folder that does is asked about disk
        let m = probe(&f, Path::new("/nonexistent-tempo-home/x"), 1);
        assert_eq!(m.cpus, 8);
        assert!((m.ram_total_gb.unwrap() - 15.625).abs() < 0.01);
        assert!(matches!(m.gpu, Gpu::Nvidia { .. }));
        assert!(!m.ollama.running);
        assert_eq!(m.local.len(), 1);
        assert_eq!(m.local[0].family, Family::Umbra2);
        assert_eq!(m.disk_free_gb, None); // no canned df answer: unknown, not guessed
    }

    #[test]
    fn mac_and_windows_memory_and_apple_gpu() {
        let mut f = Fake {
            os: "macos",
            arch: "aarch64",
            cpus: 8,
            ..Fake::default()
        };
        f.cmds
            .insert("sysctl -n hw.memsize".into(), "17179869184\n".into());
        f.cmds.insert(
            "sysctl -n machdep.cpu.brand_string".into(),
            "Apple M2\n".into(),
        );
        let (t, a) = probe_memory(&f);
        assert_eq!(t, Some(16.0));
        assert_eq!(a, None);
        assert_eq!(
            probe_gpu(&f),
            Gpu::Apple {
                chip: "Apple M2".into()
            }
        );
        let w = Fake {
            os: "windows",
            arch: "x86_64",
            win_mem: Some((32 << 30, 20 << 30)),
            win_disk: Some(50 << 30),
            ..Fake::default()
        };
        assert_eq!(probe_memory(&w), (Some(32.0), Some(20.0)));
        assert_eq!(probe_gpu(&w), Gpu::None);
        assert_eq!(probe_disk(&w, &std::env::temp_dir()), Some(50.0));
    }

    /// A mock Ollama on 127.0.0.1 answering /api/version and /api/tags.
    pub fn mock_ollama(tags: &'static str) -> u16 {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let body = if req.starts_with("GET /api/version") {
                    r#"{"version":"0.6.1"}"#
                } else if req.starts_with("GET /api/tags") {
                    tags
                } else {
                    "{}"
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    #[test]
    fn ollama_probe_against_a_mock() {
        let port = mock_ollama(
            r#"{"models":[{"name":"qwen3:4b","size":2500000000,"digest":"abc"},{"name":"umbra-2:latest","size":1100000000,"digest":"d"}]}"#,
        );
        let f = Fake {
            os: "linux",
            ollama_bin: Some(PathBuf::from("/usr/bin/ollama")),
            ..Fake::default()
        };
        let o = probe_ollama(&f, port);
        assert!(o.running);
        assert_eq!(o.version.as_deref(), Some("0.6.1"));
        assert_eq!(o.models.len(), 2);
        assert!(o.binary.is_some());
        let local = probe_local(&f, &o, Path::new("/none"));
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].at, Where::OllamaTag("umbra-2:latest".into()));
    }

    #[test]
    fn nothing_listening_means_not_running() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let o = probe_ollama(&Fake::default(), port);
        assert!(!o.running && o.models.is_empty());
    }

    #[test]
    fn run_with_timeout_gives_output_fails_and_times_out() {
        if cfg!(unix) {
            assert_eq!(
                run_with_timeout("/bin/echo", &["hi"], Duration::from_secs(3)).as_deref(),
                Some("hi\n")
            );
            assert!(run_with_timeout("/bin/false", &[], Duration::from_secs(3)).is_none());
            let t = Instant::now();
            assert!(run_with_timeout("/bin/sleep", &["5"], Duration::from_millis(300)).is_none());
            assert!(t.elapsed() < Duration::from_secs(3));
        }
        assert!(run_with_timeout("no-such-program-xyz", &[], Duration::from_secs(1)).is_none());
    }

    #[test]
    fn the_preferred_prefix_comes_from_the_environment_then_the_saved_setting_and_must_be_plain() {
        let home = Path::new("/h");
        let saved = |text: &str| Fake {
            files: [("/h/setup.json".to_string(), text.to_string())].into(),
            ..Fake::default()
        };
        // nothing set: no preference
        assert_eq!(preferred_prefix(&Fake::default(), home), "");
        // the saved setting
        let f = saved(r#"{"preferred_prefix": "mine-"}"#);
        assert_eq!(preferred_prefix(&f, home), "mine-");
        // the environment wins, in lower case
        let mut f = saved(r#"{"preferred_prefix": "mine-"}"#);
        f.vars
            .insert("TEMPO_PREFERRED_PREFIX".into(), " Env- ".into());
        assert_eq!(preferred_prefix(&f, home), "env-");
        // anything that is not a plain prefix is ignored
        for bad in [
            r#"{"preferred_prefix": "a b"}"#,
            r#"{"preferred_prefix": 3}"#,
            "not json",
        ] {
            assert_eq!(preferred_prefix(&saved(bad), home), "", "{bad}");
        }
        assert!(
            valid_prefix("mine-")
                && valid_prefix("a.b_c-1")
                && !valid_prefix("")
                && !valid_prefix("Mine")
        );
        assert!(!valid_prefix(&"x".repeat(41)));
    }
}
