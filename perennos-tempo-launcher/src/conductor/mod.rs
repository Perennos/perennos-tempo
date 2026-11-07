//! Tempo Conductor: ties the owner to their workflows and to their engines.
//!
//! The Conductor sets up bands (teams of AI members), runs them, and picks which engine plays
//! each part. `tempo-launch conductor new` sets a band up (from a template, or drafted by the
//! local model from a sentence about the work), puts each member in Room as a teammate to
//! @mention, schedules the flows and gives every member an engine. `conductor run` raises the
//! baton: it walks a flow through the agent, one member after the next, and files every step as
//! a linked note in the Vault. `conductor engines` shows every engine the owner has, and
//! `conductor page` writes one static page with the engines, the bands, the last runs and the
//! apps. `tempo-launch band ...` still works and means the same.
//!
//! The one rule: no member ever posts, sends, buys or deletes anything outside this computer.
//! Members chat as guests (no tools); the launcher writes their notes, and only under the band's
//! own notes folder.

pub mod budget;
pub mod checks;
pub mod draft;
pub mod engines;
pub mod net;
pub mod page;
pub mod propose;
pub mod run;
pub mod runlog;
pub mod runs;
pub mod schedule;
pub mod sections;
pub mod settings;
pub mod spec;
pub mod takes_file;
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

    /// `say`, for an answer that must follow `schema` (JSON Schema text, sent to the engine as
    /// its `format`). A stand-in that cannot hold the model to one keeps this default.
    fn say_json(
        &self,
        model: &str,
        role: &str,
        text: &str,
        schema: &str,
    ) -> Result<String, String> {
        let _ = schema;
        self.say(model, role, text)
    }

    /// `say`, asking for the engine at address `engine` (empty: the agent's own order). The
    /// answer says which engine played it, when the agent can tell. A stand-in that cannot
    /// choose an engine keeps this default: it ignores the address and reports none.
    fn say_on(&self, engine: &str, model: &str, role: &str, text: &str) -> Result<Said, String> {
        let _ = engine;
        self.say(model, role, text)
            .map(|text| Said { text, engine: None })
    }

    /// `say_on`, for the part `part` of band `band`: the agent puts the band's lessons in the
    /// part's prompt and picks its LoRA faders by the names. A stand-in keeps this default.
    fn say_for(
        &self,
        band: &str,
        part: &str,
        engine: &str,
        model: &str,
        role: &str,
        text: &str,
    ) -> Result<Said, String> {
        let _ = (band, part);
        self.say_on(engine, model, role, text)
    }

    /// One member's step inside a sectioned run: `say_on`, or with a `schema` the answer must
    /// follow, and ended at once when the run's meter says stop. The agent connection ends the
    /// conversation it started too. A stand-in keeps this default.
    fn say_run(
        &self,
        meter: &budget::Meter,
        engine: &str,
        model: &str,
        role: &str,
        text: &str,
        schema: &str,
    ) -> Result<Said, String> {
        if meter.halt().is_some() {
            return Err("stopped".into());
        }
        if schema.is_empty() {
            self.say_on(engine, model, role, text)
        } else {
            self.say_json(model, role, text, schema)
                .map(|text| Said { text, engine: None })
        }
    }

    /// This chat, as one that members on several threads can use at once. None: it cannot, and
    /// sections cannot be played with it.
    fn parallel(&self) -> Option<&(dyn Chat + Sync)> {
        None
    }
}

/// An answer and, when the agent said so, the address of the engine that gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    pub text: String,
    pub engine: Option<String>,
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
pub enum ConductorCmd {
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
        /// One band, or every band.
        band: Option<String>,
    },
    Engines,
    /// Play a flow as sections and takes.
    Sections {
        band: String,
        flow: Option<String>,
        note: Option<String>,
        takes: Option<u8>,
        resume: Option<String>,
    },
    /// The newest sectioned runs.
    Runs,
    /// End a run at its next step.
    Stop {
        run: Option<String>,
    },
    /// The owner's say on one take.
    Verdict {
        take: String,
        accept: bool,
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
            "conductor {what}: \"{b}\" is not a band name (lowercase letters, digits and hyphens)"
        )),
        None => Err(format!("conductor {what} needs the band's name")),
    }
}

