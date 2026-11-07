//! Which computer trains. The Fleet already knows the machines and what each one said about itself
//! (`fleet check` over the owner's own ssh). The pick is the machine with the most graphics memory
//! on an NVIDIA card, or an Apple Silicon Mac with the most memory (about two thirds of a Mac's
//! memory is what its graphics chip can use, so that is what is counted).
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
    /// Graphics memory in GB (a Mac: two thirds of its memory).
    pub gb: f64,
    pub what: String,
    pub windows: bool,
}

impl Node {
    pub fn words(&self) -> String {
        format!("{} ({}, {:.0} GB usable)", self.name, self.what, self.gb)
    }
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
            })
        }
        Gpu::None => None,
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
        });
    }
    None
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
            "a machine with no graphics is not a candidate"
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
}
