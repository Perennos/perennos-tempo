//! Settings from a JSON config file (`--config PATH` or env `TEMPO_STUDIO_CONFIG`). Flags and env
//! vars override it. Nothing machine-specific is built in; with no file the defaults apply.
use crate::master;
use crate::mix::DEFAULT_REFERENCE_LUFS;
use crate::wav::OutFormat;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// REAPER when it is found, else the built-in engine (with a note saying so).
    Auto,
    Reaper,
    Builtin,
}

impl Engine {
    pub fn parse(s: &str) -> Result<Engine, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Engine::Auto),
            "reaper" => Ok(Engine::Reaper),
            "builtin" | "built-in" => Ok(Engine::Builtin),
            other => Err(format!("unknown engine {other} (auto, reaper or builtin)")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Auto => "auto",
            Engine::Reaper => "reaper",
            Engine::Builtin => "builtin",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub reaper_exe: Option<String>,
    pub engine: Engine,
    pub preset: String,
    pub allow_roots: Vec<PathBuf>,
    pub render_timeout_s: u64,
    pub reference_lufs: f64,
    pub output_format: OutFormat,
    /// Unknown keys and similar slips; printed to stderr, never fatal.
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            reaper_exe: None,
            engine: Engine::Auto,
            preset: master::DEFAULT_PRESET.into(),
            allow_roots: Vec::new(),
            // Under the Perennos Tempo local agent's default 600 s MCP call timeout, so a stuck
            // REAPER is reported before the caller gives up.
            render_timeout_s: 540,
            reference_lufs: DEFAULT_REFERENCE_LUFS,
            output_format: OutFormat::Pcm24,
            warnings: Vec::new(),
        }
    }
}

const KEYS: [&str; 7] = [
    "reaper_exe",
    "engine",
    "preset",
    "allow_roots",
    "render_timeout_s",
    "reference_lufs",
    "output_format",
];

impl Config {
    pub fn from_json(v: &Value) -> Result<Config, String> {
        let obj = v.as_object().ok_or("the config must be a JSON object")?;
        let mut c = Config::default();
        for k in obj.keys() {
            if !KEYS.contains(&k.as_str()) && !k.starts_with('_') {
                c.warnings.push(format!(
                    "config key {k} is not used (known: {})",
                    KEYS.join(", ")
                ));
            }
        }
        let text = |k: &str| -> Result<Option<String>, String> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
                Some(_) => Err(format!("config {k} must be a string")),
            }
        };
        c.reaper_exe = text("reaper_exe")?;
        if let Some(e) = text("engine")? {
            c.engine = Engine::parse(&e)?;
        }
        if let Some(p) = text("preset")? {
            master::preset(&p)?;
            c.preset = p.to_ascii_lowercase();
        }
        if let Some(f) = text("output_format")? {
            c.output_format = OutFormat::parse(&f)?;
        }
        match obj.get("allow_roots") {
            None | Some(Value::Null) => {}
            Some(Value::Array(a)) => {
                for r in a {
                    let r = r
                        .as_str()
                        .ok_or("config allow_roots must be a list of folders")?;
                    c.allow_roots.push(PathBuf::from(r));
                }
            }
            Some(_) => return Err("config allow_roots must be a list of folders".into()),
        }
        if let Some(t) = obj.get("render_timeout_s").filter(|x| !x.is_null()) {
            let t = t
                .as_u64()
                .filter(|t| (1..=86_400).contains(t))
                .ok_or("config render_timeout_s must be 1 to 86400 seconds")?;
            c.render_timeout_s = t;
        }
        if let Some(r) = obj.get("reference_lufs").filter(|x| !x.is_null()) {
            let r = r
                .as_f64()
                .filter(|r| (-40.0..=-10.0).contains(r))
                .ok_or("config reference_lufs must be -40 to -10")?;
            c.reference_lufs = r;
        }
        Ok(c)
    }

    /// The file named by the flag, else by env TEMPO_STUDIO_CONFIG, else defaults. A file that
    /// is named but missing or broken is an error (it was asked for).
    pub fn load(flag: Option<&str>, env: Option<String>) -> Result<Config, String> {
        let Some(path) = flag
            .map(String::from)
            .or(env.filter(|s| !s.trim().is_empty()))
        else {
            return Ok(Config::default());
        };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("config {path}: {e}"))?;
        let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| format!("config {path} is not valid JSON: {e}"))?;
        Config::from_json(&v).map_err(|e| format!("config {path}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::tmpdir;
    use serde_json::json;

    #[test]
    fn defaults_file_env_and_errors() {
        let c = Config::load(None, None).unwrap();
        assert_eq!(
            (c.engine, c.preset.as_str(), c.render_timeout_s),
            (Engine::Auto, "streaming", 540)
        );
        let d = tmpdir("cfg");
        let p = d.join("studio.json");
        std::fs::write(
            &p,
            json!({"reaper_exe": "C:/Program Files/REAPER (x64)/reaper.exe", "engine": "builtin", "preset": "Club",
                   "allow_roots": ["D:/Music/out"], "render_timeout_s": 600, "reference_lufs": -20,
                   "output_format": "float32", "colour": "red", "_comment": "ignored"})
            .to_string(),
        )
        .unwrap();
        let c = Config::load(None, Some(p.display().to_string())).unwrap();
        assert_eq!(
            c.reaper_exe.as_deref(),
            Some("C:/Program Files/REAPER (x64)/reaper.exe")
        );
        assert_eq!((c.engine, c.preset.as_str()), (Engine::Builtin, "club"));
        assert_eq!(c.allow_roots, vec![PathBuf::from("D:/Music/out")]);
        assert_eq!(
            (c.render_timeout_s, c.reference_lufs, c.output_format),
            (600, -20.0, OutFormat::Float32)
        );
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        // The flag wins over env; a missing or broken file is an error.
        assert!(
            Config::load(Some("/nope/x.json"), Some(p.display().to_string()))
                .unwrap_err()
                .contains("/nope/x.json")
        );
        std::fs::write(d.join("bad.json"), "{nope").unwrap();
        assert!(
            Config::load(Some(&d.join("bad.json").display().to_string()), None)
                .unwrap_err()
                .contains("not valid JSON")
        );
        for bad in [
            json!({"engine": "magic"}),
            json!({"preset": "loud"}),
            json!({"render_timeout_s": 0}),
            json!({"allow_roots": "D:/x"}),
            json!({"reaper_exe": 3}),
            json!([1]),
        ] {
            assert!(Config::from_json(&bad).is_err(), "{bad}");
        }
        assert_eq!(Engine::parse("Built-in").unwrap(), Engine::Builtin);
    }
}
