//! Mix (the faders): llama-server with several LoRA adapters loaded on one base, and a scale for
//! each adapter per band or per part, moved live without reloading the base.
//!
//! What was checked against llama.cpp's `tools/server/README.md` (the master copy, 7 Nov 2026):
//! `--lora FNAME` (repeatable, or comma separated), `--lora-init-without-apply` (load the adapters
//! at scale 0), `GET /lora-adapters` answering `[{"id":0,"path":"...","scale":0.0}]`, `POST
//! /lora-adapters` taking `[{"id":0,"scale":0.2},...]` for the global scales, and a per-request
//! `"lora": [{"id":0,"scale":0.5}]` that overrides the global ones (an adapter left out of the list
//! runs at 0). The README documents `lora` under `/completion`; for `/v1/chat/completions` it says
//! the `/completion` options also work, which was not run against a real llama-server here.
//!
//! The ids are the order of the `--lora` flags, so adapters are only ever added at the end.
//! The agent reads `<agent data>/mix/faders.json` at every request; every move is also logged, one
//! line each, to `faders.log` beside it.

use crate::conductor::net::request;
use crate::setup::json::{parse, J};
use crate::{stamp, write_atomic, Ctx};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 8081;

/// Starts a program: its command, and the file its words go to.
pub type Spawn<'a> = &'a dyn Fn(&[String], &std::path::Path) -> Result<(), String>;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Mix {
    /// The base model, a GGUF file.
    pub base: String,
    pub port: u16,
    /// The llama-server program, when it is not next to tempo-launch (see `TEMPO_BIN`).
    pub server: String,
    /// (name, adapter .gguf), in the order that gives their ids.
    pub adapters: Vec<(String, String)>,
}

fn file(ctx: &Ctx) -> PathBuf {
    ctx.home.join("woodshed").join("mix.json")
}

fn faders_file(ctx: &Ctx) -> PathBuf {
    ctx.agent_data.join("mix").join("faders.json")
}

pub fn load(ctx: &Ctx) -> Mix {
    let j = std::fs::read_to_string(file(ctx))
        .ok()
        .and_then(|t| parse(&t).ok())
        .unwrap_or(J::Null);
    Mix {
        base: j.str("base").unwrap_or("").to_string(),
        port: j.f64("port").map_or(DEFAULT_PORT, |p| p as u16).max(1),
        server: j.str("server").unwrap_or("").to_string(),
        adapters: j
            .arr("adapters")
            .iter()
            .filter_map(|a| Some((a.str("name")?.to_string(), a.str("path")?.to_string())))
            .collect(),
    }
}

fn save(ctx: &Ctx, m: &Mix) -> Result<(), String> {
    let j = J::Obj(vec![
        ("base".into(), J::s(&m.base)),
        ("port".into(), J::n(f64::from(m.port))),
        ("server".into(), J::s(&m.server)),
        (
            "adapters".into(),
            J::Arr(
                m.adapters
                    .iter()
                    .map(|(n, p)| J::Obj(vec![("name".into(), J::s(n)), ("path".into(), J::s(p))]))
                    .collect(),
            ),
        ),
    ]);
    write_atomic(&file(ctx), j.pretty().as_bytes())
        .map_err(|e| format!("could not write {}: {e}", file(ctx).display()))
}

/// The command that starts llama-server. It listens on this computer only.
pub fn command(ctx: &Ctx, m: &Mix) -> Result<Vec<String>, String> {
    if m.base.is_empty() {
        return Err(
            "no base model yet: `tempo-launch woodshed mix base /path/to/model.gguf`".into(),
        );
    }
    if m.adapters.is_empty() {
        return Err("no adapters yet: `tempo-launch woodshed mix add <band>` adds a band's promoted adapter".into());
    }
    let program = if !m.server.is_empty() {
        m.server.clone()
    } else {
        ctx.find("llama-server").map_or_else(
            || "llama-server".to_string(),
            |p| p.to_string_lossy().into_owned(),
        )
    };
    let mut c = vec![
        program,
        "-m".into(),
        m.base.clone(),
        "--lora-init-without-apply".into(),
    ];
    for (_, path) in &m.adapters {
        c.push("--lora".into());
        c.push(path.clone());
    }
    c.extend([
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        m.port.to_string(),
    ]);
    Ok(c)
}

