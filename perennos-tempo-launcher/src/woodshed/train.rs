//! `woodshed plan` and `woodshed run`: look at what a band has earned, pick the machine, show the
//! plan, ask, install the trainer in its own venv if it is not there (asking again, with the size),
//! train, convert the adapter to a GGUF, bring it home and rehearse it against the base. A run
//! that passes the gate waits for `woodshed promote`; one that does not is kept and says why.

use super::dataset::{self, Built};
use super::eval::{self, Gen};
use super::exec::{Exec, Target, Where};
use super::node::{self, Node};
use super::script::{self, Backend, Plan};
use super::settings::Settings;
use super::takes::read_takes;
use super::versions::{self, Manifest};
use crate::conductor::Chat;
use crate::{stamp, Ctx};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// What a run needs from the world; tests hand in stand-ins for each.
pub struct Deps<'a> {
    pub exec: &'a dyn Exec,
    pub gen: &'a dyn Gen,
    pub chat: &'a dyn Chat,
    /// Every machine that could train (this computer, and the fleet's).
    pub nodes: Vec<Node>,
    pub windows: bool,
}

const TRAIN_LIMIT: Duration = Duration::from_secs(12 * 3600);
const INSTALL_LIMIT: Duration = Duration::from_secs(2 * 3600);
const STEP_LIMIT: Duration = Duration::from_secs(3600);

/// The model a band is trained on: the owner's word, else the one most of the band's members
/// play (the usual chat: the agent's first model).
pub fn base_of(ctx: &Ctx, set: &Settings, band: &str) -> Option<String> {
    if let Some(b) = set.band_text(band, "base") {
        return Some(b);
    }
    if band == "default" {
        let text = std::fs::read_to_string(ctx.agent_data.join("config.json")).ok()?;
        let j = crate::setup::json::parse(&text).ok()?;
        return j.strs("models").into_iter().find(|m| m.contains(':'));
    }
    let text =
        std::fs::read_to_string(crate::conductor::band_dir(ctx, band).join("band.json")).ok()?;
    let b = crate::conductor::spec::from_json(&text).ok()?;
    let mut counts: Vec<(String, usize)> = vec![];
    for m in b.members.iter().filter(|m| !m.model.is_empty()) {
        match counts.iter_mut().find(|(n, _)| *n == m.model) {
            Some(c) => c.1 += 1,
            None => counts.push((m.model.clone(), 1)),
        }
    }
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    counts.into_iter().next().map(|(m, _)| m)
}

pub struct Planned {
    pub band: String,
    pub base: String,
    pub hf: String,
    pub node: Node,
    pub built: Built,
    pub plan: Plan,
    /// Said when the base was swapped for a smaller one that fits the machine.
    pub note: Option<String>,
}

fn paths(ctx: &Ctx, set: &Settings, n: &Node) -> (String, String) {
    match n.at {
        // beside the work folder on the other machine, so no shell variable has to be expanded
        Where::Ssh(_) => ("../venv".into(), "../llama.cpp".into()),
        Where::Local => {
            let w = ctx.home.join("woodshed");
            (
                w.join("venv").to_string_lossy().into_owned(),
                set.text("llama_cpp")
                    .unwrap_or_else(|| w.join("llama.cpp").to_string_lossy().into_owned()),
            )
        }
    }
}

fn target(ctx: &Ctx, n: &Node, band: &str, id: &str) -> Target {
    match &n.at {
        Where::Local => Target {
            at: Where::Local,
            dir: versions::version_dir(ctx, band, id)
                .to_string_lossy()
                .into_owned(),
        },
        Where::Ssh(h) => Target {
            at: Where::Ssh(h.clone()),
            dir: format!("$HOME/.tempo-woodshed/{band}-{id}"),
        },
    }
}

/// Everything a run would do for `band`, or what is in the way.
pub fn plan_band(ctx: &Ctx, d: &Deps, set: &Settings, band: &str) -> Result<Planned, Vec<String>> {
    let mut why = vec![];
    if !set.is_on(band) {
        return Err(vec![format!(
            "it is off. Turn it on with `tempo-launch woodshed on {band}`"
        )]);
    }
    let mut base = base_of(ctx, set, band);
    if base.is_none() {
        why.push(format!("I don't know which model it plays on. Say so with `tempo-launch woodshed set {band} base qwen3:8b`"));
    }
    // the machine decides the model: a small card or a processor trains a smaller one, or none
    let pool: Vec<Node> = match set.text("node") {
        Some(n) => d.nodes.iter().filter(|x| x.name == n).cloned().collect(),
        None => d.nodes.clone(),
    };
    let mut chosen: Option<node::Chosen> = None;
    match base.as_deref() {
        Some(b) => match node::choose(&pool, b) {
            Ok(c) => chosen = Some(c),
            Err(e) => why.push(e),
        },
        None if pool.is_empty() => why.push(node::NO_NODE.into()),
        None => {}
    }
    let mut note = None;
    let mut swapped = false;
    if let Some(c) = &chosen {
        swapped = Some(&c.base) != base.as_ref();
        base = Some(c.base.clone());
        note = c.note.clone();
    }
    // the owner's `hf` line is for the model they named, so a swap leaves it behind
    let hf = base.as_deref().and_then(|b| {
        set.band_text(band, "hf")
            .filter(|_| !swapped)
            .or_else(|| script::hf_for(b))
    });
    if let (Some(b), None) = (&base, &hf) {
        why.push(format!("I don't know where the weights of {b} live on Hugging Face. Say so with `tempo-launch woodshed set {band} hf <owner/name>`"));
    }
    let (takes, _) = read_takes(ctx);
    let built = dataset::build(ctx, set, band, &takes);
    if built.report.band_examples < set.min_examples() {
        why.push(format!(
            "{} winning takes are usable so far and {} are needed. Keep playing; thumbs in chat count",
            built.report.band_examples,
            set.min_examples()
        ));
    }
    let node = chosen.map(|c| c.node);
    match (base, hf, node, why.is_empty()) {
        (Some(base), Some(hf), Some(node), true) => {
            let plan = Plan {
                band: band.into(),
                backend: node.backend,
                base: base.clone(),
                hf: hf.clone(),
                rank: set.rank(),
                epochs: set.epochs(),
                train_examples: built.train.len(),
            };
            Ok(Planned {
                band: band.into(),
                base,
                hf,
                node,
                built,
                plan,
                note,
            })
        }
        _ => Err(why),
    }
}

