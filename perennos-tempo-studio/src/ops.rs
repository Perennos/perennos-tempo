//! The operations behind the CLI and the MCP tools. Each returns a JSON report.
//!
//! Engine choice: `builtin` is plan B only; `reaper` needs REAPER; `auto` uses REAPER when it is
//! found and the built-in engine otherwise, with a note saying which and why. When a REAPER run
//! fails under `auto`, the built-in engine finishes the job (again with a note).
use crate::config::{Config, Engine};
use crate::loudness;
use crate::master::{self, MasterSettings};
use crate::mix::{self, MasterStep};
use crate::reaper::{self, Located, NotFound, Os, RenderOpts, Runner, SessionPlan, Target, Wait};
use crate::util::{free_path, jnum, r2, resolve};
use crate::wav::OutFormat;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub type Locator<'a> = &'a dyn Fn(Option<&str>, Option<&str>) -> Result<Located, NotFound>;

pub struct Ctx<'a> {
    pub cfg: Config,
    /// `--reaper` from the command line.
    pub reaper_flag: Option<String>,
    pub runner: &'a dyn Runner,
    pub os: Os,
    pub wait: Wait,
    /// `reaper::locate_here` in real use; tests describe a machine instead.
    pub locator: Locator<'a>,
}

impl Ctx<'_> {
    pub fn locate(&self) -> Result<Located, NotFound> {
        (self.locator)(self.reaper_flag.as_deref(), self.cfg.reaper_exe.as_deref())
    }

    fn pick(&self, engine: Engine, notes: &mut Vec<String>) -> Result<Option<Located>, String> {
        match engine {
            Engine::Builtin => Ok(None),
            // REAPER opens its window, so it runs only when asked for by name (the house rule: no
            // windows from background or scheduled work).
            Engine::Reaper => {
                let l = self.locate().map_err(|nf| nf.message())?;
                notes.push(format!(
                    "engine reaper: using REAPER at {} ({}); its window opens",
                    l.exe.display(),
                    l.source
                ));
                Ok(Some(l))
            }
            Engine::Auto => {
                match self.locate() {
                    Ok(l) => notes.push(format!(
                        "engine auto: the built-in engine. REAPER is at {} but opens a window, so it runs only with --engine reaper.",
                        l.exe.display()
                    )),
                    Err(nf) => notes.push(format!(
                        "engine auto: {} Used the built-in engine (plan B).",
                        nf.message()
                    )),
                }
                Ok(None)
            }
        }
    }
}

fn add_notes(j: &mut Value, notes: Vec<String>) {
    if notes.is_empty() {
        return;
    }
    match j.get_mut("notes").and_then(|n| n.as_array_mut()) {
        Some(a) => {
            for (i, n) in notes.into_iter().enumerate() {
                a.insert(i, json!(n));
            }
        }
        None => j["notes"] = json!(notes),
    }
}

/// `<stem><suffix>` next to `p`.
fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    p.with_file_name(format!("{stem}{suffix}"))
}

fn target_of(s: &MasterSettings) -> Target {
    Target {
        target_lufs: s.target_lufs,
        ceiling_dbtp: s.ceiling_dbtp,
    }
}

/// Mastering settings: a preset (else `default_preset`), with the target and ceiling overridable
/// one by one. An override drops the preset name from the report.
pub fn settings_from(
    preset: Option<&str>,
    target: Option<f64>,
    ceiling: Option<f64>,
    default_preset: &str,
) -> Result<MasterSettings, String> {
    let p = master::preset(preset.unwrap_or(default_preset))?;
    if target.is_none() && ceiling.is_none() {
        return Ok(MasterSettings::from_preset(&p));
    }
    MasterSettings::new(
        target.unwrap_or(p.target_lufs),
        ceiling.unwrap_or(p.ceiling_dbtp),
    )
}

/// Measure each file. Files that cannot be read are reported in place; the bool is false then.
pub fn analyze(paths: &[PathBuf]) -> (Value, bool) {
    let mut ok = true;
    let files: Vec<Value> = paths
        .iter()
        .map(|p| match loudness::analyze_file(p) {
            Ok(a) => a.to_json(),
            Err(e) => {
                ok = false;
                json!({"path": p.display().to_string(), "error": e})
            }
        })
        .collect();
    (json!({ "files": files }), ok)
}

pub struct MasterArgs {
    pub input: PathBuf,
    pub out: Option<PathBuf>,
    pub settings: MasterSettings,
    pub engine: Engine,
    pub force: bool,
}

