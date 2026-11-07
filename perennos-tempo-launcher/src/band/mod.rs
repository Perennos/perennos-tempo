//! Tempo Band: a team of AI members for the owner's own workflow.
//!
//! `tempo-launch band new` sets a band up (from a template, or drafted by the local model from a
//! sentence about the work), puts each member in Room as a teammate to @mention, and schedules the
//! flows. `band run` walks a flow through the agent, one member after the next, and files every
//! step as a linked note in the Vault. `band page` writes a static dashboard.
//!
//! The one rule: no member ever posts, sends, buys or deletes anything outside this computer.
//! Members chat as guests (no tools); the launcher writes their notes, and only under the band's
//! own notes folder.

pub mod draft;
pub mod net;
pub mod page;
pub mod propose;
pub mod run;
pub mod schedule;
pub mod spec;
pub mod templates;

use crate::setup::{apply, plan::Plan, probe};
use crate::{file_stamp, write_atomic, Ctx, AGENT};
use spec::{in_words, slug_ok, Band, MAX_ROLE};
use std::io::Write;
use std::path::PathBuf;

/// What the person's own computer can run, as two model names.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Models {
    /// The thinking model (writing, judging).
    pub heavy: String,
    /// The quick one (sorting, scheduling).
    pub fast: String,
}

impl Models {
    pub fn pick(&self, heavy: bool) -> &str {
        if heavy {
            &self.heavy
        } else {
            &self.fast
        }
    }

    /// From the setup plan: whichever of the two the computer can run stands in for the other.
    pub fn from_plan(p: &Plan) -> Models {
        let name = |role: &str| p.pick(role).map(|k| k.model.clone());
        let (heavy, fast) = (name("heavy"), name("fast"));
        Models {
            heavy: heavy.clone().or_else(|| fast.clone()).unwrap_or_default(),
            fast: fast.or(heavy).unwrap_or_default(),
        }
    }

    /// `heavy` and `fast` (as an AI may write them in a proposal) become this computer's models.
    pub fn resolve(&self, band: &mut Band) {
        for m in &mut band.members {
            match m.model.as_str() {
                "heavy" => m.model = self.heavy.clone(),
                "fast" => m.model = self.fast.clone(),
                _ => {}
            }
        }
    }
}

/// Whoever answers for a member: the agent, or a stand-in in tests.
pub trait Chat {
    /// One guest conversation: `model` (empty: the agent's usual one), `role`, one message in,
    /// the answer out.
    fn say(&self, model: &str, role: &str, text: &str) -> Result<String, String>;
}

const GUARD: &str = " You work only on this computer: you cannot post, send, buy, book or delete anything, so whatever is meant for the outside world is a draft for the owner to read and use themselves.";

/// A role with the house rule added, when it fits in the 2,000 characters Room and the agent take.
pub fn with_guard(role: &str) -> String {
    let t = role.trim();
    let g = format!("{t}{GUARD}");
    if g.chars().count() <= MAX_ROLE {
        g
    } else {
        t.to_string()
    }
}

#[derive(Debug, PartialEq)]
pub enum BandCmd {
    List,
    Templates,
    New {
        template: Option<String>,
        describe: Option<String>,
        name: Option<String>,
        yes: bool,
    },
    Run {
        band: String,
        flow: Option<String>,
        note: Option<String>,
    },
    Page {
        band: String,
    },
    Room {
        band: String,
    },
    ApplyProposal {
        name: Option<String>,
    },
}

fn band_arg(v: Option<&&str>, what: &str) -> Result<String, String> {
    match v {
        Some(b) if slug_ok(b, 40) => Ok(b.to_string()),
        Some(b) => Err(format!(
            "band {what}: \"{b}\" is not a band name (lowercase letters, digits and hyphens)"
        )),
        None => Err(format!("band {what} needs the band's name")),
    }
}

