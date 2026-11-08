//! How a sectioned run behaves: its budget, how many takes a part gets, the repo checks and which
//! engines count as cloud. Plain JSON in `<Tempo home>/conductor.json`, and a band's own
//! `bands/<band>/conductor.json` on top of it. Every key is optional; an unknown key or a wrong
//! type is a problem said in words, never silently dropped.

use super::budget::Budget;
use crate::setup::json::{parse, J};
use crate::Ctx;
use std::path::PathBuf;
use std::time::Duration;

pub const MAX_TAKES: u8 = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub budget: Budget,
    /// Attempts per part: 1 by default, 2 or 3 when the owner asks for the best of a few.
    pub takes: u8,
    /// Engine ids that are not on the owner's own machines. Nothing is cloud unless listed here.
    pub cloud_engines: Vec<String>,
    /// What a cloud engine costs per 1,000 tokens, in cents. Needed to count `cloud_cents`.
    pub cloud_cents_per_1k: f64,
    /// The repo's own checks, run in an isolated copy of `workdir`: a program and its arguments.
    pub check: Vec<String>,
    pub workdir: Option<PathBuf>,
    /// Also run Soundcheck's quick scanners over a take that wrote files.
    pub soundcheck: bool,
    /// How long one check may run.
    pub check_timeout: Duration,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            budget: Budget::default(),
            takes: 1,
            cloud_engines: vec![],
            cloud_cents_per_1k: 0.0,
            check: vec![],
            workdir: None,
            soundcheck: true,
            check_timeout: Duration::from_secs(300),
        }
    }
}

const KEYS: &[&str] = &[
    "budget_tokens",
    "budget_wall_s",
    "budget_tool_calls",
    "budget_cloud_cents",
    "takes",
    "cloud_engines",
    "cloud_cents_per_1k_tokens",
    "check",
    "workdir",
    "soundcheck",
    "check_timeout_s",
];

impl Settings {
    /// Lay the keys of one file over what is already set.
    pub fn apply(&mut self, text: &str, from: &str) -> Result<(), String> {
        let j = parse(text).map_err(|e| format!("{from}: not JSON ({e})"))?;
        let J::Obj(o) = &j else {
            return Err(format!("{from}: should be a JSON object"));
        };
        let whole = |k: &str| -> Result<Option<u64>, String> {
            match j.get(k) {
                None => Ok(None),
                Some(_) => j
                    .f64(k)
                    .filter(|n| *n >= 0.0 && n.fract() == 0.0)
                    .map(|n| Some(n as u64))
                    .ok_or_else(|| format!("{from}: {k} should be a whole number (0 is no limit)")),
            }
        };
        for (k, _) in o {
            if !KEYS.contains(&k.as_str()) {
                return Err(format!(
                    "{from}: {k} is not a setting I know. The settings are {}",
                    KEYS.join(", ")
                ));
            }
        }
        if let Some(n) = whole("budget_tokens")? {
            self.budget.tokens = n;
        }
        if let Some(n) = whole("budget_wall_s")? {
            self.budget.wall_time = Duration::from_secs(n);
        }
        if let Some(n) = whole("budget_tool_calls")? {
            self.budget.tool_calls = n;
        }
        if let Some(n) = whole("budget_cloud_cents")? {
            self.budget.cloud_cents = n;
        }
        if let Some(n) = whole("check_timeout_s")? {
            self.check_timeout = Duration::from_secs(n.clamp(1, 3600));
        }
        if let Some(n) = whole("takes")? {
            if n == 0 || n > u64::from(MAX_TAKES) {
                return Err(format!("{from}: takes is 1 to {MAX_TAKES}"));
            }
            self.takes = n as u8;
        }
        if j.get("cloud_engines").is_some() {
            self.cloud_engines = j.strs("cloud_engines");
        }
        if j.get("cloud_cents_per_1k_tokens").is_some() {
            self.cloud_cents_per_1k = j
                .f64("cloud_cents_per_1k_tokens")
                .filter(|n| *n >= 0.0)
                .ok_or_else(|| format!("{from}: cloud_cents_per_1k_tokens should be a number"))?;
        }
        if j.get("check").is_some() {
            let c = j.str("check").ok_or_else(|| {
                format!("{from}: check should be text, like \"cargo test --locked\"")
            })?;
            self.check = c.split_whitespace().map(String::from).collect();
        }
        if j.get("workdir").is_some() {
            let w = j
                .str("workdir")
                .filter(|w| !w.trim().is_empty())
                .ok_or_else(|| format!("{from}: workdir should be a folder"))?;
            self.workdir = Some(PathBuf::from(w));
        }
        if j.get("soundcheck").is_some() {
            self.soundcheck = j
                .bool("soundcheck")
                .ok_or_else(|| format!("{from}: soundcheck should be true or false"))?;
        }
        Ok(())
    }

    /// Defaults, then the home's file, then the band's.
    pub fn load(ctx: &Ctx, band: &str) -> Result<Settings, String> {
        let mut s = Settings::default();
        for file in [
            ctx.home.join("conductor.json"),
            super::band_dir(ctx, band).join("conductor.json"),
        ] {
            if let Ok(text) = std::fs::read_to_string(&file) {
                s.apply(&text, &file.display().to_string())?;
            }
        }
        Ok(s)
    }

    pub fn is_cloud(&self, engine_id: &str) -> bool {
        !engine_id.is_empty() && self.cloud_engines.iter().any(|c| c == engine_id)
    }
}

/// A rough count, four characters to a token: the agent does not hand the Conductor real counts.
pub fn tokens_of(text: &str) -> u64 {
    text.chars().count() as u64 / 4 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible_and_files_layer_on_top() {
        let mut s = Settings::default();
        assert_eq!(
            (s.takes, s.budget.tool_calls, s.budget.cloud_cents),
            (1, 200, 50)
        );
        s.apply(
            r#"{"takes":2,"budget_tokens":1000,"check":"cargo test --locked"}"#,
            "home",
        )
        .unwrap();
        s.apply(r#"{"budget_tokens":0,"cloud_engines":["far"]}"#, "band")
            .unwrap();
        assert_eq!((s.takes, s.budget.tokens), (2, 0));
        assert_eq!(s.check, ["cargo", "test", "--locked"]);
        assert!(s.is_cloud("far") && !s.is_cloud("local") && !s.is_cloud(""));
    }

    #[test]
    fn mistakes_are_said_in_words() {
        let bad = |t: &str| Settings::default().apply(t, "f").unwrap_err();
        assert!(bad(r#"{"takez":2}"#).contains("takez is not a setting I know"));
        assert!(bad(r#"{"takes":9}"#).contains("takes is 1 to 3"));
        assert!(bad(r#"{"budget_tokens":-5}"#).contains("whole number"));
        assert!(bad(r#"{"check":5}"#).contains("check should be text"));
        assert!(bad("[1]").contains("JSON object"));
        assert!(bad("nope").contains("not JSON"));
    }
}
