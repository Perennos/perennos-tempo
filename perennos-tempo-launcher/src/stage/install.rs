//! `stage add` and `stage remove`. Adding fetches the workflow file at its pinned commit and
//! checks its SHA-256, puts each custom node pack in only after a look at its code, and fetches
//! the models that can be fetched, all after one y/N that shows the sizes. Nothing is overwritten
//! and nothing is deleted: what leaves goes to the trash. See docs/STAGE.md.

use super::catalog::{Catalog, Model, NodePack, Workflow};
use super::guard::{self, Review, Soundcheck};
use super::pedals;
use super::state::State;
use crate::bank::install::{untar_argv, Pm};
use crate::setup::json::{parse, J};
use crate::setup::runtime::{download_limit_secs, to_trash};
use crate::setup::sha256::{file_sha256, sha256_hex};
use crate::{stamp, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Host<'a> {
    pub pm: &'a dyn Pm,
    pub os: &'a str,
    pub sc: &'a dyn Soundcheck,
    /// The owner's Hugging Face token, if they have one: for gated models only, and only ever
    /// sent to huggingface.co (see `hf.rs`).
    pub token: Option<&'a super::hf::Token>,
}

/// What the owner sees and says. `ask` shows a question and gives back the line they typed.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub ask: &'a mut dyn FnMut(&str) -> String,
}

impl Io<'_> {
    fn say(&mut self, s: &str) {
        let _ = writeln!(self.out, "{s}");
    }

    fn yes_no(&mut self, q: &str) -> bool {
        matches!(
            (self.ask)(q).trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        )
    }

    /// "yes", whole, for the things that are risky: a "y" is not enough.
    fn typed_yes(&mut self, q: &str) -> bool {
        (self.ask)(q).trim() == "yes"
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AddOpts {
    pub yes: bool,
    /// `pedal=choice` pairs whose models should be fetched as well (`--with effect=crush`).
    pub with: Vec<(String, String)>,
}

pub fn gb(g: f64) -> String {
    if g >= 10.0 {
        format!("{g:.0} GB")
    } else {
        format!("{g:.1} GB")
    }
}

// ---- where things live

pub fn stage_dir(ctx: &Ctx) -> PathBuf {
    ctx.home.join("stage")
}

pub fn workflow_path(ctx: &Ctx, id: &str) -> PathBuf {
    stage_dir(ctx)
        .join("workflows")
        .join(id)
        .join("workflow.api.json")
}

fn agent_settings(ctx: &Ctx) -> Option<J> {
    let p = crate::setup::apply::pinned_agent_settings()
        .unwrap_or_else(|| crate::setup::apply::agent_settings_path(&ctx.agent_data));
    parse(&std::fs::read_to_string(p).ok()?).ok()
}

/// The ComfyUI folder: `TEMPO_COMFY_DIR`, the one `stage folder` saved, or the parent of the
/// agent's `comfy_output_dir` when that holds `custom_nodes`.
pub fn comfy_dir(ctx: &Ctx, st: &State) -> Result<PathBuf, String> {
    let from_env = std::env::var_os("TEMPO_COMFY_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let from_agent = || {
        agent_settings(ctx)
            .and_then(|j| j.str("comfy_output_dir").map(PathBuf::from))
            .and_then(|o| o.parent().map(Path::to_path_buf))
            .filter(|p| p.join("custom_nodes").is_dir())
    };
    let dir = from_env
        .or_else(|| st.folder().map(PathBuf::from))
        .or_else(from_agent)
        .ok_or("Stage does not know where ComfyUI lives. Tell it once: `tempo-launch stage folder /path/to/ComfyUI`. (ComfyUI itself comes from the Crate: `tempo-launch crate show comfyui`.)")?;
    if !dir.join("custom_nodes").is_dir() || !dir.join("models").is_dir() {
        return Err(format!(
            "{} is not a ComfyUI folder (it needs custom_nodes and models inside). Fix it with `tempo-launch stage folder PATH`.",
            dir.display()
        ));
    }
    Ok(dir)
}

pub fn model_path(comfy: &Path, m: &Model) -> PathBuf {
    comfy.join("models").join(&m.dir).join(&m.name)
}

fn node_path(comfy: &Path, n: &NodePack) -> PathBuf {
    comfy.join("custom_nodes").join(&n.name)
}

// ---- hashes

/// A pack's tree: every file's path and SHA-256, hashed together. It is what Stage records when
/// it puts a pack in, so a changed pack is noticed later.
pub fn tree_sha256(dir: &Path) -> Result<String, String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) -> Result<(), String> {
        let mut es: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("could not read {}: {e}", dir.display()))?
            .flatten()
            .collect();
        es.sort_by_key(|e| e.file_name());
        for e in es {
            let t = e.file_type().map_err(|e| e.to_string())?;
            let p = e.path();
            if t.is_symlink() || e.file_name() == ".git" {
                continue;
            }
            if t.is_dir() {
                walk(&p, root, out)?;
            } else {
                let b = std::fs::read(&p)
                    .map_err(|e| format!("could not read {}: {e}", p.display()))?;
                out.push(format!(
                    "{}\t{}",
                    p.strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    sha256_hex(&b)
                ));
            }
        }
        Ok(())
    }
    let mut lines = vec![];
    walk(dir, dir, &mut lines)?;
    Ok(sha256_hex(lines.join("\n").as_bytes()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("could not make {}: {e}", to.display()))?;
    for e in std::fs::read_dir(from)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let t = e.file_type().map_err(|e| e.to_string())?;
        let dest = to.join(e.file_name());
        if t.is_symlink() {
            continue;
        } else if t.is_dir() {
            copy_tree(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), &dest)
                .map_err(|e| format!("could not copy into {}: {e}", dest.display()))?;
        }
    }
    Ok(())
}

/// Move a folder to a place that does not exist yet, across disks if need be.
fn put_in_place(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Err(format!(
            "{} is already there and Stage never overwrites a folder",
            to.display()
        ));
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to)
}

// ---- the plan

