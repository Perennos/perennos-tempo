//! Just enough multicast DNS (RFC 6762) to find and be found as `_tempo._tcp` (std only).
//!
//! Looking: a PTR query that asks for a unicast answer (the QU bit), sent from an ordinary port, so
//! it works beside the system's own mDNS service. Being found: `announce` answers queries for the
//! service when port 5353 can be bound; when the system's service holds it, it says so and the
//! address sweep finds the machine anyway.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const TEMPO: &str = "_tempo._tcp.local";
pub const OLLAMA: &str = "_ollama._tcp.local";
const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

fn put_name(out: &mut Vec<u8>, name: &str) {
    for l in name.split('.').filter(|l| !l.is_empty()) {
        out.push(l.len().min(63) as u8);
        out.extend_from_slice(&l.as_bytes()[..l.len().min(63)]);
    }
    out.push(0);
}

pub fn query(service: &str) -> Vec<u8> {
    let mut p = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    put_name(&mut p, service);
    p.extend_from_slice(&[0, 12, 0x80, 1]); // PTR, IN with the unicast-answer bit
    p
}

fn name_at(p: &[u8], mut i: usize) -> Option<(String, usize)> {
    let (mut out, mut end, mut jumps) = (String::new(), None, 0);
    loop {
        let l = *p.get(i)? as usize;
        if l == 0 {
            return Some((out, end.unwrap_or(i + 1)));
        }
        if l & 0xc0 == 0xc0 {
            let to = ((l & 0x3f) << 8) | *p.get(i + 1)? as usize;
            end.get_or_insert(i + 2);
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            i = to;
        } else {
            let label = p.get(i + 1..i + 1 + l)?;
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(&String::from_utf8_lossy(label));
            i += 1 + l;
        }
    }
}

fn u16_at(p: &[u8], i: usize) -> Option<usize> {
    Some((usize::from(*p.get(i)?) << 8) | usize::from(*p.get(i + 1)?))
}

/// The names a packet asks about.
pub fn questions(p: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let (Some(qd), mut i) = (u16_at(p, 4), 12) else {
        return out;
    };
    for _ in 0..qd.min(16) {
        let Some((n, e)) = name_at(p, i) else { break };
        out.push(n.to_ascii_lowercase());
        i = e + 4;
    }
    out
}

/// One service instance an answer described.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    pub instance: String,
    pub host: String,
    pub port: u16,
    pub ip: Option<Ipv4Addr>,
    pub txt: Vec<String>,
}

pub fn parse(p: &[u8], service: &str) -> Vec<Seen> {
    let counts: Vec<usize> = (0..4).filter_map(|k| u16_at(p, 4 + 2 * k)).collect();
    let mut i = 12;
    for _ in 0..counts.first().copied().unwrap_or(0).min(16) {
        match name_at(p, i) {
            Some((_, e)) => i = e + 4,
            None => return vec![],
        }
    }
    let (mut ptr, mut srv, mut a, mut txt) = (vec![], vec![], vec![], vec![]);
    for _ in 0..counts.iter().skip(1).sum::<usize>().min(64) {
        let Some((name, e)) = name_at(p, i) else {
            break;
        };
        let (Some(ty), Some(len)) = (u16_at(p, e), u16_at(p, e + 8)) else {
            break;
        };
        let d = e + 10;
        let name = name.to_ascii_lowercase();
        match ty {
            12 => {
                if let Some((n, _)) = name_at(p, d) {
                    ptr.push((name, n));
                }
            }
            33 => {
                if let (Some(port), Some((t, _))) = (u16_at(p, d + 4), name_at(p, d + 6)) {
                    srv.push((name, t.to_ascii_lowercase(), port as u16));
                }
            }
            1 if len == 4 => {
                if let Some(b) = p.get(d..d + 4) {
                    a.push((name, Ipv4Addr::new(b[0], b[1], b[2], b[3])));
                }
            }
            16 => {
                let (mut j, mut v) = (d, vec![]);
                while j < d + len {
                    let l = *p.get(j).unwrap_or(&0) as usize;
                    if let Some(s) = p.get(j + 1..j + 1 + l) {
                        v.push(String::from_utf8_lossy(s).into_owned());
                    }
                    j += 1 + l;
                }
                txt.push((name, v));
            }
            _ => {}
        }
        i = d + len;
    }
    ptr.into_iter()
        .filter(|(svc, _)| svc == service)
        .filter_map(|(_, inst)| {
            let (_, host, port) = srv
                .iter()
                .find(|(n, ..)| n.eq_ignore_ascii_case(&inst))?
                .clone();
            let ip = a.iter().find(|(n, _)| *n == host).map(|x| x.1);
            let t = txt
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&inst))
                .map(|x| x.1.clone())
                .unwrap_or_default();
            Some(Seen {
                instance: inst,
                host,
                port,
                ip,
                txt: t,
            })
        })
        .collect()
}

