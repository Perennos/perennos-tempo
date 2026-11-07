//! Stage (video and image workflows): ComfyUI workflows the open community built, each pinned to
//! a commit and a SHA-256, set up with one y/N, and played from the command line or as tools the
//! AI engines can call. Pedals (modifiers) are named presets that land on a workflow's widgets:
//! one camera orbit, one lens, one light, the same words in every workflow. ComfyUI itself comes
//! from the Crate. See docs/STAGE.md, and docs/STAGE-SOURCES.md for who built what.

pub mod catalog;
pub mod comfy;
pub mod guard;
pub mod hf;
pub mod http;
pub mod install;
pub mod mcp;
pub mod pedals;
pub mod remote;
pub mod route;
pub mod run;
pub mod state;
pub mod world;

#[cfg(test)]
mod tests;

use crate::bank::detect::Sys;
use crate::bank::install::RealPm;
use crate::setup::json::J;
use catalog::{embedded, Catalog, Pedal, Via, Workflow};
use install::{add, comfy_dir, gb, missing, remove, saved_with, AddOpts, Host, Io};
use run::{play, RunOpts, World};
use state::State;
use std::io::Write;

pub const COMMAND: &str = "tempo-launch stage";

#[derive(Debug, PartialEq)]
pub enum StageCmd {
    List,
    Show(String),
    Check(Option<String>),
    Add(String, AddOpts),
    Remove(String, bool),
    Enable(String),
    Disable(String),
    Cloud(String, bool),
    Folder(Option<String>),
    Run(String, RunSpec),
    /// The MCP server for Tempo's AI.
    Mcp,
}

#[derive(Debug, PartialEq, Default, Clone)]
pub struct RunSpec {
    pub inputs: Vec<(String, String)>,
    pub pedals: Vec<(String, String)>,
    pub takes: u32,
    pub again: bool,
    pub on: Option<String>,
    pub seed: Option<i64>,
}

fn pair(s: &str, what: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .ok_or_else(|| format!("{what} takes NAME=VALUE, like {what} light=golden_hour"))
}

pub fn parse_args(rest: &[&str]) -> Result<StageCmd, String> {
    let Some((verb, args)) = rest.split_first() else {
        return Ok(StageCmd::List);
    };
    let id = || -> Result<String, String> {
        args.iter()
            .find(|a| !a.starts_with("--"))
            .map(|a| a.to_string())
            .ok_or(format!(
                "stage {verb} takes a workflow id, like re-angle-video"
            ))
    };
    Ok(match *verb {
        "list" if args.is_empty() => StageCmd::List,
        "mcp" if args.is_empty() => StageCmd::Mcp,
        "show" => StageCmd::Show(id()?),
        "check" => StageCmd::Check(args.first().map(|a| a.to_string())),
        "enable" => StageCmd::Enable(id()?),
        "disable" => StageCmd::Disable(id()?),
        "folder" => match args {
            [] => StageCmd::Folder(None),
            [p] => StageCmd::Folder(Some(p.to_string())),
            _ => return Err("stage folder takes one folder, or nothing to show it".into()),
        },
        "cloud" => match args {
            [i, "on"] => StageCmd::Cloud(i.to_string(), true),
            [i, "off"] => StageCmd::Cloud(i.to_string(), false),
            _ => return Err("stage cloud takes a workflow id and on or off".into()),
        },
        "remove" => {
            let yes = args.iter().any(|a| *a == "--yes" || *a == "-y");
            if let Some(bad) = args
                .iter()
                .find(|a| a.starts_with('-') && !matches!(**a, "--yes" | "-y"))
            {
                return Err(format!("stage remove: unknown argument {bad}"));
            }
            StageCmd::Remove(id()?, yes)
        }
        "add" => {
            let mut o = AddOpts::default();
            let mut it = args.iter().peekable();
            let mut idv = None;
            while let Some(a) = it.next() {
                match *a {
                    "--yes" | "-y" => o.yes = true,
                    "--with" => o.with.push(pair(
                        it.next().ok_or("--with takes pedal=choice")?,
                        "--with",
                    )?),
                    s if s.starts_with('-') => {
                        return Err(format!("stage add: unknown argument {s}"))
                    }
                    s if idv.is_none() => idv = Some(s.to_string()),
                    _ => return Err("stage add takes one workflow id".into()),
                }
            }
            StageCmd::Add(
                idv.ok_or("stage add takes a workflow id, like re-angle-video")?,
                o,
            )
        }
        "run" => {
            let mut spec = RunSpec {
                takes: 1,
                ..RunSpec::default()
            };
            let mut it = args.iter();
            let mut idv = None;
            while let Some(a) = it.next() {
                match *a {
                    "--input" | "-i" => spec.inputs.push(pair(
                        it.next().ok_or("--input takes NAME=PATH")?,
                        "--input",
                    )?),
                    "--pedal" | "-p" => spec.pedals.push(pair(
                        it.next().ok_or("--pedal takes NAME=VALUE")?,
                        "--pedal",
                    )?),
                    "--takes" => {
                        spec.takes = it
                            .next()
                            .and_then(|v| v.parse().ok())
                            .filter(|n| (1..=12).contains(n))
                            .ok_or("--takes is a number from 1 to 12")?
                    }
                    "--seed" => {
                        spec.seed = Some(
                            it.next()
                                .and_then(|v| v.parse().ok())
                                .ok_or("--seed is a whole number")?,
                        )
                    }
                    "--on" => {
                        spec.on = Some(it.next().ok_or("--on takes a machine name")?.to_string())
                    }
                    "--again" | "--encore" => spec.again = true,
                    s if s.starts_with('-') => {
                        return Err(format!("stage run: unknown argument {s}"))
                    }
                    s if idv.is_none() => idv = Some(s.to_string()),
                    _ => return Err("stage run takes one workflow id".into()),
                }
            }
            StageCmd::Run(
                idv.ok_or("stage run takes a workflow id, like re-angle-video")?,
                spec,
            )
        }
        other => return Err(format!("stage: unknown command {other}")),
    })
}