pub fn parse_args(rest: &[&str]) -> Result<ConductorCmd, String> {
    let Some((verb, args)) = rest.split_first() else {
        return Ok(ConductorCmd::List);
    };
    match *verb {
        "templates" if args.is_empty() => Ok(ConductorCmd::Templates),
        "engines" if args.is_empty() => Ok(ConductorCmd::Engines),
        "new" => {
            let (mut template, mut describe, mut name, mut yes) = (None, None, None, false);
            let mut i = 0;
            let value = |i: &mut usize, flag: &str| -> Result<String, String> {
                *i += 1;
                args.get(*i)
                    .filter(|v| !v.starts_with("--") || flag == "--describe")
                    .map(|v| v.to_string())
                    .ok_or_else(|| format!("conductor new: {flag} needs a value"))
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
                    other => return Err(format!("conductor new: unknown argument {other}")),
                }
                i += 1;
            }
            match (&template, &describe) {
                (None, None) => {
                    return Err(
                        "conductor new needs --template NAME or --describe \"what you do\"".into(),
                    )
                }
                (Some(_), Some(_)) => {
                    return Err("conductor new takes --template or --describe, not both".into())
                }
                (Some(t), None) if templates::find(t).is_none() => {
                    let ids: Vec<&str> = templates::all().iter().map(|t| t.id).collect();
                    return Err(format!(
                        "conductor new: no template called {t}; the templates are {}",
                        ids.join(", ")
                    ));
                }
                (None, Some(d)) if d.trim().is_empty() => {
                    return Err("conductor new: --describe needs a sentence about your work".into())
                }
                _ => {}
            }
            if let Some(n) = &name {
                if !slug_ok(n, 40) {
                    return Err(format!(
                        "conductor new: \"{n}\" is not a band name (lowercase letters, digits and hyphens)"
                    ));
                }
            }
            Ok(ConductorCmd::New {
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
                                .ok_or("conductor run: --note needs some text")?
                                .to_string(),
                        );
                    }
                    f if !f.starts_with("--") && flow.is_none() => flow = Some(f.to_string()),
                    other => return Err(format!("conductor run: unknown argument {other}")),
                }
                i += 1;
            }
            if let Some(f) = &flow {
                if !slug_ok(f, 40) {
                    return Err(format!("conductor run: \"{f}\" is not a flow name"));
                }
            }
            Ok(ConductorCmd::Run { band, flow, note })
        }
        "sections" => {
            let band = band_arg(args.first(), "sections")?;
            let (mut flow, mut note, mut takes, mut resume) = (None, None, None, None);
            let mut i = 1;
            let value = |i: &mut usize, what: &str| -> Result<String, String> {
                *i += 1;
                args.get(*i)
                    .map(|v| v.to_string())
                    .ok_or_else(|| format!("conductor sections: {what} needs a value"))
            };
            while i < args.len() {
                match args[i] {
                    "--note" => note = Some(value(&mut i, "--note")?),
                    "--takes" => {
                        let v = value(&mut i, "--takes")?;
                        takes = Some(
                            v.parse::<u8>()
                                .ok()
                                .filter(|n| (1..=settings::MAX_TAKES).contains(n))
                                .ok_or_else(|| {
                                    format!(
                                        "conductor sections: --takes is 1 to {}",
                                        settings::MAX_TAKES
                                    )
                                })?,
                        );
                    }
                    "--resume" => {
                        let v = value(&mut i, "--resume")?;
                        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                            return Err("conductor sections: --resume takes a run id like r-1a2b3c4d".into());
                        }
                        resume = Some(v);
                    }
                    f if !f.starts_with("--") && flow.is_none() => flow = Some(f.to_string()),
                    other => return Err(format!("conductor sections: unknown argument {other}")),
                }
                i += 1;
            }
            if let Some(f) = &flow {
                if !slug_ok(f, 40) {
                    return Err(format!("conductor sections: \"{f}\" is not a flow name"));
                }
            }
            Ok(ConductorCmd::Sections {
                band,
                flow,
                note,
                takes,
                resume,
            })
        }
        "runs" if args.is_empty() => Ok(ConductorCmd::Runs),
        "stop" if args.len() <= 1 => Ok(ConductorCmd::Stop {
            run: args.first().map(|r| r.to_string()),
        }),
        "takes" => match args {
            [v @ ("accept" | "reject"), id] if !id.is_empty() && id.len() <= 80 => {
                Ok(ConductorCmd::Verdict {
                    take: id.to_string(),
                    accept: *v == "accept",
                })
            }
            _ => Err("conductor takes: accept TAKE-ID or reject TAKE-ID".into()),
        },
        "page" if args.len() <= 1 => Ok(ConductorCmd::Page {
            band: args
                .first()
                .map(|b| band_arg(Some(b), "page"))
                .transpose()?,
        }),
        "room" if args.len() == 1 => Ok(ConductorCmd::Room {
            band: band_arg(args.first(), "room")?,
        }),
        "apply-proposal" if args.len() <= 1 => Ok(ConductorCmd::ApplyProposal {
            name: args
                .first()
                .map(|n| band_arg(Some(n), "apply-proposal"))
                .transpose()?,
        }),
        "room" => Err("conductor room takes one band name".into()),
        "page" => Err("conductor page takes at most one band name".into()),
        "runs" => Err("conductor runs takes no arguments".into()),
        "stop" => Err("conductor stop takes at most one run id".into()),
        "templates" => Err("conductor templates takes no arguments".into()),
        "engines" => Err("conductor engines takes no arguments".into()),
        "apply-proposal" => Err("conductor apply-proposal takes at most one name".into()),
        other => Err(format!(
            "band: unknown command {other} (try: templates, new, run, sections, runs, stop, takes, engines, room, page, apply-proposal)"
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
            "no band called {name}. `tempo-launch conductor` lists yours; `tempo-launch conductor new --template social-media` sets one up"
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
    let wrong = engines::unknown(&band, &engines::known_ids(ctx));
    if !wrong.is_empty() {
        return Err(format!(
            "{} has problems:\n  - {}",
            file.display(),
            wrong.join("\n  - ")
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

/// What the Conductor looks at: the engines (each asked once, two seconds at most) and the apps.
pub trait Outside {
    fn engines(&self) -> Vec<engines::Engine>;
    fn apps(&self) -> Vec<page::AppState>;

    /// The repo checks a sectioned run judges takes with. None are set up unless the owner
    /// named a folder and a check command in conductor.json.
    fn checks(&self, ctx: &Ctx, settings: &settings::Settings) -> Box<dyn checks::Checks> {
        let _ = (ctx, settings);
        Box::new(checks::NoChecks)
    }
}

/// Everything the commands need from outside, so tests can stand in for the agent, Room and the
/// scheduler.
pub struct Deps<'a> {
    pub chat: &'a dyn Chat,
    /// The engines and the apps, as they are right now.
    pub outside: &'a dyn Outside,
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
            "No bands yet. The Conductor sets up a band for you: a small team of AI members for your own workflow, each playing on the engine that suits the part.\n\nTry one:\n  tempo-launch conductor templates\n  tempo-launch conductor new --template social-media\n  tempo-launch conductor new --describe \"I run a bakery's Instagram and want posts planned weekly\""
        );
        return 0;
    }
    let _ = writeln!(
        out,
        "{} {} up to the Conductor's baton.\n",
        all.len(),
        if all.len() == 1 {
            "band is"
        } else {
            "bands are"
        }
    );
    for (name, band) in all {
        match band {
            Ok(b) => {
                let who: Vec<String> = b
                    .members
                    .iter()
                    .map(|m| {
                        if m.engine.is_empty() {
                            format!("@{}", m.handle)
                        } else {
                            format!("@{} ({})", m.handle, m.engine)
                        }
                    })
                    .collect();
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
    let _ = writeln!(
        out,
        "\nSet one up: tempo-launch conductor new --template NAME"
    );
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
    let seen = d.outside.engines();
    let mut picked = band.clone();
    engines::assign(&mut picked, &seen);
    let band = &picked;
    let _ = writeln!(out, "\n{}", in_words(band, ctx.vault.is_dir()));
    if !seen.iter().any(|e| e.up) {
        let _ = writeln!(
            out,
            "No engine answered just now, so no member has an engine of its own yet: the agent uses its usual order. `tempo-launch conductor engines` shows what the Conductor sees.\n"
        );
    }
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
        "\nThe band is tuned and ready. Raise the baton:\n  tempo-launch conductor run {}{first}\n  tempo-launch conductor page {}",
        band.name, band.name
    );
    Done::Applied
}

fn put_in_room(band: &Band, d: &Deps, out: &mut dyn Write) {
    let later = format!("tempo-launch conductor room {}", band.name);
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
    cmd: ConductorCmd,
    d: &Deps,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let ConductorCmd::New {
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

fn run_flow_cmd(ctx: &Ctx, cmd: ConductorCmd, d: &Deps, out: &mut dyn Write) -> i32 {
    let ConductorCmd::Run { band, flow, note } = cmd else {
        return 2;
    };
    let mut band = match load(ctx, &band) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            return 1;
        }
    };
    // a model the Woodshed trained for this band plays the parts that played its base model
    let tuned = crate::woodshed::apply_tuned(ctx, &mut band);
    if !tuned.is_empty() {
        let _ = writeln!(
            out,
            "The Woodshed's version of {} plays for: {}. (`tempo-launch woodshed rollback {}` goes back.)",
            band.name,
            tuned.join(", "),
            band.name
        );
    }
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
    let cast = engines::cast(&band, &d.outside.engines());
    match run::run_flow_cast(ctx, &band, flow, note.as_deref(), d.chat, d.now, &cast, out) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            1
        }
    }
}

fn page_cmd(ctx: &Ctx, name: Option<&str>, d: &Deps, out: &mut dyn Write) -> i32 {
    let bands: Vec<Band> = match name {
        Some(n) => match load(ctx, n) {
            Ok(b) => vec![b],
            Err(e) => {
                eprintln!("tempo-launch: {e}");
                return 1;
            }
        },
        None => saved(ctx).into_iter().filter_map(|(_, b)| b.ok()).collect(),
    };
    let engines = d.outside.engines();
    let apps = d.outside.apps();
    match page::write(ctx, name, &bands, &engines, &apps) {
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

fn engines_cmd(d: &Deps, out: &mut dyn Write) -> i32 {
    let seen = d.outside.engines();
    let _ = writeln!(
        out,
        "The Conductor asked each of your engines what it has (two seconds each):\n"
    );
    let _ = write!(out, "{}", engines::describe(&seen));
    let up = seen.iter().filter(|e| e.up).count();
    let _ = writeln!(
        out,
        "\n{up} of {} up. Heavy parts (writing, planning, building) go to the strongest engine that has their model, light ones (sorting, scheduling) to the quickest. Add an engine with `tempo-launch link http://192.168.0.20:11434`.",
        seen.len()
    );
    0
}

/// Run one `conductor` command with its outside world given.
pub fn dispatch(
    ctx: &Ctx,
    cmd: ConductorCmd,
    d: &Deps,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    match cmd {
        ConductorCmd::List => list(ctx, out),
        ConductorCmd::Templates => templates_list(out),
        ConductorCmd::New { .. } => new_band(ctx, cmd, d, out, ask),
        ConductorCmd::Run { .. } => run_flow_cmd(ctx, cmd, d, out),
        ConductorCmd::Sections {
            band,
            flow,
            note,
            takes,
            resume,
        } => runs::sections_cmd(
            ctx,
            &band,
            flow.as_deref(),
            note.as_deref(),
            takes,
            resume.as_deref(),
            d,
            out,
        ),
        ConductorCmd::Runs => runs::runs_cmd(ctx, out),
        ConductorCmd::Stop { run } => runs::stop_cmd(ctx, run.as_deref(), out),
        ConductorCmd::Verdict { take, accept } => runs::verdict_cmd(ctx, &take, accept, out),
        ConductorCmd::Page { band } => page_cmd(ctx, band.as_deref(), d, out),
        ConductorCmd::Engines => engines_cmd(d, out),
        ConductorCmd::Room { band } => room_cmd(ctx, &band, d, out),
        ConductorCmd::ApplyProposal { name } => {
            propose::apply_proposal(ctx, name.as_deref(), d, out, ask)
        }
    }
}

// ---- the real world

/// The agent, started when a band first needs a word from it.
struct RealChat<'a> {
    ctx: &'a Ctx,
    /// The agent connection for sections, made on this thread once, then shared by many.
    par: std::sync::OnceLock<Option<net::Agent>>,
}

impl RealChat<'_> {
    fn agent(&self) -> Result<net::Agent, String> {
        crate::ensure(self.ctx, &crate::launch_for(self.ctx, AGENT), true)
            .map_err(|p| format!("{}: {}", p.title, p.what))?;
        let agent = net::Agent::new(AGENT.port, crate::agent_key(self.ctx));
        if !agent.knows_roles() {
            return Err(
                "this agent is too old to give members their own role; update Perennos Tempo"
                    .into(),
            );
        }
        Ok(agent)
    }
}

impl Chat for RealChat<'_> {
    fn say(&self, model: &str, role: &str, text: &str) -> Result<String, String> {
        self.agent()?.say(model, role, text)
    }

    fn say_on(&self, engine: &str, model: &str, role: &str, text: &str) -> Result<Said, String> {
        // an engine that is a fleet machine with its own Tempo plays the step itself
        if let Some(r) = crate::fleet::jobs::try_remote(self.ctx, engine, model, role, text) {
            return r;
        }
        self.agent()?.say_on(engine, model, role, text)
    }

    fn say_for(
        &self,
        band: &str,
        part: &str,
        engine: &str,
        model: &str,
        role: &str,
        text: &str,
    ) -> Result<Said, String> {
        if let Some(r) = crate::fleet::jobs::try_remote(self.ctx, engine, model, role, text) {
            return r;
        }
        self.agent()?.say_for(band, part, engine, model, role, text)
    }

    fn say_json(
        &self,
        model: &str,
        role: &str,
        text: &str,
        schema: &str,
    ) -> Result<String, String> {
        self.agent()?.say_json(model, role, text, schema)
    }

    /// Sections go straight to the agent. A fleet machine does not play a section: its engines
    /// are still named in the cast, but the agent here is the one asked.
    fn parallel(&self) -> Option<&(dyn Chat + Sync)> {
        self.par
            .get_or_init(|| self.agent().ok())
            .as_ref()
            .map(|a| a as &(dyn Chat + Sync))
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

/// The engines (looked at in the order the agent knows them) and the apps, for real.
struct RealOutside<'a> {
    ctx: &'a Ctx,
}

impl Outside for RealOutside<'_> {
    fn engines(&self) -> Vec<engines::Engine> {
        engines::probe_all(&engines::configured(self.ctx))
    }

    fn apps(&self) -> Vec<page::AppState> {
        page::real_apps(self.ctx)
    }

    fn checks(&self, ctx: &Ctx, s: &settings::Settings) -> Box<dyn checks::Checks> {
        match &s.workdir {
            Some(w) if w.is_dir() && (!s.check.is_empty() || s.soundcheck) => {
                Box::new(checks::RepoChecks {
                    workdir: w.clone(),
                    command: s.check.clone(),
                    soundcheck_exe: s.soundcheck.then(|| ctx.find(AGENT.bin)).flatten(),
                    scratch: runlog::runs_dir(ctx).join("work"),
                    timeout: s.check_timeout,
                })
            }
            _ => Box::new(checks::NoChecks),
        }
    }
}

/// `tempo-launch conductor ...` (and `band ...`).
pub fn run(ctx: &Ctx, cmd: ConductorCmd) -> i32 {
    let now = crate::unix_now();
    let wants_models = matches!(
        cmd,
        ConductorCmd::New { .. } | ConductorCmd::ApplyProposal { .. }
    );
    let models = if wants_models {
        real_models(ctx, now)
    } else {
        Models::default()
    };
    let chat = RealChat {
        ctx,
        par: std::sync::OnceLock::new(),
    };
    let sched = schedule::RealScheduler;
    let outside = RealOutside { ctx };
    let deps = Deps {
        chat: &chat,
        outside: &outside,
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
mod sections_tests;
#[cfg(test)]
pub mod tests;
