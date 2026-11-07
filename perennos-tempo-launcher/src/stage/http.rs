//! A small HTTP client for ComfyUI (std only). ComfyUI has no password, so Stage only talks to
//! the one on this computer, at 127.0.0.1, and never follows a redirect. Another computer's
//! ComfyUI is never reached from here: a run for it goes through the Fleet messenger.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// The biggest answer taken in memory: a long clip at a good bitrate fits.
pub const MAX_BODY: usize = 768 << 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Http {
    pub host: String,
    pub port: u16,
}

impl Http {
    /// `http://host[:port]` on this computer (127.0.0.1, ::1 or localhost).
    pub fn parse(base: &str) -> Result<Http, String> {
        let (https, host, port) = crate::parse_remote_url(base)?;
        if https {
            return Err(
                "ComfyUI is reached over http on this computer; an https address is not used here"
                    .into(),
            );
        }
        let h = Http { host, port };
        h.addr()?;
        Ok(h)
    }

    fn addr(&self) -> Result<SocketAddr, String> {
        let mut all = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|e| format!("could not find {} ({e})", self.host))?;
        all.find(|a| a.ip().is_loopback()).ok_or_else(|| {
            format!(
                "{} is not this computer. Stage only talks to the ComfyUI on 127.0.0.1; a run for another computer goes through the Fleet (`tempo-launch fleet allow stage` there), and nothing needs ComfyUI to listen on the network.",
                self.host
            )
        })
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>), String> {
        let addr = self.addr()?;
        let mut s = TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(10)))
            .map_err(|e| format!("could not reach {}:{} ({e})", self.host, self.port))?;
        s.set_read_timeout(Some(timeout)).ok();
        s.set_write_timeout(Some(timeout)).ok();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: tempo-launch/{}\r\nAccept: */*\r\nConnection: close\r\n",
            self.host,
            self.port,
            crate::VERSION
        );
        if let Some(ct) = content_type {
            head.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        if method != "GET" {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes())
            .and_then(|()| s.write_all(body))
            .map_err(|e| format!("could not send to {}:{} ({e})", self.host, self.port))?;
        let mut buf = Vec::new();
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    if buf.is_empty() {
                        return Err(format!("no answer from {}:{} ({e})", self.host, self.port));
                    }
                    break;
                }
            }
            if buf.len() > MAX_BODY {
                return Err("the answer was bigger than Stage will hold in memory".into());
            }
        }
        split(&buf).ok_or_else(|| format!("{}:{} did not answer with HTTP", self.host, self.port))
    }

    pub fn get(&self, path: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String> {
        self.request("GET", path, None, &[], timeout)
    }

    pub fn post_json(
        &self,
        path: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>), String> {
        self.request(
            "POST",
            path,
            Some("application/json"),
            body.as_bytes(),
            timeout,
        )
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Status and body of a whole HTTP/1.1 answer (chunked or not).
fn split(buf: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = find(buf, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let code: u16 = head.lines().next()?.split(' ').nth(1)?.parse().ok()?;
    let body = &buf[end + 4..];
    let chunked = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked")
    });
    if !chunked {
        return Some((code, body.to_vec()));
    }
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let eol = find(rest, b"\r\n")?;
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&rest[..eol])
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        rest = &rest[eol + 2..];
        if size == 0 {
            break;
        }
        if rest.len() < size + 2 {
            // cut short: keep what came
            out.extend_from_slice(rest);
            break;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    Some((code, out))
}

/// The text of an answer, for JSON.
pub fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A multipart/form-data body with one file and some plain fields.
pub fn multipart(
    boundary: &str,
    file_field: &str,
    file_name: &str,
    bytes: &[u8],
    fields: &[(&str, &str)],
) -> Vec<u8> {
    let mut b = Vec::new();
    for (k, v) in fields {
        b.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    b.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(bytes);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

/// Percent-encode a value for a query string.
pub fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_split_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        assert_eq!(split(plain), Some((200, b"hi".to_vec())));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(split(chunked), Some((200, b"abcde".to_vec())));
        assert_eq!(split(b"nonsense"), None);
    }

    #[test]
    fn only_this_computer_is_reached() {
        assert!(Http::parse("http://127.0.0.1:8188").is_ok());
        assert!(Http::parse("http://localhost:8188").is_ok());
        // not another computer on the network, not even a private one
        for other in ["http://10.0.0.5:8188", "http://192.168.0.7:8188"] {
            let e = Http::parse(other).unwrap_err();
            assert!(e.contains("Fleet") && e.contains("127.0.0.1"), "{e}");
        }
        assert!(Http::parse("https://127.0.0.1:8188").is_err());
        assert!(Http::parse("http://8.8.8.8:8188").is_err());
    }

    #[test]
    fn a_multipart_body_has_its_fields_and_its_file() {
        let b = text(&multipart(
            "XX",
            "image",
            "a.png",
            b"DATA",
            &[("type", "input")],
        ));
        assert!(
            b.contains("name=\"type\"\r\n\r\ninput")
                && b.contains("filename=\"a.png\"")
                && b.contains("DATA")
        );
        assert!(b.ends_with("--XX--\r\n"));
        assert_eq!(enc("a b/c.mp4"), "a%20b%2Fc.mp4");
    }
}
