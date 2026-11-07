//! `woodshed promote`, `rollback` and `versions`. Promoting makes an Ollama model from the base and
//! the adapter (a Modelfile with `FROM <base>` and `ADAPTER <file>`, then `ollama create
//! tempo-<band>-<yyyymmdd>`) and tells the Conductor to play that band's parts on it. Rolling back is
//! one command: it goes back to the version that was playing when this one was trained, or to the
//! plain base. Both only add a line to `active.jsonl`; no model and no version is ever deleted.

use super::exec::{Exec, Target, Where};
use super::train::ollama_bin;
use super::versions::{self, Manifest};
use crate::Ctx;
use std::io::Write;
use std::time::Duration;

pub fn modelfile(base: &str, adapter: &std::path::Path) -> String {
    format!("FROM {base}\nADAPTER {}\n", adapter.display())
}

/// The newest version that passed its gate and is not what plays now.
fn waiting(ctx: &Ctx, band: &str) -> Option<Manifest> {
    let now = versions::active(ctx, band).and_then(|a| a.version);
    versions::list(ctx, band)
        .into_iter()
        .rev()
        .find(|m| m.passed() && Some(&m.version) != now.as_ref())
}

pub fn promote(
    ctx: &Ctx,
    exec: &dyn Exec,
    band: &str,
    yes: bool,
    now: u64,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let Some(m) = waiting(ctx, band) else {
        let _ = writeln!(out, "Nothing is waiting for {band}. `tempo-launch woodshed run {band}` trains a new version, and only one that passes the gate can be promoted.");
        return 0;
    };
    let all = versions::list(ctx, band);
    let name = versions::model_name(band, now, &all);
    let (b, a) = (
        m.base_scores.clone().unwrap_or_default(),
        m.adapter_scores.clone().unwrap_or_default(),
    );
    let _ = writeln!(
        out,
        "Version {} for {band}: held-out takes {:.1} to {:.1}, general tasks {:.0}% to {:.0}%.\nPromoting makes the Ollama model {name} (FROM {} with the adapter on it) and plays {band}'s parts on it. The old way stays: `tempo-launch woodshed rollback {band}` goes back.",
        m.version, b.eval, a.eval, b.canary, a.canary, m.base
    );
    if !(yes || ask("Put it to work? [y/N] ")) {
        let _ = writeln!(out, "Not promoted. Nothing was changed.");
        return 0;
    }
    let dir = versions::version_dir(ctx, band, &m.version);
    let adapter = dir.join(&m.adapter_file);
    if !adapter.exists() {
        let _ = writeln!(
            out,
            "{} is not there any more, so there is nothing to promote.",
            adapter.display()
        );
        return 1;
    }
    let t = Target {
        at: Where::Local,
        dir: dir.to_string_lossy().into_owned(),
    };
    if let Err(e) = exec.put(&t, "Modelfile", &modelfile(&m.base, &adapter)) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let made = exec.run(
        &t,
        &[
            ollama_bin(ctx),
            "create".into(),
            name.clone(),
            "-f".into(),
            "Modelfile".into(),
        ],
        Duration::from_secs(3600),
    );
    match made {
        Ok(d) if d.ok => {}
        Ok(d) => {
            let _ = writeln!(
                out,
                "Ollama could not make {name}:\n{}\nNothing was changed.",
                d.tail
            );
            return 1;
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "Ollama could not make {name}: {e}\nNothing was changed."
            );
            return 1;
        }
    }
    let mut m = m;
    m.model = name.clone();
    m.note = "promoted".into();
    if let Err(e) = m.save(ctx).and_then(|()| {
        versions::set_active(ctx, band, Some(&m.version), Some(&name), "promoted", now)
    }) {
        let _ = writeln!(out, "{e}");
        return 1;
    }
    let _ = writeln!(
        out,
        "{name} now plays {band}'s parts. Undo with `tempo-launch woodshed rollback {band}`."
    );
    0
}

