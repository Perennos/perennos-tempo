//! `conductor run`: the Conductor raises the baton and walks a flow's members in order. Each gets the flow's goal and the step before it,
//! answers in a guest conversation (no tools), and the launcher files the answer as a note:
//!
//! `<notes folder>/<date> <flow>/<n> <member>.md`, with front matter and `[[links]]` to the step
//! before and to the flow's `Index` note, so the Vault graph shows the chain.
//!
//! Notes go to the Vault when one is set up, else to `<Tempo home>/bands/<band>/runs/`. Nothing
//! is written outside that one folder. Material the owner wants a flow to read goes in an `Inbox`
//! folder next to the runs; only the first step sees it.

use super::engines::Cast;
use super::spec::{Band, Flow};
use super::{with_guard, Chat};
use crate::{civil, redact, write_atomic, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_PREVIOUS: usize = 8000;
const MAX_INBOX_FILES: usize = 10;
const MAX_INBOX_FILE: usize = 6000;
const MAX_INBOX: usize = 20000;
/// Runs shown on the dashboard.
pub const RECENT: usize = 6;

/// Where a flow's notes go, and how they are linked.
#[derive(Debug, PartialEq)]
pub struct Places {
    /// The folder the run folders are made in.
    pub root: PathBuf,
    /// What a `[[link]]` starts with: the Vault-relative folder, or nothing for the fallback.
    pub link_base: String,
    /// The folder the owner drops material into.
    pub inbox: PathBuf,
}

pub fn places(ctx: &Ctx, band: &Band, flow: &Flow) -> Places {
    if ctx.vault.is_dir() {
        let root = flow
            .notes_to
            .split('/')
            .filter(|s| !s.is_empty())
            .fold(ctx.vault.clone(), |p, s| p.join(s));
        return Places {
            inbox: root.join("Inbox"),
            root,
            link_base: flow.notes_to.clone(),
        };
    }
    let own = super::band_dir(ctx, &band.name);
    let inside = flow
        .notes_to
        .strip_prefix(&super::spec::default_notes_to(&band.name))
        .unwrap_or("");
    let root = inside
        .split('/')
        .filter(|s| !s.is_empty())
        .fold(own.join("runs"), |p, s| p.join(s));
    Places {
        root,
        link_base: inside.to_string(),
        inbox: own.join("Inbox"),
    }
}

/// `2026-11-07`
fn date_of(now: u64) -> String {
    let (y, m, d, ..) = civil(now);
    format!("{y:04}-{m:02}-{d:02}")
}

/// What the owner left in the Inbox: text files directly in the folder (no links, no subfolders),
/// at most ten, each cut to a screen or two. Empty when there is nothing.
pub fn read_inbox(dir: &Path) -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
                && p.extension().is_some_and(|x| x == "md" || x == "txt")
        })
        .collect();
    files.sort();
    let mut out = String::new();
    for p in files.into_iter().take(MAX_INBOX_FILES) {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let name = p
            .file_stem()
            .map_or(String::new(), |s| s.to_string_lossy().into_owned());
        out.push_str(&format!(
            "--- {name} ---\n{}\n\n",
            cut(redact(text.trim()).as_str(), MAX_INBOX_FILE)
        ));
        if out.chars().count() > MAX_INBOX {
            break;
        }
    }
    cut(out.trim(), MAX_INBOX)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{} [cut]", s.chars().take(max).collect::<String>())
    }
}

