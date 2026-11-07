//! A retune session: one sitting of your AI at the bench, in `<home>/retune/sessions/<id>/`.
//!
//! - `session.json`: the model, where its full-precision weights are, the pass budget, the held-out
//!   set (a path only this program ever opens), when it started.
//! - `budget.json`: forward passes used. Written before a pass runs, so stopping the bench and
//!   starting it again never gives passes back.
//! - `edits.json` and `deltas/`: the edit list. Only the bench helper adds to it; undo (here) moves
//!   the last edit to `undone.jsonl`. The weights themselves are never touched: the list is
//!   replayed on top of the original whenever a tensor is read.
//! - `seen.jsonl`: every practice prompt the bench handed out, so `check` can prove the held-out
//!   set stayed unseen.
//!
//! Nothing here deletes a file.

use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::{file_stamp, stamp, write_atomic, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn root(ctx: &Ctx) -> PathBuf {
    ctx.home.join("retune")
}

/// `qwen3.5:0.8b` becomes `qwen3-5-0-8b`: a folder name that is the same on every system.
pub fn slug(model: &str) -> String {
    let s: String = model
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut out = String::new();
    for c in s.chars() {
        if c != '-' || !out.ends_with('-') {
            out.push(c);
        }
    }
    out.trim_matches('-').to_string()
}

/// A name that cannot leave its folder: letters, digits, dot, dash, underscore.
pub fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[derive(Clone, Debug)]
pub struct Session {
    pub dir: PathBuf,
    pub id: String,
    pub model: String,
    pub weights: PathBuf,
    pub passes: u32,
    pub held_out: Option<PathBuf>,
}

pub const DEFAULT_PASSES: u32 = 50;

impl Session {
    pub fn create(
        ctx: &Ctx,
        model: &str,
        weights: &Path,
        passes: u32,
        held_out: Option<&Path>,
        now: u64,
    ) -> Result<Session, String> {
        let day = &file_stamp(now)[..8];
        let base = format!("{}-{day}", slug(model));
        let sessions = root(ctx).join("sessions");
        let n = (1..)
            .find(|n| !sessions.join(format!("{base}-{n}")).exists())
            .unwrap_or(1);
        let id = format!("{base}-{n}");
        let s = Session {
            dir: sessions.join(&id),
            id,
            model: model.to_string(),
            weights: weights.to_path_buf(),
            passes,
            held_out: held_out.map(Path::to_path_buf),
        };
        std::fs::create_dir_all(s.dir.join("deltas"))
            .map_err(|e| format!("cannot make {}: {e}", s.dir.display()))?;
        let j = J::Obj(vec![
            ("id".into(), J::s(&s.id)),
            ("model".into(), J::s(model)),
            ("weights".into(), J::s(&weights.to_string_lossy())),
            ("passes".into(), J::n(f64::from(passes))),
            (
                "held_out".into(),
                held_out.map_or(J::Null, |h| J::s(&h.to_string_lossy())),
            ),
            ("created".into(), J::s(&stamp(now))),
        ]);
        write_atomic(&s.dir.join("session.json"), j.pretty().as_bytes())
            .map_err(|e| format!("cannot write session.json: {e}"))?;
        s.save_budget(0)?;
        Ok(s)
    }

    pub fn open(ctx: &Ctx, id: &str) -> Result<Session, String> {
        if !plain(id) {
            return Err(format!("\"{id}\" is not a session name"));
        }
        let dir = root(ctx).join("sessions").join(id);
        let text = std::fs::read_to_string(dir.join("session.json")).map_err(|_| {
            format!("No session called {id}. `tempo-launch retune bench <model>` starts one and says its name.")
        })?;
        let j = parse(&text)
            .map_err(|e| format!("session.json in {} is not readable: {e}", dir.display()))?;
        Ok(Session {
            dir,
            id: id.to_string(),
            model: j.str("model").unwrap_or("").to_string(),
            weights: PathBuf::from(j.str("weights").unwrap_or("")),
            passes: j.f64("passes").map_or(DEFAULT_PASSES, |p| p as u32),
            held_out: j.str("held_out").map(PathBuf::from),
        })
    }

    /// Every session, oldest first.
    pub fn all(ctx: &Ctx) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root(ctx).join("sessions"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        v.sort();
        v
    }

    // ---- the budget

    fn budget_file(&self) -> PathBuf {
        self.dir.join("budget.json")
    }

    fn save_budget(&self, used: u32) -> Result<(), String> {
        let j = J::Obj(vec![
            ("limit".into(), J::n(f64::from(self.passes))),
            ("used".into(), J::n(f64::from(used))),
        ]);
        write_atomic(&self.budget_file(), j.pretty().as_bytes())
            .map_err(|e| format!("cannot write the budget: {e}"))
    }

    pub fn used(&self) -> u32 {
        std::fs::read_to_string(self.budget_file())
            .ok()
            .and_then(|t| parse(&t).ok())
            .and_then(|j| j.f64("used"))
            .map_or(0, |u| u as u32)
    }

    pub fn left(&self) -> u32 {
        self.passes.saturating_sub(self.used())
    }

    /// Spend `n` passes, or say there are not that many left. Not given back if the pass then fails.
    pub fn charge(&self, n: u32) -> Result<u32, String> {
        let used = self.used();
        if used + n > self.passes {
            return Err(format!(
                "That needs {n} forward pass{} and {} of {} are left. The owner set the budget (`--passes`); your edits and reads still work.",
                if n == 1 { "" } else { "es" },
                self.left(),
                self.passes
            ));
        }
        self.save_budget(used + n)?;
        Ok(self.passes - used - n)
    }

    // ---- the edit list

    pub fn edits_file(&self) -> PathBuf {
        self.dir.join("edits.json")
    }

    pub fn edits(&self) -> Vec<J> {
        std::fs::read_to_string(self.edits_file())
            .ok()
            .and_then(|t| parse(&t).ok())
            .map(|j| j.arr("edits").to_vec())
            .unwrap_or_default()
    }

    /// SHA-256 of the edit list as it is on disk (each edit names its numbers' own hash).
    pub fn edit_hash(&self) -> String {
        sha256_hex(&std::fs::read(self.edits_file()).unwrap_or_default())
    }

    /// Take the last edit off the list. It moves to `undone.jsonl` with its number files left in
    /// `deltas/`, so nothing is lost and `show` can still list it.
    pub fn undo_last(&self) -> Result<Option<J>, String> {
        let mut e = self.edits();
        let Some(last) = e.pop() else {
            return Ok(None);
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("undone.jsonl"))
            .map_err(|e| format!("cannot open undone.jsonl: {e}"))?;
        writeln!(f, "{}", last.compact()).map_err(|e| format!("cannot write undone.jsonl: {e}"))?;
        let j = J::Obj(vec![("edits".into(), J::Arr(e))]);
        write_atomic(&self.edits_file(), j.pretty().as_bytes())
            .map_err(|e| format!("cannot write the edit list: {e}"))?;
        Ok(Some(last))
    }

    /// One line per edit, for a person.
    pub fn describe(e: &J) -> String {
        let p = e.get("params");
        let detail = match e.str("op").unwrap_or("") {
            "ablate_direction" => format!(
                "{} column{}",
                p.map_or(0, |p| p.arr("columns").len()),
                if p.map_or(0, |p| p.arr("columns").len()) == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            "scale" => format!(
                "x{} on {} {}",
                p.and_then(|p| p.f64("factor")).unwrap_or(0.0),
                p.map_or(0, |p| p.arr("rows").len().max(p.arr("cols").len())),
                if p.is_some_and(|p| !p.arr("rows").is_empty()) {
                    "rows"
                } else {
                    "columns"
                }
            ),
            "add_rank1" => format!("alpha {}", p.and_then(|p| p.f64("alpha")).unwrap_or(0.0)),
            "set_slice" => format!("[{}]", p.and_then(|p| p.str("index")).unwrap_or("")),
            _ => String::new(),
        };
        format!(
            "{:>3}. {} on {} ({detail}), {} weight{} changed",
            e.f64("n").unwrap_or(0.0) as u64,
            e.str("op").unwrap_or("?"),
            e.str("tensor").unwrap_or("?"),
            e.f64("touched").unwrap_or(0.0) as u64,
            if e.f64("touched") == Some(1.0) {
                ""
            } else {
                "s"
            }
        )
    }

    // ---- what the bench handed out

    pub fn note_seen(&self, prompts: &[String]) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("seen.jsonl"))
        {
            for p in prompts {
                let _ = writeln!(f, "{}", J::s(p).compact());
            }
        }
    }

    pub fn seen(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("seen.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| match parse(l) {
                Ok(J::Str(s)) => Some(s),
                _ => None,
            })
            .collect()
    }
}

/// Prompts compared without caring about case or spacing.
pub fn norm(p: &str) -> String {
    p.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The held-out set the owner wrote: one JSON object per line, `{"prompt": ..., "answer": ...}`.
pub fn read_held_out(path: &Path) -> Result<Vec<(String, String)>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the held-out set {}: {e}", path.display()))?;
    let v: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| parse(l.trim()).ok())
        .filter_map(|j| Some((j.str("prompt")?.to_string(), j.str("answer")?.to_string())))
        .collect();
    if v.is_empty() {
        return Err(format!(
            "{} has no lines like {{\"prompt\": \"...\", \"answer\": \"...\"}}",
            path.display()
        ));
    }
    Ok(v)
}
