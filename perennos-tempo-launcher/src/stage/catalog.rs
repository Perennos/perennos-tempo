//! Stage's catalogue: `catalog/stage.json`, embedded. One entry per workflow, each pinned to a
//! commit of the project it came from and to the SHA-256 of its workflow file, with the custom
//! nodes it needs (repo and commit), the models (size, licence, address), the least graphics
//! memory it runs in, what goes in and what comes out, and its Pedals: named presets that each
//! land on named widgets of the graph.

use crate::setup::json::{parse, J};

pub const EMBEDDED: &str = include_str!("../../catalog/stage.json");

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Choice,
    Number,
    Integer,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub id: String,
    pub label: String,
    /// What a prompt-driven Pedal writes into the prompt for this choice.
    pub prompt: Option<String>,
}

/// One word of the shared Pedal vocabulary: a camera orbit is the same Pedal in every workflow.
#[derive(Clone, Debug, PartialEq)]
pub struct PedalWord {
    pub name: String,
    pub kind: Kind,
    pub about: String,
    pub unit: String,
    pub choices: Vec<Choice>,
}

/// A place in the graph: node id, widget name.
#[derive(Clone, Debug, PartialEq)]
pub struct Bind {
    pub node: String,
    pub field: String,
    /// The class the node had when the catalogue was made; a different one means the graph drifted.
    pub class: String,
    pub scale: f64,
    pub offset: f64,
    pub round: bool,
    /// (step, plus): snap to step*n+plus (LTX wants frames as 8n+1).
    pub quantize: Option<(f64, f64)>,
    /// A text input's template, with `{value}` standing for what the owner gave.
    pub template: Option<String>,
}