pub fn parse_args(rest: &[&str]) -> Result<BandCmd, String> {
    let Some((verb, args)) = rest.split_first() else {
        return Ok(BandCmd::List);
    };
    match *verb {
        "templates" if args.is_empty() => Ok(BandCmd::Templates),
        "new" => {
            let (mut template, mut describe, mut name, mut yes) = (None, None, None, false);
            let mut i = 0;
            let value = |i: &mut usize, flag: &str| -> Result<String, String> {
                *i += 1;
                args.get(*i)
                    .filter(|v| !v.starts_with("--") || flag == "--describe")
                    .map(|v| v.to_string())
                    .ok_or_else(|| format!("band new: {flag} needs a value"))
            };
            while i < args.len() {
                match args[i] {
                    "--yes" | "-y" => yes = true,
                    "--template" => template = Some(value(&mut i, "--template")?),
                    "--describe" => describe = Some(value(&mut i, "--describe")?),
                    "--name" => name = Some(value(&mut i, "--name")?),
                    a if a.starts_with("--template=") => {
                        template = Some(a["--template=".len()..].to_string())
                    }
                    a if a.starts_with("--describe=") => {
                        describe = Some(a["--describe=".len()..].to_string())
                    }
                    a if a.starts_with("--name=") => name = Some(a["--name=".len()..].to_string()),
                    other => return Err(format!("band new: unknown argument {other}")),
                }
                i += 1;
            }
            match (&template, &describe) {
                (None, None) => {
                    return Err(
                        "band new needs --template NAME or --describe \"what you do\"".into(),
                    )
                }
                (Some(_), Some(_)) => {
                    return Err("band new takes --template or --describe, not both".into())
                }
                (Some(t), None) if templates::find(t).is_none() => {
                    let ids: Vec<&str> = templates::all().iter().map(|t| t.id).collect();
                    return Err(format!(
                        "band new: no template called {t}; the templates are {}",
                        ids.join(", ")
                    ));
                }
                (None, Some(d)) if d.trim().is_empty() => {
                    return Err("band new: --describe needs a sentence about your work".into())
                }
                _ => {}
            }
            if let Some(n) = &name {
                if !slug_ok(n, 40) {
                    return Err(format!(
                        "band new: \"{n}\" is not a band name (lowercase letters, digits and hyphens)"
                    ));
                }
            }
            Ok(BandCmd::New {
                template,
                describe,
                name,
                yes,
            })
        }
        "run" => {
            let band = band_arg(args.first(), "run")?;
            let (mut flow, mut note) = (None, None);
            let mut i = 1;
            while i < args.len() {
                match args[i] {
                    "--note" => {
                        i += 1;
                        note = Some(
                            args.get(i)
                                .ok_or("band run: --note needs some text")?
                                .to_string(),
                        );
                    }
                    f if !f.starts_with("--") && flow.is_none() => flow = Some(f.to_string()),
                    other => return Err(format!("band run: unknown argument {other}")),
                }
                i += 1;
            }
            if let Some(f) = &flow {
                if !slug_ok(f, 40) {
                    return Err(format!("band run: \"{f}\" is not a flow name"));
                }
            }
            Ok(BandCmd::Run { band, flow, note })
        }
        "page" if args.len() == 1 => Ok(BandCmd::Page {
            band: band_arg(args.first(), "page")?,
        }),
        "room" if args.len() == 1 => Ok(BandCmd::Room {
            band: band_arg(args.first(), "room")?,
        }),
        "apply-proposal" if args.len() <= 1 => Ok(BandCmd::ApplyProposal {
            name: args
                .first()
                .map(|n| band_arg(Some(n), "apply-proposal"))
                .transpose()?,
        }),
        "page" | "room" => Err(format!("band {verb} takes one band name")),
        "templates" => Err("band templates takes no arguments".into()),
        "apply-proposal" => Err("band apply-proposal takes at most one name".into()),
        other => Err(format!(
            "band: unknown command {other} (try: templates, new, run, room, page, apply-proposal)"
        )),
    }
}

// ---- where a band lives

pub fn bands_dir(ctx: &Ctx) -> PathBuf {
    ctx.home.join("bands")
}

pub fn band_dir(ctx: &Ctx, name: &str) -> PathBuf {
    bands_dir(ctx).join(name)
}

