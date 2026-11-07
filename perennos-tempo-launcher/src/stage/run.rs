//! Playing a workflow: pick the machine, put the request into the graph, send it through
//! ComfyUI's API (`/prompt`, then `/history`, then `/view`), and keep the result in the
//! workbench without ever replacing a file. A run can be played as N takes across the Fleet's
//! graphics cards, each take judged by a cheap local check; the best one wins and goes to the
//! takes file like any other part. A run that stops partway picks up where it was (its log
//! says which takes are done), and `--again` replays the last run with a Pedal or two changed,
//! on the same seed, so ComfyUI reuses everything upstream that did not change.

use super::catalog::{Catalog, Tier, Workflow};
use super::comfy::Comfy;
use super::guard;
use super::install::{comfy_dir, missing, model_path, saved_with, workflow_path};
use super::pedals::{self, Request};
use super::route::{self, Machine};
use super::state::State;
use crate::conductor::takes_file;
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::{file_stamp, stamp, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The biggest input file Stage will read into memory and send.
const MAX_INPUT: u64 = 1 << 30;

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    /// Some(true) fine, Some(false) a problem, None could not tell.
    pub ok: Option<bool>,
    pub note: String,
}

/// Everything a run touches outside itself, so tests can play it with a fake ComfyUI.
pub trait World: Sync {
    fn now(&self) -> u64;
    fn random(&self) -> i64;
    /// This computer's ComfyUI, if it answers.
    fn local(&self) -> Option<Machine>;
    /// The owner's own start command for ComfyUI, when it is not up. Whether it came up.
    fn start_local(&self) -> bool {
        false
    }
    fn fleet(&self, id: &str) -> Vec<Machine>;
    fn comfy(&self, base: &str) -> Result<Box<dyn Comfy>, String>;
    /// Take the GPU lease, and give the graphics card to ComfyUI (unload our language models).
    fn lease_begin(&self) -> Result<(), String> {
        Ok(())
    }
    fn lease_end(&self) {}
    fn check(&self, file: &Path, kind: &str) -> Check;
    /// The folders an AI may read inputs from (None: no limit; the owner is at the keyboard).
    fn roots(&self) -> Option<Vec<PathBuf>> {
        None
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunOpts {
    /// Input name to a path (a file input) or a text.
    pub inputs: Vec<(String, String)>,
    pub pedals: Vec<(String, String)>,
    /// 0 means 1.
    pub takes: u32,
    /// Start from the last run of this workflow, with these changes on top.
    pub again: bool,
    pub on: Option<String>,
    pub seed: Option<i64>,
    /// An AI asked: inputs must sit in the folders the owner allowed.
    pub guarded: bool,
}

#[derive(Clone, Debug)]
pub struct TakeResult {
    pub take: u32,
    pub seed: i64,
    pub machine: String,
    pub secs: f64,
    pub cached: usize,
    pub files: Vec<PathBuf>,
    pub check: Check,
    /// Done in an earlier go and found again from the run log.
    pub resumed: bool,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub key: String,
    pub takes: Vec<TakeResult>,
    pub winner: Option<usize>,
    /// The line a crew member would say: "Re-angled on the studio box, 41 s."
    pub status: String,
    pub notes: Vec<String>,
}

pub fn workbench(ctx: &Ctx) -> PathBuf {
    ctx.home.join("crate").join("work").join("stage")
}

fn runs_dir(ctx: &Ctx) -> PathBuf {
    super::install::stage_dir(ctx).join("runs")
}

/// A new file in `dir`, never over one that is there: `-2`, `-3` and so on.
pub fn new_file(dir: &Path, stem: &str, ext: &str) -> Result<(std::fs::File, PathBuf), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
    let ext = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    for n in 1..10_000 {
        let name = if n == 1 {
            format!("{stem}{ext}")
        } else {
            format!("{stem}-{n}{ext}")
        };
        let p = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(f) => return Ok((f, p)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not write {}: {e}", p.display())),
        }
    }
    Err("too many files with that name".into())
}

fn safe_name(s: &str) -> String {
    let n: String = s
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(s)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    n.trim_start_matches('.').chars().take(80).collect()
}

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_default()
}

