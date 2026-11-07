//! The model catalog: embedded (`catalog/models.json`), with an optional override at
//! `<home>/models/catalog.json` that replaces entries of the same name and adds new ones.

use super::json::{parse, J};
use std::path::Path;

pub const EMBEDDED: &str = include_str!("../../catalog/models.json");
/// Room a model needs on top of its download size (context and working memory), when a catalog
/// entry does not say.
pub const HEADROOM_GB: f64 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Ollama,
    Huggingface,
    /// Perennos's own: used where it already is, never fetched.
    LocalOnly,
}

/// How a model's size and licence were established.
#[derive(Clone, Debug, PartialEq)]
pub enum Proof {
    /// The verified catalog (shipped, or a file on this computer); not re-checked just now.
    Shipped,
    /// The Ollama registry: `None` = just now, `Some(n)` = from the cache, n days old.
    Registry { cached_days: Option<u64> },
    /// Installed here: Ollama's own size, nothing checked with the registry.
    Installed,
    /// Perennos's own model.
    Own,
}

impl Proof {
    pub fn text(&self) -> Option<String> {
        match self {
            Proof::Shipped => {
                Some("From the verified catalog, not re-checked with the Ollama registry".into())
            }
            Proof::Registry { cached_days: None } => {
                Some("Verified with the Ollama registry today".into())
            }
            Proof::Registry {
                cached_days: Some(1),
            } => Some("From the cache, 1 day old (verified with the Ollama registry)".into()),
            Proof::Registry {
                cached_days: Some(n),
            } => Some(format!(
                "From the cache, {n} days old (verified with the Ollama registry)"
            )),
            Proof::Installed => {
                Some("Installed here; size from Ollama, not checked with the registry".into())
            }
            Proof::Own => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub source: Source,
    pub gb: Option<f64>,
    pub params_b: Option<f64>,
    /// Parameters used per answer: below `params_b` for a mixture-of-experts model; None = dense.
    pub active_params_b: Option<f64>,
    pub roles: Vec<String>,
    pub licence: String,
    pub min_ram_gb: Option<f64>,
    pub min_vram_gb: Option<f64>,
    pub opt_in: bool,
    /// What `ollama pull` is given (default: the name).
    pub pull: Option<String>,
    pub link: Option<String>,
    pub notes: String,
    pub proof: Proof,
}

impl Model {
    pub fn has_role(&self, r: &str) -> bool {
        self.roles.iter().any(|x| x == r)
    }

    pub fn pull_name(&self) -> &str {
        self.pull.as_deref().unwrap_or(&self.name)
    }
}

fn model_from(j: &J) -> Result<Model, String> {
    let name = j
        .str("name")
        .filter(|n| !n.trim().is_empty())
        .ok_or("a model has no name")?;
    let source = match j.str("source") {
        Some("ollama") => Source::Ollama,
        Some("huggingface") => Source::Huggingface,
        Some("local-only") => Source::LocalOnly,
        other => return Err(format!("{name}: unknown source {other:?}")),
    };
    let gb = j.f64("gb").filter(|g| *g > 0.0);
    let derived = gb.map(|g| ((g + HEADROOM_GB) * 10.0).round() / 10.0);
    Ok(Model {
        name: name.to_string(),
        source,
        gb,
        params_b: j.f64("params_b").filter(|g| *g > 0.0),
        active_params_b: j.f64("active_params_b").filter(|g| *g > 0.0),
        roles: j.strs("roles"),
        licence: j.str("licence").unwrap_or("unknown").to_string(),
        min_ram_gb: j.f64("min_ram_gb").or(derived),
        min_vram_gb: j.f64("min_vram_gb").or(derived),
        opt_in: j.bool("opt_in").unwrap_or(false),
        pull: j.str("pull").map(String::from),
        link: j.str("link").map(String::from),
        notes: j.str("notes").unwrap_or("").to_string(),
        proof: if source == Source::LocalOnly {
            Proof::Own
        } else {
            Proof::Shipped
        },
    })
}

pub fn parse_catalog(text: &str) -> Result<Vec<Model>, String> {
    let j = parse(text)?;
    j.arr("models").iter().map(model_from).collect()
}

/// The embedded catalog, then the override file if there is one. A broken override is reported
/// (second value) and ignored: the embedded catalog still works.
pub fn load(home: &Path) -> (Vec<Model>, Option<String>) {
    let mut models = parse_catalog(EMBEDDED).unwrap_or_default();
    let path = home.join("models").join("catalog.json");
    let mut warn = None;
    if let Ok(text) = std::fs::read_to_string(&path) {
        match parse_catalog(&text) {
            Ok(extra) => {
                for m in extra {
                    match models.iter_mut().find(|e| e.name == m.name) {
                        Some(slot) => *slot = m,
                        None => models.push(m),
                    }
                }
            }
            Err(e) => warn = Some(format!("{} ignored: {e}", path.display())),
        }
    }
    (models, warn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses_and_has_the_seed_models() {
        let m = parse_catalog(EMBEDDED).unwrap();
        let by = |n: &str| m.iter().find(|x| x.name == n).unwrap();
        assert_eq!(by("qwen3:4b").gb, Some(2.5));
        assert_eq!(by("qwen3:4b").min_ram_gb, Some(4.0));
        assert_eq!(by("embeddinggemma").gb, None);
        assert_eq!(by("embeddinggemma").min_ram_gb, None);
        assert!(by("d1-2B").opt_in);
        assert_eq!(by("d1-2B").source, Source::Huggingface);
        assert_eq!(by("UMBRA-2").source, Source::LocalOnly);
        assert_eq!(by("UMBRA-3").source, Source::LocalOnly);
        assert_eq!(by("UMBRA-3").gb, None);
        // nothing local-only can ever be pulled
        assert!(m
            .iter()
            .filter(|x| x.source == Source::LocalOnly)
            .all(|x| x.pull.is_none()));
    }

    #[test]
    fn min_is_derived_from_gb_when_missing() {
        let m = parse_catalog(r#"{"models":[{"name":"a","source":"ollama","gb":3,"roles":[]}]}"#)
            .unwrap();
        assert_eq!(m[0].min_ram_gb, Some(4.5));
    }

    #[test]
    fn override_replaces_and_adds_and_a_broken_one_is_ignored() {
        let home = std::env::temp_dir().join(format!("tempo-cat-{}", std::process::id()));
        std::fs::create_dir_all(home.join("models")).unwrap();
        std::fs::write(
            home.join("models/catalog.json"),
            r#"{"models":[{"name":"qwen3:4b","source":"ollama","gb":9,"roles":["thinking"]},
            {"name":"new:7b","source":"ollama","gb":4.7,"params_b":7,"roles":["thinking"]}]}"#,
        )
        .unwrap();
        let (m, w) = load(&home);
        assert!(w.is_none());
        assert_eq!(
            m.iter().find(|x| x.name == "qwen3:4b").unwrap().gb,
            Some(9.0)
        );
        assert!(m.iter().any(|x| x.name == "new:7b"));
        std::fs::write(home.join("models/catalog.json"), "{ nope").unwrap();
        let (m, w) = load(&home);
        assert!(w.is_some());
        assert_eq!(
            m.iter().find(|x| x.name == "qwen3:4b").unwrap().gb,
            Some(2.5)
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