fn state_word(ctx: &crate::Ctx, st: &State, wf: &Workflow) -> &'static str {
    if !st.installed(&wf.id) {
        "not added"
    } else if !missing(ctx, wf, st, &saved_with(st, &wf.id)).is_empty() {
        "needs files"
    } else if st.enabled(&wf.id) {
        "on"
    } else {
        "ready, off"
    }
}

fn list(ctx: &crate::Ctx, cat: &Catalog, out: &mut dyn Write) -> i32 {
    let st = State::load(ctx);
    let _ = writeln!(
        out,
        "Stage (video and image workflows): {} workflows built by open projects, each pinned and checked. Pedals (modifiers) change them: camera, lens, light, strength, duration.\n",
        cat.workflows.len()
    );
    let _ = writeln!(
        out,
        "{:<22} {:<34} {:<6} {:<8} STATE",
        "ID", "WHAT", "GPU", "LICENCE"
    );
    for w in &cat.workflows {
        let _ = writeln!(
            out,
            "{:<22} {:<34} {:<6} {:<8} {}",
            w.id,
            w.title,
            format!("{:.0} GB", w.min_vram_gb),
            w.licence,
            state_word(ctx, &st, w)
        );
    }
    let _ = writeln!(out, "\n`{COMMAND} show ID` tells you about one, `{COMMAND} add ID` sets it up after a y/N, `{COMMAND} enable ID` lets Tempo play it.");
    let _ = writeln!(out, "ComfyUI itself comes from the Crate: `tempo-launch crate show comfyui`. Credits and licences: docs/STAGE-SOURCES.md.");
    0
}

