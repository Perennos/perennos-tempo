//! `tempo-launch setup --voice`: what it takes to talk to Tempo. whisper.cpp listens (speech in),
//! one ggml model fitting this computer is what it listens with, and Piper (`--piper`, optional)
//! speaks replies. Everything is a pinned download from `catalog/voice.json`, checked against its
//! SHA-256 before it is unpacked or used, after a y/N that shows the sizes. Nothing leaves the
//! computer once it is here: the agent runs both programs locally. Nothing is deleted (a file that
//! fails its check goes to the trash). An entry marked `"verified": false` is never fetched.

use super::json::{parse, J};
use super::probe::{self, Host, Machine, GIB};
use super::registry::Fetch;
use super::runtime::{download_limit_secs, to_trash};
use super::sha256::file_sha256;
use crate::{stamp, write_atomic, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const EMBEDDED: &str = include_str!("../../catalog/voice.json");
const UNPACK_LIMIT: Duration = Duration::from_secs(10 * 60);

/// One file we may fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
    pub verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Catalog {
    pub whisper_version: String,
    pub whisper: Vec<(String, Item)>,
    pub models: Vec<(String, Item)>,
    pub piper_version: String,
    pub piper: Vec<(String, Item)>,
    pub voice_name: String,
    /// The voice's model and its settings file, in that order.
    pub voice: Vec<Item>,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn item(key: &str, j: &J) -> Result<Item, String> {
    let file = j
        .str("file")
        .filter(|n| !n.is_empty() && !n.contains(['/', '\\', ':']) && !n.starts_with('.'))
        .ok_or_else(|| format!("{key}: no usable file name"))?;
    let url = j
        .str("url")
        .filter(|u| u.starts_with("https://"))
        .ok_or_else(|| format!("{key}: the address must be https"))?;
    let bytes = j
        .f64("bytes")
        .filter(|b| *b >= 1.0 && b.fract() == 0.0)
        .ok_or_else(|| format!("{key}: no byte count"))? as u64;
    let verified = j.bool("verified").unwrap_or(false);
    let sha = j.str("sha256").unwrap_or("");
    // a hash we cannot vouch for is never accepted as "verified"
    if verified && !is_hex64(sha) {
        return Err(format!(
            "{key}: marked verified, but the sha256 is not 64 lower-case hex characters"
        ));
    }
    Ok(Item {
        file: file.into(),
        url: url.into(),
        sha256: sha.into(),
        bytes,
        verified,
    })
}

fn items(j: &J, key: &str) -> Result<Vec<(String, Item)>, String> {
    let Some(J::Obj(files)) = j.get(key) else {
        return Err(format!("the voice list has no {key}"));
    };
    files
        .iter()
        .map(|(k, v)| item(k, v).map(|i| (k.clone(), i)))
        .collect()
}

pub fn parse_catalog(text: &str) -> Result<Catalog, String> {
    let j = parse(text)?;
    let w = j.get("whisper").ok_or("the voice list has no whisper")?;
    let m = j.get("models").ok_or("the voice list has no models")?;
    let p = j.get("piper").ok_or("the voice list has no piper")?;
    let v = p.get("voice").ok_or("the voice list has no piper voice")?;
    let voice = items(v, "files")?;
    Ok(Catalog {
        whisper_version: w.str("version").unwrap_or("").into(),
        whisper: items(w, "files")?,
        models: items(m, "files")?,
        piper_version: p.str("version").unwrap_or("").into(),
        piper: items(p, "files")?,
        voice_name: v.str("name").unwrap_or("").into(),
        voice: voice.into_iter().map(|(_, i)| i).collect(),
    })
}

impl Catalog {
    pub fn embedded() -> Catalog {
        parse_catalog(EMBEDDED).unwrap_or_default()
    }
}

fn os_key(os: &str, arch: &str) -> String {
    let a = match arch {
        "x86_64" | "amd64" => "x86_64",
        other => other,
    };
    format!("{os}-{a}")
}

/// English unless the system language says otherwise. Windows reports no such variable, so it is
/// taken as English; `--multilingual` is the way to say otherwise there.
pub fn system_is_english(host: &dyn Host) -> bool {
    for k in ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"] {
        if let Some(v) = host.env(k) {
            let v = v.to_lowercase();
            return v == "c" || v == "posix" || v.starts_with("en");
        }
    }
    true
}

/// The model that fits: tiny under 3 GB of memory, base under 8 GB, else small. `.en` ones are
/// faster and a little better in English; the plain ones speak many languages.
pub fn pick_model(m: &Machine, english: bool) -> String {
    let size = match m.ram_total_gb {
        Some(g) if g < 3.0 => "tiny",
        Some(g) if g < 8.0 => "base",
        Some(_) => "small",
        None => "base",
    };
    // there is no multilingual tiny pinned: base is the smallest that speaks everything
    match (size, english) {
        ("tiny", true) => "tiny.en".into(),
        ("tiny", false) => "base".into(),
        (s, true) => format!("{s}.en"),
        (s, false) => s.into(),
    }
}

/// The program that listens, if one is on PATH.
pub fn find_on_path(host: &dyn Host, names: &[&str]) -> Option<PathBuf> {
    let path = host.env("PATH")?;
    let sfx = if host.os() == "windows" { ".exe" } else { "" };
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .flat_map(|d| names.iter().map(move |n| d.join(format!("{n}{sfx}"))))
        .find(|p| p.is_file())
}

/// What `voice.json` in the Tempo home says is installed. The agent reads the same file.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Record {
    pub whisper: Option<PathBuf>,
    pub model: Option<PathBuf>,
    pub model_id: String,
    pub piper: Option<PathBuf>,
    pub piper_voice: Option<PathBuf>,
}