/// A saved band, checked again as it is read: a hand-edited file is held to the same rules.
pub fn load(ctx: &Ctx, name: &str) -> Result<Band, String> {
    if !slug_ok(name, 40) {
        return Err(format!("\"{name}\" is not a band name"));
    }
    let file = band_dir(ctx, name).join("band.json");
    let text = std::fs::read_to_string(&file).map_err(|_| {
        format!(
            "no band called {name}. `tempo-launch band` lists yours; `tempo-launch band new --template social-media` makes one"
        )
    })?;
    let band = spec::from_json(&text)
        .map_err(|p| format!("{} has problems:\n  - {}", file.display(), p.join("\n  - ")))?;
    if band.name != name {
        return Err(format!(
            "{} says the band is called {}, but its folder is {name}",
            file.display(),
            band.name
        ));
    }
    Ok(band)
}

/// Save band.json. A band already there goes to `<home>/trash/<date>/` first.
fn save(ctx: &Ctx, band: &Band, now: u64) -> Result<PathBuf, String> {
    let file = band_dir(ctx, &band.name).join("band.json");
    if let Ok(old) = std::fs::read_to_string(&file) {
        let stamp = file_stamp(now);
        let dir = ctx.home.join("trash").join(&stamp[..8]);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not make the trash folder: {e}"))?;
        std::fs::write(
            dir.join(format!("band-{}-{}.json", band.name, &stamp[9..])),
            old,
        )
        .map_err(|e| format!("could not keep the old band.json: {e}"))?;
    }
    write_atomic(&file, band.to_json().as_bytes())
        .map_err(|e| format!("could not write {}: {e}", file.display()))?;
    Ok(file)
}

/// Every saved band's name, and what is wrong with a band.json that does not pass.
fn saved(ctx: &Ctx) -> Vec<(String, Result<Band, String>)> {
    let mut names: Vec<String> = std::fs::read_dir(bands_dir(ctx))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("band.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let b = load(ctx, &n);
            (n, b)
        })
        .collect()
}

// ---- the commands

/// Everything the commands need from outside, so tests can stand in for the agent, Room and the
/// scheduler.
pub struct Deps<'a> {
    pub chat: &'a dyn Chat,
    pub room_dir: PathBuf,
    pub scheduler: &'a dyn schedule::Scheduler,
    pub models: Models,
    pub exe: PathBuf,
    pub now: u64,
}

fn list(ctx: &Ctx, out: &mut dyn Write) -> i32 {
    let all = saved(ctx);
    if all.is_empty() {
        let _ = writeln!(
            out,
            "No bands yet. A band is a small team of AI members for your own workflow.\n\nTry one:\n  tempo-launch band templates\n  tempo-launch band new --template social-media\n  tempo-launch band new --describe \"I run a bakery's Instagram and want posts planned weekly\""
        );
        return 0;
    }
    for (name, band) in all {
        match band {
            Ok(b) => {
                let who: Vec<String> = b.members.iter().map(|m| format!("@{}", m.handle)).collect();
                let _ = writeln!(out, "{name}: {}\n  members: {}", b.about, who.join(" "));
                for f in &b.flows {
                    let _ = writeln!(out, "  flow {}: {}", f.name, f.every.words());
                }
            }
            Err(e) => {
                let _ = writeln!(
                    out,
                    "{name}: needs attention\n  {}",
                    e.replace('\n', "\n  ")
                );
            }
        }
    }
    0
}

fn templates_list(out: &mut dyn Write) -> i32 {
    for t in templates::all() {
        let who: Vec<&str> = t.members.iter().map(|m| m.handle).collect();
        let _ = writeln!(
            out,
            "{}\n  {}\n  members: {}",
            t.id,
            t.summary,
            who.join(", ")
        );
    }
    let _ = writeln!(out, "\nSet one up: tempo-launch band new --template NAME");
    0
}

/// How `set_up` ended.
#[derive(Debug, PartialEq)]
pub enum Done {
    Applied,
    Declined,
    Failed,
}

impl Done {
    fn code(self) -> i32 {
        i32::from(self == Done::Failed)
    }
}