fn pedal_line(cat: &Catalog, p: &Pedal) -> String {
    match &p.via {
        Via::Choices(c) => format!(
            "{}: {}",
            p.name,
            c.iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Via::Prompt(_) => format!(
            "{}: {}",
            p.name,
            cat.word(&p.name).map_or(String::new(), |w| w
                .choices
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(", "))
        ),
        Via::Number { min, max, .. } => format!(
            "{}: {min} to {max}{}{}",
            p.name,
            cat.word(&p.name)
                .filter(|w| !w.unit.is_empty())
                .map_or(String::new(), |w| format!(" {}", w.unit)),
            p.default
                .as_ref()
                .map_or(String::new(), |d| format!(" (default {d})"))
        ),
    }
}

fn show_one(ctx: &crate::Ctx, cat: &Catalog, id: &str, out: &mut dyn Write) -> i32 {
    let Some(w) = cat.find(id) else {
        let _ = writeln!(
            out,
            "There is no \"{id}\" in Stage. `{COMMAND}` lists what there is."
        );
        return 1;
    };
    let st = State::load(ctx);
    let _ = writeln!(out, "{} ({})", w.title, w.id);
    let _ = writeln!(out, "  {}", w.what);
    let _ = writeln!(out, "  Feature:    {}", w.feature);
    let _ = writeln!(
        out,
        "  From:       {} at {} ({}) {}",
        w.repo,
        &w.commit[..8],
        w.licence,
        w.url
    );
    let _ = writeln!(
        out,
        "  Workflow:   {} (SHA-256 {})",
        w.file,
        &w.file_sha256[..16]
    );
    let _ = writeln!(
        out,
        "  Memory:     {:.0} GB of graphics memory, {:.0} GB of system memory",
        w.min_vram_gb, w.ram_gb
    );
    for t in &w.tiers {
        let _ = writeln!(
            out,
            "  Tier:       {:.0} GB and up: {}{}",
            t.min_vram_gb,
            t.label,
            if t.tested { " (measured upstream)" } else { "" }
        );
    }
    let _ = writeln!(out, "  Tested:     {}", w.tested);
    if w.nodes.is_empty() {
        let _ = writeln!(out, "  Nodes:      none, ComfyUI's own");
    }
    for n in &w.nodes {
        let _ = writeln!(
            out,
            "  Node:       {} ({}) at {}",
            n.name,
            n.licence,
            &n.commit[..8]
        );
    }
    let (g, u) = w.sizes(&[]);
    let _ = writeln!(
        out,
        "  Models:     about {}{}",
        gb(g),
        if u > 0 {
            format!(" plus {u} of unknown size")
        } else {
            String::new()
        }
    );
    for m in &w.models {
        let _ = writeln!(
            out,
            "    {} ({}, {}){}{}{}",
            m.name,
            m.gb.map_or("size unknown".into(), gb),
            m.licence,
            if m.gated {
                ", gated: you accept its licence"
            } else {
                ""
            },
            if m.verified {
                ""
            } else {
                ", SHA-256 not pinned"
            },
            m.only_for
                .as_ref()
                .map_or(String::new(), |(a, b)| format!(", only with {a}={b}"))
        );
    }
    for i in &w.inputs {
        let _ = writeln!(
            out,
            "  Input:      {} ({}){} {}",
            i.name,
            i.kind,
            if i.required && i.default.is_empty() {
                ", needed"
            } else {
                ""
            },
            i.about
        );
    }
    for p in &w.pedals {
        let _ = writeln!(out, "  Pedal:      {}", pedal_line(cat, p));
    }
    let _ = writeln!(
        out,
        "  Tool:       {} (when it is on)",
        catalog::tool_name(&w.id)
    );
    let _ = writeln!(out, "  State:      {}", state_word(ctx, &st, w));
    0
}

fn check_one(
    ctx: &crate::Ctx,
    world: &dyn World,
    cat: &Catalog,
    w: &Workflow,
    out: &mut dyn Write,
) -> i32 {
    let st = State::load(ctx);
    let _ = writeln!(out, "{} ({}): {}", w.title, w.id, state_word(ctx, &st, w));
    if st.installed(&w.id) {
        let lacks = missing(ctx, w, &st, &saved_with(&st, &w.id));
        for l in &lacks {
            let _ = writeln!(out, "  Missing: {l}");
        }
        if lacks.is_empty() {
            let _ = writeln!(out, "  Everything it needs is in place.");
        }
    } else {
        let _ = writeln!(out, "  Not added yet: `{COMMAND} add {}`.", w.id);
    }
    match comfy_dir(ctx, &st) {
        Ok(d) => {
            let _ = writeln!(out, "  ComfyUI folder: {}", d.display());
        }
        Err(e) => {
            let _ = writeln!(out, "  ComfyUI: {e}");
        }
    }
    let mut boxes = vec![];
    if let Some(l) = world.local() {
        boxes.push(l);
    }
    boxes.extend(world.fleet(&w.id));
    if boxes.is_empty() {
        let _ = writeln!(out, "  Where it fits: no ComfyUI is answering here or in your Fleet. {} needs {:.0} GB of graphics memory. Start ComfyUI, or allow it on another computer with `tempo-launch fleet allow stage`.", w.title, w.min_vram_gb);
    } else {
        let _ = writeln!(out, "  Where it fits:");
        for (m, t) in route::rows(&boxes, w) {
            match t {
                Ok(t) => {
                    let _ = writeln!(
                        out,
                        "    {}: {:.0} GB, plays the tier \"{}\"",
                        m.name, m.gpu_gb, t.label
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "    {}: no. {e}", m.name);
                }
            }
        }
        match route::pick(&boxes, w) {
            Ok((m, _)) => {
                let _ = writeln!(
                    out,
                    "  It would run on {}.",
                    if m.local {
                        "this computer".to_string()
                    } else {
                        format!("the {} box", m.name)
                    }
                );
            }
            Err(e) => {
                let _ = writeln!(out, "  {e}");
            }
        }
    }
    let _ = cat;
    0
}

/// Everything but the MCP server, against streams and a world that tests can fake.
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    ctx: &crate::Ctx,
    host: &Host,
    world: &dyn World,
    cat: &Catalog,
    cmd: StageCmd,
    now: u64,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> String,
) -> i32 {
    match cmd {
        StageCmd::List => list(ctx, cat, out),
        StageCmd::Show(id) => show_one(ctx, cat, &id, out),
        StageCmd::Check(id) => {
            let all: Vec<&Workflow> = match &id {
                Some(i) => match cat.find(i) {
                    Some(w) => vec![w],
                    None => {
                        let _ = writeln!(out, "There is no \"{i}\" in Stage.");
                        return 1;
                    }
                },
                None => {
                    let st = State::load(ctx);
                    cat.workflows
                        .iter()
                        .filter(|w| st.installed(&w.id))
                        .collect()
                }
            };
            if all.is_empty() {
                let _ = writeln!(
                    out,
                    "Nothing is added yet. `{COMMAND}` lists what Stage can play."
                );
            }
            for w in all {
                check_one(ctx, world, cat, w, out);
            }
            0
        }
        StageCmd::Add(id, o) => add(ctx, host, cat, &id, &o, now, &mut Io { out, ask }),
        StageCmd::Remove(id, yes) => remove(ctx, cat, &id, yes, now, &mut Io { out, ask }),
        StageCmd::Enable(id) | StageCmd::Disable(id) if cat.find(&id).is_none() => {
            let _ = writeln!(out, "There is no \"{id}\" in Stage.");
            1
        }
        StageCmd::Enable(id) => set_enabled(ctx, cat, &id, true, out),
        StageCmd::Disable(id) => set_enabled(ctx, cat, &id, false, out),
        StageCmd::Cloud(id, on) => cloud(ctx, cat, &id, on, out, ask),
        StageCmd::Folder(p) => folder(ctx, p, out),
        StageCmd::Run(id, spec) => {
            let opts = RunOpts {
                inputs: spec.inputs,
                pedals: spec.pedals,
                takes: spec.takes,
                again: spec.again,
                on: spec.on,
                seed: spec.seed,
                guarded: false,
                ..RunOpts::default()
            };
            match play(ctx, world, cat, &id, &opts) {
                Ok(o) => {
                    let _ = writeln!(out, "{}", mcp::report(&o));
                    0
                }
                Err(e) => {
                    let _ = writeln!(out, "{e}");
                    1
                }
            }
        }
        StageCmd::Mcp => mcp::run_stdio(ctx),
    }
}