pub fn master(ctx: &Ctx, a: &MasterArgs) -> Result<Value, String> {
    // Absolute paths throughout: REAPER would read relative ones against its own folder.
    let a = &MasterArgs {
        input: resolve(&a.input),
        out: a.out.as_deref().map(resolve),
        settings: a.settings.clone(),
        engine: a.engine,
        force: a.force,
    };
    if !a.input.is_file() {
        return Err(format!("{} is not a file", a.input.display()));
    }
    let mut notes = Vec::new();
    let want = a
        .out
        .clone()
        .unwrap_or_else(|| master::default_out(&a.input));
    if let Some(l) = ctx.pick(a.engine, &mut notes)? {
        if a.force {
            notes.push(
                "--force does not apply to REAPER renders: they always go to a free name".into(),
            );
        }
        match master_with_reaper(ctx, &l, a, &want) {
            Ok(mut j) => {
                add_notes(&mut j, notes);
                return Ok(j);
            }
            // plan B: a REAPER that fails still gets the song mastered
            Err(e) => notes.push(format!(
                "REAPER failed: {e}. Used the built-in engine (plan B)."
            )),
        }
    }
    let mut j = master::master_file(&a.input, &want, a.force, &a.settings)?.to_json();
    add_notes(&mut j, notes);
    Ok(j)
}

/// Plan A mastering: one track with the input, gain before ReaLimit on the master; the built-in
/// passes supply the starting gain.
fn master_with_reaper(
    ctx: &Ctx,
    l: &Located,
    a: &MasterArgs,
    want: &Path,
) -> Result<Value, String> {
    let est = master::plan_master(&a.input, &a.settings)?;
    let s = &a.settings;
    let name = a
        .input
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Song".into());
    let plan = json!({
        "sample_rate": est.input.info.sample_rate,
        "trim_db": 0.0,
        "stems": [{"path": a.input.display().to_string(), "name": name, "gain_db": 0.0}],
        "master": {"target_lufs": s.target_lufs, "ceiling_dbtp": s.ceiling_dbtp, "gain_db": est.best.gain_db},
    });
    let dir = want.parent().unwrap_or(Path::new("."));
    let script = sibling(want, "-session.lua");
    let mut session = SessionPlan::from_plan_json(&plan, dir, &script)?;
    session.render_path = free_path(want)?;
    let mut j = reaper::session_and_render(
        ctx.runner,
        &l.exe,
        ctx.os,
        &session,
        &script,
        Some(target_of(s)),
        ctx.wait,
    )?;
    j["settings"] = s.to_json();
    j["builtin_estimate"] = json!({
        "gain_db": r2(est.best.gain_db),
        "integrated_lufs": jnum(est.best.result.integrated),
    });
    j["input"] = est.input.to_json();
    Ok(j)
}

pub struct MixArgs {
    pub inputs: Vec<PathBuf>,
    pub out: PathBuf,
    pub engine: Engine,
    pub master: Option<MasterSettings>,
    pub reference_lufs: f64,
    pub format: OutFormat,
    pub force: bool,
}

