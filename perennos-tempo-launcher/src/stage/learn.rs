//! Lessons from your thumbs. When you accept or reject a take that a band played on Stage, the
//! band's Lessons (the short notes its members read, `tempo-launch woodshed lessons`) get one
//! bullet that says which Pedals gave a take you kept or turned down. The same Pedals judged the
//! same way again make the bullet stronger instead of making a second one, and a thumb that
//! says the opposite counts against the bullet that said otherwise, so a note that stops being
//! true retires itself. The Pedals come from the `.stage.json` beside the file.

use crate::conductor::takes_file::Take;
use crate::setup::json::parse;
use crate::woodshed::lessons;
use crate::Ctx;
use std::path::Path;

/// The workflow and Pedals a render was made with, from its proof. The seed is luck, not a choice.
pub fn made_with(file: &str) -> Option<(String, Vec<(String, String)>)> {
    let side = Path::new(file).with_file_name(format!(
        "{}.stage.json",
        Path::new(file).file_name()?.to_string_lossy()
    ));
    let j = parse(&std::fs::read_to_string(side).ok()?).ok()?;
    let wf = j.get("workflow")?.str("id")?.to_string();
    let pedals: Vec<(String, String)> = match j.get("pedals") {
        Some(crate::setup::json::J::Obj(o)) => o
            .iter()
            .filter(|(k, _)| k != "seed")
            .filter_map(|(k, v)| match v {
                crate::setup::json::J::Str(s) => Some((k.clone(), s.clone())),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };
    (!pedals.is_empty()).then_some((wf, pedals))
}

fn bullets(wf: &str, pedals: &[(String, String)]) -> (String, String) {
    let p = pedals
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    (
        format!("For {wf}, {p} gave a take the owner kept: use it."),
        format!("For {wf}, {p} gave a take the owner turned down: avoid it."),
    )
}

/// A bullet that says the same thing again counts for it; one that is new is added. The one that
/// said the opposite is counted against.
fn say(ctx: &Ctx, key: &str, text: &str, against: &str, now: u64) -> Result<bool, String> {
    let id = lessons::id_of(&lessons::clean(text)?);
    let fresh = match lessons::add(ctx, key, text, "owner", now) {
        Ok(_) => true,
        Err(_) => {
            // already there: it helped again (a retired one stays retired: restore is the owner's call)
            if lessons::load(ctx, key)
                .iter()
                .any(|l| l.id == id && !l.retired)
            {
                lessons::bump(ctx, key, &id, true, now)?;
            }
            false
        }
    };
    if let Ok(c) = lessons::clean(against) {
        let cid = lessons::id_of(&c);
        if lessons::load(ctx, key)
            .iter()
            .any(|l| l.id == cid && !l.retired)
        {
            lessons::bump(ctx, key, &cid, false, now)?;
        }
    }
    let _ = lessons::render(ctx, key, lessons::DEFAULT_TOKENS);
    Ok(fresh)
}

/// The owner's thumb on `take` (accept or reject). `before`: what they had said about it. What
/// to tell them, when a Lessons bullet was written.
pub fn note_verdict(
    ctx: &Ctx,
    take: &Take,
    before: Option<bool>,
    accept: bool,
    now: u64,
) -> Option<String> {
    let band = take.band.as_deref().filter(|b| lessons::key_ok(b))?;
    if before == Some(accept) {
        return None;
    }
    let (wf, pedals) = made_with(&take.answer_ref)?;
    let (kept, turned) = bullets(&wf, &pedals);
    let (text, opposite) = if accept {
        (kept, turned)
    } else {
        (turned, kept)
    };
    match say(ctx, band, &text, &opposite, now) {
        Ok(_) => Some(format!("Noted in the {band} band's Lessons: {text}")),
        Err(_) => None,
    }
}
