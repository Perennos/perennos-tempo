//! Retune (weight edits): your frontier model reads a local model's weights and edits them
//! directly, and Tempo keeps the result only if it wins.
//!
//! The idea comes from MIT CSAIL's "Sparks of Weight-Space Programming" (Takehi, Teoh, Zhang and
//! Khattab, 2 November 2026): a frontier model, with no training, found the one direction behind a
//! small model's thinking loops and subtracted it from 0.016% of the weights. A retune removes an
//! obstacle like that. It does not add skill. `docs/RETUNE.md` says what is and is not tested.
//!
//! - `retune plan MODEL`: the terms (asked once), the licence, which computer, the weights, the
//!   bench's own Python folder (each a y/N with its size), and how to connect your AI.
//! - `retune bench MODEL`: the bench, an MCP server on stdio (`bench.rs`).
//! - `retune check SESSION`: apply the edit list to a copy, make a GGUF, and put it through the
//!   Woodshed's Rehearsal gate against the base on a held-out set the editor never saw.
//! - `retune undo`, `retune versions`, `retune show`. Nothing is deleted.

pub mod bench;
pub mod model;
pub mod session;
pub mod versions;

#[cfg(test)]
mod tests;

use crate::setup::json::{parse, J};
use crate::woodshed::eval::{self, Gen};
use crate::woodshed::exec::{Exec, Target, Where};
use crate::woodshed::script;
use crate::woodshed::settings::Settings;
use crate::{stamp, write_atomic, Ctx};
use model::Machine;
use session::{norm, root, Session};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The helper that touches weights, written next to the venv so the owner can read it first.
pub const HELPER: &str = include_str!("retune_bench.py");

/// One honest sentence, shown the first time and asked once.
pub const TERMS_SENTENCE: &str = "Anthropic's consumer terms bar using the service to develop or train competing AI models. A retune edits your own local model for your own use, but it's your call: check your plan's terms.";

#[derive(Debug, PartialEq)]
pub enum RetuneCmd {
    Plan {
        model: String,
        hf: Option<String>,
        weights: Option<PathBuf>,
        yes: bool,
    },
    Bench {
        model: String,
        passes: u32,
        weights: Option<PathBuf>,
        held_out: Option<PathBuf>,
        session: Option<String>,
    },
    Show(String),
    Check {
        session: String,
        held_out: Option<PathBuf>,
        outtype: String,
        /// A Conductor band whose parts play the retuned model once it passes.
        band: Option<String>,
        yes: bool,
    },
    Undo {
        target: String,
        edit: bool,
    },
    Versions(Option<String>),
}

fn model_ok(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 100
        && !m.starts_with('-')
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_' | '/'))
}

pub fn parse_args(args: &[&str]) -> Result<RetuneCmd, String> {
    let Some((verb, rest)) = args.split_first() else {
        return Err("retune needs a command: plan, bench, show, check, undo or versions".into());
    };
    let (mut pos, mut flags): (Vec<&str>, Vec<(&str, &str)>) = (vec![], vec![]);
    let mut yes = false;
    let mut edit = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--yes" => yes = true,
            "--edit" => edit = true,
            f @ ("--hf" | "--weights" | "--passes" | "--held-out" | "--session" | "--outtype"
            | "--band") => {
                let v = rest
                    .get(i + 1)
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("retune {verb}: {f} needs a value"))?;
                flags.push((f, v));
                i += 1;
            }
            f if f.starts_with("--") => return Err(format!("retune {verb}: unknown argument {f}")),
            p => pos.push(p),
        }
        i += 1;
    }
    let flag = |k: &str| flags.iter().find(|(f, _)| *f == k).map(|(_, v)| *v);
    let allowed = |ok: &[&str]| -> Result<(), String> {
        match flags.iter().find(|(f, _)| !ok.contains(f)) {
            Some((f, _)) => Err(format!("retune {verb}: {f} does not go with {verb}")),
            None => Ok(()),
        }
    };
    let one = |what: &str| -> Result<String, String> {
        match pos.as_slice() {
            [x] => Ok(x.to_string()),
            [] => Err(format!("retune {verb} needs {what}")),
            _ => Err(format!("retune {verb} takes one {what}")),
        }
    };
    let model = |m: String| {
        if model_ok(&m) {
            Ok(m)
        } else {
            Err(format!(
                "retune {verb}: \"{m}\" is not a model name (like qwen3.5:0.8b)"
            ))
        }
    };
    match *verb {
        "plan" => {
            allowed(&["--hf", "--weights"])?;
            Ok(RetuneCmd::Plan {
                model: model(one("a model")?)?,
                hf: flag("--hf").map(String::from),
                weights: flag("--weights").map(PathBuf::from),
                yes,
            })
        }
        "bench" => {
            allowed(&["--passes", "--weights", "--held-out", "--session"])?;
            let passes = match flag("--passes") {
                None => session::DEFAULT_PASSES,
                Some(p) => p
                    .parse::<u32>()
                    .ok()
                    .filter(|p| (1..=100_000).contains(p))
                    .ok_or("retune bench: --passes is a number of forward passes, 1 or more")?,
            };
            Ok(RetuneCmd::Bench {
                model: model(one("a model")?)?,
                passes,
                weights: flag("--weights").map(PathBuf::from),
                held_out: flag("--held-out").map(PathBuf::from),
                session: flag("--session").map(String::from),
            })
        }
        "show" => Ok(RetuneCmd::Show(one("a session")?)),
        "check" => {
            allowed(&["--held-out", "--outtype", "--band"])?;
            let band = match flag("--band") {
                Some(b) if session::plain(b) => Some(b.to_string()),
                Some(b) => return Err(format!("retune check: \"{b}\" is not a band name")),
                None => None,
            };
            let outtype = flag("--outtype").unwrap_or("q8_0").to_string();
            if !matches!(outtype.as_str(), "f16" | "bf16" | "q8_0") {
                return Err("retune check: --outtype is f16, bf16 or q8_0".into());
            }
            Ok(RetuneCmd::Check {
                session: one("a session")?,
                held_out: flag("--held-out").map(PathBuf::from),
                outtype,
                band,
                yes,
            })
        }
        "undo" => {
            allowed(&[])?;
            Ok(RetuneCmd::Undo {
                target: one("a model, or a session with --edit")?,
                edit,
            })
        }
        "versions" => {
            allowed(&[])?;
            match pos.as_slice() {
                [] => Ok(RetuneCmd::Versions(None)),
                [m] => Ok(RetuneCmd::Versions(Some(model(m.to_string())?))),
                _ => Err("retune versions takes at most one model".into()),
            }
        }
        other => Err(format!(
            "retune: unknown command {other} (try: plan, bench, show, check, undo, versions)"
        )),
    }
}

