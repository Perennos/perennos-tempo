//! The Crate: a bank of free tools Tempo can set up and play, like a record crate you dig through.
//! `tempo-launch crate` lists them (the plain meaning: a bank of third-party tools you can
//! configure). Each is the best free one in its class: Blender, FFmpeg, Pandoc and so on, each
//! with its licence named. `crate add` installs one with your own package manager after a y/N;
//! `crate enable` lets Tempo play it, through `crate mcp`, with fixed arguments and only on the
//! folders you allow. See docs/TEMPO-SETUP.md.

pub mod catalog;
pub mod detect;
pub mod install;
pub mod mcp;
pub mod state;

use crate::setup::json::J;
use crate::setup::sha256::sha256_hex;
use crate::{html_escape, stamp, write_atomic, Ctx};
use catalog::{embedded, find, Tool};
use detect::{detect, Sys};
use install::{add, remove, Opts, Pm, RealPm};
use state::State;
use std::io::Write;

pub const COMMAND: &str = "tempo-launch crate";

#[derive(Debug, PartialEq)]
pub enum FolderCmd {
    List,
    Add(String),
    Remove(String),
}

#[derive(Debug, PartialEq)]
pub enum BankCmd {
    List,
    Show(String),
    Check,
    Add(String, Opts),
    Remove(String, Opts),
    Enable(String),
    Disable(String),
    /// Read a script in the scripts folder, and approve it for Blender.
    Review(String),
    Folders(FolderCmd),
    /// The MCP server for Tempo's AI.
    Mcp,
}

pub fn parse_args(rest: &[&str]) -> Result<BankCmd, String> {
    let (verb, args) = match rest.split_first() {
        None => return Ok(BankCmd::List),
        Some(x) => x,
    };
    let one = |what: &str| -> Result<String, String> {
        match args {
            [id] if !id.starts_with("--") => Ok(id.to_string()),
            _ => Err(format!("crate {verb} takes one {what}")),
        }
    };
    let with_flags = |allowed: &[&str]| -> Result<(String, Opts), String> {
        let id = args
            .iter()
            .find(|a| !a.starts_with('-'))
            .ok_or(format!("crate {verb} takes a tool id, like ffmpeg"))?;
        let mut o = Opts::default();
        for a in args.iter().filter(|a| a.starts_with('-')) {
            match *a {
                "--yes" | "-y" if allowed.contains(&"--yes") => o.yes = true,
                "--enable" if allowed.contains(&"--enable") => o.enable = true,
                other => return Err(format!("crate {verb}: unknown argument {other}")),
            }
        }
        if args.iter().filter(|a| !a.starts_with('-')).count() > 1 {
            return Err(format!("crate {verb} takes one tool id"));
        }
        Ok((id.to_string(), o))
    };
    Ok(match *verb {
        "list" if args.is_empty() => BankCmd::List,
        "check" if args.is_empty() => BankCmd::Check,
        "mcp" if args.is_empty() => BankCmd::Mcp,
        "show" => BankCmd::Show(one("tool id")?),
        "enable" => BankCmd::Enable(one("tool id")?),
        "disable" => BankCmd::Disable(one("tool id")?),
        "review" => BankCmd::Review(one("script name")?),
        "add" => {
            let (id, o) = with_flags(&["--yes", "--enable"])?;
            BankCmd::Add(id, o)
        }
        "remove" => {
            let (id, o) = with_flags(&["--yes"])?;
            BankCmd::Remove(id, o)
        }
        "folders" => match args {
            [] => BankCmd::Folders(FolderCmd::List),
            ["add", d] => BankCmd::Folders(FolderCmd::Add(d.to_string())),
            ["remove", d] => BankCmd::Folders(FolderCmd::Remove(d.to_string())),
            _ => return Err("crate folders takes nothing, add FOLDER or remove FOLDER".into()),
        },
        other => return Err(format!("crate: unknown command {other}")),
    })
}

fn here(f: &Option<detect::Found>, on: bool, t: &Tool) -> String {
    match f {
        Some(f) => format!(
            "here{}{}",
            f.version
                .as_deref()
                .map_or(String::new(), |v| format!(" {v}")),
            if on { ", on" } else { "" }
        ),
        None if t
            .route(std::env::consts::OS)
            .is_some_and(|r| !r.automatic()) =>
        {
            "by hand".into()
        }
        None => "not here".into(),
    }
}

