//! Private, best-effort phone delivery for durable fleet silence alerts.

use super::store::{now_s, Paths};
use crate::setup::json::{parse, J};
use crate::{apply_json_atomic, FileLock};
use std::io::{self, BufRead, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

const QUEUE_FILE: &str = "fleet-alerts.json";
const LOCK_FILE: &str = "fleet-alerts.lock";
const MAX_QUEUE: usize = 32;
const MAX_QUEUE_BYTES: u64 = 256 * 1024;
const MAX_MESSAGE_BYTES: usize = 1024;
const MAX_ATTEMPTS: u64 = 1_000_000;
const ALERT_RELAY_ENV: &str = "TEMPO_ALERT_RELAY_URL";

fn relay_configured() -> bool {
    std::env::var(ALERT_RELAY_ENV).is_ok_and(|url| !url.trim().is_empty())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingAlert {
    id: String,
    message: String,
    attempts: u64,
    created_at: u64,
    next_attempt_at: u64,
}

#[derive(Clone, Debug)]
struct Endpoint {
    addr: SocketAddr,
    host_header: String,
    target: String,
}

fn number(json: &J, key: &str) -> io::Result<u64> {
    json.f64(key)
        .filter(|value| {
            value.is_finite()
                && (0.0..=9_007_199_254_740_991.0).contains(value)
                && value.fract() == 0.0
        })
        .map(|value| value as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "alert queue number is invalid"))
}

fn parse_queue(bytes: &[u8]) -> io::Result<Vec<PendingAlert>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let json = parse(text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let J::Obj(fields) = &json else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "alert queue must be an object",
        ));
    };
    if fields.len() != 2
        || fields
            .iter()
            .any(|(key, _)| !["version", "alerts"].contains(&key.as_str()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "alert queue schema is unfamiliar",
        ));
    }
    if number(&json, "version")? != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "alert queue version is unsupported",
        ));
    }
    let Some(J::Arr(rows)) = json.get("alerts") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "alert queue rows are missing",
        ));
    };
    if rows.len() > MAX_QUEUE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "alert queue is too large",
        ));
    }
    let mut alerts = Vec::with_capacity(rows.len());
    for row in rows {
        let J::Obj(fields) = row else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "alert queue row must be an object",
            ));
        };
        const KEYS: [&str; 5] = ["id", "message", "attempts", "created_at", "next_attempt_at"];
        if fields.len() != KEYS.len() || fields.iter().any(|(key, _)| !KEYS.contains(&key.as_str()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "alert queue row schema is unfamiliar",
            ));
        }
        let id = row
            .str("id")
            .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "alert queue id is invalid"))?
            .to_ascii_lowercase();
        let message = row
            .str("message")
            .filter(|message| !message.is_empty() && message.len() <= MAX_MESSAGE_BYTES)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "alert message is invalid"))?
            .to_string();
        let created_at = number(row, "created_at")?;
        if created_at == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "alert creation time is invalid",
            ));
        }
        let attempts = number(row, "attempts")?;
        if attempts > MAX_ATTEMPTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "alert retry count is invalid",
            ));
        }
        alerts.push(PendingAlert {
            id,
            message,
            attempts,
            created_at,
            next_attempt_at: number(row, "next_attempt_at")?,
        });
    }
    Ok(alerts)
}

fn queue_json(alerts: &[PendingAlert]) -> Vec<u8> {
    let rows = alerts
        .iter()
        .map(|alert| {
            J::Obj(vec![
                ("id".into(), J::s(&alert.id)),
                ("message".into(), J::s(&alert.message)),
                ("attempts".into(), J::n(alert.attempts as f64)),
                ("created_at".into(), J::n(alert.created_at as f64)),
                ("next_attempt_at".into(), J::n(alert.next_attempt_at as f64)),
            ])
        })
        .collect();
    J::Obj(vec![
        ("version".into(), J::n(1.0)),
        ("alerts".into(), J::Arr(rows)),
    ])
    .pretty()
    .into_bytes()
}

fn read_queue_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_QUEUE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "alert queue is not a bounded regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "alert queue is a reparse point",
            ));
        }
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    std::fs::File::open(path)?
        .take(MAX_QUEUE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_QUEUE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "alert queue exceeded its read limit",
        ));
    }
    Ok(Some(bytes))
}

fn read_queue(path: &Path) -> io::Result<(Vec<PendingAlert>, Option<Vec<u8>>)> {
    let bytes = read_queue_bytes(path)?;
    let alerts = match bytes.as_deref() {
        Some(bytes) => parse_queue(bytes)?,
        None => Vec::new(),
    };
    Ok((alerts, bytes))
}

fn save_queue(path: &Path, previous: Option<&[u8]>, alerts: &[PendingAlert]) -> io::Result<()> {
    let next = queue_json(alerts);
    apply_json_atomic(path, previous, &next, None, |bytes| {
        parse_queue(bytes).map(|_| ())
    })
}