pub fn mix(ctx: &Ctx, a: &MixArgs) -> Result<Value, String> {
    let a = &MixArgs {
        inputs: a.inputs.iter().map(|p| resolve(p)).collect(),
        out: resolve(&a.out),
        engine: a.engine,
        master: a.master.clone(),
        reference_lufs: a.reference_lufs,
        format: a.format,
        force: a.force,
    };
    let mut notes = Vec::new();
    let reaper = ctx.pick(a.engine, &mut notes)?;
    let files = mix::collect_stems(&a.inputs, Some(&a.out))?;
    let plan = match mix::plan_mix(&files, a.reference_lufs) {
        Ok(p) => p,
        // Stems only REAPER can mix: no built-in mix, REAPER renders straight to --out.
        Err(e) if reaper.is_some() && e.contains("--engine reaper") => {
            let l = reaper.as_ref().expect("checked");
            notes.push(format!("{e}. REAPER mixes them."));
            let plan = mix::plan_for_reaper(&files, a.reference_lufs)?;
            let (plan_path, pj) = mix::write_plan_only(&plan, &a.out, a.master.as_ref())?;
            let mut j = reaper_mix(ctx, l, &pj, &plan_path, &a.out, a.master.as_ref())?;
            j["plan_path"] = json!(plan_path.display().to_string());
            add_notes(&mut j, notes);
            return Ok(j);
        }
        Err(e) => return Err(e),
    };
    let Some(l) = reaper else {
        let step = a
            .master
            .as_ref()
            .map_or(MasterStep::None, MasterStep::Write);
        let mut j = mix::render_mix(plan, &a.out, a.force, a.format, step)?.to_json();
        add_notes(&mut j, notes);
        return Ok(j);
    };
    // Plan A on top of plan B: the built-in mix and its plan come first (the reference and the
    // fallback), then the REAPER session built from that plan.
    let step = a
        .master
        .as_ref()
        .map_or(MasterStep::None, MasterStep::Estimate);
    let r = mix::render_mix(plan, &a.out, a.force, a.format, step)?;
    let mut j = r.to_json();
    let render_want = sibling(&r.mix_path, "-reaper.wav");
    match reaper_mix(
        ctx,
        &l,
        &r.plan_json(),
        &r.plan_path,
        &render_want,
        a.master.as_ref(),
    ) {
        Ok(rj) => j["reaper"] = rj,
        Err(e) => {
            notes.push(format!(
                "REAPER failed: {e}. Used the built-in engine (plan B)."
            ));
            if let Some(s) = &a.master {
                let m = master::master_file(
                    &r.mix_path,
                    &master::default_out(&r.mix_path),
                    a.force,
                    s,
                )?;
                j["master_report"] = m.to_json();
            }
        }
    }
    add_notes(&mut j, notes);
    Ok(j)
}

/// A REAPER session for a mix plan, rendering to `render_want` (a free name), verified against
/// the master target when there is one.
fn reaper_mix(
    ctx: &Ctx,
    l: &Located,
    plan: &Value,
    plan_path: &Path,
    render_want: &Path,
    master: Option<&MasterSettings>,
) -> Result<Value, String> {
    let dir = plan_path.parent().unwrap_or(Path::new("."));
    let script = sibling(render_want, "-session.lua");
    let mut session = SessionPlan::from_plan_json(plan, dir, &script)?;
    session.render_path = free_path(render_want)?;
    reaper::session_and_render(
        ctx.runner,
        &l.exe,
        ctx.os,
        &session,
        &script,
        master.map(target_of),
        ctx.wait,
    )
}

/// `{"found": true, ...}` or the not-found list; the bool is false when not found.
pub fn reaper_locate(ctx: &Ctx) -> (Value, bool) {
    match ctx.locate() {
        Ok(l) => (l.to_json(), true),
        Err(nf) => (nf.to_json(), false),
    }
}

/// Start the REAPER GUI with a project, media file or script (visible on purpose; not waited for).
pub fn reaper_open(ctx: &Ctx, file: &Path) -> Result<Value, String> {
    let file = &resolve(file);
    if !file.is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    let l = ctx.locate().map_err(|nf| nf.message())?;
    let cmd = reaper::open_cmd(&l.exe, file, ctx.os);
    let p = ctx.runner.start(&cmd)?;
    Ok(json!({
        "opened": file.display().to_string(),
        "exe": l.exe.display().to_string(),
        "command": cmd.display(),
        "pid": p.id(),
    }))
}

fn read_plan(plan_path: &Path) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(plan_path).map_err(|e| format!("{}: {e}", plan_path.display()))?;
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("{} is not valid JSON: {e}", plan_path.display()))
}

/// Default script name for a plan: `<name>-session.lua` next to it (`mix.plan.json` gives `mix-session.lua`).
pub fn default_script_for(plan_path: &Path) -> PathBuf {
    let stem = plan_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "studio".into());
    let stem = stem.strip_suffix(".plan").unwrap_or(&stem).to_string();
    plan_path.with_file_name(format!("{stem}-session.lua"))
}

