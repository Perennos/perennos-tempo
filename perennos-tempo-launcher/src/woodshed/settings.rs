//! `<agent data>/woodshed.json`: the owner's switches. The agent reads the same file to know which
//! bands to keep thumbs for, so it lives in the agent's folder. Keys this program does not know
//! are kept as they were. The old file goes to `<home>/trash/<date>/` before a new one is written.
//!
//! ```json
//! {"allow_cloud_text": false, "margin": 5, "min_examples": 40,
//!  "bands": {"social-media": {"on": true, "base": "qwen3:8b", "hf": "Qwen/Qwen3-8B"}}}
//! ```

use crate::setup::json::{parse, J};
use crate::{file_stamp, write_atomic, Ctx};
use std::path::PathBuf;

pub struct Settings {
    raw: J,
}

pub fn path(ctx: &Ctx) -> PathBuf {
    ctx.agent_data.join("woodshed.json")
}

impl Settings {
    pub fn load(ctx: &Ctx) -> Settings {
        let raw = std::fs::read_to_string(path(ctx))
            .ok()
            .and_then(|t| parse(&t).ok())
            .filter(|j| matches!(j, J::Obj(_)))
            .unwrap_or(J::Obj(vec![]));
        Settings { raw }
    }

    /// Cloud text stays out of training unless the owner says their own contract allows it.
    pub fn allow_cloud_text(&self) -> bool {
        self.raw.bool("allow_cloud_text") == Some(true)
    }

    /// How many points the adapter must win by on the held-out set (default 5).
    pub fn margin(&self) -> f64 {
        self.raw.f64("margin").filter(|m| *m >= 0.0).unwrap_or(5.0)
    }

    /// Fewer winning takes than this and there is nothing worth training on yet (default 40).
    pub fn min_examples(&self) -> usize {
        self.raw
            .f64("min_examples")
            .filter(|m| *m >= 4.0)
            .map_or(40, |m| m as usize)
    }

    /// Most points of canary score the adapter may lose against the base (default 0: none).
    pub fn canary_slack(&self) -> f64 {
        self.raw
            .f64("canary_slack")
            .filter(|m| *m >= 0.0)
            .unwrap_or(0.0)
    }

    /// LoRA rank, 8 to 16.
    pub fn rank(&self) -> u32 {
        self.raw.f64("rank").map_or(16, |r| (r as u32).clamp(8, 16))
    }

    /// Passes over the data, 1 or 2.
    pub fn epochs(&self) -> u32 {
        self.raw.f64("epochs").map_or(2, |r| (r as u32).clamp(1, 2))
    }

    /// Tokens of lessons that go into a prompt.
    pub fn lesson_tokens(&self) -> usize {
        self.raw
            .f64("lesson_tokens")
            .filter(|t| *t >= 50.0)
            .map_or(super::lessons::DEFAULT_TOKENS, |t| t as usize)
    }

    /// Put a passing adapter to work without asking (default: no).
    pub fn auto_promote(&self) -> bool {
        self.raw.bool("auto_promote") == Some(true)
    }

