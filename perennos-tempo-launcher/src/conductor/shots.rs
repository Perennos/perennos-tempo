//! The shot list: a band's Stage parts write `SHOT` lines, and the Conductor, acting for the owner,
//! plays them after one yes.
//!
//! ```text
//! SHOT 2 | re-angle-video | in clip=/work/dance.mp4 | pedal camera=orbit_left lens=35mm | takes 3
//! ```
//!
//! The members still get no tools, and they play nothing: they write lines. Here the Conductor
//! reads them, checks each against what Stage has (the workflow, its inputs, every Pedal value,
//! a file only from a folder the owner allowed, a machine that fits), shows the WHOLE list with
//! the machine and the time each would take, and asks y/N once. A shot that cannot play is listed
//! with why and is left out; nothing plays until the owner says yes to the list. Then each shot
//! plays through Stage, with its takes across the Fleet, and the results go three places: the
//! takes file (so the Woodshed and the thumbs can see them), the run's hash-chained log, and a
//! `Shots` note in the run's folder, linked from its Index.

use super::run::link;
use super::runlog::{Ev, RunLog};
use super::spec::{Band, Flow};
use crate::setup::json::J;
use crate::setup::sha256::sha256_hex;
use crate::stage::catalog::Catalog;
use crate::stage::run::{input_value, new_file, play, Outcome, RunOpts, World};
use crate::stage::state::State;
use crate::stage::{estimate, pedals, route};
use crate::{write_atomic, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};

/// More than this in one list and the rest are left out: a list nobody can read is not asked.
pub const MAX_SHOTS: usize = 12;
/// Takes in all, over every shot.
pub const MAX_TAKES: u32 = 36;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Shot {
    pub n: usize,
    pub workflow: String,
    pub inputs: Vec<(String, String)>,
    pub pedals: Vec<(String, String)>,
    pub takes: u32,
    pub on: Option<String>,
    /// The member (handle) whose note it came from: the part its takes are filed under.
    pub by: String,
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(['`', '"', '\''])
}

fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The `SHOT` lines in a member's note. A line that is not in the form is not a shot (it is the
/// member talking), so nothing is guessed.
pub fn parse(text: &str, by: &str) -> Vec<Shot> {
    let mut out = vec![];
    for line in text.lines() {
        let l = line
            .trim()
            .trim_start_matches(['-', '*', '>', ' '])
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')')
            .trim();
        let up = l.to_ascii_uppercase();
        let Some(rest) = up.strip_prefix("SHOT").map(|_| &l[4..]) else {
            continue;
        };
        if rest.chars().next().is_some_and(|c| c.is_alphabetic()) {
            continue;
        }
        let segs: Vec<&str> = rest.split('|').map(str::trim).collect();
        let n = segs[0]
            .trim_matches(|c: char| !c.is_ascii_digit())
            .parse::<usize>()
            .unwrap_or(out.len() + 1);
        let Some(wf) = segs.get(1).map(|s| unquote(s)).filter(|s| is_id(s)) else {
            continue;
        };
        let mut s = Shot {
            n,
            workflow: wf.to_string(),
            takes: 1,
            by: by.to_string(),
            ..Shot::default()
        };
        for seg in &segs[2..] {
            let low = seg.to_ascii_lowercase();
            let word_end = seg.find(char::is_whitespace).unwrap_or(seg.len());
            let (word, tail) = (&low[..word_end], seg[word_end..].trim());
            match word {
                "in" | "input" => {
                    if let Some((k, v)) = tail.split_once('=') {
                        s.inputs
                            .push((k.trim().to_string(), unquote(v).to_string()));
                    }
                }
                "pedal" | "pedals" => {
                    for t in tail.split_whitespace() {
                        if let Some((k, v)) = t.trim_matches(',').split_once('=') {
                            s.pedals.push((k.to_string(), unquote(v).to_string()));
                        }
                    }
                }
                "takes" | "take" => {
                    if let Ok(t) = tail.parse::<u32>() {
                        s.takes = t.clamp(1, 12);
                    }
                }
                "on" if !tail.is_empty() => s.on = Some(unquote(tail).to_string()),
                _ => {}
            }
        }
        out.push(s);
    }
    out
}

/// A shot that can play, and on what.
#[derive(Clone, Debug, PartialEq)]
pub struct Ready {
    pub machine: String,
    pub est: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub shot: Shot,
    pub verdict: Result<Ready, String>,
}

