//! The commands around sectioned runs: `conductor sections` (play one), `conductor runs` (what
//! ran lately), `conductor stop` (end one) and `conductor takes accept|reject` (the owner's say).

use super::runlog::{self, RunLog};
use super::sections::{Outcome, Report, Tree};
use super::settings::Settings;
use super::spec::{Band, Flow};
use super::takes_file::{self, Take};
use super::{engines, run, Deps};
use crate::setup::sha256::sha256_hex;
use crate::{write_atomic, Ctx};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn run_id(now: u64, band: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        "r-{}",
        &sha256_hex(format!("{now}{nanos}{}{band}", std::process::id()).as_bytes())[..8]
    )
}

fn take_line(t: &Take) -> String {
    let tests = match t.tests {
        Some(true) => "tests green",
        Some(false) => "tests red",
        None => "no tests",
    };
    let found = t.soundcheck.map_or(String::new(), |(h, m, l)| {
        format!(", findings {h} high {m} med {l} low")
    });
    let judge = t
        .judge
        .map_or(String::new(), |j| format!(", producer {j:.2}"));
    let owner = match t.owner {
        Some(true) => ", you accepted it",
        Some(false) => ", you rejected it",
        None => "",
    };
    format!(
        "{}{}: {tests}{found}{judge}, on {}{owner}",
        t.id.rsplit('-').next().unwrap_or(&t.id),
        if t.winner { " (the winner)" } else { "" },
        t.engine
    )
}

fn index_note(
    band: &Band,
    flow: &Flow,
    date: &str,
    o: &Outcome,
    takes: &[Take],
    log_rel: &str,
    used: &str,
) -> String {
    let mut t = format!(
        "---\nband: {}\nflow: {}\nkind: sections-index\nrun: {}\ndate: {date}\ntags: [tempo-band]\n---\n# {}, {date}, in sections\n\nGoal: {}\n\nSections (sub-teams) played at the same time, each as takes (attempts) that the producer judged.\n\n",
        band.name,
        flow.name,
        o.run,
        flow.name,
        flow.goal.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    fn walk(t: &mut String, r: &Report, takes: &[Take], indent: usize) {
        let pad = "  ".repeat(indent);
        t.push_str(&format!(
            "{pad}- {} ({}, @{}): {}\n",
            r.title, r.path, r.member, r.status
        ));
        for k in takes
            .iter()
            .filter(|k| k.part.as_deref() == Some(&format!("section {}", r.path)))
        {
            t.push_str(&format!("{pad}  - take {}\n", take_line(k)));
        }
        for s in &r.subs {
            walk(t, s, takes, indent + 1);
        }
    }
    for r in &o.reports {
        walk(&mut t, r, takes, 0);
    }
    if let Some((_, line)) = &o.halt {
        t.push_str(&format!("\n{line}\n"));
    }
    t.push_str(&format!(
        "\nUsed: {used}.\nRun log: {log_rel} (each line is chained to the one before it).\n"
    ));
    t.push_str("\nDrafts by AI members on this computer. Nothing here was posted or sent.\n");
    t
}

fn used_words(o: &Outcome, tree: &Tree) -> String {
    let (u, b) = (tree.meter.used(), tree.meter.budget());
    let of = |used: u64, max: u64| {
        if max == 0 {
            commas(used)
        } else {
            format!("{} of {}", commas(used), commas(max))
        }
    };
    format!(
        "{} tokens, {} tool calls, {} cloud cents, {} s",
        of(u.tokens, b.tokens),
        of(u.tool_calls, b.tool_calls),
        of(u.cloud_cents, b.cloud_cents),
        o.secs
    )
}

fn choose_flow<'a>(band: &'a Band, flow: Option<&str>) -> Result<&'a Flow, String> {
    let names: Vec<&str> = band.flows.iter().map(|f| f.name.as_str()).collect();
    match flow {
        Some(f) => band.flow(f).ok_or_else(|| {
            format!(
                "{} has no flow called {f}; its flows are {}",
                band.name,
                names.join(", ")
            )
        }),
        None if band.flows.len() == 1 => Ok(&band.flows[0]),
        None => Err(format!(
            "{} has several flows; say which: {}",
            band.name,
            names.join(", ")
        )),
    }
}