/// The same questions for `new` and `apply-proposal`: show the band in words, ask, save, put the
/// members in Room and schedule the flows.
pub fn set_up(
    ctx: &Ctx,
    band: &Band,
    d: &Deps,
    yes: bool,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> Done {
    let exists = band_dir(ctx, &band.name).join("band.json").is_file();
    let _ = writeln!(out, "\n{}", in_words(band, ctx.vault.is_dir()));
    if exists {
        let _ = writeln!(
            out,
            "A band called {} is already here. Saying yes replaces it; the old band.json goes to the trash folder first.\n",
            band.name
        );
    }
    if !yes && !ask("Set this band up? [y/N] ") {
        let _ = writeln!(out, "Nothing was changed.");
        return Done::Declined;
    }
    let file = match save(ctx, band, d.now) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            return Done::Failed;
        }
    };
    let _ = writeln!(out, "Saved {}", file.display());
    put_in_room(band, d, out);
    schedule_flows(band, d, out);
    let first = band
        .flows
        .first()
        .map_or(String::new(), |f| format!(" {}", f.name));
    let _ = writeln!(
        out,
        "\nThe band is ready. Try it:\n  tempo-launch band run {}{first}\n  tempo-launch band page {}",
        band.name, band.name
    );
    Done::Applied
}

fn put_in_room(band: &Band, d: &Deps, out: &mut dyn Write) {
    let later = format!("tempo-launch band room {}", band.name);
    let Some(link) = net::find_room(&d.room_dir) else {
        let _ = writeln!(
            out,
            "\nRoom is not running, so nobody is in a room yet. Start it with `tempo-launch room`, then add the band:\n  {later}"
        );
        return;
    };
    match net::register_band(&link, band) {
        Ok(r) => {
            let at: Vec<String> = r.added.iter().map(|h| format!("@{h}")).collect();
            if !at.is_empty() {
                let _ = writeln!(
                    out,
                    "\nIn Room, the room \"{}\" now has {}. Mention any of them like a colleague, for example @{} hello.",
                    band.name,
                    at.join(" "),
                    r.added[0]
                );
            }
            for (h, why) in &r.left_out {
                let _ = writeln!(out, "  @{h} did not join: {why}");
            }
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "\nRoom did not take the band ({e}). Try again later with:\n  {later}"
            );
        }
    }
}

fn schedule_flows(band: &Band, d: &Deps, out: &mut dyn Write) {
    let entries = match schedule::entries(band, &d.exe) {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(out, "\nThe flows were not scheduled: {e}");
            return;
        }
    };
    if entries.is_empty() {
        let _ = writeln!(out, "\nEvery flow runs only when you run it.");
        return;
    }
    match d.scheduler.install(&band.name, &entries) {
        Ok(()) => {
            let _ = writeln!(out, "\nScheduled (no window opens when they run):");
            for e in &entries {
                let _ = writeln!(out, "  {} {}", e.flow, e.every.words());
            }
        }
        Err(why) => {
            let _ = writeln!(
                out,
                "\nThe flows were not scheduled ({why}). To schedule them yourself, add:"
            );
            for e in &entries {
                let _ = writeln!(out, "  {}", e.line());
            }
        }
    }
}

fn new_band(
    ctx: &Ctx,
    cmd: BandCmd,
    d: &Deps,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let BandCmd::New {
        template,
        describe,
        name,
        yes,
    } = cmd
    else {
        return 2;
    };
    let band = match (template, describe) {
        (Some(t), _) => {
            let Some(t) = templates::find(&t) else {
                return 2;
            };
            t.band(name.as_deref().unwrap_or(t.id), &d.models)
        }
        (None, Some(text)) => {
            let _ = writeln!(
                out,
                "Asking the model on this computer to fit a band to what you do..."
            );
            let (band, how) = draft::from_description(&text, name.as_deref(), &d.models, d.chat);
            let _ = writeln!(out, "{how}");
            band
        }
        (None, None) => return 2,
    };
    set_up(ctx, &band, d, yes, out, ask).code()
}

fn room_cmd(ctx: &Ctx, name: &str, d: &Deps, out: &mut dyn Write) -> i32 {
    let band = match load(ctx, name) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            return 1;
        }
    };
    if net::find_room(&d.room_dir).is_none() {
        eprintln!("tempo-launch: Room is not running. Start it with `tempo-launch room`, then run this again.");
        return 1;
    }
    put_in_room(&band, d, out);
    0
}