fn proposals(ctx: &Ctx) -> Vec<String> {
    let dir = ctx.home.join("crate").join("proposals");
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| crate::setup::json::parse(&t).ok())
        .filter_map(|j| {
            Some(format!(
                "{} ({})",
                j.str("command")?,
                j.str("note").unwrap_or("")
            ))
        })
        .collect();
    v.sort();
    v
}

fn list(ctx: &Ctx, sys: &Sys, tools: &[Tool], save: bool, now: u64, out: &mut dyn Write) -> i32 {
    let mut st = State::load(ctx);
    let found: Vec<_> = tools.iter().map(|t| detect(ctx, sys, t)).collect();
    let n = found.iter().flatten().count();
    let _ = writeln!(
        out,
        "The Crate: free tools Tempo can set up and play (a bank of {} tools, {n} here).\n",
        tools.len()
    );
    let _ = writeln!(
        out,
        "{:<12} {:<13} {:<18} {:<16} TEMPO",
        "ID", "TOOL", "HERE", "KIND"
    );
    for (t, f) in tools.iter().zip(&found) {
        let on = st.enabled(&t.id);
        let plays = if t.tools.is_empty() {
            "listed"
        } else {
            "plays"
        };
        let _ = writeln!(
            out,
            "{:<12} {:<13} {:<18} {:<16} {plays}",
            t.id,
            t.name,
            here(f, on, t),
            t.category
        );
        if save {
            st.set(
                &t.id,
                vec![
                    ("installed", J::Bool(f.is_some())),
                    (
                        "version",
                        f.as_ref()
                            .and_then(|f| f.version.as_deref())
                            .map_or(J::Null, J::s),
                    ),
                    ("checked_at", J::s(&stamp(now))),
                ],
            );
        }
    }
    let waiting = proposals(ctx);
    if !waiting.is_empty() {
        let _ = writeln!(out, "\nYour AI suggested:");
        for w in waiting {
            let _ = writeln!(out, "  {COMMAND} {w}");
        }
    }
    let _ = writeln!(
        out,
        "\n`{COMMAND} show ID` tells you about one, `{COMMAND} add ID` sets it up, `{COMMAND} enable ID` lets Tempo play it."
    );
    if save {
        if let Err(e) = st.save(ctx) {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    }
    0
}

fn show_one(
    ctx: &Ctx,
    sys: &Sys,
    pm: &dyn Pm,
    tools: &[Tool],
    id: &str,
    out: &mut dyn Write,
) -> i32 {
    let Some(t) = find(tools, id) else {
        let _ = writeln!(
            out,
            "There is no \"{id}\" in the Crate. `{COMMAND}` lists what there is."
        );
        return 1;
    };
    let st = State::load(ctx);
    let f = detect(ctx, sys, t);
    let _ = writeln!(out, "{} ({})", t.name, t.id);
    let _ = writeln!(out, "  {}", t.what);
    let _ = writeln!(out, "  Kind:      {}", t.category);
    let _ = writeln!(out, "  Licence:   {}", t.licence);
    let _ = writeln!(out, "  Homepage:  {}", t.homepage);
    let _ = writeln!(out, "  Size:      {}", t.size_text());
    let _ = writeln!(out, "  Here:      {}", here(&f, st.enabled(&t.id), t));
    if let Some(f) = &f {
        let _ = writeln!(out, "  Program:   {}", f.bin.display());
    }
    match install::plan_add(t, &sys.os, pm) {
        Ok(p) if p.download.is_some() => {
            let _ = writeln!(
                out,
                "  Install:   a pinned download, checked against its SHA-256 first"
            );
        }
        Ok(p) => {
            let _ = writeln!(
                out,
                "  Install:   {}{}",
                install::show(&p.argv),
                if p.admin {
                    "  (needs admin: you run it)"
                } else {
                    ""
                }
            );
        }
        Err(e) => {
            let _ = writeln!(out, "  Install:   {e}");
        }
    }
    let _ = writeln!(out, "  Tempo:     {}", t.tempo);
    if !t.tools.is_empty() {
        let on = if st.enabled(&t.id) { "on" } else { "off" };
        let _ = writeln!(out, "  AI tools:  {} ({on})", t.tools.join(", "));
    }
    let _ = writeln!(
        out,
        "  Checked:   {}",
        if t.verified { "yes" } else { "not fully" }
    );
    if !t.note.is_empty() {
        let _ = writeln!(out, "  Note:      {}", t.note);
    }
    0
}

fn set_enabled(
    ctx: &Ctx,
    sys: &Sys,
    tools: &[Tool],
    id: &str,
    on: bool,
    out: &mut dyn Write,
) -> i32 {
    let Some(t) = find(tools, id) else {
        let _ = writeln!(out, "There is no \"{id}\" in the Crate.");
        return 1;
    };
    if on {
        if t.tools.is_empty() {
            let _ = writeln!(out, "Tempo does not play {} yet: {}", t.name, t.tempo);
            return 1;
        }
        if detect(ctx, sys, t).is_none() {
            let _ = writeln!(
                out,
                "{} is not installed. `{COMMAND} add {}` sets it up first.",
                t.name, t.id
            );
            return 1;
        }
    }
    let mut st = State::load(ctx);
    st.set(&t.id, vec![("enabled", J::Bool(on))]);
    if let Err(e) = st.save(ctx) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    if on {
        let _ = writeln!(
            out,
            "On. Tempo can play {}: {}. It works only on the folders you allow (`{COMMAND} folders`), and the agent asks you each time unless you trust it.",
            t.name,
            t.tools.join(", ")
        );
    } else {
        let _ = writeln!(out, "Off. Tempo no longer plays {}.", t.name);
    }
    0
}

fn review(ctx: &Ctx, name: &str, out: &mut dyn Write, ask: &mut dyn FnMut(&str) -> bool) -> i32 {
    let st0 = State::load(ctx);
    let rules = mcp::Rules::load(ctx, &st0);
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.ends_with(".py")
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        let _ = writeln!(
            out,
            "A script name is a plain file name ending in .py, like make_cube.py, in {}.",
            rules.scripts.display()
        );
        return 1;
    }
    let path = rules.scripts.join(name);
    let Ok(body) = std::fs::read(&path) else {
        let _ = writeln!(
            out,
            "{name} is not in {}. Put the script there first.",
            rules.scripts.display()
        );
        return 1;
    };
    if body.len() > 256 * 1024 {
        let _ = writeln!(out, "{name} is over 256 KB, too long to read through here.");
        return 1;
    }
    let sha = sha256_hex(&body);
    let _ = writeln!(
        out,
        "----- {name} -----\n{}\n----- end -----",
        String::from_utf8_lossy(&body)
    );
    let _ = writeln!(
        out,
        "This script runs inside Blender with your own rights: it can read, change and delete your files. SHA-256 {sha}"
    );
    if !ask("Have you read it, and may Tempo run exactly this text? [y/N] ") {
        let _ = writeln!(out, "Not approved.");
        return 0;
    }
    let mut st = State::load(ctx);
    st.set_reviewed(name, &sha);
    if let Err(e) = st.save(ctx) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = writeln!(out, "Approved. If the file changes, it must be read again.");
    0
}