fn target_ok(t: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && s.len() <= 40
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    };
    t == "*"
        || match t.split_once('/') {
            Some((b, p)) => part(b) && part(p),
            None => part(t),
        }
}

/// `name=scale` pairs, as `[{"id":0,"scale":0.7},...]` with every adapter named: the ones left
/// out are set to 0.0, because that is what llama-server does with them anyway.
pub fn scales(m: &Mix, pairs: &[(String, f64)]) -> Result<J, String> {
    for (n, s) in pairs {
        if !m.adapters.iter().any(|(a, _)| a == n) {
            let names: Vec<&str> = m.adapters.iter().map(|(a, _)| a.as_str()).collect();
            return Err(format!(
                "no adapter called {n}; the adapters are {}",
                if names.is_empty() {
                    "none yet".to_string()
                } else {
                    names.join(", ")
                }
            ));
        }
        if !(0.0..=2.0).contains(s) {
            return Err(format!(
                "{n}={s}: a fader goes from 0 to 2 (1 is the adapter as trained)"
            ));
        }
    }
    Ok(J::Arr(
        m.adapters
            .iter()
            .enumerate()
            .map(|(id, (n, _))| {
                let s = pairs.iter().find(|(p, _)| p == n).map_or(0.0, |(_, s)| *s);
                J::Obj(vec![
                    ("id".into(), J::n(id as f64)),
                    ("scale".into(), J::n(s)),
                ])
            })
            .collect(),
    ))
}

/// Move the faders for a band, a part (`band/part`) or everything (`*`). Heard on the next answer.
pub fn set(ctx: &Ctx, target: &str, pairs: &[(String, f64)], now: u64, out: &mut dyn Write) -> i32 {
    if !target_ok(target) {
        let _ = writeln!(out, "\"{target}\" is not a band, a band/part or *.");
        return 2;
    }
    let m = load(ctx);
    let s = match scales(&m, pairs) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 2;
        }
    };
    let mut all = std::fs::read_to_string(faders_file(ctx))
        .ok()
        .and_then(|t| parse(&t).ok())
        .filter(|j| matches!(j, J::Obj(_)))
        .unwrap_or(J::Obj(vec![]));
    all.set(target, s.clone());
    if let Err(e) = write_atomic(&faders_file(ctx), all.pretty().as_bytes()) {
        let _ = writeln!(out, "could not write the faders: {e}");
        return 1;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ctx.agent_data.join("mix").join("faders.log"))
    {
        let _ = writeln!(
            f,
            "{}",
            J::Obj(vec![
                ("at".into(), J::s(&stamp(now))),
                ("target".into(), J::s(target)),
                ("scales".into(), s)
            ])
            .compact()
        );
    }
    let words: Vec<String> = m
        .adapters
        .iter()
        .map(|(n, _)| {
            format!(
                "{n} {}",
                pairs.iter().find(|(p, _)| p == n).map_or(0.0, |(_, s)| *s)
            )
        })
        .collect();
    let _ = writeln!(
        out,
        "Faders for {target}: {}. Heard on the next answer; nothing was reloaded.",
        words.join(", ")
    );
    0
}

/// Register an adapter at the end of the list (the ids are the order, so nothing is reordered).
pub fn add(ctx: &Ctx, name: &str, path: &str, out: &mut dyn Write) -> i32 {
    let mut m = load(ctx);
    if !target_ok(name) || name == "*" || name.contains('/') {
        let _ = writeln!(
            out,
            "\"{name}\" is not a name for an adapter (lowercase letters, digits, - and _)."
        );
        return 2;
    }
    if m.adapters.iter().any(|(n, _)| n == name) {
        let _ = writeln!(out, "{name} is already adapter {}. Its place never changes, so a new version goes in under a new name.", m.adapters.iter().position(|(n, _)| n == name).unwrap_or(0));
        return 1;
    }
    if !std::path::Path::new(path).is_file() {
        let _ = writeln!(out, "{path} is not a file. An adapter here is a .gguf from `convert_lora_to_gguf.py` (a promoted version has one).");
        return 1;
    }
    m.adapters.push((name.to_string(), path.to_string()));
    if let Err(e) = save(ctx, &m) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = writeln!(out, "Added {name} as adapter {}. llama-server picks up a new adapter when it starts again: `tempo-launch woodshed mix start`.", m.adapters.len() - 1);
    0
}