pub fn record_path(home: &Path) -> PathBuf {
    home.join("voice").join("voice.json")
}

pub fn read_record(home: &Path) -> Record {
    let Some(j) = std::fs::read_to_string(record_path(home))
        .ok()
        .and_then(|t| parse(&t).ok())
    else {
        return Record::default();
    };
    let p = |k: &str| j.str(k).filter(|s| !s.is_empty()).map(PathBuf::from);
    Record {
        whisper: p("whisper"),
        model: p("model"),
        model_id: j.str("model_id").unwrap_or("").into(),
        piper: p("piper"),
        piper_voice: p("piper_voice"),
    }
}

fn write_record(home: &Path, r: &Record, now: u64) -> Result<(), String> {
    let s = |p: &Option<PathBuf>| match p {
        Some(p) => J::s(&p.to_string_lossy()),
        None => J::Null,
    };
    let j = J::Obj(vec![
        ("whisper".into(), s(&r.whisper)),
        ("model".into(), s(&r.model)),
        ("model_id".into(), J::s(&r.model_id)),
        ("piper".into(), s(&r.piper)),
        ("piper_voice".into(), s(&r.piper_voice)),
        ("installed_at".into(), J::s(&stamp(now))),
    ]);
    write_atomic(&record_path(home), j.pretty().as_bytes())
        .map_err(|e| format!("could not write {}: {e}", record_path(home).display()))
}

fn mb(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GB", bytes as f64 / GIB)
    } else {
        format!("{} MB", bytes.div_ceil(1_000_000))
    }
}

/// The program and arguments that unpack a zip (Windows) or tar.gz into `dest`.
pub fn unpack_command(
    os: &str,
    system_root: Option<&str>,
    file: &Path,
    dest: &Path,
) -> (String, Vec<String>) {
    let zip = file.extension().is_some_and(|e| e == "zip");
    let (prog, flag) = if os == "windows" {
        let root = system_root.unwrap_or(r"C:\Windows").trim_end_matches('\\');
        (format!(r"{root}\System32\tar.exe"), "-xf")
    } else if zip {
        ("unzip".to_string(), "-q")
    } else if os == "macos" {
        ("/usr/bin/tar".to_string(), "-xzf")
    } else {
        ("tar".to_string(), "-xzf")
    };
    let mut args = vec![flag.to_string(), file.to_string_lossy().into_owned()];
    if zip && os != "windows" {
        args.push("-d".into());
    } else {
        args.push("-C".into());
    }
    args.push(dest.to_string_lossy().into_owned());
    (prog, args)
}

