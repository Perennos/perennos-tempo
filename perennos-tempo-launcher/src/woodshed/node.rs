//! Which computer trains. The Fleet already knows the machines and what each one said about itself
//! (`fleet check` over the owner's own ssh). The pick is the machine with the most graphics memory
//! on an NVIDIA card, or an Apple Silicon Mac with the most memory (about two thirds of a Mac's
//! memory is what its graphics chip can use, so that is what is counted).
//!
//! No graphics card anywhere is not the end of it. A processor with enough memory can train a small
//! model (3B and under) with plain 16-bit LoRA, slowly, and the plan says how slowly, as an
//! estimate. Past 3B a processor takes days, so Tempo says so and starts nothing: a retune or
//! Lessons does that job without a card. A small card (6 to 8 GB) trains QLoRA on a model that
//! fits it, and the plan names which one.
//!
//! Never a guest's machine: only this computer and machines in the owner's fleet that answered
//! the owner's own ssh key are candidates. A computer that only paired to chat is not one.
//! And only when it is idle.

use super::exec::{Exec, Target, Where};
use super::script::Backend;
use crate::fleet::store::Fleet;
use crate::setup::json::J;
use crate::setup::probe::{self, Gpu, Host};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub name: String,
    pub at: Where,
    pub backend: Backend,
    /// Memory training can use, in GB: graphics memory, a Mac's two thirds, or on a processor
    /// three quarters of the RAM.
    pub gb: f64,
    pub what: String,
    pub windows: bool,
    /// Processor threads (0 when not known; only the processor path needs it).
    pub cores: u32,
}

impl Node {
    pub fn words(&self) -> String {
        format!("{} ({}, {:.0} GB usable)", self.name, self.what, self.gb)
    }
}

/// Three quarters of a computer's RAM is what a training run can count on.
fn usable_ram(gb: f64) -> f64 {
    (gb * 0.75 * 10.0).round() / 10.0
}

fn cpu_node(name: &str, at: Where, cores: u32, ram_gb: f64, windows: bool) -> Option<Node> {
    (cores >= 1 && ram_gb > 0.0).then(|| Node {
        name: name.into(),
        at,
        backend: Backend::Cpu,
        gb: usable_ram(ram_gb),
        what: format!("CPU only, {cores} threads"),
        windows,
        cores,
    })
}

/// This computer, if it can train at all.
pub fn local(host: &dyn Host) -> Option<Node> {
    let windows = host.os() == "windows";
    match probe::probe_gpu(host) {
        Gpu::Nvidia { name, total_gb, .. } => Some(Node {
            name: "this computer".into(),
            at: Where::Local,
            backend: Backend::Cuda,
            gb: total_gb,
            what: name,
            windows,
            cores: host.cpus() as u32,
        }),
        Gpu::Apple { chip } => {
            let mem = probe::probe_memory(host).0?;
            Some(Node {
                name: "this computer".into(),
                at: Where::Local,
                backend: Backend::Mlx,
                gb: (mem * 0.65 * 10.0).round() / 10.0,
                what: chip,
                windows,
                cores: host.cpus() as u32,
            })
        }
        Gpu::None => cpu_node(
            "this computer",
            Where::Local,
            host.cpus() as u32,
            probe::probe_memory(host).0.unwrap_or(0.0),
            windows,
        ),
    }
}

/// The number in `"NVIDIA GeForce RTX 4090, 24564 MiB"`, in GB.
fn mib_in(gpu: &str) -> Option<f64> {
    let low = gpu.to_ascii_lowercase();
    let at = low.find("mib")?;
    let n: String = low[..at]
        .trim_end()
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    n.parse::<f64>()
        .ok()
        .map(|m| (m / 1024.0 * 10.0).round() / 10.0)
}

