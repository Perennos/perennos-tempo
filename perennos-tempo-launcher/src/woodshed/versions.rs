//! Versions: every training run keeps its own folder, `<home>/woodshed/<band>/versions/<id>/`, with
//! the data, the scrub report, the script, the adapter and a `manifest.json` (the base, the data
//! hash, the scores and the parent version). Nothing in there is ever deleted.
//!
//! What plays now is the last line of `<home>/woodshed/<band>/active.jsonl`, which only grows:
//! promoting adds a line, and so does rolling back. A line with no model means the plain base.

use super::eval::{Gate, Scores};
use crate::setup::json::{parse, J};
use crate::{file_stamp, stamp, Ctx};
use std::io::Write;
use std::path::PathBuf;

pub fn root(ctx: &Ctx, band: &str) -> PathBuf {
    ctx.home.join("woodshed").join(band)
}

pub fn version_dir(ctx: &Ctx, band: &str, id: &str) -> PathBuf {
    root(ctx, band).join("versions").join(id)
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Manifest {
    pub version: String,
    pub band: String,
    pub created: String,
    pub base: String,
    pub hf: String,
    pub backend: String,
    pub node: String,
    pub rank: u32,
    pub epochs: u32,
    pub data_hash: String,
    pub train: usize,
    pub valid: usize,
    pub general: usize,
    /// The version that was playing when this one was trained.
    pub parent: Option<String>,
    /// started, trained or failed.
    pub status: String,
    pub note: String,
    pub base_scores: Option<Scores>,
    pub adapter_scores: Option<Scores>,
    pub margin: f64,
    pub gate: Option<Gate>,
    pub model: String,
    pub adapter_file: String,
}

fn scores_j(s: &Option<Scores>) -> J {
    match s {
        None => J::Null,
        Some(s) => J::Obj(vec![
            ("eval".into(), J::n(s.eval)),
            ("n_eval".into(), J::n(s.n_eval as f64)),
            ("canary".into(), J::n(s.canary)),
            ("n_canary".into(), J::n(s.n_canary as f64)),
        ]),
    }
}

fn scores_of(j: Option<&J>) -> Option<Scores> {
    let j = j?;
    Some(Scores {
        eval: j.f64("eval")?,
        n_eval: j.f64("n_eval")? as usize,
        canary: j.f64("canary")?,
        n_canary: j.f64("n_canary")? as usize,
    })
}

impl Manifest {
    pub fn to_j(&self) -> J {
        let n = |v: usize| J::n(v as f64);
        J::Obj(vec![
            ("version".into(), J::s(&self.version)),
            ("band".into(), J::s(&self.band)),
            ("created".into(), J::s(&self.created)),
            ("status".into(), J::s(&self.status)),
            ("note".into(), J::s(&self.note)),
            ("base".into(), J::s(&self.base)),
            ("hf".into(), J::s(&self.hf)),
            ("backend".into(), J::s(&self.backend)),
            ("node".into(), J::s(&self.node)),
            ("rank".into(), J::n(f64::from(self.rank))),
            ("epochs".into(), J::n(f64::from(self.epochs))),
            ("data_hash".into(), J::s(&self.data_hash)),
            (
                "examples".into(),
                J::Obj(vec![
                    ("train".into(), n(self.train)),
                    ("valid".into(), n(self.valid)),
                    ("general".into(), n(self.general)),
                ]),
            ),
            (
                "parent".into(),
                self.parent.as_deref().map_or(J::Null, J::s),
            ),
            (
                "scores".into(),
                J::Obj(vec![
                    ("base".into(), scores_j(&self.base_scores)),
                    ("adapter".into(), scores_j(&self.adapter_scores)),
                ]),
            ),
            ("margin".into(), J::n(self.margin)),
            (
                "gate".into(),
                self.gate.as_ref().map_or(J::Null, |g| {
                    J::Obj(vec![
                        ("passed".into(), J::Bool(g.passed)),
                        ("reason".into(), J::s(&g.reason)),
                    ])
                }),
            ),
            ("model".into(), J::s(&self.model)),
            ("adapter_file".into(), J::s(&self.adapter_file)),
        ])
    }

    pub fn from_j(j: &J) -> Option<Manifest> {
        let s = |k: &str| j.str(k).unwrap_or("").to_string();
        let ex = j.get("examples");
        let count = |k: &str| ex.and_then(|e| e.f64(k)).unwrap_or(0.0) as usize;
        Some(Manifest {
            version: j.str("version").filter(|v| !v.is_empty())?.to_string(),
            band: s("band"),
            created: s("created"),
            base: s("base"),
            hf: s("hf"),
            backend: s("backend"),
            node: s("node"),
            rank: j.f64("rank").unwrap_or(0.0) as u32,
            epochs: j.f64("epochs").unwrap_or(0.0) as u32,
            data_hash: s("data_hash"),
            train: count("train"),
            valid: count("valid"),
            general: count("general"),
            parent: j.str("parent").map(String::from),
            status: s("status"),
            note: s("note"),
            base_scores: scores_of(j.get("scores").and_then(|x| x.get("base"))),
            adapter_scores: scores_of(j.get("scores").and_then(|x| x.get("adapter"))),
            margin: j.f64("margin").unwrap_or(0.0),
            gate: j.get("gate").and_then(|g| {
                Some(Gate {
                    passed: g.bool("passed")?,
                    reason: g.str("reason")?.to_string(),
                })
            }),
            model: s("model"),
            adapter_file: s("adapter_file"),
        })
    }

    pub fn passed(&self) -> bool {
        self.gate.as_ref().is_some_and(|g| g.passed)
    }

    pub fn save(&self, ctx: &Ctx) -> Result<(), String> {
        let dir = version_dir(ctx, &self.band, &self.version);
        crate::write_atomic(&dir.join("manifest.json"), self.to_j().pretty().as_bytes())
            .map_err(|e| format!("cannot write the manifest in {}: {e}", dir.display()))
    }
}

/// Every version of a band, oldest first.
pub fn list(ctx: &Ctx, band: &str) -> Vec<Manifest> {
    let mut v: Vec<Manifest> = std::fs::read_dir(root(ctx, band).join("versions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("manifest.json")).ok())
        .filter_map(|t| parse(&t).ok())
        .filter_map(|j| Manifest::from_j(&j))
        .collect();
    v.sort_by(|a, b| a.version.cmp(&b.version));
    v
}

/// The bands that have any versions.
pub fn bands_with_versions(ctx: &Ctx) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(ctx.home.join("woodshed"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("versions").is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    v.sort();
    v
}

fn date_of(now: u64) -> String {
    file_stamp(now)[..8].to_string()
}

/// `20261107-1`, `20261107-2` ...: the day, and how many versions that day has had.
pub fn next_id(ctx: &Ctx, band: &str, now: u64) -> String {
    let day = date_of(now);
    let taken = std::fs::read_dir(root(ctx, band).join("versions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{day}-"))
        })
        .count();
    format!("{day}-{}", taken + 1)
}

/// `tempo-<band>-<yyyymmdd>`, with `-2`, `-3` when that name was used before (Ollama would
/// silently replace a model made under the same name).
pub fn model_name(band: &str, now: u64, existing: &[Manifest]) -> String {
    let base = format!("tempo-{band}-{}", date_of(now));
    let used = |n: &str| existing.iter().any(|m| m.model == n);
    if !used(&base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !used(n))
        .unwrap_or(base)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Active {
    pub version: Option<String>,
    pub model: Option<String>,
    pub at: String,
}

fn active_file(ctx: &Ctx, band: &str) -> PathBuf {
    root(ctx, band).join("active.jsonl")
}

/// What plays for a band now; None is the plain base.
pub fn active(ctx: &Ctx, band: &str) -> Option<Active> {
    let text = std::fs::read_to_string(active_file(ctx, band)).ok()?;
    let j = text.lines().rev().find_map(|l| parse(l.trim()).ok())?;
    let model = j.str("model").filter(|m| !m.is_empty()).map(String::from);
    Some(Active {
        version: j.str("version").map(String::from),
        model,
        at: j.str("at").unwrap_or("").to_string(),
    })
}

pub fn set_active(
    ctx: &Ctx,
    band: &str,
    version: Option<&str>,
    model: Option<&str>,
    why: &str,
    now: u64,
) -> Result<(), String> {
    let line = J::Obj(vec![
        ("at".into(), J::s(&stamp(now))),
        ("version".into(), version.map_or(J::Null, J::s)),
        ("model".into(), model.map_or(J::Null, J::s)),
        ("why".into(), J::s(why)),
    ])
    .compact();
    let p = active_file(ctx, band);
    std::fs::create_dir_all(root(ctx, band))
        .map_err(|e| format!("cannot make {}: {e}", root(ctx, band).display()))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|e| format!("cannot open {}: {e}", p.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("cannot write {}: {e}", p.display()))
}

/// For the Conductor: the model a band's parts play on now, and the base model it stands in for.
pub fn tuned(ctx: &Ctx, band: &str) -> Option<(String, String)> {
    let a = active(ctx, band)?;
    let (v, model) = (a.version?, a.model?);
    let m = list(ctx, band).into_iter().find(|m| m.version == v)?;
    Some((m.base, model))
}

#[cfg(test)]
mod tests {
    use super::super::takes::test_ctx;
    use super::*;

    fn m(band: &str, id: &str, model: &str) -> Manifest {
        Manifest {
            version: id.into(),
            band: band.into(),
            created: "2026-11-07 02:00:00Z".into(),
            base: "qwen3:8b".into(),
            hf: "Qwen/Qwen3-8B".into(),
            backend: "cuda".into(),
            node: "big-box".into(),
            rank: 16,
            epochs: 2,
            data_hash: "ab".repeat(32),
            train: 85,
            valid: 15,
            general: 20,
            parent: None,
            status: "trained".into(),
            note: String::new(),
            base_scores: Some(Scores {
                eval: 30.0,
                n_eval: 15,
                canary: 75.0,
                n_canary: 12,
            }),
            adapter_scores: Some(Scores {
                eval: 38.5,
                n_eval: 15,
                canary: 75.0,
                n_canary: 12,
            }),
            margin: 5.0,
            gate: Some(Gate {
                passed: true,
                reason: "won".into(),
            }),
            model: model.into(),
            adapter_file: "adapter.gguf".into(),
        }
    }

    #[test]
    fn a_manifest_keeps_the_base_the_data_hash_the_scores_and_the_parent() {
        let ctx = test_ctx("v-manifest");
        let mut a = m("b", "20261107-1", "tempo-b-20261107");
        a.parent = Some("20261106-1".into());
        a.save(&ctx).unwrap();
        let text =
            std::fs::read_to_string(version_dir(&ctx, "b", "20261107-1").join("manifest.json"))
                .unwrap();
        for want in [
            "\"base\": \"qwen3:8b\"",
            "\"data_hash\"",
            "\"parent\": \"20261106-1\"",
            "\"eval\": 38.5",
            "\"passed\": true",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
        let back = list(&ctx, "b");
        assert_eq!(back, vec![a]);
        assert!(back[0].passed());
    }

    #[test]
    fn ids_and_model_names_never_repeat() {
        let ctx = test_ctx("v-ids");
        let now = 1_794_000_000; // 2026-11-06 or 07
        let day = date_of(now);
        assert_eq!(next_id(&ctx, "b", now), format!("{day}-1"));
        m("b", &format!("{day}-1"), &format!("tempo-b-{day}"))
            .save(&ctx)
            .unwrap();
        assert_eq!(next_id(&ctx, "b", now), format!("{day}-2"));
        let all = list(&ctx, "b");
        assert_eq!(model_name("b", now, &all), format!("tempo-b-{day}-2"));
        assert_eq!(model_name("other", now, &all), format!("tempo-other-{day}"));
    }

    #[test]
    fn what_plays_is_the_last_line_and_a_line_with_no_model_means_the_base() {
        let ctx = test_ctx("v-active");
        assert!(active(&ctx, "b").is_none());
        m("b", "v1", "tempo-b-1").save(&ctx).unwrap();
        m("b", "v2", "tempo-b-2").save(&ctx).unwrap();
        set_active(&ctx, "b", Some("v1"), Some("tempo-b-1"), "promoted", 10).unwrap();
        set_active(&ctx, "b", Some("v2"), Some("tempo-b-2"), "promoted", 20).unwrap();
        assert_eq!(
            active(&ctx, "b").unwrap().model.as_deref(),
            Some("tempo-b-2")
        );
        assert_eq!(
            tuned(&ctx, "b"),
            Some(("qwen3:8b".to_string(), "tempo-b-2".to_string()))
        );
        set_active(&ctx, "b", None, None, "rolled back", 30).unwrap();
        assert_eq!(active(&ctx, "b").unwrap().model, None);
        assert_eq!(tuned(&ctx, "b"), None);
        // only grew: three lines, nothing rewritten
        assert_eq!(
            std::fs::read_to_string(active_file(&ctx, "b"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        assert_eq!(bands_with_versions(&ctx), ["b"]);
    }

    #[test]
    fn a_hand_damaged_manifest_is_skipped_not_fatal() {
        let ctx = test_ctx("v-bad");
        m("b", "v1", "x").save(&ctx).unwrap();
        let d = version_dir(&ctx, "b", "v2");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("manifest.json"), "{ broken").unwrap();
        assert_eq!(list(&ctx, "b").len(), 1);
    }
}
