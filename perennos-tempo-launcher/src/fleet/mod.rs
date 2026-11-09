//! Tempo Fleet: Tempo knows the person's own computers and talks between them.
//!
//! `scan` finds them on their own networks (opt-in, gentle, private addresses only), `add` and
//! `remove` choose which are in the fleet (and write the agent's settings), `ssh`, `check` and
//! `setup` reach them with the person's own ssh, and `serve`, `send`, `inbox` and `ping` are the
//! messenger between them: signed, durable, fast. See docs/TEMPO-SETUP.md ("Tempo Fleet").

pub mod blob;
pub mod crypto;
pub mod jobs;
pub mod mdns;
pub mod msg;
pub mod page;
pub mod pair;
pub mod route;
pub mod scan;
pub mod serve;
pub mod service;
pub mod ssh;
pub mod stage;
pub mod store;
pub mod tools;

#[cfg(test)]
mod band_tests;
#[cfg(test)]
mod blob_tests;
#[cfg(test)]
pub(crate) mod tests;

use crate::setup::json::{parse, J};
use crate::{allowed_remote_host, Ctx};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;
use store::{Fleet, Machine, Paths};

pub const PROPOSAL_FILE: &str = "fleet-proposal.json";
pub const APPLY_COMMAND: &str = "tempo-launch fleet apply-proposal";

// ---- the heading

