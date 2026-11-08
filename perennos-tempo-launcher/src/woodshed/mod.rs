//! The Woodshed (training): the part of Tempo that gets your local models better at their parts
//! from the takes your engines and bands produce. It runs at three speeds (docs/WOODSHED.md):
//!
//! 1. Lessons, at once, with no training: short notes per band that go into the prompt.
//! 2. The Woodshed itself, by night: a LoRA trained on your winning takes, gated by a Rehearsal
//!    against the base, versioned, and one command to roll back.
//! 3. The Mix, live: llama-server with adapters loaded, and a fader per band or part.
//!
//! Capture stays off until you turn it on for a band (`woodshed on <band>`). Nothing leaves your
//! computers: training runs on a machine of your own Fleet over your own ssh, and text a cloud model
//! wrote stays out of the training data unless `woodshed.allow_cloud_text` says your own contract
//! allows it.

pub mod dataset;
pub mod eval;
pub mod exec;
pub mod lessons;
pub mod mix;
pub mod node;
pub mod promote;
pub mod reflect;
pub mod script;
pub mod settings;
pub mod takes;
pub mod train;
pub mod versions;

use crate::conductor::spec::{slug_ok, Band};
use crate::setup::json::J;
use crate::Ctx;
use settings::Settings;
use std::io::Write;
use train::Deps;

/// One honest sentence, shown where the owner turns cloud text on.
pub const CLOUD_TEXT_SENTENCE: &str = "Anthropic's terms bar using its models' outputs to train competing models (consumer terms section 3, commercial terms D.4(a)), so turn this on only if your own contract with the provider allows it.";

#[derive(Debug, PartialEq)]
pub enum LessonAct {
    Show,
    Add(String),
    Edit(String, String),
    Retire(String),
    Restore(String),
}

#[derive(Debug, PartialEq)]
pub enum MixCmd {
    Show,
    Base(String),
    Add(String, Option<String>),
    Start { yes: bool },
    Set(String, Vec<(String, f64)>),
    Check,
    Global(Vec<(String, f64)>),
}

#[derive(Debug, PartialEq)]
pub enum WoodCmd {
    Status,
    Plan(Option<String>),
    Run {
        band: Option<String>,
        yes: bool,
    },
    Promote {
        band: Option<String>,
        yes: bool,
    },
    Rollback(Option<String>),
    Versions(Option<String>),
    Lessons {
        band: Option<String>,
        act: LessonAct,
    },
    On(String),
    Off(String),
    Set {
        band: String,
        key: String,
        value: String,
    },
    CloudText {
        on: bool,
        yes: bool,
    },
    Reflect {
        band: Option<String>,
        from: Option<String>,
    },
    Rehearse(Option<String>),
    Mix(MixCmd),
}

fn band_ok(b: &str) -> bool {
    b == "default" || slug_ok(b, 40)
}

fn band_arg(v: Option<&&str>, verb: &str) -> Result<String, String> {
    match v {
        Some(b) if band_ok(b) => Ok(b.to_string()),
        Some(b) => Err(format!("woodshed {verb}: \"{b}\" is not a band name (lowercase letters, digits and hyphens, or default)")),
        None => Err(format!("woodshed {verb} needs a band name")),
    }
}

fn opt_band(
    rest: &[&str],
    verb: &str,
    flags: &[&str],
) -> Result<(Option<String>, Vec<String>), String> {
    let (mut band, mut seen) = (None, vec![]);
    for a in rest {
        if flags.contains(a) {
            seen.push(a.to_string());
        } else if a.starts_with("--") {
            return Err(format!("woodshed {verb}: unknown argument {a}"));
        } else if band.is_none() {
            band = Some(band_arg(Some(a), verb)?);
        } else {
            return Err(format!("woodshed {verb} takes at most one band"));
        }
    }
    Ok((band, seen))
}

fn pairs(args: &[&str], verb: &str) -> Result<Vec<(String, f64)>, String> {
    args.iter()
        .map(|a| {
            let (n, v) = a
                .split_once('=')
                .ok_or_else(|| format!("woodshed {verb}: \"{a}\" should look like name=0.7"))?;
            let v: f64 = v
                .parse()
                .map_err(|_| format!("woodshed {verb}: \"{v}\" is not a number"))?;
            Ok((n.to_string(), v))
        })
        .collect()
}

