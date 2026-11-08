//! `<home>/stage.json`: what Stage has set up, what the owner turned on, which workflows may use
//! a cloud service (none, until the owner says), and what each run left behind. Kept as written:
//! keys it does not know stay, so a person editing it by hand and the agent never fight.

use crate::setup::json::{parse, J};
use crate::{write_atomic, Ctx};
use std::path::PathBuf;

pub const FILE: &str = "stage.json";

pub struct State {
    root: J,
}

pub fn path(ctx: &Ctx) -> PathBuf {
    ctx.home.join(FILE)
}

fn obj_mut<'a>(parent: &'a mut J, key: &str) -> &'a mut Vec<(String, J)> {
    if !matches!(parent.get(key), Some(J::Obj(_))) {
        parent.set(key, J::Obj(vec![]));
    }
    match parent {
        J::Obj(o) => match o.iter_mut().find(|(k, _)| k == key) {
            Some((_, J::Obj(inner))) => inner,
            _ => unreachable!("just made"),
        },
        _ => unreachable!("the root is always an object"),
    }
}

impl State {
    pub fn load(ctx: &Ctx) -> State {
        let root = std::fs::read_to_string(path(ctx))
            .ok()
            .and_then(|t| parse(&t).ok())
            .filter(|j| matches!(j, J::Obj(_)))
            .unwrap_or(J::Obj(vec![]));
        State { root }
    }

    pub fn save(&self, ctx: &Ctx) -> Result<(), String> {
        write_atomic(&path(ctx), self.root.pretty().as_bytes())
            .map_err(|e| format!("could not write {}: {e}", path(ctx).display()))
    }

    fn entry(&self, id: &str) -> Option<&J> {
        self.root.get("workflows").and_then(|w| w.get(id))
    }

    pub fn installed(&self, id: &str) -> bool {
        self.entry(id)
            .and_then(|e| e.bool("installed"))
            .unwrap_or(false)
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.installed(id)
            && self
                .entry(id)
                .and_then(|e| e.bool("enabled"))
                .unwrap_or(false)
    }

    /// Whether the owner let this workflow use a paid or cloud service.
    pub fn cloud_ok(&self, id: &str) -> bool {
        self.entry(id)
            .and_then(|e| e.bool("allow_cloud"))
            .unwrap_or(false)
    }

    /// The facts about one workflow's record (`workflow_sha256`, `nodes`, `models`, ...).
    pub fn record(&self, id: &str) -> Option<&J> {
        self.entry(id)
    }

    pub fn set(&mut self, id: &str, fields: Vec<(&str, J)>) {
        let all = obj_mut(&mut self.root, "workflows");
        let i = match all.iter().position(|(k, _)| k == id) {
            Some(i) => i,
            None => {
                all.push((id.to_string(), J::Obj(vec![])));
                all.len() - 1
            }
        };
        for (k, v) in fields {
            all[i].1.set(k, v);
        }
    }

    pub fn forget(&mut self, id: &str) {
        obj_mut(&mut self.root, "workflows").retain(|(k, _)| k != id);
    }

    pub fn enabled_ids(&self) -> Vec<String> {
        match self.root.get("workflows") {
            Some(J::Obj(o)) => o
                .iter()
                .filter(|(_, v)| {
                    v.bool("installed") == Some(true) && v.bool("enabled") == Some(true)
                })
                .map(|(k, _)| k.clone())
                .collect(),
            _ => vec![],
        }
    }

    pub fn installed_ids(&self) -> Vec<String> {
        match self.root.get("workflows") {
            Some(J::Obj(o)) => o
                .iter()
                .filter(|(_, v)| v.bool("installed") == Some(true))
                .map(|(k, _)| k.clone())
                .collect(),
            _ => vec![],
        }
    }

    /// The ComfyUI folder the owner pointed Stage at.
    pub fn folder(&self) -> Option<String> {
        self.root.str("comfy_dir").map(String::from)
    }

    pub fn set_folder(&mut self, dir: &str) {
        self.root.set("comfy_dir", J::s(dir));
    }