/// 0 to 99 in words; larger numbers stay digits.
pub fn words(n: usize) -> String {
    const ONES: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = [
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    match n {
        0..=19 => ONES[n].to_string(),
        20..=99 if n.is_multiple_of(10) => TENS[n / 10 - 2].to_string(),
        20..=99 => format!("{}-{}", TENS[n / 10 - 2], ONES[n % 10]),
        _ => n.to_string(),
    }
}

/// "One machine, one field." / "Three machines, one field." The number is real: this computer
/// and every machine in the fleet.
pub fn heading(n: usize) -> String {
    let w = words(n);
    let mut c = w.chars();
    let first: String = c
        .next()
        .map(|f| f.to_uppercase().collect())
        .unwrap_or_default();
    format!(
        "{first}{} machine{}, one field.",
        c.as_str(),
        if n == 1 { "" } else { "s" }
    )
}

/// The heading for the fleet in `ctx`'s home.
pub fn heading_now(ctx: &Ctx) -> String {
    heading(store::load(&Paths::of(ctx)).count())
}

// ---- the command line

#[derive(Debug, PartialEq)]
pub enum FleetCmd {
    List,
    Scan {
        yes: bool,
    },
    Add {
        who: String,
        role: Option<String>,
        name: Option<String>,
    },
    Remove(String),
    Enable(String, bool),
    Init,
    Hosts,
    Ssh {
        who: String,
        cmd: Vec<String>,
    },
    Check(String),
    Setup {
        who: String,
        yes: bool,
    },
    Serve {
        quiet: bool,
        announce: bool,
    },
    Send {
        who: String,
        text: String,
    },
    Inbox,
    Ping(String),
    Page,
    ApplyProposal,
    InstallKey {
        name: Option<String>,
        peers: Vec<String>,
    },
    Invite,
    Join {
        code: Option<String>,
        controller: Option<String>,
        yes: bool,
    },
    Allow(Vec<String>, bool),
    /// The most one file may be in a blob stream, in GiB; None shows it.
    Cap(Option<f64>),
    Rotate {
        yes: bool,
    },
}

pub fn parse_args(rest: &[&str]) -> Result<FleetCmd, String> {
    let one = |what: &str| -> Result<String, String> {
        match rest {
            [_, x] if !x.starts_with("--") => Ok(x.to_string()),
            _ => Err(format!(
                "fleet {} takes one computer, by name or address",
                what
            )),
        }
    };
    Ok(match rest.first().copied() {
        None | Some("list") => FleetCmd::List,
        Some("scan") => match &rest[1..] {
            [] => FleetCmd::Scan { yes: false },
            ["--yes"] => FleetCmd::Scan { yes: true },
            _ => return Err("fleet scan takes only --yes".into()),
        },
        Some("add") => {
            let mut who = None;
            let (mut role, mut name) = (None, None);
            let mut it = rest[1..].iter();
            while let Some(a) = it.next() {
                match *a {
                    "--role" => role = it.next().map(|s| s.to_string()),
                    "--name" => name = it.next().map(|s| s.to_string()),
                    x if !x.starts_with("--") && who.is_none() => who = Some(x.to_string()),
                    x => return Err(format!("fleet add: unexpected {x}")),
                }
            }
            if role
                .as_deref()
                .is_some_and(|r| !["engine", "agent", "both"].contains(&r))
            {
                return Err("fleet add: --role is engine, agent or both".into());
            }
            FleetCmd::Add {
                who: who.ok_or("fleet add takes an address or a name")?,
                role,
                name,
            }
        }
        Some("remove") => FleetCmd::Remove(one("remove")?),
        Some("enable") => FleetCmd::Enable(one("enable")?, true),
        Some("disable") => FleetCmd::Enable(one("disable")?, false),
        Some("init") => FleetCmd::Init,
        Some("hosts") => FleetCmd::Hosts,
        Some("ssh") => match rest {
            [_, who, "--", cmd @ ..] if !cmd.is_empty() => FleetCmd::Ssh {
                who: who.to_string(),
                cmd: cmd.iter().map(|s| s.to_string()).collect(),
            },
            _ => return Err("fleet ssh takes a computer, then -- and the command".into()),
        },
        Some("check") => FleetCmd::Check(one("check")?),
        Some("setup") => match &rest[1..] {
            [w] if !w.starts_with("--") => FleetCmd::Setup {
                who: w.to_string(),
                yes: false,
            },
            [w, "--yes"] | ["--yes", w] => FleetCmd::Setup {
                who: w.to_string(),
                yes: true,
            },
            _ => return Err("fleet setup takes a computer (and --yes)".into()),
        },
        Some("serve") => {
            let flags = &rest[1..];
            if flags
                .iter()
                .any(|f| !matches!(*f, "--quiet" | "--announce"))
            {
                return Err("fleet serve takes only --quiet and --announce".into());
            }
            FleetCmd::Serve {
                quiet: flags.contains(&"--quiet"),
                announce: flags.contains(&"--announce"),
            }
        }
        Some("send") => match rest {
            [_, who, text] => FleetCmd::Send {
                who: who.to_string(),
                text: text.to_string(),
            },
            _ => return Err("fleet send takes a computer and the text in quotes".into()),
        },
        Some("inbox") => FleetCmd::Inbox,
        Some("ping") => FleetCmd::Ping(one("ping")?),
        Some("page") => FleetCmd::Page,
        Some("apply-proposal") => FleetCmd::ApplyProposal,
        Some("invite") => match &rest[1..] {
            [] => FleetCmd::Invite,
            _ => return Err("fleet invite takes nothing".into()),
        },
        Some("join") => {
            let (mut code, mut controller, mut yes) = (None, None, false);
            let mut it = rest[1..].iter();
            while let Some(a) = it.next() {
                match *a {
                    "--code" => code = it.next().map(|s| s.to_string()),
                    "--controller" => controller = it.next().map(|s| s.to_string()),
                    "--yes" => yes = true,
                    x => return Err(format!("fleet join: unexpected {x}")),
                }
            }
            FleetCmd::Join {
                code,
                controller,
                yes,
            }
        }
        Some(w @ ("allow" | "deny")) => {
            let list: Vec<String> = rest[1..].iter().map(|s| s.to_string()).collect();
            if list.is_empty() || list.iter().any(|e| !tools::allow_entry_ok(e)) {
                return Err(format!(
                    "fleet {w} takes comfy_image, stage, mcp:SERVER/TOOL or mcp:SERVER/*"
                ));
            }
            FleetCmd::Allow(list, w == "allow")
        }
        Some("cap") => match &rest[1..] {
            [] => FleetCmd::Cap(None),
            [n] => FleetCmd::Cap(Some(
                n.parse::<f64>()
                    .ok()
                    .filter(|g| (0.001..=1024.0).contains(g))
                    .ok_or("fleet cap takes a number of GiB, like 4 or 8")?,
            )),
            _ => return Err("fleet cap takes one number of GiB, or nothing to show it".into()),
        },
        Some("rotate") => match &rest[1..] {
            [] => FleetCmd::Rotate { yes: false },
            ["--yes"] => FleetCmd::Rotate { yes: true },
            _ => return Err("fleet rotate takes only --yes".into()),
        },
        Some("install-key") => {
            let (mut name, mut peers) = (None, vec![]);
            let mut it = rest[1..].iter();
            while let Some(a) = it.next() {
                match *a {
                    "--name" => name = it.next().map(|s| s.to_string()),
                    "--peer" => peers.extend(it.next().map(|s| s.to_string())),
                    x => return Err(format!("fleet install-key: unexpected {x}")),
                }
            }
            FleetCmd::InstallKey { name, peers }
        }
        Some(o) => return Err(format!("fleet: unknown command {o}")),
    })
}

fn age_words(s: u64) -> String {
    match s {
        0..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

fn health_words(h: &J) -> String {
    let mut v = Vec::new();
    if let Some(l) = h.f64("load") {
        v.push(format!("load {:.0}%", l * 100.0));
    }
    if let (Some(f), Some(t)) = (h.f64("mem_free_gb"), h.f64("mem_total_gb")) {
        v.push(format!("memory {f:.0} of {t:.0} GB free"));
    }
    let models = h.strs("models");
    if !models.is_empty() {
        v.push(format!("loaded: {}", models.join(", ")));
    }
    v.join(", ")
}

// ---- list, add, remove

pub fn list(p: &Paths, out: &mut dyn Write) {
    let f = store::load(p);
    let now = store::now_s();
    let _ = writeln!(out, "{}\n", heading(f.count()));
    let _ = writeln!(out, "  {:<16} this computer", f.me);
    for m in &f.machines {
        let seen = if m.last_seen == 0 {
            "not heard from".to_string()
        } else {
            format!("heard {}", age_words(now.saturating_sub(m.last_seen)))
        };
        let _ = writeln!(
            out,
            "  {:<16} {:<15} {:<7} {:<10} {}",
            m.name,
            m.addr,
            m.role,
            if m.enabled {
                "in the fleet"
            } else {
                "switched off"
            },
            seen
        );
        let h = health_words(&m.health);
        if !h.is_empty() {
            let _ = writeln!(out, "  {:<16} {h}", "");
        }
    }
    if f.machines.is_empty() {
        let _ = writeln!(
            out,
            "\nNo other computer yet. `tempo-launch fleet scan` looks for them on your own network."
        );
    }
}

/// The first private IPv4 address a name or address stands for (nothing public is ever probed).
fn private_v4(host: &str) -> Option<std::net::Ipv4Addr> {
    use std::net::{IpAddr, ToSocketAddrs};
    (host, 0u16)
        .to_socket_addrs()
        .ok()?
        .find_map(|a| match a.ip() {
            IpAddr::V4(v) if crate::private_ip(a.ip()) => Some(v),
            _ => None,
        })
}

/// What a computer is, from a quick look at its ports: engine, agent or both.
fn guess_role(addr: &str) -> Option<&'static str> {
    let ip = private_v4(addr)?;
    let ports = scan::real_ports();
    let open = scan::open_ports(
        &[ip],
        &[ports.ollama, ports.agent, ports.fleet],
        scan::CONNECT,
        3,
    );
    let has = |p: u16| open.iter().any(|o| o.1 == p);
    match (has(ports.ollama), has(ports.agent) || has(ports.fleet)) {
        (true, true) => Some("both"),
        (true, false) => Some("engine"),
        (false, true) => Some("agent"),
        _ => None,
    }
}

fn known_name(p: &Paths, who: &str) -> Option<(String, String)> {
    let t = std::fs::read_to_string(store::found_file(&p.home)).ok()?;
    parse(&t).ok()?.arr("found").iter().find_map(|f| {
        (f.str("addr") == Some(who) || f.str("name") == Some(who)).then(|| {
            (
                f.str("name").unwrap_or("").to_string(),
                f.str("role").unwrap_or("").to_string(),
            )
        })
    })
}

pub fn add(
    p: &Paths,
    pinned: Option<PathBuf>,
    who: &str,
    role: Option<&str>,
    name: Option<&str>,
    out: &mut dyn Write,
) -> i32 {
    let who = who.trim().to_ascii_lowercase();
    let who = who
        .strip_prefix("http://")
        .unwrap_or(&who)
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();
    if !allowed_remote_host(&who) {
        eprintln!(
            "tempo-launch: fleet add: {who} is not a home-network, tailnet or private address (allowed: 10/8, 172.16/12, 192.168/16, 100.64/10, 127/8, a single-label name like tower, or a name ending in .ts.net, .local, .lan, .internal, .home.arpa)"
        );
        return 2;
    }
    let cached = known_name(p, &who);
    let role = role
        .map(String::from)
        .or_else(|| {
            cached
                .as_ref()
                .map(|c| c.1.clone())
                .filter(|r| !r.is_empty())
        })
        .or_else(|| guess_role(&who).map(String::from))
        .unwrap_or_else(|| "engine".into());
    let default_name = if who.parse::<std::net::IpAddr>().is_ok() {
        cached
            .as_ref()
            .map(|c| c.0.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("pc-{}", who.rsplit('.').next().unwrap_or("x")))
    } else {
        who.split('.').next().unwrap_or(&who).to_string()
    };
    let name = store::clean_name(name.unwrap_or(&default_name));
    if name.is_empty() {
        eprintln!("tempo-launch: fleet add: that name is empty");
        return 2;
    }
    let mut m = Machine::new(&name, &who, &role);
    let r = store::update(p, |f| {
        if let Some(i) = f.find(&who).or_else(|| f.find(&name)) {
            let was = f.machines[i].enabled;
            f.machines[i].enabled = true;
            f.machines[i].role = role.clone();
            m = f.machines[i].clone();
            Err(was)
        } else {
            f.machines.push(m.clone());
            Ok(())
        }
    });
    let existed = match r {
        Ok(Ok(())) => false,
        Ok(Err(_)) => true,
        Err(e) => {
            eprintln!("tempo-launch: fleet add: {e}");
            return 1;
        }
    };
    let note = match store::config_set(p, pinned, &m, true, store::now_s()) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("tempo-launch: fleet add: {e}");
            return 1;
        }
    };
    let _ = writeln!(
        out,
        "{} {} ({}, {}) {} the fleet.",
        if existed { "Switched on" } else { "Added" },
        m.name,
        m.addr,
        m.role,
        if existed { "in" } else { "to" }
    );
    if m.engine() {
        let _ = writeln!(out, "Its Ollama ({}) is now in ollama_fallbacks and hosts in the agent's settings. Restart the agent (tempo-launch stop, then tempo-launch start) to use it.", m.engine_url());
    }
    if !note.is_empty() {
        let _ = writeln!(out, "{note}");
    }
    0
}