fn show_plan(
    cat: &Catalog,
    wf: &Workflow,
    opts: &AddOpts,
    comfy: &Path,
    host_token: bool,
    out: &mut dyn Write,
) {
    let _ = cat;
    let _ = writeln!(out, "{} ({})", wf.title, wf.id);
    let _ = writeln!(out, "  {}", wf.what);
    let _ = writeln!(
        out,
        "  From {} at commit {} ({}). Workflow file {} bytes, SHA-256 checked.",
        wf.repo,
        &wf.commit[..8],
        wf.licence,
        wf.file_bytes
    );
    if wf.nodes.is_empty() {
        let _ = writeln!(out, "  Custom nodes: none. It runs on ComfyUI's own nodes.");
    } else {
        let _ = writeln!(
            out,
            "  Custom nodes (Python that runs inside ComfyUI, so each gets a Soundcheck first):"
        );
        for n in &wf.nodes {
            let there = node_path(comfy, n).exists();
            let _ = writeln!(
                out,
                "    {} from {} at {} ({}){}",
                n.name,
                n.repo,
                &n.commit[..8],
                n.licence,
                if there {
                    "  already in custom_nodes, left alone"
                } else {
                    ""
                }
            );
        }
    }
    let _ = writeln!(out, "  Models:");
    for m in wf.models_for(&opts.with) {
        let there = model_path(comfy, m).exists();
        let size = m.gb.map_or("size unknown".to_string(), gb);
        let how = if there {
            "already there".to_string()
        } else if m.gated && host_token {
            "gated: its licence is shown first, then fetched with your Hugging Face token"
                .to_string()
        } else if m.gated {
            "gated: its licence needs your yes on the model's page (set HF_TOKEN and Stage fetches it)".to_string()
        } else {
            "fetched".to_string()
        };
        let _ = writeln!(
            out,
            "    {} ({size}, {}){}  {how}",
            m.name,
            m.licence,
            if m.verified {
                ""
            } else {
                ", SHA-256 not pinned upstream"
            }
        );
    }
    let todo: Vec<&Model> = wf
        .models_for(&opts.with)
        .filter(|m| !m.gated && !model_path(comfy, m).exists())
        .collect();
    let (known, unknown) = todo.iter().fold((0.0, 0usize), |(g, u), m| match m.gb {
        Some(x) => (g + x, u),
        None => (g, u + 1),
    });
    let _ = writeln!(
        out,
        "  Download: about {}{} into {}.",
        gb(known),
        if unknown > 0 {
            format!(" plus {unknown} file(s) of unknown size")
        } else {
            String::new()
        },
        comfy.join("models").display()
    );
    let _ = writeln!(
        out,
        "  Needs a graphics card with {:.0} GB or more. {}",
        wf.min_vram_gb, wf.tested
    );
    let _ = writeln!(out, "  Nothing leaves your computer when it runs. A graph or node that calls a paid or cloud service is refused unless you turn that on for this workflow.");
}

/// Fetch one file with `download`, into `dest`, only if its SHA-256 is right.
#[allow(clippy::too_many_arguments)]
fn fetch_checked(
    host: &Host,
    ctx: &Ctx,
    url: &str,
    dest: &Path,
    bytes: u64,
    want: Option<&str>,
    now: u64,
    token: Option<&super::hf::Token>,
) -> Result<String, String> {
    let fetch = host
        .pm
        .fetcher()
        .ok_or("this computer has no curl, so Tempo cannot fetch the file")?;
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("could not make {}: {e}", d.display()))?;
    }
    let part = dest.with_extension(format!(
        "{}part",
        dest.extension()
            .map_or(String::new(), |e| format!("{}.", e.to_string_lossy()))
    ));
    if part.exists() {
        to_trash(&ctx.home, &part, now)?;
    }
    let secs = download_limit_secs(bytes);
    match token {
        // a gated model: hop by hop, the token only where it belongs
        Some(t) => super::hf::fetch(&*fetch, url, Some(t), &part, secs, &mut |_| {}),
        None => fetch.download(url, &part, secs, &mut |_| {}),
    }
    .map_err(|e| format!("Could not fetch {url}: {e}"))?;
    let got = file_sha256(&part, &mut |_| {})
        .map_err(|e| format!("could not read {}: {e}", part.display()))?;
    if let Some(w) = want {
        if got != w {
            let moved = to_trash(&ctx.home, &part, now)?;
            return Err(format!(
                "{} did not match its pinned SHA-256 (wanted {w}, got {got}), so Stage refused it. It was moved to {}; nothing was put in place.",
                dest.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()),
                moved.display()
            ));
        }
    }
    if dest.exists() {
        to_trash(&ctx.home, &part, now)?;
        return Err(format!(
            "{} appeared while fetching, and Stage never overwrites a file",
            dest.display()
        ));
    }
    std::fs::rename(&part, dest)
        .map_err(|e| format!("could not finish {}: {e}", dest.display()))?;
    Ok(got)
}