pub fn parse_args(rest: &[&str]) -> Result<WoodCmd, String> {
    let Some((verb, args)) = rest.split_first() else {
        return Ok(WoodCmd::Status);
    };
    let none = |c: WoodCmd| {
        if args.is_empty() {
            Ok(c)
        } else {
            Err(format!("woodshed {verb} takes no arguments"))
        }
    };
    match *verb {
        "status" => none(WoodCmd::Status),
        "plan" => Ok(WoodCmd::Plan(opt_band(args, verb, &[])?.0)),
        "run" => {
            let (band, f) = opt_band(args, verb, &["--yes", "-y"])?;
            Ok(WoodCmd::Run { band, yes: !f.is_empty() })
        }
        "promote" => {
            let (band, f) = opt_band(args, verb, &["--yes", "-y"])?;
            Ok(WoodCmd::Promote { band, yes: !f.is_empty() })
        }
        "rollback" => Ok(WoodCmd::Rollback(opt_band(args, verb, &[])?.0)),
        "versions" => Ok(WoodCmd::Versions(opt_band(args, verb, &[])?.0)),
        "rehearse" => Ok(WoodCmd::Rehearse(opt_band(args, verb, &[])?.0)),
        "on" | "off" => {
            let b = band_arg(args.first(), verb)?;
            if args.len() > 1 {
                return Err(format!("woodshed {verb} takes one band"));
            }
            Ok(if *verb == "on" { WoodCmd::On(b) } else { WoodCmd::Off(b) })
        }
        "set" => match args {
            [b, k @ ("base" | "hf"), v] if band_ok(b) && !v.trim().is_empty() => Ok(WoodCmd::Set { band: b.to_string(), key: k.to_string(), value: v.trim().to_string() }),
            _ => Err("woodshed set takes a band, base or hf, and a value: `woodshed set social-media base qwen3:8b`".into()),
        },
        "allow-cloud-text" => match args {
            [w @ ("on" | "off")] => Ok(WoodCmd::CloudText { on: *w == "on", yes: false }),
            [w @ ("on" | "off"), "--yes"] => Ok(WoodCmd::CloudText { on: *w == "on", yes: true }),
            _ => Err("woodshed allow-cloud-text takes on or off".into()),
        },
        "reflect" => {
            let mut from = None;
            let mut plain = vec![];
            let mut i = 0;
            while i < args.len() {
                if args[i] == "--from" {
                    i += 1;
                    from = Some(args.get(i).ok_or("woodshed reflect: --from needs a file (or - for what is piped in)")?.to_string());
                } else {
                    plain.push(args[i]);
                }
                i += 1;
            }
            let (band, _) = opt_band(&plain, verb, &[])?;
            if from.is_some() && band.is_none() {
                return Err("woodshed reflect --from FILE needs the band the notes are for".into());
            }
            Ok(WoodCmd::Reflect { band, from })
        }
        "lessons" => {
            let mut band = None;
            let mut act = LessonAct::Show;
            let mut i = 0;
            let take = |i: &mut usize, n: usize, flag: &str| -> Result<Vec<String>, String> {
                let got: Vec<String> = args.iter().skip(*i + 1).take(n).map(|s| s.to_string()).collect();
                if got.len() < n {
                    return Err(format!("woodshed lessons: {flag} needs {}", if n == 2 { "an id and the new text" } else if flag == "--add" { "the lesson's text" } else { "an id" }));
                }
                *i += n;
                Ok(got)
            };
            while i < args.len() {
                match args[i] {
                    "--add" => act = LessonAct::Add(take(&mut i, 1, "--add")?.remove(0)),
                    "--retire" => act = LessonAct::Retire(take(&mut i, 1, "--retire")?.remove(0)),
                    "--restore" => act = LessonAct::Restore(take(&mut i, 1, "--restore")?.remove(0)),
                    "--edit" => {
                        let mut v = take(&mut i, 2, "--edit")?;
                        let text = v.remove(1);
                        act = LessonAct::Edit(v.remove(0), text);
                    }
                    a if a.starts_with("--") => return Err(format!("woodshed lessons: unknown argument {a}")),
                    b if band.is_none() => band = Some(band_arg(Some(&b), "lessons")?),
                    _ => return Err("woodshed lessons takes at most one band".into()),
                }
                i += 1;
            }
            if act != LessonAct::Show && band.is_none() {
                band = Some("default".into());
            }
            Ok(WoodCmd::Lessons { band, act })
        }
        "mix" => {
            let Some((v, a)) = args.split_first() else {
                return Ok(WoodCmd::Mix(MixCmd::Show));
            };
            Ok(WoodCmd::Mix(match (*v, a) {
                ("show" | "plan", []) => MixCmd::Show,
                ("check", []) => MixCmd::Check,
                ("base", [p]) => MixCmd::Base(p.to_string()),
                ("add", [n]) => MixCmd::Add(n.to_string(), None),
                ("add", [n, p]) => MixCmd::Add(n.to_string(), Some(p.to_string())),
                ("start", []) => MixCmd::Start { yes: false },
                ("start", ["--yes"]) => MixCmd::Start { yes: true },
                ("set", [t, rest @ ..]) if !rest.is_empty() => MixCmd::Set(t.to_string(), pairs(rest, "mix set")?),
                ("global", rest) if !rest.is_empty() => MixCmd::Global(pairs(rest, "mix global")?),
                _ => return Err("woodshed mix: try show, base PATH, add NAME [PATH], start, set <band>[/<part>] name=scale ..., check, global name=scale ...".into()),
            }))
        }
        other => Err(format!(
            "woodshed: unknown command {other} (try: status, plan, run, promote, rollback, versions, lessons, on, off, set, allow-cloud-text, reflect, rehearse, mix)"
        )),
    }
}