fn run_flow_cmd(ctx: &Ctx, cmd: BandCmd, d: &Deps, out: &mut dyn Write) -> i32 {
    let BandCmd::Run { band, flow, note } = cmd else {
        return 2;
    };
    let band = match load(ctx, &band) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            return 1;
        }
    };
    let names: Vec<&str> = band.flows.iter().map(|f| f.name.as_str()).collect();
    let chosen = match flow.as_deref() {
        Some(f) => band.flow(f),
        None if band.flows.len() == 1 => band.flows.first(),
        None => {
            eprintln!(
                "tempo-launch: {} has several flows; say which: {}",
                band.name,
                names.join(", ")
            );
            return 2;
        }
    };
    let Some(flow) = chosen else {
        eprintln!(
            "tempo-launch: {} has no flow called {}; its flows are {}",
            band.name,
            flow.unwrap_or_default(),
            names.join(", ")
        );
        return 2;
    };
    match run::run_flow(ctx, &band, flow, note.as_deref(), d.chat, d.now, out) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            1
        }
    }
}

fn page_cmd(ctx: &Ctx, name: &str, out: &mut dyn Write) -> i32 {
    let band = match load(ctx, name) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            return 1;
        }
    };
    match page::write(ctx, &band) {
        Ok(file) => {
            let _ = writeln!(out, "Wrote {}", file.display());
            match ctx.open(&file.to_string_lossy()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("tempo-launch: the page did not open ({e}); open the file yourself");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            1
        }
    }
}

/// Run one `band` command with its outside world given.
pub fn dispatch(
    ctx: &Ctx,
    cmd: BandCmd,
    d: &Deps,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    match cmd {
        BandCmd::List => list(ctx, out),
        BandCmd::Templates => templates_list(out),
        BandCmd::New { .. } => new_band(ctx, cmd, d, out, ask),
        BandCmd::Run { .. } => run_flow_cmd(ctx, cmd, d, out),
        BandCmd::Page { band } => page_cmd(ctx, &band, out),
        BandCmd::Room { band } => room_cmd(ctx, &band, d, out),
        BandCmd::ApplyProposal { name } => {
            propose::apply_proposal(ctx, name.as_deref(), d, out, ask)
        }
    }
}

// ---- the real world

/// The agent, started when a band first needs a word from it.
struct RealChat<'a> {
    ctx: &'a Ctx,
}

impl Chat for RealChat<'_> {
    fn say(&self, model: &str, role: &str, text: &str) -> Result<String, String> {
        crate::ensure(self.ctx, &crate::launch_for(self.ctx, AGENT), true)
            .map_err(|p| format!("{}: {}", p.title, p.what))?;
        let agent = net::Agent::new(AGENT.port, crate::agent_key(self.ctx));
        if !agent.knows_roles() {
            return Err(
                "this agent is too old to give members their own role; update Perennos Tempo"
                    .into(),
            );
        }
        agent.say(model, role, text)
    }
}

fn real_models(ctx: &Ctx, now: u64) -> Models {
    match apply::compute(
        ctx,
        &probe::RealHost,
        &apply::Opts::default(),
        false,
        apply::ollama_port(),
        now,
    ) {
        Ok(c) => Models::from_plan(&c.plan),
        Err(e) => {
            eprintln!("tempo-launch: could not read this computer's model plan ({e}); members will use the agent's usual model");
            Models::default()
        }
    }
}

/// `tempo-launch band ...`.
pub fn run(ctx: &Ctx, cmd: BandCmd) -> i32 {
    let now = crate::unix_now();
    let wants_models = matches!(cmd, BandCmd::New { .. } | BandCmd::ApplyProposal { .. });
    let models = if wants_models {
        real_models(ctx, now)
    } else {
        Models::default()
    };
    let chat = RealChat { ctx };
    let sched = schedule::RealScheduler;
    let deps = Deps {
        chat: &chat,
        room_dir: net::room_dir(ctx),
        scheduler: &sched,
        models,
        exe: schedule::scheduled_exe(),
        now,
    };
    dispatch(
        ctx,
        cmd,
        &deps,
        &mut std::io::stdout(),
        &mut apply::ask_terminal,
    )
}

#[cfg(test)]
pub mod tests;
