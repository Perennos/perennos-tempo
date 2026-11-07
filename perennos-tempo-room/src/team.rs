//! Teammates: named AI members the owner sets up and talks to like colleagues.
//!
//! A teammate has a handle (`@sage`), a display name, a role (the owner's instruction), a kind and
//! an optional model. Two kinds, both without any tool:
//! - `agent`: answers through the local Perennos Tempo agent in guest mode, like `@tempo`, in its
//!   own agent conversation per (room, teammate);
//! - `engine`: a direct, stateless chat with one Ollama model (`POST <ollama>/api/chat`, stream
//!   false, no `tools` field at all).
//!
//! This file holds the rules (handles, roles, models), the records, and the little Ollama client.
//! The Ollama address is `ollama_url` in `config.json`, else `OLLAMA_URL` or `TEMPO_OLLAMA`, else
//! `http://127.0.0.1:11434`; only this machine, a private or link-local network or a tailnet may
//! be called, redirects are never followed and no proxy is used.
use crate::store::Config;
use crate::util::{clean_text, scrub};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

/// The longest role, in characters (the agent takes the same).
pub const MAX_ROLE: usize = 2000;
/// The longest model tag.
pub const MAX_MODEL: usize = 100;
pub const DEFAULT_OLLAMA: &str = "http://127.0.0.1:11434";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TeamKind {
    /// Through the local Perennos Tempo agent, in guest mode.
    Agent,
    /// A direct chat with one Ollama model.
    Engine,
}

impl TeamKind {
    pub fn parse(s: &str) -> Option<TeamKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "agent" => Some(TeamKind::Agent),
            "engine" => Some(TeamKind::Engine),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            TeamKind::Agent => "agent",
            TeamKind::Engine => "engine",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Teammate {
    pub handle: String,
    pub name: String,
    /// The owner's instruction for this teammate (redacted before it is stored).
    pub role: String,
    pub kind: TeamKind,
    pub model: Option<String>,
    pub created: f64,
}

impl Teammate {
    /// The whole record (owner only; the role is the owner's instruction).
    pub fn to_json(&self) -> Value {
        json!({"handle": self.handle, "name": self.name, "role": self.role, "kind": self.kind.as_str(), "model": self.model, "created": self.created})
    }

    /// What everyone in a room may see: no role, no model.
    pub fn public_json(&self) -> Value {
        json!({"handle": self.handle, "name": self.name, "kind": self.kind.as_str()})
    }

    pub fn from_json(v: &Value) -> Option<Teammate> {
        let handle = v["handle"].as_str().filter(|h| valid_handle(h).is_ok())?;
        Some(Teammate {
            handle: handle.to_string(),
            name: v["name"].as_str().unwrap_or(handle).to_string(),
            role: v["role"].as_str().unwrap_or("").to_string(),
            kind: TeamKind::parse(v["kind"].as_str().unwrap_or("")).unwrap_or(TeamKind::Agent),
            model: v["model"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map(str::to_string),
            created: v["created"].as_f64().unwrap_or(0.0),
        })
    }

    /// `You are <name>. <role>`: the owner's instruction for this teammate.
    pub fn instruction(&self) -> String {
        if self.role.trim().is_empty() {
            format!("You are {}.", self.name)
        } else {
            format!("You are {}. {}", self.name, self.role.trim())
        }
    }
}

/// A handle: lowercase letters, digits and hyphens, 2 to 24 characters, starting and ending with
/// a letter or digit. (Reserved names and look-alikes are the store's business.)
pub fn valid_handle(h: &str) -> Result<String, String> {
    let n = h.chars().count();
    let ok_chars = h
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !(2..=24).contains(&n)
        || !ok_chars
        || h.starts_with('-')
        || h.ends_with('-')
        || h.contains("--")
    {
        return Err("A handle is 2 to 24 lowercase letters, digits or hyphens (like sage or code-helper), starting and ending with a letter or digit.".into());
    }
    Ok(h.to_string())
}

/// A model tag, by the agent's rules: letters, digits and `: . - _ /`, at most 100 characters.
pub fn valid_model(m: &str) -> Result<String, String> {
    let m = m.trim();
    if m.is_empty()
        || m.chars().count() > MAX_MODEL
        || !m
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_' | '/'))
    {
        return Err("A model is letters, digits and : . - _ / only, at most 100 characters (like qwen3:4b).".into());
    }
    Ok(m.to_string())
}

/// A role: the owner's words, at most 2,000 characters. Control characters are dropped and
/// secrets are redacted before it is stored.
pub fn valid_role(r: &str) -> Result<String, String> {
    let r = scrub(clean_text(r).trim());
    if r.chars().count() > MAX_ROLE {
        return Err(format!(
            "A role is 2,000 characters at most (this one is {}).",
            r.chars().count()
        ));
    }
    Ok(r)
}

// ---- the Ollama address

/// May a request go to this address? This machine, a private or link-local network, or a tailnet.
pub fn ip_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => v4_allowed(a),
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return v4_allowed(v4);
            }
            let seg = a.segments();
            a.is_loopback()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local (a tailnet's fd7a:... is in here)
                || (seg[0] & 0xffc0) == 0xfe80 // link-local
        }
    }
}

