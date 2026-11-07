//! `<home>/crate.json`: what is installed, what Tempo may use, the folders its tools may touch and
//! the scripts the owner has read. The file is kept as it was written (unknown keys stay), so the
//! agent and a person editing it by hand never fight.

use crate::setup::json::{parse, J};
use crate::{write_atomic, Ctx};
use std::path::PathBuf;

pub const FILE: &str = "crate.json";

pub struct State {
    root: J,
}

pub fn path(ctx: &Ctx) -> PathBuf {
    ctx.home.join(FILE)
}

/// The object under `key` of an object, made if it is not there.
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
        self.root.get("tools").and_then(|t| t.get(id))
    }

    pub fn enabled(&self, id: &str) -> bool {
        self.entry(id)
            .and_then(|e| e.bool("enabled"))
            .unwrap_or(false)
    }

    pub fn how(&self, id: &str) -> Option<String> {
        self.entry(id).and_then(|e| e.str("how")).map(String::from)
    }

    /// Change some fields of one tool's record, keeping the others.
    pub fn set(&mut self, id: &str, fields: Vec<(&str, J)>) {
        let tools = obj_mut(&mut self.root, "tools");
        let i = match tools.iter().position(|(k, _)| k == id) {
            Some(i) => i,
            None => {
                tools.push((id.to_string(), J::Obj(vec![])));
                tools.len() - 1
            }
        };
        for (k, v) in fields {
            tools[i].1.set(k, v);
        }
    }

    pub fn enabled_ids(&self) -> Vec<String> {
        match self.root.get("tools") {
            Some(J::Obj(o)) => o
                .iter()
                .filter(|(_, v)| v.bool("enabled") == Some(true))
                .map(|(k, _)| k.clone())
                .collect(),
            _ => vec![],
        }
    }

    pub fn folders(&self) -> Vec<String> {
        self.root.strs("folders")
    }

    pub fn set_folders(&mut self, f: &[String]) {
        self.root.set("folders", J::strings(f));
    }

    /// The SHA-256 of a script the owner approved under this name.
    pub fn reviewed(&self, name: &str) -> Option<String> {
        self.root
            .get("reviewed_scripts")
            .and_then(|r| r.str(name))
            .map(String::from)
    }

    pub fn set_reviewed(&mut self, name: &str, sha: &str) {
        let r = obj_mut(&mut self.root, "reviewed_scripts");
        match r.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = J::s(sha),
            None => r.push((name.to_string(), J::s(sha))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::detect::tests::{ctx_in, tmp};

    #[test]
    fn the_file_keeps_what_it_had_and_remembers_what_changed() {
        let d = tmp("state");
        let ctx = ctx_in(&d);
        std::fs::write(
            path(&ctx),
            r#"{"mine":{"keep":1},"tools":{"ffmpeg":{"enabled":false,"note":"x"}}}"#,
        )
        .unwrap();
        let mut s = State::load(&ctx);
        assert!(!s.enabled("ffmpeg"));
        s.set(
            "ffmpeg",
            vec![("enabled", J::Bool(true)), ("how", J::s("winget"))],
        );
        s.set("sox", vec![("enabled", J::Bool(true))]);
        s.set_folders(&["/a".to_string()]);
        s.set_reviewed("r.py", "abc");
        s.set_reviewed("r.py", "abd");
        s.save(&ctx).unwrap();
        let s = State::load(&ctx);
        assert_eq!(s.enabled_ids(), ["ffmpeg", "sox"]);
        assert_eq!(s.how("ffmpeg").as_deref(), Some("winget"));
        assert_eq!(s.folders(), ["/a"]);
        assert_eq!(s.reviewed("r.py").as_deref(), Some("abd"));
        let text = std::fs::read_to_string(path(&ctx)).unwrap();
        assert!(
            text.contains("\"mine\"") && text.contains("\"note\""),
            "{text}"
        );
    }

    #[test]
    fn a_missing_or_broken_file_is_an_empty_state() {
        let d = tmp("state-empty");
        let ctx = ctx_in(&d);
        assert!(State::load(&ctx).enabled_ids().is_empty());
        std::fs::write(path(&ctx), "not json").unwrap();
        assert!(State::load(&ctx).enabled_ids().is_empty());
    }
}