fn s_n(v: f64) -> J {
    J::n(v)
}

/// What a run leaves next to its output: where it came from, enough to make it again.
#[derive(Clone)]
struct Provenance {
    base: J,
}

fn provenance(
    ctx: &Ctx,
    wf: &Workflow,
    st: &State,
    inputs: &[(String, String, Option<String>)],
    now: u64,
) -> Provenance {
    let nodes: Vec<J> = wf
        .nodes
        .iter()
        .map(|n| {
            let tree = st
                .record(&wf.id)
                .and_then(|r| r.get("nodes"))
                .and_then(|x| x.get(&n.name))
                .and_then(|x| x.str("tree_sha256"))
                .map_or(J::Null, J::s);
            J::Obj(vec![
                ("name".into(), J::s(&n.name)),
                ("repo".into(), J::s(&n.repo)),
                ("commit".into(), J::s(&n.commit)),
                ("tree_sha256".into(), tree),
            ])
        })
        .collect();
    let models: Vec<J> = wf
        .models
        .iter()
        .map(|m| {
            let sha = m.sha256.clone().or_else(|| st.model_sha(&m.name));
            J::Obj(vec![
                ("name".into(), J::s(&m.name)),
                ("licence".into(), J::s(&m.licence)),
                ("sha256".into(), sha.as_deref().map_or(J::Null, J::s)),
                ("pinned".into(), J::Bool(m.sha256.is_some())),
            ])
        })
        .collect();
    let ins: Vec<J> = inputs
        .iter()
        .map(|(k, v, sha)| {
            J::Obj(vec![
                ("name".into(), J::s(k)),
                (
                    if sha.is_some() { "file" } else { "text" }.into(),
                    J::s(&if sha.is_some() {
                        safe_name(v)
                    } else {
                        v.chars().take(200).collect()
                    }),
                ),
                ("sha256".into(), sha.as_deref().map_or(J::Null, J::s)),
            ])
        })
        .collect();
    let _ = ctx;
    Provenance {
        base: J::Obj(vec![
            ("made_by".into(), J::s("tempo-launch stage")),
            ("tempo_version".into(), J::s(crate::VERSION)),
            ("made_at".into(), J::s(&stamp(now))),
            (
                "workflow".into(),
                J::Obj(vec![
                    ("id".into(), J::s(&wf.id)),
                    ("title".into(), J::s(&wf.title)),
                    ("repo".into(), J::s(&wf.repo)),
                    ("commit".into(), J::s(&wf.commit)),
                    ("licence".into(), J::s(&wf.licence)),
                    ("file_sha256".into(), J::s(&wf.file_sha256)),
                ]),
            ),
            ("nodes".into(), J::Arr(nodes)),
            ("models".into(), J::Arr(models)),
            ("inputs".into(), J::Arr(ins)),
        ]),
    }
}

fn run_key(
    wf: &Workflow,
    inputs: &[(String, String, Option<String>)],
    pedals: &[(String, String)],
    takes: u32,
    seed: Option<i64>,
) -> String {
    let mut parts: Vec<String> = vec![
        wf.id.clone(),
        wf.file_sha256.clone(),
        format!("takes={takes}"),
    ];
    for (k, v, sha) in inputs {
        parts.push(format!("in:{k}={}", sha.as_deref().unwrap_or(v)));
    }
    let mut p: Vec<String> = pedals
        .iter()
        .map(|(k, v)| format!("pedal:{k}={v}"))
        .collect();
    p.sort();
    parts.extend(p);
    if let Some(s) = seed {
        parts.push(format!("seed={s}"));
    }
    sha256_hex(parts.join("\n").as_bytes())[..12].to_string()
}

fn read_log(path: &Path) -> (Option<i64>, Vec<J>) {
    let mut seed = None;
    let mut done = vec![];
    for l in std::fs::read_to_string(path).unwrap_or_default().lines() {
        if let Ok(j) = parse(l) {
            if j.str("run").is_some() {
                seed = j.f64("seed").map(|s| s as i64);
            } else if j.f64("take").is_some() {
                done.push(j);
            }
        }
    }
    (seed, done)
}