fn set_enabled(ctx: &crate::Ctx, cat: &Catalog, id: &str, on: bool, out: &mut dyn Write) -> i32 {
    let Some(w) = cat.find(id) else { return 1 };
    let mut st = State::load(ctx);
    if on {
        if !st.installed(id) {
            let _ = writeln!(
                out,
                "{id} is not added yet. `{COMMAND} add {id}` sets it up first."
            );
            return 1;
        }
        let lacks = missing(ctx, w, &st, &saved_with(&st, id));
        if !lacks.is_empty() {
            let _ = writeln!(
                out,
                "{id} is not ready: {}. `{COMMAND} check {id}` has the details.",
                lacks.join("; ")
            );
            return 1;
        }
    }
    st.set(id, vec![("enabled", J::Bool(on))]);
    if let Err(e) = st.save(ctx) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    if on {
        let _ = writeln!(out, "On. Tempo can play {} as {}. It runs on ComfyUI on your own machines, only on files in the folders you allow (`tempo-launch crate folders`), and the agent asks you each time unless you trust it. Restart the agent so it connects.", w.title, catalog::tool_name(id));
    } else {
        let _ = writeln!(out, "Off. Tempo no longer plays {}.", w.title);
    }
    0
}

fn cloud(
    ctx: &crate::Ctx,
    cat: &Catalog,
    id: &str,
    on: bool,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> String,
) -> i32 {
    let Some(w) = cat.find(id) else {
        let _ = writeln!(out, "There is no \"{id}\" in Stage.");
        return 1;
    };
    if on {
        let _ = writeln!(out, "This lets {} use nodes that call a paid or cloud service. What you send leaves your computer, and it can cost money. Today nothing in the catalogue needs it.", w.title);
        if !matches!(
            ask("Turn it on for this workflow? [y/N] ")
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "y" | "yes"
        ) {
            let _ = writeln!(out, "Left off.");
            return 0;
        }
    }
    let mut st = State::load(ctx);
    st.set(id, vec![("allow_cloud", J::Bool(on))]);
    if let Err(e) = st.save(ctx) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = writeln!(
        out,
        "{} for {}.",
        if on {
            "Cloud nodes are allowed"
        } else {
            "Cloud nodes are refused again"
        },
        w.title
    );
    0
}