fn v4_allowed(a: Ipv4Addr) -> bool {
    let o = a.octets();
    a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || (o[0] == 100 && (64..=127).contains(&o[1])) // tailnet (carrier-grade NAT range)
}

/// A host name (or address) is allowed when every address it resolves to is.
fn host_allowed(host: &str, port: u16) -> Result<(), String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return if ip_allowed(ip) {
            Ok(())
        } else {
            Err(not_local(host))
        };
    }
    let addrs: Vec<_> = (bare, port)
        .to_socket_addrs()
        .map_err(|_| format!("Cannot find the Ollama host {host}."))?
        .collect();
    if addrs.is_empty() || !addrs.iter().all(|a| ip_allowed(a.ip())) {
        return Err(not_local(host));
    }
    Ok(())
}

fn not_local(host: &str) -> String {
    format!("The Ollama address {host} is not allowed: only this machine, a home or office network, or your tailnet.")
}

/// `http://host[:port]` from what the owner wrote (a bare `host:port` gets `http://`). No user
/// name, no path.
pub fn ollama_base(raw: &str) -> Result<String, String> {
    let raw = raw.trim().trim_end_matches('/');
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let (scheme, rest) = with_scheme
        .split_once("://")
        .ok_or("The Ollama address is a URL like http://127.0.0.1:11434.")?;
    if !matches!(scheme, "http" | "https")
        || rest.is_empty()
        || rest.contains(['@', '/', '?', '#', ' '])
    {
        return Err("The Ollama address is just http://host:port (no user name, no path).".into());
    }
    let (host, port) = if let Some(r) = rest.strip_prefix('[') {
        let (h, after) = r
            .split_once(']')
            .ok_or("The Ollama address is not valid.")?;
        let p = after.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
        (format!("[{h}]"), p)
    } else if let Some((h, p)) = rest.rsplit_once(':') {
        (
            h.to_string(),
            Some(
                p.parse::<u16>()
                    .map_err(|_| "The Ollama port is a number.")?,
            ),
        )
    } else {
        (rest.to_string(), None)
    };
    if host.is_empty() {
        return Err("The Ollama address is not valid.".into());
    }
    host_allowed(&host, port.unwrap_or(11434))?;
    Ok(with_scheme)
}

/// The configured Ollama address, checked: `ollama_url`, else `OLLAMA_URL`, else `TEMPO_OLLAMA`,
/// else the default.
pub fn ollama_url(cfg: &Config) -> Result<String, String> {
    let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let raw = cfg
        .ollama_url
        .clone()
        .or_else(|| from_env("OLLAMA_URL"))
        .or_else(|| from_env("TEMPO_OLLAMA"))
        .unwrap_or_else(|| DEFAULT_OLLAMA.to_string());
    ollama_base(&raw)
}

// ---- the Ollama client

#[derive(Debug, PartialEq)]
pub enum EngineError {
    /// Nothing answered.
    Down(String),
    Timeout,
    Error(String),
    Empty,
}

fn client(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

/// The body of the one chat call an engine teammate makes. There is never a `tools` field.
pub fn chat_body(model: &str, system: &str, user: &str) -> Value {
    json!({
        "model": model,
        "stream": false,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    })
}

/// One answer from `model`: `POST <base>/api/chat`, stream false, no tools.
pub fn chat(
    base: &str,
    model: &str,
    system: &str,
    user: &str,
    timeout: Duration,
) -> Result<String, EngineError> {
    let body = chat_body(model, system, user);
    let r = client(timeout)
        .post(&format!("{base}/api/chat"))
        .header("Content-Type", "application/json")
        .send(body.to_string());
    let mut r = match r {
        Ok(r) => r,
        Err(ureq::Error::Timeout(_)) => return Err(EngineError::Timeout),
        Err(e) => return Err(EngineError::Down(scrub(&e.to_string()))),
    };
    let code = r.status().as_u16();
    let text = r
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| match e {
            ureq::Error::Timeout(_) => EngineError::Timeout,
            e => EngineError::Down(scrub(&e.to_string())),
        })?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if code != 200 {
        let why = v["error"].as_str().unwrap_or("");
        return Err(EngineError::Error(scrub(&format!(
            "HTTP {code} {}",
            crate::util::head_chars(why, 160)
        ))));
    }
    match v["message"]["content"].as_str() {
        Some(c) if !c.trim().is_empty() => Ok(c.to_string()),
        _ => Err(EngineError::Empty),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub size: Option<u64>,
}

/// The installed models: `GET <base>/api/tags`. `Err` means Ollama did not answer properly.
pub fn tags(base: &str, timeout: Duration) -> Result<Vec<Model>, String> {
    let mut r = client(timeout)
        .get(&format!("{base}/api/tags"))
        .call()
        .map_err(|e| scrub(&e.to_string()))?;
    if r.status().as_u16() != 200 {
        return Err(format!("HTTP {}", r.status().as_u16()));
    }
    let text = r
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| scrub(&e.to_string()))?;
    let v: Value = serde_json::from_str(&text).map_err(|_| "not JSON".to_string())?;
    let list = v["models"].as_array().ok_or("no model list")?;
    Ok(list
        .iter()
        .filter_map(|m| {
            let name = m["name"].as_str().or_else(|| m["model"].as_str())?;
            Some(Model {
                name: name.to_string(),
                size: m["size"].as_u64(),
            })
        })
        .collect())
}