/// Everything a retune command needs from the world; `run` builds the real ones.
pub struct Deps<'a> {
    pub exec: &'a dyn Exec,
    pub gen: &'a dyn Gen,
    /// This computer, and the fleet's.
    pub machines: Vec<Machine>,
    /// The base model's Modelfile, so the retuned one keeps its chat template.
    pub modelfile_of: &'a dyn Fn(&str) -> Option<String>,
    /// How to start tempo-launch from another program (Claude's config).
    pub exe: String,
    pub windows: bool,
}

// ---- terms

fn terms_file(ctx: &Ctx) -> PathBuf {
    root(ctx).join("terms.json")
}

pub fn terms_ok(ctx: &Ctx) -> bool {
    std::fs::read_to_string(terms_file(ctx))
        .ok()
        .and_then(|t| parse(&t).ok())
        .is_some_and(|j| j.bool("accepted") == Some(true))
}

/// Said once, asked once, remembered. A `--yes` does not answer this one: it is the owner's own.
fn terms(ctx: &Ctx, now: u64, ask: &mut dyn FnMut(&str) -> bool, out: &mut dyn Write) -> bool {
    if terms_ok(ctx) {
        return true;
    }
    let _ = writeln!(out, "{TERMS_SENTENCE}");
    if !ask("Go on? [y/N] ") {
        let _ = writeln!(out, "Noted. Nothing was changed, and nothing was set up. Run `tempo-launch retune plan <model>` again if you change your mind.");
        return false;
    }
    let j = J::Obj(vec![
        ("accepted".into(), J::Bool(true)),
        ("at".into(), J::s(&stamp(now))),
    ]);
    if let Err(e) = write_atomic(&terms_file(ctx), j.pretty().as_bytes()) {
        let _ = writeln!(
            out,
            "Could not remember that ({e}); I will ask again next time."
        );
    }
    true
}

// ---- where things live

fn venv(ctx: &Ctx) -> String {
    root(ctx).join("venv").to_string_lossy().into_owned()
}

fn py(ctx: &Ctx, windows: bool) -> String {
    script::venv_bin(&venv(ctx), "python", windows)
}

/// The helper's file, named for its content so a newer one never replaces what the owner read.
pub fn helper_path(ctx: &Ctx) -> Result<PathBuf, String> {
    let h = crate::setup::sha256::sha256_hex(HELPER.as_bytes());
    let p = root(ctx)
        .join("bench")
        .join(format!("retune_bench-{}.py", &h[..8]));
    if !p.exists() {
        write_atomic(&p, HELPER.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    }
    Ok(p)
}

fn llama_cpp(ctx: &Ctx) -> String {
    Settings::load(ctx).text("llama_cpp").unwrap_or_else(|| {
        ctx.home
            .join("woodshed")
            .join("llama.cpp")
            .to_string_lossy()
            .into_owned()
    })
}

fn local_target(dir: &Path) -> Target {
    Target {
        at: Where::Local,
        dir: dir.to_string_lossy().into_owned(),
    }
}

const STEP: Duration = Duration::from_secs(3600);
const LONG: Duration = Duration::from_secs(4 * 3600);

/// The last line a helper printed, as JSON.
fn last_json(tail: &str) -> Option<J> {
    tail.lines()
        .rev()
        .find_map(|l| parse(l.trim()).ok().filter(|j| matches!(j, J::Obj(_))))
}

// ---- plan

fn connect_steps(d: &Deps, model: &str, out: &mut dyn Write) {
    let exe = &d.exe;
    let q = if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe.clone()
    };
    let _ = writeln!(out, "\nConnect your AI to the bench:");
    let _ = writeln!(
        out,
        "  Claude Code:     claude mcp add tempo-retune -- {q} retune bench {model}"
    );
    let _ = writeln!(out, "  Claude Desktop:  add this to claude_desktop_config.json (Settings, Developer, Edit Config), then restart it:");
    let j = J::Obj(vec![(
        "mcpServers".into(),
        J::Obj(vec![(
            "tempo-retune".into(),
            J::Obj(vec![
                ("command".into(), J::s(exe)),
                (
                    "args".into(),
                    J::strings(&["retune".into(), "bench".into(), model.into()]),
                ),
            ]),
        )]),
    )]);
    for l in j.pretty().lines() {
        let _ = writeln!(out, "    {l}");
    }
    let _ = writeln!(
        out,
        "The bench stops when your AI disconnects. Each sitting is a session; `tempo-launch retune show <session>` lists its edits, and `tempo-launch retune check <session>` tests them."
    );
    let _ = writeln!(
        out,
        "Add `--held-out FILE` (lines like {{\"prompt\": \"...\", \"answer\": \"...\"}}) so check has a set your AI never saw, and put practice prompts in <home>/retune/practice/NAME.txt, one per line."
    );
}