/// For the Conductor: put a band's promoted model in for the members that play its base model.
/// Returns the handles that changed.
pub fn apply_tuned(ctx: &Ctx, band: &mut Band) -> Vec<String> {
    let mut changed = vec![];
    if let Some((base, model)) = versions::tuned(ctx, &band.name) {
        for m in band.members.iter_mut().filter(|m| m.model == base) {
            m.model = model.clone();
            changed.push(m.handle.clone());
        }
    }
    changed
}

fn known_band(ctx: &Ctx, b: &str) -> bool {
    b == "default"
        || crate::conductor::band_dir(ctx, b)
            .join("band.json")
            .is_file()
}

/// The band a command means: the one named, else the only one with versions.
fn which(ctx: &Ctx, given: &Option<String>, out: &mut dyn Write) -> Option<String> {
    if let Some(b) = given {
        return Some(b.clone());
    }
    let have = versions::bands_with_versions(ctx);
    match have.as_slice() {
        [one] => Some(one.clone()),
        [] => {
            let _ = writeln!(out, "Nothing in the woodshed has a version yet.");
            None
        }
        many => {
            let _ = writeln!(out, "Which band? {}.", many.join(", "));
            None
        }
    }
}

fn status(ctx: &Ctx, out: &mut dyn Write) -> i32 {
    let set = Settings::load(ctx);
    let on = set.bands_on();
    let have = versions::bands_with_versions(ctx);
    if on.is_empty() && have.is_empty() {
        let _ = writeln!(out, "Nothing in the woodshed yet. Turn it on for a band with `tempo-launch woodshed on <band>`.");
        return 0;
    }
    let (all, bad) = takes::read_takes(ctx);
    let _ = writeln!(
        out,
        "Woodshed (training): {} band{} on.\n",
        on.len(),
        if on.len() == 1 { "" } else { "s" }
    );
    let mut names = on.clone();
    names.extend(have.iter().filter(|b| !on.contains(b)).cloned());
    for b in names {
        let mine: Vec<&takes::Take> = all.iter().filter(|t| takes::key(t) == b).collect();
        let winners = mine.iter().filter(|t| t.winner).count();
        let usable = mine
            .iter()
            .filter(|t| t.winner && dataset::usable_author(t, set.allow_cloud_text()))
            .count();
        let ls = lessons::load(ctx, &b);
        let active = ls.iter().filter(|l| !l.retired).count();
        let v = versions::list(ctx, &b);
        let playing = versions::active(ctx, &b).and_then(|a| a.model);
        let _ = writeln!(
            out,
            "{b}: {}. {} take{} kept, {winners} winners ({usable} usable for training, {} needed). {active} lesson{} active. {} version{}. Playing: {}.",
            if set.is_on(&b) { "on" } else { "off" },
            mine.len(),
            if mine.len() == 1 { "" } else { "s" },
            set.min_examples(),
            if active == 1 { "" } else { "s" },
            v.len(),
            if v.len() == 1 { "" } else { "s" },
            playing.as_deref().unwrap_or("the plain base"),
        );
    }
    let _ = writeln!(
        out,
        "\nText a cloud model wrote: {}.",
        if set.allow_cloud_text() {
            "included in training (woodshed.allow_cloud_text is on)"
        } else {
            "left out of training (woodshed.allow_cloud_text is off)"
        }
    );
    if bad > 0 {
        let _ = writeln!(
            out,
            "{bad} line{} in the takes file could not be read and {} skipped.",
            if bad == 1 { "" } else { "s" },
            if bad == 1 { "was" } else { "were" }
        );
    }
    0
}