/// What a step is told.
fn step_text(
    band: &Band,
    flow: &Flow,
    n: usize,
    handle: &str,
    note: Option<&str>,
    inbox: &str,
    previous: Option<(&str, &str)>,
) -> String {
    let who = band.member(handle).map_or("a member", |m| m.name.as_str());
    let mut t = format!("Flow goal: {}\n", flow.goal.trim());
    if let Some(n) = note.filter(|n| !n.trim().is_empty()) {
        t.push_str(&format!("Note from the owner for this run: {}\n", n.trim()));
    }
    t.push_str(&format!(
        "\nYou are {who} (@{handle}), step {n} of {} in the flow \"{}\".\n",
        flow.steps.len(),
        flow.name
    ));
    if n == 1 && !inbox.is_empty() {
        t.push_str(&format!(
            "\nMaterial the owner left for this flow:\n{inbox}\n"
        ));
    }
    match previous {
        Some((name, text)) => t.push_str(&format!(
            "\nThe step before you was written by {name}:\n{}\n",
            cut(text, MAX_PREVIOUS)
        )),
        None => t.push_str("\nYou are first, so nothing comes before you.\n"),
    }
    t.push_str("\nDo your part now. Answer in plain Markdown, in full.");
    t
}

struct Step {
    n: usize,
    handle: String,
    name: String,
    instrument: String,
    /// The address of the engine the agent said played this step, when it said.
    engine: Option<String>,
}

impl Step {
    fn file(&self) -> String {
        format!("{} {}", self.n, self.handle)
    }
}

/// A name for the engine at `url`: the id the Conductor knows it by, else its address.
fn engine_label(url: &str, cast: &[Cast]) -> String {
    let bare = url.trim_end_matches('/');
    cast.iter()
        .find(|c| !c.url.is_empty() && c.url.trim_end_matches('/') == bare)
        .map(|c| c.engine.clone())
        .unwrap_or_else(|| bare.split_once("://").map_or(bare, |x| x.1).to_string())
}

pub(super) fn link(base: &str, folder: &str, file: &str, label: &str) -> String {
    format!("[[{base}{folder}/{file}|{label}]]")
}

#[allow(clippy::too_many_arguments)]
fn index_note(
    band: &Band,
    flow: &Flow,
    base: &str,
    folder: &str,
    date: &str,
    done: &[Step],
    stopped: Option<&str>,
    cast: &[Cast],
) -> String {
    let mut t = format!(
        "---\nband: {}\nflow: {}\nkind: flow-index\ndate: {date}\ntags: [tempo-band]\n---\n# {}, {date}\n\nGoal: {}\n\n",
        band.name,
        flow.name,
        flow.name,
        flow.goal.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    for s in done {
        t.push_str(&format!(
            "{}. {}\n",
            s.n,
            link(
                base,
                folder,
                &s.file(),
                &format!("{} ({})", s.handle, s.name)
            )
        ));
    }
    if let Some(why) = stopped {
        t.push_str(&format!("\nThe flow stopped early: {why}\n"));
    }
    let used: Vec<&Cast> = flow
        .steps
        .iter()
        .filter_map(|h| cast.iter().find(|c| &c.handle == h))
        .collect();
    if !used.is_empty() {
        t.push_str("\nEngines, as the Conductor cast them for this run:\n");
        let (mut told, mut guessed) = (false, false);
        for c in used {
            let played = done
                .iter()
                .rev()
                .find(|s| s.handle == c.handle)
                .and_then(|s| s.engine.as_deref());
            let on = match played {
                Some(url) => {
                    told = true;
                    format!("played on {}", engine_label(url, cast))
                }
                None => {
                    guessed = true;
                    if c.engine.is_empty() {
                        "the agent's own order".to_string()
                    } else {
                        c.engine.clone()
                    }
                }
            };
            t.push_str(&format!(
                "- {} ({}): {on}, model {}",
                c.handle, c.name, c.model
            ));
            if !c.note.is_empty() {
                t.push_str(&format!(". Note: {}", c.note));
            }
            t.push('\n');
        }
        if guessed {
            t.push_str("The agent decides which engine answers: it tries the first one it knows and moves on only when one cannot be reached.\n");
        }
        if told {
            t.push_str("The agent asks the engine a member was cast on first, and moves on to the next one when it cannot be reached or does not have the model.\n");
        }
    }
    t.push_str("\nDrafts by AI members on this computer. Nothing here was posted or sent.\n");
    t
}

#[allow(clippy::too_many_arguments)]
fn step_note(
    band: &Band,
    flow: &Flow,
    base: &str,
    folder: &str,
    date: &str,
    s: &Step,
    before: Option<&Step>,
    text: &str,
) -> String {
    let mut t = format!(
        "---\nband: {}\nmember: {}\nflow: {}\nstep: {}\ndate: {date}\ntags: [tempo-band]\n---\n# {} {}: {}, the {}\n\nFlow: {}\n",
        band.name,
        s.handle,
        flow.name,
        s.n,
        s.n,
        s.handle,
        s.name,
        s.instrument,
        link(base, folder, "Index", &format!("{}, {date}", flow.name)),
    );
    if let Some(b) = before {
        t.push_str(&format!(
            "Step before: {}\n",
            link(
                base,
                folder,
                &b.file(),
                &format!("{} ({})", b.handle, b.name)
            )
        ));
    }
    t.push_str(&format!(
        "\n{}\n\n---\nA draft by {} on this computer. Nothing here was posted or sent.\n",
        text.trim(),
        s.name
    ));
    t
}

/// What a run made.
#[derive(Debug)]
pub struct Outcome {
    pub folder: PathBuf,
    pub index: PathBuf,
    pub notes: Vec<PathBuf>,
}

/// A folder for this run that does not exist yet: `<date> <flow>`, then `(2)`, `(3)`...
pub(super) fn make_run_folder(
    root: &Path,
    date: &str,
    flow: &str,
) -> Result<(PathBuf, String), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("could not make {}: {e}", root.display()))?;
    for i in 1..100 {
        let name = if i == 1 {
            format!("{date} {flow}")
        } else {
            format!("{date} {flow} ({i})")
        };
        let p = root.join(&name);
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok((p, name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("could not make {}: {e}", p.display())),
        }
    }
    Err("a hundred runs of this flow today already; look in the notes folder".into())
}