/// The folder a pack's code is in, once its archive is unpacked into `stage`.
fn pack_root(stage: &Path, n: &NodePack) -> Result<PathBuf, String> {
    let tops: Vec<PathBuf> = std::fs::read_dir(stage)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .collect();
    let top = match tops.as_slice() {
        [one] if one.is_dir() => one.clone(),
        _ => stage.to_path_buf(),
    };
    let root = match &n.subdir {
        Some(s) => {
            if s.contains("..") || s.starts_with('/') {
                return Err("a node pack's subfolder must stay inside it".into());
            }
            top.join(s)
        }
        None => top,
    };
    root.is_dir().then_some(root).ok_or_else(|| {
        format!(
            "the archive for {} did not hold the folder it should",
            n.name
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn add_node(
    host: &Host,
    ctx: &Ctx,
    wf: &Workflow,
    n: &NodePack,
    comfy: &Path,
    cloud_ok: bool,
    opts: &AddOpts,
    now: u64,
    io: &mut Io,
) -> Result<J, String> {
    let dest = node_path(comfy, n);
    if dest.exists() {
        io.say(&format!(
            "{} is already in custom_nodes. Stage leaves it alone. (The workflow was made against {}; if it misbehaves, update that folder.)",
            n.name,
            &n.commit[..8]
        ));
        return Ok(J::Obj(vec![
            ("commit".into(), J::s(&n.commit)),
            ("by_stage".into(), J::Bool(false)),
        ]));
    }
    let work = stage_dir(ctx)
        .join("staging")
        .join(format!("{}-{}", wf.id, n.name));
    if work.exists() {
        to_trash(&ctx.home, &work, now)?;
    }
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let url = format!("https://codeload.github.com/{}/tar.gz/{}", n.repo, n.commit);
    let archive =
        stage_dir(ctx)
            .join("downloads")
            .join(format!("{}-{}.tar.gz", n.name, &n.commit[..12]));
    if archive.exists() {
        to_trash(&ctx.home, &archive, now)?;
    }
    io.say(&format!("Fetching {} at {} ...", n.name, &n.commit[..8]));
    fetch_checked(host, ctx, &url, &archive, 50 << 20, None, now, None)?;
    host.pm.run(&untar_argv(host.os, &archive, &work))?;
    let root = pack_root(&work, n)?;
    // the look
    let review: Review = guard::review(&root, host.sc);
    io.say(&format!("Soundcheck on {}:", n.name));
    guard::show(&review, &n.name, io.out);
    if review.cloud() && !cloud_ok {
        to_trash(&ctx.home, &work, now)?;
        return Err(format!(
            "refused: {} calls a paid or cloud service, and nothing leaves your computer unless you turn that on for this workflow. If you want it: `tempo-launch stage cloud {} on`, then add it again.",
            n.name, wf.id
        ));
    }
    if review.blocks() {
        if opts.yes {
            to_trash(&ctx.home, &work, now)?;
            return Err(format!(
                "{} has a high finding, and --yes does not cover that: you read it and type yes yourself. Run the same command without --yes.",
                n.name
            ));
        }
        if !io.typed_yes(&format!(
            "{} has a HIGH finding. It is Python that runs with your rights. Type yes to put it in anyway, anything else to stop: ",
            n.name
        )) {
            to_trash(&ctx.home, &work, now)?;
            return Err(format!("Stopped. {} was not put in. The fetched copy is in the trash.", n.name));
        }
    }
    let tree = tree_sha256(&root)?;
    if let Ok(req) = std::fs::read_to_string(root.join("requirements.txt")) {
        let lines: Vec<&str> = req
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        if !lines.is_empty() {
            io.say(&format!(
                "{} asks for Python packages ({}). Stage never runs pip for you: install them with ComfyUI's own Python when you are ready (`python -m pip install -r {}`).",
                n.name,
                lines.iter().take(6).copied().collect::<Vec<_>>().join(", ") + if lines.len() > 6 { ", ..." } else { "" },
                dest.join("requirements.txt").display()
            ));
        }
    }
    put_in_place(&root, &dest)?;
    let _ = to_trash(&ctx.home, &work, now);
    io.say(&format!(
        "{} is in custom_nodes (tree SHA-256 {}). Restart ComfyUI so it loads.",
        n.name,
        &tree[..16]
    ));
    Ok(J::Obj(vec![
        ("commit".into(), J::s(&n.commit)),
        ("tree_sha256".into(), J::s(&tree)),
        ("by_stage".into(), J::Bool(true)),
    ]))
}

/// Everything `add` does. Returns the exit code.
pub fn add(
    ctx: &Ctx,
    host: &Host,
    cat: &Catalog,
    id: &str,
    opts: &AddOpts,
    now: u64,
    io: &mut Io,
) -> i32 {
    let Some(wf) = cat.find(id) else {
        io.say(&format!(
            "There is no \"{id}\" in Stage. `tempo-launch stage list` shows what there is."
        ));
        return 1;
    };
    let mut st = State::load(ctx);
    if st.installed(id) {
        io.say(&format!("{id} is already added. `tempo-launch stage check {id}` says what it still needs; `stage remove {id}` takes it out."));
        return 0;
    }
    let comfy = match comfy_dir(ctx, &st) {
        Ok(d) => d,
        Err(e) => {
            io.say(&e);
            return 1;
        }
    };
    for (p, v) in &opts.with {
        if pedals::check_value(cat, wf, p, v).is_err() {
            io.say(&format!("--with {p}={v}: {} has no such choice.", wf.id));
            return 1;
        }
    }
    show_plan(cat, wf, opts, &comfy, host.token.is_some(), io.out);
    // a graph from upstream with a paid node in it is refused before anything is fetched
    if !wf.api_nodes.is_empty() && !st.cloud_ok(id) {
        io.say(&format!(
            "Refused: this workflow uses {}, which call a paid or cloud service.",
            wf.api_nodes.join(", ")
        ));
        return 1;
    }
    if !opts.yes && !io.yes_no("\nAdd it? [y/N] ") {
        io.say("Not added.");
        return 0;
    }
    // the workflow file, at its pin
    let wf_path = workflow_path(ctx, id);
    if wf_path.exists() {
        if let Err(e) = to_trash(&ctx.home, wf_path.parent().unwrap_or(&wf_path), now) {
            io.say(&e);
            return 1;
        }
    }
    io.say("Fetching the workflow ...");
    if let Err(e) = fetch_checked(
        host,
        ctx,
        &wf.file_url(),
        &wf_path,
        wf.file_bytes.max(1 << 20),
        Some(&wf.file_sha256),
        now,
        None,
    ) {
        io.say(&e);
        return 1;
    }
    io.say("The workflow matches its pinned SHA-256.");
    let graph = match std::fs::read_to_string(&wf_path)
        .map_err(|e| e.to_string())
        .and_then(|t| parse(&t))
        .and_then(|j| pedals::clean_graph(&j))
    {
        Ok(g) => g,
        Err(e) => {
            io.say(&format!("The workflow could not be read: {e}"));
            return 1;
        }
    };
    let bad = pedals::check_binds(wf, &graph);
    if !bad.is_empty() {
        io.say(&format!(
            "The workflow no longer fits the catalogue ({}). Stage will not run it.",
            bad.join("; ")
        ));
        return 1;
    }
    let api = guard::api_classes_in(&graph);
    if !api.is_empty() && !st.cloud_ok(id) {
        let _ = to_trash(&ctx.home, wf_path.parent().unwrap_or(&wf_path), now);
        io.say(&format!("Refused: the graph uses {}, which call a paid or cloud service. Nothing leaves your computer unless you turn that on: `tempo-launch stage cloud {id} on`.", api.join(", ")));
        return 1;
    }
    // the custom nodes
    let mut nodes: Vec<(String, J)> = vec![];
    for n in &wf.nodes {
        match add_node(host, ctx, wf, n, &comfy, st.cloud_ok(id), opts, now, io) {
            Ok(rec) => nodes.push((n.name.clone(), rec)),
            Err(e) => {
                io.say(&e);
                io.say("Stopped before the models. What was fetched so far stays; `stage add` picks up again.");
                return 1;
            }
        }
    }
    // the models
    let mut by_hand: Vec<String> = vec![];
    let mut made: Vec<(String, J)> = vec![];
    for m in wf.models_for(&opts.with) {
        let dest = model_path(&comfy, m);
        if dest.exists() {
            continue;
        }
        // a gated model: the licence and the word "gated" first; then, with the owner's token and
        // a yes of their own (`--yes` never accepts a licence for them), the fetch
        let mut token = None;
        if m.gated {
            io.say(&format!(
                "\n{}",
                super::hf::gated_notice(&m.name, &m.licence, &m.url, host.token.is_some())
            ));
            let go = host.token.is_some()
                && super::hf::is_hf(&m.url)
                && io.yes_no("Fetch it with your token? You have read the licence and accepted it on the page. [y/N] ");
            if !go {
                by_hand.push(format!(
                    "{}  ->  {}  (accept the licence at {})",
                    m.name,
                    dest.display(),
                    super::hf::page_of(&m.url)
                ));
                continue;
            }
            token = host.token;
        }
        io.say(&format!(
            "Fetching {} ({}) ...",
            m.name,
            m.gb.map_or("size unknown".into(), gb)
        ));
        let bytes = m.gb.map_or(4_000_000_000, |g| (g * 1e9) as u64);
        match fetch_checked(
            host,
            ctx,
            &m.url,
            &dest,
            bytes,
            m.sha256.as_deref(),
            now,
            token,
        ) {
            Ok(sha) => {
                if m.sha256.is_some() {
                    io.say("SHA-256 matches the pin.");
                } else {
                    io.say(&format!("SHA-256 {} (the catalogue has no pin for this file, so Stage recorded it).", &sha[..16]));
                }
                made.push((m.name.clone(), J::s(&sha)));
            }
            Err(e) => {
                io.say(&e);
                by_hand.push(format!(
                    "{}  ->  {}  (from {})",
                    m.name,
                    dest.display(),
                    m.url
                ));
            }
        }
    }
    for (name, sha) in &made {
        if let J::Str(s) = sha {
            st.set_model_sha(name, s);
        }
    }
    st.set(
        id,
        vec![
            ("installed", J::Bool(true)),
            ("enabled", J::Bool(false)),
            ("added_at", J::s(&stamp(now))),
            ("workflow_sha256", J::s(&wf.file_sha256)),
            ("commit", J::s(&wf.commit)),
            ("nodes", J::Obj(nodes)),
            (
                "with",
                J::Arr(
                    opts.with
                        .iter()
                        .map(|(a, b)| J::s(&format!("{a}={b}")))
                        .collect(),
                ),
            ),
        ],
    );
    if st.folder().is_none() {
        st.set_folder(&comfy.to_string_lossy());
    }
    if let Err(e) = st.save(ctx) {
        io.say(&e);
        return 1;
    }
    if by_hand.is_empty() {
        io.say(&format!("\nAdded. `tempo-launch stage check {id}` to see where it fits, `stage enable {id}` to let Tempo play it."));
    } else {
        io.say(
            "\nAdded, with these files left for you (a licence to accept, or a fetch that failed):",
        );
        for l in &by_hand {
            io.say(&format!("  {l}"));
        }
        io.say(&format!(
            "Then `tempo-launch stage check {id}`, and `stage enable {id}`."
        ));
    }
    0
}

/// What a workflow still lacks on this computer: missing models, nodes, or a changed file.
pub fn missing(ctx: &Ctx, wf: &Workflow, st: &State, with: &[(String, String)]) -> Vec<String> {
    let mut v = vec![];
    let Ok(comfy) = comfy_dir(ctx, st) else {
        return vec!["the ComfyUI folder is not set (`stage folder PATH`)".into()];
    };
    match std::fs::read(workflow_path(ctx, &wf.id)) {
        Ok(b) if sha256_hex(&b) == wf.file_sha256 => {}
        Ok(_) => v.push("the workflow file no longer matches its pinned SHA-256".into()),
        Err(_) => v.push("the workflow file is not here".into()),
    }
    for n in &wf.nodes {
        if !node_path(&comfy, n).is_dir() {
            v.push(format!("custom node {} is not in custom_nodes", n.name));
        }
    }
    for m in wf.models_for(with) {
        if !model_path(&comfy, m).is_file() {
            v.push(format!("model {} is not in models/{}", m.name, m.dir));
        }
    }
    v
}

/// The `with` list Stage saved for a workflow.
pub fn saved_with(st: &State, id: &str) -> Vec<(String, String)> {
    st.record(id)
        .map(|r| r.strs("with"))
        .unwrap_or_default()
        .iter()
        .filter_map(|s| {
            s.split_once('=')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .collect()
}

pub fn remove(ctx: &Ctx, cat: &Catalog, id: &str, yes: bool, now: u64, io: &mut Io) -> i32 {
    let Some(wf) = cat.find(id) else {
        io.say(&format!("There is no \"{id}\" in Stage."));
        return 1;
    };
    let mut st = State::load(ctx);
    if !st.installed(id) {
        io.say(&format!("{id} is not added."));
        return 0;
    }
    let comfy = comfy_dir(ctx, &st).ok();
    // a node pack goes only if Stage put it there and no other workflow of Stage's needs it
    let mine: Vec<&NodePack> = wf
        .nodes
        .iter()
        .filter(|n| {
            st.record(id)
                .and_then(|r| r.get("nodes"))
                .and_then(|x| x.get(&n.name))
                .and_then(|x| x.bool("by_stage"))
                .unwrap_or(false)
                && st.node_owner(&n.name).iter().all(|o| o == id)
        })
        .collect();
    io.say(&format!("Taking out {} ({id}).", wf.title));
    io.say("  The workflow file goes to the trash.");
    for n in &mine {
        io.say(&format!(
            "  The custom node {} that Stage put in goes to the trash.",
            n.name
        ));
    }
    io.say("  Models stay where they are: other workflows share them, and they are big. Move them to the trash yourself if you want the room.");
    if !yes && !io.yes_no("Remove it? [y/N] ") {
        io.say("Kept.");
        return 0;
    }
    if let Some(p) = workflow_path(ctx, id).parent().filter(|p| p.exists()) {
        if let Err(e) = to_trash(&ctx.home, p, now) {
            io.say(&e);
            return 1;
        }
    }
    if let Some(c) = &comfy {
        for n in &mine {
            let p = node_path(c, n);
            if p.exists() {
                match to_trash(&ctx.home, &p, now) {
                    Ok(t) => io.say(&format!("{} is in the trash: {}", n.name, t.display())),
                    Err(e) => io.say(&e),
                }
            }
        }
    }
    st.forget(id);
    if let Err(e) = st.save(ctx) {
        io.say(&e);
        return 1;
    }
    io.say("Removed. Nothing was deleted.");
    0
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::bank::detect::tests::{ctx_in, tmp};
    use crate::bank::install::Pm;
    use crate::setup::registry::{Fetch, Fetched};
    use crate::stage::catalog::embedded;
    use crate::stage::guard::tests::FakeSoundcheck;
    use std::sync::{Arc, Mutex};

    /// A fetcher with a shelf of pretend files by address; a tar.gz it hands out is a marker the
    /// fake `tar` below turns into a folder.
    pub struct Shelf(
        pub Mutex<Vec<(String, Vec<u8>)>>,
        pub Mutex<Vec<String>>,
        /// Every hop of a gated fetch: the address, and the bearer token that went with it.
        pub Mutex<Vec<(String, Option<String>)>>,
    );

    impl Fetch for Shelf {
        fn get(&self, _: &str, _: Option<&str>, _: bool) -> Fetched {
            Fetched::Failed
        }
        fn download(
            &self,
            url: &str,
            part: &Path,
            _: u64,
            _: &mut dyn FnMut(u64),
        ) -> Result<(), String> {
            self.1.lock().unwrap().push(url.to_string());
            let shelf = self.0.lock().unwrap();
            let (_, body) = shelf
                .iter()
                .find(|(u, _)| u == url)
                .ok_or_else(|| format!("404 {url}"))?;
            std::fs::write(part, body).map_err(|e| e.to_string())
        }
        /// huggingface.co hands a gated file over from another server, and only to a token.
        fn probe(
            &self,
            url: &str,
            bearer: Option<&str>,
        ) -> Result<crate::setup::registry::Probe, String> {
            use crate::setup::registry::Probe;
            self.2
                .lock()
                .unwrap()
                .push((url.to_string(), bearer.map(String::from)));
            Ok(if url.starts_with("https://huggingface.co/") {
                if bearer.is_some() {
                    Probe::Moved(
                        "https://cdn-lfs.huggingface.co/x/y/gated.safetensors?sig=1".into(),
                    )
                } else {
                    Probe::Denied(401)
                }
            } else {
                Probe::Here
            })
        }
        fn download_hop(
            &self,
            url: &str,
            part: &Path,
            _: u64,
            _: &mut dyn FnMut(u64),
            bearer: Option<&str>,
        ) -> Result<(), String> {
            self.2
                .lock()
                .unwrap()
                .push((format!("GET {url}"), bearer.map(String::from)));
            self.1.lock().unwrap().push(url.to_string());
            let shelf = self.0.lock().unwrap();
            let (_, body) = shelf
                .iter()
                .find(|(u, _)| u == url)
                .ok_or_else(|| format!("404 {url}"))?;
            std::fs::write(part, body).map_err(|e| e.to_string())
        }
    }

    /// A package manager that only fetches from the shelf, and "unpacks" a node archive by
    /// writing the files its marker lists.
    pub struct FakePm {
        pub shelf: Arc<Shelf>,
        pub node_files: Vec<(String, String)>,
    }

    impl Pm for FakePm {
        fn which(&self, _: &str) -> bool {
            true
        }
        fn run(&self, argv: &[String]) -> Result<(), String> {
            // tar -xf FILE -C DEST: a top folder like GitHub's, with the node's files in it
            let dest = PathBuf::from(&argv[4]);
            let top = dest.join("pack-abcdef");
            for (n, b) in &self.node_files {
                let p = top.join(n);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, b).unwrap();
            }
            Ok(())
        }
        fn fetcher(&self) -> Option<Arc<dyn Fetch>> {
            Some(self.shelf.clone())
        }
    }

    pub fn comfy_folder(d: &Path) -> PathBuf {
        let c = d.join("ComfyUI");
        std::fs::create_dir_all(c.join("custom_nodes")).unwrap();
        std::fs::create_dir_all(c.join("models")).unwrap();
        c
    }

    /// A catalogue of one small workflow whose file the shelf holds, to play the whole of `add`.
    pub fn tiny(shelf_body: &[u8], with_node: bool) -> (Catalog, Arc<Shelf>) {
        let mut cat = embedded();
        let mut wf = cat.find("paint-edit-photo").unwrap().clone();
        wf.file_sha256 = sha256_hex(shelf_body);
        wf.file_bytes = shelf_body.len() as u64;
        wf.models = vec![
            Model {
                name: "small.safetensors".into(),
                dir: "loras".into(),
                licence: "Apache-2.0".into(),
                gb: Some(0.001),
                url: "https://example.invalid/small.safetensors".into(),
                gated: false,
                sha256: Some(sha256_hex(b"weights")),
                verified: true,
                only_for: None,
            },
            Model {
                name: "gated.safetensors".into(),
                dir: "vae".into(),
                licence: "FLUX Kommunity License".into(),
                gb: Some(0.3),
                url: "https://huggingface.co/x/y/resolve/main/gated.safetensors".into(),
                gated: true,
                sha256: None,
                verified: false,
                only_for: None,
            },
        ];
        wf.nodes = if with_node {
            vec![NodePack {
                name: "ComfyUI-Thing".into(),
                repo: "someone/ComfyUI-Thing".into(),
                commit: "a".repeat(40),
                licence: "MIT".into(),
                subdir: None,
            }]
        } else {
            vec![]
        };
        cat.workflows = vec![wf.clone()];
        let shelf = Arc::new(Shelf(
            Mutex::new(vec![
                (wf.file_url(), shelf_body.to_vec()),
                ("https://example.invalid/small.safetensors".into(), b"weights".to_vec()),
                (
                    "https://codeload.github.com/someone/ComfyUI-Thing/tar.gz/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    b"archive".to_vec(),
                ),
            ]),
            Mutex::new(vec![]),
            Mutex::new(vec![]),
        ));
        shelf.0.lock().unwrap().push((
            "https://cdn-lfs.huggingface.co/x/y/gated.safetensors?sig=1".into(),
            b"gated-weights".to_vec(),
        ));
        (cat, shelf)
    }

    /// The real inpaint graph's shape, enough for the catalogue's binds to find their nodes.
    pub fn inpaint_graph() -> String {
        r#"{"11":{"class_type":"LoadImage","inputs":{"image":"a.png"}},
            "12":{"class_type":"LoadImage","inputs":{"image":"m.png"}},
            "4":{"class_type":"CLIPTextEncode","inputs":{"text":"x","clip":["1",0]}},
            "7":{"class_type":"KSampler","inputs":{"steps":16,"seed":1}},
            "9":{"class_type":"SaveImage","inputs":{"images":["8",0]}}}"#
            .into()
    }

    fn run_add(
        ctx: &Ctx,
        pm: &FakePm,
        sc: &FakeSoundcheck,
        cat: &Catalog,
        opts: AddOpts,
        answers: &[&str],
    ) -> (i32, String) {
        let mut out = Vec::new();
        let mut it = answers
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter();
        let mut ask = |_: &str| it.next().unwrap_or_default();
        let host = Host {
            pm,
            os: "linux",
            sc,
            token: None,
        };
        let code = add(
            ctx,
            &host,
            cat,
            "paint-edit-photo",
            &opts,
            1_700_000_000,
            &mut Io {
                out: &mut out,
                ask: &mut ask,
            },
        );
        (code, String::from_utf8(out).unwrap())
    }

    fn setup(
        tag: &str,
        with_node: bool,
        node_py: &str,
    ) -> (
        Ctx,
        PathBuf,
        Catalog,
        FakePm,
        crate::woodshed::takes::TestDir,
    ) {
        let d = tmp(tag);
        let ctx = ctx_in(&d.join("home"));
        std::fs::create_dir_all(&ctx.home).unwrap();
        let comfy = comfy_folder(&d);
        let mut st = State::load(&ctx);
        st.set_folder(&comfy.to_string_lossy());
        st.save(&ctx).unwrap();
        let (cat, shelf) = tiny(inpaint_graph().as_bytes(), with_node);
        let pm = FakePm {
            shelf,
            node_files: vec![
                ("nodes.py".into(), node_py.into()),
                ("requirements.txt".into(), "numpy\nkornia\n".into()),
            ],
        };
        (ctx, comfy, cat, pm, d)
    }

    #[test]
    fn add_asks_first_shows_sizes_and_fetches_with_hashes() {
        let (ctx, comfy, cat, pm, _dir) = setup("add-ok", false, "");
        let sc = FakeSoundcheck(Ok(vec![]));
        // no: nothing happens
        let (code, out) = run_add(&ctx, &pm, &sc, &cat, AddOpts::default(), &["n"]);
        assert_eq!(code, 0);
        assert!(
            out.contains("Not added")
                && out.contains("small.safetensors")
                && out.contains("gated: its licence needs your yes"),
            "{out}"
        );
        assert!(
            pm.shelf.1.lock().unwrap().is_empty()
                && !State::load(&ctx).installed("paint-edit-photo")
        );
        // yes
        let (code, out) = run_add(&ctx, &pm, &sc, &cat, AddOpts::default(), &["y"]);
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("matches its pinned SHA-256")
                && out.contains("Added, with these files left for you"),
            "{out}"
        );
        assert!(
            out.contains("gated.safetensors") && out.contains("accept the licence"),
            "{out}"
        );
        assert_eq!(
            std::fs::read(comfy.join("models/loras/small.safetensors")).unwrap(),
            b"weights"
        );
        assert!(workflow_path(&ctx, "paint-edit-photo").is_file());
        let st = State::load(&ctx);
        assert!(st.installed("paint-edit-photo") && !st.enabled("paint-edit-photo"));
        // the model with no hash upstream is not what readiness waits on: the gated one is
        let wf = cat.find("paint-edit-photo").unwrap();
        let m = missing(&ctx, wf, &st, &[]);
        assert_eq!(m, ["model gated.safetensors is not in models/vae"], "{m:?}");
    }

    /// Like `run_add`, with an owner who has a Hugging Face token.
    fn run_add_with_token(
        ctx: &Ctx,
        pm: &FakePm,
        cat: &Catalog,
        opts: AddOpts,
        answers: &[&str],
        token: &crate::stage::hf::Token,
    ) -> (i32, String) {
        let sc = FakeSoundcheck(Ok(vec![]));
        let mut out = Vec::new();
        let mut it = answers
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter();
        let mut ask = |_: &str| it.next().unwrap_or_default();
        let host = Host {
            pm,
            os: "linux",
            sc: &sc,
            token: Some(token),
        };
        let code = add(
            ctx,
            &host,
            cat,
            "paint-edit-photo",
            &opts,
            1_700_000_000,
            &mut Io {
                out: &mut out,
                ask: &mut ask,
            },
        );
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_gated_model_shows_its_licence_then_is_fetched_with_the_token_only_after_a_yes_of_its_own()
    {
        let tok = crate::stage::hf::tests::fake_token();
        let t = crate::stage::hf::Token::new(&tok).unwrap();
        let (ctx, comfy, cat, pm, _dir) = setup("add-gated", false, "");
        // `--yes` covers the plan, not the licence: the second question gets "n"
        let (code, out) = run_add_with_token(
            &ctx,
            &pm,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &["n"],
            &t,
        );
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("gated.safetensors is gated")
                && out.contains("Its licence:")
                && out.contains("Stage found your Hugging Face token")
                && out.contains("Added, with these files left for you"),
            "{out}"
        );
        assert!(
            pm.shelf.2.lock().unwrap().is_empty(),
            "nothing was asked of Hugging Face"
        );
        assert!(!comfy.join("models/vae/gated.safetensors").exists());
        // the token never reaches the output, whatever the answer
        assert!(!out.contains(&tok));
        // a fresh home, and this time the owner says yes to the licence question
        let (ctx, comfy, cat, pm, _dir) = setup("add-gated-yes", false, "");
        let (code, out) = run_add_with_token(
            &ctx,
            &pm,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &["y"],
            &t,
        );
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            std::fs::read(comfy.join("models/vae/gated.safetensors")).unwrap(),
            b"gated-weights"
        );
        assert!(!out.contains(&tok), "{out}");
        let hops = pm.shelf.2.lock().unwrap().clone();
        // huggingface.co got the token; the server it handed the file over from did not
        let hf: Vec<_> = hops
            .iter()
            .filter(|(u, _)| u.contains("//huggingface.co/"))
            .collect();
        let cdn: Vec<_> = hops.iter().filter(|(u, _)| u.contains("cdn-lfs")).collect();
        assert!(
            !hf.is_empty() && hf.iter().all(|(_, b)| b.as_deref() == Some(tok.as_str())),
            "{hops:?}"
        );
        assert!(
            !cdn.is_empty() && cdn.iter().all(|(_, b)| b.is_none()),
            "{hops:?}"
        );
        // no token on file anywhere under the home: the state, the logs, the trash
        fn all_text(d: &Path, out: &mut String) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    all_text(&p, out);
                } else if let Ok(t) = std::fs::read_to_string(&p) {
                    out.push_str(&t);
                }
            }
        }
        let mut on_disk = String::new();
        all_text(&ctx.home, &mut on_disk);
        assert!(!on_disk.contains(&tok), "the token was written down");
    }

    #[test]
    fn without_a_token_a_gated_model_is_left_for_the_owner_and_the_licence_is_still_shown() {
        let (ctx, _comfy, cat, pm, _dir) = setup("add-gated-none", false, "");
        let sc = FakeSoundcheck(Ok(vec![]));
        let (code, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("is gated")
                && out.contains("HF_TOKEN")
                && out.contains("accept the licence at"),
            "{out}"
        );
        assert!(pm.shelf.2.lock().unwrap().is_empty());
    }

    #[test]
    fn a_workflow_file_with_the_wrong_hash_is_refused_and_trashed() {
        let (ctx, comfy, mut cat, pm, _dir) = setup("add-badhash", false, "");
        cat.workflows[0].file_sha256 = "0".repeat(64);
        let sc = FakeSoundcheck(Ok(vec![]));
        let (code, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert_eq!(code, 1);
        assert!(
            out.contains("did not match its pinned SHA-256") && out.contains("trash"),
            "{out}"
        );
        assert!(!workflow_path(&ctx, "paint-edit-photo").exists());
        assert!(!State::load(&ctx).installed("paint-edit-photo"));
        assert!(!comfy.join("models/loras/small.safetensors").exists());
    }

    #[test]
    fn a_model_with_the_wrong_hash_is_refused_and_never_put_in_place() {
        let (ctx, comfy, mut cat, pm, _dir) = setup("add-badmodel", false, "");
        cat.workflows[0].models[0].sha256 = Some("f".repeat(64));
        let sc = FakeSoundcheck(Ok(vec![]));
        let (_, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert!(out.contains("did not match its pinned SHA-256"), "{out}");
        assert!(!comfy.join("models/loras/small.safetensors").exists());
        assert!(!comfy.join("models/loras/small.safetensors.part").exists());
    }

    #[test]
    fn a_high_finding_blocks_until_the_owner_types_yes_and_yes_flag_does_not_cover_it() {
        let bad = "def run(x):\n    return eval(x)\n";
        // --yes: refused, the fetched copy goes to the trash
        let (ctx, comfy, cat, pm, _dir) = setup("add-high-yes", true, bad);
        let sc = FakeSoundcheck(Err(
            "the Tempo agent is not installed here, so Soundcheck did not run".into(),
        ));
        let (code, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert_eq!(code, 1);
        assert!(
            out.contains("1 HIGH") && out.contains("--yes does not cover that"),
            "{out}"
        );
        assert!(!comfy.join("custom_nodes/ComfyUI-Thing").exists());
        // a plain "y" is not "yes"
        let (ctx, comfy, cat, pm, _dir) = setup("add-high-y", true, bad);
        let (code, out) = run_add(&ctx, &pm, &sc, &cat, AddOpts::default(), &["y", "y"]);
        assert_eq!(code, 1, "{out}");
        assert!(
            out.contains("Stopped. ComfyUI-Thing was not put in"),
            "{out}"
        );
        assert!(!comfy.join("custom_nodes/ComfyUI-Thing").exists());
        // typed yes: it goes in, with its tree hash, and pip is never run
        let (ctx2, comfy2, cat2, pm2, _dir) = setup("add-high-typed", true, bad);
        let (code, out) = run_add(&ctx2, &pm2, &sc, &cat2, AddOpts::default(), &["y", "yes"]);
        assert_eq!(code, 0, "{out}");
        assert!(comfy2.join("custom_nodes/ComfyUI-Thing/nodes.py").is_file());
        assert!(
            out.contains("Stage never runs pip") && out.contains("numpy"),
            "{out}"
        );
        let st = State::load(&ctx2);
        let rec = st
            .record("paint-edit-photo")
            .unwrap()
            .get("nodes")
            .unwrap()
            .get("ComfyUI-Thing")
            .unwrap();
        assert_eq!(rec.str("tree_sha256").unwrap().len(), 64);
        assert_eq!(rec.bool("by_stage"), Some(true));
        let _ = (ctx, comfy);
    }

    #[test]
    fn a_node_that_calls_a_cloud_api_is_refused_unless_the_owner_allowed_it() {
        let cloud = "import requests\nKEY = 'x'\ndef go():\n    return requests.post('https://api.example.com/run', headers={'Authorization': 'Bearer ' + KEY})\n";
        let (ctx, comfy, cat, pm, _dir) = setup("add-cloud", true, cloud);
        let sc = FakeSoundcheck(Ok(vec![]));
        let (code, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert_eq!(code, 1);
        assert!(
            out.contains("refused: ComfyUI-Thing calls a paid or cloud service")
                && out.contains("stage cloud paint-edit-photo on"),
            "{out}"
        );
        assert!(!comfy.join("custom_nodes/ComfyUI-Thing").exists());
        // allowed for this workflow: it still has the high finding to read, which --yes does not cover
        let mut st = State::load(&ctx);
        st.set("paint-edit-photo", vec![("allow_cloud", J::Bool(true))]);
        st.save(&ctx).unwrap();
        let (code, out) = run_add(&ctx, &pm, &sc, &cat, AddOpts::default(), &["y", "yes"]);
        assert_eq!(code, 0, "{out}");
        assert!(comfy.join("custom_nodes/ComfyUI-Thing").is_dir());
    }

    #[test]
    fn a_pack_the_owner_already_has_is_left_alone() {
        let (ctx, comfy, cat, pm, _dir) = setup("add-have", true, "def f():\n    return 1\n");
        std::fs::create_dir_all(comfy.join("custom_nodes/ComfyUI-Thing")).unwrap();
        std::fs::write(comfy.join("custom_nodes/ComfyUI-Thing/mine.py"), "x = 1\n").unwrap();
        let sc = FakeSoundcheck(Ok(vec![]));
        let (code, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("already in custom_nodes") && out.contains("leaves it alone"),
            "{out}"
        );
        assert_eq!(
            std::fs::read_to_string(comfy.join("custom_nodes/ComfyUI-Thing/mine.py")).unwrap(),
            "x = 1\n"
        );
        assert!(!pm
            .shelf
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.contains("codeload")));
    }

    #[test]
    fn a_graph_with_a_paid_node_is_refused_before_anything_else() {
        let body = inpaint_graph().replace("\"9\":{\"class_type\":\"SaveImage\"", "\"2\":{\"class_type\":\"KlingTextToVideoNode\",\"inputs\":{}},\"9\":{\"class_type\":\"SaveImage\"");
        let (ctx, _, cat, pm, _dir) = setup("add-paidgraph", false, "");
        let (cat, shelf) = {
            let (mut c, s) = tiny(body.as_bytes(), false);
            c.workflows[0].models.clear();
            let _ = cat;
            (c, s)
        };
        let pm = FakePm {
            shelf,
            node_files: pm.node_files,
        };
        let sc = FakeSoundcheck(Ok(vec![]));
        let (code, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert_eq!(code, 1);
        assert!(
            out.contains("KlingTextToVideoNode") && out.contains("paid or cloud service"),
            "{out}"
        );
        assert!(!State::load(&ctx).installed("paint-edit-photo"));
    }

    #[test]
    fn remove_goes_to_the_trash_and_keeps_models() {
        let (ctx, comfy, cat, pm, _dir) = setup("remove", true, "def f():\n    return 1\n");
        let sc = FakeSoundcheck(Ok(vec![]));
        let (code, out) = run_add(
            &ctx,
            &pm,
            &sc,
            &cat,
            AddOpts {
                yes: true,
                with: vec![],
            },
            &[],
        );
        assert_eq!(code, 0, "{out}");
        assert!(comfy.join("custom_nodes/ComfyUI-Thing").is_dir());
        let mut out = Vec::new();
        let mut ask = |_: &str| "y".to_string();
        let code = remove(
            &ctx,
            &cat,
            "paint-edit-photo",
            false,
            1_700_000_000,
            &mut Io {
                out: &mut out,
                ask: &mut ask,
            },
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(
            !comfy.join("custom_nodes/ComfyUI-Thing").exists()
                && !workflow_path(&ctx, "paint-edit-photo").exists()
        );
        assert!(
            comfy.join("models/loras/small.safetensors").is_file(),
            "models stay"
        );
        let trash = ctx.home.join("trash");
        assert!(trash.is_dir() && walk_count(&trash) >= 2, "{out}");
        assert!(!State::load(&ctx).installed("paint-edit-photo"));
    }

    fn walk_count(d: &Path) -> usize {
        std::fs::read_dir(d).map_or(0, |r| {
            r.flatten()
                .map(|e| {
                    if e.path().is_dir() {
                        1 + walk_count(&e.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
    }

    #[test]
    fn a_folder_that_is_not_comfyui_is_said_so() {
        let d = tmp("not-comfy");
        let ctx = ctx_in(&d);
        let mut st = State::load(&ctx);
        st.set_folder(&d.to_string_lossy());
        let e = comfy_dir(&ctx, &st).unwrap_err();
        assert!(e.contains("not a ComfyUI folder"), "{e}");
        let st = State::load(&ctx_in(&tmp("nofolder")));
        assert!(comfy_dir(&ctx_in(&tmp("nofolder2")), &st)
            .unwrap_err()
            .contains("stage folder"));
    }

    #[test]
    fn the_tree_hash_changes_when_a_file_does() {
        let d = tmp("tree");
        std::fs::write(d.join("a.py"), "1").unwrap();
        let h1 = tree_sha256(&d).unwrap();
        assert_eq!(tree_sha256(&d).unwrap(), h1);
        std::fs::write(d.join("a.py"), "2").unwrap();
        assert_ne!(tree_sha256(&d).unwrap(), h1);
    }
}