fn node_of(name: &str, target: String, probe: &J) -> Option<Node> {
    let gpu = probe.str("gpu").unwrap_or("");
    let os = probe.str("os").unwrap_or("");
    if gpu.to_ascii_lowercase().contains("nvidia") {
        return Some(Node {
            name: name.into(),
            at: Where::Ssh(target),
            backend: Backend::Cuda,
            gb: mib_in(gpu)?,
            what: gpu.split(',').next().unwrap_or(gpu).trim().to_string(),
            windows: false,
            cores: probe.f64("cpus").unwrap_or(0.0) as u32,
        });
    }
    if os.contains("Darwin") && os.contains("arm64") {
        let mem = probe.f64("ram_gb")?;
        return Some(Node {
            name: name.into(),
            at: Where::Ssh(target),
            backend: Backend::Mlx,
            gb: (mem * 0.65 * 10.0).round() / 10.0,
            what: if gpu.is_empty() {
                "Apple Silicon".into()
            } else {
                gpu.into()
            },
            windows: false,
            cores: probe.f64("cpus").unwrap_or(0.0) as u32,
        });
    }
    // anything else, an AMD or Intel graphics chip included, has nothing Tempo trains on: it counts
    // as a processor
    cpu_node(
        name,
        Where::Ssh(target),
        probe.f64("cpus").unwrap_or(0.0) as u32,
        probe.f64("ram_gb").unwrap_or(0.0),
        false,
    )
}

/// The fleet's machines that can train and that the owner's own ssh reaches. `hosts` are the
/// `Host` names in the owner's ssh config, which win over an address as the name to dial.
pub fn fleet_nodes(f: &Fleet, hosts: &[String]) -> Vec<Node> {
    f.peers()
        .filter(|m| m.ssh.bool("ok") == Some(true))
        .filter_map(|m| {
            let target = if hosts.contains(&m.name) {
                m.name.clone()
            } else {
                m.addr.clone()
            };
            node_of(&m.name, target, &m.probe)
        })
        .collect()
}

/// The best of them: the most memory, and on a tie this computer, then the name.
pub fn pick(mut nodes: Vec<Node>) -> Option<Node> {
    nodes.sort_by(|a, b| {
        b.gb.partial_cmp(&a.gb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then((b.at == Where::Local).cmp(&(a.at == Where::Local)))
            .then(a.name.cmp(&b.name))
    });
    nodes.into_iter().next()
}

pub const NO_NODE: &str = "no computer in your fleet can train: it takes an NVIDIA card or a Mac with Apple Silicon, or for a small model (3B and under) a processor with the memory, that your own ssh reaches (`tempo-launch fleet check NAME`)";

/// The biggest model a processor trains. Past it a run takes days, not hours.
pub const CPU_MAX_B: f64 = 3.0;

/// Billions of parameters from an Ollama tag: `qwen3:8b` is 8, `qwen3:0.6b-q4_K_M` is 0.6,
/// `gemma3:270m` is 0.27. None when the tag gives no size (`latest`).
pub fn params_b(tag: &str) -> Option<f64> {
    let size = tag.split_once(':')?.1.split(['-', '_']).next()?;
    let size = size.to_ascii_lowercase();
    if let Some(b) = size.strip_suffix('b') {
        return b.parse().ok().filter(|n: &f64| *n > 0.0);
    }
    size.strip_suffix('m')?
        .parse::<f64>()
        .ok()
        .filter(|n| *n > 0.0)
        .map(|m| m / 1000.0)
}

/// The sizes Tempo knows for a family, small to big. A family it does not know steps down
/// the Qwen3 ones.
fn ladder(tag: &str) -> (&'static str, &'static [f64]) {
    match tag.split(':').next().unwrap_or("") {
        "llama3.2" => ("llama3.2", &[1.0, 3.0]),
        "gemma3" => ("gemma3", &[1.0, 4.0, 12.0, 27.0]),
        _ => ("qwen3", &[0.6, 1.7, 4.0, 8.0, 14.0, 32.0]),
    }
}

/// Memory QLoRA needs for a model of `b` billion parameters, in GB: 4-bit weights, the adapter and
/// its optimizer, and room for a 2,048-token step. A rule of thumb, never measured here.
pub fn gpu_need_gb(b: f64) -> f64 {
    b * 0.65 + 2.0
}

/// Memory a 16-bit LoRA run on a processor needs, in GB: two bytes a parameter and working room.
pub fn cpu_need_gb(b: f64) -> f64 {
    b * 2.0 + 4.0
}

/// What the desktop keeps for itself on a card.
const CARD_HEADROOM_GB: f64 = 0.4;

/// The training pick: where, on which model, and a sentence when the model is not the one asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct Chosen {
    pub node: Node,
    pub base: String,
    pub note: Option<String>,
}

