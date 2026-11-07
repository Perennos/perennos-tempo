//! Tempo Fleet: Tempo knows the person's own computers and talks between them.
//!
//! `scan` finds them on their own networks (opt-in, gentle, private addresses only), `add` and
//! `remove` choose which are in the fleet (and write the agent's settings), `ssh`, `check` and
//! `setup` reach them with the person's own ssh, and `serve`, `send`, `inbox` and `ping` are the
//! messenger between them: signed, durable, fast. See docs/TEMPO-SETUP.md ("Tempo Fleet").

pub mod crypto;
pub mod jobs;
pub mod mdns;
pub mod msg;
pub mod page;
pub mod scan;
pub mod serve;
pub mod service;
pub mod ssh;
pub mod store;

#[cfg(test)]
mod tests;

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
        Some("serve") => match &rest[1..] {
            [] => FleetCmd::Serve { quiet: false },
            ["--quiet"] => FleetCmd::Serve { quiet: true },
            _ => return Err("fleet serve takes only --quiet".into()),
        },
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

pub const SCAN_WORDS: &str = "What a scan does: it looks at the addresses on this computer's own home or office networks (the /24 around each of its addresses), the computers your tailnet lists, and mDNS. At each address it tries to connect, for a third of a second, to the ports of Ollama (11434), the Tempo agent (8765) and the Tempo fleet messenger (8770), at most 64 at a time; where something answers it reads one public page (the model list, or the health answer). It sends no password and no file, never looks at a public address, and changes nothing. Adding a computer to the fleet is a separate step, and yours.";

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
    if let Some(a) = me_addr {
        peers.push((me.clone(), a));
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
    for (i, s) in steps.iter().enumerate() {
        let r = ssh::args_for(&target, &["sh".into(), "-s".into()])
            .and_then(|a| ssh::run(&a, Some(&s.script), Duration::from_secs(900)));
        match r {
            Ok(said) => {
                let _ = write!(out, "{}", said.out);
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
            if let Some((n, a)) = pe.split_once('=') {
                let n = store::clean_name(n);
                if allowed_remote_host(a) && !n.is_empty() && f.find(&n).is_none() {
                    f.machines.push(Machine::new(&n, a, "both"));
                }
            }
        }
    });
    if let Err(e) = r {
        eprintln!("tempo-launch: fleet install-key: {e}");
        return 1;
    }
    let _ = writeln!(out, "The fleet key is in place.");
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
        FleetCmd::Serve { quiet } => serve::run(&p, quiet),
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
        FleetCmd::InstallKey { name, peers } => {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin().take(4096), &mut s);
            install_key(&p, name.as_deref(), &peers, &s, &mut out)
        }
    }
}
