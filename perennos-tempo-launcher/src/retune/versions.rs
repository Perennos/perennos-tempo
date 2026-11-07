//! Retune versions: every `retune check` keeps a folder, `<home>/retune/versions/<model>/<id>/`,
//! with a `manifest.json` that names the edit list's hash, how many weights changed (count and
//! share of the model) and the scores. What plays now is the last line of `active.jsonl`, which
//! only grows: promoting adds a line, and so does `retune undo`. Nothing is ever deleted.

use super::session::{root, slug};
use crate::setup::json::{parse, J};
use crate::woodshed::eval::{Gate, Scores};
use crate::woodshed::versions::{scores_j, scores_of};
use crate::{file_stamp, stamp, write_atomic, Ctx};
use std::io::Write;
use std::path::PathBuf;

pub fn dir(ctx: &Ctx, model: &str) -> PathBuf {
    root(ctx).join("versions").join(slug(model))
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Manifest {
    pub version: String,
    /// The Ollama model the retune started from.
    pub base: String,
    pub session: String,
    pub created: String,
    pub edit_hash: String,
    pub edits: usize,
    pub changed: u64,
    pub total: u64,
    pub outtype: String,
    pub base_scores: Option<Scores>,
    pub tuned_scores: Option<Scores>,
    /// The owner's own quantised Ollama copy of the base, scored for the record. The decision
    /// uses `base_scores`, from the baseline built through the same path as the retune.
    pub ollama_scores: Option<Scores>,
    /// The Ollama model the baseline was scored as.
    pub baseline: String,
    pub margin: f64,
    pub gate: Option<Gate>,
    /// The Ollama model made for it; set when it passed and was put to work.
    pub model: String,
    pub status: String,
    pub note: String,
}

impl Manifest {
    pub fn passed(&self) -> bool {
        self.gate.as_ref().is_some_and(|g| g.passed)
    }

    /// Share of the model's weights that changed, in percent.
    pub fn percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.changed as f64 * 100.0 / self.total as f64
        }
    }

    pub fn to_j(&self) -> J {
        let s = |v: &str| J::s(v);
        J::Obj(vec![
            ("version".into(), s(&self.version)),
            ("base".into(), s(&self.base)),
            ("session".into(), s(&self.session)),
            ("created".into(), s(&self.created)),
            ("status".into(), s(&self.status)),
            ("note".into(), s(&self.note)),
            ("edit_hash".into(), s(&self.edit_hash)),
            ("edits".into(), J::n(self.edits as f64)),
            (
                "weights".into(),
                J::Obj(vec![
                    ("changed".into(), J::n(self.changed as f64)),
                    ("total".into(), J::n(self.total as f64)),
                    (
                        "percent".into(),
                        J::n((self.percent() * 10000.0).round() / 10000.0),
                    ),
                ]),
            ),
            ("outtype".into(), s(&self.outtype)),
            (
                "scores".into(),
                J::Obj(vec![
                    ("base".into(), scores_j(&self.base_scores)),
                    ("retuned".into(), scores_j(&self.tuned_scores)),
                    ("ollama_base".into(), scores_j(&self.ollama_scores)),
                ]),
            ),
            ("margin".into(), J::n(self.margin)),
            (
                "gate".into(),
                self.gate.as_ref().map_or(J::Null, |g| {
                    J::Obj(vec![
                        ("passed".into(), J::Bool(g.passed)),
                        ("reason".into(), s(&g.reason)),
                    ])
                }),
            ),
            ("baseline".into(), s(&self.baseline)),
            ("model".into(), s(&self.model)),
        ])
    }

    pub fn from_j(j: &J) -> Option<Manifest> {
        let s = |k: &str| j.str(k).unwrap_or("").to_string();
        let w = j.get("weights");
        Some(Manifest {
            version: j.str("version").filter(|v| !v.is_empty())?.to_string(),
            base: s("base"),
            session: s("session"),
            created: s("created"),
            edit_hash: s("edit_hash"),
            edits: j.f64("edits").unwrap_or(0.0) as usize,
            changed: w.and_then(|w| w.f64("changed")).unwrap_or(0.0) as u64,
            total: w.and_then(|w| w.f64("total")).unwrap_or(0.0) as u64,
            outtype: s("outtype"),
            base_scores: scores_of(j.get("scores").and_then(|x| x.get("base"))),
            tuned_scores: scores_of(j.get("scores").and_then(|x| x.get("retuned"))),
            ollama_scores: scores_of(j.get("scores").and_then(|x| x.get("ollama_base"))),
            baseline: s("baseline"),
            margin: j.f64("margin").unwrap_or(0.0),
            gate: j.get("gate").and_then(|g| {
                Some(Gate {
                    passed: g.bool("passed")?,
                    reason: g.str("reason")?.to_string(),
                })
            }),
            model: s("model"),
            status: s("status"),
            note: s("note"),
        })
    }

    pub fn save(&self, ctx: &Ctx) -> Result<(), String> {
        let d = dir(ctx, &self.base).join(&self.version);
        std::fs::create_dir_all(&d).map_err(|e| format!("cannot make {}: {e}", d.display()))?;
        write_atomic(&d.join("manifest.json"), self.to_j().pretty().as_bytes())
            .map_err(|e| format!("cannot write the manifest in {}: {e}", d.display()))
    }
}