/// The session for `plan_path`, with every stem path it names checked by `allow`.
pub fn session_for(
    plan_path: &Path,
    script: &Path,
    render: bool,
    close: bool,
    allow: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<SessionPlan, String> {
    let (plan_path, script) = (&resolve(plan_path), &resolve(script));
    let plan = read_plan(plan_path)?;
    let dir = plan_path.parent().unwrap_or(Path::new("."));
    let mut session = SessionPlan::from_plan_json(&plan, dir, script)?;
    for t in &session.tracks {
        allow(&t.file)?;
    }
    session.render = render;
    session.close = close;
    Ok(session)
}

/// Write the ReaScript for a plan; with `run`, also start REAPER with it and wait for its status.
pub fn reaper_build_session(
    ctx: &Ctx,
    session: &SessionPlan,
    script_want: &Path,
    run: bool,
) -> Result<Value, String> {
    let script_want = &resolve(script_want);
    if !run {
        let script = reaper::write_script(session, script_want)?;
        return Ok(json!({
            "script": script.display().to_string(),
            "session": session.to_json(),
            "how_to_run": "start REAPER with this file, or in REAPER: Actions > Show action list > New action > Load ReaScript",
        }));
    }
    let l = ctx.locate().map_err(|nf| nf.message())?;
    let run = reaper::run_session(ctx.runner, &l.exe, ctx.os, session, script_want, ctx.wait)?;
    Ok(run.to_json())
}

/// Headless render of a project, verified against a target when one is given.
pub fn reaper_render(
    ctx: &Ctx,
    rpp: &Path,
    target: Option<Target>,
    out: Option<PathBuf>,
) -> Result<Value, String> {
    let rpp = &resolve(rpp);
    let l = ctx.locate().map_err(|nf| nf.message())?;
    let o = RenderOpts {
        target,
        out: out.as_deref().map(resolve),
        wait: ctx.wait,
        ..RenderOpts::default()
    };
    Ok(reaper::render_project(ctx.runner, &l.exe, rpp, None, &o)?.to_json())
}

#[cfg(test)]
pub mod testctx {
    use super::*;
    use crate::reaper::Tried;

    pub fn not_found(_: Option<&str>, _: Option<&str>) -> Result<Located, NotFound> {
        Err(NotFound {
            tried: vec![Tried {
                source: "standard install path".into(),
                path: "/opt/REAPER/reaper".into(),
                why: "not there".into(),
            }],
        })
    }

    pub fn found(_: Option<&str>, _: Option<&str>) -> Result<Located, NotFound> {
        Ok(Located {
            exe: PathBuf::from("/fake/reaper"),
            source: "--reaper".into(),
            skipped: vec![],
        })
    }

    pub fn ctx<'a>(runner: &'a dyn Runner, locator: Locator<'a>) -> Ctx<'a> {
        Ctx {
            cfg: Config::default(),
            reaper_flag: None,
            runner,
            os: Os::Linux,
            wait: Wait {
                timeout: std::time::Duration::from_secs(5),
                poll: std::time::Duration::from_millis(5),
            },
            locator,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testctx::*;
    use super::*;
    use crate::loudness::testsig::{db, sine};
    use crate::reaper::testfake::{tone_at, FakeReaper};
    use crate::util::testutil::tmpdir;
    use crate::wav::write_new;

    fn streaming() -> MasterSettings {
        MasterSettings::from_preset(&master::preset("streaming").unwrap())
    }

    #[test]
    fn settings_from_presets_and_overrides() {
        let s = settings_from(None, None, None, "streaming").unwrap();
        assert_eq!(
            (s.preset, s.target_lufs, s.ceiling_dbtp),
            (Some("streaming"), -14.0, -1.0)
        );
        let s = settings_from(Some("club"), None, None, "streaming").unwrap();
        assert_eq!((s.target_lufs, s.ceiling_dbtp), (-9.0, -0.3));
        let s = settings_from(None, Some(-12.0), None, "apple").unwrap();
        assert_eq!(
            (s.preset, s.target_lufs, s.ceiling_dbtp),
            (None, -12.0, -1.0)
        );
        assert!(settings_from(Some("nope"), None, None, "streaming").is_err());
        assert!(settings_from(None, None, Some(1.0), "streaming").is_err());
    }

    #[test]
    fn auto_without_reaper_masters_with_the_builtin_engine_and_says_so() {
        let d = tmpdir("ops-auto");
        let input = d.join("song.wav");
        tone_at(&input, -20.0);
        let fake = FakeReaper::new(-14.0);
        let c = ctx(&fake, &not_found);
        let a = MasterArgs {
            input: input.clone(),
            out: None,
            settings: streaming(),
            engine: Engine::Auto,
            force: false,
        };
        let j = master(&c, &a).unwrap();
        assert_eq!(j["engine"], "builtin");
        assert!(
            j["notes"][0].as_str().unwrap().contains("REAPER not found"),
            "{j:#}"
        );
        assert!(j["notes"][0]
            .as_str()
            .unwrap()
            .contains("/opt/REAPER/reaper"));
        assert_eq!(j["within_target"], true);
        assert!(fake.calls.borrow().is_empty());
        // engine reaper without REAPER: an error with the paths tried.
        let a = MasterArgs {
            engine: Engine::Reaper,
            ..a
        };
        let e = master(&c, &a).unwrap_err();
        assert!(
            e.contains("REAPER not found") && e.contains("/opt/REAPER/reaper"),
            "{e}"
        );
    }

    #[test]
    fn auto_never_opens_reaper_and_engine_reaper_masters_in_a_session() {
        let d = tmpdir("ops-reaper");
        let input = d.join("song.wav");
        tone_at(&input, -20.0);
        let fake = FakeReaper::new(-14.2);
        let c = ctx(&fake, &found);
        // auto with REAPER installed: the built-in engine, no REAPER window, and it says why
        let auto = MasterArgs {
            input: input.clone(),
            out: Some(d.join("auto-master.wav")),
            settings: streaming(),
            engine: Engine::Auto,
            force: false,
        };
        let j = master(&c, &auto).unwrap();
        assert_eq!(j["engine"], "builtin", "{j:#}");
        assert!(fake.calls.borrow().is_empty(), "REAPER was not started");
        assert!(
            j["notes"][0].as_str().unwrap().contains("--engine reaper"),
            "{j:#}"
        );
        let a = MasterArgs {
            input,
            out: Some(d.join("song-master.wav")),
            settings: streaming(),
            engine: Engine::Reaper,
            force: false,
        };
        let j = master(&c, &a).unwrap();
        assert_eq!(j["engine"], "reaper", "{j:#}");
        assert_eq!(j["within_target"], true);
        assert_eq!(
            j["final_path"],
            d.join("song-master.wav").display().to_string()
        );
        assert!(j["notes"][0]
            .as_str()
            .unwrap()
            .contains("using REAPER at /fake/reaper"));
        // The session track carries the built-in gain estimate (about +6 dB) before ReaLimit.
        let lua = std::fs::read_to_string(d.join("song-master-session.lua")).unwrap();
        assert!(
            lua.contains("vol_db = 5.") || lua.contains("vol_db = 6."),
            "{lua}"
        );
        assert!(lua.contains("master = { limiter = true, ceiling_db = -1.5000 }"));
        // A REAPER that fails: the built-in engine finishes the job (plan B).
        let mut broken = FakeReaper::new(-14.0);
        broken.status_override =
            Some(r#"{"done":true,"ok":false,"error":"no audio device"}"#.into());
        let c = ctx(&broken, &found);
        let a = MasterArgs {
            out: Some(d.join("b.wav")),
            ..a
        };
        let j = master(&c, &a).unwrap();
        assert_eq!(j["engine"], "builtin");
        assert!(
            j["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("no audio device")),
            "{j:#}"
        );
    }

    #[test]
    fn mix_with_reaper_builds_a_session_from_the_builtin_plan() {
        let d = tmpdir("ops-mix");
        let stems = d.join("stems");
        std::fs::create_dir_all(&stems).unwrap();
        for (n, l) in [
            ("kick.wav", -12.0),
            ("bass.wav", -18.0),
            ("lead vox.wav", -16.0),
        ] {
            tone_at(&stems.join(n), l);
        }
        let fake = FakeReaper::new(-14.3);
        let c = ctx(&fake, &found);
        let a = MixArgs {
            inputs: vec![stems.clone()],
            out: d.join("mix.wav"),
            engine: Engine::Reaper,
            master: Some(streaming()),
            reference_lufs: mix::DEFAULT_REFERENCE_LUFS,
            format: OutFormat::Pcm24,
            force: false,
        };
        let j = mix(&c, &a).unwrap();
        assert!(d.join("mix.wav").is_file() && d.join("mix.plan.json").is_file());
        assert!(
            !d.join("mix-master.wav").exists(),
            "REAPER masters, not the built-in engine"
        );
        assert_eq!(j["reaper"]["within_target"], true, "{j:#}");
        assert_eq!(j["reaper"]["session"]["status"]["items"], 3);
        let lua = std::fs::read_to_string(d.join("mix-reaper-session.lua")).unwrap();
        assert!(lua.contains("\"lead vox\"") && lua.contains("\"ReaComp\""));
        // Builtin only: mix, plan and master.
        let b = MixArgs {
            engine: Engine::Builtin,
            out: d.join("b.wav"),
            ..a
        };
        let j = mix(&c, &b).unwrap();
        assert_eq!(j["master_report"]["within_target"], true);
        assert!(d.join("b-master.wav").is_file());
    }

    #[test]
    fn mixed_rate_stems_go_straight_to_reaper() {
        let d = tmpdir("ops-rates");
        write_new(
            &d.join("a.wav"),
            44100,
            1,
            OutFormat::Pcm24,
            &sine(44100, 1, 2.0, 300.0, db(-10.0), 0.0),
        )
        .unwrap();
        write_new(
            &d.join("b.wav"),
            48000,
            1,
            OutFormat::Pcm24,
            &sine(48000, 1, 2.0, 200.0, db(-10.0), 0.0),
        )
        .unwrap();
        let a = MixArgs {
            inputs: vec![d.join("a.wav"), d.join("b.wav")],
            out: d.join("mix.wav"),
            engine: Engine::Auto,
            master: None,
            reference_lufs: mix::DEFAULT_REFERENCE_LUFS,
            format: OutFormat::Pcm24,
            force: false,
        };
        let fake = FakeReaper::new(-16.0);
        let in_reaper = MixArgs {
            inputs: a.inputs.clone(),
            out: a.out.clone(),
            engine: Engine::Reaper,
            master: None,
            reference_lufs: mix::DEFAULT_REFERENCE_LUFS,
            format: OutFormat::Pcm24,
            force: false,
        };
        let j = mix(&ctx(&fake, &found), &in_reaper).unwrap();
        assert_eq!(
            j["final_path"],
            d.join("mix.wav").display().to_string(),
            "{j:#}"
        );
        assert!(j["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("REAPER mixes them")));
        // Under auto (and without REAPER) the same stems are a clear error that points to REAPER.
        let e = mix(&ctx(&fake, &found), &a).unwrap_err();
        assert!(e.contains("--engine reaper"), "{e}");
        let e = mix(&ctx(&fake, &not_found), &a).unwrap_err();
        assert!(
            e.contains("different sample rates") && e.contains("REAPER"),
            "{e}"
        );
    }

    #[test]
    fn script_build_and_open_and_render_ops() {
        let d = tmpdir("ops-script");
        tone_at(&d.join("a.wav"), -20.0);
        let plan = d.join("mix.plan.json");
        std::fs::write(
            &plan,
            json!({"stems": [{"path": d.join("a.wav").display().to_string(), "name": "A"}]})
                .to_string(),
        )
        .unwrap();
        assert_eq!(default_script_for(&plan), d.join("mix-session.lua"));
        let ok = |_: &Path| Ok(());
        let s = session_for(&plan, &default_script_for(&plan), true, true, &ok).unwrap();
        let fake = FakeReaper::new(-14.0);
        let c = ctx(&fake, &found);
        let j = reaper_build_session(&c, &s, &default_script_for(&plan), false).unwrap();
        assert_eq!(j["script"], d.join("mix-session.lua").display().to_string());
        assert!(fake.calls.borrow().is_empty());
        let j = reaper_build_session(&c, &s, &default_script_for(&plan), true).unwrap();
        assert_eq!(j["status"]["done"], true);
        assert_eq!(fake.calls.borrow().len(), 1);
        // A stem outside the allowed folders is refused.
        let deny = |p: &Path| Err(format!("{} is outside", p.display()));
        assert!(session_for(&plan, &d.join("x.lua"), false, false, &deny)
            .unwrap_err()
            .contains("outside"));
        // open: visible, not waited for.
        let j = reaper_open(&c, &d.join("a.wav")).unwrap();
        assert_eq!(j["pid"], 4242);
        assert!(reaper_open(&c, &d.join("missing.wav")).is_err());
        // render a project.
        let rpp = d.join("p.rpp");
        std::fs::write(&rpp, crate::reaper::testfake::FAKE_RPP).unwrap();
        let j = reaper_render(&c, &rpp, Some(target_of(&streaming())), None).unwrap();
        assert_eq!(j["within_target"], true);
        let (lj, found_it) = reaper_locate(&ctx(&fake, &not_found));
        assert!(!found_it && lj["tried"][0]["path"] == "/opt/REAPER/reaper");
        let (analysis, all_ok) = analyze(&[d.join("a.wav"), d.join("missing.wav")]);
        assert!(!all_ok);
        assert!(analysis["files"][1]["error"]
            .as_str()
            .unwrap()
            .contains("missing.wav"));
    }
}