fn folders(ctx: &Ctx, cmd: &FolderCmd, out: &mut dyn Write) -> i32 {
    let mut st = State::load(ctx);
    let mut list = st.folders();
    match cmd {
        FolderCmd::List => {
            let r = mcp::Rules::load(ctx, &st);
            let _ = writeln!(
                out,
                "The Crate's tools may use these folders, and no others:"
            );
            for f in &r.roots {
                let _ = writeln!(out, "  {}", f.display());
            }
            let _ = writeln!(out, "Add one with `{COMMAND} folders add FOLDER`. The first is the workbench, always there; the scripts and models folders sit beside it.");
            return 0;
        }
        FolderCmd::Add(d) => {
            let p = std::path::PathBuf::from(d);
            let Some(c) = p
                .is_absolute()
                .then(|| std::fs::canonicalize(&p).ok())
                .flatten()
                .filter(|c| c.is_dir())
            else {
                let _ = writeln!(out, "{d} must be the full path of a folder that exists.");
                return 1;
            };
            if c.parent().is_none() {
                let _ = writeln!(out, "A whole drive is too wide. Pick a folder inside it.");
                return 1;
            }
            let t = c.to_string_lossy().into_owned();
            if !list.contains(&t) {
                list.push(t.clone());
            }
            let _ = writeln!(
                out,
                "Added {t}. Tools Tempo plays can now read and write there."
            );
        }
        FolderCmd::Remove(d) => {
            let before = list.len();
            let canon = std::fs::canonicalize(d)
                .ok()
                .map(|c| c.to_string_lossy().into_owned());
            list.retain(|x| x != d && Some(x) != canon.as_ref());
            if list.len() == before {
                let _ = writeln!(out, "{d} is not in the list.");
                return 1;
            }
            let _ = writeln!(out, "Removed {d}. Nothing in it was touched.");
        }
    }
    st.set_folders(&list);
    match st.save(ctx) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            1
        }
    }
}