/// Every version of a model, oldest first.
pub fn list(ctx: &Ctx, model: &str) -> Vec<Manifest> {
    let mut v: Vec<Manifest> = std::fs::read_dir(dir(ctx, model))
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

/// The model folders that have any versions, as the Ollama names the manifests carry.
pub fn models(ctx: &Ctx) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(root(ctx).join("versions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            std::fs::read_dir(e.path())
                .ok()?
                .flatten()
                .find_map(|v| std::fs::read_to_string(v.path().join("manifest.json")).ok())
        })
        .filter_map(|t| parse(&t).ok())
        .filter_map(|j| j.str("base").map(String::from))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// `20261107-1`, `20261107-2` ...
pub fn next_id(ctx: &Ctx, model: &str, now: u64) -> String {
    let day = &file_stamp(now)[..8];
    let n = std::fs::read_dir(dir(ctx, model))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{day}-"))
        })
        .count();
    format!("{day}-{}", n + 1)
}

/// `tempo-<model>-retuned-<yyyymmdd>`, with `-2`, `-3` when it was used before (Ollama would
/// quietly replace a model made under the same name).
pub fn model_name(model: &str, now: u64, existing: &[Manifest]) -> String {
    let base = format!("tempo-{}-retuned-{}", slug(model), &file_stamp(now)[..8]);
    let used = |n: &str| existing.iter().any(|m| m.model == n);
    if !used(&base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !used(n))
        .unwrap_or(base)
}

fn active_file(ctx: &Ctx, model: &str) -> PathBuf {
    dir(ctx, model).join("active.jsonl")
}

/// What plays for a model now: (version, Ollama model). None is the plain base.
pub fn active(ctx: &Ctx, model: &str) -> Option<(String, String)> {
    let text = std::fs::read_to_string(active_file(ctx, model)).ok()?;
    let j = text.lines().rev().find_map(|l| parse(l.trim()).ok())?;
    Some((
        j.str("version")?.to_string(),
        j.str("model").filter(|m| !m.is_empty())?.to_string(),
    ))
}

pub fn set_active(
    ctx: &Ctx,
    model: &str,
    version: Option<&str>,
    ollama: Option<&str>,
    why: &str,
    now: u64,
) -> Result<(), String> {
    let line = J::Obj(vec![
        ("at".into(), J::s(&stamp(now))),
        ("version".into(), version.map_or(J::Null, J::s)),
        ("model".into(), ollama.map_or(J::Null, J::s)),
        ("why".into(), J::s(why)),
    ])
    .compact();
    std::fs::create_dir_all(dir(ctx, model)).map_err(|e| format!("cannot make the folder: {e}"))?;
    let p = active_file(ctx, model);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|e| format!("cannot open {}: {e}", p.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("cannot write {}: {e}", p.display()))
}