pub fn remove(p: &Paths, pinned: Option<PathBuf>, who: &str, out: &mut dyn Write) -> i32 {
    let gone = store::update(p, |f| f.find(who).map(|i| f.machines.remove(i)));
    match gone {
        Ok(Some(m)) => {
            let note = store::config_set(p, pinned, &m, false, store::now_s()).unwrap_or_default();
            let _ = writeln!(
                out,
                "Removed {} from the fleet and from the agent's settings.",
                m.name
            );
            if !note.is_empty() {
                let _ = writeln!(out, "{note}");
            }
            0
        }
        Ok(None) => {
            eprintln!("tempo-launch: fleet remove: no computer called {who} in the fleet");
            1
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet remove: {e}");
            1
        }
    }
}

pub fn enable(p: &Paths, pinned: Option<PathBuf>, who: &str, on: bool, out: &mut dyn Write) -> i32 {
    let r = store::update(p, |f| {
        f.find(who).map(|i| {
            f.machines[i].enabled = on;
            f.machines[i].clone()
        })
    });
    match r {
        Ok(Some(m)) => {
            let note = store::config_set(p, pinned, &m, on, store::now_s()).unwrap_or_default();
            let _ = writeln!(
                out,
                "{} is {} the fleet.",
                m.name,
                if on { "in" } else { "out of" }
            );
            if !note.is_empty() {
                let _ = writeln!(out, "{note}");
            }
            0
        }
        Ok(None) => {
            eprintln!("tempo-launch: fleet: no computer called {who} in the fleet");
            1
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet: {e}");
            1
        }
    }
}

// ---- scan

pub const SCAN_WORDS: &str = "What a scan does: it looks at the addresses on this computer's own home or office networks (the /24 around each of its addresses), the computers your tailnet lists, and mDNS. At each address it tries to connect, for a third of a second, to the ports of Ollama (11434), the Tempo agent (8765) and the Tempo fleet messenger (8770), at most 64 at a time; where something answers it reads one open page, one that needs no password (the model list, or the health answer), from that same private address. It sends no password and no file, never looks at a public address, and changes nothing. Adding a computer to the fleet is a separate step, and yours.";

pub fn scan_cmd(
    p: &Paths,
    yes: bool,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> (i32, Vec<scan::Found>) {
    let marker = p.file("fleet-scan.ok");
    if !yes && !marker.exists() {
        let _ = writeln!(out, "{SCAN_WORDS}\n");
        if !ask("Scan now? [y/N] ") {
            let _ = writeln!(out, "Nothing was scanned.");
            return (0, vec![]);
        }
    } else if !yes {
        let _ = writeln!(out, "(Scanning the way you agreed to before. The details: `tempo-launch fleet scan --yes` skips this note; docs/TEMPO-SETUP.md explains it.)");
    }
    let _ = crate::write_atomic(&marker, b"the person agreed to the scan\n");
    let fleet = store::load(p);
    let found = scan::scan(&fleet, out);
    if found.is_empty() {
        let _ = writeln!(out, "No other computer answered. A computer shows up when it runs Ollama, the Tempo agent or the fleet messenger on its private address.");
    }
    for f in &found {
        let _ = writeln!(out, "  {}", f.line());
    }
    let cache = J::Obj(vec![(
        "found".into(),
        J::Arr(
            found
                .iter()
                .map(|f| {
                    J::Obj(vec![
                        ("addr".into(), J::s(&f.addr)),
                        ("name".into(), J::s(&f.name)),
                        ("role".into(), J::s(f.role())),
                    ])
                })
                .collect(),
        ),
    )]);
    let _ = crate::write_atomic(&store::found_file(&p.home), cache.pretty().as_bytes());
    if found.iter().any(|f| !f.in_fleet) {
        let _ = writeln!(
            out,
            "\nTo use one: tempo-launch fleet add <address or name>"
        );
    }
    (0, found)
}

// ---- ssh

fn ssh_target(p: &Paths, who: &str) -> Result<(String, Option<Machine>), String> {
    let f = store::load(p);
    let hosts = ssh::user_hosts();
    if let Some(i) = f.find(who) {
        let m = f.machines[i].clone();
        let t = if hosts.contains(&m.name) {
            m.name.clone()
        } else {
            m.addr.clone()
        };
        return Ok((t, Some(m)));
    }
    if hosts.iter().any(|h| h == who) {
        return Ok((who.to_string(), None));
    }
    Err(format!(
        "{who} is neither in the fleet nor a Host in your ssh config{}",
        if hosts.is_empty() {
            String::new()
        } else {
            format!(" (it has: {})", hosts.join(", "))
        }
    ))
}

pub fn ssh_cmd(p: &Paths, who: &str, cmd: &[String], out: &mut dyn Write) -> i32 {
    let (target, _) = match ssh_target(p, who) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tempo-launch: fleet ssh: {e}");
            return 2;
        }
    };
    match ssh::args_for(&target, cmd).and_then(|a| ssh::run(&a, None, Duration::from_secs(600))) {
        Ok(s) => {
            let _ = write!(out, "{}", s.out);
            eprint!("{}", s.err);
            i32::from(!s.ok)
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet ssh: {e}");
            1
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect()
}

fn record_ssh(p: &Paths, name: &str, ok: bool, note: &str, probe: Option<J>) {
    let ssh = J::Obj(vec![
        ("ok".into(), J::Bool(ok)),
        ("at".into(), J::n(store::now_s() as f64)),
        ("note".into(), J::s(note)),
    ]);
    let _ = store::update(p, |f| {
        if let Some(m) = f.machines.iter_mut().find(|m| m.name == name) {
            m.ssh = ssh;
            if let Some(pr) = probe {
                m.probe = pr;
            }
        }
    });
}

pub fn check_cmd(p: &Paths, who: &str, out: &mut dyn Write) -> i32 {
    let (target, m) = match ssh_target(p, who) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tempo-launch: fleet check: {e}");
            return 2;
        }
    };
    let r = ssh::args_for(&target, &["sh".into(), "-s".into()])
        .and_then(|a| ssh::run(&a, Some(ssh::CHECK_SCRIPT), Duration::from_secs(30)));
    match r {
        Ok(s) if s.ok => {
            let c = ssh::parse_check(&s.out);
            let _ = writeln!(out, "{who} answered over ssh:");
            for l in c.lines() {
                let _ = writeln!(out, "  {l}");
            }
            if let Some(m) = m {
                record_ssh(p, &m.name, true, "ok", Some(c.to_j()));
            }
            0
        }
        Ok(s) => {
            let why = first_line(&s.err);
            eprintln!("tempo-launch: fleet check: ssh to {who} failed: {why}\n(Tempo uses your own ssh keys or agent and never asks for a password.)");
            if let Some(m) = m {
                record_ssh(p, &m.name, false, &why, None);
            }
            1
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet check: {e}");
            if let Some(m) = m {
                record_ssh(p, &m.name, false, &e, None);
            }
            1
        }
    }
}

/// The script that puts the key and the fleet's names on the other computer. The key travels on
/// ssh's input, never on a command line.
fn install_script(key: &[u8; 32], name: &str, peers: &[(String, String)]) -> String {
    let peers: String = peers
        .iter()
        .map(|(n, a)| format!(" --peer {n}={a}"))
        .collect();
    format!(
        "PATH=\"$PATH:$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin\"\ntempo-launch fleet install-key --name {name}{peers} <<'TEMPOKEY'\n{}\nTEMPOKEY\n",
        crypto::hex(key)
    )
}