fn lessons_cmd(
    ctx: &Ctx,
    band: &Option<String>,
    act: &LessonAct,
    now: u64,
    out: &mut dyn Write,
) -> i32 {
    let set = Settings::load(ctx);
    let cap = set.lesson_tokens();
    let Some(key) = band else {
        // every band that has a file
        let mut keys: Vec<String> = std::fs::read_dir(lessons::dir(ctx))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(".jsonl").map(String::from))
            .collect();
        keys.sort();
        if keys.is_empty() {
            let _ = writeln!(out, "No lessons yet. Tempo writes them after judged takes (`tempo-launch woodshed reflect <band>`), or you can: `tempo-launch woodshed lessons <band> --add \"what to do next time\"`.");
        }
        for k in keys {
            lessons::show(ctx, &k, out);
            let _ = writeln!(out);
        }
        return 0;
    };
    let r = match act {
        LessonAct::Show => {
            lessons::show(ctx, key, out);
            return 0;
        }
        LessonAct::Add(t) => {
            lessons::add(ctx, key, t, "owner", now).map(|l| format!("Added {}: {}", l.id, l.text))
        }
        LessonAct::Edit(id, t) => lessons::edit(ctx, key, id, t, now)
            .map(|()| format!("Edited {id}. The old words stay in the file.")),
        LessonAct::Retire(id) => lessons::retire(ctx, key, id, "retired by you", now)
            .map(|()| format!("Retired {id}. It is kept, and `--restore {id}` brings it back.")),
        LessonAct::Restore(id) => lessons::restore(ctx, key, id, now)
            .map(|()| format!("Restored {id}, with its count started again.")),
    };
    match r.and_then(|m| lessons::render(ctx, key, cap).map(|n| (m, n))) {
        Ok((m, n)) => {
            let _ = writeln!(
                out,
                "{m}\n{n} lesson{} go into {key}'s prompt.",
                if n == 1 { "" } else { "s" }
            );
            0
        }
        Err(e) => {
            let _ = writeln!(out, "{e}");
            1
        }
    }
}

fn turn(ctx: &Ctx, band: &str, on: bool, now: u64, out: &mut dyn Write) -> i32 {
    if on && !known_band(ctx, band) {
        let _ = writeln!(out, "No band called {band}. `tempo-launch conductor` lists yours; `default` is the usual chat.");
        return 1;
    }
    let mut s = Settings::load(ctx);
    s.set_band(band, "on", J::Bool(on));
    if let Err(e) = s.save(ctx, now) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    if on {
        let _ = writeln!(
            out,
            "Woodshed (training) is on for {band}. From now on Tempo keeps the judged takes of {band} in {} (a thumb in chat counts), and learns lessons from them.\nNothing leaves your computers: training runs on a machine of your own Fleet, over your own ssh, and only after a y/N. Turn it off with `tempo-launch woodshed off {band}`; nothing already kept is deleted.",
            takes::takes_file(ctx).display()
        );
    } else {
        let _ = writeln!(out, "Woodshed is off for {band}. Takes, lessons and versions already kept stay where they are.");
    }
    0
}

/// Everything the command needs from the world; `run` builds the real ones.
#[allow(clippy::too_many_arguments)]
pub fn run_with(
    ctx: &Ctx,
    cmd: WoodCmd,
    d: &Deps,
    now: u64,
    spawn: mix::Spawn,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let list = |b: &Option<String>| b.iter().cloned().collect::<Vec<_>>();
    match cmd {
        WoodCmd::Status => status(ctx, out),
        WoodCmd::Plan(b) => train::plan(ctx, d, &list(&b), out),
        WoodCmd::Run { band, yes } => train::run(ctx, d, &list(&band), yes, now, ask, out),
        WoodCmd::Rehearse(b) => match which(ctx, &b, out) {
            Some(b) => train::rehearse(ctx, d, &b, out),
            None => 0,
        },
        WoodCmd::Promote { band, yes } => match which(ctx, &band, out) {
            Some(b) => promote::promote(ctx, d.exec, &b, yes, now, ask, out),
            None => 0,
        },
        WoodCmd::Rollback(b) => match which(ctx, &b, out) {
            Some(b) => promote::rollback(ctx, &b, now, out),
            None => 0,
        },
        WoodCmd::Versions(b) => promote::list(ctx, &list(&b), out),
        WoodCmd::Lessons { band, act } => lessons_cmd(ctx, &band, &act, now, out),
        WoodCmd::On(b) => turn(ctx, &b, true, now, out),
        WoodCmd::Off(b) => turn(ctx, &b, false, now, out),
        WoodCmd::Set { band, key, value } => {
            let mut s = Settings::load(ctx);
            s.set_band(&band, &key, J::s(&value));
            match s.save(ctx, now) {
                Ok(()) => {
                    let _ = writeln!(out, "{band}: {key} is {value}.");
                    0
                }
                Err(e) => {
                    let _ = writeln!(out, "{e}");
                    1
                }
            }
        }
        WoodCmd::CloudText { on, yes } => {
            let mut s = Settings::load(ctx);
            if on {
                let _ = writeln!(out, "{CLOUD_TEXT_SENTENCE}");
                if !(yes || ask("Include text a cloud model wrote in your training data? [y/N] ")) {
                    let _ = writeln!(out, "Left off. Cloud text stays out of training.");
                    return 0;
                }
            }
            s.set_top("allow_cloud_text", J::Bool(on));
            match s.save(ctx, now) {
                Ok(()) => {
                    let _ = writeln!(
                        out,
                        "woodshed.allow_cloud_text is {}.",
                        if on { "on" } else { "off" }
                    );
                    0
                }
                Err(e) => {
                    let _ = writeln!(out, "{e}");
                    1
                }
            }
        }
        WoodCmd::Reflect { band, from } => {
            let s = Settings::load(ctx);
            if let (Some(file), Some(b)) = (&from, &band) {
                let text = if file == "-" {
                    let mut t = String::new();
                    let _ = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut t);
                    Ok(t)
                } else {
                    std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))
                };
                return match text
                    .and_then(|t| reflect::from_text(ctx, b, s.lesson_tokens(), &t, now, out))
                {
                    Ok(()) => 0,
                    Err(e) => {
                        let _ = writeln!(out, "{e}");
                        1
                    }
                };
            }
            let bands = if band.is_some() {
                list(&band)
            } else {
                s.bands_on()
            };
            if bands.is_empty() {
                let _ = writeln!(out, "Nothing in the woodshed yet. Turn it on for a band with `tempo-launch woodshed on <band>`.");
                return 0;
            }
            let mut code = 0;
            for b in bands {
                let model = s.text("reflector_model").unwrap_or_default();
                match reflect::run(ctx, &b, s.lesson_tokens(), d.chat, &model, now, out) {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = writeln!(out, "No new judged takes for {b}.");
                    }
                    Err(e) => {
                        let _ = writeln!(out, "Could not reflect on {b}: {e}");
                        code = 1;
                    }
                }
            }
            code
        }
        WoodCmd::Mix(m) => match m {
            MixCmd::Show => mix::show(ctx, out),
            MixCmd::Base(p) => mix::set_base(ctx, &p, out),
            MixCmd::Add(name, path) => {
                let p = path.or_else(|| adapter_of(ctx, &name));
                match p {
                    Some(p) => mix::add(ctx, &name, &p, out),
                    None => {
                        let _ = writeln!(out, "No promoted adapter for {name} yet. Give the file: `tempo-launch woodshed mix add {name} /path/to/adapter.gguf`.");
                        1
                    }
                }
            }
            MixCmd::Start { yes } => mix::start(
                ctx,
                yes,
                now,
                spawn,
                std::time::Duration::from_secs(180),
                ask,
                out,
            ),
            MixCmd::Set(t, p) => mix::set(ctx, &t, &p, now, out),
            MixCmd::Check => mix::check(ctx, out),
            MixCmd::Global(p) => mix::global(ctx, &p, out),
        },
    }
}