fn plan_words(ctx: &Ctx, p: &Planned) -> String {
    let r = &p.built.report;
    let mut s = format!(
        "Woodshed (training) for {band}\n\n  Data     {n} winning takes kept, {g} general ones mixed in: {t} to train on, {v} held out.\n           What was scrubbed is written beside the data (scrub-report.txt).\n  Base     {base} (the trainer reads its weights from Hugging Face: {hf})\n  Trains   {node}, with {how}, rank {rank}, {epochs} pass{es}\n",
        band = p.band,
        n = r.band_examples,
        g = r.general_examples,
        t = r.train,
        v = r.valid,
        base = p.base,
        hf = p.hf,
        node = p.node.words(),
        how = p.plan.backend.words(),
        rank = p.plan.rank,
        epochs = p.plan.epochs,
        es = if p.plan.epochs == 1 { "" } else { "es" },
    );
    if let Some(n) = &p.note {
        s.push_str(&format!("  Fits     {n}.\n"));
    }
    if p.plan.backend == Backend::Cpu {
        if let Some(b) = node::params_b(&p.base) {
            s.push_str(&format!(
                "  Time     {} (an estimate from the threads; Tempo has not timed it, and the real speed depends on the chip).\n",
                node::cpu_estimate(p.plan.train_examples, p.plan.epochs, b, p.node.cores)
            ));
        }
    }
    match &p.node.at {
        Where::Local => s.push_str("  Stays    everything stays on this computer.\n"),
        Where::Ssh(h) => s.push_str(&format!(
            "  Stays    the scrubbed data goes to {h} over your own ssh, and nowhere else.\n"
        )),
    }
    if r.general_examples < r.general_wanted {
        s.push_str(&format!(
            "  Mix      only {} of the {} general examples wanted were found, so the mix is thinner than 20%. Winning takes from your other bands count, and so does a file of your own at {}.\n",
            r.general_examples,
            r.general_wanted,
            dataset::general_file(ctx).display()
        ));
    }
    s.push_str(&format!(
        "  Keeps    every version is kept under {} and nothing is deleted.\n",
        ctx.home.join("woodshed").join(&p.band).display()
    ));
    s
}

/// `woodshed plan [band]`.
pub fn plan(ctx: &Ctx, d: &Deps, bands: &[String], out: &mut dyn Write) -> i32 {
    let set = Settings::load(ctx);
    let bands = if bands.is_empty() {
        set.bands_on()
    } else {
        bands.to_vec()
    };
    if bands.is_empty() {
        let _ = writeln!(out, "Nothing in the woodshed yet. Turn it on for a band with `tempo-launch woodshed on <band>`.");
        return 0;
    }
    for b in &bands {
        match plan_band(ctx, d, &set, b) {
            Ok(p) => {
                let _ = write!(out, "{}", plan_words(ctx, &p));
                let (_, size) = script::pip_packages(p.plan.backend);
                let _ = writeln!(out, "  Install  the trainer lives in its own folder; if it is not there yet it is {size} (an estimate), and you are asked first.\n\nNothing was started. `tempo-launch woodshed run` does it, after a y/N.\n");
            }
            Err(why) => {
                let _ = writeln!(out, "Not ready to practise {b} yet:");
                for w in why {
                    let _ = writeln!(out, "  - {w}");
                }
                let _ = writeln!(out);
            }
        }
    }
    0
}

fn fail(ctx: &Ctx, m: &mut Manifest, note: &str, out: &mut dyn Write) -> i32 {
    m.status = "failed".into();
    m.note = note.to_string();
    let _ = m.save(ctx);
    let _ = writeln!(
        out,
        "The run stopped: {note}\nWhat it had made so far is kept in {}.",
        versions::version_dir(ctx, &m.band, &m.version).display()
    );
    1
}

fn exists(d: &Deps, t: &Target, path: &str) -> bool {
    match t.at {
        Where::Local => std::path::Path::new(path).exists(),
        Where::Ssh(_) => d
            .exec
            .run(
                t,
                &["test".into(), "-e".into(), path.into()],
                Duration::from_secs(30),
            )
            .is_ok_and(|x| x.ok),
    }
}