pub fn setup_cmd(
    p: &Paths,
    pinned: Option<PathBuf>,
    who: &str,
    yes: bool,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let (target, m) = match ssh_target(p, who) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tempo-launch: fleet setup: {e}");
            return 2;
        }
    };
    let Some(m) = m else {
        eprintln!("tempo-launch: fleet setup: add {who} to the fleet first: tempo-launch fleet add <its address> --name {who}");
        return 2;
    };
    if m.addr.parse::<std::net::IpAddr>().is_err() {
        eprintln!("tempo-launch: fleet setup: give {} as an address (Ollama is told to listen on it): tempo-launch fleet add <address> --name {}", m.name, m.name);
        return 2;
    }
    let with_launch = {
        let a = ssh::args_for(&target, &["sh".into(), "-s".into()]);
        match a.and_then(|a| ssh::run(&a, Some(ssh::CHECK_SCRIPT), Duration::from_secs(30))) {
            Ok(s) if s.ok => {
                let c = ssh::parse_check(&s.out);
                for l in c.lines() {
                    let _ = writeln!(out, "  {l}");
                }
                record_ssh(p, &m.name, true, "ok", Some(c.to_j()));
                !c.launch.is_empty()
            }
            Ok(s) => {
                eprintln!(
                    "tempo-launch: fleet setup: ssh to {who} failed: {}",
                    first_line(&s.err)
                );
                return 1;
            }
            Err(e) => {
                eprintln!("tempo-launch: fleet setup: {e}");
                return 1;
            }
        }
    };
    let _ = store::init_key(p);
    let Some(key) = store::read_key(p) else {
        eprintln!("tempo-launch: fleet setup: the fleet key could not be made");
        return 1;
    };
    let me_addr = scan::listen_addrs(&scan::local_addrs())
        .first()
        .map(|a| a.to_string());
    let me = store::load(p).me;
    let mut steps = ssh::setup_steps(&m.addr, with_launch);
    let mut peers = vec![];
    let my_id = pair::identity(p)
        .map(|sk| crypto::hex(sk.verifying_key().as_bytes()))
        .unwrap_or_default();
    if let Some(a) = me_addr {
        peers.push((me.clone(), format!("{a}#{my_id}")));
    }
    steps.insert(
        0,
        ssh::Step {
            why: "the fleet key goes to the other computer, over ssh (it is not shown), with this computer's name and address",
            shown: format!(
                "tempo-launch fleet install-key --name {} {}   (the key is sent on ssh's input)",
                m.name,
                peers.iter().map(|(n, a)| format!("--peer {n}={a}")).collect::<Vec<_>>().join(" ")
            ),
            script: install_script(&key, &m.name, &peers),
        },
    );
    if with_launch {
        steps.push(ssh::Step {
            why: "the fleet messenger starts there",
            shown: "nohup tempo-launch fleet serve --quiet >/dev/null 2>&1 &".into(),
            script: "PATH=\"$PATH:$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin\"\nnohup tempo-launch fleet serve --quiet >/dev/null 2>&1 &\n".into(),
        });
    }
    let _ = writeln!(
        out,
        "\nOn {} ({target}) Tempo will run, over your own ssh:\n",
        m.name
    );
    for (i, s) in steps.iter().enumerate() {
        let _ = writeln!(
            out,
            "{}. {}\n{}\n",
            i + 1,
            s.why,
            s.shown
                .trim_end()
                .lines()
                .map(|l| format!("     {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    if !yes && !ask("Run these now? [y/N] ") {
        let _ = writeln!(out, "Nothing was run.");
        return 0;
    }
    let mut remote_id = String::new();
    for (i, s) in steps.iter().enumerate() {
        let r = ssh::args_for(&target, &["sh".into(), "-s".into()])
            .and_then(|a| ssh::run(&a, Some(&s.script), Duration::from_secs(900)));
        match r {
            Ok(said) => {
                // the other computer's identity, to pin: its line is kept, not shown
                for l in said.out.lines() {
                    match l.strip_prefix("identity=") {
                        Some(id) if id.len() == 64 && crypto::unhex(id).is_some() => {
                            remote_id = id.to_string()
                        }
                        _ => {
                            let _ = writeln!(out, "{l}");
                        }
                    }
                }
                if !said.ok {
                    eprintln!(
                        "tempo-launch: fleet setup: step {} failed: {}",
                        i + 1,
                        first_line(&said.err)
                    );
                    return 1;
                }
            }
            Err(e) => {
                eprintln!("tempo-launch: fleet setup: step {} failed: {e}", i + 1);
                return 1;
            }
        }
    }
    let role = if m.engine() {
        m.role.clone()
    } else {
        "both".into()
    };
    let code = add(p, pinned, &m.addr, Some(&role), Some(&m.name), out);
    if code == 0 && !remote_id.is_empty() {
        let _ = store::update(p, |f| {
            if let Some(x) = f.machines.iter_mut().find(|x| x.name == m.name) {
                x.id = remote_id.clone();
            }
        });
    } else if code == 0 {
        let _ = writeln!(out, "Its identity was not reported (an older Tempo there?), so its messages will not be trusted yet. Update Tempo there and run `fleet setup {}` again, or add it with `fleet invite` and `fleet join`.", m.name);
    }
    if code == 0 {
        let _ = writeln!(out, "{} is set up and linked here.", m.name);
    }
    code
}

/// `fleet install-key`: what the other end of `fleet setup` runs. The key comes on standard
/// input as hex; an existing different key is kept aside, never overwritten.
pub fn install_key(
    p: &Paths,
    name: Option<&str>,
    peers: &[String],
    stdin: &str,
    out: &mut dyn Write,
) -> i32 {
    let Some(key) = crypto::unhex(stdin.trim()).filter(|k| k.len() == 32) else {
        eprintln!(
            "tempo-launch: fleet install-key: the key on standard input is not 64 hex characters"
        );
        return 2;
    };
    if let Some(old) = store::read_key(p) {
        if old[..] != key[..] {
            let _ = std::fs::copy(p.file("fleet.key"), p.file("fleet.key.old"));
        }
    }
    if let Err(e) = crypto::write_private(&p.file("fleet.key"), &key) {
        eprintln!("tempo-launch: fleet install-key: {e}");
        return 1;
    }
    let r = store::update(p, |f| {
        if let Some(n) = name {
            f.me = store::clean_name(n);
        }
        for pe in peers {
            // `name=address`, or `name=address#identity` (the identity to pin, 64 hex characters)
            if let Some((n, rest)) = pe.split_once('=') {
                let (a, id) = rest.split_once('#').unwrap_or((rest, ""));
                let n = store::clean_name(n);
                if allowed_remote_host(a) && !n.is_empty() && f.find(&n).is_none() {
                    f.machines.push(Machine::new(&n, a, "both"));
                }
                if let (Some(i), true) = (f.find(&n), id.len() == 64 && crypto::unhex(id).is_some())
                {
                    f.machines[i].id = id.to_string();
                }
            }
        }
    });
    if let Err(e) = r {
        eprintln!("tempo-launch: fleet install-key: {e}");
        return 1;
    }
    let _ = writeln!(out, "The fleet key is in place.");
    // this computer's identity, for the other side to pin
    if let Ok(sk) = pair::identity(p) {
        let _ = writeln!(
            out,
            "identity={}",
            crypto::hex(sk.verifying_key().as_bytes())
        );
    }
    0
}

// ---- messages

fn machine(p: &Paths, who: &str) -> Result<Machine, String> {
    let f = store::load(p);
    f.find(who)
        .map(|i| f.machines[i].clone())
        .ok_or_else(|| format!("no computer called {who} in the fleet"))
}

fn node(p: &Paths) -> Result<std::sync::Arc<msg::Node>, String> {
    let key = store::read_key(p).ok_or("there is no fleet key yet; run tempo-launch fleet init")?;
    msg::Node::open(p, key)
}

pub fn send_cmd(p: &Paths, who: &str, text: &str, out: &mut dyn Write) -> i32 {
    let r = machine(p, who)
        .and_then(|m| node(p).and_then(|n| n.send(&m, "note", text.as_bytes()).map(|d| (m, d))));
    match r {
        Ok((m, true)) => {
            let _ = writeln!(out, "Sent to {}.", m.name);
            0
        }
        Ok((m, false)) => {
            let _ = writeln!(out, "{} is not answering. The note is kept and goes out when it is up (fleet serve retries with backoff).", m.name);
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet send: {e}");
            1
        }
    }
}

pub fn inbox_cmd(p: &Paths, out: &mut dyn Write) -> i32 {
    match node(p) {
        Ok(n) => {
            let msgs = n.inbox(None, 50);
            if msgs.is_empty() {
                let _ = writeln!(out, "The inbox is empty.");
            }
            for m in msgs.iter().rev() {
                let text = String::from_utf8_lossy(&m.body);
                let shown: String = text.chars().take(200).collect();
                let _ = writeln!(
                    out,
                    "[{}] {} ({}): {}",
                    crate::stamp(m.created / 1000),
                    m.peer,
                    m.topic,
                    shown
                );
            }
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet inbox: {e}");
            1
        }
    }
}

pub fn ping_cmd(p: &Paths, who: &str, out: &mut dyn Write) -> i32 {
    let r = machine(p, who).and_then(|m| {
        let n = node(p)?;
        let first = n.ping(&m)?; // includes opening the connection
        let warm = n.ping(&m)?;
        Ok((m, first, warm))
    });
    match r {
        Ok((m, first, warm)) => {
            let _ = writeln!(out, "{} answered: {warm} us round trip on the open connection ({first} us the first time, with the connection opened).", m.name);
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet ping: {e}");
            1
        }
    }
}

pub fn init_cmd(p: &Paths, out: &mut dyn Write) -> i32 {
    match store::init_key(p) {
        Ok(made) => {
            let _ = store::update(p, |_| ());
            let _ = writeln!(
                out,
                "{} {}. Every message between your computers is signed with it; `fleet setup` copies it to a computer over your ssh.",
                if made { "Made the fleet key" } else { "The fleet key is already there" },
                p.file("fleet.key").display()
            );
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet init: {e}");
            1
        }
    }
}

// ---- what an AI may see and propose (the MCP tools)

fn sorted(f: &Fleet) -> Vec<&Machine> {
    let mut v: Vec<&Machine> = f.machines.iter().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// The fleet as `fleet_status` tells it: counts, roles and health, with a label ("machine-2")
/// in place of each name and no address.
pub fn status_facts(p: &Paths) -> J {
    let f = store::load(p);
    let now = store::now_s();
    let ms: Vec<J> = sorted(&f)
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let up =
                m.enabled && m.last_seen > 0 && now.saturating_sub(m.last_seen) < serve::AWAY_S;
            J::Obj(vec![
                ("label".into(), J::s(&format!("machine-{}", i + 1))),
                ("role".into(), J::s(&m.role)),
                ("in_the_fleet".into(), J::Bool(m.enabled)),
                ("answering".into(), J::Bool(up)),
                (
                    "load".into(),
                    m.health.get("load").cloned().unwrap_or(J::Null),
                ),
                (
                    "memory_gb_free".into(),
                    m.health.get("mem_free_gb").cloned().unwrap_or(J::Null),
                ),
                (
                    "models_loaded".into(),
                    J::n(m.health.strs("models").len() as f64),
                ),
                (
                    "ssh_checked_ok".into(),
                    m.ssh.bool("ok").map_or(J::Null, J::Bool),
                ),
            ])
        })
        .collect();
    J::Obj(vec![
        ("heading".into(), J::s(&heading(f.count()))),
        ("machines_in_the_fleet".into(), J::n(f.count() as f64)),
        ("other_machines".into(), J::Arr(ms)),
        (
            "fleet_key_made".into(),
            J::Bool(store::read_key(p).is_some()),
        ),
        (
            "proposal_waiting_for_the_owner".into(),
            J::Bool(p.file(PROPOSAL_FILE).is_file()),
        ),
    ])
}

/// Write a proposal for the owner: `add` (each `{addr, name?, role?}`), `enable`, `disable` and
/// `remove` (labels from `fleet_status`, or names). Nothing changes until `fleet apply-proposal`.
pub fn write_proposal(p: &Paths, args: &J, note: &str, now: u64) -> Result<&'static str, String> {
    let mut doc = vec![
        (
            "proposed_by".to_string(),
            J::s("an AI, through tempo-launch mcp"),
        ),
        ("proposed_at".into(), J::s(&crate::stamp(now))),
        (
            "note".into(),
            J::s(&crate::setup::propose::clean_note(note)),
        ),
    ];
    let mut total = 0;
    let adds = args.arr("add");
    let mut checked = Vec::new();
    for a in adds {
        let addr = a.str("addr").unwrap_or("").trim().to_ascii_lowercase();
        if !allowed_remote_host(&addr) {
            return Err(format!(
                "add: {addr:?} is not a home-network, tailnet or private address"
            ));
        }
        let role = a.str("role").unwrap_or("engine");
        if !["engine", "agent", "both"].contains(&role) {
            return Err("add: role is engine, agent or both".into());
        }
        let mut o = vec![
            ("addr".to_string(), J::s(&addr)),
            ("role".into(), J::s(role)),
        ];
        if let Some(n) = a.str("name") {
            o.push(("name".into(), J::s(&store::clean_name(n))));
        }
        checked.push(J::Obj(o));
    }
    total += checked.len();
    doc.push(("add".into(), J::Arr(checked)));
    for key in ["enable", "disable", "remove"] {
        let names: Vec<String> = args
            .strs(key)
            .iter()
            .map(|n| store::clean_name(n))
            .filter(|n| !n.is_empty())
            .collect();
        total += names.len();
        doc.push((key.into(), J::strings(&names)));
    }
    if total == 0 || total > 16 {
        return Err("a proposal holds 1 to 16 changes (add, enable, disable, remove)".into());
    }
    crate::write_atomic(&p.file(PROPOSAL_FILE), J::Obj(doc).pretty().as_bytes())
        .map_err(|e| format!("could not write the proposal: {e}"))?;
    Ok(APPLY_COMMAND)
}

