//! A small HTTP/1.1 client on `std::net`, enough for Ollama: plain `http://` on loopback
//! or the LAN, JSON bodies, and streamed NDJSON replies (chunked, Content-Length or
//! read-to-close). No TLS and no proxies, on purpose: engines are local.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// `host:port` as written, used for the Host header.
    pub authority: String,
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

pub fn parse_url(url: &str) -> io::Result<Url> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| invalid("only http:// engine URLs are supported (Ollama serves plain HTTP)"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(invalid("missing or unsupported host in URL"));
    }
    let bad_port = || invalid("bad port in URL");
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let end = v6.find(']').ok_or_else(|| invalid("bad IPv6 host in URL"))?;
        let port = match &v6[end + 1..] {
            "" => 80,
            p => p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or_else(bad_port)?,
        };
        (v6[..end].to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| bad_port())?),
            None => (authority.to_string(), 80),
        }
    };
    Ok(Url { authority: authority.to_string(), host, port, path: path.to_string() })
}

/// Status plus a body reader already positioned after the headers and de-chunked.
pub struct Response {
    pub status: u16,
    pub body: Box<dyn BufRead + Send>,
}

impl Response {
    pub fn text(mut self) -> io::Result<String> {
        let mut raw = Vec::new();
        self.body.read_to_end(&mut raw)?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }
}

/// Send one request with `Connection: close`. `timeout` bounds the connect and every
/// read or write, so a long generation is fine as long as tokens keep arriving.
pub fn request(method: &str, url: &str, json_body: Option<&[u8]>, timeout: Duration) -> io::Result<Response> {
    let u = parse_url(url)?;
    let mut last_err = None;
    let mut stream = None;
    for addr in (u.host.as_str(), u.port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT.min(timeout)) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let stream = match stream {
        Some(s) => s,
        None => return Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "host did not resolve"))),
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let _ = stream.set_nodelay(true);

    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: perennos-tempo-agent\r\nAccept: */*\r\nConnection: close\r\n",
        u.path, u.authority
    );
    if let Some(b) = json_body {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    let mut w = &stream;
    w.write_all(head.as_bytes())?;
    if let Some(b) = json_body {
        w.write_all(b)?;
    }
    w.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|_| line.starts_with("HTTP/"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an HTTP response"))?;
    let (mut chunked, mut length) = (false, None);
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed inside the headers"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if k.eq_ignore_ascii_case("content-length") {
                length = v.parse::<u64>().ok();
            }
        }
    }
    let body: Box<dyn BufRead + Send> = if chunked {
        Box::new(BufReader::new(Chunked { inner: reader, remaining: 0, done: false }))
    } else if let Some(n) = length {
        Box::new(reader.take(n))
    } else {
        Box::new(reader)
    };
    Ok(Response { status, body })
}

/// `Transfer-Encoding: chunked` decoder.
struct Chunked<R> {
    inner: R,
    remaining: u64,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut line = String::new();
            if self.inner.read_line(&mut line)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunked body ended early"));
            }
            let size = line.trim().split(';').next().unwrap_or("").trim();
            self.remaining = u64::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if self.remaining == 0 {
                loop {
                    line.clear();
                    if self.inner.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
        }
        let want = self.remaining.min(buf.len() as u64) as usize;
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunked body ended early"));
        }
        self.remaining -= n as u64;
        if self.remaining == 0 {
            let mut crlf = String::new();
            self.inner.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Framing, MockOllama, Reply};
    use serde_json::json;

    #[test]
    fn parses_urls() {
        let u = parse_url("http://127.0.0.1:11434").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("127.0.0.1", 11434, "/"));
        let u = parse_url("http://gpu-box.local/api/tags?x=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("gpu-box.local", 80, "/api/tags?x=1"));
        let u = parse_url("http://[::1]:8080/a").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.authority.as_str()), ("::1", 8080, "[::1]:8080"));
        assert!(parse_url("https://example.com").is_err());
        assert!(parse_url("http://user@host").is_err());
        assert!(parse_url("http://host:notaport").is_err());
    }

    #[test]
    fn reads_every_body_framing() {
        for framing in [Framing::Chunked, Framing::Length, Framing::Close] {
            let lines = vec![json!({"n": 1}), json!({"n": 2, "text": "x".repeat(20_000)})];
            let mock = MockOllama::start(vec![Reply::Chat { lines, framing }]);
            let r = request("POST", &format!("{}/api/chat", mock.url), Some(b"{\"model\":\"m\"}"), Duration::from_secs(5))
                .unwrap();
            assert_eq!(r.status, 200);
            let text = r.text().unwrap();
            let got: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
            assert_eq!(got.len(), 2, "{framing:?}");
            assert_eq!(got[1]["text"].as_str().unwrap().len(), 20_000);
            assert_eq!(mock.chat_requests(), vec![json!({"model": "m"})]);
        }
    }

    #[test]
    fn reports_status_and_connect_errors() {
        let mock = MockOllama::start(vec![Reply::Status(503, r#"{"error":"loading"}"#.into())]);
        let r = request("POST", &format!("{}/api/chat", mock.url), Some(b"{}"), Duration::from_secs(5)).unwrap();
        assert_eq!(r.status, 503);
        assert!(r.text().unwrap().contains("loading"));
        // A port nothing listens on: the error comes back instead of a hang.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        assert!(request("GET", &format!("http://127.0.0.1:{port}/"), None, Duration::from_secs(2)).is_err());
    }
}