/// Everything but the MCP server, against streams and a package manager that tests can fake.
pub fn run_with(
    ctx: &Ctx,
    sys: &Sys,
    pm: &dyn Pm,
    cmd: BankCmd,
    now: u64,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let tools = embedded();
    match cmd {
        BankCmd::List => list(ctx, sys, &tools, false, now, out),
        BankCmd::Check => list(ctx, sys, &tools, true, now, out),
        BankCmd::Show(id) => show_one(ctx, sys, pm, &tools, &id, out),
        BankCmd::Add(id, o) => add(ctx, sys, pm, &tools, &id, o, now, out, ask),
        BankCmd::Remove(id, o) => remove(ctx, sys, pm, &tools, &id, o, now, out, ask),
        BankCmd::Enable(id) => set_enabled(ctx, sys, &tools, &id, true, out),
        BankCmd::Disable(id) => set_enabled(ctx, sys, &tools, &id, false, out),
        BankCmd::Review(name) => review(ctx, &name, out, ask),
        BankCmd::Folders(f) => folders(ctx, &f, out),
        BankCmd::Mcp => mcp::run_stdio(ctx),
    }
}

/// The real command.
pub fn run(ctx: &Ctx, cmd: BankCmd) -> i32 {
    let sys = Sys::real();
    run_with(
        ctx,
        &sys,
        &RealPm(&sys),
        cmd,
        crate::unix_now(),
        &mut std::io::stdout(),
        &mut crate::setup::apply::ask_terminal,
    )
}

// ---- for the AI helper (`tempo-launch mcp`) and the welcome page

/// What the setup helper's `crate_list` says: the catalog and this computer's state, no folders.
pub fn list_facts(ctx: &Ctx, sys: &Sys) -> J {
    let st = State::load(ctx);
    J::Arr(
        embedded()
            .iter()
            .map(|t| {
                let f = detect(ctx, sys, t);
                J::Obj(vec![
                    ("id".into(), J::s(&t.id)),
                    ("name".into(), J::s(&t.name)),
                    ("for".into(), J::s(&t.what)),
                    ("kind".into(), J::s(&t.category)),
                    ("licence".into(), J::s(&t.licence)),
                    ("size".into(), J::s(&t.size_text())),
                    ("installed".into(), J::Bool(f.is_some())),
                    (
                        "version".into(),
                        f.and_then(|f| f.version).map_or(J::Null, |v| J::s(&v)),
                    ),
                    ("on_for_tempo".into(), J::Bool(st.enabled(&t.id))),
                    ("tempo_can".into(), J::s(&t.tempo)),
                    ("ai_tools".into(), J::strings(&t.tools)),
                    ("checked".into(), J::Bool(t.verified)),
                ])
            })
            .collect(),
    )
}

