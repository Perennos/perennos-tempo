//! `tempo-launch stage cue "re-angle the last clip with a slow orbit"`: the words of the owner,
//! turned into a run of the last thing that ran, with the Pedals those words name (see
//! `phrase.rs`, the one parser Room and Voice share). It says what it understood, which machine
//! will play it and about how long it takes, and asks y/N once. `--yes` is for a caller that has
//! asked already (Room asks the owner in the room, then calls this with `--yes`). A phrase
//! cannot carry a file: the inputs are the last run's, so with no earlier run it says how to
//! make the first one.

use super::catalog::{Catalog, Workflow};
use super::mcp::report;
use super::pedals;
use super::phrase::{self, Cue};
use super::route;
use super::run::{play, RunOpts, World};
use super::state::State;
use crate::setup::json::J;
use crate::Ctx;
use std::io::Write;

#[derive(Debug, Default, PartialEq, Clone)]
pub struct CueOpts {
    /// Say what would play and stop.
    pub plan: bool,
    /// One line of JSON instead of words (for Room).
    pub json: bool,
    /// The owner has been asked already.
    pub yes: bool,
}

pub struct Plan {
    pub id: String,
    pub title: String,
    pub pedals: Vec<(String, String)>,
    /// Pedals the words named that this workflow does not have, said so and left out.
    pub left_out: Vec<String>,
    pub machine: String,
    pub est: Option<f64>,
    /// The last run's inputs it will use.
    pub inputs: Vec<(String, String)>,
}

impl Plan {
    /// What the owner reads before they say y or N.
    pub fn text(&self) -> String {
        let p = if self.pedals.is_empty() {
            "the same Pedals".to_string()
        } else {
            self.pedals
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let ins = self
            .inputs
            .iter()
            .map(|(k, v)| {
                format!(
                    "{k}: {}",
                    v.rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(v)
                        .chars()
                        .take(60)
                        .collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let mut t = format!(
            "{} ({}) with {p}, on the last run's inputs ({ins}). It plays on {}, {}.",
            self.title,
            self.id,
            self.machine,
            super::estimate::words(self.est)
        );
        for l in &self.left_out {
            t.push_str(&format!(" {l}"));
        }
        t
    }
}

/// Who would play it, in words, or why nobody can.
fn who_plays(boxes: &[route::Machine], wf: &Workflow) -> Result<(String, String), String> {
    let (m, _) = route::pick(boxes, wf)?;
    let words = if m.local {
        "this computer".to_string()
    } else {
        format!("the {} box", m.name)
    };
    Ok((words, m.name.clone()))
}

/// The words, checked against the catalogue and the last run.
pub fn plan(
    ctx: &Ctx,
    world: &dyn World,
    cat: &Catalog,
    phrase_text: &str,
) -> Result<(Plan, Cue), String> {
    let cue: Cue = phrase::parse(phrase_text, true).ok_or_else(|| {
        "I did not catch a Pedal or a workflow in that. Try \"golden hour\", \"slow orbit\", \"push in, 35mm\" or \"re-angle the last clip\".".to_string()
    })?;
    let st = State::load(ctx);
    let id = cue
        .workflow
        .clone()
        .or_else(|| st.last_workflow())
        .ok_or("Nothing has played yet, so there is nothing to change. Play one first: `tempo-launch stage run ID --input NAME=PATH`.")?;
    let wf = cat
        .find(&id)
        .ok_or_else(|| format!("There is no \"{id}\" in Stage."))?;
    if !st.installed(&id) {
        return Err(format!(
            "{id} is not added yet: `tempo-launch stage add {id}`."
        ));
    }
    let last = st.last_run(&id).ok_or_else(|| {
        format!("There is no earlier run of {id} to change. Play it once with `tempo-launch stage run {id} --input NAME=PATH` (a phrase cannot carry a file), then say it again.")
    })?;
    let mut pedal_list = vec![];
    let mut left_out = vec![];
    for (k, v) in &cue.pedals {
        if wf.pedal(k).is_none() {
            left_out.push(format!(
                "{} has no {k} Pedal, so {k}={v} was left out.",
                wf.title
            ));
            continue;
        }
        match pedals::check_value(cat, wf, k, v) {
            Ok(val) => pedal_list.push((k.clone(), val)),
            Err(e) => left_out.push(format!("{e} It was left out.")),
        }
    }
    if pedal_list.is_empty() && cue.workflow.is_none() {
        return Err(left_out.join(" "));
    }
    let mut boxes = vec![];
    if let Some(l) = world.local() {
        boxes.push(l);
    }
    boxes.extend(world.fleet(&id));
    let (machine, on) = who_plays(&boxes, wf)?;
    let hist = super::estimate::history(ctx);
    let inputs = last
        .arr("inputs")
        .iter()
        .filter_map(|p| Some((p.str("name")?.to_string(), p.str("value")?.to_string())))
        .collect();
    Ok((
        Plan {
            id: id.clone(),
            title: wf.title.clone(),
            pedals: pedal_list,
            left_out,
            machine,
            est: super::estimate::secs(&hist, &id, &on),
            inputs,
        },
        cue,
    ))
}

fn say(out: &mut dyn Write, o: &CueOpts, ok: bool, text: &str, files: &[String]) {
    if o.json {
        let j = J::Obj(vec![
            ("ok".into(), J::Bool(ok)),
            ("text".into(), J::s(text)),
            ("files".into(), J::strings(files)),
        ]);
        let _ = writeln!(out, "{}", j.compact());
    } else {
        let _ = writeln!(out, "{text}");
    }
}

pub fn run(
    ctx: &Ctx,
    world: &dyn World,
    cat: &Catalog,
    phrase_text: &str,
    o: &CueOpts,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> String,
) -> i32 {
    let (p, _) = match plan(ctx, world, cat, phrase_text) {
        Ok(x) => x,
        Err(e) => {
            say(out, o, false, &e, &[]);
            return 1;
        }
    };
    if o.plan {
        say(out, o, true, &p.text(), &[]);
        return 0;
    }
    if !o.yes {
        let _ = writeln!(out, "{}", p.text());
        let a = ask("Play it? [y/N] ");
        if !matches!(a.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            let _ = writeln!(out, "Not played.");
            return 0;
        }
    }
    let opts = RunOpts {
        pedals: p.pedals.clone(),
        takes: 1,
        again: true,
        ..RunOpts::default()
    };
    match play(ctx, world, cat, &p.id, &opts) {
        Ok(r) => {
            let files: Vec<String> = r
                .winner
                .and_then(|w| r.takes.get(w))
                .or(r.takes.first())
                .map(|t| {
                    t.files
                        .iter()
                        .map(|f| f.to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            let text = if o.json {
                format!(
                    "{} Result: {}",
                    r.status,
                    files.first().cloned().unwrap_or_default()
                )
            } else {
                report(&r)
            };
            say(out, o, true, &text, &files);
            0
        }
        Err(e) => {
            say(out, o, false, &e, &[]);
            1
        }
    }
}