    pub fn text(&self, key: &str) -> Option<String> {
        self.raw
            .str(key)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    fn band(&self, band: &str) -> Option<&J> {
        self.raw.get("bands").and_then(|b| b.get(band))
    }

    pub fn is_on(&self, band: &str) -> bool {
        self.band(band).and_then(|b| b.bool("on")) == Some(true)
    }

    pub fn band_text(&self, band: &str, key: &str) -> Option<String> {
        self.band(band)
            .and_then(|b| b.str(key))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    /// The bands that are on, in the order they were turned on.
    pub fn bands_on(&self) -> Vec<String> {
        match self.raw.get("bands") {
            Some(J::Obj(o)) => o
                .iter()
                .filter(|(_, v)| v.bool("on") == Some(true))
                .map(|(k, _)| k.clone())
                .collect(),
            _ => vec![],
        }
    }

    pub fn set_top(&mut self, key: &str, v: J) {
        self.raw.set(key, v);
    }

    pub fn set_band(&mut self, band: &str, key: &str, v: J) {
        if !matches!(self.raw.get("bands"), Some(J::Obj(_))) {
            self.raw.set("bands", J::Obj(vec![]));
        }
        let mut bands = self.raw.get("bands").cloned().unwrap_or(J::Obj(vec![]));
        if !matches!(bands.get(band), Some(J::Obj(_))) {
            bands.set(band, J::Obj(vec![]));
        }
        let mut b = bands.get(band).cloned().unwrap_or(J::Obj(vec![]));
        b.set(key, v);
        bands.set(band, b);
        self.raw.set("bands", bands);
    }

    pub fn save(&self, ctx: &Ctx, now: u64) -> Result<(), String> {
        let p = path(ctx);
        if let Ok(old) = std::fs::read(&p) {
            let stamp = file_stamp(now);
            let dir = ctx.home.join("trash").join(&stamp[..8]);
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("could not make the trash folder: {e}"))?;
            std::fs::write(dir.join(format!("woodshed-{}.json", &stamp[9..])), old)
                .map_err(|e| format!("could not keep the old settings: {e}"))?;
        }
        write_atomic(&p, self.raw.pretty().as_bytes())
            .map_err(|e| format!("could not write {}: {e}", p.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::takes::test_ctx;
    use super::*;

    #[test]
    fn nothing_is_on_and_cloud_text_is_out_until_the_owner_says() {
        let ctx = test_ctx("s-default");
        let s = Settings::load(&ctx);
        assert!(!s.allow_cloud_text());
        assert!(s.bands_on().is_empty());
        assert_eq!(
            (s.margin(), s.min_examples(), s.rank(), s.epochs()),
            (5.0, 40, 16, 2)
        );
    }

    #[test]
    fn a_band_is_turned_on_and_the_agent_can_read_it() {
        let ctx = test_ctx("s-on");
        let mut s = Settings::load(&ctx);
        s.set_band("social-media", "on", J::Bool(true));
        s.set_band("social-media", "base", J::s("qwen3:8b"));
        s.save(&ctx, 1_800_000_000).unwrap();
        let s = Settings::load(&ctx);
        assert!(s.is_on("social-media") && !s.is_on("dev-team"));
        assert_eq!(
            s.band_text("social-media", "base").as_deref(),
            Some("qwen3:8b")
        );
        assert_eq!(s.bands_on(), ["social-media"]);
        // the file is the one the agent reads
        let raw = std::fs::read_to_string(path(&ctx)).unwrap();
        assert!(raw.contains("\"social-media\"") && raw.contains("\"on\": true"));
    }

    #[test]
    fn unknown_keys_survive_and_the_old_file_goes_to_the_trash() {
        let ctx = test_ctx("s-keep");
        std::fs::create_dir_all(&ctx.agent_data).unwrap();
        std::fs::write(path(&ctx), r#"{"future_key": [1, 2], "margin": 8}"#).unwrap();
        let mut s = Settings::load(&ctx);
        assert_eq!(s.margin(), 8.0);
        s.set_top("allow_cloud_text", J::Bool(true));
        s.save(&ctx, 1_800_000_000).unwrap();
        let raw = std::fs::read_to_string(path(&ctx)).unwrap();
        assert!(raw.contains("future_key") && raw.contains("allow_cloud_text"));
        let trash = ctx.home.join("trash");
        let kept: Vec<_> = std::fs::read_dir(trash.join("20270115")).unwrap().collect();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn numbers_stay_inside_their_ranges() {
        let ctx = test_ctx("s-range");
        std::fs::create_dir_all(&ctx.agent_data).unwrap();
        std::fs::write(
            path(&ctx),
            r#"{"rank": 64, "epochs": 9, "margin": -3, "min_examples": 1}"#,
        )
        .unwrap();
        let s = Settings::load(&ctx);
        assert_eq!(
            (s.rank(), s.epochs(), s.margin(), s.min_examples()),
            (16, 2, 5.0, 40)
        );
        std::fs::write(path(&ctx), r#"{"rank": 2, "epochs": 0}"#).unwrap();
        let s = Settings::load(&ctx);
        assert_eq!((s.rank(), s.epochs()), (8, 1));
    }
}
