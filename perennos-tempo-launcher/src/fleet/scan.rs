//! `tempo-launch fleet scan`: finds the person's own computers on their own networks.
//!
//! Where it looks: the /24 around each of this computer's own home-network addresses (10/8,
//! 172.16/12, 192.168/16; never link-local, never a public range), the tailnet machines that
//! `tailscale status --json` lists (the 100.64/10 range is never swept), and mDNS. What it does:
//! a TCP connect with a 300 ms limit to the ports of Ollama, the Tempo agent and the fleet
//! messenger, at most 64 at once, then one plain GET on the ones that answered. It sends no
//! credential and changes nothing.

use super::store::{fleet_port, Fleet};
use crate::conductor::engines::{models_in, Engine, Model};
use crate::setup::json::{parse, J};
use crate::setup::probe::run_with_timeout;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub const CONNECT: Duration = Duration::from_millis(300);
pub const AT_ONCE: usize = 64;
/// More than this many addresses is more than a home: the scan stops there.
const MAX_HOSTS: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct Ports {
    pub ollama: u16,
    pub agent: u16,
    pub fleet: u16,
    pub ssh: u16,
}

pub fn real_ports() -> Ports {
    Ports {
        ollama: crate::setup::apply::ollama_port(),
        agent: crate::AGENT.port,
        fleet: fleet_port(),
        ssh: 22,
    }
}