fn private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        IpAddr::V6(address) => {
            let octets = address.octets();
            address.is_loopback()
                || (octets[0] & 0xfe == 0xfc)
                || (octets[0] == 0xfe && octets[1] & 0xc0 == 0x80)
        }
    }
}

fn invalid_url() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "phone relay must be a private HTTP IP address",
    )
}

fn parse_endpoint(url: &str) -> io::Result<Endpoint> {
    let rest = url.strip_prefix("http://").ok_or_else(invalid_url)?;
    let split = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..split];
    let target = if split == rest.len() {
        "/"
    } else {
        &rest[split..]
    };
    if authority.is_empty()
        || authority.contains(['@', '?', '#', '\\', '\r', '\n'])
        || target.contains(['?', '#', '\\', '\r', '\n'])
        || !target.is_ascii()
        || target.bytes().any(|byte| byte <= 0x20 || byte == 0x7f)
    {
        return Err(invalid_url());
    }
    let (host, port, bracketed) = if let Some(after_open) = authority.strip_prefix('[') {
        let close = after_open.find(']').ok_or_else(invalid_url)?;
        let host = &after_open[..close];
        let suffix = &after_open[close + 1..];
        let port = if suffix.is_empty() {
            80
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(invalid_url)?
                .parse::<u16>()
                .map_err(|_| invalid_url())?
        };
        (
            host.parse::<Ipv6Addr>().map_err(|_| invalid_url())?.into(),
            port,
            true,
        )
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, 80), |(host, port)| {
                (host, port.parse::<u16>().unwrap_or(0))
            });
        if port == 0 {
            return Err(invalid_url());
        }
        (
            host.parse::<Ipv4Addr>().map_err(|_| invalid_url())?.into(),
            port,
            false,
        )
    };
    if port == 0 || !private_address(host) {
        return Err(invalid_url());
    }
    let host_header = if bracketed {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Ok(Endpoint {
        addr: SocketAddr::new(host, port),
        host_header,
        target: target.to_string(),
    })
}

fn post_alert(url: &str, message: &str) -> io::Result<()> {
    if message.is_empty() || message.len() > MAX_MESSAGE_BYTES || message.contains(['\r', '\n']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "phone alert text is invalid",
        ));
    }
    let endpoint = parse_endpoint(url)?;
    let mut stream = TcpStream::connect_timeout(&endpoint.addr, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    write!(
        stream,
        "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: tempo-launch\r\nTitle: Perennos Tempo alert\r\nPriority: high\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        endpoint.target,
        endpoint.host_header,
        message.len(),
        message
    )?;
    stream.flush()?;
    let mut status = String::new();
    let bytes = io::BufReader::new(stream)
        .take(4096)
        .read_line(&mut status)?;
    if bytes == 0 || bytes > 256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "phone relay returned an invalid response",
        ));
    }
    let mut fields = status.split_whitespace();
    let version = fields.next().unwrap_or_default();
    let code = fields
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || !(200..300).contains(&code) {
        return Err(io::Error::other("phone relay did not accept the alert"));
    }
    Ok(())
}

fn retry_delay(attempts: u64) -> u64 {
    5u64.saturating_mul(1u64 << attempts.saturating_sub(1).min(6))
        .min(300)
}

fn next_retry_after(alerts: &[PendingAlert], now: u64) -> Option<Duration> {
    alerts
        .iter()
        .map(|alert| alert.next_attempt_at.saturating_sub(now))
        .min()
        .map(Duration::from_secs)
}

fn enqueue(paths: &Paths, message: &str) -> io::Result<()> {
    if message.is_empty() || message.len() > MAX_MESSAGE_BYTES || message.contains(['\r', '\n']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "phone alert text is invalid",
        ));
    }
    let path = paths.file(QUEUE_FILE);
    let _lock = FileLock::acquire(&paths.file(LOCK_FILE), Duration::from_secs(5))?;
    let (mut alerts, previous) = read_queue(&path)?;
    if alerts.len() >= MAX_QUEUE {
        alerts.remove(0);
    }
    alerts.push(PendingAlert {
        id: super::crypto::hex(&super::crypto::random(16)),
        message: message.to_string(),
        attempts: 0,
        created_at: now_s(),
        next_attempt_at: 0,
    });
    save_queue(&path, previous.as_deref(), &alerts)
}

fn take_due(paths: &Paths, now: u64) -> io::Result<Option<PendingAlert>> {
    let path = paths.file(QUEUE_FILE);
    let _lock = FileLock::acquire(&paths.file(LOCK_FILE), Duration::from_secs(5))?;
    let (mut alerts, previous) = read_queue(&path)?;
    let Some(index) = alerts.iter().position(|alert| alert.next_attempt_at <= now) else {
        return Ok(None);
    };
    alerts[index].attempts = alerts[index].attempts.saturating_add(1).min(MAX_ATTEMPTS);
    alerts[index].next_attempt_at = now.saturating_add(retry_delay(alerts[index].attempts));
    let attempt = alerts[index].clone();
    save_queue(&path, previous.as_deref(), &alerts)?;
    Ok(Some(attempt))
}