fn size_name(b: f64) -> String {
    format!("{b}B")
}

/// The biggest model of the base's family, no bigger than the base, that `fits`.
fn step_down(base: &str, want: f64, fits: &dyn Fn(f64) -> bool) -> Option<(String, f64)> {
    let (family, sizes) = ladder(base);
    sizes
        .iter()
        .rev()
        .copied()
        .filter(|s| *s < want)
        .find(|s| fits(*s))
        .map(|s| (format!("{family}:{s}b"), s))
}

fn on_a_card(n: &Node, base: &str) -> Result<Chosen, String> {
    let room = n.gb - CARD_HEADROOM_GB;
    let keep = || Chosen {
        node: n.clone(),
        base: base.into(),
        note: None,
    };
    let Some(want) = params_b(base) else {
        return Ok(keep());
    };
    if gpu_need_gb(want) <= room {
        return Ok(keep());
    }
    let Some((smaller, b)) = step_down(base, want, &|s| gpu_need_gb(s) <= room) else {
        return Err(format!(
            "{} has {:.0} GB to train in, and QLoRA on even the smallest model Tempo knows ({}) needs about {:.0} GB (an estimate). A card with more memory, or a retune or Lessons, which need no training, would do it",
            n.words(),
            n.gb,
            size_name(ladder(base).1[0]),
            gpu_need_gb(ladder(base).1[0])
        ));
    };
    Ok(Chosen {
        node: n.clone(),
        note: Some(format!(
            "{base} ({}) needs about {:.0} GB for QLoRA (an estimate) and {} has {:.0}, so this run trains {smaller} ({}, about {:.0} GB) with QLoRA instead. The adapter fits {smaller}, not {base}",
            size_name(want),
            gpu_need_gb(want),
            n.name,
            n.gb,
            size_name(b),
            gpu_need_gb(b)
        )),
        base: smaller,
    })
}

fn on_a_processor(n: &Node, base: &str) -> Result<Chosen, String> {
    let Some(want) = params_b(base) else {
        return Err(format!(
            "no graphics card in your fleet, and I can't tell from the name how big {base} is, which is what decides whether a processor can train it. Give the band a base with its size, like qwen3:1.7b"
        ));
    };
    if want > CPU_MAX_B {
        return Err(format!(
            "no graphics card in your fleet, and {base} is {} ({} is the most a processor trains in hours; past that it is days). Tempo won't start a CPU job that size. A retune or Lessons is the better tool here, and neither needs a card. Or add a computer with an NVIDIA card or Apple Silicon to your fleet",
            size_name(want),
            size_name(CPU_MAX_B)
        ));
    }
    if cpu_need_gb(want) <= n.gb {
        return Ok(Chosen {
            node: n.clone(),
            base: base.into(),
            note: None,
        });
    }
    let Some((smaller, b)) = step_down(base, want, &|s| cpu_need_gb(s) <= n.gb) else {
        return Err(format!(
            "{} has about {:.0} GB of memory to train in, and {base} ({}) needs about {:.0} GB for a 16-bit run (an estimate). More memory, or a retune or Lessons, which need less, would do it",
            n.words(),
            n.gb,
            size_name(want),
            cpu_need_gb(want)
        ));
    };
    Ok(Chosen {
        node: n.clone(),
        note: Some(format!(
            "{base} ({}) needs about {:.0} GB for a 16-bit run on a processor (an estimate) and {} has {:.0}, so this run trains {smaller} ({}) instead. The adapter fits {smaller}, not {base}",
            size_name(want),
            cpu_need_gb(want),
            n.name,
            n.gb,
            size_name(b)
        )),
        base: smaller,
    })
}

/// The best processor: the most threads, then the most memory, then this computer, then the name.
fn pick_cpu(mut nodes: Vec<Node>) -> Option<Node> {
    nodes.sort_by(|a, b| {
        b.cores
            .cmp(&a.cores)
            .then(b.gb.partial_cmp(&a.gb).unwrap_or(std::cmp::Ordering::Equal))
            .then((b.at == Where::Local).cmp(&(a.at == Where::Local)))
            .then(a.name.cmp(&b.name))
    });
    nodes.into_iter().next()
}