/// Set up the trainer's own folder when it is not there, after a y/N that shows the size.
#[allow(clippy::too_many_arguments)]
fn install(
    ctx: &Ctx,
    d: &Deps,
    set: &Settings,
    p: &Planned,
    t: &Target,
    yes: bool,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let (venv, llama) = paths(ctx, set, &p.node);
    let py = script::venv_bin(&venv, "python", d.windows && p.node.at == Where::Local);
    let have_venv = exists(d, t, &py);
    let have_llama = exists(d, t, &format!("{llama}/convert_lora_to_gguf.py"));
    if have_venv && have_llama {
        return Ok(());
    }
    let steps = script::install_steps(
        p.plan.backend,
        &venv,
        &llama,
        have_llama,
        d.windows && p.node.at == Where::Local,
    );
    let steps: Vec<_> = if have_venv {
        steps.into_iter().skip(1).collect()
    } else {
        steps
    };
    let (_, size) = script::pip_packages(p.plan.backend);
    let _ = writeln!(out, "\nThe trainer is not set up on {} yet. These commands make it, in its own folder, touching nothing else:", p.node.name);
    for s in &steps {
        let _ = writeln!(out, "  {}", script::shown(s));
    }
    let q = format!("That is {size} to download (an estimate; Tempo has not measured it) on {}. Set it up? [y/N] ", p.node.name);
    if !(yes || ask(&q)) {
        return Err("the trainer was not set up, so there was nothing to train with".into());
    }
    for s in steps {
        let done = d.exec.run(t, &s, INSTALL_LIMIT)?;
        if !done.ok {
            return Err(format!("`{}` failed:\n{}", script::shown(&s), done.tail));
        }
    }
    Ok(())
}

/// A version's models: the base and the adapter on it, scored the same way.
fn rehearse_version(
    ctx: &Ctx,
    d: &Deps,
    set: &Settings,
    m: &mut Manifest,
    held: &[(Vec<(String, String)>, String)],
    out: &mut dyn Write,
) -> Result<(), String> {
    let dir = versions::version_dir(ctx, &m.band, &m.version);
    let adapter = dir.join("adapter.gguf");
    if !adapter.exists() {
        return Err("there is no adapter.gguf in that version".into());
    }
    // a model of its own for the rehearsal, so the base and anything already promoted stay as they are
    let try_model = format!(
        "tempo-{}-try-{}",
        m.band,
        m.version.replace(|c: char| !c.is_ascii_alphanumeric(), "")
    );
    let modelfile = format!("FROM {}\nADAPTER {}\n", m.base, adapter.display());
    let t = Target {
        at: Where::Local,
        dir: dir.to_string_lossy().into_owned(),
    };
    d.exec.put(&t, "Modelfile.try", &modelfile)?;
    let ollama = ollama_bin(ctx);
    let made = d.exec.run(
        &t,
        &[
            ollama,
            "create".into(),
            try_model.clone(),
            "-f".into(),
            "Modelfile.try".into(),
        ],
        STEP_LIMIT,
    )?;
    if !made.ok {
        return Err(format!(
            "Ollama could not make {try_model} from the adapter:\n{}",
            made.tail
        ));
    }
    let _ = writeln!(
        out,
        "Rehearsal: {} on {} held-out takes and {} general tasks, base and adapter.",
        m.version,
        held.len().min(eval::MAX_EVAL),
        eval::CANARY.len()
    );
    let base = eval::score(d.gen, &m.base, held)?;
    let tuned = eval::score(d.gen, &try_model, held)?;
    if let Some(w) = eval::loops_words(&m.base, &base) {
        let _ = writeln!(out, "{w}");
    }
    let g = eval::gate(&base, &tuned, set.margin(), set.canary_slack());
    m.base_scores = Some(base);
    m.adapter_scores = Some(tuned);
    m.margin = set.margin();
    m.gate = Some(g);
    m.status = "trained".into();
    m.note = format!("rehearsed with {try_model}");
    m.save(ctx)
}

pub fn ollama_bin(ctx: &Ctx) -> String {
    ctx.find("ollama").map_or_else(
        || "ollama".to_string(),
        |p| p.to_string_lossy().into_owned(),
    )
}

fn held_out(b: &Built) -> Vec<(Vec<(String, String)>, String)> {
    b.valid
        .iter()
        .map(|e| (e.messages.clone(), e.answer.clone()))
        .collect()
}