fn append_log(path: &Path, j: &J) {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}", j.compact());
    }
}

/// What one take needs to run on its machine, plain data so a thread can carry it.
struct Job {
    take: u32,
    seed: i64,
    machine: Machine,
    tier: Tier,
    /// Input name to the value ComfyUI holds for it (an uploaded file's name, or the text).
    inputs: Vec<(String, String)>,
}

#[allow(clippy::too_many_arguments)]
fn play_take(
    world: &dyn World,
    comfy: &dyn Comfy,
    cat: &Catalog,
    wf: &Workflow,
    graph: &J,
    job: &Job,
    pedals: &[(String, String)],
    prov: &Provenance,
    bench: &Path,
    log: &Path,
    tag: &str,
    cmd_again: &str,
) -> Result<TakeResult, String> {
    let started = Instant::now();
    let mut pedals = pedals.to_vec();
    // each take has its own seed; a workflow with a seed Pedal gets it there
    if wf.pedal("seed").is_some() {
        pedals.retain(|(k, _)| k != "seed");
        pedals.push(("seed".into(), job.seed.to_string()));
    }
    let req = Request {
        inputs: job.inputs.clone(),
        pedals,
    };
    let applied = pedals::apply(cat, wf, graph, &job.tier, &req, &mut || job.seed)?;
    let id = comfy.queue(&applied.graph, &format!("tempo-stage-{tag}-{}", job.take))?;
    let done = comfy.wait(&id, Duration::from_secs(3 * 3600), &mut || {})?;
    if let Some(e) = done.error {
        return Err(format!("ComfyUI on {} stopped: {e}", job.machine.name));
    }
    let wanted: Vec<_> = done
        .files
        .iter()
        .filter(|f| wf.outputs.iter().any(|o| o.node == f.node) && f.kind == "output")
        .collect();
    if wanted.is_empty() {
        return Err(format!(
            "ComfyUI on {} finished but made no file at the output node",
            job.machine.name
        ));
    }
    let mut files = vec![];
    let mut first_check = Check {
        ok: None,
        note: String::new(),
    };
    for (i, f) in wanted.iter().enumerate() {
        let bytes = comfy.fetch(f)?;
        if bytes.is_empty() {
            return Err(format!("ComfyUI gave an empty file for {}", f.name));
        }
        let stem = format!(
            "{}-{}-take{}-seed{}{}",
            wf.id,
            tag,
            job.take,
            job.seed,
            if i > 0 {
                format!("-part{}", i + 1)
            } else {
                String::new()
            }
        );
        let (mut file, path) = new_file(bench, &stem, &ext_of(&f.name))?;
        file.write_all(&bytes)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        drop(file);
        let kind = wf
            .outputs
            .iter()
            .find(|o| o.node == f.node)
            .map_or("video", |o| o.kind.as_str());
        let c = world.check(&path, kind);
        if i == 0 {
            first_check = c.clone();
        }
        // the sidecar: where this file came from
        let mut side = prov.base.clone();
        side.set("take", J::n(job.take as f64));
        side.set("seed", J::n(job.seed as f64));
        side.set(
            "pedals",
            J::Obj(
                applied
                    .pedals
                    .iter()
                    .map(|(k, v)| (k.clone(), J::s(v)))
                    .collect(),
            ),
        );
        side.set(
            "machine",
            J::Obj(vec![
                ("name".into(), J::s(&job.machine.name)),
                ("gpu_gb".into(), s_n(job.machine.gpu_gb)),
            ]),
        );
        side.set("tier", J::s(&job.tier.label));
        side.set(
            "seconds",
            J::n((started.elapsed().as_secs_f64() * 10.0).round() / 10.0),
        );
        side.set("cached_nodes", J::n(done.cached as f64));
        side.set(
            "output",
            J::Obj(vec![
                (
                    "file".into(),
                    J::s(
                        &path
                            .file_name()
                            .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
                    ),
                ),
                ("sha256".into(), J::s(&sha256_hex(&bytes))),
                ("bytes".into(), J::n(bytes.len() as f64)),
            ]),
        );
        side.set(
            "check",
            J::Obj(vec![
                ("ok".into(), c.ok.map_or(J::Null, J::Bool)),
                ("note".into(), J::s(&c.note)),
            ]),
        );
        side.set("make_it_again", J::s(cmd_again));
        let (mut sf, sp) = new_file(
            bench,
            &format!(
                "{}.stage",
                path.file_name()
                    .map_or(String::new(), |n| n.to_string_lossy().into_owned())
            ),
            "json",
        )?;
        sf.write_all(side.pretty().as_bytes())
            .map_err(|e| format!("could not write {}: {e}", sp.display()))?;
        files.push(path);
    }
    let secs = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    append_log(
        log,
        &J::Obj(vec![
            ("take".into(), J::n(job.take as f64)),
            ("seed".into(), J::n(job.seed as f64)),
            ("machine".into(), J::s(&job.machine.name)),
            ("local".into(), J::Bool(job.machine.local)),
            ("secs".into(), J::n(secs)),
            ("cached".into(), J::n(done.cached as f64)),
            (
                "files".into(),
                J::Arr(files.iter().map(|p| J::s(&p.to_string_lossy())).collect()),
            ),
            ("ok".into(), first_check.ok.map_or(J::Null, J::Bool)),
            ("note".into(), J::s(&first_check.note)),
        ]),
    );
    Ok(TakeResult {
        take: job.take,
        seed: job.seed,
        machine: if job.machine.local {
            "this computer".into()
        } else {
            job.machine.name.clone()
        },
        secs,
        cached: done.cached,
        files,
        check: first_check,
        resumed: false,
    })
}