/// A fixed value for a widget: what a choice or a tier sets.
#[derive(Clone, Debug, PartialEq)]
pub struct Set {
    pub node: String,
    pub field: String,
    pub class: String,
    pub value: J,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Via {
    /// Each choice sets widgets.
    Choices(Vec<(String, Vec<Set>)>),
    /// A number scaled onto widgets.
    Number {
        min: f64,
        max: f64,
        binds: Vec<Bind>,
    },
    /// The vocabulary's phrase for the choice is added to the prompt at these widgets.
    Prompt(Vec<Bind>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pedal {
    pub name: String,
    pub via: Via,
    /// A choice's id or a number as text; None means the Pedal does nothing unless asked.
    pub default: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Input {
    pub name: String,
    /// image, video, audio or text.
    pub kind: String,
    pub required: bool,
    pub default: String,
    pub about: String,
    /// A text input whose `{name}` is replaced wherever it stands in the graph's text.
    pub placeholder: bool,
    pub binds: Vec<Bind>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub node: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tier {
    pub min_vram_gb: f64,
    pub label: String,
    pub sets: Vec<Set>,
    pub tested: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodePack {
    pub name: String,
    pub repo: String,
    pub commit: String,
    pub licence: String,
    /// The folder inside the repo that is the pack (when the repo holds more).
    pub subdir: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub dir: String,
    pub licence: String,
    pub gb: Option<f64>,
    pub url: String,
    pub gated: bool,
    pub sha256: Option<String>,
    pub verified: bool,
    /// `pedal=choice`: only needed when the Pedal is set to that choice.
    pub only_for: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    pub id: String,
    pub title: String,
    pub feature: String,
    pub what: String,
    pub said: String,
    pub repo: String,
    pub commit: String,
    pub licence: String,
    pub url: String,
    pub file: String,
    pub file_sha256: String,
    pub file_bytes: u64,
    pub nodes: Vec<NodePack>,
    pub models: Vec<Model>,
    pub min_vram_gb: f64,
    pub ram_gb: f64,
    pub tested: String,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub pedals: Vec<Pedal>,
    pub tiers: Vec<Tier>,
    /// Partner nodes the graph uses (they call a paid or cloud service); none in the catalogue.
    pub api_nodes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    pub words: Vec<PedalWord>,
    pub workflows: Vec<Workflow>,
}

fn hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The id a tool is named by: `swap-character-video` is `stage_swap_character_video`.
pub fn tool_name(id: &str) -> String {
    format!("stage_{}", id.replace('-', "_"))
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn text(j: &J, k: &str, who: &str) -> Result<String, String> {
    j.str(k)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| format!("{who}: \"{k}\" is missing"))
}

fn value_text(j: &J) -> String {
    match j {
        J::Str(s) => s.clone(),
        J::Num(n) => n.clone(),
        J::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn bind_from(j: &J, who: &str) -> Result<Bind, String> {
    Ok(Bind {
        node: text(j, "node", who)?,
        field: j.str("field").unwrap_or("").to_string(),
        class: j.str("class").unwrap_or("").to_string(),
        scale: j.f64("scale").unwrap_or(1.0),
        offset: j.f64("offset").unwrap_or(0.0),
        round: j.bool("round").unwrap_or(false),
        quantize: match j.arr("quantize") {
            [J::Num(a), J::Num(b)] => a.parse().ok().zip(b.parse().ok()),
            _ => None,
        },
        template: j.str("template").map(String::from),
    })
}

fn set_from(j: &J, who: &str) -> Result<Set, String> {
    Ok(Set {
        node: text(j, "node", who)?,
        field: text(j, "field", who)?,
        class: j.str("class").unwrap_or("").to_string(),
        value: j
            .get("value")
            .cloned()
            .ok_or_else(|| format!("{who}: a set needs a \"value\""))?,
    })
}

fn pedal_from(j: &J, words: &[PedalWord], who: &str) -> Result<Pedal, String> {
    let name = text(j, "name", who)?;
    let who = format!("{who} pedal {name}");
    let word = words
        .iter()
        .find(|w| w.name == name)
        .ok_or_else(|| format!("{who}: not in the vocabulary"))?;
    let via = match j.str("via") {
        Some("choices") => {
            let mut v = vec![];
            if let Some(J::Obj(o)) = j.get("choices") {
                for (k, sets) in o {
                    if !word.choices.is_empty() && !word.choices.iter().any(|c| c.id == *k) {
                        return Err(format!("{who}: \"{k}\" is not one of its choices"));
                    }
                    let J::Arr(a) = sets else {
                        return Err(format!("{who}: \"{k}\" needs a list of sets"));
                    };
                    v.push((
                        k.clone(),
                        a.iter()
                            .map(|s| set_from(s, &who))
                            .collect::<Result<_, _>>()?,
                    ));
                }
            }
            if v.is_empty() {
                return Err(format!("{who}: no choices"));
            }
            Via::Choices(v)
        }
        Some("number") => {
            let binds: Vec<Bind> = j
                .arr("binds")
                .iter()
                .map(|b| bind_from(b, &who))
                .collect::<Result<_, _>>()?;
            if binds.is_empty() {
                return Err(format!("{who}: it lands on nothing"));
            }
            let (min, max) = (
                j.f64("min").ok_or(format!("{who}: \"min\" is missing"))?,
                j.f64("max").ok_or(format!("{who}: \"max\" is missing"))?,
            );
            if min > max {
                return Err(format!("{who}: min is over max"));
            }
            Via::Number { min, max, binds }
        }
        Some("prompt") => {
            if word.choices.iter().any(|c| c.prompt.is_none()) || word.choices.is_empty() {
                return Err(format!("{who}: the vocabulary has no prompt words for it"));
            }
            let binds: Vec<Bind> = j
                .arr("binds")
                .iter()
                .map(|b| bind_from(b, &who))
                .collect::<Result<_, _>>()?;
            if binds.is_empty() {
                return Err(format!("{who}: it lands on nothing"));
            }
            Via::Prompt(binds)
        }
        _ => return Err(format!("{who}: \"via\" is choices, number or prompt")),
    };
    let default = j.get("default").map(value_text).filter(|s| !s.is_empty());
    if let (Some(d), Via::Choices(c)) = (&default, &via) {
        if !c.iter().any(|(k, _)| k == d) {
            return Err(format!("{who}: the default \"{d}\" is not a choice"));
        }
    }
    Ok(Pedal { name, via, default })
}

fn model_from(j: &J, who: &str) -> Result<Model, String> {
    let name = text(j, "name", who)?;
    let who = format!("{who} model {name}");
    let url = text(j, "url", &who)?;
    if !url.starts_with("https://") {
        return Err(format!("{who}: the address must be https"));
    }
    let sha256 = j.str("sha256").map(|s| s.to_ascii_lowercase());
    if sha256.as_deref().is_some_and(|s| !hex(s, 64)) {
        return Err(format!("{who}: the SHA-256 must be 64 hex digits"));
    }
    // a file the catalogue has not hashed must say so
    if sha256.is_none() && j.bool("verified") == Some(true) {
        return Err(format!("{who}: marked verified with no SHA-256"));
    }
    if !plain_name(&name) {
        return Err(format!("{who}: the file name is not plain"));
    }
    Ok(Model {
        dir: text(j, "dir", &who)?,
        licence: text(j, "licence", &who)?,
        gb: j.f64("gb"),
        url,
        gated: j.bool("gated").unwrap_or(false),
        verified: j.bool("verified").unwrap_or(false),
        sha256,
        only_for: j
            .str("for")
            .and_then(|s| s.split_once('='))
            .map(|(a, b)| (a.to_string(), b.to_string())),
        name,
    })
}

/// A file or folder name with no path in it.
pub fn plain_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 120
        && !n.starts_with('.')
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
}

fn workflow_from(j: &J, words: &[PedalWord]) -> Result<Workflow, String> {
    let id = text(j, "id", "a workflow")?;
    if !valid_id(&id) {
        return Err(format!("\"{id}\" is not a plain id (a-z, 0-9, dashes)"));
    }
    let who = id.clone();
    let src = j.get("source").ok_or(format!("{who}: no source"))?;
    let commit = text(src, "commit", &who)?;
    if !hex(&commit, 40) {
        return Err(format!("{who}: the commit must be 40 hex digits"));
    }
    let wf = j
        .get("workflow")
        .ok_or(format!("{who}: no workflow file"))?;
    let sha = text(wf, "sha256", &who)?.to_ascii_lowercase();
    if !hex(&sha, 64) {
        return Err(format!("{who}: the workflow SHA-256 must be 64 hex digits"));
    }
    let nodes = j
        .arr("nodes")
        .iter()
        .map(|n| {
            let name = text(n, "name", &who)?;
            let commit = text(n, "commit", &who)?;
            if !hex(&commit, 40) || !plain_name(&name) {
                return Err(format!(
                    "{who}: node {name} needs a plain name and a 40 digit commit"
                ));
            }
            Ok(NodePack {
                repo: text(n, "repo", &who)?,
                licence: text(n, "licence", &who)?,
                subdir: n.str("subdir").map(String::from),
                name,
                commit,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let inputs = j
        .arr("inputs")
        .iter()
        .map(|i| {
            let name = text(i, "name", &who)?;
            let binds: Vec<Bind> = i
                .arr("binds")
                .iter()
                .map(|b| bind_from(b, &who))
                .collect::<Result<_, _>>()?;
            let placeholder = i.bool("placeholder").unwrap_or(false);
            if binds.is_empty() && !placeholder {
                return Err(format!("{who}: input {name} lands on nothing"));
            }
            Ok(Input {
                kind: text(i, "kind", &who)?,
                required: i.bool("required").unwrap_or(false),
                default: i.str("default").unwrap_or("").to_string(),
                about: i.str("about").unwrap_or("").to_string(),
                placeholder,
                binds,
                name,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if inputs.is_empty() {
        return Err(format!("{who}: no inputs"));
    }
    let outputs = j
        .arr("outputs")
        .iter()
        .map(|o| {
            Ok(Output {
                node: text(o, "node", &who)?,
                kind: text(o, "kind", &who)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if outputs.is_empty() {
        return Err(format!("{who}: no outputs"));
    }
    let tiers = j
        .arr("tiers")
        .iter()
        .map(|t| {
            Ok(Tier {
                min_vram_gb: t
                    .f64("min_vram_gb")
                    .ok_or(format!("{who}: a tier needs min_vram_gb"))?,
                label: text(t, "label", &who)?,
                sets: t
                    .arr("sets")
                    .iter()
                    .map(|s| set_from(s, &who))
                    .collect::<Result<_, String>>()?,
                tested: t.bool("tested").unwrap_or(false),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if tiers.is_empty() {
        return Err(format!("{who}: no tiers"));
    }
    let pedals = j
        .arr("pedals")
        .iter()
        .map(|p| pedal_from(p, words, &who))
        .collect::<Result<Vec<_>, String>>()?;
    let models = j
        .arr("models")
        .iter()
        .map(|m| model_from(m, &who))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Workflow {
        title: text(j, "title", &who)?,
        feature: text(j, "feature", &who)?,
        what: text(j, "what", &who)?,
        said: text(j, "said", &who)?,
        repo: text(src, "repo", &who)?,
        licence: text(src, "licence", &who)?,
        url: text(src, "url", &who)?,
        file: text(wf, "path", &who)?,
        file_sha256: sha,
        file_bytes: wf.f64("bytes").unwrap_or(0.0) as u64,
        min_vram_gb: j
            .f64("min_vram_gb")
            .ok_or(format!("{who}: min_vram_gb is missing"))?,
        ram_gb: j.f64("ram_gb").unwrap_or(0.0),
        tested: j.str("tested").unwrap_or("").to_string(),
        api_nodes: j.strs("api_nodes"),
        id,
        commit,
        nodes,
        models,
        inputs,
        outputs,
        pedals,
        tiers,
    })
}

pub fn parse_catalog(text: &str) -> Result<Catalog, String> {
    let root = parse(text)?;
    let mut words = vec![];
    if let Some(J::Obj(o)) = root.get("pedals") {
        for (name, w) in o {
            let kind = match w.str("kind") {
                Some("choice") => Kind::Choice,
                Some("number") => Kind::Number,
                Some("integer") => Kind::Integer,
                _ => return Err(format!("pedal {name}: kind is choice, number or integer")),
            };
            words.push(PedalWord {
                name: name.clone(),
                kind,
                about: w.str("about").unwrap_or("").to_string(),
                unit: w.str("unit").unwrap_or("").to_string(),
                choices: w
                    .arr("choices")
                    .iter()
                    .filter_map(|c| {
                        Some(Choice {
                            id: c.str("id")?.to_string(),
                            label: c.str("label").unwrap_or("").to_string(),
                            prompt: c.str("prompt").map(String::from),
                        })
                    })
                    .collect(),
            });
        }
    }
    let mut workflows: Vec<Workflow> = vec![];
    for w in root.arr("workflows") {
        let w = workflow_from(w, &words)?;
        if workflows.iter().any(|x| x.id == w.id) {
            return Err(format!("{} is in the catalogue twice", w.id));
        }
        workflows.push(w);
    }
    Ok(Catalog { words, workflows })
}

/// The shipped catalogue. A broken one is a bug in Tempo, caught by a test, so it panics there
/// and is empty here rather than stopping every command.
pub fn embedded() -> Catalog {
    parse_catalog(EMBEDDED).unwrap_or(Catalog {
        words: vec![],
        workflows: vec![],
    })
}

impl Catalog {
    pub fn find(&self, id: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|w| w.id == id)
    }

    pub fn word(&self, name: &str) -> Option<&PedalWord> {
        self.words.iter().find(|w| w.name == name)
    }
}

impl Workflow {
    /// Where the workflow file is fetched from: the raw file at the pinned commit.
    pub fn file_url(&self) -> String {
        format!(
            "https://raw.githubusercontent.com/{}/{}/{}",
            self.repo, self.commit, self.file
        )
    }

    /// The tier a card with `gb` of memory plays: the biggest one it meets.
    pub fn tier_for(&self, gb: f64) -> Option<&Tier> {
        self.tiers
            .iter()
            .filter(|t| gb + 0.25 >= t.min_vram_gb)
            .max_by(|a, b| a.min_vram_gb.total_cmp(&b.min_vram_gb))
    }

    pub fn pedal(&self, name: &str) -> Option<&Pedal> {
        self.pedals.iter().find(|p| p.name == name)
    }

    pub fn input(&self, name: &str) -> Option<&Input> {
        self.inputs.iter().find(|i| i.name == name)
    }

    /// Total download in GB of the models the owner can have fetched (known sizes only), and how
    /// many files have no known size.
    pub fn sizes(&self, with: &[(String, String)]) -> (f64, usize) {
        let mut gb = 0.0;
        let mut unknown = 0;
        for m in self.models_for(with) {
            match m.gb {
                Some(g) => gb += g,
                None => unknown += 1,
            }
        }
        (gb, unknown)
    }

    /// The models needed: those with no condition, and those whose condition is in `with`.
    pub fn models_for<'a>(
        &'a self,
        with: &'a [(String, String)],
    ) -> impl Iterator<Item = &'a Model> {
        self.models.iter().filter(move |m| match &m.only_for {
            None => true,
            Some(c) => with.contains(c),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_catalogue_parses_and_every_pin_is_complete() {
        let c = parse_catalog(EMBEDDED).expect("catalog/stage.json must parse");
        assert!(c.workflows.len() >= 8, "{}", c.workflows.len());
        for w in &c.workflows {
            assert!(hex(&w.commit, 40) && hex(&w.file_sha256, 64), "{}", w.id);
            assert!(w
                .file_url()
                .starts_with("https://raw.githubusercontent.com/"));
            assert!(w.min_vram_gb >= 8.0 && !w.tiers.is_empty(), "{}", w.id);
            assert!(w.api_nodes.is_empty(), "{} uses a paid node", w.id);
            for n in &w.nodes {
                assert!(hex(&n.commit, 40), "{} {}", w.id, n.name);
            }
            // a model with no hash says so, and a hashed one is not a guess
            for m in &w.models {
                assert_eq!(m.verified, m.sha256.is_some(), "{} {}", w.id, m.name);
            }
            // the lowest tier is the least the workflow needs
            let lowest = w
                .tiers
                .iter()
                .map(|t| t.min_vram_gb)
                .fold(f64::MAX, f64::min);
            assert_eq!(lowest, w.min_vram_gb, "{}", w.id);
            assert!(
                w.tier_for(w.min_vram_gb).is_some() && w.tier_for(w.min_vram_gb - 2.0).is_none()
            );
        }
        // one Pedal vocabulary: the camera Pedal means the same in every workflow that has it
        let cam = c.word("camera").unwrap();
        for w in &c.workflows {
            if let Some(Pedal {
                via: Via::Choices(ch),
                ..
            }) = w.pedal("camera")
            {
                for (k, _) in ch {
                    assert!(cam.choices.iter().any(|x| x.id == *k), "{} {k}", w.id);
                }
            }
        }
    }

    #[test]
    fn every_feature_asked_for_has_a_workflow_or_a_reason() {
        let c = embedded();
        for f in [
            "character swap",
            "effect pack",
            "camera moves and cinematic looks",
            "re-angle",
            "paint-to-edit",
            "relight",
            "lip sync",
        ] {
            assert!(c.workflows.iter().any(|w| w.feature == f), "{f}");
        }
    }

    #[test]
    fn a_bad_entry_is_refused_in_words() {
        let good = EMBEDDED;
        let bad_commit = good.replacen(
            "\"commit\": \"d13a284274d13f8d915b147c41e9c07196da2bb5\"",
            "\"commit\": \"main\"",
            1,
        );
        assert!(parse_catalog(&bad_commit).unwrap_err().contains("40 hex"));
        let http = good.replacen("https://huggingface.co", "http://huggingface.co", 1);
        assert!(parse_catalog(&http).unwrap_err().contains("https"));
        let unknown = good.replacen("\"name\": \"lens\"", "\"name\": \"zoomy\"", 1);
        assert!(parse_catalog(&unknown)
            .unwrap_err()
            .contains("not in the vocabulary"));
    }

    #[test]
    fn tool_names_are_plain() {
        assert_eq!(tool_name("re-angle-video"), "stage_re_angle_video");
        assert!(valid_id("a-1") && !valid_id("A") && !valid_id("a_b") && !valid_id(""));
    }
}