/// A label from `fleet_status` or a name, as the name of a machine in the fleet.
fn resolve(f: &Fleet, who: &str) -> Option<String> {
    if let Some(n) = who
        .strip_prefix("machine-")
        .and_then(|n| n.parse::<usize>().ok())
    {
        return sorted(f).get(n.checked_sub(1)?).map(|m| m.name.clone());
    }
    f.find(who).map(|i| f.machines[i].name.clone())
}

pub fn apply_proposal(
    p: &Paths,
    pinned: Option<PathBuf>,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    let Some(doc) = std::fs::read_to_string(p.file(PROPOSAL_FILE))
        .ok()
        .and_then(|t| parse(&t).ok())
    else {
        let _ = writeln!(out, "No proposal is waiting. (Your AI writes one with the fleet_propose tool of tempo-launch mcp.)");
        return 1;
    };
    let f = store::load(p);
    let _ = writeln!(out, "Your AI proposes these changes to your fleet:");
    let note = doc.str("note").unwrap_or("");
    if !note.is_empty() {
        let _ = writeln!(out, "  Its reason: {note}");
    }
    let mut todo: Vec<(&str, String, Option<J>)> = Vec::new();
    for a in doc.arr("add") {
        let addr = a.str("addr").unwrap_or("");
        let _ = writeln!(
            out,
            "  add      {addr} as {} ({})",
            a.str("name").unwrap_or("a name from its scan"),
            a.str("role").unwrap_or("engine")
        );
        todo.push(("add", addr.to_string(), Some(a.clone())));
    }
    for k in ["enable", "disable", "remove"] {
        for w in doc.strs(k) {
            match resolve(&f, &w) {
                Some(n) => {
                    let _ = writeln!(out, "  {k:<8} {n}");
                    todo.push((k, n, None));
                }
                None => {
                    let _ = writeln!(out, "  {k:<8} {w}: no such computer, skipped");
                }
            }
        }
    }
    if todo.is_empty() || !ask("Apply these? [y/N] ") {
        let _ = writeln!(out, "Nothing was changed.");
        return 0;
    }
    let mut code = 0;
    for (k, who, a) in todo {
        code |= match (k, a) {
            ("add", Some(a)) => add(p, pinned.clone(), &who, a.str("role"), a.str("name"), out),
            ("enable", _) => enable(p, pinned.clone(), &who, true, out),
            ("disable", _) => enable(p, pinned.clone(), &who, false, out),
            _ => remove(p, pinned.clone(), &who, out),
        };
    }
    let _ = std::fs::remove_file(p.file(PROPOSAL_FILE));
    code
}