/// `conductor sections <band> [flow]`: play a flow as sections and takes.
#[allow(clippy::too_many_arguments)]
pub fn sections_cmd(
    ctx: &Ctx,
    band: &str,
    flow: Option<&str>,
    note: Option<&str>,
    takes: Option<u8>,
    resume: Option<&str>,
    d: &Deps,
    out: &mut dyn Write,
) -> i32 {
    let fail = |e: String| {
        eprintln!("tempo-launch: {e}");
        1
    };
    let band = match super::load(ctx, band) {
        Ok(b) => b,
        Err(e) => return fail(e),
    };
    let flow = match choose_flow(&band, flow) {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    let mut settings = match Settings::load(ctx, &band.name) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    if let Some(t) = takes {
        settings.takes = t;
    }
    let Some(chat) = d.chat.parallel() else {
        return fail("this agent connection cannot play members side by side".into());
    };
    let engines_now = d.outside.engines();
    let cast = engines::cast(&band, &engines_now);
    let checks = d.outside.checks(ctx, &settings);
    let at = run::places(ctx, &band, flow);
    let date = runlog::date_of(d.now);
    let (log, resumed) = match resume {
        Some(id) => match RunLog::reopen(ctx, id) {
            Ok(l) => {
                let done = runlog::done_steps(&l.path);
                (l, done)
            }
            Err(e) => return fail(e),
        },
        None => match RunLog::create(ctx, &run_id(d.now, &band.name), d.now) {
            Ok(l) => (l, Default::default()),
            Err(e) => return fail(format!("could not start the run log: {e}")),
        },
    };
    let stop_file = runlog::runs_dir(ctx).join(format!("{}.stop", log.run));
    if resume.is_some() && stop_file.exists() {
        // a stop from before the run was picked up again must not end it at once; kept, not deleted
        let _ = std::fs::rename(&stop_file, stop_file.with_extension("stop.seen"));
    }
    let takes_path = takes_file::path(ctx);
    let (dir, folder) = match run::make_run_folder(&at.root, &date, &flow.name) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let _ = writeln!(
        out,
        "The Conductor raises the baton, in sections: {} / {}. Run {}. Notes go to {}",
        band.name,
        flow.name,
        log.run,
        dir.display()
    );
    let _ = writeln!(
        out,
        "  Sections (sub-teams) up to 4 at a time, depth 2; {} {} per part (attempts); the producer judges them.",
        settings.takes,
        if settings.takes == 1 { "take" } else { "takes" }
    );
    let mut tree = Tree::new(
        chat,
        &*checks,
        &log,
        takes_path.clone(),
        &settings,
        &band,
        flow,
        &cast,
        &engines_now,
    );
    tree.note = note.unwrap_or("").to_string();
    tree.material = run::read_inbox(&at.inbox);
    tree.resume = resumed;
    let goal = match note.filter(|n| !n.trim().is_empty()) {
        Some(n) => format!("{} (owner's note: {})", flow.goal.trim(), n.trim()),
        None => flow.goal.trim().to_string(),
    };
    let outcome = std::thread::scope(|s| {
        let h = s.spawn(|| tree.run(&goal));
        loop {
            for l in tree.drain_lines() {
                let _ = writeln!(out, "  {l}");
            }
            if stop_file.exists() {
                tree.meter.stop();
            }
            if h.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        for l in tree.drain_lines() {
            let _ = writeln!(out, "  {l}");
        }
        h.join()
    });
    let Ok(o) = outcome else {
        return fail(format!(
            "the run stopped unexpectedly; what it did is in {}",
            log.path.display()
        ));
    };
    // the notes: the index, one note per section, the finished piece
    let all_takes: Vec<Take> = takes_file::read_all(&takes_path)
        .into_iter()
        .filter(|t| t.run == o.run)
        .collect();
    let used = used_words(&o, &tree);
    let log_rel = log.reference(1).split('#').next().unwrap_or("").to_string();
    let index = dir.join("Index.md");
    let mut wrote = write_atomic(
        &index,
        index_note(&band, flow, &date, &o, &all_takes, &log_rel, &used).as_bytes(),
    );
    for (i, r) in o.reports.iter().enumerate() {
        if r.answer.is_empty() {
            continue;
        }
        let body = format!(
            "---\nband: {}\nmember: {}\nflow: {}\nstep: {}\nrun: {}\ndate: {date}\ntags: [tempo-band]\n---\n# Section {}: {}\n\nFlow: [[{}{}/Index|{}, {date}]]\n\n{}\n\n---\nA draft by {} on this computer. Nothing here was posted or sent.\n",
            band.name, r.member, flow.name, i + 1, o.run, r.path, r.title, at.link_base, folder, flow.name, r.answer.trim(), r.member
        );
        wrote = wrote.and_then(|()| {
            write_atomic(
                &dir.join(format!("{} section-{}.md", i + 1, r.path)),
                body.as_bytes(),
            )
        });
    }
    if let Some(text) = &o.final_text {
        let body = format!(
            "---\nband: {}\nmember: {}\nflow: {}\nstep: {}\nrun: {}\ndate: {date}\ntags: [tempo-band]\n---\n# The finished piece\n\nFlow: [[{}{}/Index|{}, {date}]]\n\n{}\n\n---\nA draft by {} on this computer. Nothing here was posted or sent.\n",
            band.name, o.final_by, flow.name, o.reports.len() + 1, o.run, at.link_base, folder, flow.name, text.trim(), o.final_by
        );
        wrote = wrote.and_then(|()| {
            write_atomic(
                &dir.join(format!("{} final.md", o.reports.len() + 1)),
                body.as_bytes(),
            )
        });
    }
    if let Err(e) = wrote {
        return fail(format!(
            "could not write the notes in {}: {e}",
            dir.display()
        ));
    }
    match &o.halt {
        Some((_, line)) => {
            let _ = writeln!(out, "{line}\n  What got done is in {}. Pick it up again with: tempo-launch conductor sections {} --resume {}", dir.display(), band.name, o.run);
        }
        None => {
            let _ = writeln!(
                out,
                "Done in {} s. {} sections, {} takes. Used: {used}.\n  Start at {}. The run log is {}.",
                o.secs,
                o.reports.len(),
                o.takes,
                index.display(),
                log.path.display()
            );
        }
    }
    0
}

struct Row {
    run: String,
    day: String,
    state: String,
    band_flow: String,
    used: String,
    sections: u64,
    intact: Result<usize, String>,
}

fn row_of(day: &str, path: &Path) -> Row {
    let ev = runlog::read(path);
    let end = ev
        .iter()
        .rev()
        .find(|e| matches!(e.str("kind"), Some("final" | "stop")));
    let run = path
        .file_stem()
        .map_or(String::new(), |s| s.to_string_lossy().into_owned());
    let (state, band_flow, used, sections) = match end {
        Some(e) => {
            let u = e.get("used");
            let n = |k: &str| u.and_then(|u| u.f64(k)).map_or(0, |v| v as u64);
            let state = if e.str("kind") == Some("stop") {
                e.str("line").unwrap_or("stopped").to_string()
            } else {
                format!("done in {} s", n("secs"))
            };
            (
                state,
                format!("{}/{}", e.str("band").unwrap_or("?"), e.str("flow").unwrap_or("?")),
                format!("{} tokens, {} tool calls, {} cloud cents", commas(n("tokens")), n("tool_calls"), n("cloud_cents")),
                e.f64("sections").map_or(0, |v| v as u64),
            )
        }
        None => (
            format!("unfinished; pick it up with: tempo-launch conductor sections <band> --resume {run}"),
            "?".into(),
            String::new(),
            0,
        ),
    };
    Row {
        run,
        day: day.into(),
        state,
        band_flow,
        used,
        sections,
        intact: runlog::verify(path),
    }
}

/// `conductor runs`: the newest runs, what they used, their takes and winners.
pub fn runs_cmd(ctx: &Ctx, out: &mut dyn Write) -> i32 {
    let mut files: Vec<(String, std::time::SystemTime, std::path::PathBuf)> = vec![];
    for day in std::fs::read_dir(runlog::runs_dir(ctx))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = day.file_name().to_string_lossy().into_owned();
        if !day.path().is_dir() || name.len() != 10 {
            continue;
        }
        for f in std::fs::read_dir(day.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if f.path().extension().is_some_and(|x| x == "jsonl") {
                let m = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                files.push((name.clone(), m, f.path()));
            }
        }
    }
    if files.is_empty() {
        let _ = writeln!(out, "No sectioned runs yet. `tempo-launch conductor sections <band>` plays a flow as sections and takes.");
        return 0;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let takes = takes_file::read_all(&takes_file::path(ctx));
    for (day, _, path) in files.into_iter().take(10) {
        let r = row_of(&day, &path);
        let mine: Vec<&Take> = takes.iter().filter(|t| t.run == r.run).collect();
        let winners = mine.iter().filter(|t| t.winner).count();
        let _ = writeln!(out, "{}  {}  {}  {}", r.run, r.day, r.band_flow, r.state);
        if !r.used.is_empty() {
            let _ = writeln!(out, "  used {}", r.used);
        }
        let chain = match &r.intact {
            Ok(n) => format!("log intact ({n} lines)"),
            Err(e) => format!("log NOT intact: {e}"),
        };
        let _ = writeln!(
            out,
            "  {} sections, {} takes, {winners} winners, {chain}",
            r.sections,
            mine.len()
        );
        for t in mine.iter().filter(|t| t.winner) {
            let _ = writeln!(out, "    {}", take_line(t));
        }
    }
    0
}

/// `conductor stop [run]`: the run ends at its next step. With no name, the newest unfinished one.
pub fn stop_cmd(ctx: &Ctx, run: Option<&str>, out: &mut dyn Write) -> i32 {
    let id = match run {
        Some(r) => r.to_string(),
        None => {
            let mut open: Vec<(std::time::SystemTime, String)> = vec![];
            for day in std::fs::read_dir(runlog::runs_dir(ctx))
                .into_iter()
                .flatten()
                .flatten()
            {
                for f in std::fs::read_dir(day.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let p = f.path();
                    if p.extension().is_none_or(|x| x != "jsonl") {
                        continue;
                    }
                    let done = runlog::read(&p)
                        .iter()
                        .any(|e| matches!(e.str("kind"), Some("final" | "stop")));
                    if !done {
                        let m = f
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::UNIX_EPOCH);
                        open.push((
                            m,
                            p.file_stem()
                                .map_or(String::new(), |s| s.to_string_lossy().into_owned()),
                        ));
                    }
                }
            }
            open.sort();
            match open.pop() {
                Some((_, id)) => id,
                None => {
                    let _ = writeln!(
                        out,
                        "No run is going. `tempo-launch conductor runs` shows the recent ones."
                    );
                    return 0;
                }
            }
        }
    };
    if runlog::find(ctx, &id).is_none() {
        eprintln!(
            "tempo-launch: no run called {id}. `tempo-launch conductor runs` lists the recent ones"
        );
        return 1;
    }
    match write_atomic(&runlog::runs_dir(ctx).join(format!("{id}.stop")), b"stop\n") {
        Ok(()) => {
            let _ = writeln!(out, "Stop sent to {id}. It ends at its next step and says what got done; a request already with a model finishes first.");
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: could not send the stop: {e}");
            1
        }
    }
}

/// `conductor takes accept|reject <take>`: the owner's say on one take.
pub fn verdict_cmd(ctx: &Ctx, take: &str, accept: bool, out: &mut dyn Write) -> i32 {
    let file = takes_file::path(ctx);
    let before = takes_file::read_all(&file)
        .into_iter()
        .find(|t| t.id == take);
    match takes_file::set_owner(&file, take, accept) {
        Ok(m) => {
            let _ = writeln!(out, "{m}");
            // a thumb on a render a band made on Stage teaches that band which Pedals worked
            if let Some(t) = before {
                if let Some(note) =
                    crate::stage::learn::note_verdict(ctx, &t, t.owner, accept, crate::unix_now())
                {
                    let _ = writeln!(out, "{note}");
                }
            }
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            1
        }
    }
}
