//! Ask Tempo from the vault. The app's Tempo panel talks to the Perennos Tempo agent through this
//! server, on this computer only: exactly five calls (health, new, send, events, confirm) are
//! passed to the agent's loopback address, server to server. The page never talks to the agent
//! itself (the agent refuses other pages), and a vault reached from another computer or through
//! a proxy cannot reach the agent at all (see `server::ask_proxy`).
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const DEFAULT_AGENT: &str = "http://127.0.0.1:8765";

/// An agent answer is never read past this.
const MAX_REPLY: u64 = 8 << 20;

/// The agent's address (`TEMPO_AGENT_URL`, else 127.0.0.1:8765), as the Host header and a socket
/// address. Only this computer is accepted.
pub fn agent_addr(url: Option<&str>) -> Result<(String, SocketAddr), String> {
    let url = url
        .map(str::to_string)
        .or_else(|| std::env::var("TEMPO_AGENT_URL").ok())
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT.into());
    let rest = url
        .trim()
        .trim_end_matches('/')
        .strip_prefix("http://")
        .ok_or("the agent address must be http:// on this computer")?;
    if rest.contains(['/', '@', '?', '#']) || rest.is_empty() {
        return Err("the agent address is a host and a port only".into());
    }
    let hostport = if rest
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && !rest.ends_with(']')
    {
        rest.to_string()
    } else {
        format!("{rest}:80")
    };
    let addr = hostport
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {hostport}: {e}"))?
        .find(|a| a.ip().is_loopback())
        .ok_or("the agent must be on this computer (127.0.0.1)")?;
    Ok((hostport, addr))
}

/// One call to the agent: its status and JSON answer.
pub fn call(
    url: Option<&str>,
    method: &str,
    path: &str,
    body: Option<&Value>,
    key: Option<&str>,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    let (host, addr) = agent_addr(url)?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| format!("Tempo is not running at {host}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let payload = body.map(Value::to_string).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n");
    if let Some(k) = key {
        req.push_str(&format!("X-Tempo-Key: {k}\r\n"));
    }
    if method == "POST" {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    req.push_str("\r\n");
    req.push_str(&payload);
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = vec![];
    (&mut s)
        .take(MAX_REPLY)
        .read_to_end(&mut raw)
        .map_err(|e| format!("Tempo did not answer in time: {e}"))?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("Tempo sent an answer that is not HTTP")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut b = raw[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        b = crate::learn::decode_chunked(&b).ok_or("Tempo sent a broken answer")?;
    }
    let v = serde_json::from_slice(&b).unwrap_or(Value::Null);
    Ok((code, v))
}

/// A conversation id as the agent makes them (letters, digits, `-`, `_`).
pub fn conv_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::TcpListener;

    #[test]
    fn only_this_computer() {
        assert!(agent_addr(Some("http://127.0.0.1:8765")).is_ok());
        assert!(agent_addr(Some("http://localhost:9")).is_ok());
        for bad in [
            "https://127.0.0.1:8765",
            "http://user@127.0.0.1:8765",
            "http://127.0.0.1:8765/agent",
            "http://192.0.2.10:8765",
            "ftp://127.0.0.1",
        ] {
            assert!(agent_addr(Some(bad)).is_err(), "{bad}");
        }
        assert!(conv_ok("2026-11-06-0102-ab12") && !conv_ok("../x") && !conv_ok(""));
    }

    #[test]
    fn talks_to_an_agent() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = "{\"conv\":\"c1\"}";
            write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            req
        });
        let (code, v) = call(
            Some(&format!("http://127.0.0.1:{port}")),
            "POST",
            "/agent/new",
            Some(&json!({})),
            Some("abc123"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!((code, v["conv"].as_str()), (200, Some("c1")));
        let req = t.join().unwrap();
        assert!(
            req.starts_with("POST /agent/new HTTP/1.1\r\nHost: 127.0.0.1:"),
            "{req}"
        );
        assert!(
            !req.to_lowercase().contains("origin:"),
            "server to server: no Origin"
        );
        assert!(
            req.contains("\r\nX-Tempo-Key: abc123\r\n"),
            "the page's key goes along: {req}"
        );
        let e = call(
            Some("http://127.0.0.1:1"),
            "GET",
            "/agent/health",
            None,
            None,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(e.contains("not running"), "{e}");
    }
}