// ---- adding a computer: invite and join

fn hello_answers(port: u16) -> bool {
    let a = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    crate::http_get_at(
        a,
        &format!("127.0.0.1:{port}"),
        "/hello",
        Duration::from_millis(500),
    )
    .is_some_and(|(c, b)| c == 200 && b.contains("tempo-fleet"))
}

/// `fleet invite`: a code for the new computer, and the messenger running to hear it. With
/// `wait`, stays until the code is used or runs out and says who joined.
pub fn invite_cmd(p: &Paths, wait: bool, out: &mut dyn Write) -> i32 {
    if let Err(e) = store::init_key(p).and_then(|_| pair::identity(p).map(|_| ())) {
        eprintln!("tempo-launch: fleet invite: {e}");
        return 1;
    }
    let port = store::fleet_port();
    if !hello_answers(port) {
        // the messenger has to be up to hear the new computer knock
        if let Ok(exe) = std::env::current_exe() {
            let mut c = std::process::Command::new(exe);
            // an invite is the moment to be found by name (mDNS), so the new computer finds this one at once
            c.args(["fleet", "serve", "--quiet", "--announce"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let _ = crate::spawn_hidden(&mut c);
        }
        for _ in 0..50 {
            if hello_answers(port) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !hello_answers(port) {
            eprintln!("tempo-launch: fleet invite: the messenger did not start. Run `tempo-launch fleet serve` in another window and try again.");
            return 1;
        }
    }
    let inv = match pair::create_invite(p, store::now_s()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("tempo-launch: fleet invite: {e}");
            return 1;
        }
    };
    let _ = writeln!(
        out,
        "Add a computer to the band.\n\nInstall Tempo on the new computer, then run this there:\n\n    tempo-launch fleet join\n\nWhen it asks, give it this code:\n\n        {}\n\nIt works once, for 10 minutes, and only on your own network. Waiting here for it.",
        inv.code
    );
    if !wait {
        return 0;
    }
    let before: Vec<String> = store::load(p)
        .machines
        .iter()
        .map(|m| m.name.clone())
        .collect();
    while store::now_s() < inv.expires && p.file(pair::INVITE_FILE).exists() {
        std::thread::sleep(Duration::from_millis(300));
    }
    let f = store::load(p);
    let new = f.machines.iter().find(|m| !before.contains(&m.name));
    match new {
        Some(m) => {
            let _ = writeln!(
                out,
                "\n{} joined the band as {}. The Conductor sees it already. Restart Tempo (`tempo-launch stop`, then `tempo-launch start`) so the agent can use its models too.",
                m.name,
                pair::role_words(&m.role)
            );
            0
        }
        None if store::now_s() >= inv.expires => {
            let _ = writeln!(
                out,
                "\nThe code ran out. Run `tempo-launch fleet invite` for a fresh one."
            );
            1
        }
        None => {
            let _ = writeln!(out, "\nThat code was used up by wrong tries. Run `tempo-launch fleet invite` for a fresh one.");
            1
        }
    }
}

pub struct JoinOpts {
    pub code: Option<String>,
    pub controller: Option<String>,
    pub yes: bool,
}

/// A main computer found on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub addr: std::net::SocketAddr,
    pub invite: bool,
}

/// Look for main computers: mDNS, then the same gentle sweep as `fleet scan`, asking only the
/// fleet port for its `/hello`.
pub fn discover() -> Vec<Candidate> {
    let own = scan::local_addrs();
    let mut targets = scan::ranges(&own);
    let mut hosts: Vec<std::net::Ipv4Addr> = vec![];
    for s in mdns::browse(
        mdns::TEMPO,
        &scan::listen_addrs(&own),
        Duration::from_millis(900),
    ) {
        if let Some(ip) = s.ip.filter(|ip| scan::is_lan(*ip) || scan::is_tailnet(*ip)) {
            hosts.push(ip);
        }
    }
    let tail = scan::tailnet_peers(
        &crate::setup::probe::run_with_timeout(
            "tailscale",
            &["status", "--json"],
            Duration::from_secs(5),
        )
        .unwrap_or_default(),
    );
    hosts.extend(tail.iter().map(|t| t.0));
    for h in hosts {
        if !targets.contains(&h) {
            targets.push(h);
        }
    }
    targets.retain(|t| !own.contains(t));
    probe_hello(&targets, store::fleet_port())
}

pub fn probe_hello(targets: &[std::net::Ipv4Addr], port: u16) -> Vec<Candidate> {
    scan::open_ports(targets, &[port], scan::CONNECT, scan::AT_ONCE)
        .into_iter()
        .filter_map(|(ip, port)| {
            let a = std::net::SocketAddr::from((ip, port));
            let (c, b) =
                crate::http_get_at(a, &format!("{ip}:{port}"), "/hello", Duration::from_secs(2))?;
            (c == 200 && b.contains("\"app\":\"tempo-fleet\"")).then(|| Candidate {
                name: crate::json_str(&b, "name").unwrap_or_else(|| ip.to_string()),
                addr: a,
                invite: b.contains("\"invite\":true"),
            })
        })
        .collect()
}

/// How `join` runs a command or a script: what it is for, the program and its arguments, and a
/// script for its input. True when it worked.
pub type Exec<'a> = &'a mut dyn FnMut(&str, &[String], Option<&str>) -> bool;

pub fn real_exec(_why: &str, argv: &[String], stdin: Option<&str>) -> bool {
    let Some((prog, args)) = argv.split_first() else {
        return false;
    };
    let mut c = std::process::Command::new(prog);
    c.args(args);
    if stdin.is_some() {
        c.stdin(std::process::Stdio::piped());
    }
    match c.spawn() {
        Ok(mut ch) => {
            if let (Some(t), Some(mut si)) = (stdin, ch.stdin.take()) {
                let _ = si.write_all(t.as_bytes());
            }
            ch.wait().is_ok_and(|s| s.success())
        }
        Err(_) => false,
    }
}

/// After joining: this computer gets ready to play. Ollama listens on the private address (the
/// engine only), Tempo sets itself up, and the messenger starts and is kept running.
pub fn configure_self(ctx: &Ctx, role: &str, out: &mut dyn Write, exec: Exec) {
    let own = scan::listen_addrs(&scan::local_addrs());
    if role == "engine" {
        match (own.first(), cfg!(unix)) {
            (Some(a), true) => {
                let _ = writeln!(out, "Starting Ollama on {a}, for your own network only...");
                let step = &ssh::setup_steps(&a.to_string(), true)[0];
                if !exec("ollama", &["sh".into(), "-s".into()], Some(&step.script)) {
                    let _ = writeln!(
                        out,
                        "That did not work. By hand: OLLAMA_HOST={a}:11434 ollama serve"
                    );
                }
            }
            (Some(a), false) => {
                let _ = writeln!(out, "To let the band use this computer's models, set OLLAMA_HOST to {a}:11434 (Windows: `setx OLLAMA_HOST {a}:11434`), then restart Ollama.");
            }
            _ => {}
        }
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tempo-launch"));
    let _ = writeln!(
        out,
        "Getting Tempo ready here (it fetches what is missing and asks nothing)..."
    );
    let argv = [
        exe.to_string_lossy().into_owned(),
        "setup".into(),
        "--apply".into(),
        "--yes".into(),
    ];
    if !exec("setup", &argv, None) {
        let _ = writeln!(
            out,
            "Setup did not finish. Run `tempo-launch setup --apply` when you are ready."
        );
    }
    schedule(ctx, &exe, false);
    autostart(ctx);
    let _ = writeln!(out, "The messenger is running here and starts with this computer. See the band on `tempo-launch fleet page`.");
}

/// `fleet join`: find the main computer, say who it is, ask for the code, and play along.
pub fn join_cmd(
    ctx: &Ctx,
    p: &Paths,
    o: &JoinOpts,
    input: &mut dyn std::io::BufRead,
    out: &mut dyn Write,
    exec: Exec,
) -> i32 {
    let target = match &o.controller {
        Some(c) => {
            let (h, port) = c
                .rsplit_once(':')
                .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h, p)))
                .unwrap_or((c.as_str(), store::fleet_port()));
            match private_v4(h) {
                Some(ip) => Candidate {
                    name: h.to_string(),
                    addr: std::net::SocketAddr::from((ip, port)),
                    invite: true,
                },
                None => {
                    eprintln!("tempo-launch: fleet join: {h} is not a home-network, tailnet or private address");
                    return 2;
                }
            }
        }
        None => {
            let _ = writeln!(out, "Looking for your main computer on your own network...");
            let found = discover();
            let ready: Vec<&Candidate> = found.iter().filter(|c| c.invite).collect();
            match (ready.as_slice(), found.as_slice()) {
                ([one], _) => (*one).clone(),
                ([], []) => {
                    let _ = writeln!(out, "Nobody answered. On your main computer run `tempo-launch fleet invite`, then try again. If it is on another network, use `tempo-launch fleet join --controller ADDRESS`.");
                    return 1;
                }
                ([], _) => {
                    let _ = writeln!(
                        out,
                        "Found {}, but it is not showing a code. Run `tempo-launch fleet invite` there, then try again.",
                        found.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join(", ")
                    );
                    return 1;
                }
                (many, _) => {
                    for (i, c) in many.iter().enumerate() {
                        let _ = writeln!(out, "  {}. {} ({})", i + 1, c.name, c.addr.ip());
                    }
                    let _ = write!(out, "Which one? ");
                    let _ = out.flush();
                    let mut l = String::new();
                    let _ = input.read_line(&mut l);
                    match l
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| many.get(n.wrapping_sub(1)))
                    {
                        Some(c) => (*c).clone(),
                        None => {
                            let _ = writeln!(out, "No such number. Nothing was done.");
                            return 1;
                        }
                    }
                }
            }
        }
    };
    if !o.yes && o.controller.is_none() {
        let _ = write!(out, "Join {} ({})? [y/N] ", target.name, target.addr.ip());
        let _ = out.flush();
        let mut l = String::new();
        let _ = input.read_line(&mut l);
        if !matches!(l.trim().to_lowercase().as_str(), "y" | "yes") {
            let _ = writeln!(out, "Okay, nothing was done.");
            return 0;
        }
    }
    let code = match &o.code {
        Some(c) => c.clone(),
        None => {
            let _ = write!(out, "Type the code from the main computer's screen: ");
            let _ = out.flush();
            let mut l = String::new();
            let _ = input.read_line(&mut l);
            l
        }
    };
    let probe = pair::probe_json(p);
    let me = store::hostname();
    match pair::join(p, target.addr, &code, &me, store::fleet_port(), &probe) {
        Ok(j) => {
            if let Err(e) = pair::keep(p, &j, &target.addr.ip().to_string()) {
                eprintln!("tempo-launch: fleet join: {e}");
                return 1;
            }
            let _ = writeln!(
                out,
                "Joined the band. {} knows this computer as {}, and gave it the part of {}.",
                j.main,
                j.you_are,
                pair::role_words(&j.role)
            );
            configure_self(ctx, &j.role, out, exec);
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet join: {e}. Check the code on the other screen, or run `tempo-launch fleet invite` there for a fresh one.");
            1
        }
    }
}