/// Write a suggestion for the owner. Nothing is installed or turned on: it returns the command
/// the owner runs. `action` is add, remove, enable or disable; or `script`, which saves a
/// Blender script into the scripts folder (never over a file) for the owner to read and approve.
pub fn propose(ctx: &Ctx, args: &J, now: u64) -> Result<String, String> {
    let tools = embedded();
    let action = args.str("action").unwrap_or("");
    let note: String = args
        .str("note")
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect();
    let dir = ctx.home.join("crate");
    let (command, file) = if action == "script" {
        let name = args.str("script_name").unwrap_or("");
        let body = args
            .str("script_text")
            .ok_or("script needs script_name and script_text")?;
        let st = State::load(ctx);
        let scripts = mcp::Rules::load(ctx, &st).scripts;
        let ok = !name.is_empty()
            && name.len() <= 64
            && name.ends_with(".py")
            && !name.starts_with(['.', '-'])
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if !ok {
            return Err("script_name is a plain file name ending in .py, like make_cube.py".into());
        }
        if body.len() > 64 * 1024 {
            return Err("script_text is over 64 KB".into());
        }
        let path = scripts.join(name);
        if path.exists() {
            return Err(format!("{name} already exists in the scripts folder. Pick another name; a script is never replaced."));
        }
        write_atomic(&path, body.as_bytes())
            .map_err(|e| format!("could not save the script: {e}"))?;
        (
            format!("{COMMAND} review {name}"),
            format!("script-{name}.json"),
        )
    } else {
        let id = args.str("id").unwrap_or("");
        let t = find(&tools, id)
            .ok_or_else(|| format!("there is no \"{id}\" in the Crate (see crate_list)"))?;
        if !["add", "remove", "enable", "disable"].contains(&action) {
            return Err("action is add, remove, enable, disable or script".into());
        }
        (
            format!("{COMMAND} {action} {}", t.id),
            format!("{}-{action}.json", t.id),
        )
    };
    let doc = J::Obj(vec![
        (
            "proposed_by".into(),
            J::s("an AI, through tempo-launch mcp"),
        ),
        ("proposed_at".into(), J::s(&stamp(now))),
        (
            "command".into(),
            J::s(command.trim_start_matches(&format!("{COMMAND} "))),
        ),
        ("note".into(), J::s(&note)),
    ]);
    write_atomic(&dir.join("proposals").join(file), doc.pretty().as_bytes())
        .map_err(|e| format!("could not write the proposal: {e}"))?;
    Ok(format!(
        "Saved as a suggestion. Nothing was installed or turned on.\nTell the owner to run this in a terminal; it shows what it will do and asks y/N:\n\n  {command}\n\nYou cannot run it yourself."
    ))
}

/// The best picks for a computer: what is free, quick to set up here, and fits its memory.
pub fn picks(os: &str, ram_gb: Option<f64>, has_gpu: bool) -> Vec<Tool> {
    let ram = ram_gb.unwrap_or(0.0);
    let mut ids = vec!["ffmpeg", "imagemagick", "pandoc", "tesseract", "ripgrep"];
    if ram >= 8.0 {
        ids.extend(["whisper-cpp", "piper"]);
    }
    if has_gpu || ram >= 16.0 {
        ids.push("blender");
    }
    let cat = embedded();
    ids.into_iter()
        .filter_map(|i| find(&cat, i).cloned())
        .filter(|t| {
            t.route(os)
                .is_some_and(|r| r.automatic() || r.manual.is_some())
        })
        .collect()
}