pub fn rollback(ctx: &Ctx, band: &str, now: u64, out: &mut dyn Write) -> i32 {
    let Some(a) = versions::active(ctx, band).filter(|a| a.version.is_some()) else {
        let _ = writeln!(
            out,
            "{band} is already playing on its plain base. Nothing to roll back."
        );
        return 0;
    };
    let cur = a.version.clone().unwrap_or_default();
    let list = versions::list(ctx, band);
    let parent = list
        .iter()
        .find(|m| m.version == cur)
        .and_then(|m| m.parent.clone());
    // the parent's model, when that version had been promoted
    let back = parent
        .as_ref()
        .and_then(|p| list.iter().find(|m| &m.version == p))
        .filter(|m| !m.model.is_empty());
    let r = match back {
        Some(p) => versions::set_active(
            ctx,
            band,
            Some(&p.version),
            Some(&p.model),
            "rolled back",
            now,
        )
        .map(|()| format!("{band} is back on version {} ({}).", p.version, p.model)),
        None => versions::set_active(ctx, band, None, None, "rolled back", now).map(|()| {
            format!(
                "{band} is back on its plain base, {}.",
                list.iter()
                    .find(|m| m.version == cur)
                    .map_or("the model it started on", |m| m.base.as_str())
            )
        }),
    };
    match r {
        Ok(t) => {
            let _ = writeln!(out, "{t} Version {cur} is kept; `tempo-launch woodshed promote {band}` can bring it back.");
            0
        }
        Err(e) => {
            let _ = writeln!(out, "{e}");
            1
        }
    }
}