/// Which machine trains `base`, and on which model, or why none does. A card wins over a
/// processor; the processor is for fleets with no card that can take the job.
pub fn choose(nodes: &[Node], base: &str) -> Result<Chosen, String> {
    let cards: Vec<Node> = nodes
        .iter()
        .filter(|n| n.backend != Backend::Cpu)
        .cloned()
        .collect();
    let cpus: Vec<Node> = nodes
        .iter()
        .filter(|n| n.backend == Backend::Cpu)
        .cloned()
        .collect();
    let mut first = None;
    if let Some(n) = pick(cards) {
        match on_a_card(&n, base) {
            Ok(c) => return Ok(c),
            Err(e) => first = Some(e),
        }
    }
    if let Some(n) = pick_cpu(cpus) {
        match on_a_processor(&n, base) {
            Ok(c) => return Ok(c),
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    Err(first.unwrap_or_else(|| NO_NODE.into()))
}

/// How long a processor run takes, in words, from the work and the threads. FLOPs for a LoRA step
/// are about 6 x parameters x tokens (forward, backward and the re-run checkpointing costs);
/// a thread is taken to deliver 6 to 9 GFLOPS in PyTorch, and an example to run 350 to 450 tokens.
/// Those are guesses, never timed on a real run, so it is a range and it is called an estimate.
pub fn cpu_estimate(examples: usize, epochs: u32, params_b: f64, threads: u32) -> String {
    let hours = |tokens: f64, gflops: f64| {
        6.0 * params_b * 1e9 * examples as f64 * f64::from(epochs) * tokens
            / (gflops * 1e9 * f64::from(threads.max(1)))
            / 3600.0
    };
    let (lo, hi) = (hours(350.0, 9.0), hours(450.0, 6.0));
    let span = if hi < 1.5 {
        let (a, b) = (
            (lo * 60.0 / 5.0).floor() * 5.0,
            (hi * 60.0 / 5.0).ceil() * 5.0,
        );
        let a = a.max(5.0);
        if a >= b {
            format!("{b} minutes")
        } else {
            format!("{a}\u{2013}{b} minutes")
        }
    } else if hi < 24.0 {
        let (a, b) = (lo.floor().max(1.0), hi.ceil());
        if a == b {
            format!("{a} hours")
        } else {
            format!("{a}\u{2013}{b} hours")
        }
    } else {
        format!(
            "{}\u{2013}{} days",
            (lo / 24.0).floor().max(1.0),
            (hi / 24.0).ceil()
        )
    };
    format!(
        "about {span} for {examples} example{} ({epochs} pass{}) on {threads} threads",
        if examples == 1 { "" } else { "s" },
        if epochs == 1 { "" } else { "es" }
    )
}

/// Whether the machine is quiet: its one-minute load per processor is under half. None: could not
/// tell (no load figure on that system).
pub fn idle(exec: &dyn Exec, n: &Node, local_dir: &str) -> Option<bool> {
    let t = Target {
        at: n.at.clone(),
        dir: if n.at == Where::Local {
            local_dir.to_string()
        } else {
            "$HOME/.tempo-woodshed".into()
        },
    };
    let said = exec
        .run(
            &t,
            &["sh".into(), "-c".into(), "cat /proc/loadavg 2>/dev/null || sysctl -n vm.loadavg 2>/dev/null; (nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null)".into()],
            Duration::from_secs(20),
        )
        .ok()?;
    parse_load(&said.tail)
}

/// `0.42 0.30 0.20 1/300 1234` or `{ 0.42 0.30 0.20 }`, then the processor count on its own line.
pub fn parse_load(text: &str) -> Option<bool> {
    let cleaned = text.replace(['{', '}'], " ");
    let mut lines = cleaned.lines().filter(|l| !l.trim().is_empty());
    let load: f64 = lines.next()?.split_whitespace().next()?.parse().ok()?;
    let cpus: f64 = lines
        .next_back()?
        .trim()
        .parse()
        .ok()
        .filter(|c: &f64| *c >= 1.0)?;
    Some(load / cpus < 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::store::Machine;
    use crate::setup::json::parse;

    fn machine(name: &str, ssh_ok: bool, probe: &str) -> Machine {
        let mut m = Machine::new(name, "192.168.0.20", "engine");
        m.ssh = parse(&format!("{{\"ok\":{ssh_ok}}}")).unwrap();
        m.probe = parse(probe).unwrap();
        m
    }

    #[test]
    fn the_most_graphics_memory_wins_and_a_mac_counts_two_thirds() {
        let f = Fleet {
            me: "here".into(),
            machines: vec![
                machine(
                    "small-box",
                    true,
                    r#"{"os":"Linux x86_64","gpu":"NVIDIA GeForce RTX 3060, 12288 MiB","ram_gb":32}"#,
                ),
                machine(
                    "big-box",
                    true,
                    r#"{"os":"Linux x86_64","gpu":"NVIDIA GeForce RTX 4090, 24564 MiB","ram_gb":64}"#,
                ),
                machine(
                    "studio-mac",
                    true,
                    r#"{"os":"Darwin arm64","gpu":"Apple M2 Ultra","ram_gb":128}"#,
                ),
                machine(
                    "old-laptop",
                    true,
                    r#"{"os":"Linux x86_64","gpu":"","ram_gb":8}"#,
                ),
            ],
            allow: vec![],
        };
        let nodes = fleet_nodes(&f, &[]);
        assert_eq!(
            nodes.len(),
            3,
            "a machine that never said how many threads it has is not a candidate"
        );
        let best = pick(nodes.clone()).unwrap();
        assert_eq!(
            (best.name.as_str(), best.backend),
            ("studio-mac", Backend::Mlx)
        );
        assert_eq!(best.gb, 83.2);
        // a smaller Mac loses to the 4090
        let mut f2 = f.clone();
        f2.machines[2] = machine(
            "studio-mac",
            true,
            r#"{"os":"Darwin arm64","gpu":"Apple M2","ram_gb":16}"#,
        );
        let best = pick(fleet_nodes(&f2, &[])).unwrap();
        assert_eq!(
            (best.name.as_str(), best.backend, best.gb),
            ("big-box", Backend::Cuda, 24.0)
        );
        assert_eq!(best.at, Where::Ssh("192.168.0.20".into()));
    }

    #[test]
    fn only_machines_the_owners_own_ssh_reaches_are_candidates() {
        let f = Fleet {
            me: "here".into(),
            machines: vec![
                machine(
                    "paired-only",
                    false,
                    r#"{"os":"Linux","gpu":"NVIDIA A100, 81920 MiB"}"#,
                ),
                machine(
                    "never-checked",
                    true,
                    r#"{"os":"Linux","gpu":"NVIDIA A100, 81920 MiB"}"#,
                ),
            ],
            allow: vec![],
        };
        let mut f = f;
        f.machines[1].ssh = J::Null; // never ran `fleet check`
        assert!(fleet_nodes(&f, &[]).is_empty());
        // a disabled machine is out even when it answers
        f.machines[0].ssh = parse(r#"{"ok":true}"#).unwrap();
        f.machines[0].enabled = false;
        assert!(fleet_nodes(&f, &[]).is_empty());
        f.machines[0].enabled = true;
        // an ssh config name beats the address
        let n = fleet_nodes(&f, &["paired-only".to_string()]);
        assert_eq!(n[0].at, Where::Ssh("paired-only".into()));
    }

    #[test]
    fn memory_is_read_from_the_gpu_line_and_this_computer_wins_a_tie() {
        assert_eq!(mib_in("NVIDIA GeForce RTX 4090, 24564 MiB"), Some(24.0));
        assert_eq!(mib_in("NVIDIA T4"), None);
        let a = Node {
            name: "z".into(),
            at: Where::Ssh("h".into()),
            backend: Backend::Cuda,
            gb: 24.0,
            what: "x".into(),
            windows: false,
            cores: 0,
        };
        let b = Node {
            name: "this computer".into(),
            at: Where::Local,
            ..a.clone()
        };
        assert_eq!(pick(vec![a, b]).unwrap().at, Where::Local);
        assert!(pick(vec![]).is_none());
    }

    #[test]
    fn quiet_means_half_a_load_per_processor() {
        assert_eq!(parse_load("0.42 0.30 0.20 1/300 1234\n8"), Some(true));
        assert_eq!(parse_load("{ 6.50 5.0 4.0 }\n8"), Some(false));
        assert_eq!(parse_load("0.10 0 0\n16\n"), Some(true));
        assert_eq!(parse_load("0.10 0 0"), None);
        assert_eq!(parse_load(""), None);
        assert_eq!(parse_load("garbage\n8"), None);
    }
    // ---- computers without a big card

    fn cpu(name: &str, threads: u32, ram_gb: f64) -> Node {
        cpu_node(name, Where::Ssh("h".into()), threads, ram_gb, false).unwrap()
    }

    fn card(gb: f64) -> Node {
        Node {
            name: "gamer".into(),
            at: Where::Ssh("h".into()),
            backend: Backend::Cuda,
            gb,
            what: "NVIDIA GeForce".into(),
            windows: false,
            cores: 8,
        }
    }

    #[test]
    fn sizes_come_from_the_tag() {
        assert_eq!(params_b("qwen3:8b"), Some(8.0));
        assert_eq!(params_b("qwen3:0.6b-q4_K_M"), Some(0.6));
        assert_eq!(params_b("gemma3:270m"), Some(0.27));
        assert_eq!(params_b("llama3:latest"), None);
        assert_eq!(params_b("noversion"), None);
    }

    #[test]
    fn no_card_and_a_small_model_trains_on_the_processor_with_an_honest_estimate() {
        let c = choose(&[cpu("big-cpu", 16, 32.0)], "qwen3:1.7b").unwrap();
        assert_eq!(c.node.backend, Backend::Cpu);
        assert_eq!((c.base.as_str(), c.note), ("qwen3:1.7b", None));
        // 3B is the top of what a processor takes
        assert!(choose(&[cpu("big-cpu", 16, 32.0)], "llama3.2:3b").is_ok());
        let said = cpu_estimate(2000, 1, 0.6, 16);
        assert!(
            said.starts_with("about ")
                && said.contains("hours for 2000 examples (1 pass) on 16 threads"),
            "{said}"
        );
        // more threads or a smaller model is faster; more examples or a bigger model is slower
        let hrs = |t: &str| -> f64 {
            let n: f64 = t.split(['\u{2013}', ' ']).nth(1).unwrap().parse().unwrap();
            if t.contains("days") {
                n * 24.0
            } else {
                n
            }
        };
        assert!(hrs(&cpu_estimate(2000, 1, 0.6, 32)) < hrs(&cpu_estimate(2000, 1, 0.6, 16)));
        assert!(hrs(&cpu_estimate(2000, 1, 1.7, 16)) > hrs(&cpu_estimate(2000, 1, 0.6, 16)));
        assert!(cpu_estimate(2000, 2, 1.7, 16).contains("days"));
        // over 24 hours is said in days, never "48 hours"
        let long = cpu_estimate(2000, 1, 1.7, 16);
        assert!(long.contains("days") && !long.contains("hours"), "{long}");
        assert!(cpu_estimate(2000, 1, 0.6, 16).contains("hours"));
        assert!(
            cpu_estimate(10, 1, 0.6, 16).contains("minutes"),
            "{}",
            cpu_estimate(10, 1, 0.6, 16)
        );
        assert!(cpu_estimate(1, 1, 0.6, 16).contains("1 example "));
    }

    #[test]
    fn no_card_and_a_big_model_says_so_and_starts_nothing() {
        for base in ["qwen3:4b", "qwen3:8b", "gemma3:27b"] {
            let e = choose(&[cpu("big-cpu", 64, 256.0)], base).unwrap_err();
            assert!(e.contains("Tempo won't start a CPU job that size"), "{e}");
            assert!(e.contains("retune or Lessons is the better tool"), "{e}");
        }
        // a name with no size cannot be judged, so nothing starts either
        let e = choose(&[cpu("big-cpu", 64, 256.0)], "llama3:latest").unwrap_err();
        assert!(e.contains("can't tell"), "{e}");
    }

    #[test]
    fn a_processor_without_the_memory_steps_down_to_a_model_that_fits() {
        // 3B needs about 10 GB, 1.7B about 7.4: 8 GB of RAM leaves 6 usable
        let small = cpu("tiny", 8, 8.0);
        assert_eq!(small.gb, 6.0);
        let c = choose(&[small], "qwen3:1.7b").unwrap();
        assert_eq!(c.base, "qwen3:0.6b");
        assert!(c.note.unwrap().contains("trains qwen3:0.6b"));
        // nothing fits 2 GB
        let e = choose(&[cpu("tiny", 8, 2.0)], "qwen3:1.7b").unwrap_err();
        assert!(e.contains("needs about"), "{e}");
    }

    #[test]
    fn a_small_card_trains_qlora_on_the_model_that_fits_and_says_which() {
        // 8 GB: the 8B just fits
        assert_eq!(choose(&[card(8.0)], "qwen3:8b").unwrap().base, "qwen3:8b");
        // 6 GB: the 8B does not, the 4B does
        let c = choose(&[card(6.0)], "qwen3:8b").unwrap();
        assert_eq!(
            (c.base.as_str(), c.node.backend),
            ("qwen3:4b", Backend::Cuda)
        );
        let note = c.note.unwrap();
        assert!(
            note.contains("qwen3:8b") && note.contains("trains qwen3:4b") && note.contains("QLoRA"),
            "{note}"
        );
        // a family of its own steps down within itself
        assert_eq!(
            choose(&[card(5.0)], "gemma3:12b").unwrap().base,
            "gemma3:4b"
        );
        // a bigger base than the ladder knows still lands on a size that fits
        assert_eq!(choose(&[card(6.0)], "qwen3:70b").unwrap().base, "qwen3:4b");
        // a card too small for anything falls to the processor when the fleet has one
        let c = choose(&[card(2.0), cpu("big-cpu", 16, 32.0)], "qwen3:1.7b").unwrap();
        assert_eq!(c.node.backend, Backend::Cpu);
        let e = choose(&[card(2.0)], "qwen3:1.7b").unwrap_err();
        assert!(e.contains("even the smallest"), "{e}");
    }

    #[test]
    fn a_card_beats_a_processor_and_a_big_card_keeps_the_model() {
        let c = choose(&[cpu("big-cpu", 64, 256.0), card(24.0)], "qwen3:8b").unwrap();
        assert_eq!(
            (c.node.backend, c.base.as_str(), c.note),
            (Backend::Cuda, "qwen3:8b", None)
        );
        // the strongest processor of several
        let c = choose(&[cpu("a", 8, 64.0), cpu("b", 16, 32.0)], "qwen3:1.7b").unwrap();
        assert_eq!(c.node.name, "b");
        assert_eq!(choose(&[], "qwen3:8b").unwrap_err(), NO_NODE);
    }

    #[test]
    fn this_computer_and_the_fleet_count_as_processors_when_they_have_no_card() {
        use crate::setup::probe::tests::Fake;
        let host = Fake {
            os: "linux",
            arch: "x86_64",
            cpus: 12,
            files: [(
                "/proc/meminfo".to_string(),
                "MemTotal:       32768000 kB\nMemAvailable:   30000000 kB\n".to_string(),
            )]
            .into(),
            ..Fake::default()
        };
        let n = local(&host).unwrap();
        assert_eq!(
            (n.backend, n.cores, n.at.clone()),
            (Backend::Cpu, 12, Where::Local)
        );
        assert!(n.words().contains("CPU only, 12 threads"), "{}", n.words());
        // no memory figure, nothing to plan with
        assert!(local(&Fake {
            os: "linux",
            arch: "x86_64",
            cpus: 12,
            ..Fake::default()
        })
        .is_none());
        // and in the fleet
        let f = Fleet {
            me: "here".into(),
            machines: vec![machine(
                "old-laptop",
                true,
                r#"{"os":"Linux x86_64","gpu":"","cpus":8,"ram_gb":16}"#,
            )],
            allow: vec![],
        };
        let nodes = fleet_nodes(&f, &[]);
        assert_eq!(
            (nodes[0].backend, nodes[0].cores, nodes[0].gb),
            (Backend::Cpu, 8, 12.0)
        );
    }
}