fn has_model(list: &[Model], model: &str) -> bool {
    list.iter().any(|m| {
        m.name == model
            || m.name == format!("{model}:latest")
            || format!("{}:latest", m.name) == model
    })
}

/// An engine teammate's model against what Ollama has. `Ok(None)`: it is installed.
/// `Ok(Some(note))`: Ollama is not answering, so the teammate is kept with a note.
/// `Err`: Ollama answered and does not have the model (or its address is not allowed).
pub fn verify_model(cfg: &Config, model: &str) -> Result<Option<String>, String> {
    let base = ollama_url(cfg)?;
    match tags(&base, Duration::from_secs(5)) {
        Err(_) => Ok(Some(format!(
            "Ollama is not answering right now, so I could not check that {model} is installed. The teammate is kept; it will answer once Ollama is back and has that model."
        ))),
        Ok(list) if has_model(&list, model) => Ok(None),
        Ok(list) => {
            let names: Vec<&str> = list.iter().take(12).map(|m| m.name.as_str()).collect();
            Err(format!(
                "Ollama does not have a model called {model}. Installed: {}.",
                if names.is_empty() {
                    "none yet".to_string()
                } else {
                    names.join(", ")
                }
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_follow_the_rules() {
        for ok in ["sage", "ab", "code-helper", "r2d2", "a1", &"x".repeat(24)] {
            assert!(valid_handle(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a",
            "Sage",
            "sa ge",
            "sa_ge",
            "-sage",
            "sage-",
            "sa--ge",
            "sagé",
            "@sage",
            &"x".repeat(25),
        ] {
            assert!(valid_handle(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn models_and_roles_follow_the_agents_rules() {
        for ok in ["qwen3:4b", "mock:4b", "library/llama3.2:3b-q4_K_M", "a"] {
            assert!(valid_model(ok).is_ok(), "{ok}");
        }
        for bad in ["", "two words", "semi;colon", "q\"x", &"m".repeat(101)] {
            assert!(valid_model(bad).is_err(), "{bad}");
        }
        assert!(valid_model(&"m".repeat(100)).is_ok());
        assert_eq!(valid_role("  Be brief.\u{0}  ").unwrap(), "Be brief.");
        assert!(valid_role(&"r".repeat(2000)).is_ok());
        assert!(valid_role(&"r".repeat(2001)).is_err());
        let key = format!("sk-{}", "abcdefghijKLMNOPQRSTuvwxyz0123456789");
        assert!(!valid_role(&format!("Use {key} to log in"))
            .unwrap()
            .contains(&key));
    }

    #[test]
    fn only_local_private_and_tailnet_addresses_are_called() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for ok in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.9",
            "192.168.0.20",
            "169.254.1.1",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(ip_allowed(ip(ok)), "{ok}");
        }
        assert!(ip_allowed(IpAddr::V4(Ipv4Addr::new(100, 100, 0, 9))));
        assert!(ip_allowed(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(ip_allowed(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 1))));
        assert!(!ip_allowed(IpAddr::V4(Ipv4Addr::new(100, 63, 0, 1))));
        assert!(!ip_allowed(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        for bad in [
            "8.8.8.8",
            "172.32.0.1",
            "0.0.0.0",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!ip_allowed(ip(bad)), "{bad}");
        }
        assert_eq!(
            ollama_base("127.0.0.1:11434").unwrap(),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            ollama_base("http://[::1]:11434/").unwrap(),
            "http://[::1]:11434"
        );
        assert_eq!(
            ollama_base("https://localhost").unwrap(),
            "https://localhost"
        );
        for bad in [
            "http://8.8.8.8:11434",
            "http://user:pw@127.0.0.1:11434",
            "http://127.0.0.1:11434/api",
            "ftp://127.0.0.1",
            "http://",
            "http://127.0.0.1:notaport",
        ] {
            assert!(ollama_base(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_chat_body_never_carries_tools() {
        let b = chat_body("mock:4b", "sys", "hello");
        assert_eq!(b["stream"], false);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["content"], "hello");
        assert!(b.get("tools").is_none() && b.get("functions").is_none());
    }
}