pub fn allow_cmd(p: &Paths, list: &[String], on: bool, out: &mut dyn Write) -> i32 {
    let r = store::update(p, |f| {
        for e in list {
            f.allow.retain(|a| a != e);
            if on {
                f.allow.push(e.clone());
            }
        }
        f.allow.clone()
    });
    match r {
        Ok(a) => {
            let _ = writeln!(
                out,
                "{} {}. This computer now offers the band: {}.",
                if on { "Allowed" } else { "No longer offered" },
                list.join(", "),
                if a.is_empty() {
                    "nothing".to_string()
                } else {
                    a.join(", ")
                }
            );
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: fleet: {e}");
            1
        }
    }
}

fn gib_words(bytes: u64) -> String {
    let g = bytes as f64 / store::GIB as f64;
    if g.fract() == 0.0 {
        format!("{g:.0} GiB")
    } else {
        format!("{g:.2} GiB")
    }
}

/// `fleet cap [GIB]`: the most one file may be when it crosses between computers (a clip going
/// to another computer's Stage, or a render coming back). Each computer keeps its own: the
/// receiver turns away a bigger file, so raise it on both ends.
pub fn cap_cmd(p: &Paths, gib: Option<f64>, out: &mut dyn Write) -> i32 {
    if let Some(g) = gib {
        if let Err(e) = store::update(p, |f| f.blob_cap = (g * store::GIB as f64) as u64) {
            eprintln!("tempo-launch: fleet: {e}");
            return 1;
        }
    }
    let cap = store::load(p).blob_cap();
    let _ = writeln!(
        out,
        "A file may be {} between computers here ({} unless you change it: `tempo-launch fleet cap GIB`). Each computer keeps its own number; the one that takes the file turns away a bigger one.",
        gib_words(cap),
        gib_words(store::DEFAULT_BLOB_CAP),
    );
    0
}