/// The IPv4 addresses a command like `ip -o addr`, `ifconfig` or `ipconfig` printed.
pub fn parse_addrs(text: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cand = if let Some((_, r)) = line.split_once("inet ") {
            r.split(|c: char| c.is_whitespace() || c == '/').next()
        } else if line.contains("IPv4") {
            line.rsplit(':').next().map(str::trim)
        } else {
            None
        };
        if let Some(ip) = cand.and_then(|c| c.trim_end_matches("(Preferred)").parse().ok()) {
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

/// This computer's own IPv4 addresses.
pub fn local_addrs() -> Vec<Ipv4Addr> {
    let t = Duration::from_secs(2);
    let mut out = if cfg!(windows) {
        run_with_timeout("ipconfig", &[], t).unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        run_with_timeout("ifconfig", &[], t).unwrap_or_default()
    } else {
        run_with_timeout("ip", &["-4", "-o", "addr", "show"], t)
            .or_else(|| run_with_timeout("ifconfig", &[], t))
            .unwrap_or_default()
    };
    out.push('\n');
    let mut v = parse_addrs(&out);
    // the address the system would use to reach a private neighbour (no packet is sent)
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("10.255.255.254:9").is_ok() {
            if let Ok(SocketAddr::V4(a)) = s.local_addr() {
                if !v.contains(a.ip()) {
                    v.push(*a.ip());
                }
            }
        }
    }
    v
}

/// 10/8, 172.16/12, 192.168/16: the ranges a home or office network uses, and the only ones a
/// sweep may walk.
pub fn is_lan(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

/// 100.64/10: the tailnet range. Only listed peers are ever looked at.
pub fn is_tailnet(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Addresses the messenger may listen on: a private or tailnet address of this computer. Never
/// link-local, never public.
pub fn listen_addrs(own: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    own.iter()
        .copied()
        .filter(|ip| is_lan(*ip) || is_tailnet(*ip))
        .collect()
}

/// The private address this computer would use to reach a specific peer; UDP connect sends no packet.
pub fn route_address(destination: &str) -> Result<String, String> {
    let ip = destination
        .parse::<IpAddr>()
        .map_err(|_| "the peer address is not an IP")?;
    let bind = if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let socket = UdpSocket::bind(bind).map_err(|e| format!("could not inspect the route: {e}"))?;
    socket
        .connect(SocketAddr::new(ip, 9))
        .map_err(|e| format!("could not inspect the route: {e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("could not read the route: {e}"))?
        .ip();
    if !crate::allowed_remote_host(&local.to_string()) {
        return Err("the selected callback address is not private".into());
    }
    Ok(local.to_string())
}

/// Every other host of the /24 around each home-network address of this computer.
pub fn ranges(own: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for ip in own.iter().filter(|ip| is_lan(**ip)) {
        let o = ip.octets();
        for h in 1..=254u8 {
            let t = Ipv4Addr::new(o[0], o[1], o[2], h);
            if t != *ip && !out.contains(&t) && out.len() < MAX_HOSTS {
                out.push(t);
            }
        }
    }
    out
}

/// The tailnet peers (address and name) in `tailscale status --json`. Only addresses inside
/// 100.64/10 are taken.
pub fn tailnet_peers(json: &str) -> Vec<(Ipv4Addr, String)> {
    let Ok(J::Obj(top)) = parse(json) else {
        return vec![];
    };
    let Some((_, J::Obj(peers))) = top.iter().find(|(k, _)| k == "Peer") else {
        return vec![];
    };
    peers
        .iter()
        .filter_map(|(_, p)| {
            let ip = p
                .strs("TailscaleIPs")
                .iter()
                .find_map(|a| a.parse::<Ipv4Addr>().ok().filter(|i| is_tailnet(*i)))?;
            let name = p
                .str("HostName")
                .filter(|n| !n.is_empty())
                .or_else(|| p.str("DNSName"))?;
            Some((ip, super::store::clean_name(name)))
        })
        .collect()
}

fn tailscale() -> Vec<(Ipv4Addr, String)> {
    run_with_timeout("tailscale", &["status", "--json"], Duration::from_secs(5))
        .map(|t| tailnet_peers(&t))
        .unwrap_or_default()
}

/// Which of `ports` answer a TCP connect at each address: at most `AT_ONCE` attempts at a time.
pub fn open_ports(
    targets: &[Ipv4Addr],
    ports: &[u16],
    wait: Duration,
    at_once: usize,
) -> Vec<(Ipv4Addr, u16)> {
    let jobs: Vec<(Ipv4Addr, u16)> = targets
        .iter()
        .flat_map(|t| ports.iter().map(move |p| (*t, *p)))
        .collect();
    let next = AtomicUsize::new(0);
    let found = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..at_once.max(1).min(jobs.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((ip, port)) = jobs.get(i).copied() else {
                    break;
                };
                if TcpStream::connect_timeout(&SocketAddr::from((ip, port)), wait).is_ok() {
                    if let Ok(mut f) = found.lock() {
                        f.push((ip, port));
                    }
                }
            });
        }
    });
    let mut f = found.into_inner().unwrap_or_default();
    f.sort();
    f
}

/// One computer as the scan saw it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub addr: String,
    pub name: String,
    pub models: Option<Vec<Model>>,
    pub class: String,
    /// The Tempo agent's version, when one answers (empty: it answers but gave none).
    pub agent: Option<String>,
    /// The name a Tempo fleet messenger there gave itself.
    pub fleet: Option<String>,
    /// SSH answered, so the computer can be prepared after the person approves a plan.
    pub ssh: bool,
    pub in_fleet: bool,
}

impl Found {
    pub fn role(&self) -> &'static str {
        match (
            self.models.is_some(),
            self.agent.is_some() || self.fleet.is_some(),
        ) {
            (true, true) => "both",
            (true, false) => "engine",
            _ => "agent",
        }
    }

    pub fn line(&self) -> String {
        let mut what = Vec::new();
        if let Some(m) = &self.models {
            what.push(format!(
                "Ollama with {} model{} ({})",
                m.len(),
                if m.len() == 1 { "" } else { "s" },
                self.class
            ));
        }
        if let Some(v) = &self.agent {
            what.push(if v.is_empty() {
                "Tempo agent".into()
            } else {
                format!("Tempo agent {v}")
            });
        }
        if self.fleet.is_some() {
            what.push("fleet messenger".into());
        }
        if self.ssh && self.models.is_none() && self.agent.is_none() && self.fleet.is_none() {
            what.push("SSH reachable".into());
        }
        format!(
            "{:<15} {:<18} {}{}",
            self.addr,
            if self.name.is_empty() {
                "-"
            } else {
                &self.name
            },
            what.join(", "),
            if self.in_fleet {
                "  [in the fleet]"
            } else {
                ""
            }
        )
    }
}