/// The adapter of the version that plays for `band`, else its newest one that passed.
fn adapter_of(ctx: &Ctx, band: &str) -> Option<String> {
    let all = versions::list(ctx, band);
    let playing = versions::active(ctx, band).and_then(|a| a.version);
    let m = all
        .iter()
        .find(|m| Some(&m.version) == playing.as_ref())
        .or_else(|| all.iter().rev().find(|m| m.passed()))?;
    let p = versions::version_dir(ctx, band, &m.version).join(&m.adapter_file);
    p.is_file().then(|| p.to_string_lossy().into_owned())
}

/// The real command.
pub fn run(ctx: &Ctx, cmd: WoodCmd) -> i32 {
    let exec = exec::RealExec;
    let gen = eval::OllamaGen {
        port: crate::setup::apply::ollama_port(),
    };
    let agent = crate::conductor::net::Agent::new(crate::AGENT.port, crate::agent_key(ctx));
    let mut nodes: Vec<node::Node> = node::local(&crate::setup::probe::RealHost)
        .into_iter()
        .collect();
    // the fleet is only read when it is needed
    if matches!(cmd, WoodCmd::Plan(_) | WoodCmd::Run { .. }) {
        let f = crate::fleet::store::load(&crate::fleet::store::Paths::of(ctx));
        nodes.extend(node::fleet_nodes(&f, &crate::fleet::ssh::user_hosts()));
    }
    let d = Deps {
        exec: &exec,
        gen: &gen,
        chat: &agent,
        nodes,
        windows: cfg!(windows),
    };
    run_with(
        ctx,
        cmd,
        &d,
        crate::unix_now(),
        &mix::real_spawn,
        &mut crate::setup::apply::ask_terminal,
        &mut std::io::stdout(),
    )
}

#[cfg(test)]
mod tests {
    use super::takes::test_ctx;
    use super::train::tests::{cuda_node, FakeExec, FakeGen};
    use super::*;
    use crate::conductor::Chat;

    fn p(a: &[&str]) -> Result<WoodCmd, String> {
        parse_args(a)
    }