fn input_value(
    ctx_world: &dyn World,
    wf: &Workflow,
    name: &str,
    v: &str,
    guarded: bool,
) -> Result<(PathBuf, u64), String> {
    let _ = wf;
    let p = std::fs::canonicalize(v)
        .map_err(|_| format!("the input \"{name}\" ({v}) is not a file that exists"))?;
    if !p.is_file() {
        return Err(format!("the input \"{name}\" ({v}) is not a file"));
    }
    if guarded {
        if let Some(roots) = ctx_world.roots() {
            if !roots.iter().any(|r| p.starts_with(r)) {
                return Err(format!(
                    "the input \"{name}\" is not in a folder you allowed. Put it in the workbench, or allow its folder with `tempo-launch crate folders add FOLDER`."
                ));
            }
        }
    }
    let len = std::fs::metadata(&p).map_err(|e| e.to_string())?.len();
    if len > MAX_INPUT {
        return Err(format!(
            "the input \"{name}\" is over 1 GB; Stage sends files up to that"
        ));
    }
    Ok((p, len))
}

fn pairs_of(j: &J, key: &str) -> Vec<(String, String)> {
    j.arr(key)
        .iter()
        .filter_map(|p| Some((p.str("name")?.to_string(), p.str("value")?.to_string())))
        .collect()
}

fn pairs_j(v: &[(String, String)]) -> J {
    J::Arr(
        v.iter()
            .map(|(k, x)| J::Obj(vec![("name".into(), J::s(k)), ("value".into(), J::s(x))]))
            .collect(),
    )
}

/// The command that plays this run again.
fn again_command(id: &str) -> String {
    format!("tempo-launch stage run {id} --again")
}