/// The short "The Crate" section of the welcome page.
pub fn welcome_section(os: &str, ram_gb: Option<f64>, has_gpu: bool) -> String {
    let items: String = picks(os, ram_gb, has_gpu)
        .iter()
        .map(|t| {
            format!(
                "<li><strong>{}</strong> ({}): {} <code>{COMMAND} add {}</code></li>",
                html_escape(&t.name),
                html_escape(&t.licence),
                html_escape(&t.what),
                html_escape(&t.id)
            )
        })
        .collect();
    format!(
        r#"<h2>The Crate: free tools Tempo can play</h2>
<p>The <em>Crate</em> is a bank of free tools you can set up here and let Tempo play: video, pictures, documents, speech, 3D. Every one names its licence, and Tempo shows the exact command and asks y/N before it installs anything, with your own package manager. Nothing is installed by this page. Good picks for this computer:</p>
<ul>{items}</ul>
<p>See all of them, then turn one on for Tempo (it only works on folders you allow, and the agent asks each time unless you trust it):</p>
<pre>{COMMAND}
{COMMAND} enable ffmpeg</pre>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use detect::tests::fake_bin;
    use detect::tests::{ctx_in, sys_with, tmp};

    fn p(a: &[&str]) -> Result<BankCmd, String> {
        parse_args(a)
    }

    #[test]
    fn the_command_line_is_parsed() {
        assert_eq!(p(&[]), Ok(BankCmd::List));
        assert_eq!(p(&["check"]), Ok(BankCmd::Check));
        assert_eq!(p(&["mcp"]), Ok(BankCmd::Mcp));
        assert_eq!(p(&["show", "blender"]), Ok(BankCmd::Show("blender".into())));
        assert_eq!(
            p(&["add", "ffmpeg", "--yes", "--enable"]),
            Ok(BankCmd::Add(
                "ffmpeg".into(),
                Opts {
                    yes: true,
                    enable: true
                }
            ))
        );
        assert_eq!(
            p(&["remove", "--yes", "sox"]),
            Ok(BankCmd::Remove(
                "sox".into(),
                Opts {
                    yes: true,
                    enable: false
                }
            ))
        );
        assert_eq!(
            p(&["folders", "add", "/x"]),
            Ok(BankCmd::Folders(FolderCmd::Add("/x".into())))
        );
        for bad in [
            &["add"][..],
            &["remove", "a", "--enable"],
            &["add", "a", "b"],
            &["nope"],
            &["show"],
            &["mcp", "x"],
            &["folders", "wipe"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
    }

    struct NoPm;
    impl Pm for NoPm {
        fn which(&self, _: &str) -> bool {
            true
        }
        fn run(&self, _: &[String]) -> Result<(), String> {
            panic!("nothing runs here")
        }
        fn fetcher(&self) -> Option<std::sync::Arc<dyn crate::setup::registry::Fetch>> {
            None
        }
    }

    fn go(ctx: &Ctx, sys: &Sys, cmd: BankCmd, yes: bool) -> (i32, String) {
        let mut out = vec![];
        let code = run_with(ctx, sys, &NoPm, cmd, 5, &mut out, &mut |_| yes);
        (code, String::from_utf8(out).unwrap())
    }

    #[cfg(unix)]
    #[test]
    fn the_list_says_what_is_here_and_check_remembers_it() {
        let d = tmp("list");
        let ctx = ctx_in(&d.join("home"));
        let bin = d.join("bin");
        fake_bin(&bin, "ffmpeg", "ffmpeg version 7.1 Copyright");
        let sys = sys_with("linux", &[&bin]);
        let (code, text) = go(&ctx, &sys, BankCmd::List, false);
        assert_eq!(code, 0);
        let line = text.lines().find(|l| l.starts_with("ffmpeg")).unwrap();
        assert!(
            line.contains("here 7.1") && line.contains("plays"),
            "{line}"
        );
        assert!(text
            .lines()
            .any(|l| l.starts_with("blender") && l.contains("not here")));
        assert!(
            text.contains("23 tools") || text.contains("a bank of"),
            "{text}"
        );
        assert!(!state::path(&ctx).exists(), "listing writes nothing");
        go(&ctx, &sys, BankCmd::Check, false);
        let saved = std::fs::read_to_string(state::path(&ctx)).unwrap();
        assert!(
            saved.contains("\"ffmpeg\"") && saved.contains("7.1"),
            "{saved}"
        );
        // enable needs it installed, and a tool Tempo plays
        let (code, text) = go(&ctx, &sys, BankCmd::Enable("blender".into()), false);
        assert_eq!(code, 1);
        assert!(text.contains("crate add blender"), "{text}");
        let (code, _) = go(&ctx, &sys, BankCmd::Enable("ffmpeg".into()), false);
        assert_eq!(code, 0);
        assert!(State::load(&ctx).enabled("ffmpeg"));
        let (code, text) = go(&ctx, &sys, BankCmd::Enable("audacity".into()), false);
        assert_eq!(code, 1);
        assert!(text.contains("does not play"), "{text}");
        go(&ctx, &sys, BankCmd::Disable("ffmpeg".into()), false);
        assert!(!State::load(&ctx).enabled("ffmpeg"));
    }

    #[test]
    fn show_tells_the_command_licence_and_what_tempo_can_do() {
        let d = tmp("show");
        let ctx = ctx_in(&d.join("home"));
        let sys = sys_with("macos", &[]);
        let (code, text) = go(&ctx, &sys, BankCmd::Show("blender".into()), false);
        assert_eq!(code, 0);
        for want in [
            "GPL-3.0-or-later",
            "brew install --cask blender",
            "blender_render",
            "https://www.blender.org/",
            "about 400 MB",
        ] {
            assert!(text.contains(want), "{want} in {text}");
        }
        let (code, text) = go(&ctx, &sys, BankCmd::Show("nope".into()), false);
        assert_eq!(code, 1);
        assert!(text.contains("no \"nope\""));
        // the unsure ones say so
        let (_, text) = go(&ctx, &sys, BankCmd::Show("whisper-cpp".into()), false);
        assert!(text.contains("not fully"), "{text}");
    }

    #[test]
    fn folders_and_script_review_are_the_owners_to_give() {
        let d = tmp("own");
        let ctx = ctx_in(&d.join("home"));
        let sys = sys_with("linux", &[]);
        let share = d.join("share");
        std::fs::create_dir_all(&share).unwrap();
        let (code, _) = go(
            &ctx,
            &sys,
            BankCmd::Folders(FolderCmd::Add(share.to_string_lossy().into())),
            false,
        );
        assert_eq!(code, 0);
        let (_, text) = go(&ctx, &sys, BankCmd::Folders(FolderCmd::List), false);
        assert!(text.contains("share") && text.contains("work"), "{text}");
        assert_eq!(
            go(
                &ctx,
                &sys,
                BankCmd::Folders(FolderCmd::Add("relative".into())),
                false
            )
            .0,
            1
        );
        assert_eq!(
            go(
                &ctx,
                &sys,
                BankCmd::Folders(FolderCmd::Add("/".into())),
                false
            )
            .0,
            1
        );
        assert_eq!(
            go(
                &ctx,
                &sys,
                BankCmd::Folders(FolderCmd::Remove(share.to_string_lossy().into())),
                false
            )
            .0,
            0
        );
        // review: shown, asked, remembered only on yes
        let scripts = ctx.home.join("crate").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("cube.py"), "import bpy\n").unwrap();
        let (_, text) = go(&ctx, &sys, BankCmd::Review("cube.py".into()), false);
        assert!(
            text.contains("import bpy") && text.contains("Not approved"),
            "{text}"
        );
        assert!(State::load(&ctx).reviewed("cube.py").is_none());
        go(&ctx, &sys, BankCmd::Review("cube.py".into()), true);
        assert_eq!(
            State::load(&ctx).reviewed("cube.py"),
            Some(sha256_hex(b"import bpy\n"))
        );
        assert_eq!(go(&ctx, &sys, BankCmd::Review("../x.py".into()), true).0, 1);
    }

    #[test]
    fn a_proposal_only_writes_a_suggestion_and_gives_the_command() {
        let d = tmp("propose");
        let ctx = ctx_in(&d.join("home"));
        let a = |t: &str| crate::setup::json::parse(t).unwrap();
        let said = propose(
            &ctx,
            &a(r#"{"action":"add","id":"ffmpeg","note":"you convert a lot of video"}"#),
            1,
        )
        .unwrap();
        assert!(
            said.contains("tempo-launch crate add ffmpeg")
                && said.contains("Nothing was installed"),
            "{said}"
        );
        assert!(!state::path(&ctx).exists(), "nothing is turned on");
        assert!(
            proposals(&ctx)[0].contains("add ffmpeg")
                && proposals(&ctx)[0].contains("convert a lot")
        );
        assert!(propose(&ctx, &a(r#"{"action":"add","id":"nope"}"#), 1).is_err());
        assert!(propose(&ctx, &a(r#"{"action":"rm -rf","id":"ffmpeg"}"#), 1).is_err());
        // a script is saved for review, never over another, and is not approved
        let said = propose(
            &ctx,
            &a(r#"{"action":"script","script_name":"cube.py","script_text":"import bpy\n"}"#),
            1,
        )
        .unwrap();
        assert!(said.contains("crate review cube.py"), "{said}");
        let scripts = ctx.home.join("crate").join("scripts");
        assert!(scripts.join("cube.py").is_file());
        assert!(State::load(&ctx).reviewed("cube.py").is_none());
        assert!(propose(
            &ctx,
            &a(r#"{"action":"script","script_name":"cube.py","script_text":"evil"}"#),
            1
        )
        .unwrap_err()
        .contains("already exists"));
        assert!(propose(
            &ctx,
            &a(r#"{"action":"script","script_name":"../x.py","script_text":"x"}"#),
            1
        )
        .is_err());
    }

    #[test]
    fn the_welcome_section_lists_picks_for_this_computer() {
        let small = picks("linux", Some(4.0), false);
        let ids: Vec<&str> = small.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ids.contains(&"ffmpeg") && !ids.contains(&"blender") && !ids.contains(&"piper"),
            "{ids:?}"
        );
        let big: Vec<String> = picks("windows", Some(32.0), true)
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(big.contains(&"blender".to_string()) && big.contains(&"whisper-cpp".to_string()));
        let html = welcome_section("linux", Some(16.0), false);
        assert!(
            html.contains("The Crate")
                && html.contains("crate add ffmpeg")
                && html.contains("LGPL-2.1-or-later")
        );
        assert!(!html.to_lowercase().contains("open source"));
    }
}