pub fn set_base(ctx: &Ctx, path: &str, out: &mut dyn Write) -> i32 {
    if !std::path::Path::new(path).is_file() || !path.to_ascii_lowercase().ends_with(".gguf") {
        let _ = writeln!(out, "{path} is not a .gguf file.");
        return 1;
    }
    let mut m = load(ctx);
    m.base = path.to_string();
    match save(ctx, &m) {
        Ok(()) => {
            let _ = writeln!(out, "Base model: {path}");
            0
        }
        Err(e) => {
            let _ = writeln!(out, "{e}");
            1
        }
    }
}

/// `GET /lora-adapters`, and whether the ids and paths are the ones in `mix.json`.
pub fn check(ctx: &Ctx, out: &mut dyn Write) -> i32 {
    let m = load(ctx);
    let (code, text) = match request(
        m.port,
        "GET",
        "/lora-adapters",
        &[],
        None,
        Duration::from_secs(5),
    ) {
        Ok(x) => x,
        Err(e) => {
            let _ = writeln!(out, "llama-server is not answering on port {} ({e}). `tempo-launch woodshed mix start` starts it.", m.port);
            return 1;
        }
    };
    let j = parse(&text).unwrap_or(J::Null);
    let list = match (&j, code) {
        (J::Arr(a), 200) => a.clone(),
        _ => {
            let _ = writeln!(out, "llama-server answered {code} to /lora-adapters, and not with a list. It needs to be started with --lora.");
            return 1;
        }
    };
    let mut bad = false;
    for (id, (name, path)) in m.adapters.iter().enumerate() {
        let theirs = list.iter().find(|a| a.f64("id") == Some(id as f64));
        let same = theirs
            .and_then(|a| a.str("path"))
            .is_some_and(|p| p == path);
        if !same {
            bad = true;
        }
        let _ = writeln!(
            out,
            "  {id}  {name}  {}  global scale {}",
            if same {
                "loaded"
            } else {
                "NOT what mix.json says: start llama-server again"
            },
            theirs
                .and_then(|a| a.f64("scale"))
                .map_or("?".to_string(), |s| s.to_string())
        );
    }
    if list.len() > m.adapters.len() {
        bad = true;
        let _ = writeln!(
            out,
            "  llama-server has {} adapters loaded and mix.json lists {}.",
            list.len(),
            m.adapters.len()
        );
    }
    i32::from(bad)
}

/// `POST /lora-adapters`: the scales used when a request has no `lora` of its own.
pub fn global(ctx: &Ctx, pairs: &[(String, f64)], out: &mut dyn Write) -> i32 {
    let m = load(ctx);
    let body = match scales(&m, pairs) {
        Ok(b) => b.compact(),
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 2;
        }
    };
    match request(
        m.port,
        "POST",
        "/lora-adapters",
        &[],
        Some(&body),
        Duration::from_secs(5),
    ) {
        Ok((200, _)) => {
            let _ = writeln!(out, "Global scales set: {body}");
            0
        }
        Ok((c, t)) => {
            let _ = writeln!(
                out,
                "llama-server answered {c}: {}",
                t.chars().take(160).collect::<String>()
            );
            1
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "llama-server is not answering on port {} ({e}).",
                m.port
            );
            1
        }
    }
}

/// Put the server in the agent's `llama_servers`, so a part with faders plays on it.
fn tell_agent(ctx: &Ctx, port: u16, now: u64) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}");
    if let Some(pinned) = std::env::var_os("TEMPO_AGENT_CONFIG").filter(|v| !v.is_empty()) {
        return Ok(format!("TEMPO_AGENT_CONFIG names {}, which tempo-launch leaves alone. Add \"llama_servers\": [\"{url}\"] to it yourself.", std::path::Path::new(&pinned).display()));
    }
    let path = crate::setup::apply::agent_settings_path(&ctx.agent_data);
    let (cfg, old) = crate::setup::propose::load_settings(&path)?;
    let mut list = cfg.strs("llama_servers");
    if list.contains(&url) {
        return Ok(String::new());
    }
    list.push(url.clone());
    crate::setup::propose::store(
        ctx,
        &path,
        cfg,
        old,
        &[("llama_servers".to_string(), J::strings(&list))],
        now,
    )?;
    Ok(format!("Added {url} to the agent's llama_servers (the old settings are in the trash folder). The agent reads it when it starts."))
}