pub fn play(
    ctx: &Ctx,
    world: &dyn World,
    cat: &Catalog,
    id: &str,
    opts: &RunOpts,
) -> Result<Outcome, String> {
    let wf = cat.find(id).ok_or_else(|| {
        format!("There is no \"{id}\" in Stage. `tempo-launch stage list` shows what there is.")
    })?;
    let mut st = State::load(ctx);
    if !st.installed(id) {
        return Err(format!(
            "{id} is not added yet: `tempo-launch stage add {id}`."
        ));
    }
    let with = saved_with(&st, id);
    let lacks = missing(ctx, wf, &st, &with);
    if !lacks.is_empty() {
        return Err(format!(
            "{id} is not ready: {}. `tempo-launch stage check {id}` says more.",
            lacks.join("; ")
        ));
    }
    // --again: the last run, with this one's changes on top
    let (mut inputs, mut pedal_list, mut base_seed) =
        (opts.inputs.clone(), opts.pedals.clone(), opts.seed);
    if opts.again {
        let last = st
            .last_run(id)
            .ok_or_else(|| format!("There is no earlier run of {id} to play again."))?
            .clone();
        for (k, v) in pairs_of(&last, "inputs") {
            if !inputs.iter().any(|(n, _)| *n == k) {
                inputs.push((k, v));
            }
        }
        for (k, v) in pairs_of(&last, "pedals") {
            if !pedal_list.iter().any(|(n, _)| *n == k) {
                pedal_list.push((k, v));
            }
        }
        if base_seed.is_none() {
            base_seed = last.f64("seed").map(|s| s as i64);
        }
    }
    // pedals checked, in words, before anything is sent
    let mut checked: Vec<(String, String)> = vec![];
    for (k, v) in &pedal_list {
        checked.push((k.clone(), pedals::check_value(cat, wf, k, v)?));
    }
    if let Some(s) = base_seed {
        if wf.pedal("seed").is_some() {
            checked.retain(|(k, _)| k != "seed");
            checked.push(("seed".into(), s.to_string()));
        }
    }
    let explicit_seed = checked
        .iter()
        .find(|(k, _)| k == "seed")
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .filter(|s| *s >= 0);
    // inputs: files are read once and hashed; text stays text
    let mut files: Vec<(String, PathBuf, Vec<u8>, String)> = vec![];
    let mut tagged: Vec<(String, String, Option<String>)> = vec![];
    for (k, v) in &inputs {
        let Some(def) = wf.input(k) else {
            return Err(format!(
                "{id} has no input \"{k}\". It has: {}.",
                wf.inputs
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        if def.kind == "text" {
            tagged.push((k.clone(), v.clone(), None));
        } else {
            let (p, _) = input_value(world, wf, k, v, opts.guarded)?;
            let bytes =
                std::fs::read(&p).map_err(|e| format!("could not read {}: {e}", p.display()))?;
            let sha = sha256_hex(&bytes);
            tagged.push((
                k.clone(),
                p.to_string_lossy().into_owned(),
                Some(sha.clone()),
            ));
            files.push((k.clone(), p, bytes, sha));
        }
    }
    for i in wf
        .inputs
        .iter()
        .filter(|i| i.required && i.default.is_empty())
    {
        if !inputs.iter().any(|(k, _)| *k == i.name) {
            return Err(format!(
                "{id} needs the input \"{}\"{}.",
                i.name,
                if i.kind == "text" {
                    ""
                } else {
                    " (a file: --input NAME=PATH)"
                }
            ));
        }
    }
    let takes = opts.takes.clamp(1, 12);
    // machines
    let mut boxes: Vec<Machine> = vec![];
    if let Some(l) = world.local() {
        boxes.push(l);
    } else if world.start_local() {
        if let Some(l) = world.local() {
            boxes.push(l);
        }
    }
    boxes.extend(world.fleet(id));
    if let Some(on) = &opts.on {
        boxes.retain(|m| m.name == *on || (m.local && (on == "here" || on == "this computer")));
        if boxes.is_empty() {
            return Err(format!(
                "{on} is not a machine with ComfyUI up (or it does not have {id})."
            ));
        }
    }
    let ranked = route::ranked(&boxes, wf);
    if ranked.is_empty() {
        return Err(route::pick(&boxes, wf)
            .err()
            .unwrap_or_else(|| "no machine fits".into()));
    }
    // the graph, and what it holds
    let graph_text = std::fs::read_to_string(workflow_path(ctx, id))
        .map_err(|e| format!("could not read the workflow: {e}"))?;
    let graph = pedals::clean_graph(&parse(&graph_text)?)?;
    let classes = guard::classes_in(&graph);
    let cloud_ok = st.cloud_ok(id);
    let api = guard::api_classes_in(&graph);
    if !api.is_empty() && !cloud_ok {
        return Err(format!("refused: {id} uses {}, which call a paid or cloud service. Nothing leaves your computer unless you turn that on: `tempo-launch stage cloud {id} on`.", api.join(", ")));
    }
    let mut notes: Vec<String> = vec![];
    // each machine we will use: can it run these nodes, and may it?
    let mut used: Vec<(Machine, Tier, Box<dyn Comfy>)> = vec![];
    for (m, t) in ranked.iter().take(takes as usize) {
        let c = match world.comfy(&m.base) {
            Ok(c) => c,
            Err(e) => {
                notes.push(format!("{}: {e}", m.name));
                continue;
            }
        };
        match c.classes(&classes) {
            Ok((gone, paid)) => {
                if !paid.is_empty() && !cloud_ok {
                    return Err(format!("refused: ComfyUI on {} says {} call a paid or cloud service. Nothing leaves your computer unless you turn that on: `tempo-launch stage cloud {id} on`.", m.name, paid.join(", ")));
                }
                if !gone.is_empty() {
                    notes.push(format!("{} does not have the node(s) {} (a custom node pack is missing there, or ComfyUI needs a restart), so it sat this one out.", m.name, gone.join(", ")));
                    continue;
                }
            }
            Err(e) => {
                notes.push(format!("{}: {e}", m.name));
                continue;
            }
        }
        // a Pedal that needs a model file this computer does not have
        if m.local {
            if let Ok(comfy) = comfy_dir(ctx, &st) {
                for model in wf
                    .models
                    .iter()
                    .filter(|x| x.only_for.as_ref().is_some_and(|c| checked.contains(c)))
                {
                    if !model_path(&comfy, model).is_file() {
                        let (p, v) = model.only_for.clone().unwrap_or_default();
                        return Err(format!("{p}={v} needs {}, which is not here yet: `tempo-launch stage add {id} --with {p}={v}`.", model.name));
                    }
                }
            }
        }
        used.push(((*m).clone(), (*t).clone(), c));
    }
    if used.is_empty() {
        return Err(format!("no machine could play {id}: {}", notes.join(" ")));
    }
    // the run's log: a second go at the same command finds what is done
    let key = run_key(wf, &tagged, &checked, takes, explicit_seed);
    let log = runs_dir(ctx).join(format!("{key}.jsonl"));
    let (logged_seed, done_before) = read_log(&log);
    let seed0 = explicit_seed
        .or(logged_seed)
        .unwrap_or_else(|| world.random().abs() % 4_000_000_000);
    if logged_seed.is_none() {
        append_log(
            &log,
            &J::Obj(vec![
                ("run".into(), J::s(&key)),
                ("id".into(), J::s(id)),
                ("seed".into(), J::n(seed0 as f64)),
                ("takes".into(), J::n(takes as f64)),
                ("at".into(), J::s(&stamp(world.now()))),
            ]),
        );
    }
    let mut results: Vec<Option<TakeResult>> = (0..takes).map(|_| None).collect();
    for d in &done_before {
        let n = d.f64("take").unwrap_or(0.0) as u32;
        let fs: Vec<PathBuf> = d.strs("files").into_iter().map(PathBuf::from).collect();
        if n >= 1 && n <= takes && !fs.is_empty() && fs.iter().all(|p| p.is_file()) {
            results[(n - 1) as usize] = Some(TakeResult {
                take: n,
                seed: d.f64("seed").unwrap_or(0.0) as i64,
                machine: d.str("machine").unwrap_or("").to_string(),
                secs: d.f64("secs").unwrap_or(0.0),
                cached: d.f64("cached").unwrap_or(0.0) as usize,
                files: fs,
                check: Check {
                    ok: d.bool("ok"),
                    note: d.str("note").unwrap_or("").to_string(),
                },
                resumed: true,
            });
        }
    }
    let todo: Vec<u32> = (1..=takes)
        .filter(|n| results[(*n - 1) as usize].is_none())
        .collect();
    if !todo.is_empty() {
        // upload the inputs to each machine that will play a take
        let mut stored: Vec<Vec<(String, String)>> = vec![];
        let need: Vec<usize> = todo
            .iter()
            .map(|n| ((*n - 1) as usize) % used.len())
            .collect();
        for (mi, (m, _, c)) in used.iter().enumerate() {
            let mut names = vec![];
            if need.contains(&mi) {
                for (k, p, bytes, sha) in &files {
                    let nm = format!("stage_{}_{}", &sha[..12], safe_name(&p.to_string_lossy()));
                    let kept = c
                        .upload(&nm, bytes)
                        .map_err(|e| format!("{}: {e}", m.name))?;
                    names.push((k.clone(), kept));
                }
            }
            stored.push(names);
        }
        let text_inputs: Vec<(String, String)> = inputs
            .iter()
            .filter(|(k, _)| wf.input(k).is_some_and(|d| d.kind == "text"))
            .cloned()
            .collect();
        let local_used = used.iter().any(|(m, _, _)| m.local);
        if local_used {
            world.lease_begin()?;
        }
        let bench = workbench(ctx);
        let prov = provenance(ctx, wf, &st, &tagged, world.now());
        let tag = file_stamp(world.now());
        let cmd_again = again_command(id);
        let jobs: Vec<(usize, Job)> = todo
            .iter()
            .map(|n| {
                let mi = ((*n - 1) as usize) % used.len();
                let mut ins = text_inputs.clone();
                ins.extend(stored[mi].clone());
                (
                    mi,
                    Job {
                        take: *n,
                        seed: seed0 + i64::from(*n - 1),
                        machine: used[mi].0.clone(),
                        tier: used[mi].1.clone(),
                        inputs: ins,
                    },
                )
            })
            .collect();
        let gathered: Vec<Result<TakeResult, String>> = std::thread::scope(|sc| {
            let handles: Vec<_> = (0..used.len())
                .map(|mi| {
                    let mine: Vec<&Job> = jobs
                        .iter()
                        .filter(|(m, _)| *m == mi)
                        .map(|(_, j)| j)
                        .collect();
                    let c: &dyn Comfy = &*used[mi].2;
                    let (prov, bench, log, tag, cmd_again, graph, checked) =
                        (&prov, &bench, &log, &tag, &cmd_again, &graph, &checked);
                    sc.spawn(move || {
                        mine.into_iter()
                            .map(|j| {
                                play_take(
                                    world, c, cat, wf, graph, j, checked, prov, bench, log, tag,
                                    cmd_again,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });
        if local_used {
            world.lease_end();
        }
        let mut errors = vec![];
        for r in gathered {
            match r {
                Ok(t) => {
                    let i = (t.take - 1) as usize;
                    results[i] = Some(t);
                }
                Err(e) => errors.push(e),
            }
        }
        if results.iter().all(Option::is_none) {
            return Err(errors.join("; "));
        }
        for e in errors {
            notes.push(format!("a take failed: {e}"));
        }
    }
    let made: Vec<TakeResult> = results.into_iter().flatten().collect();
    // the takes file, and the winner
    let ts = stamp(world.now());
    let mut lines: Vec<takes_file::Take> = made
        .iter()
        .map(|t| takes_file::Take {
            id: format!("stage-{key}-{}", t.take),
            run: format!("stage-{key}"),
            band: None,
            part: Some("stage".into()),
            task: format!(
                "{id}: {}",
                inputs
                    .iter()
                    .find(|(k, _)| wf.input(k).is_some_and(|d| d.kind == "text"))
                    .map_or("", |(_, v)| v.as_str())
            ),
            engine: "comfyui".into(),
            model: wf.id.clone(),
            author: takes_file::Author::Local,
            prompt_ref: format!("stage/runs/{key}.jsonl#{}", t.take),
            answer_ref: t
                .files
                .first()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            tests: t.check.ok,
            soundcheck: None,
            judge: None,
            owner: None,
            winner: false,
            pair: None,
            ts: ts.clone(),
        })
        .collect();
    takes_file::pick_winner(&mut lines);
    let fresh: Vec<takes_file::Take> = lines
        .iter()
        .zip(&made)
        .filter(|(_, t)| !t.resumed)
        .map(|(l, _)| l.clone())
        .collect();
    if !fresh.is_empty() {
        let _ = takes_file::append(&takes_file::path(ctx), &fresh);
    }
    let winner = lines.iter().position(|l| l.winner);
    let total: f64 = made.iter().map(|t| t.secs).fold(0.0, f64::max);
    let tier_note = used
        .first()
        .filter(|(_, t, _)| {
            wf.tiers.len() > 1
                && t.min_vram_gb < wf.tiers.iter().map(|x| x.min_vram_gb).fold(0.0, f64::max)
        })
        .map(|(m, t, _)| {
            format!(
                " ({} has {:.0} GB, so it played the tier: {}.)",
                m.name, m.gpu_gb, t.label
            )
        });
    let names: Vec<String> = {
        let mut v: Vec<String> = vec![];
        for t in &made {
            let n = if t.machine == "this computer" {
                "this computer".to_string()
            } else {
                format!("the {} box", t.machine)
            };
            if !v.contains(&n) {
                v.push(n);
            }
        }
        v
    };
    let cached: usize = made.iter().map(|t| t.cached).sum();
    let all_resumed = made.iter().all(|t| t.resumed);
    let mut status = if made.len() == 1 {
        format!(
            "{} on {}, {:.0} s.",
            wf.said,
            names.join(" and "),
            made[0].secs
        )
    } else {
        format!(
            "{} {} takes on {}, best is take {}, {:.0} s.",
            wf.said,
            made.len(),
            names.join(" and "),
            winner.map_or(1, |w| made[w].take),
            total
        )
    };
    if all_resumed {
        status = format!("Already done: {}", status.to_lowercase());
    }
    if cached > 0 {
        status.push_str(&format!(" ComfyUI reused {cached} nodes from before."));
    }
    if let Some(t) = tier_note {
        status.push_str(&t);
    }
    // what Tempo remembers: for --again and for "make it golden hour"
    st.set_last_run(
        id,
        J::Obj(vec![
            ("inputs".into(), pairs_j(&inputs)),
            (
                "pedals".into(),
                pairs_j(
                    &checked
                        .iter()
                        .filter(|(k, _)| k != "seed")
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
            ),
            ("seed".into(), J::n(seed0 as f64)),
            ("takes".into(), J::n(takes as f64)),
            ("at".into(), J::s(&ts)),
        ]),
    );
    st.set_last_workflow(id);
    let _ = st.save(ctx);
    if let Some(w) = winner {
        vault_note(ctx, wf, &made[w], &checked, &key, world.now());
    }
    Ok(Outcome {
        key,
        takes: made,
        winner,
        status,
        notes,
    })
}

/// A note in the Vault for the render that won: what made it, so it can be found and made again.
/// Only when the Vault folder is already there; Stage never makes one.
fn vault_note(
    ctx: &Ctx,
    wf: &Workflow,
    t: &TakeResult,
    pedals: &[(String, String)],
    key: &str,
    now: u64,
) {
    if !ctx.vault.is_dir() {
        return;
    }
    let file = t
        .files
        .first()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let body = format!(
        "---\ntags: [stage, {}]\nworkflow: {}\ncommit: {}\nseed: {}\nmachine: {}\nmade: {}\n---\n# {}\n\nMade with {} ({}), take {}, seed {}, on {}.\n\n- File: `{file}`\n- Pedals: {}\n- Run: `{key}`\n\nTo make it again: `tempo-launch stage run {} --again`. The file's `.stage.json` beside it lists the workflow hash, the node commits and the model hashes.\n",
        wf.feature.replace(' ', "-"),
        wf.id,
        wf.commit,
        t.seed,
        t.machine,
        stamp(now),
        wf.title,
        wf.title,
        wf.licence,
        t.take,
        t.seed,
        t.machine,
        if pedals.is_empty() { "none".into() } else { pedals.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ") },
        wf.id
    );
    if let Ok((mut f, _)) = new_file(
        &ctx.vault.join("Stage"),
        &format!("{}-{}-{key}", &file_stamp(now)[..8], wf.id),
        "md",
    ) {
        let _ = f.write_all(body.as_bytes());
    }
}