/// The nearest file with one of these names, up to five folders deep. Links are not followed.
pub fn find_named(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..5 {
        let mut next = Vec::new();
        for d in &level {
            let mut entries: Vec<_> = std::fs::read_dir(d).ok()?.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().to_lowercase();
                if ft.is_file() && names.contains(&name.as_str()) {
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

/// `dir/<file>` once its SHA-256 matches the pin: fetched (carrying on from a part), checked, and
/// a file that does not match goes to the trash.
fn fetch_checked(
    home: &Path,
    fetch: &dyn Fetch,
    dir: &Path,
    it: &Item,
    now: u64,
    say: &mut dyn FnMut(&str),
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let done = dir.join(&it.file);
    let part = dir.join(format!("{}.part", it.file));
    let sum = |p: &Path| {
        file_sha256(p, &mut |_| {}).map_err(|e| format!("could not read {}: {e}", p.display()))
    };
    if done.is_file() {
        if sum(&done)? == it.sha256 {
            say(&format!("{} is already here and checked.", it.file));
            return Ok(done);
        }
        let t = to_trash(home, &done, now)?;
        say(&format!(
            "An older {} did not match its SHA-256, so it was moved to {}.",
            it.file,
            t.display()
        ));
    }
    if std::fs::metadata(&part).map_or(0, |m| m.len()) > it.bytes.saturating_mul(2) {
        let t = to_trash(home, &part, now)?;
        say(&format!(
            "A leftover {}.part was far too big and moved to {}.",
            it.file,
            t.display()
        ));
    }
    say(&format!(
        "Fetching {} ({}) from {}",
        it.file,
        mb(it.bytes),
        it.url
    ));
    let mut tenth = 0;
    fetch
        .download(&it.url, &part, download_limit_secs(it.bytes), &mut |n| {
            let d = n.min(it.bytes) * 10 / it.bytes.max(1);
            if d > tenth {
                tenth = d;
                say(&format!("  {}%", d * 10));
            }
        })
        .map_err(|e| {
            format!(
                "Could not fetch {}: {e}. What arrived is kept, and running setup again carries on from there.",
                it.file
            )
        })?;
    let got = sum(&part)?;
    if got != it.sha256 {
        let t = to_trash(home, &part, now)?;
        return Err(format!(
            "The file fetched for {} did not match its pinned SHA-256 (wanted {}, got {got}), so Tempo refused it. It was moved to {} and nothing was unpacked or used.",
            it.file,
            it.sha256,
            t.display()
        ));
    }
    std::fs::rename(&part, &done)
        .map_err(|e| format!("could not rename {}: {e}", part.display()))?;
    say(&format!("{}: SHA-256 matches the pin.", it.file));
    Ok(done)
}

/// Unpack a checked archive into `<voice>/<name>` and return the program inside it.
#[allow(clippy::too_many_arguments)]
fn unpack_program(
    home: &Path,
    host: &dyn Host,
    archive: &Path,
    name: &str,
    names: &[&str],
    now: u64,
    say: &mut dyn FnMut(&str),
) -> Result<PathBuf, String> {
    let folder = home.join("voice").join(name);
    if folder.exists() {
        return find_named(&folder, names).ok_or_else(|| {
            format!(
                "{} is already there but holds no {}. Tempo never overwrites a folder: move it aside and run setup again.",
                folder.display(),
                names[0]
            )
        });
    }
    let partial = home.join("voice").join(format!("{name}.partial"));
    if partial.exists() {
        let t = to_trash(home, &partial, now)?;
        say(&format!(
            "A leftover unpack folder was moved to {}.",
            t.display()
        ));
    }
    std::fs::create_dir_all(&partial)
        .map_err(|e| format!("could not make {}: {e}", partial.display()))?;
    let (prog, args) = unpack_command(
        &host.os(),
        host.env("SystemRoot").as_deref(),
        archive,
        &partial,
    );
    say(&format!("Unpacking into {} ...", folder.display()));
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    host.run_tool(&prog, &argv, UNPACK_LIMIT)
        .map_err(|e| format!("Unpacking {} failed: {e}. The half-unpacked folder {} is left; the next run moves it to the trash first.", archive.display(), partial.display()))?;
    let found = find_named(&partial, names).ok_or_else(|| {
        format!(
            "{} unpacked, but no {} was inside. It is left in {}.",
            archive.display(),
            names[0],
            partial.display()
        )
    })?;
    let rel = found
        .strip_prefix(&partial)
        .map_err(|_| "program outside its folder")?
        .to_path_buf();
    make_executable(&found);
    std::fs::rename(&partial, &folder)
        .map_err(|e| format!("could not move the unpacked folder into place: {e}"))?;
    Ok(folder.join(rel))
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

/// What this run would do.
#[derive(Debug, Default)]
pub struct Plan {
    pub have: Record,
    /// (label, item, where it goes) in the order they are fetched.
    pub fetch: Vec<(String, Item)>,
    pub notes: Vec<String>,
    pub model_id: String,
    pub engine_on_path: Option<PathBuf>,
}

pub fn plan(
    cat: &Catalog,
    host: &dyn Host,
    machine: &Machine,
    home: &Path,
    piper: bool,
    multilingual: bool,
) -> Plan {
    let have = read_record(home);
    let mut p = Plan {
        have: have.clone(),
        ..Plan::default()
    };
    let exists = |o: &Option<PathBuf>| o.as_ref().is_some_and(|f| f.is_file());
    // the engine
    if exists(&have.whisper) {
        p.notes.push("whisper.cpp is already set up.".into());
    } else if let Some(bin) = find_on_path(host, &["whisper-cli", "whisper-cpp"]) {
        p.notes.push(format!(
            "Using the whisper.cpp already on this computer: {}",
            bin.display()
        ));
        p.engine_on_path = Some(bin);
    } else if let Some((_, it)) = cat
        .whisper
        .iter()
        .find(|(k, _)| *k == os_key(&host.os(), &host.arch()))
    {
        p.fetch.push(("whisper.cpp".into(), it.clone()));
    } else {
        p.notes.push(format!(
            "whisper.cpp publishes no ready-made program for {} on {}. Install it (macOS: brew install whisper-cpp; Linux: your package manager, or build it from github.com/ggml-org/whisper.cpp with cmake) so that whisper-cli is on your PATH, then run this again. The model below is fetched either way.",
            host.os(),
            host.arch()
        ));
    }
    // the model
    p.model_id = pick_model(machine, !multilingual && system_is_english(host));
    if exists(&have.model) && have.model_id == p.model_id {
        p.notes
            .push(format!("The {} model is already here.", p.model_id));
    } else if let Some((_, it)) = cat.models.iter().find(|(k, _)| *k == p.model_id) {
        p.fetch.push((format!("{} model", p.model_id), it.clone()));
    }
    // Piper
    if piper {
        if exists(&have.piper) {
            p.notes.push("Piper is already set up.".into());
        } else if let Some((_, it)) = cat
            .piper
            .iter()
            .find(|(k, _)| *k == os_key(&host.os(), &host.arch()))
        {
            p.fetch.push(("Piper".into(), it.clone()));
        } else {
            p.notes
                .push("Piper has no program for this computer, so replies stay text.".into());
        }
        if !exists(&have.piper_voice) {
            for (i, it) in cat.voice.iter().enumerate() {
                let what = if i == 0 { "voice" } else { "voice settings" };
                p.fetch
                    .push((format!("{} {what}", cat.voice_name), it.clone()));
            }
        }
    }
    p
}

/// `setup --voice`. Returns the exit code.
#[allow(clippy::too_many_arguments)]
pub fn run(
    ctx: &Ctx,
    host: &dyn Host,
    cat: &Catalog,
    fetch: Option<&dyn Fetch>,
    port: u16,
    now: u64,
    piper: bool,
    multilingual: bool,
    yes: bool,
    out: &mut dyn Write,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let mut say = |s: &str| {
        let _ = writeln!(out, "{s}");
    };
    if cat.models.is_empty() {
        say("The voice list that ships with this program is empty, so there is nothing to fetch.");
        return 2;
    }
    let machine = probe::probe(host, &ctx.home, port);
    let p = plan(cat, host, &machine, &ctx.home, piper, multilingual);
    say("Talk to Tempo: speech in, on this computer. Nothing you say leaves it.");
    for n in &p.notes {
        say(n);
    }
    if p.fetch.is_empty() {
        return finish(ctx, &p, None, None, None, None, now, &mut say);
    }
    let total: u64 = p.fetch.iter().map(|(_, i)| i.bytes).sum();
    say("To fetch:");
    for (label, it) in &p.fetch {
        say(&format!("  {label}: {} ({})", it.file, mb(it.bytes)));
    }
    say(&format!(
        "  about {} in all, into {}",
        mb(total),
        ctx.home.join("voice").display()
    ));
    let loose: Vec<&str> = p
        .fetch
        .iter()
        .filter(|(_, i)| !i.verified)
        .map(|(_, i)| i.file.as_str())
        .collect();
    if !loose.is_empty() {
        say(&format!(
            "\nNothing was fetched. These aren't pinned yet: {}. Tempo only fetches a file it can check against a SHA-256 the publisher's own release shows, and this list doesn't have those hashes yet. A maintainer fills them in with catalog/pin-voice.sh (see docs/TALK-TO-TEMPO.md); until then you can put whisper-cli and a ggml model in place yourself and point TEMPO_WHISPER_MODEL at the model.",
            loose.join(", ")
        ));
        return 3;
    }
    let Some(fetch) = fetch else {
        say("\nTempo needs curl to fetch these, and this computer has none. Install curl and run this again.");
        return 2;
    };
    if let Some(gb) = machine.disk_free_gb {
        if gb * GIB < total as f64 * 2.5 {
            say(&format!(
                "\nThis disk has {gb:.1} GB free and the fetch needs room to unpack, about {} . Free some space and run this again.",
                mb((total as f64 * 2.5) as u64)
            ));
            return 2;
        }
    }
    if !yes && !confirm("\nFetch these now? [y/N] ") {
        say("Nothing was fetched.");
        return 0;
    }
    let voice_dir = ctx.home.join("voice");
    let (mut whisper, mut model, mut piper_bin, mut voice) = (None, None, None, None);
    for (label, it) in &p.fetch {
        let dir = match label.as_str() {
            "whisper.cpp" | "Piper" => voice_dir.join("downloads"),
            l if l.ends_with(" model") => voice_dir.join("models"),
            _ => voice_dir.join("voices"),
        };
        let file = match fetch_checked(&ctx.home, fetch, &dir, it, now, &mut say) {
            Ok(f) => f,
            Err(e) => {
                say(&e);
                return 1;
            }
        };
        let res = match label.as_str() {
            "whisper.cpp" => unpack_program(
                &ctx.home,
                host,
                &file,
                &format!("whisper-{}", cat.whisper_version),
                &["whisper-cli", "whisper-cli.exe"],
                now,
                &mut say,
            )
            .map(|b| whisper = Some(b)),
            "Piper" => unpack_program(
                &ctx.home,
                host,
                &file,
                &format!("piper-{}", cat.piper_version),
                &["piper", "piper.exe"],
                now,
                &mut say,
            )
            .map(|b| piper_bin = Some(b)),
            l if l.ends_with(" model") => {
                model = Some(file);
                Ok(())
            }
            l if l.ends_with(" voice") => {
                voice = Some(file);
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(e) = res {
            say(&e);
            return 1;
        }
    }
    finish(ctx, &p, whisper, model, piper_bin, voice, now, &mut say)
}

#[allow(clippy::too_many_arguments)]
fn finish(
    ctx: &Ctx,
    p: &Plan,
    whisper: Option<PathBuf>,
    model: Option<PathBuf>,
    piper: Option<PathBuf>,
    voice: Option<PathBuf>,
    now: u64,
    say: &mut dyn FnMut(&str),
) -> i32 {
    let mut r = p.have.clone();
    r.whisper = whisper.or(p.engine_on_path.clone()).or(r.whisper);
    if let Some(m) = model {
        r.model = Some(m);
        r.model_id = p.model_id.clone();
    }
    r.piper = piper.or(r.piper);
    r.piper_voice = voice.or(r.piper_voice);
    if let Err(e) = write_record(&ctx.home, &r, now) {
        say(&e);
        return 1;
    }
    if r.whisper.is_some() && r.model.is_some() {
        say("Voice is ready. Hold the mic in the chat, or press Space with the box empty. Start the agent again if it was running.");
        0
    } else {
        say("Part of voice is still missing (see above). What's here is kept.");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::super::probe::tests::Fake;
    use super::super::runtime::tests::{tmp, FakeDl};
    use super::super::sha256::sha256_hex;
    use super::*;
    use std::cell::RefCell;

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

    fn pc(os: &'static str) -> Fake {
        Fake {
            os,
            arch: "x86_64",
            cpus: 8,
            win_mem: Some((16 << 30, 10 << 30)),
            win_disk: Some(50 << 30),
            ..Fake::default()
        }
    }

    fn pinned(body: &[u8], file: &str, url_tail: &str) -> Item {
        Item {
            file: file.into(),
            url: format!("https://example.test/{url_tail}"),
            sha256: sha256_hex(body),
            bytes: body.len() as u64,
            verified: true,
        }
    }

    fn test_catalog(body: &[u8]) -> Catalog {
        Catalog {
            whisper_version: "1.0.0".into(),
            whisper: vec![(
                "windows-x86_64".into(),
                pinned(body, "whisper-bin-x64.zip", "w.zip"),
            )],
            models: vec![
                ("tiny.en".into(), pinned(body, "ggml-tiny.en.bin", "t")),
                ("base.en".into(), pinned(body, "ggml-base.en.bin", "b")),
                ("small.en".into(), pinned(body, "ggml-small.en.bin", "s")),
                ("base".into(), pinned(body, "ggml-base.bin", "bm")),
                ("small".into(), pinned(body, "ggml-small.bin", "sm")),
            ],
            piper_version: "9".into(),
            piper: vec![(
                "windows-x86_64".into(),
                pinned(body, "piper_windows_amd64.zip", "p.zip"),
            )],
            voice_name: "test-voice".into(),
            voice: vec![
                pinned(body, "test-voice.onnx", "v.onnx"),
                pinned(body, "test-voice.onnx.json", "v.json"),
            ],
        }
    }

    #[test]
    fn the_catalog_that_ships_parses_and_hides_no_invented_hash() {
        let c = parse_catalog(EMBEDDED).unwrap();
        assert!(c.models.len() >= 4 && !c.whisper.is_empty() && c.voice.len() == 2);
        // every entry is either verified with a real-looking hash, or says so and has none
        for i in c
            .whisper
            .iter()
            .map(|x| &x.1)
            .chain(c.models.iter().map(|x| &x.1))
            .chain(c.piper.iter().map(|x| &x.1))
            .chain(c.voice.iter())
        {
            assert!(i.verified == is_hex64(&i.sha256), "{}", i.file);
            assert!(i.url.starts_with("https://"));
        }
    }

    #[test]
    fn a_verified_entry_needs_a_real_hash() {
        let j = r#"{"file":"a.bin","url":"https://x.test/a.bin","sha256":"abc","bytes":3,"verified":true}"#;
        assert!(item("a", &parse(j).unwrap()).is_err());
    }

    #[test]
    fn model_fits_the_machine_and_the_language() {
        let mut m = probe::probe(&pc("windows"), Path::new("/nonexistent"), 1);
        m.ram_total_gb = Some(2.0);
        assert_eq!(pick_model(&m, true), "tiny.en");
        assert_eq!(pick_model(&m, false), "base");
        m.ram_total_gb = Some(6.0);
        assert_eq!(pick_model(&m, true), "base.en");
        m.ram_total_gb = Some(16.0);
        assert_eq!(pick_model(&m, true), "small.en");
        assert_eq!(pick_model(&m, false), "small");
        let mut h = pc("linux");
        assert!(system_is_english(&h));
        h.vars.insert("LANG".into(), "de_DE.UTF-8".into());
        assert!(!system_is_english(&h));
        h.vars.insert("LANG".into(), "en_GB.UTF-8".into());
        assert!(system_is_english(&h));
    }

    #[test]
    fn unverified_pins_are_never_fetched() {
        let root = tmp("voice-unverified");
        let c = ctx(&root);
        let dl = FakeDl::new();
        let mut cat = test_catalog(b"x");
        cat.models[2].1.verified = false;
        cat.models[2].1.sha256 = String::new();
        let mut out = Vec::new();
        let code = run(
            &c,
            &pc("windows"),
            &cat,
            Some(&dl),
            1,
            5,
            false,
            false,
            true,
            &mut out,
            &mut |_| true,
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 3, "{text}");
        assert_eq!(dl.count(), 0);
        assert!(
            text.contains("aren't pinned yet") && text.contains("ggml-small.en.bin"),
            "{text}"
        );
    }

    #[test]
    fn windows_fetches_checks_unpacks_and_records() {
        let root = tmp("voice-win");
        let c = ctx(&root);
        let body = b"pretend archive or model";
        let dl = FakeDl {
            body: body.to_vec(),
            ..FakeDl::new()
        };
        let mut host = pc("windows");
        host.tar_makes = Some("Release/whisper-cli.exe");
        let cat = test_catalog(body);
        let mut out = Vec::new();
        let code = run(
            &c,
            &host,
            &cat,
            Some(&dl),
            1,
            5,
            false,
            false,
            true,
            &mut out,
            &mut |_| false,
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{text}");
        assert_eq!(dl.count(), 2, "engine and model: {text}");
        let r = read_record(&c.home);
        assert!(r.whisper.unwrap().ends_with("Release/whisper-cli.exe"));
        assert!(r.model.unwrap().ends_with("models/ggml-small.en.bin"));
        assert!(r.piper.is_none());
        // a second run has nothing to fetch
        let mut out = Vec::new();
        let code = run(
            &c,
            &host,
            &cat,
            Some(&dl),
            1,
            6,
            false,
            false,
            true,
            &mut out,
            &mut |_| false,
        );
        assert_eq!(code, 0);
        assert_eq!(dl.count(), 2, "nothing fetched twice");
    }

    #[test]
    fn it_asks_first_and_no_means_nothing() {
        let root = tmp("voice-ask");
        let c = ctx(&root);
        let body = b"abc";
        let dl = FakeDl {
            body: body.to_vec(),
            ..FakeDl::new()
        };
        let mut out = Vec::new();
        let mut asked = String::new();
        let code = run(
            &c,
            &pc("windows"),
            &test_catalog(body),
            Some(&dl),
            1,
            5,
            true,
            false,
            false,
            &mut out,
            &mut |q| {
                asked = q.into();
                false
            },
        );
        assert_eq!(code, 0);
        assert!(asked.contains("[y/N]"));
        assert_eq!(dl.count(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("ggml-small.en.bin")
                && text.contains("Piper")
                && text.contains("test-voice"),
            "{text}"
        );
        assert!(!record_path(&c.home).exists());
    }

    #[test]
    fn a_wrong_hash_is_refused_and_trashed() {
        let root = tmp("voice-bad");
        let c = ctx(&root);
        let dl = FakeDl {
            body: b"tampered".to_vec(),
            ..FakeDl::new()
        };
        let mut host = pc("windows");
        host.tar_makes = Some("whisper-cli.exe");
        let cat = test_catalog(b"the real thing");
        let mut out = Vec::new();
        let code = run(
            &c,
            &host,
            &cat,
            Some(&dl),
            1,
            5,
            false,
            false,
            true,
            &mut out,
            &mut |_| true,
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 1);
        assert!(text.contains("did not match its pinned SHA-256"), "{text}");
        assert!(host.tools.lock().unwrap().is_empty(), "nothing unpacked");
        assert!(!record_path(&c.home).exists());
        assert!(c.home.join("trash").exists());
    }

    #[test]
    fn linux_uses_the_whisper_cli_it_finds_and_fetches_only_the_model() {
        let root = tmp("voice-linux");
        let c = ctx(&root);
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("whisper-cli"), "#!/bin/sh\n").unwrap();
        let body = b"model";
        let dl = FakeDl {
            body: body.to_vec(),
            ..FakeDl::new()
        };
        let mut host = pc("linux");
        host.vars
            .insert("PATH".into(), bin_dir.to_string_lossy().into());
        host.vars.insert("LANG".into(), "fr_FR.UTF-8".into());
        host.files.insert(
            "/proc/meminfo".into(),
            "MemTotal: 16384000 kB\nMemAvailable: 9000000 kB\n".into(),
        );
        let mut out = Vec::new();
        let code = run(
            &c,
            &host,
            &test_catalog(body),
            Some(&dl),
            1,
            5,
            false,
            false,
            true,
            &mut out,
            &mut |_| true,
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{text}");
        assert_eq!(dl.count(), 1);
        let r = read_record(&c.home);
        assert_eq!(r.whisper.unwrap(), bin_dir.join("whisper-cli"));
        assert_eq!(
            r.model_id, "small",
            "French system, plenty of memory: multilingual small"
        );
    }

    #[test]
    fn linux_without_whisper_says_how_to_get_it() {
        let root = tmp("voice-linux2");
        let c = ctx(&root);
        let body = b"m";
        let dl = FakeDl {
            body: body.to_vec(),
            ..FakeDl::new()
        };
        let mut out = Vec::new();
        let code = run(
            &c,
            &pc("linux"),
            &test_catalog(body),
            Some(&dl),
            1,
            5,
            false,
            false,
            true,
            &mut out,
            &mut |_| true,
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 1);
        assert!(
            text.contains("brew install whisper-cpp") && text.contains("publishes no ready-made"),
            "{text}"
        );
    }

    #[test]
    fn unpack_commands_per_os() {
        let (p, a) = unpack_command(
            "windows",
            Some(r"C:\Windows"),
            Path::new("a.zip"),
            Path::new("d"),
        );
        assert!(p.ends_with("tar.exe") && a[0] == "-xf");
        let (p, a) = unpack_command("linux", None, Path::new("a.tar.gz"), Path::new("d"));
        assert_eq!(
            (p.as_str(), a[0].as_str(), a[2].as_str()),
            ("tar", "-xzf", "-C")
        );
        let (p, a) = unpack_command("macos", None, Path::new("a.tar.gz"), Path::new("d"));
        assert_eq!(p, "/usr/bin/tar");
        assert_eq!(a[0], "-xzf");
    }
}
