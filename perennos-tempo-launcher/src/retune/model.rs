//! What Retune needs to know about a model before it edits it: where its full-precision weights
//! are on Hugging Face, what its licence says about changing it, how big the download is, which
//! of your computers should run the bench, and how long a forward pass will honestly take.
//!
//! Ollama's models are quantised GGUF files, and an edit needs the original numbers, so Retune
//! works from the safetensors weights. Everything here is a guess from the model's name until the
//! owner says otherwise (`--hf REPO`, `--weights FOLDER`).

use super::session::{root, slug};
use crate::fleet::store::Fleet;
use crate::setup::probe::{self, Gpu, Host};
use crate::woodshed::exec::Where;
use crate::woodshed::node;
use crate::woodshed::script;
use crate::Ctx;
use std::path::{Path, PathBuf};

/// `0.8b` is 0.8 billion, `270m` is 0.27 billion: the size in an Ollama tag.
pub fn params_b(model: &str) -> Option<f64> {
    let tag = model.split_once(':')?.1.to_ascii_lowercase();
    let size = tag.split(['-', '_']).next()?;
    if let Some(b) = size.strip_suffix('b') {
        return b.parse().ok().filter(|v: &f64| *v > 0.0);
    }
    size.strip_suffix('m')?
        .parse::<f64>()
        .ok()
        .filter(|v| *v > 0.0)
        .map(|m| m / 1000.0)
}

/// The Hugging Face repo of an Ollama tag of a family Tempo knows (not checked against the Hub).
pub fn hf_repo(model: &str) -> Option<String> {
    let (name, tag) = model.split_once(':')?;
    let size = tag.split(['-', '_']).next()?.to_ascii_lowercase();
    let big = size.to_ascii_uppercase();
    match name {
        "qwen3.5" => Some(format!("Qwen/Qwen3.5-{big}")),
        "gemma3" => Some(format!("google/gemma-3-{size}-it")),
        _ => script::hf_for(model),
    }
}

pub struct Licence {
    pub name: &'static str,
    /// Some(true): modifying is allowed. None: Tempo does not know.
    pub modify_ok: Option<bool>,
    pub words: &'static str,
}

pub fn licence(model: &str) -> Licence {
    let family = model
        .split([':', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let family = if model.contains('/') {
        model.rsplit('/').next().unwrap_or("").to_ascii_lowercase()
    } else {
        family
    };
    if family.starts_with("qwen") {
        Licence {
            name: "Apache-2.0",
            modify_ok: Some(true),
            words: "Apache-2.0 lets you change the weights and share the result with its licence and notices. A few Qwen sizes carry the Qwen licence instead, so read your size's model card.",
        }
    } else if family.starts_with("gemma") {
        Licence {
            name: "Gemma Terms of Use",
            modify_ok: Some(true),
            words: "Gemma's terms allow changing the model for your own use. If you share the result it carries Gemma's terms and use restrictions with it. The download on Hugging Face asks you to accept them first.",
        }
    } else if family.starts_with("llama") {
        Licence {
            name: "Llama Community Licence",
            modify_ok: Some(true),
            words: "The Llama community licence allows changing the model. If you share the result it carries the licence and the Llama naming rules, and very large services need a licence from Meta. The download on Hugging Face asks you to accept it first.",
        }
    } else {
        Licence {
            name: "unknown",
            modify_ok: None,
            words: "Tempo does not know this model's licence. Read its model card before you change it, and before you share the result.",
        }
    }
}

/// Download size of the bf16 weights in GB, from the parameter count (an estimate).
pub fn weights_gb(params_b: f64) -> f64 {
    (params_b * 2.0 * 10.0).round() / 10.0
}

/// Where the full-precision weights might already be: Tempo's own folder, then the Hugging Face cache.
pub fn find_weights(ctx: &Ctx, model: &str, hf: Option<&str>) -> Option<PathBuf> {
    let has = |p: &Path| {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".safetensors"))
    };
    let own = root(ctx).join("weights").join(slug(model));
    if has(&own) {
        return Some(own);
    }
    let cache = std::env::var_os("HUGGINGFACE_HUB_CACHE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HF_HOME").map(|h| PathBuf::from(h).join("hub")))
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".cache/huggingface/hub"))
        })?;
    let snaps = cache
        .join(format!("models--{}", hf?.replace('/', "--")))
        .join("snapshots");
    let mut found: Vec<PathBuf> = std::fs::read_dir(snaps)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| has(p))
        .collect();
    found.sort();
    found.pop()
}

// ---- which computer

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub name: String,
    pub at: Where,
    pub ram_gb: f64,
    pub cpus: usize,
    /// Name and graphics memory in GB.
    pub gpu: Option<(String, f64)>,
}

pub fn local_machine(host: &dyn Host) -> Machine {
    let ram = probe::probe_memory(host).0.unwrap_or(0.0);
    let gpu = match probe::probe_gpu(host) {
        Gpu::Nvidia { name, total_gb, .. } => Some((name, total_gb)),
        Gpu::Apple { chip } => Some((chip, (ram * 0.65 * 10.0).round() / 10.0)),
        Gpu::None => None,
    };
    Machine {
        name: "this computer".into(),
        at: Where::Local,
        ram_gb: (ram * 10.0).round() / 10.0,
        cpus: host.cpus(),
        gpu,
    }
}