/// The answer a machine gives for `_tempo._tcp`: PTR, SRV, TXT and its address.
pub fn response(name: &str, ip: Ipv4Addr, port: u16, txt: &[String]) -> Vec<u8> {
    let inst = format!("{name}.{TEMPO}");
    let host = format!("{name}.local");
    let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 0];
    let rr = |p: &mut Vec<u8>, n: &str, ty: u8, data: Vec<u8>| {
        put_name(p, n);
        p.extend_from_slice(&[0, ty, 0x80, 1, 0, 0, 0, 120]);
        p.extend_from_slice(&(data.len() as u16).to_be_bytes());
        p.extend_from_slice(&data);
    };
    let mut d = vec![];
    put_name(&mut d, &inst);
    rr(&mut p, TEMPO, 12, d);
    let mut d = vec![0, 0, 0, 0, (port >> 8) as u8, port as u8];
    put_name(&mut d, &host);
    rr(&mut p, &inst, 33, d);
    let mut d: Vec<u8> = txt
        .iter()
        .flat_map(|t| std::iter::once(t.len().min(255) as u8).chain(t.bytes().take(255)))
        .collect();
    if d.is_empty() {
        d.push(0);
    }
    rr(&mut p, &inst, 16, d);
    rr(&mut p, &host, 1, ip.octets().to_vec());
    p
}

/// Ask every private interface for `service` and collect what answers within `wait`.
pub fn browse(service: &str, local: &[Ipv4Addr], wait: Duration) -> Vec<Seen> {
    let q = query(service);
    let socks: Vec<UdpSocket> = local
        .iter()
        .filter_map(|ip| UdpSocket::bind((*ip, 0)).ok())
        .inspect(|s| {
            let _ = s.set_multicast_ttl_v4(255);
            let _ = s.set_read_timeout(Some(Duration::from_millis(60)));
            let _ = s.send_to(&q, (GROUP, 5353));
        })
        .collect();
    let mut out: Vec<Seen> = vec![];
    let end = Instant::now() + wait;
    let mut buf = [0u8; 4096];
    while Instant::now() < end {
        for s in &socks {
            if let Ok((n, from)) = s.recv_from(&mut buf) {
                for mut seen in parse(&buf[..n], service) {
                    if seen.ip.is_none() {
                        seen.ip = match from {
                            SocketAddr::V4(a) => Some(*a.ip()),
                            _ => None,
                        };
                    }
                    if !out.iter().any(|o| o.ip == seen.ip && o.port == seen.port) {
                        out.push(seen);
                    }
                }
            }
        }
    }
    out
}

/// Answer queries for `_tempo._tcp` until `stop`. Returns why it could not start, if it could not.
pub fn announce(
    stop: &AtomicBool,
    name: &str,
    port: u16,
    local: &[Ipv4Addr],
    txt: &[String],
) -> Result<(), String> {
    let s = UdpSocket::bind(("0.0.0.0", 5353)).map_err(|e| {
        format!("port 5353 is taken ({e}); this computer is found by the address sweep instead")
    })?;
    for ip in local {
        let _ = s.join_multicast_v4(&GROUP, ip);
    }
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = s.recv_from(&mut buf) else {
            continue;
        };
        if questions(&buf[..n]).iter().any(|q| q == TEMPO) {
            let SocketAddr::V4(f) = from else { continue };
            // the address of the interface that faces the asker: the one on its network
            let mine = local
                .iter()
                .find(|ip| ip.octets()[..3] == f.ip().octets()[..3])
                .or(local.first());
            if let Some(ip) = mine {
                let _ = s.send_to(&response(name, *ip, port, txt), from);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_names_the_service_and_an_answer_reads_back() {
        assert_eq!(questions(&query(TEMPO)), [TEMPO]);
        let ip = Ipv4Addr::new(10, 0, 0, 7);
        let r = response("tower", ip, 8770, &["v=1".to_string()]);
        let seen = parse(&r, TEMPO);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].host, "tower.local");
        assert_eq!((seen[0].port, seen[0].ip), (8770, Some(ip)));
        assert_eq!(seen[0].txt, ["v=1"]);
        assert!(parse(&r, OLLAMA).is_empty());
    }

    #[test]
    fn junk_and_loops_are_refused() {
        assert!(parse(&[0xff; 40], TEMPO).is_empty());
        // a name pointer that points at itself
        let mut p = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        p.extend_from_slice(&[0xc0, 12, 0, 12, 0, 1]);
        assert!(questions(&p).is_empty());
    }
}