pub fn list(ctx: &Ctx, bands: &[String], out: &mut dyn Write) -> i32 {
    let bands = if bands.is_empty() {
        versions::bands_with_versions(ctx)
    } else {
        bands.to_vec()
    };
    let mut any = false;
    for b in bands {
        let all = versions::list(ctx, &b);
        if all.is_empty() {
            continue;
        }
        any = true;
        let playing = versions::active(ctx, &b).and_then(|a| a.version);
        let _ = writeln!(
            out,
            "{b}: {} version{}",
            all.len(),
            if all.len() == 1 { "" } else { "s" }
        );
        for m in all {
            let score = match (&m.base_scores, &m.adapter_scores) {
                (Some(x), Some(y)) => format!(
                    "held-out {:.1} to {:.1}, general {:.0}% to {:.0}%",
                    x.eval, y.eval, x.canary, y.canary
                ),
                _ => "not scored".to_string(),
            };
            let gate = match &m.gate {
                Some(g) if g.passed => "passed",
                Some(_) => "did not pass",
                None => "-",
            };
            let _ = writeln!(
                out,
                "  {}  {}  base {}  data {}  {}  {}{}{}",
                m.version,
                m.created.split(' ').next().unwrap_or(""),
                m.base,
                &m.data_hash[..m.data_hash.len().min(8)],
                score,
                gate,
                m.parent
                    .as_ref()
                    .map_or(String::new(), |p| format!("  parent {p}")),
                if playing.as_ref() == Some(&m.version) {
                    "  <- playing"
                } else {
                    ""
                },
            );
        }
    }
    if !any {
        let _ = writeln!(out, "No versions yet. `tempo-launch woodshed run` makes the first one, once a band has enough winning takes.");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::super::eval::{Gate, Scores};
    use super::super::exec::Done;
    use super::super::takes::test_ctx;
    use super::*;
    use std::cell::RefCell;

    struct Rec(RefCell<Vec<String>>, bool);
    impl Exec for Rec {
        fn run(&self, _: &Target, a: &[String], _: Duration) -> Result<Done, String> {
            self.0.borrow_mut().push(a.join(" "));
            Ok(Done {
                ok: self.1,
                tail: "ollama: no space left (made up)".into(),
            })
        }
        fn put(&self, t: &Target, name: &str, text: &str) -> Result<(), String> {
            std::fs::write(std::path::Path::new(&t.dir).join(name), text).map_err(|e| e.to_string())
        }
        fn get(&self, _: &Target, _: &str, _: &std::path::Path) -> Result<(), String> {
            Ok(())
        }
    }

    fn version(ctx: &Ctx, id: &str, passed: bool, parent: Option<&str>) {
        let m = Manifest {
            version: id.into(),
            band: "b".into(),
            created: "2026-11-07 02:00:00Z".into(),
            base: "qwen3:8b".into(),
            data_hash: "cd".repeat(32),
            parent: parent.map(String::from),
            status: "trained".into(),
            base_scores: Some(Scores {
                eval: 30.0,
                n_eval: 15,
                canary: 75.0,
                n_canary: 12,
            }),
            adapter_scores: Some(Scores {
                eval: 40.0,
                n_eval: 15,
                canary: 75.0,
                n_canary: 12,
            }),
            margin: 5.0,
            gate: Some(Gate {
                passed,
                reason: "r".into(),
            }),
            adapter_file: "adapter.gguf".into(),
            ..Manifest::default()
        };
        m.save(ctx).unwrap();
        std::fs::write(
            versions::version_dir(ctx, "b", id).join("adapter.gguf"),
            "GGUF",
        )
        .unwrap();
    }

    fn said(f: impl FnOnce(&mut Vec<u8>) -> i32) -> (i32, String) {
        let mut o = Vec::new();
        let c = f(&mut o);
        (c, String::from_utf8(o).unwrap())
    }

    #[test]
    fn promoting_makes_the_model_and_the_conductor_can_find_it() {
        let ctx = test_ctx("p-promote");
        version(&ctx, "20261107-1", true, None);
        let ex = Rec(RefCell::new(vec![]), true);
        let (code, text) =
            said(|o| promote(&ctx, &ex, "b", false, 1_794_000_000, &mut |_| true, o));
        assert_eq!(code, 0, "{text}");
        let day = super::super::versions::list(&ctx, "b")[0].model.clone();
        assert!(day.starts_with("tempo-b-2026"), "{day}");
        let cmd = ex.0.borrow()[0].clone();
        assert!(
            cmd.ends_with(&format!("create {day} -f Modelfile")),
            "{cmd}"
        );
        let mf = std::fs::read_to_string(
            versions::version_dir(&ctx, "b", "20261107-1").join("Modelfile"),
        )
        .unwrap();
        assert!(
            mf.starts_with("FROM qwen3:8b\nADAPTER ") && mf.trim_end().ends_with("adapter.gguf"),
            "{mf}"
        );
        assert_eq!(
            versions::tuned(&ctx, "b"),
            Some(("qwen3:8b".to_string(), day.clone()))
        );
        assert!(
            text.contains("now plays b's parts") && text.contains("rollback b"),
            "{text}"
        );
        // nothing more is waiting
        let (_, text) = said(|o| promote(&ctx, &ex, "b", true, 1_794_000_001, &mut |_| true, o));
        assert!(text.contains("Nothing is waiting"), "{text}");
    }

    #[test]
    fn a_failed_gate_or_a_no_leaves_everything_as_it_was() {
        let ctx = test_ctx("p-no");
        version(&ctx, "v1", false, None);
        let ex = Rec(RefCell::new(vec![]), true);
        let (_, text) = said(|o| promote(&ctx, &ex, "b", true, 1, &mut |_| true, o));
        assert!(
            text.contains("Nothing is waiting") && ex.0.borrow().is_empty(),
            "{text}"
        );
        let ctx = test_ctx("p-no2");
        version(&ctx, "v1", true, None);
        let (_, text) = said(|o| promote(&ctx, &ex, "b", false, 1, &mut |_| false, o));
        assert!(
            text.contains("Not promoted. Nothing was changed.")
                && versions::active(&ctx, "b").is_none()
        );
        // Ollama failing changes nothing either
        let bad = Rec(RefCell::new(vec![]), false);
        let (code, text) = said(|o| promote(&ctx, &bad, "b", true, 1, &mut |_| true, o));
        assert_eq!(code, 1);
        assert!(
            text.contains("no space left") && text.contains("Nothing was changed"),
            "{text}"
        );
        assert!(versions::active(&ctx, "b").is_none());
    }

    #[test]
    fn rollback_is_one_command_and_goes_to_the_parent_or_the_base() {
        let ctx = test_ctx("p-rollback");
        version(&ctx, "v1", true, None);
        version(&ctx, "v2", true, Some("v1"));
        let ex = Rec(RefCell::new(vec![]), true);
        promote(
            &ctx,
            &ex,
            "b",
            true,
            1_794_000_000,
            &mut |_| true,
            &mut Vec::new(),
        );
        assert_eq!(
            versions::active(&ctx, "b").unwrap().version.as_deref(),
            Some("v2"),
            "the newest passing version"
        );
        // v2's parent v1 was never promoted, so there is no model of it: back to the base
        let (code, text) = said(|o| rollback(&ctx, "b", 1_794_000_100, o));
        assert_eq!(code, 0);
        assert!(
            text.contains("plain base") && text.contains("Version v2 is kept"),
            "{text}"
        );
        assert!(versions::tuned(&ctx, "b").is_none());
        // promoting again brings v2 back (it is no longer what plays), and v1 can follow it
        promote(
            &ctx,
            &ex,
            "b",
            true,
            1_794_000_200,
            &mut |_| true,
            &mut Vec::new(),
        );
        assert_eq!(
            versions::active(&ctx, "b").unwrap().version.as_deref(),
            Some("v2")
        );
        let (_, text) = said(|o| rollback(&ctx, "other", 1, o));
        assert!(text.contains("already playing on its plain base"));
        // active.jsonl only grew
        let n = std::fs::read_to_string(versions::root(&ctx, "b").join("active.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(n, 3);
    }

    #[test]
    fn rollback_to_a_promoted_parent_plays_its_model() {
        let ctx = test_ctx("p-rollback2");
        version(&ctx, "v1", true, None);
        let ex = Rec(RefCell::new(vec![]), true);
        promote(
            &ctx,
            &ex,
            "b",
            true,
            1_794_000_000,
            &mut |_| true,
            &mut Vec::new(),
        );
        let m1 = versions::list(&ctx, "b")[0].model.clone();
        version(&ctx, "v2", true, Some("v1"));
        // v1's manifest was saved with its model; saving v2 must not have lost it
        promote(
            &ctx,
            &ex,
            "b",
            true,
            1_794_100_000,
            &mut |_| true,
            &mut Vec::new(),
        );
        let (_, text) = said(|o| rollback(&ctx, "b", 1_794_200_000, o));
        assert!(
            text.contains(&format!("back on version v1 ({m1})")),
            "{text}"
        );
        assert_eq!(versions::tuned(&ctx, "b").unwrap().1, m1);
    }

    #[test]
    fn versions_list_shows_scores_data_hash_parent_and_what_plays() {
        let ctx = test_ctx("p-list");
        let (_, text) = said(|o| list(&ctx, &[], o));
        assert!(text.contains("No versions yet"));
        version(&ctx, "v1", true, None);
        version(&ctx, "v2", false, Some("v1"));
        versions::set_active(&ctx, "b", Some("v1"), Some("tempo-b-x"), "promoted", 1).unwrap();
        let (_, text) = said(|o| list(&ctx, &[], o));
        assert!(
            text.contains("b: 2 versions")
                && text.contains("held-out 30.0 to 40.0")
                && text.contains("cdcdcdcd"),
            "{text}"
        );
        assert!(
            text.contains("v1  2026-11-07") && text.contains("<- playing"),
            "{text}"
        );
        assert!(
            text.contains("parent v1") && text.contains("did not pass"),
            "{text}"
        );
    }
}