fn scores_line(m: &Manifest) -> String {
    match (&m.base_scores, &m.adapter_scores, &m.gate) {
        (Some(b), Some(a), Some(g)) => format!(
            "Held-out takes: base {:.1}, adapter {:.1}. General tasks: base {:.0}%, adapter {:.0}%.\n{} {}.",
            b.eval,
            a.eval,
            b.canary,
            a.canary,
            if g.passed { "It passed:" } else { "It did not pass:" },
            g.reason
        ),
        _ => "It has not been scored yet.".into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_band(
    ctx: &Ctx,
    d: &Deps,
    set: &Settings,
    p: Planned,
    now: u64,
    yes: bool,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let _ = write!(out, "{}", plan_words(ctx, &p));
    // only when idle
    let (venv, llama) = paths(ctx, set, &p.node);
    let work = ctx.home.join("woodshed");
    match node::idle(d.exec, &p.node, &work.to_string_lossy()) {
        Some(false) => {
            let _ = writeln!(out, "\n{} is busy right now, and training waits for a quiet moment. Try again later, or let the night run do it.", p.node.name);
            return 1;
        }
        None => {
            let _ = writeln!(
                out,
                "\n(I can't tell whether {} is busy: it gave no load figure.)",
                p.node.name
            );
        }
        Some(true) => {}
    }
    if !(yes || ask("\nStart training now? [y/N] ")) {
        let _ = writeln!(out, "Not started. Nothing was changed.");
        return 0;
    }
    let id = versions::next_id(ctx, &p.band, now);
    let dir = versions::version_dir(ctx, &p.band, &id);
    let mut m = Manifest {
        version: id.clone(),
        band: p.band.clone(),
        created: stamp(now),
        base: p.base.clone(),
        hf: p.hf.clone(),
        backend: match p.plan.backend {
            Backend::Cuda => "cuda",
            Backend::Mlx => "mlx",
            Backend::Cpu => "cpu",
        }
        .into(),
        node: p.node.name.clone(),
        rank: p.plan.rank,
        epochs: p.plan.epochs,
        train: p.built.report.train,
        valid: p.built.report.valid,
        general: p.built.report.general_examples,
        parent: versions::active(ctx, &p.band).and_then(|a| a.version),
        status: "started".into(),
        ..Manifest::default()
    };
    match dataset::write(&dir, &p.built) {
        Ok(h) => m.data_hash = h,
        Err(e) => return fail(ctx, &mut m, &e, out),
    }
    let t = target(ctx, &p.node, &p.band, &id);
    for (name, text) in script::files(&p.plan) {
        if let Err(e) = std::fs::write(dir.join(&name), &text) {
            return fail(ctx, &mut m, &format!("cannot write {name}: {e}"), out);
        }
    }
    let _ = m.save(ctx);
    let _ = writeln!(
        out,
        "\nVersion {id}: the data and the training script are in {}. Read them first if you like.",
        dir.display()
    );
    if let Err(e) = install(ctx, d, set, &p, &t, yes, ask, out) {
        return fail(ctx, &mut m, &e, out);
    }
    if let Where::Ssh(h) = &t.at {
        let _ = writeln!(out, "Sending the scrubbed data to {h} over your own ssh.");
        let mut names: Vec<String> = vec!["train.jsonl".into(), "valid.jsonl".into()];
        names.extend(script::files(&p.plan).into_iter().map(|(n, _)| n));
        for name in names {
            let text = std::fs::read_to_string(dir.join(&name)).unwrap_or_default();
            if let Err(e) = d.exec.put(&t, &name, &text) {
                return fail(ctx, &mut m, &e, out);
            }
        }
    }
    let win = d.windows && p.node.at == Where::Local;
    let _ = writeln!(out, "Training on {}. This can take a long while; its own log is woodshed.log in the work folder.", p.node.name);
    for (i, step) in script::steps(&p.plan, &venv, &llama, win)
        .iter()
        .enumerate()
    {
        let limit = if i == 0 { TRAIN_LIMIT } else { STEP_LIMIT };
        match d.exec.run(&t, step, limit) {
            Ok(x) if x.ok => {}
            Ok(x) => {
                return fail(
                    ctx,
                    &mut m,
                    &format!("`{}` failed:\n{}", script::shown(step), x.tail),
                    out,
                )
            }
            Err(e) => return fail(ctx, &mut m, &e, out),
        }
    }
    if t.at != Where::Local {
        if let Err(e) = d.exec.get(&t, "adapter.gguf", &dir.join("adapter.gguf")) {
            return fail(ctx, &mut m, &e, out);
        }
    }
    if !dir.join("adapter.gguf").exists() {
        return fail(
            ctx,
            &mut m,
            "the trainer finished but there is no adapter.gguf",
            out,
        );
    }
    m.adapter_file = "adapter.gguf".into();
    m.status = "trained".into();
    m.note = "trained; not scored yet".into();
    let _ = m.save(ctx);
    let _ = writeln!(out, "Trained. Now the Rehearsal.");
    if let Err(e) = rehearse_version(ctx, d, set, &mut m, &held_out(&p.built), out) {
        m.note = format!("trained, not scored: {e}");
        let _ = m.save(ctx);
        let _ = writeln!(out, "The adapter is made and kept, but could not be scored: {e}\nStart Ollama (with {} on it), then `tempo-launch woodshed rehearse {}`.", p.base, p.band);
        return 1;
    }
    let _ = writeln!(out, "{}", scores_line(&m));
    if m.passed() {
        let _ = writeln!(
            out,
            "`tempo-launch woodshed promote {}` puts it to work.",
            p.band
        );
    } else {
        let _ = writeln!(
            out,
            "It stays in the woodshed, and {} keeps playing on what it had.",
            p.band
        );
    }
    0
}

/// `woodshed run [band] [--yes]`: reflect on new takes, then train each band that is on and ready.
#[allow(clippy::too_many_arguments)]
pub fn run(
    ctx: &Ctx,
    d: &Deps,
    bands: &[String],
    yes: bool,
    now: u64,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let set = Settings::load(ctx);
    let bands = if bands.is_empty() {
        set.bands_on()
    } else {
        bands.to_vec()
    };
    if bands.is_empty() {
        let _ = writeln!(out, "Nothing in the woodshed yet. Turn it on for a band with `tempo-launch woodshed on <band>`.");
        return 0;
    }
    let mut code = 0;
    for b in bands {
        // new lessons first: they cost nothing and the next prompt gets them tonight
        if set.is_on(&b) {
            let model = set.text("reflector_model").unwrap_or_default();
            if let Err(e) =
                super::reflect::run(ctx, &b, set.lesson_tokens(), d.chat, &model, now, out)
            {
                let _ = writeln!(out, "No new lessons for {b} tonight: {e}");
            }
        }
        match plan_band(ctx, d, &set, &b) {
            Ok(p) => code |= run_band(ctx, d, &set, p, now, yes, ask, out),
            Err(why) => {
                let _ = writeln!(out, "Not training {b}:");
                for w in why {
                    let _ = writeln!(out, "  - {w}");
                }
            }
        }
    }
    code
}

/// `woodshed rehearse [band]`: score the newest version again (when Ollama was not there the first time).
pub fn rehearse(ctx: &Ctx, d: &Deps, band: &str, out: &mut dyn Write) -> i32 {
    let set = Settings::load(ctx);
    let Some(mut m) = versions::list(ctx, band)
        .into_iter()
        .rev()
        .find(|m| m.adapter_file == "adapter.gguf" && m.gate.is_none())
    else {
        let _ = writeln!(out, "Nothing waits to be scored for {band}.");
        return 0;
    };
    let dir = versions::version_dir(ctx, band, &m.version);
    let held = std::fs::read_to_string(dir.join("valid.jsonl")).unwrap_or_default();
    let held: Vec<_> = held
        .lines()
        .filter_map(|l| crate::setup::json::parse(l).ok())
        .filter_map(|j| {
            let mut msgs: Vec<(String, String)> = j
                .arr("messages")
                .iter()
                .filter_map(|x| Some((x.str("role")?.to_string(), x.str("content")?.to_string())))
                .collect();
            let (_, answer) = msgs.pop()?;
            Some((msgs, answer))
        })
        .collect();
    match rehearse_version(ctx, d, &set, &mut m, &held, out) {
        Ok(()) => {
            let _ = writeln!(out, "{}", scores_line(&m));
            0
        }
        Err(e) => {
            let _ = writeln!(out, "Could not score {}: {e}", m.version);
            1
        }
    }
}

pub fn work_dir(ctx: &Ctx) -> PathBuf {
    ctx.home.join("woodshed")
}

#[cfg(test)]
pub mod tests {
    use super::super::exec::Done;
    use super::super::takes::test_ctx;
    use super::*;
    use std::cell::RefCell;

    /// A trainer, an Ollama CLI and a machine that are all made up. It records every command, and
    /// when the converter runs it leaves an adapter.gguf in the work folder, as the real one would.
    #[derive(Default)]
    pub struct FakeExec {
        pub log: RefCell<Vec<(Where, Vec<String>)>>,
        pub puts: RefCell<Vec<(Where, String)>>,
        pub load: RefCell<String>,
        pub present: RefCell<Vec<String>>,
        pub fail_on: RefCell<Option<String>>,
        pub no_adapter: RefCell<bool>,
    }

    impl Exec for FakeExec {
        fn run(&self, t: &Target, argv: &[String], _l: Duration) -> Result<Done, String> {
            self.log.borrow_mut().push((t.at.clone(), argv.to_vec()));
            let line = argv.join(" ");
            if let Some(f) = self.fail_on.borrow().as_ref() {
                if line.contains(f.as_str()) {
                    return Ok(Done {
                        ok: false,
                        tail: "CUDA out of memory (made up)".into(),
                    });
                }
            }
            if line.contains("loadavg") {
                let l = self.load.borrow();
                return Ok(Done {
                    ok: true,
                    tail: if l.is_empty() {
                        "0.10 0.10 0.10 1/100 1\n8".into()
                    } else {
                        l.clone()
                    },
                });
            }
            if argv[0] == "test" {
                let p = &argv[2];
                return Ok(Done {
                    ok: self
                        .present
                        .borrow()
                        .iter()
                        .any(|x| p.ends_with(x.as_str())),
                    tail: String::new(),
                });
            }
            if line.contains("convert_lora_to_gguf.py")
                && !*self.no_adapter.borrow()
                && t.at == Where::Local
            {
                std::fs::write(
                    std::path::Path::new(&t.dir).join("adapter.gguf"),
                    "GGUF fake adapter",
                )
                .unwrap();
            }
            Ok(Done {
                ok: true,
                tail: String::new(),
            })
        }
        fn put(&self, t: &Target, name: &str, text: &str) -> Result<(), String> {
            self.puts
                .borrow_mut()
                .push((t.at.clone(), name.to_string()));
            if t.at == Where::Local {
                std::fs::create_dir_all(&t.dir).unwrap();
                std::fs::write(std::path::Path::new(&t.dir).join(name), text).unwrap();
            }
            Ok(())
        }
        fn get(&self, _t: &Target, name: &str, to: &std::path::Path) -> Result<(), String> {
            if *self.no_adapter.borrow() {
                return Err(format!("could not bring {name}"));
            }
            std::fs::write(to, "GGUF fake adapter from far away").map_err(|e| e.to_string())
        }
    }

    /// Answers by model: the adapter's model is better at the band's task, and equal on the canary.
    pub struct FakeGen {
        pub tuned_boost: bool,
        pub tuned_breaks_canary: bool,
    }
    impl Gen for FakeGen {
        fn answer(&self, model: &str, m: &[(String, String)]) -> Result<String, String> {
            let q = m.last().map(|x| x.1.as_str()).unwrap_or("");
            let tuned = model.contains("-try");
            if let Some((_, any)) = eval::CANARY.iter().find(|(p, _)| *p == q) {
                return Ok(if tuned && self.tuned_breaks_canary {
                    "no idea".into()
                } else {
                    any[0].to_string()
                });
            }
            // held-out tasks look like "task number N for b"; the reference answer for N is below
            let n: usize = q
                .split_whitespace()
                .nth(2)
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
            Ok(if tuned && self.tuned_boost {
                reference(n)
            } else {
                "something unrelated entirely".into()
            })
        }
    }

    pub fn reference(n: usize) -> String {
        format!("Answer number {n}, long enough to keep around.")
    }

    struct Quiet;
    impl Chat for Quiet {
        fn say(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
            Ok("{\"add\":[],\"helped\":[],\"hurt\":[]}".into())
        }
    }

    pub fn cuda_node(at: Where) -> Node {
        Node {
            name: if at == Where::Local {
                "this computer".into()
            } else {
                "big-box".into()
            },
            at,
            backend: Backend::Cuda,
            gb: 24.0,
            what: "NVIDIA GeForce RTX 4090".into(),
            windows: false,
            cores: 16,
        }
    }

    /// A home with `n` winning takes for band `b` (run log and takes file), the band switched on.
    pub fn world(tag: &str, n: usize) -> Ctx {
        let ctx = test_ctx(tag);
        let d = ctx.agent_data.join("runs/2026-11-07");
        std::fs::create_dir_all(&d).unwrap();
        let mut log = vec![];
        let mut takes = vec![];
        for i in 0..n {
            log.push(format!("{{\"prompt\":\"task number {i} for b\"}}"));
            log.push(format!(
                "{{\"answer\":{}}}",
                crate::setup::json::J::s(&reference(i)).compact()
            ));
            takes.push(format!(
                "{{\"take\":\"t{i}\",\"band\":\"b\",\"author\":\"local\",\"winner\":true,\"prompt_ref\":\"runs/2026-11-07/r.jsonl#{}\",\"answer_ref\":\"runs/2026-11-07/r.jsonl#{}\"}}",
                i * 2,
                i * 2 + 1
            ));
        }
        std::fs::write(d.join("r.jsonl"), log.join("\n")).unwrap();
        std::fs::create_dir_all(ctx.agent_data.join("takes")).unwrap();
        std::fs::write(ctx.agent_data.join("takes/takes.jsonl"), takes.join("\n")).unwrap();
        let mut s = Settings::load(&ctx);
        s.set_band("b", "on", crate::setup::json::J::Bool(true));
        s.set_band("b", "base", crate::setup::json::J::s("qwen3:8b"));
        s.save(&ctx, 1_800_000_000).unwrap();
        ctx
    }

    fn deps<'a>(exec: &'a FakeExec, gen: &'a FakeGen, nodes: Vec<Node>) -> Deps<'a> {
        Deps {
            exec,
            gen,
            chat: &Quiet,
            nodes,
            windows: false,
        }
    }

    fn said(f: impl FnOnce(&mut Vec<u8>) -> i32) -> (i32, String) {
        let mut out = Vec::new();
        let code = f(&mut out);
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn plan_says_what_is_in_the_way() {
        let ctx = world("t-plan", 10);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        let (code, text) = said(|o| plan(&ctx, &deps(&e, &g, vec![]), &[], o));
        assert_eq!(code, 0);
        assert!(
            text.contains("10 winning takes are usable so far and 40 are needed"),
            "{text}"
        );
        assert!(
            text.contains("no computer in your fleet can train"),
            "{text}"
        );
        // nothing on at all
        let empty = test_ctx("t-plan-empty");
        let (_, text) = said(|o| plan(&empty, &deps(&e, &g, vec![]), &[], o));
        assert!(text.starts_with("Nothing in the woodshed yet. Turn it on for a band with `tempo-launch woodshed on <band>`."), "{text}");
        // a band that is off
        let (_, text) = said(|o| plan(&ctx, &deps(&e, &g, vec![]), &["other".to_string()], o));
        assert!(text.contains("it is off"), "{text}");
    }

    #[test]
    fn plan_for_a_ready_band_names_the_machine_the_base_and_where_the_data_goes() {
        let ctx = world("t-plan-ready", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        let nodes = vec![cuda_node(Where::Ssh("big-box".into()))];
        let (_, text) = said(|o| plan(&ctx, &deps(&e, &g, nodes), &[], o));
        for want in [
            "Woodshed (training) for b",
            "50 winning takes kept, 0 general ones mixed in",
            "only 0 of the 13 general examples wanted",
            "qwen3:8b",
            "Qwen/Qwen3-8B",
            "big-box",
            "QLoRA with TRL",
            "over your own ssh, and nowhere else",
            "about 5 GB",
            "Nothing was started",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        assert!(e.log.borrow().is_empty(), "plan runs nothing");
        assert!(!versions::root(&ctx, "b").exists(), "plan writes nothing");
    }

    fn quick_plan(tag: &str, base: &str, nodes: Vec<Node>) -> String {
        let ctx = world(tag, 50);
        let mut set = Settings::load(&ctx);
        set.set_band("b", "base", crate::setup::json::J::s(base));
        set.save(&ctx, 1_800_000_001).unwrap();
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        said(|o| plan(&ctx, &deps(&e, &g, nodes), &[], o)).1
    }

    fn cpu_box(threads: u32, usable_gb: f64) -> Node {
        Node {
            name: "big-cpu".into(),
            at: Where::Ssh("big-cpu".into()),
            backend: Backend::Cpu,
            gb: usable_gb,
            what: format!("CPU only, {threads} threads"),
            windows: false,
            cores: threads,
        }
    }

    #[test]
    fn a_fleet_with_no_card_plans_a_small_model_on_the_processor_with_an_estimate() {
        let text = quick_plan("t-cpu-small", "qwen3:1.7b", vec![cpu_box(16, 24.0)]);
        for want in [
            "big-cpu (CPU only, 16 threads, 24 GB usable)",
            "the processor (LoRA in 16-bit",
            "Qwen/Qwen3-1.7B",
            "Time     about ",
            "minutes for 42 examples (2 passes)",
            "on 16 threads (an estimate from the threads; Tempo has not timed it",
            "Nothing was started",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        assert!(
            !text.contains("Fits"),
            "the model it asked for fits: {text}"
        );
    }

    #[test]
    fn a_fleet_with_no_card_and_a_big_model_starts_nothing_and_names_the_better_tools() {
        let text = quick_plan("t-cpu-big", "qwen3:8b", vec![cpu_box(64, 192.0)]);
        assert!(text.contains("Not ready to practise b yet"), "{text}");
        assert!(
            text.contains("Tempo won't start a CPU job that size"),
            "{text}"
        );
        assert!(
            text.contains("A retune or Lessons is the better tool"),
            "{text}"
        );
        assert!(!text.contains("Woodshed (training) for b"), "{text}");
    }

    #[test]
    fn a_small_card_plans_qlora_on_the_model_that_fits_and_says_which() {
        let mut card = cuda_node(Where::Ssh("big-box".into()));
        card.gb = 6.0;
        card.what = "NVIDIA GeForce RTX 3060".into();
        let text = quick_plan("t-small-card", "qwen3:8b", vec![card]);
        for want in [
            "Base     qwen3:4b",
            "Qwen/Qwen3-4B",
            "QLoRA with TRL",
            "Fits     qwen3:8b (8B) needs about 7 GB for QLoRA (an estimate) and big-box has 6, so this run trains qwen3:4b",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        assert!(
            !text.contains("Time "),
            "only a processor run gets a time: {text}"
        );
    }

    #[test]
    fn a_whole_run_on_this_computer_trains_converts_rehearses_and_passes() {
        let ctx = world("t-run", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        let d = deps(&e, &g, vec![cuda_node(Where::Local)]);
        let mut asked: Vec<String> = vec![];
        let (code, text) = said(|o| {
            run(
                &ctx,
                &d,
                &[],
                false,
                1_794_000_000,
                &mut |q| {
                    asked.push(q.to_string());
                    true
                },
                o,
            )
        });
        assert_eq!(code, 0, "{text}");
        // it asked before training, and again before installing, with the size
        assert!(
            asked.iter().any(|q| q.starts_with("\nStart training now?")),
            "{asked:?}"
        );
        assert!(
            asked
                .iter()
                .any(|q| q.contains("about 5 GB") && q.contains("[y/N]")),
            "{asked:?}"
        );
        let cmds: Vec<String> = e.log.borrow().iter().map(|(_, a)| a.join(" ")).collect();
        assert!(cmds.iter().any(|c| c.contains("-m venv")), "{cmds:?}");
        assert!(cmds
            .iter()
            .any(|c| c.contains("pip install") && c.contains("trl")));
        assert!(cmds.iter().any(|c| c.ends_with("train.py")));
        assert!(cmds.iter().any(|c| c.contains("convert_lora_to_gguf.py")
            && c.contains("--base-model-id Qwen/Qwen3-8B")));
        assert!(cmds.iter().any(|c| c.contains("create tempo-b-")
            && c.contains("-try-")
            && c.contains("-f Modelfile.try")));
        // the version folder has everything, and the manifest the facts
        let v = versions::list(&ctx, "b");
        assert_eq!(v.len(), 1);
        let m = &v[0];
        assert!(m.passed(), "{:?}", m.gate);
        assert_eq!(
            (m.base.as_str(), m.status.as_str(), m.parent.clone()),
            ("qwen3:8b", "trained", None)
        );
        assert_eq!(m.data_hash.len(), 64);
        assert!(
            m.adapter_scores.as_ref().unwrap().eval - m.base_scores.as_ref().unwrap().eval >= 5.0
        );
        let dir = versions::version_dir(&ctx, "b", &m.version);
        for f in [
            "train.jsonl",
            "valid.jsonl",
            "scrub-report.txt",
            "train.py",
            "adapter.gguf",
            "manifest.json",
            "Modelfile.try",
        ] {
            assert!(dir.join(f).exists(), "{f}");
        }
        assert!(
            text.contains("It passed:") && text.contains("woodshed promote b"),
            "{text}"
        );
        // it did not promote by itself
        assert!(versions::active(&ctx, "b").is_none());
    }

    #[test]
    fn an_adapter_that_does_not_win_by_the_margin_is_kept_and_not_promoted() {
        let ctx = world("t-fail", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: false,
                tuned_breaks_canary: false,
            },
        );
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                true,
                1_794_000_000,
                &mut |_| false,
                o,
            )
        });
        assert_eq!(code, 0, "{text}");
        let m = &versions::list(&ctx, "b")[0];
        assert!(!m.passed());
        assert!(
            text.contains("It did not pass:")
                && text.contains("margin is 5.0")
                && text.contains("keeps playing on what it had"),
            "{text}"
        );
        assert!(!text.contains("promote b"));
        // a win that breaks the general tasks is not a win
        let ctx = world("t-canary", 50);
        let g = FakeGen {
            tuned_boost: true,
            tuned_breaks_canary: true,
        };
        let (_, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                true,
                1_794_000_000,
                &mut |_| false,
                o,
            )
        });
        assert!(text.contains("general tasks slipped"), "{text}");
    }

    #[test]
    fn nothing_starts_without_a_yes_and_a_busy_machine_waits() {
        let ctx = world("t-ask", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                false,
                1,
                &mut |_| false,
                o,
            )
        });
        assert_eq!(code, 0);
        assert!(text.contains("Not started. Nothing was changed."), "{text}");
        assert!(versions::list(&ctx, "b").is_empty() && !versions::root(&ctx, "b").exists());
        // busy: load 6 on 8 processors
        *e.load.borrow_mut() = "6.00 5.00 4.00 3/300 99\n8".into();
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                true,
                1,
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(
            text.contains("is busy right now") && !text.contains("Version "),
            "{text}"
        );
        // declining the install stops the run, and the version it started is kept with the reason
        *e.load.borrow_mut() = String::new();
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                false,
                1_794_000_000,
                &mut |q| !q.contains("Set it up?"),
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(text.contains("the trainer was not set up"), "{text}");
        assert_eq!(versions::list(&ctx, "b")[0].status, "failed");
    }

    #[test]
    fn a_trainer_that_fails_leaves_the_version_and_says_what_it_said() {
        let ctx = world("t-crash", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        *e.fail_on.borrow_mut() = Some("train.py".into());
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                true,
                1_794_000_000,
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(
            text.contains("CUDA out of memory (made up)") && text.contains("is kept in"),
            "{text}"
        );
        let m = &versions::list(&ctx, "b")[0];
        assert_eq!(m.status, "failed");
        assert!(versions::version_dir(&ctx, "b", &m.version)
            .join("train.jsonl")
            .exists());
    }

    #[test]
    fn a_remote_run_sends_the_data_over_ssh_and_brings_the_adapter_home() {
        let ctx = world("t-remote", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Ssh("big-box".into()))]),
                &[],
                true,
                1_794_000_000,
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 0, "{text}");
        let puts: Vec<String> = e
            .puts
            .borrow()
            .iter()
            .filter(|(w, _)| *w != Where::Local)
            .map(|(_, n)| n.clone())
            .collect();
        assert_eq!(puts, ["train.jsonl", "valid.jsonl", "train.py"]);
        let ssh: Vec<String> = e
            .log
            .borrow()
            .iter()
            .filter(|(w, _)| *w == Where::Ssh("big-box".into()))
            .map(|(_, a)| a.join(" "))
            .collect();
        // the trainer and the converter ran there, beside the work folder's own venv; nothing absolute
        assert!(
            ssh.iter()
                .any(|c| c.starts_with("../venv/bin/python train.py")),
            "{ssh:?}"
        );
        assert!(ssh
            .iter()
            .any(|c| c.contains("../llama.cpp/convert_lora_to_gguf.py")));
        let m = &versions::list(&ctx, "b")[0];
        let adapter = std::fs::read_to_string(
            versions::version_dir(&ctx, "b", &m.version).join("adapter.gguf"),
        )
        .unwrap();
        assert!(adapter.contains("from far away"));
        assert!(m.passed());
        // the Ollama commands ran here, on this computer
        assert!(e
            .log
            .borrow()
            .iter()
            .any(|(w, a)| *w == Where::Local && a.iter().any(|x| x == "create")));
    }

    #[test]
    fn a_missing_adapter_or_a_silent_ollama_is_said_and_kept() {
        let ctx = world("t-noadapter", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        *e.no_adapter.borrow_mut() = true;
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                true,
                1_794_000_000,
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(text.contains("no adapter.gguf"), "{text}");
        // Ollama refusing the create: the adapter is kept, not scored, and `rehearse` can try again
        let ctx = world("t-noollama", 50);
        let e = FakeExec::default();
        *e.fail_on.borrow_mut() = Some("create".into());
        let (code, text) = said(|o| {
            run(
                &ctx,
                &deps(&e, &g, vec![cuda_node(Where::Local)]),
                &[],
                true,
                1_794_000_000,
                &mut |_| true,
                o,
            )
        });
        assert_eq!(code, 1);
        assert!(
            text.contains("could not be scored") && text.contains("woodshed rehearse b"),
            "{text}"
        );
        let m = &versions::list(&ctx, "b")[0];
        assert!(m.gate.is_none() && m.adapter_file == "adapter.gguf");
        *e.fail_on.borrow_mut() = None;
        let (code, text) = said(|o| rehearse(&ctx, &deps(&e, &g, vec![]), "b", o));
        assert_eq!(code, 0, "{text}");
        assert!(versions::list(&ctx, "b")[0].passed());
    }

    #[test]
    fn the_second_version_of_a_day_has_the_first_as_its_parent_once_that_plays() {
        let ctx = world("t-parent", 50);
        let (e, g) = (
            FakeExec::default(),
            FakeGen {
                tuned_boost: true,
                tuned_breaks_canary: false,
            },
        );
        run(
            &ctx,
            &deps(&e, &g, vec![cuda_node(Where::Local)]),
            &[],
            true,
            1_794_000_000,
            &mut |_| true,
            &mut Vec::new(),
        );
        let first = versions::list(&ctx, "b")[0].version.clone();
        versions::set_active(&ctx, "b", Some(&first), Some("tempo-b-x"), "promoted", 1).unwrap();
        run(
            &ctx,
            &deps(&e, &g, vec![cuda_node(Where::Local)]),
            &[],
            true,
            1_794_000_100,
            &mut |_| true,
            &mut Vec::new(),
        );
        let v = versions::list(&ctx, "b");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].parent.as_deref(), Some(first.as_str()));
        assert_ne!(v[0].version, v[1].version);
    }

    #[test]
    fn the_base_comes_from_the_owner_or_the_bands_members() {
        let ctx = test_ctx("t-base");
        let mut s = Settings::load(&ctx);
        assert_eq!(base_of(&ctx, &s, "b"), None);
        s.set_band("b", "base", crate::setup::json::J::s("qwen3:4b"));
        assert_eq!(base_of(&ctx, &s, "b").as_deref(), Some("qwen3:4b"));
        // the usual chat reads the agent's first model with a tag
        std::fs::create_dir_all(&ctx.agent_data).unwrap();
        std::fs::write(
            ctx.agent_data.join("config.json"),
            r#"{"models":["local-heavy","qwen3:14b"]}"#,
        )
        .unwrap();
        assert_eq!(
            base_of(&ctx, &Settings::load(&ctx), "default").as_deref(),
            Some("qwen3:14b")
        );
    }
}