    #[test]
    fn the_subcommands_parse() {
        assert_eq!(p(&[]).unwrap(), WoodCmd::Status);
        assert_eq!(p(&["status"]).unwrap(), WoodCmd::Status);
        assert_eq!(p(&["plan"]).unwrap(), WoodCmd::Plan(None));
        assert_eq!(
            p(&["plan", "social-media"]).unwrap(),
            WoodCmd::Plan(Some("social-media".into()))
        );
        assert_eq!(
            p(&["run", "--yes"]).unwrap(),
            WoodCmd::Run {
                band: None,
                yes: true
            }
        );
        assert_eq!(
            p(&["run", "dev-team"]).unwrap(),
            WoodCmd::Run {
                band: Some("dev-team".into()),
                yes: false
            }
        );
        assert_eq!(
            p(&["promote", "b1", "-y"]).unwrap(),
            WoodCmd::Promote {
                band: Some("b1".into()),
                yes: true
            }
        );
        assert_eq!(p(&["rollback"]).unwrap(), WoodCmd::Rollback(None));
        assert_eq!(
            p(&["versions", "default"]).unwrap(),
            WoodCmd::Versions(Some("default".into()))
        );
        assert_eq!(
            p(&["on", "social-media"]).unwrap(),
            WoodCmd::On("social-media".into())
        );
        assert_eq!(
            p(&["off", "default"]).unwrap(),
            WoodCmd::Off("default".into())
        );
        assert_eq!(
            p(&["set", "b1", "base", "qwen3:8b"]).unwrap(),
            WoodCmd::Set {
                band: "b1".into(),
                key: "base".into(),
                value: "qwen3:8b".into()
            }
        );
        assert_eq!(
            p(&["allow-cloud-text", "on"]).unwrap(),
            WoodCmd::CloudText {
                on: true,
                yes: false
            }
        );
        assert_eq!(
            p(&["reflect", "b1", "--from", "-"]).unwrap(),
            WoodCmd::Reflect {
                band: Some("b1".into()),
                from: Some("-".into())
            }
        );
        assert_eq!(
            p(&["lessons"]).unwrap(),
            WoodCmd::Lessons {
                band: None,
                act: LessonAct::Show
            }
        );
        assert_eq!(
            p(&["lessons", "b1", "--add", "say it plainly please"]).unwrap(),
            WoodCmd::Lessons {
                band: Some("b1".into()),
                act: LessonAct::Add("say it plainly please".into())
            }
        );
        assert_eq!(
            p(&["lessons", "b1", "--edit", "L1", "new words here"]).unwrap(),
            WoodCmd::Lessons {
                band: Some("b1".into()),
                act: LessonAct::Edit("L1".into(), "new words here".into())
            }
        );
        assert_eq!(
            p(&["lessons", "--retire", "L1"]).unwrap(),
            WoodCmd::Lessons {
                band: Some("default".into()),
                act: LessonAct::Retire("L1".into())
            }
        );
        assert_eq!(p(&["mix"]).unwrap(), WoodCmd::Mix(MixCmd::Show));
        assert_eq!(
            p(&["mix", "add", "b1"]).unwrap(),
            WoodCmd::Mix(MixCmd::Add("b1".into(), None))
        );
        assert_eq!(
            p(&["mix", "start", "--yes"]).unwrap(),
            WoodCmd::Mix(MixCmd::Start { yes: true })
        );
        assert_eq!(
            p(&["mix", "set", "b1/lead", "b1=0.7", "b2=0"]).unwrap(),
            WoodCmd::Mix(MixCmd::Set(
                "b1/lead".into(),
                vec![("b1".into(), 0.7), ("b2".into(), 0.0)]
            ))
        );
    }

    #[test]
    fn mistakes_are_said_in_words() {
        for (a, want) in [
            (vec!["nope"], "unknown command nope"),
            (vec!["on"], "needs a band name"),
            (vec!["on", "Bad Name"], "not a band name"),
            (vec!["run", "aa", "bb"], "at most one band"),
            (vec!["run", "--fast"], "unknown argument --fast"),
            (vec!["set", "b1", "color", "red"], "base or hf"),
            (vec!["allow-cloud-text"], "on or off"),
            (vec!["status", "x"], "takes no arguments"),
            (vec!["lessons", "b1", "--add"], "needs the lesson's text"),
            (
                vec!["lessons", "b1", "--edit", "L1"],
                "an id and the new text",
            ),
            (vec!["reflect", "--from", "f.json"], "needs the band"),
            (vec!["mix", "set", "b1"], "woodshed mix:"),
            (vec!["mix", "set", "b1", "oops"], "name=0.7"),
            (vec!["mix", "set", "b1", "a=loud"], "not a number"),
        ] {
            let e = p(&a).unwrap_err();
            assert!(e.contains(want), "{a:?}: {e}");
        }
    }

    struct Quiet;
    impl Chat for Quiet {
        fn say(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
            Ok(
                "{\"add\":[\"Name the instrument in the first line.\"],\"helped\":[],\"hurt\":[]}"
                    .into(),
            )
        }
    }