/// `mix start`: show the command, ask, start llama-server on this computer and wait for it.
pub fn start(
    ctx: &Ctx,
    yes: bool,
    now: u64,
    spawn: Spawn,
    wait: Duration,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let m = load(ctx);
    let cmd = match command(ctx, &m) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    let _ = writeln!(out, "llama-server runs on this computer only (127.0.0.1:{}), loading {} adapter{} at scale 0:\n  {}", m.port, m.adapters.len(), if m.adapters.len() == 1 { "" } else { "s" }, super::script::shown(&cmd));
    if crate::http_get(m.port, "/health", Duration::from_secs(1)).is_some() {
        let _ = writeln!(
            out,
            "Something already answers on port {}. Stop it first, or change the port in {}.",
            m.port,
            file(ctx).display()
        );
        return 1;
    }
    if !(yes || ask("Start it? [y/N] ")) {
        let _ = writeln!(out, "Not started.");
        return 0;
    }
    let log = ctx.log_path("llama-server");
    if let Err(e) = spawn(&cmd, &log) {
        let _ = writeln!(out, "Could not start llama-server: {e}\nIf it is not installed, llama.cpp's releases have it; put it next to tempo-launch or set \"server\" in {}.", file(ctx).display());
        return 1;
    }
    let t0 = std::time::Instant::now();
    while t0.elapsed() < wait {
        if let Some((200, _)) = crate::http_get(m.port, "/health", Duration::from_secs(2)) {
            let _ = writeln!(out, "llama-server is up.");
            let _ = check(ctx, out);
            match tell_agent(ctx, m.port, now) {
                Ok(t) if !t.is_empty() => {
                    let _ = writeln!(out, "{t}");
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = writeln!(out, "Could not tell the agent: {e}");
                }
            }
            return 0;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = writeln!(
        out,
        "llama-server did not answer in {} seconds. Its words are in {}.",
        wait.as_secs(),
        log.display()
    );
    1
}

/// The real thing: llama-server, hidden, its words in a log.
pub fn real_spawn(cmd: &[String], log: &std::path::Path) -> Result<(), String> {
    let f = crate::open_log(log).map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stdout(f.try_clone().map_err(|e| e.to_string())?)
        .stderr(f);
    crate::spawn_hidden(&mut c)
        .map(|_| ())
        .map_err(|e| format!("{} ({e})", cmd[0]))
}

/// `mix` and `mix show`: what is set up and where the faders are.
pub fn show(ctx: &Ctx, out: &mut dyn Write) -> i32 {
    let m = load(ctx);
    if m.base.is_empty() && m.adapters.is_empty() {
        let _ = writeln!(out, "The Mix is empty. Set it up in three steps:\n  tempo-launch woodshed mix base /path/to/base.gguf\n  tempo-launch woodshed mix add <band>      (a band's promoted adapter; add each band you want on the faders)\n  tempo-launch woodshed mix start\nThen move a fader with: tempo-launch woodshed mix set <band>[/<part>] <adapter>=<0 to 2> ...");
        return 0;
    }
    let _ = writeln!(
        out,
        "Mix: base {}",
        if m.base.is_empty() {
            "(not set)"
        } else {
            &m.base
        }
    );
    for (i, (n, p)) in m.adapters.iter().enumerate() {
        let _ = writeln!(out, "  adapter {i}: {n}  {p}");
    }
    match command(ctx, &m) {
        Ok(c) => {
            let _ = writeln!(out, "\nStart: {}", super::script::shown(&c));
        }
        Err(e) => {
            let _ = writeln!(out, "\n{e}");
        }
    }
    match std::fs::read_to_string(faders_file(ctx))
        .ok()
        .and_then(|t| parse(&t).ok())
    {
        Some(J::Obj(f)) if !f.is_empty() => {
            let _ = writeln!(out, "\nFaders now (read at every request):");
            for (target, v) in f {
                let line: Vec<String> = match &v {
                    J::Arr(a) => a
                        .iter()
                        .map(|e| {
                            let id = e.f64("id").unwrap_or(0.0) as usize;
                            format!(
                                "{} {}",
                                m.adapters.get(id).map_or("?", |a| a.0.as_str()),
                                e.f64("scale").unwrap_or(0.0)
                            )
                        })
                        .collect(),
                    _ => vec!["(not a list of scales)".into()],
                };
                let _ = writeln!(out, "  {target}: {}", line.join(", "));
            }
        }
        _ => {
            let _ = writeln!(out, "\nNo faders moved yet.");
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::super::takes::test_ctx;
    use super::*;
    use std::io::Read;

    fn mixed(tag: &str) -> Ctx {
        let ctx = test_ctx(tag);
        std::fs::create_dir_all(ctx.home.join("woodshed")).unwrap();
        for f in ["base.gguf", "a.gguf", "b.gguf"] {
            std::fs::write(ctx.home.join("woodshed").join(f), "x").unwrap();
        }
        let p = |f: &str| {
            ctx.home
                .join("woodshed")
                .join(f)
                .to_string_lossy()
                .into_owned()
        };
        let mut out = Vec::new();
        assert_eq!(set_base(&ctx, &p("base.gguf"), &mut out), 0);
        assert_eq!(add(&ctx, "social-media", &p("a.gguf"), &mut out), 0);
        assert_eq!(add(&ctx, "dev-team", &p("b.gguf"), &mut out), 0);
        ctx
    }

    fn said(f: impl FnOnce(&mut Vec<u8>) -> i32) -> (i32, String) {
        let mut o = Vec::new();
        let c = f(&mut o);
        (c, String::from_utf8(o).unwrap())
    }

    #[test]
    fn the_server_starts_with_the_adapters_loaded_at_zero_on_this_computer_only() {
        let ctx = mixed("m-cmd");
        let c = command(&ctx, &load(&ctx)).unwrap();
        let line = c.join(" ");
        assert!(
            line.starts_with("llama-server -m ")
                && line.contains("base.gguf --lora-init-without-apply --lora "),
            "{line}"
        );
        assert_eq!(c.iter().filter(|x| *x == "--lora").count(), 2);
        assert!(line.ends_with("--host 127.0.0.1 --port 8081"), "{line}");
        // adapters keep their place: the order is the id
        let m = load(&ctx);
        assert_eq!(m.adapters[0].0, "social-media");
        assert!(
            c.iter().position(|x| x.ends_with("a.gguf")).unwrap()
                < c.iter().position(|x| x.ends_with("b.gguf")).unwrap()
        );
        // an empty mix says what is missing
        assert!(command(&test_ctx("m-empty"), &Mix::default())
            .unwrap_err()
            .contains("no base model"));
        let mut only_base = Mix {
            base: "x.gguf".into(),
            ..Mix::default()
        };
        assert!(command(&ctx, &only_base)
            .unwrap_err()
            .contains("no adapters"));
        only_base.adapters.push(("a".into(), "a.gguf".into()));
        only_base.server = "/opt/llama/llama-server".into();
        assert_eq!(
            command(&ctx, &only_base).unwrap()[0],
            "/opt/llama/llama-server"
        );
    }

    #[test]
    fn a_fader_move_names_every_adapter_and_is_logged() {
        let ctx = mixed("m-set");
        let (code, text) = said(|o| {
            set(
                &ctx,
                "social-media/lead",
                &[("social-media".into(), 0.7)],
                1_794_000_000,
                o,
            )
        });
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("social-media 0.7, dev-team 0") && text.contains("nothing was reloaded"),
            "{text}"
        );
        let f = parse(&std::fs::read_to_string(faders_file(&ctx)).unwrap()).unwrap();
        // exactly what llama-server takes per request, and what the agent reads
        assert_eq!(
            f.get("social-media/lead").unwrap().compact(),
            r#"[{"id":0,"scale":0.7},{"id":1,"scale":0}]"#
        );
        set(
            &ctx,
            "*",
            &[("dev-team".into(), 1.0)],
            1_794_000_001,
            &mut Vec::new(),
        );
        set(
            &ctx,
            "social-media/lead",
            &[("social-media".into(), 0.2), ("dev-team".into(), 0.5)],
            1_794_000_002,
            &mut Vec::new(),
        );
        let f = parse(&std::fs::read_to_string(faders_file(&ctx)).unwrap()).unwrap();
        assert_eq!(
            f.get("social-media/lead").unwrap().compact(),
            r#"[{"id":0,"scale":0.2},{"id":1,"scale":0.5}]"#
        );
        assert!(f.get("*").is_some(), "other targets are kept");
        let log = std::fs::read_to_string(ctx.agent_data.join("mix/faders.log")).unwrap();
        assert_eq!(log.lines().count(), 3, "every move is a line");
        // what the agent would read back
        let (_, show_text) = said(|o| show(&ctx, o));
        assert!(
            show_text.contains("social-media/lead: social-media 0.2, dev-team 0.5"),
            "{show_text}"
        );
    }

    #[test]
    fn nonsense_is_refused_and_nothing_moves() {
        let ctx = mixed("m-bad");
        for (target, pairs) in [
            ("../x", vec![("social-media".to_string(), 1.0)]),
            ("b", vec![("nope".to_string(), 1.0)]),
            ("b", vec![("social-media".to_string(), 3.0)]),
            ("b", vec![("social-media".to_string(), -0.1)]),
        ] {
            let (code, _) = said(|o| set(&ctx, target, &pairs, 1, o));
            assert_eq!(code, 2, "{target} {pairs:?}");
        }
        assert!(!faders_file(&ctx).exists());
        // an adapter is never added twice, nor from a file that is not there
        let (code, text) = said(|o| add(&ctx, "social-media", "x", o));
        assert_eq!(code, 1);
        assert!(text.contains("already adapter 0"), "{text}");
        assert_eq!(said(|o| add(&ctx, "new-one", "/no/such/file.gguf", o)).0, 1);
    }

    /// A stand-in for llama-server's two lora endpoints.
    fn fake_llama(adapters: &str) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let (s2, body) = (seen.clone(), adapters.to_string());
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                s2.lock().unwrap().push(req.clone());
                let out = if req.starts_with("GET /health") {
                    "{\"status\":\"ok\"}".to_string()
                } else if req.starts_with("GET /lora-adapters") {
                    body.clone()
                } else {
                    "{\"success\":true}".to_string()
                };
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
            }
        });
        (port, seen)
    }

    fn with_port(ctx: &Ctx, port: u16) {
        let mut m = load(ctx);
        m.port = port;
        save(ctx, &m).unwrap();
    }

    #[test]
    fn check_compares_the_servers_ids_and_paths_with_mix_json() {
        let ctx = mixed("m-check");
        let m = load(&ctx);
        let body = format!(
            "[{{\"id\":0,\"path\":{},\"scale\":0.0}},{{\"id\":1,\"path\":{},\"scale\":0.0}}]",
            J::s(&m.adapters[0].1).compact(),
            J::s(&m.adapters[1].1).compact()
        );
        let (port, _) = fake_llama(&body);
        with_port(&ctx, port);
        let (code, text) = said(|o| check(&ctx, o));
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("0  social-media  loaded") && text.contains("1  dev-team  loaded"),
            "{text}"
        );
        // a server that loaded something else
        let (port, _) = fake_llama("[{\"id\":0,\"path\":\"/other.gguf\",\"scale\":1.0}]");
        with_port(&ctx, port);
        let (code, text) = said(|o| check(&ctx, o));
        assert_eq!(code, 1);
        assert!(text.contains("NOT what mix.json says"), "{text}");
        // nothing there
        with_port(&ctx, 9);
        assert!(said(|o| check(&ctx, o)).1.contains("not answering"));
    }

    #[test]
    fn global_scales_go_to_post_lora_adapters_in_the_documented_shape() {
        let ctx = mixed("m-global");
        let (port, seen) = fake_llama("[]");
        with_port(&ctx, port);
        let (code, _) = said(|o| global(&ctx, &[("dev-team".into(), 0.5)], o));
        assert_eq!(code, 0);
        let req = seen.lock().unwrap().last().unwrap().clone();
        assert!(req.starts_with("POST /lora-adapters"), "{req}");
        assert!(
            req.ends_with("[{\"id\":0,\"scale\":0},{\"id\":1,\"scale\":0.5}]"),
            "{req}"
        );
    }

    #[test]
    fn start_shows_the_command_asks_starts_waits_and_tells_the_agent() {
        let ctx = mixed("m-start");
        let m = load(&ctx);
        let body = format!(
            "[{{\"id\":0,\"path\":{},\"scale\":0.0}},{{\"id\":1,\"path\":{},\"scale\":0.0}}]",
            J::s(&m.adapters[0].1).compact(),
            J::s(&m.adapters[1].1).compact()
        );
        let (port, _) = fake_llama(&body);
        // the stand-in is already "up" when spawn is called; free the port check by using a second port
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        with_port(&ctx, free);
        let started = std::cell::RefCell::new(vec![]);
        let spawn = |c: &[String], _log: &std::path::Path| -> Result<(), String> {
            started.borrow_mut().push(c.join(" "));
            // the "server" appears on the port as the real one would
            let l = std::net::TcpListener::bind(("127.0.0.1", free)).map_err(|e| e.to_string())?;
            let body = body.clone();
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    let mut s = s;
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let out = if req.starts_with("GET /lora-adapters") {
                        body.clone()
                    } else {
                        "{\"status\":\"ok\"}".into()
                    };
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    );
                }
            });
            Ok(())
        };
        let _ = port;
        // no: nothing starts
        let (code, text) = said(|o| {
            start(
                &ctx,
                false,
                1_794_000_000,
                &spawn,
                Duration::from_secs(5),
                &mut |_| false,
                o,
            )
        });
        assert_eq!(code, 0);
        assert!(
            text.contains("--lora-init-without-apply") && text.contains("Not started."),
            "{text}"
        );
        assert!(started.borrow().is_empty());
        // yes
        let (code, text) = said(|o| {
            start(
                &ctx,
                true,
                1_794_000_000,
                &spawn,
                Duration::from_secs(5),
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("llama-server is up.") && text.contains("loaded"),
            "{text}"
        );
        assert_eq!(started.borrow().len(), 1);
        // the agent was told (its settings file now lists the server)
        let cfg =
            std::fs::read_to_string(crate::setup::apply::agent_settings_path(&ctx.agent_data))
                .unwrap();
        assert!(
            cfg.contains("llama_servers") && cfg.contains(&format!("http://127.0.0.1:{free}")),
            "{cfg}"
        );
        // something already on the port: it says so and does not start another
        let (code, text) = said(|o| {
            start(
                &ctx,
                true,
                1,
                &spawn,
                Duration::from_secs(1),
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(text.contains("already answers"), "{text}");
    }

    #[test]
    fn an_unstartable_server_is_said_in_plain_words() {
        let ctx = mixed("m-nostart");
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        with_port(&ctx, free);
        let spawn = |_: &[String], _: &std::path::Path| -> Result<(), String> {
            Err("llama-server (No such file or directory)".into())
        };
        let (code, text) = said(|o| {
            start(
                &ctx,
                true,
                1,
                &spawn,
                Duration::from_secs(1),
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(
            text.contains("Could not start llama-server")
                && text.contains("put it next to tempo-launch"),
            "{text}"
        );
        // started but never answers
        let spawn = |_: &[String], _: &std::path::Path| -> Result<(), String> { Ok(()) };
        let (code, text) = said(|o| {
            start(
                &ctx,
                true,
                1,
                &spawn,
                Duration::from_millis(500),
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(text.contains("did not answer"), "{text}");
    }

    #[test]
    fn an_empty_mix_explains_how_to_begin() {
        let (_, text) = said(|o| show(&test_ctx("m-empty-show"), o));
        assert!(
            text.contains("mix base")
                && text.contains("mix add <band>")
                && text.contains("mix start"),
            "{text}"
        );
    }
}