/// Run `flow` of `band` with every member on the agent's own order.
pub fn run_flow(
    ctx: &Ctx,
    band: &Band,
    flow: &Flow,
    note: Option<&str>,
    chat: &dyn Chat,
    now: u64,
    out: &mut dyn Write,
) -> Result<Outcome, String> {
    run_flow_cast(ctx, band, flow, note, chat, now, &[], out)
}

/// Run `flow` of `band`: every step through `chat`, every answer filed as a note. `cast` says
/// which engine and model each member plays on (see `engines::cast`); the index note keeps it.
#[allow(clippy::too_many_arguments)]
pub fn run_flow_cast(
    ctx: &Ctx,
    band: &Band,
    flow: &Flow,
    note: Option<&str>,
    chat: &dyn Chat,
    now: u64,
    cast: &[Cast],
    out: &mut dyn Write,
) -> Result<Outcome, String> {
    let at = places(ctx, band, flow);
    let date = date_of(now);
    let (dir, folder) = make_run_folder(&at.root, &date, &flow.name)?;
    let inbox = read_inbox(&at.inbox);
    let _ = writeln!(
        out,
        "The Conductor raises the baton: {} / {} ({} steps). Notes go to {}",
        band.name,
        flow.name,
        flow.steps.len(),
        dir.display()
    );
    if !inbox.is_empty() {
        let _ = writeln!(out, "  Reading what is in {}", at.inbox.display());
    }
    let cues: Vec<Cast> = flow
        .steps
        .iter()
        .filter_map(|h| cast.iter().find(|c| &c.handle == h).cloned())
        .collect();
    let _ = write!(out, "{}", super::engines::cast_words(&cues));
    let mut done: Vec<Step> = Vec::new();
    let mut notes = Vec::new();
    let mut last_text = String::new();
    let index = dir.join("Index.md");
    for (i, handle) in flow.steps.iter().enumerate() {
        let n = i + 1;
        let m = band
            .member(handle)
            .ok_or_else(|| format!("{handle} is not in the band"))?;
        let _ = writeln!(
            out,
            "  step {n} of {}: {} (@{handle}) is on it...",
            flow.steps.len(),
            m.name
        );
        let previous = done.last().map(|s| (s.name.as_str(), last_text.as_str()));
        let ask = step_text(band, flow, n, handle, note, &inbox, previous);
        let model = cast
            .iter()
            .find(|c| &c.handle == handle)
            .map_or(m.model.as_str(), |c| c.model.as_str());
        let url = cast
            .iter()
            .find(|c| &c.handle == handle)
            .map_or("", |c| c.url.as_str());
        let (answer, engine) =
            match chat.say_for(&band.name, handle, url, model, &with_guard(&m.role), &ask) {
                Ok(a) => (redact(a.text.trim()), a.engine),
                Err(e) => {
                    let why = format!("{} could not answer ({e})", m.name);
                    let _ = write_atomic(
                        &index,
                        index_note(
                            band,
                            flow,
                            &at.link_base,
                            &folder,
                            &date,
                            &done,
                            Some(&why),
                            cast,
                        )
                        .as_bytes(),
                    );
                    return Err(format!(
                        "{why}. What was done so far is in {}",
                        dir.display()
                    ));
                }
            };
        let step = Step {
            n,
            handle: handle.clone(),
            name: m.name.clone(),
            instrument: m.instrument.clone(),
            engine,
        };
        let file = dir.join(format!("{}.md", step.file()));
        let body = step_note(
            band,
            flow,
            &at.link_base,
            &folder,
            &date,
            &step,
            done.last(),
            &answer,
        );
        write_atomic(&file, body.as_bytes())
            .map_err(|e| format!("could not write {}: {e}", file.display()))?;
        let _ = writeln!(out, "    wrote {}", file.display());
        notes.push(file);
        last_text = answer;
        done.push(step);
        write_atomic(
            &index,
            index_note(band, flow, &at.link_base, &folder, &date, &done, None, cast).as_bytes(),
        )
        .map_err(|e| format!("could not write {}: {e}", index.display()))?;
    }
    let _ = writeln!(out, "Done. Start at {}", index.display());
    Ok(Outcome {
        folder: dir,
        index,
        notes,
    })
}