/// One plain GET on what answered.
pub fn inspect(ip: Ipv4Addr, open: &[u16], ports: Ports) -> Found {
    let get = |port: u16, path: &str| {
        crate::http_get_at(
            SocketAddr::from((ip, port)),
            &format!("{ip}:{port}"),
            path,
            Duration::from_secs(2),
        )
        .filter(|(c, _)| *c == 200)
        .map(|x| x.1)
    };
    let mut f = Found {
        addr: ip.to_string(),
        ssh: open.contains(&ports.ssh),
        ..Found::default()
    };
    if open.contains(&ports.ollama) {
        if let Some(models) = get(ports.ollama, "/api/tags").and_then(|b| models_in(&b)) {
            let e = Engine {
                id: String::new(),
                url: String::new(),
                up: true,
                ms: 0,
                models: models.clone(),
                why: String::new(),
            };
            f.class = e.class().word().to_string();
            f.models = Some(models);
        }
    }
    if open.contains(&ports.agent) {
        if let Some(b) =
            get(ports.agent, "/agent/health").filter(|b| crate::signed(b, crate::AGENT.signs))
        {
            f.agent = Some(crate::json_str(&b, "version").unwrap_or_default());
        }
    }
    if open.contains(&ports.fleet) {
        if let Some(b) =
            get(ports.fleet, "/hello").filter(|b| crate::signed(b, &["\"app\":\"tempo-fleet\""]))
        {
            f.fleet = crate::json_str(&b, "name");
        }
    }
    f
}

fn parse_reverse(text: &str) -> Option<String> {
    for l in text.lines() {
        if let Some((_, n)) = l.split_once("name =") {
            return Some(n.trim().trim_end_matches('.').to_string());
        }
    }
    // `getent hosts ADDRESS`: "ADDRESS name aliases..."
    let mut w = text.split_whitespace();
    w.next()?;
    w.next().map(|n| n.trim_end_matches('.').to_string())
}

fn reverse_dns(ip: Ipv4Addr) -> Option<String> {
    let s = ip.to_string();
    let t = Duration::from_secs(1);
    let said = run_with_timeout("getent", &["hosts", &s], t)
        .or_else(|| run_with_timeout("nslookup", &[&s], t).filter(|o| o.contains("name =")))?;
    parse_reverse(&said).map(|n| super::store::clean_name(n.split('.').next().unwrap_or("")))
}

/// What the scan looks at, and in what order a name is found.
pub struct Plan {
    pub lan: Vec<Ipv4Addr>,
    pub tail: Vec<(Ipv4Addr, String)>,
    pub mdns: Vec<(Ipv4Addr, String)>,
}