    fn go(ctx: &Ctx, cmd: WoodCmd, yes_to_all: bool) -> (i32, String) {
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        let d = Deps {
            exec: &e,
            gen: &g,
            chat: &Quiet,
            nodes: vec![cuda_node(exec::Where::Local)],
            windows: false,
        };
        let mut out = Vec::new();
        let code = run_with(
            ctx,
            cmd,
            &d,
            1_794_000_000,
            &|_, _| Ok(()),
            &mut |_| yes_to_all,
            &mut out,
        );
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn with_nothing_trained_it_says_so_and_how_to_begin() {
        let ctx = test_ctx("w-empty");
        let (code, text) = go(&ctx, WoodCmd::Status, false);
        assert_eq!(code, 0);
        assert_eq!(text.trim(), "Nothing in the woodshed yet. Turn it on for a band with `tempo-launch woodshed on <band>`.");
        for c in [
            WoodCmd::Versions(None),
            WoodCmd::Rollback(None),
            WoodCmd::Promote {
                band: None,
                yes: false,
            },
        ] {
            let (code, text) = go(&ctx, c, false);
            assert_eq!(code, 0);
            assert!(
                text.contains("No versions yet")
                    || text.contains("Nothing in the woodshed has a version yet"),
                "{text}"
            );
        }
    }

    #[test]
    fn on_needs_a_real_band_keeps_nothing_until_then_and_off_deletes_nothing() {
        let ctx = test_ctx("w-on");
        let (code, text) = go(&ctx, WoodCmd::On("ghost".into()), false);
        assert_eq!(code, 1);
        assert!(
            text.contains("No band called ghost") && text.contains("`default` is the usual chat"),
            "{text}"
        );
        assert!(!settings::path(&ctx).exists());
        // the usual chat is always a band
        let (code, text) = go(&ctx, WoodCmd::On("default".into()), false);
        assert_eq!(code, 0);
        assert!(
            text.contains("Woodshed (training) is on for default")
                && text.contains("Nothing leaves your computers"),
            "{text}"
        );
        assert!(Settings::load(&ctx).is_on("default"));
        // and a conductor band
        let band_dir = crate::conductor::band_dir(&ctx, "social-media");
        std::fs::create_dir_all(&band_dir).unwrap();
        std::fs::write(band_dir.join("band.json"), "{}").unwrap();
        assert_eq!(go(&ctx, WoodCmd::On("social-media".into()), false).0, 0);
        let (_, text) = go(&ctx, WoodCmd::Status, false);
        assert!(
            text.contains("Woodshed (training): 2 bands on.")
                && text.contains("social-media: on.")
                && text.contains("default: on."),
            "{text}"
        );
        assert!(
            text.contains("left out of training (woodshed.allow_cloud_text is off)"),
            "{text}"
        );
        let (_, text) = go(&ctx, WoodCmd::Off("social-media".into()), false);
        assert!(text.contains("stay where they are"), "{text}");
        assert!(!Settings::load(&ctx).is_on("social-media"));
    }

    #[test]
    fn cloud_text_is_off_and_turning_it_on_says_why_and_asks() {
        let ctx = test_ctx("w-cloud");
        let (_, text) = go(
            &ctx,
            WoodCmd::CloudText {
                on: true,
                yes: false,
            },
            false,
        );
        assert!(
            text.contains("consumer terms section 3")
                && text.contains("commercial terms D.4(a)")
                && text.contains("only if your own contract"),
            "{text}"
        );
        assert!(text.contains("Left off."), "{text}");
        assert!(!Settings::load(&ctx).allow_cloud_text());
        let (_, text) = go(
            &ctx,
            WoodCmd::CloudText {
                on: true,
                yes: false,
            },
            true,
        );
        assert!(text.contains("woodshed.allow_cloud_text is on."), "{text}");
        assert!(Settings::load(&ctx).allow_cloud_text());
        go(
            &ctx,
            WoodCmd::CloudText {
                on: false,
                yes: false,
            },
            false,
        );
        assert!(!Settings::load(&ctx).allow_cloud_text());
        // one sentence
        assert_eq!(CLOUD_TEXT_SENTENCE.matches(". ").count(), 0);
    }

    #[test]
    fn the_owner_edits_lessons_and_the_prompt_file_follows() {
        let ctx = test_ctx("w-lessons");
        let (code, text) = go(
            &ctx,
            WoodCmd::Lessons {
                band: Some("social-media".into()),
                act: LessonAct::Add("Open with the number, not the adjective.".into()),
            },
            false,
        );
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("1 lesson go into social-media's prompt."),
            "{text}"
        );
        let id = lessons::load(&ctx, "social-media")[0].id.clone();
        assert_eq!(lessons::load(&ctx, "social-media")[0].source, "owner");
        let prompt = ctx.agent_data.join("lessons/social-media.prompt.txt");
        assert!(std::fs::read_to_string(&prompt)
            .unwrap()
            .contains("Open with the number"));
        go(
            &ctx,
            WoodCmd::Lessons {
                band: Some("social-media".into()),
                act: LessonAct::Retire(id.clone()),
            },
            false,
        );
        assert_eq!(std::fs::read_to_string(&prompt).unwrap(), "");
        let (_, text) = go(
            &ctx,
            WoodCmd::Lessons {
                band: Some("social-media".into()),
                act: LessonAct::Show,
            },
            false,
        );
        assert!(text.contains("Retired (kept, not played): 1"), "{text}");
        go(
            &ctx,
            WoodCmd::Lessons {
                band: Some("social-media".into()),
                act: LessonAct::Restore(id),
            },
            false,
        );
        assert!(std::fs::read_to_string(&prompt)
            .unwrap()
            .contains("Open with the number"));
        // no band named: every file is shown
        let (_, text) = go(
            &ctx,
            WoodCmd::Lessons {
                band: None,
                act: LessonAct::Show,
            },
            false,
        );
        assert!(
            text.contains("Lessons for social-media: 1 active."),
            "{text}"
        );
        let (code, text) = go(
            &ctx,
            WoodCmd::Lessons {
                band: Some("x1".into()),
                act: LessonAct::Retire("Lnope".into()),
            },
            false,
        );
        assert_eq!(code, 1);
        assert!(text.contains("no lesson Lnope"), "{text}");
    }