/// `fleet rotate`: a new fleet key. It goes to each computer over your ssh first, then this
/// computer switches. A computer it cannot reach keeps the old key and drops out of the
/// conversation until `fleet setup NAME` (or a new `fleet invite`) gives it the new one.
pub fn rotate_cmd(
    p: &Paths,
    yes: bool,
    ask: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
) -> i32 {
    if store::read_key(p).is_none() {
        eprintln!(
            "tempo-launch: fleet rotate: there is no fleet key yet; run tempo-launch fleet init"
        );
        return 1;
    }
    let f = store::load(p);
    let names: Vec<&str> = f.peers().map(|m| m.name.as_str()).collect();
    let _ = writeln!(
        out,
        "A new fleet key will be sent over your ssh to: {}. Then this computer switches to it. Anyone who has the old key (a copied file, a lost laptop) is shut out once everyone has switched.",
        if names.is_empty() { "nobody else yet".to_string() } else { names.join(", ") }
    );
    if !yes && !ask("Rotate the key? [y/N] ") {
        let _ = writeln!(out, "Okay, the key stays as it is.");
        return 0;
    }
    let key: [u8; 32] = crypto::random(32).try_into().unwrap_or([0; 32]);
    let mut missed = Vec::new();
    for m in f.peers() {
        let sent = ssh_target(p, &m.name).and_then(|(t, _)| {
            ssh::args_for(&t, &["sh".into(), "-s".into()])
                .and_then(|a| {
                    ssh::run(
                        &a,
                        Some(&install_script(&key, &m.name, &[])),
                        Duration::from_secs(30),
                    )
                })
                .and_then(|s| {
                    if s.ok {
                        Ok(())
                    } else {
                        Err(first_line(&s.err))
                    }
                })
        });
        match sent {
            Ok(()) => {
                let _ = writeln!(out, "  {} has the new key.", m.name);
            }
            Err(e) => {
                let _ = writeln!(out, "  {} could not be reached over ssh ({e}).", m.name);
                missed.push(m.name.clone());
            }
        }
    }
    if let Err(e) = std::fs::copy(p.file("fleet.key"), p.file("fleet.key.old"))
        .and_then(|_| crypto::write_private(&p.file("fleet.key"), &key))
    {
        eprintln!("tempo-launch: fleet rotate: {e}");
        return 1;
    }
    let _ = writeln!(
        out,
        "This computer has the new key; a running messenger picks it up within a couple of seconds."
    );
    if !missed.is_empty() {
        let _ = writeln!(
            out,
            "Still on the old key: {}. Give each the new one with {}.",
            missed.join(", "),
            missed
                .iter()
                .map(|n| format!("`tempo-launch fleet setup {n}`"))
                .collect::<Vec<_>>()
                .join(" and ")
        );
        return 1;
    }
    0
}

// ---- starting with Tempo

/// `tempo-launch start`: when the fleet has more than one machine, start the messenger (it ends
/// at once if one is already running).
pub fn autostart(ctx: &Ctx) {
    let p = Paths::of(ctx);
    if store::read_key(&p).is_none() || store::load(&p).count() < 2 {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let log = crate::open_log(&ctx.log_path("fleet")).ok();
    let mut c = std::process::Command::new(exe);
    c.args(["fleet", "serve", "--quiet"])
        .stdin(std::process::Stdio::null());
    match log.and_then(|l| l.try_clone().ok().map(|l2| (l, l2))) {
        Some((a, b)) => c.stdout(a).stderr(b),
        None => c
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    };
    if let Err(e) = crate::spawn_hidden(&mut c) {
        ctx.note(&format!("fleet: could not start the messenger: {e}"));
    }
}

/// `tempo-launch schedule`: keep the messenger running when there is a fleet.
pub fn schedule(ctx: &Ctx, exe: &std::path::Path, remove: bool) {
    let p = Paths::of(ctx);
    if !remove && (store::read_key(&p).is_none() || store::load(&p).count() < 2) {
        return;
    }
    match service::install(exe, remove) {
        Ok(w) => ctx.note(&format!("schedule: {w}")),
        Err(e) => ctx.note(&format!("schedule: fleet messenger: {e}")),
    }
}

// ---- the dispatcher

pub fn run(ctx: &Ctx, cmd: FleetCmd) -> i32 {
    let p = Paths::of(ctx);
    let pinned = crate::setup::propose::real_pinned();
    let mut out = std::io::stdout();
    let mut ask = crate::setup::apply::ask_terminal;
    match cmd {
        FleetCmd::List => {
            list(&p, &mut out);
            0
        }
        FleetCmd::Scan { yes } => scan_cmd(&p, yes, &mut ask, &mut out).0,
        FleetCmd::Add { who, role, name } => {
            add(&p, pinned, &who, role.as_deref(), name.as_deref(), &mut out)
        }
        FleetCmd::Remove(w) => remove(&p, pinned, &w, &mut out),
        FleetCmd::Enable(w, on) => enable(&p, pinned, &w, on, &mut out),
        FleetCmd::Init => init_cmd(&p, &mut out),
        FleetCmd::Hosts => {
            let h = ssh::user_hosts();
            if h.is_empty() {
                println!("No Host entries in ~/.ssh/config.");
            }
            for n in h {
                println!("{n}");
            }
            0
        }
        FleetCmd::Ssh { who, cmd } => ssh_cmd(&p, &who, &cmd, &mut out),
        FleetCmd::Check(w) => check_cmd(&p, &w, &mut out),
        FleetCmd::Setup { who, yes } => setup_cmd(&p, pinned, &who, yes, &mut ask, &mut out),
        FleetCmd::Serve { quiet, announce } => serve::run(&p, quiet, announce),
        FleetCmd::Send { who, text } => send_cmd(&p, &who, &text, &mut out),
        FleetCmd::Inbox => inbox_cmd(&p, &mut out),
        FleetCmd::Ping(w) => ping_cmd(&p, &w, &mut out),
        FleetCmd::Page => match page::write(ctx) {
            Ok(f) => {
                let _ = ctx.open(&f.to_string_lossy());
                0
            }
            Err(e) => {
                eprintln!("tempo-launch: fleet page: {e}");
                1
            }
        },
        FleetCmd::ApplyProposal => apply_proposal(&p, pinned, &mut ask, &mut out),
        FleetCmd::Invite => invite_cmd(&p, true, &mut out),
        FleetCmd::Join {
            code,
            controller,
            yes,
        } => join_cmd(
            ctx,
            &p,
            &JoinOpts {
                code,
                controller,
                yes,
            },
            &mut std::io::stdin().lock(),
            &mut out,
            &mut real_exec,
        ),
        FleetCmd::Allow(list, on) => allow_cmd(&p, &list, on, &mut out),
        FleetCmd::Cap(g) => cap_cmd(&p, g, &mut out),
        FleetCmd::Rotate { yes } => rotate_cmd(&p, yes, &mut ask, &mut out),
        FleetCmd::InstallKey { name, peers } => {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin().take(4096), &mut s);
            install_key(&p, name.as_deref(), &peers, &s, &mut out)
        }
    }
}