/// Run the scan over `targets` (tests give their own).
pub fn probe_all(
    targets: &[Ipv4Addr],
    names: &[(Ipv4Addr, String)],
    ports: Ports,
    fleet: &Fleet,
) -> Vec<Found> {
    let mut list = vec![ports.ollama, ports.agent, ports.fleet, ports.ssh];
    list.dedup();
    let open = open_ports(targets, &list, CONNECT, AT_ONCE);
    let mut ips: Vec<Ipv4Addr> = open.iter().map(|x| x.0).collect();
    ips.dedup();
    let mut found: Vec<Found> = std::thread::scope(|s| {
        let hs: Vec<_> = ips
            .iter()
            .map(|ip| {
                let mine: Vec<u16> = open.iter().filter(|x| x.0 == *ip).map(|x| x.1).collect();
                s.spawn(move || inspect(*ip, &mine, ports))
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for f in &mut found {
        let ip: Ipv4Addr = f.addr.parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
        f.name = names
            .iter()
            .find(|n| n.0 == ip)
            .map(|n| n.1.clone())
            .or_else(|| f.fleet.clone())
            .or_else(|| reverse_dns(ip))
            .unwrap_or_default();
        f.in_fleet =
            fleet.find(&f.addr).is_some() || (!f.name.is_empty() && fleet.find(&f.name).is_some());
    }
    found
}

/// The scan as `fleet scan` runs it: the networks of this computer, the tailnet peers and mDNS.
pub fn scan(fleet: &Fleet, out: &mut dyn std::io::Write) -> Vec<Found> {
    let own = local_addrs();
    let lan = ranges(&own);
    let tail = tailscale();
    let mut names: Vec<(Ipv4Addr, String)> = tail.clone();
    let browsed: Vec<_> = [super::mdns::TEMPO, super::mdns::OLLAMA]
        .iter()
        .flat_map(|svc| super::mdns::browse(svc, &listen_addrs(&own), Duration::from_millis(900)))
        .collect();
    let mut targets = lan;
    for s in &browsed {
        // only a private address an mDNS answer names is looked at, and never twice
        if let Some(ip) = s.ip.filter(|ip| is_lan(*ip) || is_tailnet(*ip)) {
            names.push((
                ip,
                super::store::clean_name(s.host.trim_end_matches(".local")),
            ));
            if !own.contains(&ip) && !targets.contains(&ip) {
                targets.push(ip);
            }
        }
    }
    for (ip, _) in &tail {
        if !targets.contains(ip) {
            targets.push(*ip);
        }
    }
    let _ = writeln!(
        out,
        "Looking at {} address{} ({} on your own networks, {} tailnet peer{}).",
        targets.len(),
        if targets.len() == 1 { "" } else { "es" },
        targets.len() - tail.len().min(targets.len()),
        tail.len(),
        if tail.len() == 1 { "" } else { "s" }
    );
    probe_all(&targets, &names, real_ports(), fleet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
        Ipv4Addr::new(a, b, c, d)
    }

    #[test]
    fn parses_what_the_system_prints() {
        let linux = "2: eth0    inet 10.1.2.30/24 brd 10.1.2.255 scope global eth0\n1: lo    inet 127.0.0.1/8 scope host lo\n";
        assert_eq!(parse_addrs(linux), [ip(10, 1, 2, 30), ip(127, 0, 0, 1)]);
        let mac = "en0: flags=8863<UP>\n\tinet 172.20.5.9 netmask 0xffffff00 broadcast 172.20.5.255\n\tinet6 fe80::1 prefixlen 64\n";
        assert_eq!(parse_addrs(mac), [ip(172, 20, 5, 9)]);
        let win = "   IPv4 Address. . . . . . . . . . . : 10.0.0.4(Preferred) \n   Subnet Mask . . . : 255.255.255.0\n";
        assert_eq!(parse_addrs(win), [ip(10, 0, 0, 4)]);
    }

    #[test]
    fn the_sweep_stays_on_home_networks_and_never_goes_public() {
        let own = [
            ip(10, 1, 2, 30),
            ip(8, 8, 8, 8),        // public: never swept
            ip(169, 254, 3, 4),    // link-local: never swept
            ip(100, 64 + 1, 0, 1), // tailnet interface: never swept
            ip(127, 0, 0, 1),
            ip(172, 32, 0, 1), // just outside 172.16/12
        ];
        let t = ranges(&own);
        assert_eq!(t.len(), 253);
        assert!(t
            .iter()
            .all(|a| a.octets()[..3] == [10, 1, 2] && *a != own[0]));
        assert!(t.iter().all(|a| is_lan(*a)));
        // however many interfaces there are, the sweep has a limit
        let many: Vec<Ipv4Addr> = (0..20).map(|i| ip(10, i, 0, 5)).collect();
        assert!(ranges(&many).len() <= MAX_HOSTS);
        assert_eq!(
            listen_addrs(&own),
            [ip(10, 1, 2, 30), ip(100, 65, 0, 1)],
            "the messenger may listen on home and tailnet addresses only"
        );
    }

    #[test]
    fn the_tailnet_is_read_from_the_peer_list_only() {
        let a = format!("100.{}.0.7", 64 + 3);
        let outside = format!("100.{}.0.8", 64 - 1);
        let json = format!(
            r#"{{"Self":{{"HostName":"me","TailscaleIPs":["100.{}.0.1"]}},"Peer":{{"k1":{{"HostName":"Tower","TailscaleIPs":["{a}","fd7a::1"]}},"k2":{{"HostName":"odd","TailscaleIPs":["{outside}"]}},"k3":{{"HostName":"pub","TailscaleIPs":["8.8.8.8"]}}}}}}"#,
            64 + 2
        );
        let peers = tailnet_peers(&json);
        assert_eq!(peers, [(a.parse().unwrap(), "tower".to_string())]);
        assert!(tailnet_peers("not json").is_empty());
        assert!(
            !is_tailnet(ip(100, 63, 0, 1))
                && is_tailnet(ip(100, 127, 0, 1))
                && !is_tailnet(ip(100, 128, 0, 1))
        );
    }

    fn serve(body: &'static str) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    #[test]
    fn the_probe_finds_what_answers_and_reads_it() {
        let ollama = serve(
            r#"{"models":[{"name":"qwen3:8b","size":5000000000,"details":{"parameter_size":"8B"}}]}"#,
        );
        let agent = serve(r#"{"guest":true,"version":"2.0.1"}"#);
        let fleet = serve(r#"{"app":"tempo-fleet","name":"tower"}"#);
        // a port with nothing on it
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let ports = Ports {
            ollama,
            agent,
            fleet,
            ssh: 22,
        };
        let t = [ip(127, 0, 0, 1)];
        let open = open_ports(&t, &[ollama, agent, fleet, dead], CONNECT, 64);
        assert_eq!(open.len(), 3, "{open:?}");
        let mut known = Fleet::default();
        known.machines.push(super::super::store::Machine::new(
            "tower",
            "127.0.0.1",
            "both",
        ));
        let found = probe_all(&t, &[], ports, &known);
        assert_eq!(found.len(), 1);
        let f = &found[0];
        assert_eq!(f.models.as_ref().map(Vec::len), Some(1));
        assert_eq!(f.agent.as_deref(), Some("2.0.1"));
        assert_eq!(f.fleet.as_deref(), Some("tower"));
        assert_eq!(f.name, "tower");
        assert!(f.in_fleet);
        assert_eq!(f.role(), "both");
        assert!(f.line().contains("Ollama with 1 model") && f.line().contains("[in the fleet]"));
    }

    #[test]
    fn a_listener_that_is_not_what_it_seems_is_not_taken_for_it() {
        let liar = serve("hello");
        let ports = Ports {
            ollama: liar,
            agent: liar,
            fleet: liar,
            ssh: 0,
        };
        let f = inspect(ip(127, 0, 0, 1), &[liar], ports);
        assert!(f.models.is_none() && f.agent.is_none() && f.fleet.is_none());
    }

    #[test]
    fn ssh_only_is_an_agent_candidate_without_adding_it_to_the_fleet() {
        let f = inspect(
            ip(10, 0, 0, 9),
            &[22],
            Ports {
                ollama: 11434,
                agent: 8765,
                fleet: 8770,
                ssh: 22,
            },
        );
        assert!(f.ssh);
        assert_eq!(f.role(), "agent");
        assert!(f.line().contains("SSH reachable"));
        assert!(!f.in_fleet);
    }

    #[test]
    fn at_most_the_limit_connects_at_once() {
        // many closed ports on this computer, 64 at a time: it ends and finds none
        let t = [ip(127, 0, 0, 1)];
        let ports: Vec<u16> = (1..=200).collect();
        let open = open_ports(&t, &ports, Duration::from_millis(50), AT_ONCE);
        assert!(open.len() < 5);
    }

    #[test]
    fn reverse_answers_are_read() {
        assert_eq!(
            parse_reverse("10.0.0.5  tower.lan tower\n"),
            Some("tower.lan".into())
        );
        assert_eq!(
            parse_reverse("5.0.0.10.in-addr.arpa\tname = nas.home.arpa.\n"),
            Some("nas.home.arpa".into())
        );
    }
}