fn folder(ctx: &crate::Ctx, p: Option<String>, out: &mut dyn Write) -> i32 {
    let mut st = State::load(ctx);
    match p {
        None => match comfy_dir(ctx, &st) {
            Ok(d) => {
                let _ = writeln!(out, "ComfyUI folder: {}", d.display());
                0
            }
            Err(e) => {
                let _ = writeln!(out, "{e}");
                1
            }
        },
        Some(p) => {
            let path = std::path::PathBuf::from(&p);
            let Some(c) = path
                .is_absolute()
                .then(|| std::fs::canonicalize(&path).ok())
                .flatten()
            else {
                let _ = writeln!(out, "{p} must be the full path of a folder that exists.");
                return 1;
            };
            if !c.join("custom_nodes").is_dir() || !c.join("models").is_dir() {
                let _ = writeln!(
                    out,
                    "{} is not a ComfyUI folder: it needs custom_nodes and models inside.",
                    c.display()
                );
                return 1;
            }
            st.set_folder(&c.to_string_lossy());
            if let Err(e) = st.save(ctx) {
                let _ = writeln!(out, "{e}");
                return 1;
            }
            let _ = writeln!(out, "Stage will use {}.", c.display());
            0
        }
    }
}

/// The real command.
pub fn run(ctx: &crate::Ctx, cmd: StageCmd) -> i32 {
    let sys = Sys::real();
    let pm = RealPm(&sys);
    let sc = guard::AgentSoundcheck {
        exe: ctx.find("tempo-local-agent"),
        limit: std::time::Duration::from_secs(180),
    };
    let token = hf::token_from_env();
    let host = Host {
        pm: &pm,
        os: std::env::consts::OS,
        sc: &sc,
        token: token.as_ref(),
    };
    let world = world::RealWorld::new(ctx);
    run_with(
        ctx,
        &host,
        &world,
        &embedded(),
        cmd,
        crate::unix_now(),
        &mut std::io::stdout(),
        &mut |q| {
            print!("{q}");
            let _ = std::io::stdout().flush();
            let mut s = String::new();
            let _ = std::io::stdin().read_line(&mut s);
            s
        },
    )
}

/// Whether `<home>/stage.json` has at least one workflow turned on: the agent connects Stage's
/// tools only then.
pub fn any_on(ctx: &crate::Ctx) -> bool {
    !State::load(ctx).enabled_ids().is_empty()
}

/// The workflows this computer has ready and on, for the Fleet heartbeat.
pub fn ready_ids(ctx: &crate::Ctx) -> Vec<String> {
    let st = State::load(ctx);
    let cat = embedded();
    cat.workflows
        .iter()
        .filter(|w| st.enabled(&w.id) && missing(ctx, w, &st, &saved_with(&st, &w.id)).is_empty())
        .map(|w| w.id.clone())
        .collect()
}