/// One finished or partial run, for the dashboard.
#[derive(Debug, PartialEq)]
pub struct RunInfo {
    pub folder: String,
    /// The index first, then the steps in order.
    pub notes: Vec<(String, PathBuf)>,
}

/// The newest run folders (`<date> <flow>`) under the band's notes folders.
pub fn recent_runs(ctx: &Ctx, band: &Band) -> Vec<RunInfo> {
    let mut roots: Vec<PathBuf> = band
        .flows
        .iter()
        .map(|f| places(ctx, band, f).root)
        .collect();
    roots.sort();
    roots.dedup();
    let mut folders: Vec<(String, PathBuf)> = roots
        .iter()
        .flat_map(|r| std::fs::read_dir(r).into_iter().flatten().flatten())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let b = n.as_bytes();
            let dated = b.len() > 11
                && b[4] == b'-'
                && b[7] == b'-'
                && b[10] == b' '
                && b[..4].iter().all(u8::is_ascii_digit);
            dated.then(|| (n, e.path()))
        })
        .collect();
    folders.sort_by(|a, b| b.0.cmp(&a.0));
    folders
        .into_iter()
        .take(RECENT)
        .map(|(folder, path)| {
            let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .filter_map(|p| {
                    let stem = p.file_stem()?.to_string_lossy().into_owned();
                    Some((stem, p))
                })
                .collect();
            // the index first, then 1, 2, ... 10 in number order
            files.sort_by_key(|(stem, _)| {
                (
                    stem != "Index",
                    stem.split(' ')
                        .next()
                        .and_then(|n| n.parse::<u32>().ok())
                        .unwrap_or(0),
                )
            });
            RunInfo {
                folder,
                notes: files,
            }
        })
        .collect()
}