fn judge(
    ctx: &Ctx,
    cat: &Catalog,
    world: &dyn World,
    st: &State,
    hist: &[(String, String, f64)],
    s: &Shot,
) -> Result<Ready, String> {
    let wf = cat
        .find(&s.workflow)
        .ok_or_else(|| format!("Stage has no workflow called {}", s.workflow))?;
    if !st.installed(&s.workflow) {
        return Err(format!(
            "{} is not added here (`tempo-launch stage add {}`)",
            s.workflow, s.workflow
        ));
    }
    for (k, v) in &s.inputs {
        let Some(def) = wf.input(k) else {
            return Err(format!(
                "{} has no input \"{k}\" (it has {})",
                wf.id,
                wf.inputs
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        if v.contains("[CHECK") {
            return Err(format!("needs {k}: the Shooter left {v}"));
        }
        if def.kind != "text" {
            input_value(world, wf, k, v, true)?;
        }
    }
    for i in wf
        .inputs
        .iter()
        .filter(|i| i.required && i.default.is_empty())
    {
        if !s.inputs.iter().any(|(k, _)| *k == i.name) {
            return Err(format!("needs the input \"{}\"", i.name));
        }
    }
    for (k, v) in &s.pedals {
        pedals::check_value(cat, wf, k, v)?;
    }
    let mut boxes = vec![];
    if let Some(l) = world.local() {
        boxes.push(l);
    }
    boxes.extend(world.fleet(&s.workflow));
    if let Some(on) = &s.on {
        boxes.retain(|m| m.name == *on || (m.local && (on == "here" || on == "this computer")));
    }
    let _ = ctx;
    let ranked = route::ranked(&boxes, wf);
    let (m, _) = ranked.first().ok_or_else(|| {
        route::pick(&boxes, wf)
            .err()
            .unwrap_or_else(|| "no machine fits".into())
    })?;
    Ok(Ready {
        machine: if m.local {
            "this computer".into()
        } else {
            format!("the {} box", m.name)
        },
        est: estimate::secs(hist, &s.workflow, &m.name),
    })
}

/// Every shot, checked.
pub fn plan(ctx: &Ctx, cat: &Catalog, world: &dyn World, shots: Vec<Shot>) -> Vec<Line> {
    let st = State::load(ctx);
    let hist = estimate::history(ctx);
    let mut takes_left = MAX_TAKES;
    shots
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let verdict = if i >= MAX_SHOTS {
                Err(format!("past the {MAX_SHOTS} shots one list may hold"))
            } else if s.takes > takes_left {
                Err(format!("past the {MAX_TAKES} takes one list may hold"))
            } else {
                judge(ctx, cat, world, &st, &hist, &s)
            };
            if verdict.is_ok() {
                takes_left -= s.takes;
            }
            Line { shot: s, verdict }
        })
        .collect()
}

fn pedals_words(p: &[(String, String)]) -> String {
    if p.is_empty() {
        "no Pedals".into()
    } else {
        p.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn file_words(s: &Shot) -> String {
    s.inputs
        .iter()
        .map(|(k, v)| {
            let short = if v.contains(['/', '\\']) {
                v.rsplit(['/', '\\']).next().unwrap_or(v)
            } else {
                v.as_str()
            };
            format!("{k}: {}", short.chars().take(40).collect::<String>())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The whole list, as the owner reads it before the y/N.
pub fn list_text(lines: &[Line]) -> String {
    let mut t = String::new();
    for l in lines {
        let s = &l.shot;
        match &l.verdict {
            Ok(r) => t.push_str(&format!(
                "  {}. {}  {}  [{}]  {} take{}, on {}, {} each\n",
                s.n,
                s.workflow,
                pedals_words(&s.pedals),
                file_words(s),
                s.takes,
                if s.takes == 1 { "" } else { "s" },
                r.machine,
                estimate::words(r.est)
            )),
            Err(why) => t.push_str(&format!(
                "  {}. {}  will not play: {why}\n",
                s.n, s.workflow
            )),
        }
    }
    t
}

/// What a shot made, for the notes.
pub struct Played {
    pub shot: Shot,
    pub result: Result<Outcome, String>,
}

fn shots_note(
    band: &Band,
    flow: &Flow,
    date: &str,
    base: &str,
    folder: &str,
    played: &[Played],
) -> String {
    let mut t = format!(
        "---\nband: {}\nflow: {}\nkind: stage-shots\ndate: {date}\ntags: [tempo-band, stage]\n---\n# Stage shots, {date}\n\nPlayed by the Conductor for the owner, after one yes. Flow: {}\n\n",
        band.name,
        flow.name,
        link(base, folder, "Index", &format!("{}, {date}", flow.name)),
    );
    for p in played {
        let s = &p.shot;
        t.push_str(&format!(
            "## Shot {}: {}\n\n- Pedals: {}\n- Inputs: {}\n- Takes: {}\n",
            s.n,
            s.workflow,
            pedals_words(&s.pedals),
            if s.inputs.is_empty() {
                "none".to_string()
            } else {
                file_words(s)
            },
            s.takes
        ));
        match &p.result {
            Ok(o) => {
                t.push_str(&format!("- {}\n", o.status));
                for k in &o.takes {
                    t.push_str(&format!(
                        "- Take {} (seed {}, {}): {}\n",
                        k.take,
                        k.seed,
                        match k.check.ok {
                            Some(true) => "passed the check",
                            Some(false) => "failed the check",
                            None => "not checked",
                        },
                        k.files
                            .iter()
                            .map(|f| format!("`{}`", f.display()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if let Some(w) = o.winner {
                    t.push_str(&format!("- Best: take {}\n", o.takes[w].take));
                }
            }
            Err(e) => t.push_str(&format!("- It did not play: {e}\n")),
        }
        t.push('\n');
    }
    t.push_str("Each file has a `.stage.json` beside it that says how it was made. Your thumbs on a take (`tempo-launch conductor takes accept|reject TAKE`) teach this band which Pedals work.\n");
    t
}

/// The shot lines in a run's notes, each with the member whose note it was in.
pub fn shots_in(notes: &[PathBuf]) -> Vec<Shot> {
    let mut all: Vec<Shot> = vec![];
    for p in notes {
        let by = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .and_then(|s| s.split_once(' ').map(|x| x.1.to_string()))
            .unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        all.extend(parse(&text, &by));
    }
    // numbered in the order they were written, whatever each member called them
    for (i, s) in all.iter_mut().enumerate() {
        s.n = i + 1;
    }
    all
}

/// After a flow: if its notes hold shot lines, show the whole list and ask once. Returns what
/// it did, for the exit code (0 for nothing to play, no, or played).
#[allow(clippy::too_many_arguments)]
pub fn offer(
    ctx: &Ctx,
    band: &Band,
    flow: &Flow,
    run_dir: &Path,
    base: &str,
    notes: &[PathBuf],
    world: &dyn World,
    cat: &Catalog,
    now: u64,
    out: &mut dyn Write,
    ask: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let shots = shots_in(notes);
    if shots.is_empty() {
        return 0;
    }
    let lines = plan(ctx, cat, world, shots);
    let ready: Vec<&Line> = lines.iter().filter(|l| l.verdict.is_ok()).collect();
    let _ = writeln!(
        out,
        "\nThe band wrote {} shot{} for Stage. Here is the whole list. Nothing has played.\n\n{}",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
        list_text(&lines)
    );
    if ready.is_empty() {
        let _ = writeln!(
            out,
            "None of them can play yet. Fix what each line says, or ask the band again."
        );
        return 0;
    }
    let takes: u32 = ready.iter().map(|l| l.shot.takes).sum();
    let known: f64 = ready
        .iter()
        .filter_map(|l| {
            l.verdict
                .as_ref()
                .ok()
                .and_then(|r| r.est)
                .map(|e| e * f64::from(l.shot.takes))
        })
        .sum();
    let unknown = ready
        .iter()
        .filter(|l| l.verdict.as_ref().is_ok_and(|r| r.est.is_none()))
        .count();
    let when = if known > 0.0 {
        format!(
            ", {} of playing{}",
            estimate::words(Some(known)),
            if unknown > 0 {
                format!(" plus {unknown} shot(s) with no estimate yet")
            } else {
                String::new()
            }
        )
    } else {
        ", no estimate yet because none of these has played here".to_string()
    };
    let q = format!(
        "Play the {} shot{} that can play ({takes} take{} in all{when}; takes spread over your Fleet's cards)? [y/N] ",
        ready.len(),
        if ready.len() == 1 { "" } else { "s" },
        if takes == 1 { "" } else { "s" },
    );
    if !ask(&q) {
        let _ = writeln!(
            out,
            "Left. Nothing was played; the shot lines stay in the notes."
        );
        return 0;
    }
    play_list(ctx, band, flow, run_dir, base, lines, world, cat, now, out)
}

#[allow(clippy::too_many_arguments)]
fn play_list(
    ctx: &Ctx,
    band: &Band,
    flow: &Flow,
    run_dir: &Path,
    base: &str,
    lines: Vec<Line>,
    world: &dyn World,
    cat: &Catalog,
    now: u64,
    out: &mut dyn Write,
) -> i32 {
    let folder = run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let run_id = format!(
        "shots-{}",
        &sha256_hex(format!("{}{now}", run_dir.display()).as_bytes())[..10]
    );
    let log = RunLog::create(ctx, &run_id, now).ok();
    let emit = |kind: &str, member: &str, fields: Vec<(&str, J)>| {
        if let Some(l) = &log {
            l.emit(
                now,
                Ev {
                    parent: None,
                    depth: 0,
                    member,
                    engine: "comfyui",
                    model: "stage",
                    kind,
                    fields,
                },
            );
        }
    };
    emit(
        "plan",
        "conductor",
        vec![
            ("band", J::s(&band.name)),
            ("flow", J::s(&flow.name)),
            (
                "shots",
                J::Arr(
                    lines
                        .iter()
                        .map(|l| {
                            J::Obj(vec![
                                ("n".into(), J::n(l.shot.n as f64)),
                                ("workflow".into(), J::s(&l.shot.workflow)),
                                ("pedals".into(), J::s(&pedals_words(&l.shot.pedals))),
                                ("takes".into(), J::n(f64::from(l.shot.takes))),
                                (
                                    "plays".into(),
                                    l.verdict.as_ref().map_or(J::s("no"), |_| J::s("yes")),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
        ],
    );
    let mut played: Vec<Played> = vec![];
    let mut failed = 0;
    for l in lines.into_iter().filter(|l| l.verdict.is_ok()) {
        let s = l.shot;
        let _ = writeln!(out, "Shot {}: {} ...", s.n, s.workflow);
        let opts = RunOpts {
            inputs: s.inputs.clone(),
            pedals: s.pedals.clone(),
            takes: s.takes,
            on: s.on.clone(),
            // the words came from a model: files only from the folders the owner allowed
            guarded: true,
            band: Some(band.name.clone()),
            part: Some(if s.by.is_empty() {
                "stage".into()
            } else {
                s.by.clone()
            }),
            ..RunOpts::default()
        };
        let result = play(ctx, world, cat, &s.workflow, &opts);
        match &result {
            Ok(o) => {
                let _ = writeln!(out, "  {}", o.status);
                for k in &o.takes {
                    for f in &k.files {
                        let _ = writeln!(out, "    take {}: {}", k.take, f.display());
                    }
                }
                emit(
                    "result",
                    &s.by,
                    vec![
                        ("shot", J::n(s.n as f64)),
                        ("workflow", J::s(&s.workflow)),
                        ("ok", J::Bool(true)),
                        ("status", J::s(&o.status)),
                        ("stage_run", J::s(&o.key)),
                        (
                            "files",
                            J::strings(
                                &o.takes
                                    .iter()
                                    .flat_map(|k| k.files.iter())
                                    .map(|f| f.to_string_lossy().into_owned())
                                    .collect::<Vec<_>>(),
                            ),
                        ),
                    ],
                );
            }
            Err(e) => {
                failed += 1;
                let _ = writeln!(out, "  It did not play: {e}");
                emit(
                    "result",
                    &s.by,
                    vec![
                        ("shot", J::n(s.n as f64)),
                        ("workflow", J::s(&s.workflow)),
                        ("ok", J::Bool(false)),
                        ("error", J::s(e)),
                    ],
                );
            }
        }
        played.push(Played { shot: s, result });
    }
    emit(
        "final",
        "conductor",
        vec![
            ("played", J::n(played.len() as f64)),
            ("failed", J::n(failed as f64)),
        ],
    );
    // the note in the run's folder, linked from its Index
    let date = super::runlog::date_of(now);
    let body = shots_note(band, flow, &date, base, &folder, &played);
    match new_file(run_dir, "Shots", "md") {
        Ok((mut f, path)) => {
            let _ = f.write_all(body.as_bytes());
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Shots".into());
            let index = run_dir.join("Index.md");
            if let Ok(mut t) = std::fs::read_to_string(&index) {
                t.push_str(&format!(
                    "\nStage: {}\n",
                    link(base, &folder, &stem, "Stage shots")
                ));
                let _ = write_atomic(&index, t.as_bytes());
            }
            let _ = writeln!(out, "Results are in {}", path.display());
        }
        Err(e) => {
            let _ = writeln!(out, "Could not write the shots note: {e}");
        }
    }
    if let Some(l) = &log {
        let _ = writeln!(out, "The run log is {}", l.path.display());
    }
    if failed == played.len() {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn a_shot_line_is_read_in_its_form_and_talk_is_not_a_shot() {
        let text = "Here is the plan.\n\nSHOT 1 | re-angle-video | in clip=/work/dance.mp4 | pedal camera=orbit_left lens=35mm | takes 3\n- SHOT 2 | `relight-photo` | in image=\"/work/a b.png\" | pedal light=golden_hour\n3. shot 3 | camera-photo | in image=/w/c.png | pedal camera=push_in speed=0.5 | takes 2 | on studio\nThe shots are great.\nSHOTS are fun\nSHOT | not an id!! | takes 2\nSHOT 9 | effect-clip\n";
        let s = parse(text, "shooter");
        assert_eq!(s.len(), 4, "{s:?}");
        assert_eq!(s[0].workflow, "re-angle-video");
        assert_eq!(
            s[0].inputs,
            [("clip".to_string(), "/work/dance.mp4".to_string())]
        );
        assert_eq!(
            s[0].pedals,
            [
                ("camera".into(), "orbit_left".into()),
                ("lens".into(), "35mm".into())
            ]
        );
        assert_eq!((s[0].takes, s[0].by.as_str()), (3, "shooter"));
        assert_eq!(
            s[1].inputs[0].1, "/work/a b.png",
            "spaces inside quotes stay"
        );
        assert_eq!(s[1].takes, 1);
        assert_eq!((s[2].takes, s[2].on.as_deref()), (2, Some("studio")));
        assert_eq!(s[3].workflow, "effect-clip");
        assert!(s[3].inputs.is_empty() && s[3].pedals.is_empty());
        // takes are held to 1..=12
        assert_eq!(parse("SHOT 1 | effect-clip | takes 99", "x")[0].takes, 12);
        assert_eq!(parse("SHOT 1 | effect-clip | takes 0", "x")[0].takes, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::templates;
    use crate::conductor::{run::places, Models};
    use crate::stage::state::State as StageState;
    use crate::stage::tests::{pics, ready, world, FakeComfy, FakeWorld};

    fn models() -> Models {
        Models {
            heavy: "m".into(),
            fast: "m".into(),
        }
    }

    /// A band (the product ad), a run folder with the shooter's note, and Stage ready.
    struct Rig {
        _dir: crate::woodshed::takes::TestDir,
        ctx: Ctx,
        cat: Catalog,
        band: Band,
        dir: PathBuf,
        notes: Vec<PathBuf>,
    }

    fn rig(tag: &str, shooter: &str) -> Rig {
        let (ctx, cat, _fixture) = ready(tag);
        let band = templates::find("product-ad")
            .unwrap()
            .band("product-ad", &models());
        let flow = band.flows[0].clone();
        let dir = places(&ctx, &band, &flow).root.join("2026-11-07 ad");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Index.md"), "# index\n").unwrap();
        std::fs::write(
            dir.join("1 director.md"),
            "A plan in words. SHOTs are shots.\n",
        )
        .unwrap();
        std::fs::write(dir.join("2 shooter.md"), shooter).unwrap();
        let notes = vec![dir.join("1 director.md"), dir.join("2 shooter.md")];
        Rig {
            _dir: _fixture,
            ctx,
            cat,
            band,
            dir,
            notes,
        }
    }

    fn offered(r: &Rig, w: &FakeWorld, yes: bool) -> (i32, String, Vec<String>) {
        let mut out = Vec::new();
        let asked = std::cell::RefCell::new(vec![]);
        let code = offer(
            &r.ctx,
            &r.band,
            &r.band.flows[0],
            &r.dir,
            "",
            &r.notes,
            w,
            &r.cat,
            1_793_000_000,
            &mut out,
            &mut |q| {
                asked.borrow_mut().push(q.to_string());
                yes
            },
        );
        (code, String::from_utf8(out).unwrap(), asked.into_inner())
    }

    fn shot_text(d: &Path) -> String {
        let (a, b) = pics(d);
        format!(
            "Shots:\nSHOT 1 | paint-edit-photo | in image={} | in mask={} | in instruction=a red jacket | pedal strength=20 | takes 2\nSHOT 2 | paint-edit-photo | in image=[CHECK: which photo] | in mask={} | in instruction=a hat\nSHOT 3 | no-such-workflow | takes 1\nSHOT 4 | paint-edit-photo | in image={} | in mask={} | in instruction=a scarf | pedal strength=500\nSHOT 5 | paint-edit-photo | in image={} | in mask={} | in instruction=a coat | pedal strength=30\n",
            a.display(), b.display(), b.display(), a.display(), b.display(), a.display(), b.display()
        )
    }

    #[test]
    fn the_whole_list_is_shown_with_what_cannot_play_and_why_then_asked_once() {
        let (_c, _, d) = ready("shots-list-seed");
        let r = rig("shots-list", &shot_text(&d));
        let fake = FakeComfy::start(24.0);
        let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
        let (code, out, asked) = offered(&r, &w, false);
        assert_eq!(code, 0, "{out}");
        assert_eq!(asked.len(), 1, "one question for the whole list: {asked:?}");
        assert!(
            asked[0].contains("[y/N]")
                && asked[0].contains("2 shots")
                && asked[0].contains("3 takes"),
            "{asked:?}"
        );
        // every shot is on the list, in order, with what it is
        assert!(
            out.contains("The band wrote 5 shots for Stage") && out.contains("Nothing has played"),
            "{out}"
        );
        assert!(
            out.contains("1. paint-edit-photo  strength=20")
                && out.contains("2 takes, on this computer"),
            "{out}"
        );
        assert!(out.contains("no estimate yet"), "{out}");
        // and the ones that cannot play say why
        assert!(out.contains("2. paint-edit-photo  will not play: needs image: the Shooter left [CHECK: which photo]"), "{out}");
        assert!(
            out.contains(
                "3. no-such-workflow  will not play: Stage has no workflow called no-such-workflow"
            ),
            "{out}"
        );
        assert!(
            out.contains("4. paint-edit-photo  will not play:") && out.contains("strength"),
            "{out}"
        );
        // no: nothing played, nothing written
        assert!(out.contains("Left. Nothing was played"), "{out}");
        assert!(fake.st.lock().unwrap().prompts.is_empty());
        assert!(!r.dir.join("Shots.md").exists());
        assert!(!crate::conductor::takes_file::path(&r.ctx).exists());
    }

    #[test]
    fn on_one_yes_the_conductor_plays_the_shots_and_files_them_three_places() {
        let (_c, _, d) = ready("shots-play-seed");
        let r = rig("shots-play", &shot_text(&d));
        let fake = FakeComfy::start(24.0);
        let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
        let (code, out, asked) = offered(&r, &w, true);
        assert_eq!(code, 0, "{out}");
        assert_eq!(asked.len(), 1, "one yes covers the list");
        // shot 1 has 2 takes and shot 5 has 1: three in all, and only the shots that could play
        assert_eq!(fake.st.lock().unwrap().prompts.len(), 3, "{out}");
        assert!(
            out.contains("Shot 1: paint-edit-photo") && out.contains("Shot 5: paint-edit-photo"),
            "{out}"
        );
        assert!(
            !out.contains("Shot 2: ") && !out.contains("Shot 3: ") && !out.contains("Shot 4: "),
            "{out}"
        );
        // 1. the takes file: filed under the band and the shooter's part, winners picked per shot
        let lines =
            crate::conductor::takes_file::read_all(&crate::conductor::takes_file::path(&r.ctx));
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.band.as_deref() == Some("product-ad")
            && l.part.as_deref() == Some("shooter")
            && l.engine == "comfyui"));
        assert_eq!(
            lines.iter().filter(|l| l.winner).count(),
            2,
            "one winner per shot"
        );
        // 2. the run's log: a plan, a result per shot, a final, and an unbroken chain
        let runs = crate::conductor::runlog::runs_dir(&r.ctx);
        let log = std::fs::read_dir(&runs)
            .unwrap()
            .flatten()
            .flat_map(|day| std::fs::read_dir(day.path()).unwrap().flatten())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("shots-")
            })
            .expect("a run log");
        let ev = crate::conductor::runlog::read(&log);
        let kinds: Vec<&str> = ev.iter().map(|e| e.str("kind").unwrap()).collect();
        assert_eq!(kinds, ["plan", "result", "result", "final"], "{kinds:?}");
        assert!(crate::conductor::runlog::verify(&log).is_ok());
        // 3. the note in the run's folder, linked from its Index
        let shots = std::fs::read_to_string(r.dir.join("Shots.md")).unwrap();
        assert!(
            shots.contains("## Shot 1: paint-edit-photo")
                && shots.contains("Take 2")
                && shots.contains("kind: stage-shots"),
            "{shots}"
        );
        assert!(
            std::fs::read_to_string(r.dir.join("Index.md"))
                .unwrap()
                .contains("[[2026-11-07 ad/Shots|Stage shots]]")
                || std::fs::read_to_string(r.dir.join("Index.md"))
                    .unwrap()
                    .contains("Shots|Stage shots")
        );
        // each render has its proof beside it
        let first = &lines[0].answer_ref;
        assert!(std::path::Path::new(&format!("{first}.stage.json")).is_file());
        // and Stage remembers the last shot, so "make it golden hour" has something to change
        assert_eq!(
            StageState::load(&r.ctx).last_workflow().as_deref(),
            Some("paint-edit-photo")
        );
        // a thumb on a take teaches the band which Pedals worked
        let take = lines
            .iter()
            .find(|l| l.winner && l.task.contains("a red jacket"))
            .unwrap();
        let mut o = Vec::new();
        assert_eq!(
            crate::conductor::runs::verdict_cmd(&r.ctx, &take.id, true, &mut o),
            0
        );
        let said = String::from_utf8(o).unwrap();
        assert!(said.contains("Noted in the product-ad band's Lessons: For paint-edit-photo, strength=20 gave a take the owner kept"), "{said}");
        let all = crate::woodshed::lessons::load(&r.ctx, "product-ad");
        assert_eq!(all.len(), 1);
        assert!(all[0].text.contains("strength=20") && all[0].source == "owner");
        // the same thumb again is not counted twice
        let mut o = Vec::new();
        crate::conductor::runs::verdict_cmd(&r.ctx, &take.id, true, &mut o);
        assert_eq!(
            crate::woodshed::lessons::load(&r.ctx, "product-ad")[0].helped,
            0
        );
        // a thumbs down on a take made the same way says the opposite and counts against the first
        let other = lines
            .iter()
            .find(|l| l.id != take.id && l.task.contains("a red jacket"))
            .unwrap();
        let mut o = Vec::new();
        crate::conductor::runs::verdict_cmd(&r.ctx, &other.id, false, &mut o);
        let all = crate::woodshed::lessons::load(&r.ctx, "product-ad");
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all.iter().any(|l| l.text.contains("turned down: avoid it")));
        assert_eq!(
            all.iter()
                .find(|l| l.text.contains("kept: use it"))
                .unwrap()
                .hurt,
            1
        );
    }

    #[test]
    fn a_file_outside_the_folders_the_owner_allowed_is_listed_as_not_playing() {
        let (_c, _, d) = ready("shots-roots-seed");
        let r = rig("shots-roots", &shot_text(&d));
        let fake = FakeComfy::start(24.0);
        let mut w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
        // only the workbench is allowed: the files the band named are somewhere else
        w.roots = Some(vec![crate::stage::run::workbench(&r.ctx)]);
        let (_, out, asked) = offered(&r, &w, true);
        assert!(
            asked.is_empty(),
            "nothing can play, so nothing is asked: {out}"
        );
        assert!(
            out.contains("is not in a folder you allowed")
                && out.contains("None of them can play yet"),
            "{out}"
        );
        assert!(fake.st.lock().unwrap().prompts.is_empty());
    }

    #[test]
    fn no_shot_lines_means_no_list_and_no_question() {
        let r = rig("shots-none", "Just words, no shots.\n");
        let fake = FakeComfy::start(24.0);
        let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
        let (code, out, asked) = offered(&r, &w, true);
        assert_eq!((code, out.as_str(), asked.len()), (0, "", 0));
    }

    #[test]
    fn a_list_is_capped_in_shots_and_in_takes() {
        let (_c, _, d) = ready("shots-cap-seed");
        let (a, b) = pics(&d);
        let mut text = String::new();
        for i in 0..14 {
            text.push_str(&format!(
                "SHOT {i} | paint-edit-photo | in image={} | in mask={} | in instruction=x | takes 12\n",
                a.display(),
                b.display()
            ));
        }
        let r = rig("shots-cap", &text);
        let fake = FakeComfy::start(24.0);
        let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
        let lines = plan(&r.ctx, &r.cat, &w, shots_in(&r.notes));
        let ok = lines.iter().filter(|l| l.verdict.is_ok()).count();
        // 36 takes in all: three shots of 12
        assert_eq!(ok, 3);
        assert!(lines[3].verdict.as_ref().unwrap_err().contains("36 takes"));
        assert!(
            lines[13].verdict.as_ref().unwrap_err().contains("takes")
                || lines[13].verdict.as_ref().unwrap_err().contains("12 shots")
        );
    }
    /// A chat where the shooter answers with a shot list and everyone else with words.
    struct ShooterChat;

    impl crate::conductor::Chat for ShooterChat {
        fn say(&self, _model: &str, role: &str, _text: &str) -> Result<String, String> {
            Ok(if role.contains("You are the shooter") {
                "Here are the shots.\nSHOT 1 | re-angle-video | in video=/media/dance.mp4 | pedal camera=orbit_left | takes 2\n".into()
            } else {
                "Words.".into()
            })
        }
    }

    #[test]
    fn conductor_run_ends_with_the_shot_list_and_one_question_and_the_members_play_nothing() {
        use crate::conductor::tests::{ctx_in, tmp, FakeOutside, RecSched};
        use crate::conductor::{dispatch, parse_args, Deps};
        let root = tmp("shots-run");
        let ctx = ctx_in(&root, false);
        let mut st = StageState::load(&ctx);
        st.set(
            "re-angle-video",
            vec![("installed", crate::setup::json::J::Bool(true))],
        );
        st.save(&ctx).unwrap();
        let media = root.join("media");
        std::fs::create_dir_all(&media).unwrap();
        // the file the shooter named has to exist where it said
        let fake = FakeComfy::start(24.0);
        let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
        let sched = RecSched::default();
        let outside = FakeOutside::default();
        let d = Deps {
            chat: &ShooterChat,
            stage: Some(&w),
            outside: &outside,
            room_dir: root.join("room"),
            scheduler: &sched,
            models: models(),
            exe: PathBuf::from("/opt/tempo/tempo-launch"),
            now: 1_793_000_000,
        };
        let mut out = Vec::new();
        dispatch(
            &ctx,
            parse_args(&["new", "--template", "product-ad", "--yes"]).unwrap(),
            &d,
            &mut out,
            &mut |_| true,
        );
        // the file is not there yet: the list says so and nothing is asked
        let mut out = Vec::new();
        let mut asked = vec![];
        let code = dispatch(
            &ctx,
            parse_args(&["run", "product-ad"]).unwrap(),
            &d,
            &mut out,
            &mut |q| {
                asked.push(q.to_string());
                false
            },
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("The band wrote 1 shot for Stage")
                && text.contains("will not play")
                && text.contains("dance.mp4"),
            "{text}"
        );
        assert!(asked.is_empty(), "{asked:?}");
        // with the file there, the same run asks once, and "n" plays nothing
        std::fs::write(media.join("dance.mp4"), vec![0u8; 3000]).unwrap();
        let chat_path = media.join("dance.mp4").to_string_lossy().into_owned();
        struct Path1(String);
        impl crate::conductor::Chat for Path1 {
            fn say(&self, _m: &str, role: &str, _t: &str) -> Result<String, String> {
                Ok(if role.contains("You are the shooter") {
                    format!("SHOT 1 | re-angle-video | in video={} | pedal camera=orbit_left | takes 2\n", self.0)
                } else {
                    "Words.".into()
                })
            }
        }
        let c2 = Path1(chat_path);
        let d2 = Deps { chat: &c2, ..d };
        let mut out = Vec::new();
        let mut asked = vec![];
        dispatch(
            &ctx,
            parse_args(&["run", "product-ad"]).unwrap(),
            &d2,
            &mut out,
            &mut |q| {
                asked.push(q.to_string());
                false
            },
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(asked.len(), 1, "{text}");
        assert!(
            asked[0].contains("1 shot")
                && asked[0].contains("2 takes")
                && asked[0].contains("[y/N]"),
            "{asked:?}"
        );
        assert!(text.contains("1. re-angle-video  camera=orbit_left  [video: dance.mp4]  2 takes, on this computer"), "{text}");
        assert!(fake.st.lock().unwrap().prompts.is_empty(), "no was no");
        // and `conductor shots` offers the newest run's list again later
        let mut out = Vec::new();
        let mut asked = vec![];
        dispatch(
            &ctx,
            parse_args(&["shots", "product-ad"]).unwrap(),
            &d2,
            &mut out,
            &mut |q| {
                asked.push(q.to_string());
                false
            },
        );
        assert_eq!(asked.len(), 1, "{}", String::from_utf8_lossy(&out));
    }
}