/// The fleet's machines that your own ssh reaches.
pub fn fleet_machines(f: &Fleet, hosts: &[String]) -> Vec<Machine> {
    f.peers()
        .filter(|m| m.ssh.bool("ok") == Some(true))
        .map(|m| {
            let target = if hosts.contains(&m.name) {
                m.name.clone()
            } else {
                m.addr.clone()
            };
            Machine {
                name: m.name.clone(),
                gpu: node::node_of(&m.name, target.clone(), &m.probe).map(|n| (n.what, n.gb)),
                at: Where::Ssh(target),
                ram_gb: m.probe.f64("ram_gb").unwrap_or(0.0),
                cpus: m.probe.f64("cpus").unwrap_or(0.0) as usize,
            }
        })
        .collect()
}

#[derive(Debug, PartialEq)]
pub struct Pick {
    pub machine: Machine,
    /// Forward passes run on its graphics card; false: on its processors, from memory.
    pub gpu: bool,
}

/// The computer for the bench. One with a graphics card that holds the model (the most graphics
/// memory wins), else the one with the most memory, running on its processors. The bench and its
/// forward passes run on the same computer. None: no computer here holds the model.
pub fn pick(machines: &[Machine], params_b: f64) -> Option<Pick> {
    let gpu_need = params_b * 2.0 * 1.3 + 1.0;
    let cpu_need = params_b * 4.0 * 1.3 + 1.0;
    let best_gpu = machines
        .iter()
        .filter(|m| m.gpu.as_ref().is_some_and(|g| g.1 >= gpu_need))
        .max_by(|a, b| {
            let g = |m: &Machine| m.gpu.as_ref().map_or(0.0, |g| g.1);
            g(a).partial_cmp(&g(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then((a.at == Where::Local).cmp(&(b.at == Where::Local)))
        });
    if let Some(m) = best_gpu {
        return Some(Pick {
            machine: m.clone(),
            gpu: true,
        });
    }
    machines
        .iter()
        .filter(|m| m.ram_gb >= cpu_need)
        .max_by(|a, b| {
            a.ram_gb
                .partial_cmp(&b.ram_gb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then((a.at == Where::Local).cmp(&(b.at == Where::Local)))
        })
        .map(|m| Pick {
            machine: m.clone(),
            gpu: false,
        })
}

/// Seconds for one full forward pass (512 prompts, 512 tokens each). A guess from the parameter
/// count, not a measurement: a card makes about 2000 tokens a second on a 1B model across a
/// batch, and a processor about 8 per core. Say so wherever it is shown.
pub fn pass_seconds(params_b: f64, gpu: bool, cpus: usize) -> f64 {
    let tokens = 512.0 * 512.0;
    let per_sec = if gpu {
        2000.0 / params_b.max(0.1)
    } else {
        8.0 * cpus.max(1) as f64 / params_b.max(0.1)
    };
    tokens / per_sec
}

pub fn duration_words(secs: f64) -> String {
    if secs < 90.0 {
        format!("{secs:.0} seconds")
    } else if secs < 5400.0 {
        format!("{:.0} minutes", secs / 60.0)
    } else if secs < 86400.0 * 2.0 {
        format!("{:.1} hours", secs / 3600.0)
    } else {
        format!("{:.0} days", secs / 86400.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, ram: f64, gpu: Option<f64>) -> Machine {
        Machine {
            name: name.into(),
            at: if name == "this computer" {
                Where::Local
            } else {
                Where::Ssh(name.into())
            },
            ram_gb: ram,
            cpus: 8,
            gpu: gpu.map(|g| ("A card".into(), g)),
        }
    }

    #[test]
    fn names_turn_into_repos_sizes_and_licences() {
        assert_eq!(
            hf_repo("qwen3.5:0.8b").as_deref(),
            Some("Qwen/Qwen3.5-0.8B")
        );
        assert_eq!(
            hf_repo("gemma3:270m").as_deref(),
            Some("google/gemma-3-270m-it")
        );
        assert_eq!(hf_repo("qwen3:4b").as_deref(), Some("Qwen/Qwen3-4B"));
        assert_eq!(hf_repo("mystery:7b"), None);
        assert_eq!(params_b("qwen3.5:0.8b"), Some(0.8));
        assert_eq!(params_b("gemma3:270m"), Some(0.27));
        assert_eq!(params_b("llama3.2"), None);
        assert_eq!(weights_gb(0.8), 1.6);
        assert_eq!(licence("qwen3.5:0.8b").name, "Apache-2.0");
        assert!(licence("gemma3:270m").words.contains("Gemma"));
        assert!(licence("llama3.2:1b").words.contains("community licence"));
        assert_eq!(licence("mystery:7b").modify_ok, None);
    }

    #[test]
    fn a_card_that_holds_the_model_wins_else_the_most_memory() {
        let ms = vec![
            m("this computer", 32.0, None),
            m("studio", 16.0, Some(12.0)),
        ];
        let p = pick(&ms, 0.8).unwrap();
        assert!(p.gpu && p.machine.name == "studio");
        // a card too small for it: the most memory, on its processors
        let ms = vec![m("this computer", 32.0, None), m("studio", 16.0, Some(2.0))];
        let p = pick(&ms, 0.8).unwrap();
        assert!(!p.gpu && p.machine.name == "this computer");
        // nothing holds a 70B model
        assert_eq!(pick(&ms, 70.0), None);
    }

    #[test]
    fn the_time_is_a_guess_that_says_how_long() {
        let cpu = pass_seconds(0.8, false, 8);
        let gpu = pass_seconds(0.8, true, 8);
        assert!(cpu > 20.0 * gpu && cpu > 1800.0, "{cpu} {gpu}");
        assert_eq!(duration_words(30.0), "30 seconds");
        assert_eq!(duration_words(7200.0), "2.0 hours");
        assert_eq!(duration_words(86400.0 * 4.0), "4 days");
    }
}
