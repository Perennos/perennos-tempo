//! Band proposals, for the Conductor: the owner's own AI (through `tempo-launch mcp`) can design a band with them and
//! write it down as a proposal. It cannot set anything up: the owner applies it with
//! `tempo-launch conductor apply-proposal`, which shows the band in words and asks y/N.
//!
//! A proposal is a file in `<Tempo home>/bands/proposals/<band>.json`. It is read back with the
//! same strict checks as any band.json, because an AI wrote it.

use super::spec::{self, from_j, Band, MAX_ROLE};
use super::{templates, Deps, Done, Models};
use crate::setup::json::{parse, J};
use crate::setup::propose::clean_note;
use crate::{file_stamp, stamp, write_atomic, Ctx};
use std::io::Write;
use std::path::PathBuf;

pub const APPLY_COMMAND: &str = "tempo-launch conductor apply-proposal";

pub fn proposals_dir(ctx: &Ctx) -> PathBuf {
    super::bands_dir(ctx).join("proposals")
}

/// Whether any proposal is waiting.
pub fn waiting(ctx: &Ctx) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(proposals_dir(ctx))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let stem = n.strip_suffix(".json")?;
            spec::slug_ok(stem, 40).then(|| stem.to_string())
        })
        .collect();
    names.sort();
    names
}

/// Write a proposal. The AI may leave models out, or write `fast` or `heavy`; the launcher picks
/// real ones from this computer's plan when the owner applies it. Every problem is returned in
/// words, so the AI can fix its draft.
pub fn write(ctx: &Ctx, band: &J, note: &str, now: u64) -> Result<String, String> {
    let b = from_j(&spec::without_engines(band.clone())).map_err(|p| {
        format!(
            "The band has problems; fix them and propose again:\n- {}",
            p.join("\n- ")
        )
    })?;
    let doc = J::Obj(vec![
        (
            "proposed_by".into(),
            J::s("an AI, through tempo-launch mcp"),
        ),
        ("proposed_at".into(), J::s(&stamp(now))),
        ("note".into(), J::s(&clean_note(note))),
        ("band".into(), b.to_j()),
    ]);
    let file = proposals_dir(ctx).join(format!("{}.json", b.name));
    write_atomic(&file, doc.pretty().as_bytes())
        .map_err(|e| format!("could not write the proposal: {e}"))?;
    Ok(b.name)
}

/// A proposal read back: the band and the AI's note.
fn read(ctx: &Ctx, name: &str) -> Result<(Band, String), String> {
    let file = proposals_dir(ctx).join(format!("{name}.json"));
    let text = std::fs::read_to_string(&file).map_err(|_| format!("no proposal called {name}"))?;
    let doc = parse(&text).map_err(|e| format!("the proposal is not valid JSON ({e})"))?;
    let Some(b) = doc.get("band") else {
        return Err("the proposal holds no band".into());
    };
    let band = from_j(&spec::without_engines(b.clone())).map_err(|p| {
        format!(
            "the proposal has problems, so it was left alone:\n- {}",
            p.join("\n- ")
        )
    })?;
    if band.name != name {
        return Err(format!(
            "the proposal file is named {name} but the band is {}",
            band.name
        ));
    }
    Ok((band, doc.str("note").unwrap_or("").to_string()))
}

/// Models for what the AI left open: `fast` and `heavy` become this computer's models; an empty
/// one becomes the thinking model.
fn settle_models(band: &mut Band, models: &Models) {
    models.resolve(band);
    for m in &mut band.members {
        if m.model.is_empty() {
            m.model = models.heavy.clone();
        }
    }
}