    /// The custom node folders Stage put there itself (so only those can be removed).
    pub fn node_owner(&self, node: &str) -> Vec<String> {
        match self.root.get("workflows") {
            Some(J::Obj(o)) => o
                .iter()
                .filter(|(_, v)| v.bool("installed") == Some(true))
                .filter(|(_, v)| v.get("nodes").and_then(|n| n.get(node)).is_some())
                .map(|(k, _)| k.clone())
                .collect(),
            _ => vec![],
        }
    }

    /// The SHA-256 Stage recorded for a model file when it first saw it.
    pub fn model_sha(&self, name: &str) -> Option<String> {
        match self.root.get("model_hashes") {
            Some(h) => h.str(name).map(String::from),
            None => None,
        }
    }

    pub fn set_model_sha(&mut self, name: &str, sha: &str) {
        let h = obj_mut(&mut self.root, "model_hashes");
        match h.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = J::s(sha),
            None => h.push((name.to_string(), J::s(sha))),
        }
    }

    /// The last run of a workflow: inputs, Pedals and seed, for `--again`.
    pub fn last_run(&self, id: &str) -> Option<&J> {
        self.root.get("last_run").and_then(|l| l.get(id))
    }

    pub fn set_last_run(&mut self, id: &str, run: J) {
        let l = obj_mut(&mut self.root, "last_run");
        match l.iter_mut().find(|(k, _)| k == id) {
            Some(slot) => slot.1 = run,
            None => l.push((id.to_string(), run)),
        }
    }

    pub fn last_workflow(&self) -> Option<String> {
        self.root.str("last_workflow").map(String::from)
    }

    pub fn set_last_workflow(&mut self, id: &str) {
        self.root.set("last_workflow", J::s(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::detect::tests::{ctx_in, tmp};

    #[test]
    fn the_file_keeps_what_it_had_and_remembers_what_changed() {
        let d = tmp("stage-state");
        let ctx = ctx_in(&d);
        std::fs::write(
            path(&ctx),
            r#"{"mine":{"keep":1},"workflows":{"a":{"installed":true,"enabled":false,"note":"x"}}}"#,
        )
        .unwrap();
        let mut s = State::load(&ctx);
        assert!(s.installed("a") && !s.enabled("a") && !s.cloud_ok("a"));
        s.set("a", vec![("enabled", J::Bool(true))]);
        s.set("b", vec![("installed", J::Bool(true))]);
        s.set_folder("/x/ComfyUI");
        s.set_model_sha("m.safetensors", "ab");
        s.set_last_run("a", J::Obj(vec![("seed".into(), J::n(7.0))]));
        s.save(&ctx).unwrap();
        let s = State::load(&ctx);
        assert_eq!(s.enabled_ids(), ["a"]);
        assert_eq!(s.installed_ids(), ["a", "b"]);
        assert_eq!(s.folder().as_deref(), Some("/x/ComfyUI"));
        assert_eq!(s.model_sha("m.safetensors").as_deref(), Some("ab"));
        assert_eq!(s.last_run("a").unwrap().f64("seed"), Some(7.0));
        let text = std::fs::read_to_string(path(&ctx)).unwrap();
        assert!(
            text.contains("\"mine\"") && text.contains("\"note\""),
            "{text}"
        );
    }

    #[test]
    fn a_missing_or_broken_file_is_an_empty_state() {
        let d = tmp("stage-state-empty");
        let ctx = ctx_in(&d);
        assert!(State::load(&ctx).enabled_ids().is_empty());
        std::fs::write(path(&ctx), "not json").unwrap();
        assert!(State::load(&ctx).installed_ids().is_empty());
    }

    #[test]
    fn forgetting_a_workflow_drops_only_it() {
        let d = tmp("stage-forget");
        let ctx = ctx_in(&d);
        let mut s = State::load(&ctx);
        s.set("a", vec![("installed", J::Bool(true))]);
        s.set("b", vec![("installed", J::Bool(true))]);
        s.forget("a");
        assert_eq!(s.installed_ids(), ["b"]);
    }
}