    #[test]
    fn reflect_with_a_local_model_and_with_the_owners_cloud_model() {
        let ctx = train::tests::world("w-reflect", 3);
        let (code, text) = go(
            &ctx,
            WoodCmd::Reflect {
                band: None,
                from: None,
            },
            false,
        );
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("Reflected on 3 takes for b")
                && text.contains("new lesson: Name the instrument"),
            "{text}"
        );
        assert_eq!(lessons::load(&ctx, "b")[0].source, "local");
        let (_, text) = go(
            &ctx,
            WoodCmd::Reflect {
                band: Some("b".into()),
                from: None,
            },
            false,
        );
        assert!(text.contains("No new judged takes for b."), "{text}");
        // the owner's cloud model hands a file over
        let f = ctx.home.join("delta.json");
        std::fs::write(
            &f,
            "{\"add\":[\"Prefer a number to an adjective.\"],\"helped\":[],\"hurt\":[]}",
        )
        .unwrap();
        let (code, text) = go(
            &ctx,
            WoodCmd::Reflect {
                band: Some("b".into()),
                from: Some(f.to_string_lossy().into_owned()),
            },
            false,
        );
        assert_eq!(code, 0, "{text}");
        let l = lessons::load(&ctx, "b");
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].source, "cloud");
        let (code, text) = go(
            &ctx,
            WoodCmd::Reflect {
                band: Some("b".into()),
                from: Some("/no/such/file.json".into()),
            },
            false,
        );
        assert_eq!(code, 1);
        assert!(text.contains("cannot read"), "{text}");
    }

    #[test]
    fn status_counts_takes_winners_and_what_plays() {
        let ctx = train::tests::world("w-status", 5);
        let (_, text) = go(&ctx, WoodCmd::Status, false);
        assert!(text.contains("b: on. 5 takes kept, 5 winners (5 usable for training, 40 needed). 0 lessons active. 0 versions. Playing: the plain base."), "{text}");
    }

    #[test]
    fn set_and_the_conductor_hook() {
        let ctx = test_ctx("w-set");
        let (_, text) = go(
            &ctx,
            WoodCmd::Set {
                band: "b".into(),
                key: "hf".into(),
                value: "org/model".into(),
            },
            false,
        );
        assert!(text.contains("b: hf is org/model."), "{text}");
        assert_eq!(
            Settings::load(&ctx).band_text("b", "hf").as_deref(),
            Some("org/model")
        );
        // a promoted band's members that play the base move to the tuned model; others stay
        let mut m = versions::Manifest {
            version: "v1".into(),
            band: "b".into(),
            base: "qwen3:8b".into(),
            model: "tempo-b-20261107".into(),
            ..versions::Manifest::default()
        };
        m.save(&ctx).unwrap();
        versions::set_active(
            &ctx,
            "b",
            Some("v1"),
            Some("tempo-b-20261107"),
            "promoted",
            1,
        )
        .unwrap();
        let models = crate::conductor::Models {
            heavy: "qwen3:8b".into(),
            fast: "qwen3:1.7b".into(),
        };
        let mut band = crate::conductor::templates::find("social-media")
            .unwrap()
            .band("b", &models);
        for m in &mut band.members {
            m.model = "qwen3:1.7b".into();
        }
        band.members[0].model = "qwen3:8b".into();
        let changed = apply_tuned(&ctx, &mut band);
        assert_eq!(changed, [band.members[0].handle.clone()]);
        assert_eq!(band.members[0].model, "tempo-b-20261107");
        assert_eq!(band.members[1].model, "qwen3:1.7b");
        m.base = "other".into();
        assert!(m.base != "qwen3:8b");
    }
}