/// `tempo-launch conductor apply-proposal [NAME]`.
pub fn apply_proposal(
    ctx: &Ctx,
    name: Option<&str>,
    d: &Deps,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let waiting = waiting(ctx);
    let name = match (name, waiting.as_slice()) {
        (Some(n), _) => n.to_string(),
        (None, []) => {
            let _ = writeln!(out, "No band proposal is waiting. (Your AI writes one with the band_propose tool of tempo-launch mcp.)");
            return 1;
        }
        (None, [one]) => one.clone(),
        (None, many) => {
            let _ = writeln!(
                out,
                "Several band proposals are waiting: {}. Say which: {APPLY_COMMAND} NAME",
                many.join(", ")
            );
            return 2;
        }
    };
    let (mut band, note) = match read(ctx, &name) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            return 1;
        }
    };
    settle_models(&mut band, &d.models);
    if !note.is_empty() {
        let _ = writeln!(
            out,
            "Your AI says: {note}\n(That note is the AI's, not Tempo's.)"
        );
    }
    match super::set_up(ctx, &band, d, false, out, ask) {
        Done::Applied => {
            let dir = proposals_dir(ctx).join("applied");
            let moved = std::fs::create_dir_all(&dir).and_then(|()| {
                std::fs::rename(
                    proposals_dir(ctx).join(format!("{name}.json")),
                    dir.join(format!("{name}-{}.json", file_stamp(d.now))),
                )
            });
            if moved.is_err() {
                let _ = writeln!(
                    out,
                    "(The proposal file could not be moved aside; it is still in {}.)",
                    proposals_dir(ctx).display()
                );
            }
            0
        }
        Done::Declined => 0,
        Done::Failed => 1,
    }
}

// ---- what the MCP tools answer

fn o(pairs: Vec<(&str, J)>) -> J {
    J::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// The ready-made bands and the rules a band follows, for an AI that designs one with the owner.
pub fn templates_facts() -> J {
    let list = templates::all()
        .iter()
        .map(|t| {
            let members = t
                .members
                .iter()
                .map(|m| {
                    o(vec![
                        ("handle", J::s(m.handle)),
                        ("name", J::s(m.name)),
                        ("instrument", J::s(m.instrument)),
                        ("may", J::s(m.may.text())),
                        ("model", J::s(if m.heavy { "heavy" } else { "fast" })),
                        ("role", J::s(m.role)),
                    ])
                })
                .collect();
            let flows = t
                .flows
                .iter()
                .map(|f| {
                    o(vec![
                        ("name", J::s(f.name)),
                        ("goal", J::s(f.goal)),
                        (
                            "steps",
                            J::strings(&f.steps.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
                        ),
                        ("every", J::s(f.every)),
                    ])
                })
                .collect();
            o(vec![
                ("id", J::s(t.id)),
                ("summary", J::s(t.summary)),
                ("members", J::Arr(members)),
                ("flows", J::Arr(flows)),
            ])
        })
        .collect();
    o(vec![
        (
            "what_a_band_is",
            J::s("A small team of AI members for the owner's own workflow, set up and led by the Tempo Conductor. Each member is a teammate in Room that the owner can @mention, and flows walk the members one after another and file their drafts as linked notes in the Vault. When the owner sets a band up, the Conductor picks which of the owner's engines plays each part (heavy parts on the strongest engine that has their model, light ones on a quick one), so do not name engines. Members have no tools: they never post, send, buy or delete anything outside the computer."),
        ),
        (
            "rules",
            J::Arr(
                [
                    "name: 2 to 40 lowercase letters, digits or hyphens.",
                    "about: one line, at most 200 characters.",
                    "2 to 8 members; each has handle (2 to 24 lowercase letters, digits, hyphens), name, instrument (one word, like Metronome), role (an instruction, at most 2000 characters), may (\"talk\" or \"draft\") and optionally model (\"fast\", \"heavy\" or leave it out: the owner's computer picks).",
                    "1 to 4 flows; each has name (lowercase, hyphens), goal (at most 600 characters), steps (member handles in order, 1 to 8, only members who may \"draft\"), every (\"manual\", \"daily HH:MM\" or \"weekly <mon..sun> HH:MM\") and optionally notes_to (Bands/<name>/ or a folder inside it).",
                    "Roles should be specific to the work and say what the member must not invent.",
                ]
                .iter()
                .map(|s| J::s(s))
                .collect(),
            ),
        ),
        ("max_role_characters", J::n(MAX_ROLE as f64)),
        ("templates", J::Arr(list)),
        (
            "the_owner_applies_a_proposal_with",
            J::s(APPLY_COMMAND),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_facts_hold_every_template_and_the_rules() {
        let j = templates_facts();
        assert_eq!(j.arr("templates").len(), 11);
        let first = &j.arr("templates")[0];
        assert_eq!(first.str("id"), Some("social-media"));
        assert!(first.arr("members")[0].str("role").unwrap().len() > 100);
        assert!(j.arr("rules").len() >= 4);
        assert!(j.compact().contains("never post, send, buy or delete"));
    }
}