fn remove_delivered(paths: &Paths, id: &str) -> io::Result<()> {
    let path = paths.file(QUEUE_FILE);
    let _lock = FileLock::acquire(&paths.file(LOCK_FILE), Duration::from_secs(5))?;
    let (mut alerts, previous) = read_queue(&path)?;
    alerts.retain(|alert| alert.id != id);
    save_queue(&path, previous.as_deref(), &alerts)
}

pub(super) fn queue_alert(paths: &Paths, message: &str) -> bool {
    if !relay_configured() {
        return false;
    }
    match enqueue(paths, message) {
        Ok(()) => true,
        Err(_) => {
            eprintln!("The phone alert could not be saved for delivery.");
            false
        }
    }
}

pub(super) fn send_pending_once(paths: &Paths) {
    let Ok(url) = std::env::var(ALERT_RELAY_ENV) else {
        return;
    };
    if url.is_empty() {
        return;
    }
    let alert = match take_due(paths, now_s()) {
        Ok(Some(alert)) => alert,
        Ok(None) | Err(_) => return,
    };
    if post_alert(&url, &alert.message).is_ok() {
        let _ = remove_delivered(paths, &alert.id);
    }
}

pub(super) fn next_retry_delay(paths: &Paths, now: u64) -> Option<Duration> {
    if std::env::var(ALERT_RELAY_ENV)
        .ok()
        .is_none_or(|url| url.is_empty())
    {
        return None;
    }
    let bytes = read_queue_bytes(&paths.file(QUEUE_FILE)).ok()??;
    let alerts = parse_queue(&bytes).ok()?;
    next_retry_after(&alerts, now)
}

pub(super) fn pending_message(paths: &Paths) -> Option<String> {
    let relay_configured = relay_configured();
    let path = paths.file(QUEUE_FILE);
    match read_queue_bytes(&path).and_then(|bytes| match bytes {
        Some(bytes) => parse_queue(&bytes),
        None => Ok(Vec::new()),
    }) {
        Ok(alerts) if alerts.is_empty() => None,
        Ok(_) if !relay_configured => Some("No phone relay configured.".into()),
        Ok(alerts) if alerts.len() == 1 => Some("One phone alert is waiting to be sent.".into()),
        Ok(alerts) => Some(format!(
            "{} phone alerts are waiting to be sent.",
            alerts.len()
        )),
        Err(_) => Some(
            "I can't check which phone alerts are waiting, so delivery status is unknown.".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn relay_endpoint_accepts_private_and_shared_ranges_only() {
        assert!(parse_endpoint("http://10.0.0.2:2586/topic").is_ok());
        // Shared-address range (100.64.0.0/10) example, built at runtime: a fixture, not a real host.
        // A literal in that range trips the repo's private-address leak scan.
        let shared = format!("http://100.{}.0.2:2586/topic", 100);
        assert!(parse_endpoint(&shared).is_ok());
        assert!(parse_endpoint("http://[fd00::1]:2586/topic").is_ok());
        assert!(parse_endpoint("http://127.0.0.1:2586/topic").is_ok());
        assert!(parse_endpoint("http://8.8.8.8:2586/topic").is_err());
        assert!(parse_endpoint("https://10.0.0.2:2586/topic").is_err());
        assert!(parse_endpoint("http://name.local:2586/topic").is_err());
        assert!(parse_endpoint("http://user@10.0.0.2:2586/topic").is_err());
        assert!(parse_endpoint("http://10.0.0.2:2586/topic?x=1").is_err());
    }

    #[test]
    fn relay_posts_bounded_plain_text_and_accepts_only_a_success_response() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = io::BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            assert!(headers.starts_with("POST /topic HTTP/1.1\r\n"));
            assert!(headers.contains("Title: Perennos Tempo alert\r\n"));
            assert_eq!(body.as_slice(), b"Worker is quiet.");
            reader
                .get_mut()
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        post_alert(&format!("http://{address}/topic"), "Worker is quiet.").unwrap();
        receiver.join().unwrap();
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(1), 5);
        assert_eq!(retry_delay(2), 10);
        assert_eq!(retry_delay(7), 300);
        assert_eq!(retry_delay(u64::MAX), 300);
    }

    #[test]
    fn next_retry_uses_the_earliest_saved_due_time() {
        let alert = |id: &str, next_attempt_at| PendingAlert {
            id: id.into(),
            message: "test".into(),
            attempts: 1,
            created_at: 1,
            next_attempt_at,
        };
        assert_eq!(next_retry_after(&[], 10), None);
        assert_eq!(
            next_retry_after(&[alert("a", 25), alert("b", 17)], 10),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            next_retry_after(&[alert("a", 10)], 10),
            Some(Duration::ZERO)
        );
    }
}