#[allow(clippy::too_many_arguments)]
fn plan(
    ctx: &Ctx,
    d: &Deps,
    model: &str,
    hf_arg: Option<&str>,
    weights_arg: Option<&Path>,
    yes: bool,
    now: u64,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    if !terms(ctx, now, ask, out) {
        return 0;
    }
    let hf = hf_arg.map(String::from).or_else(|| model::hf_repo(model));
    let lic = model::licence(hf.as_deref().filter(|h| h.contains('/')).unwrap_or(model));
    let _ = writeln!(out, "\nLicence: {}. {}", lic.name, lic.words);
    if lic.modify_ok.is_none()
        && !ask("Have you read the model card, and does it let you change the model? [y/N] ")
    {
        let _ = writeln!(out, "Nothing was changed.");
        return 0;
    }
    let Some(params) = model::params_b(model) else {
        let _ = writeln!(out, "I can't tell how big {model} is from its name. Use a tag with the size in it, like qwen3.5:0.8b.");
        return 1;
    };
    let Some(pick) = model::pick(&d.machines, params) else {
        let most = d.machines.iter().map(|m| m.ram_gb).fold(0.0, f64::max);
        let _ = writeln!(out, "No computer you have holds {model} ({params}B needs about {:.0} GB of memory to run on its processors, and the most here is {most:.0} GB). Nothing was changed.", params * 4.0 * 1.3 + 1.0);
        return 1;
    };
    let m = &pick.machine;
    let pass = model::pass_seconds(params, pick.gpu, m.cpus);
    let budget = f64::from(session::DEFAULT_PASSES);
    match &m.gpu {
        Some((g, gb)) if pick.gpu => {
            let _ = writeln!(out, "\nThe bench runs best on {} ({g}, {gb:.0} GB). A full forward pass (512 prompts of 512 tokens) takes about {}, so {} passes is about {}. That is a guess from the model's size; I have not measured it here.", m.name, model::duration_words(pass), session::DEFAULT_PASSES, model::duration_words(pass * budget));
        }
        _ => {
            let _ = writeln!(out, "\nNo computer you have has a graphics card that holds {model}, so the bench runs on {}'s processors ({} cores, {:.0} GB). CPU-only works, but it is slow: a full forward pass (512 prompts of 512 tokens) takes about {}, and {} passes about {}. That is a guess from the model's size; I have not measured it. A pass with fewer prompts counts the same against the budget but takes less time (64 prompts is about 8 times quicker), so ask your AI to use small batches.", m.name, m.cpus, m.ram_gb, model::duration_words(pass), session::DEFAULT_PASSES, model::duration_words(pass * budget));
        }
    }
    if m.at != Where::Local {
        let n = &m.name;
        let _ = writeln!(out, "\n{n} is another computer of yours, so run the rest there: `ssh {n} tempo-launch retune plan {model}`. Your AI then connects over the same ssh: `claude mcp add tempo-retune -- ssh {n} tempo-launch retune bench {model}`. Nothing was changed here.");
        return 0;
    }
    let work = root(ctx).join("work");
    let t = local_target(&work);
    let py = py(ctx, d.windows);
    // the bench's own Python folder
    if !Path::new(&py).exists() {
        let pyname = if d.windows { "python" } else { "python3" };
        let mut pip = vec![py.clone(), "-m".into(), "pip".into(), "install".into()];
        let (size, cpu_only) = if pick.gpu {
            ("about 5 GB", false)
        } else {
            ("about 1 GB", true)
        };
        let mut steps = vec![vec![pyname.into(), "-m".into(), "venv".into(), venv(ctx)]];
        if cpu_only {
            let mut torch = pip.clone();
            torch.extend([
                "torch".into(),
                "--index-url".into(),
                "https://download.pytorch.org/whl/cpu".into(),
            ]);
            steps.push(torch);
            pip.extend(["numpy", "transformers", "huggingface_hub"].map(String::from));
        } else {
            pip.extend(["numpy", "torch", "transformers", "huggingface_hub"].map(String::from));
        }
        steps.push(pip);
        let _ = writeln!(out, "\nThe bench needs numpy, torch and transformers, in a folder of their own that touches nothing else. These commands make it:");
        for s in &steps {
            let _ = writeln!(out, "  {}", script::shown(s));
        }
        if !(yes
            || ask(&format!(
            "That is {size} to download (an estimate; I have not measured it). Set it up? [y/N] "
        ))) {
            let _ = writeln!(out, "Not set up. Nothing else was changed.");
            return 0;
        }
        for s in steps {
            match d.exec.run(&t, &s, LONG) {
                Ok(done) if done.ok => {}
                Ok(done) => {
                    let _ = writeln!(out, "`{}` failed:\n{}", script::shown(&s), done.tail);
                    return 1;
                }
                Err(e) => {
                    let _ = writeln!(out, "{e}");
                    return 1;
                }
            }
        }
    }
    let helper = match helper_path(ctx) {
        Ok(h) => h,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    let _ = writeln!(out, "\nThe helper that touches the weights is {}. Read it before you connect anything: it has no way to run code.", helper.display());
    // the full-precision weights
    let found = weights_arg
        .map(Path::to_path_buf)
        .or_else(|| model::find_weights(ctx, model, hf.as_deref()));
    match found {
        Some(w) => {
            let _ = writeln!(out, "Full-precision weights: {}. (Ollama's own copy is quantised, so an edit needs these.)", w.display());
        }
        None => {
            let Some(hf) = hf else {
                let _ = writeln!(out, "I don't know where {model}'s full-precision weights are on Hugging Face. Pass `--hf ORG/NAME`, or `--weights FOLDER` if you have them.");
                return 1;
            };
            let dest = root(ctx).join("weights").join(session::slug(model));
            let gated = lic.name != "Apache-2.0" && lic.modify_ok.is_some();
            let _ = writeln!(out, "\nOllama's copy of {model} is quantised, and an edit needs the original numbers: {hf} on Hugging Face.{}", if gated { " It asks you to accept its terms there first, and you need to be signed in (`hf auth login`)." } else { "" });
            let q = format!(
                "That is about {:.1} GB (bf16, an estimate) into {}. Fetch it? [y/N] ",
                model::weights_gb(params),
                dest.display()
            );
            if !(yes || ask(&q)) {
                let _ = writeln!(out, "Not fetched. Nothing else was changed.");
                return 0;
            }
            let argv = [
                py.clone(),
                helper.to_string_lossy().into_owned(),
                "fetch".into(),
                hf.clone(),
                dest.to_string_lossy().into_owned(),
            ];
            match d.exec.run(&t, &argv, LONG) {
                Ok(done) if done.ok => {
                    let _ = writeln!(out, "Fetched into {}.", dest.display());
                }
                Ok(done) => {
                    let _ = writeln!(out, "The download failed:\n{}", done.tail);
                    return 1;
                }
                Err(e) => {
                    let _ = writeln!(out, "{e}");
                    return 1;
                }
            }
        }
    }
    connect_steps(d, model, out);
    0
}

// ---- bench

/// What `bench` needs: the weights' folder and the session, or what is missing. Messages go to
/// stderr, because stdout belongs to the protocol.
#[allow(clippy::too_many_arguments)]
pub fn bench_session(
    ctx: &Ctx,
    model: &str,
    passes: u32,
    weights: Option<&Path>,
    held_out: Option<&Path>,
    resume: Option<&str>,
    windows: bool,
    now: u64,
) -> Result<Session, String> {
    if !terms_ok(ctx) {
        return Err(format!("Run `tempo-launch retune plan {model}` first. It says one honest thing about Anthropic's terms and asks you once; the bench does not start before that."));
    }
    if !Path::new(&py(ctx, windows)).exists() {
        return Err(format!("The bench's Python folder is not set up. `tempo-launch retune plan {model}` sets it up after a y/N that shows the size."));
    }
    if let Some(h) = held_out {
        session::read_held_out(h)?;
    }
    if let Some(id) = resume {
        let s = Session::open(ctx, id)?;
        if s.model != model {
            return Err(format!("{id} is a session for {}, not {model}.", s.model));
        }
        return Ok(s);
    }
    let w = weights
        .map(Path::to_path_buf)
        .or_else(|| model::find_weights(ctx, model, model::hf_repo(model).as_deref()))
        .ok_or_else(|| format!("No full-precision weights for {model} yet. `tempo-launch retune plan {model}` fetches them (after a y/N), or pass --weights FOLDER."))?;
    Session::create(ctx, model, &w, passes, held_out, now)
}

pub fn helper_argv(ctx: &Ctx, s: &Session, windows: bool) -> Result<Vec<String>, String> {
    Ok(vec![
        py(ctx, windows),
        helper_path(ctx)?.to_string_lossy().into_owned(),
        "serve".into(),
        s.weights.to_string_lossy().into_owned(),
        s.dir.to_string_lossy().into_owned(),
    ])
}

fn run_bench(ctx: &Ctx, cmd: RetuneCmd) -> i32 {
    let RetuneCmd::Bench {
        model,
        passes,
        weights,
        held_out,
        session: resume,
    } = cmd
    else {
        return 2;
    };
    let now = crate::unix_now();
    let s = match bench_session(
        ctx,
        &model,
        passes,
        weights.as_deref(),
        held_out.as_deref(),
        resume.as_deref(),
        cfg!(windows),
        now,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let argv = match helper_argv(ctx, &s, cfg!(windows)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let helper = match bench::PyHelper::spawn(&argv, &s.dir.join("helper.log")) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    eprintln!(
        "Retune bench for {model}: session {}, {} of {} forward passes left. The weights are read-only here; edits go to {}. Stops when your AI disconnects.",
        s.id,
        s.left(),
        s.passes,
        s.dir.join("edits.json").display()
    );
    let b = bench::Bench {
        ctx,
        session: s,
        helper: std::cell::RefCell::new(Box::new(helper)),
    };
    match b.run(std::io::stdin().lock(), std::io::stdout().lock()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

// ---- show, undo, versions

fn show(ctx: &Ctx, id: &str, out: &mut dyn Write) -> i32 {
    let s = match Session::open(ctx, id) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    let edits = s.edits();
    let _ = writeln!(
        out,
        "Session {} on {}: {} edit{}, {} of {} forward passes used.",
        s.id,
        s.model,
        edits.len(),
        if edits.len() == 1 { "" } else { "s" },
        s.used(),
        s.passes
    );
    for e in &edits {
        let _ = writeln!(out, "{}", Session::describe(e));
    }
    if !edits.is_empty() {
        let _ = writeln!(out, "Edit list hash {}. `tempo-launch retune undo {} --edit` takes the last one off (it is kept in undone.jsonl).", s.edit_hash(), s.id);
    }
    0
}

fn undo(ctx: &Ctx, target: &str, edit: bool, now: u64, out: &mut dyn Write) -> i32 {
    if edit {
        let s = match Session::open(ctx, target) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(out, "{e}");
                return 1;
            }
        };
        return match s.undo_last() {
            Ok(Some(e)) => {
                let _ = writeln!(
                    out,
                    "Took off: {}\nThe weights are as they were before it. The edit is kept in {}.",
                    Session::describe(&e),
                    s.dir.join("undone.jsonl").display()
                );
                0
            }
            Ok(None) => {
                let _ = writeln!(out, "{} has no edits to undo.", s.id);
                0
            }
            Err(e) => {
                let _ = writeln!(out, "{e}");
                1
            }
        };
    }
    // a retune that is playing: go back to the one before it, or to the plain model
    let Some((now_v, now_m)) = versions::active(ctx, target) else {
        let _ = writeln!(
            out,
            "No retune of {target} is playing, so there is nothing to undo."
        );
        return 0;
    };
    let all = versions::list(ctx, target);
    let before = all
        .iter()
        .take_while(|m| m.version != now_v)
        .filter(|m| m.passed() && !m.model.is_empty())
        .last();
    let (v, m) = (
        before.map(|m| m.version.as_str()),
        before.map(|m| m.model.as_str()),
    );
    if let Err(e) = versions::set_active(ctx, target, v, m, "undone", now) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = match before {
        Some(b) => writeln!(
            out,
            "Back to version {} ({}). {now_m} is still in Ollama; nothing was deleted.",
            b.version, b.model
        ),
        None => writeln!(
            out,
            "Back to plain {target}. {now_m} is still in Ollama; nothing was deleted."
        ),
    };
    0
}

fn versions_cmd(ctx: &Ctx, model: Option<&str>, out: &mut dyn Write) -> i32 {
    let models = match model {
        Some(m) => vec![m.to_string()],
        None => versions::models(ctx),
    };
    let mut any = false;
    for m in &models {
        let list = versions::list(ctx, m);
        if list.is_empty() {
            continue;
        }
        any = true;
        let playing = versions::active(ctx, m).map(|a| a.0);
        let _ = writeln!(out, "{m}:");
        for v in &list {
            let (b, t) = (
                v.base_scores.clone().unwrap_or_default(),
                v.tuned_scores.clone().unwrap_or_default(),
            );
            let _ = writeln!(
                out,
                "  {}{} {}: {} edit{}, {} of {} weights ({:.4}%), edit list {}. Held-out {:.1} to {:.1}, general tasks {:.0}% to {:.0}%, loops {:.0}% to {:.0}%. {}",
                v.version,
                if playing.as_deref() == Some(&v.version) { " (playing)" } else { "" },
                if v.passed() { "passed" } else { "did not pass" },
                v.edits,
                if v.edits == 1 { "" } else { "s" },
                v.changed,
                v.total,
                v.percent(),
                v.edit_hash.get(..12).unwrap_or(&v.edit_hash),
                b.eval, t.eval, b.canary, t.canary, b.loops, t.loops,
                v.gate.as_ref().map_or("", |g| g.reason.as_str())
            );
        }
    }
    if !any {
        let _ = writeln!(
            out,
            "No retune has been checked yet. `tempo-launch retune plan <model>` starts."
        );
    }
    0
}

// ---- check

/// A Modelfile for the retuned weights: the base's template, system line and settings, and none
/// of its weights (the `FROM` lines).
pub fn modelfile_for(base_modelfile: &str) -> String {
    let mut out = String::from("FROM ./model.gguf\n");
    for l in base_modelfile.lines() {
        let t = l.trim_start();
        if t.starts_with("FROM ") || t.starts_with("ADAPTER ") || t.starts_with('#') {
            continue;
        }
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// A command run in a folder, with a line saying what it is doing; the output tail or nothing.
type Step<'a> = dyn Fn(&Target, Vec<String>, &str, &mut dyn Write) -> Result<String, ()> + 'a;

/// The baseline for a retune: the same unedited weights, converted and quantised the same way,
/// made once per model and output type and reused. Its name is kept in `baseline-<outtype>.json`.
#[allow(clippy::too_many_arguments)]
fn baseline_model(
    ctx: &Ctx,
    d: &Deps,
    s: &Session,
    outtype: &str,
    py: &str,
    converter: &str,
    base_mf: &str,
    ollama: &str,
    now: u64,
    step: &Step,
    out: &mut dyn Write,
) -> Option<String> {
    let dir = versions::dir(ctx, &s.model);
    let rec = dir.join(format!("baseline-{outtype}.json"));
    let weights = s.weights.to_string_lossy().into_owned();
    let known = std::fs::read_to_string(&rec)
        .ok()
        .and_then(|t| parse(&t).ok())
        .filter(|j| j.str("weights") == Some(weights.as_str()))
        .and_then(|j| j.str("model").map(String::from));
    let here = local_target(&dir);
    if let Some(m) = known {
        let alive = d
            .exec
            .run(&here, &[ollama.into(), "show".into(), m.clone()], STEP)
            .is_ok_and(|x| x.ok);
        if alive {
            let _ = writeln!(out, "Baseline: {m}, built earlier from the same weights.");
            return Some(m);
        }
    }
    let name = format!(
        "tempo-{}-base-{}-{}",
        session::slug(&s.model),
        outtype.trim_end_matches("_0"),
        &crate::file_stamp(now)[..8]
    );
    let work = local_target(&dir.join("baseline").join(outtype));
    step(
        &work,
        vec![
            py.into(),
            converter.into(),
            weights.clone(),
            "--outfile".into(),
            "model.gguf".into(),
            "--outtype".into(),
            outtype.into(),
        ],
        "Converting the unedited weights the same way, for the baseline.",
        out,
    )
    .ok()?;
    d.exec
        .put(&work, "Modelfile", &modelfile_for(base_mf))
        .map_err(|e| {
            let _ = writeln!(out, "{e}");
        })
        .ok()?;
    step(
        &work,
        vec![
            ollama.into(),
            "create".into(),
            name.clone(),
            "-f".into(),
            "Modelfile".into(),
        ],
        &format!("Making the baseline {name} in Ollama."),
        out,
    )
    .ok()?;
    let j = J::Obj(vec![
        ("model".into(), J::s(&name)),
        ("weights".into(), J::s(&weights)),
        ("outtype".into(), J::s(outtype)),
        ("at".into(), J::s(&stamp(now))),
    ]);
    if let Err(e) = write_atomic(&rec, j.pretty().as_bytes()) {
        let _ = writeln!(
            out,
            "Could not remember the baseline ({e}); it will be built again next time."
        );
    }
    Some(name)
}

fn band_link(ctx: &Ctx, band: &str) -> PathBuf {
    root(ctx).join("bands").join(format!("{band}.json"))
}

/// `retune check --band B`: B's parts that play `base` will play whatever retune is active for it.
fn link_band(ctx: &Ctx, band: &str, base: &str) -> Result<(), String> {
    let j = J::Obj(vec![("base".into(), J::s(base))]);
    write_atomic(&band_link(ctx, band), j.pretty().as_bytes())
        .map_err(|e| format!("Could not link the band: {e}"))
}

/// For the Conductor, like the Woodshed's hook: a linked band's parts that play the base model
/// play its active retune. Going back with `retune undo` takes effect here too. Returns the
/// handles that changed.
pub fn apply_retuned(ctx: &Ctx, band: &mut crate::conductor::spec::Band) -> Vec<String> {
    let base = std::fs::read_to_string(band_link(ctx, &band.name))
        .ok()
        .and_then(|t| parse(&t).ok())
        .and_then(|j| j.str("base").map(String::from));
    let (Some(base), mut changed) = (base, vec![]) else {
        return vec![];
    };
    if let Some((_, model)) = versions::active(ctx, &base) {
        for m in band.members.iter_mut().filter(|m| m.model == base) {
            m.model = model.clone();
            changed.push(m.handle.clone());
        }
    }
    changed
}

fn check_folder(s: &Session) -> PathBuf {
    let n = (1..)
        .find(|n| !s.dir.join("work").join(format!("check-{n}")).exists())
        .unwrap_or(1);
    s.dir.join("work").join(format!("check-{n}"))
}

#[allow(clippy::too_many_arguments)]
fn check(
    ctx: &Ctx,
    d: &Deps,
    id: &str,
    held_arg: Option<&Path>,
    outtype: &str,
    band: Option<&str>,
    yes: bool,
    now: u64,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let s = match Session::open(ctx, id) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    let edits = s.edits();
    if edits.is_empty() {
        let _ = writeln!(
            out,
            "{} has no edits yet, so there is nothing to check.",
            s.id
        );
        return 0;
    }
    // the held-out set, minus anything the bench handed out as practice
    let Some(held_path) = held_arg.map(Path::to_path_buf).or(s.held_out.clone()) else {
        let _ = writeln!(out, "check needs a held-out set your AI never saw: lines like {{\"prompt\": \"...\", \"answer\": \"...\"}}. Pass `--held-out FILE`.");
        return 1;
    };
    let all = match session::read_held_out(&held_path) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    let seen: Vec<String> = s.seen().iter().map(|p| norm(p)).collect();
    let held: Vec<(Vec<(String, String)>, String)> = all
        .iter()
        .filter(|(p, _)| !seen.contains(&norm(p)))
        .map(|(p, a)| (vec![("user".to_string(), p.clone())], a.clone()))
        .collect();
    let dropped = all.len() - held.len();
    if dropped > 0 {
        let _ = writeln!(out, "{dropped} held-out prompt{} had been handed out as practice, so {} left out. {} remain.", if dropped == 1 { "" } else { "s" }, if dropped == 1 { "it is" } else { "they are" }, held.len());
    }
    let py = py(ctx, d.windows);
    if !Path::new(&py).exists() {
        let _ = writeln!(
            out,
            "The bench's Python folder is not set up. `tempo-launch retune plan {}` sets it up.",
            s.model
        );
        return 1;
    }
    let llama = llama_cpp(ctx);
    let converter = format!("{llama}/convert_hf_to_gguf.py");
    if !Path::new(&converter).exists() {
        let _ = writeln!(out, "llama.cpp's converter is not at {converter}. Get it with `git clone --depth 1 https://github.com/ggml-org/llama.cpp {llama}` and `{py} -m pip install -r {llama}/requirements/requirements-convert_hf_to_gguf.txt`, or point `llama_cpp` in woodshed.json at your own copy. Nothing was changed.");
        return 1;
    }
    let gb = model::params_b(&s.model).map_or(0.0, model::weights_gb);
    let _ = writeln!(
        out,
        "Session {}: {} edit{} on {}, edit list {}.",
        s.id,
        edits.len(),
        if edits.len() == 1 { "" } else { "s" },
        s.model,
        &s.edit_hash()[..12]
    );
    let _ = writeln!(out, "Check makes a copy of the edited model (about {gb:.1} GB) in {}, turns it into a GGUF ({outtype}), and makes the Ollama model it will be scored as. It also builds the same unedited weights through the same conversion once, as the baseline (reused after that, about {gb:.1} GB more the first time), and runs the Rehearsal of the retune against that baseline on {} held-out prompts and the general tasks. Your own {} is scored too, for the record, and is not touched.", s.dir.join("work").display(), held.len(), s.model);
    if !(yes || ask("Go? [y/N] ")) {
        let _ = writeln!(out, "Not started. Nothing was changed.");
        return 0;
    }
    let dir = check_folder(&s);
    let t = local_target(&dir);
    let helper = match helper_path(ctx) {
        Ok(h) => h.to_string_lossy().into_owned(),
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return 1;
        }
    };
    let hf_dir = dir.join("hf").to_string_lossy().into_owned();
    let step =
        |tg: &Target, argv: Vec<String>, what: &str, out: &mut dyn Write| -> Result<String, ()> {
            let _ = writeln!(out, "{what}");
            match d.exec.run(tg, &argv, STEP) {
                Ok(done) if done.ok => Ok(done.tail),
                Ok(done) => {
                    let _ = writeln!(
                        out,
                        "`{}` failed:\n{}\nNothing was changed.",
                        script::shown(&argv),
                        done.tail
                    );
                    Err(())
                }
                Err(e) => {
                    let _ = writeln!(out, "{e}\nNothing was changed.");
                    Err(())
                }
            }
        };
    let Ok(tail) = step(
        &t,
        vec![
            py.clone(),
            helper,
            "apply".into(),
            s.weights.to_string_lossy().into_owned(),
            s.dir.to_string_lossy().into_owned(),
            hf_dir.clone(),
        ],
        "Applying the edit list to a copy.",
        out,
    ) else {
        return 1;
    };
    let Some(applied) = last_json(&tail) else {
        let _ = writeln!(
            out,
            "The helper did not say what it changed:\n{tail}\nNothing was changed."
        );
        return 1;
    };
    if let Some(e) = applied.str("error") {
        let _ = writeln!(out, "{e}\nNothing was changed.");
        return 1;
    }
    let (changed, total) = (
        applied.f64("changed").unwrap_or(0.0) as u64,
        applied.f64("total").unwrap_or(0.0) as u64,
    );
    let _ = writeln!(
        out,
        "{changed} of {total} weights changed ({:.4}%).",
        if total == 0 {
            0.0
        } else {
            changed as f64 * 100.0 / total as f64
        }
    );
    if step(
        &t,
        vec![
            py.clone(),
            converter.clone(),
            hf_dir,
            "--outfile".into(),
            "model.gguf".into(),
            "--outtype".into(),
            outtype.into(),
        ],
        "Converting to GGUF with llama.cpp.",
        out,
    )
    .is_err()
    {
        return 1;
    }
    // the same template as the base, or the retuned model would answer in the wrong shape
    let Some(base_mf) = (d.modelfile_of)(&s.model) else {
        let _ = writeln!(out, "Ollama would not show {}'s Modelfile (is it running, and is the model there?). Nothing was changed.", s.model);
        return 1;
    };
    if let Err(e) = d.exec.put(&t, "Modelfile", &modelfile_for(&base_mf)) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let ollama = crate::woodshed::train::ollama_bin(ctx);
    let id_v = versions::next_id(ctx, &s.model, now);
    let try_model = format!(
        "tempo-{}-retune-try-{}",
        session::slug(&s.model),
        id_v.replace('-', "")
    );
    if step(
        &t,
        vec![
            ollama.clone(),
            "create".into(),
            try_model.clone(),
            "-f".into(),
            "Modelfile".into(),
        ],
        &format!("Making {try_model} in Ollama to score it (it stays, and nothing is replaced)."),
        out,
    )
    .is_err()
    {
        return 1;
    }
    // the baseline: the same unedited weights, through the same conversion and quantisation
    let Some(baseline) = baseline_model(
        ctx, d, &s, outtype, &py, &converter, &base_mf, &ollama, now, &step, out,
    ) else {
        return 1;
    };
    let set = Settings::load(ctx);
    let _ = writeln!(
        out,
        "Rehearsal: {} held-out prompts and {} general tasks, retuned against the baseline {baseline}.",
        held.len().min(eval::MAX_EVAL),
        eval::CANARY.len()
    );
    let (base, tuned) = match (
        eval::score(d.gen, &baseline, &held),
        eval::score(d.gen, &try_model, &held),
    ) {
        (Ok(b), Ok(t)) => (b, t),
        (Err(e), _) | (_, Err(e)) => {
            let _ = writeln!(
                out,
                "Could not score: {e}\nThe copy is kept in {}. Start Ollama, then run check again.",
                dir.display()
            );
            return 1;
        }
    };
    // your own quantised copy, for the record only: the decision does not use it
    let ollama_base = eval::score(d.gen, &s.model, &held).ok();
    if let Some(w) = eval::loops_words(&s.model, ollama_base.as_ref().unwrap_or(&base)) {
        let _ = writeln!(out, "{w}");
    }
    let g = eval::gate(&base, &tuned, set.margin(), set.canary_slack());
    let all = versions::list(ctx, &s.model);
    let mut m = versions::Manifest {
        version: id_v,
        base: s.model.clone(),
        session: s.id.clone(),
        created: stamp(now),
        edit_hash: s.edit_hash(),
        edits: edits.len(),
        changed,
        total,
        outtype: outtype.into(),
        base_scores: Some(base.clone()),
        tuned_scores: Some(tuned.clone()),
        ollama_scores: ollama_base.clone(),
        baseline: baseline.clone(),
        margin: set.margin(),
        gate: Some(g.clone()),
        status: if g.passed { "passed" } else { "failed" }.into(),
        note: format!("scored as {try_model}"),
        ..versions::Manifest::default()
    };
    let _ = writeln!(
        out,
        "Held-out: baseline {:.1}, retuned {:.1}. General tasks: {:.0}% to {:.0}%. Loops: {:.0}% to {:.0}%.\n{} {}.",
        base.eval, tuned.eval, base.canary, tuned.canary, base.loops, tuned.loops,
        if g.passed { "It passed:" } else { "It did not pass:" },
        g.reason
    );
    if let Some(o) = &ollama_base {
        let _ = writeln!(
            out,
            "For the record, your own {} scores {:.1} on the held-out set, {:.0}% on the general tasks, loops {:.0}%. The decision above does not use it.",
            s.model, o.eval, o.canary, o.loops
        );
    }
    if !g.passed {
        let saved = m.save(ctx);
        let _ = writeln!(out, "Not promoted. The edits are kept: undo the last with `tempo-launch retune undo {} --edit`, or keep editing.", s.id);
        return match saved {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(out, "{e}");
                1
            }
        };
    }
    let name = versions::model_name(&s.model, now, &all);
    if step(
        &t,
        vec![
            ollama,
            "create".into(),
            name.clone(),
            "-f".into(),
            "Modelfile".into(),
        ],
        &format!("Making {name}."),
        out,
    )
    .is_err()
    {
        let _ = m.save(ctx);
        return 1;
    }
    m.model = name.clone();
    m.status = "promoted".into();
    if let Err(e) = m.save(ctx).and_then(|()| {
        versions::set_active(
            ctx,
            &s.model,
            Some(&m.version),
            Some(&name),
            "promoted",
            now,
        )
    }) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = writeln!(out, "{name} is made and recorded as version {}. Use it by name (`ollama run {name}`). Undo with `tempo-launch retune undo {}`; the plain model is still there.", m.version, s.model);
    if let Some(b) = band {
        match link_band(ctx, b, &s.model) {
            Ok(()) => {
                let _ = writeln!(
                    out,
                    "The Conductor plays it for the parts of {b} that play {}.",
                    s.model
                );
            }
            Err(e) => {
                let _ = writeln!(out, "{e}");
            }
        }
    }
    0
}

pub fn run_with(
    ctx: &Ctx,
    cmd: RetuneCmd,
    d: &Deps,
    now: u64,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    match cmd {
        RetuneCmd::Plan {
            model,
            hf,
            weights,
            yes,
        } => plan(
            ctx,
            d,
            &model,
            hf.as_deref(),
            weights.as_deref(),
            yes,
            now,
            ask,
            out,
        ),
        RetuneCmd::Bench { .. } => 2, // `run` serves it: it owns stdin and stdout
        RetuneCmd::Show(id) => show(ctx, &id, out),
        RetuneCmd::Check {
            session,
            held_out,
            outtype,
            band,
            yes,
        } => check(
            ctx,
            d,
            &session,
            held_out.as_deref(),
            &outtype,
            band.as_deref(),
            yes,
            now,
            ask,
            out,
        ),
        RetuneCmd::Undo { target, edit } => undo(ctx, &target, edit, now, out),
        RetuneCmd::Versions(m) => versions_cmd(ctx, m.as_deref(), out),
    }
}

pub fn run(ctx: &Ctx, cmd: RetuneCmd) -> i32 {
    if matches!(cmd, RetuneCmd::Bench { .. }) {
        return run_bench(ctx, cmd);
    }
    let exec = crate::woodshed::exec::RealExec;
    let gen = eval::OllamaGen {
        port: crate::setup::apply::ollama_port(),
    };
    let mut machines = vec![model::local_machine(&crate::setup::probe::RealHost)];
    if matches!(cmd, RetuneCmd::Plan { .. }) {
        let f = crate::fleet::store::load(&crate::fleet::store::Paths::of(ctx));
        machines.extend(model::fleet_machines(&f, &crate::fleet::ssh::user_hosts()));
    }
    let ollama = crate::woodshed::train::ollama_bin(ctx);
    let show_mf = move |m: &str| -> Option<String> {
        let o = std::process::Command::new(&ollama)
            .args(["show", "--modelfile", m])
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let d = Deps {
        exec: &exec,
        gen: &gen,
        machines,
        modelfile_of: &show_mf,
        exe: std::env::current_exe().map_or_else(
            |_| "tempo-launch".into(),
            |p| p.to_string_lossy().into_owned(),
        ),
        windows: cfg!(windows),
    };
    run_with(
        ctx,
        cmd,
        &d,
        crate::unix_now(),
        &mut crate::setup::apply::ask_terminal,
        &mut std::io::stdout(),
    )
}
