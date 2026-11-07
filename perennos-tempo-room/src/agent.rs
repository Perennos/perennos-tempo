//! A client for the Perennos Tempo agent's HTTP API (`tempo-local-agent serve`, loopback).
//!
//! Contract (from the agent's `server.rs`): `POST /agent/new` -> `{"conv"}`; `POST /agent/send
//! {"conv","text"}` -> 200, 409 while busy, 404 for an unknown conversation; `GET
//! /agent/events?conv&since` -> `{"events":[{"n","t","type","text",...}],"busy"}`; `POST
//! /agent/confirm {"conv","id","ok"}`; `GET /agent/health`. The Host header is the agent URL's
//! host:port (ureq sets it), POSTs are JSON, and no Origin header is ever sent.
//!
//! The key: every call except `GET /agent/health` sends `X-Tempo-Key`. It is `TEMPO_AGENT_KEY` when
//! that is set (a remote or tunnelled agent), else the contents of `<agent data>/control.key`
//! (`TEMPO_AGENT_DATA`, else `<Tempo home>/agent`), which the agent rewrites on every start. On a 401
//! the file is read once more and the call retried once, because the agent may have restarted. With
//! no key and no file nothing is sent, so an older agent without keys still works. The key is
//! registered with the scrubber and is never logged or shown.
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, PartialEq)]
pub enum AgentError {
    /// Nothing answered: the agent is not running (or not reachable).
    Down(String),
    /// 409: a turn is running in this conversation.
    Busy,
    /// 404: the agent does not know this conversation (its data was reset).
    UnknownConv,
    /// 401: the agent did not accept Room's key (or Room has none).
    Unauthorized,
    Http(u16, String),
    Bad(String),
    /// The agent has no guest mode (`GET /agent/health` without `"guest": true`): a room never
    /// talks to an agent that would give room members its tools or the owner's memory.
    NoGuestMode,
}

pub struct AgentClient {
    base: String,
    http: ureq::Agent,
    /// `TEMPO_AGENT_KEY`: used as given, never replaced by the file.
    env_key: Option<String>,
    /// `<agent data>/control.key`.
    key_file: Option<PathBuf>,
    /// The key last read from the file.
    cached: Mutex<Option<String>>,
}

/// A key that can be sent as a header value: printable ASCII, no spaces, a sane length.
fn clean_key(raw: &str) -> Option<String> {
    let k = raw.trim();
    (!k.is_empty() && k.len() <= 512 && k.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| k.to_string())
}

/// `<agent data>/control.key` for this machine.
pub fn default_key_file() -> PathBuf {
    let data = std::env::var_os("TEMPO_AGENT_DATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::tempo_home::tempo_home().dir.join("agent"));
    data.join("control.key")
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

impl AgentClient {
    pub fn new(base: &str) -> AgentClient {
        let env_key = std::env::var("TEMPO_AGENT_KEY")
            .ok()
            .and_then(|k| clean_key(&k));
        AgentClient::with_key(base, env_key, Some(default_key_file()))
    }

    /// `new` with the key sources given (tests use this; the program uses `new`).
    pub fn with_key(base: &str, env_key: Option<String>, key_file: Option<PathBuf>) -> AgentClient {
        if let Some(k) = &env_key {
            crate::util::register_secret(k);
        }
        AgentClient {
            base: base.trim().trim_end_matches('/').to_string(),
            http: agent(Duration::from_secs(20)),
            env_key,
            key_file,
            cached: Mutex::new(None),
        }
    }

    /// The key to send now: the environment's, else the file's (read when nothing is cached).
    fn key(&self) -> Option<String> {
        if self.env_key.is_some() {
            return self.env_key.clone();
        }
        let mut c = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if c.is_none() {
            *c = self.read_file();
        }
        c.clone()
    }

    fn read_file(&self) -> Option<String> {
        let k = std::fs::read_to_string(self.key_file.as_ref()?)
            .ok()
            .and_then(|t| clean_key(&t))?;
        crate::util::register_secret(&k);
        Some(k)
    }

    /// After a 401: the file's key, if it is not the one just refused. `None`: nothing new to try.
    fn fresh_key(&self, refused: Option<&str>) -> Option<String> {
        if self.env_key.is_some() {
            return None;
        }
        let k = self.read_file().filter(|k| Some(k.as_str()) != refused)?;
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(k.clone());
        Some(k)
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// One call. Everything but the health check carries the key; a 401 reads the key file once
    /// more and retries once.
    fn call(&self, path: &str, body: Option<&Value>) -> Result<(u16, Value), AgentError> {
        if path == "/agent/health" {
            return self.call_with(path, body, None);
        }
        let key = self.key();
        let first = self.call_with(path, body, key.as_deref())?;
        if first.0 != 401 {
            return Ok(first);
        }
        match self.fresh_key(key.as_deref()) {
            Some(k) => self.call_with(path, body, Some(&k)),
            None => Ok(first),
        }
    }

    fn call_with(
        &self,
        path: &str,
        body: Option<&Value>,
        key: Option<&str>,
    ) -> Result<(u16, Value), AgentError> {
        let url = format!("{}{path}", self.base);
        let resp = match body {
            None => {
                let mut r = self.http.get(&url);
                if let Some(k) = key {
                    r = r.header("X-Tempo-Key", k);
                }
                r.call()
            }
            Some(b) => {
                let mut r = self
                    .http
                    .post(&url)
                    .header("Content-Type", "application/json");
                if let Some(k) = key {
                    r = r.header("X-Tempo-Key", k);
                }
                r.send(b.to_string())
            }
        };
        let mut resp = resp.map_err(|e| AgentError::Down(e.to_string()))?;
        let code = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(16 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| AgentError::Down(e.to_string()))?;
        let v = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok((code, v))
    }

    /// True when the agent answers its health check in time.
    pub fn health(&self, timeout: Duration) -> bool {
        let url = format!("{}/agent/health", self.base);
        self.http
            .get(&url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call()
            .is_ok_and(|r| r.status().as_u16() == 200)
    }

    /// Does the agent take a model and a role when a guest conversation is made? Its health says
    /// `"roles": true` (with or without the key). An older agent, or one that is down: false.
    pub fn roles_supported(&self) -> bool {
        matches!(self.call("/agent/health", None), Ok((200, h)) if h["roles"] == true)
    }

    /// A guest conversation: the agent offers it no tools, no memory and no owner context.
    pub fn new_conv(&self) -> Result<String, AgentError> {
        self.new_conv_with(None, None).map(|(c, _)| c)
    }

    /// A guest conversation for a teammate: with its `model` and `role` when the agent supports
    /// them (`"roles": true` in its health); an older agent gets the plain guest conversation
    /// (the caller then puts the role in front of the request instead). It stays a guest
    /// conversation either way.
    ///
    /// Returns the conversation and whether the agent took roles for it (the health this very
    /// call read said `"roles": true`), so the caller records how the conversation was made.
    pub fn new_conv_with(
        &self,
        model: Option<&str>,
        role: Option<&str>,
    ) -> Result<(String, bool), AgentError> {
        let roles = match self.call("/agent/health", None)? {
            (200, h) if h["guest"] == true => h["roles"] == true,
            (200, _) => return Err(AgentError::NoGuestMode),
            (401, _) => return Err(AgentError::Unauthorized),
            (c, v) => {
                return Err(AgentError::Http(
                    c,
                    v["error"].as_str().unwrap_or("").into(),
                ))
            }
        };
        let mut body = json!({"mode": "guest"});
        if roles {
            if let Some(m) = model {
                body["model"] = json!(m);
            }
            if let Some(r) = role {
                body["role"] = json!(r);
            }
        }
        match self.call("/agent/new", Some(&body))? {
            (200, v) => v["conv"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(|c| (c.to_string(), roles))
                .ok_or_else(|| AgentError::Bad("no conversation id".into())),
            (401, _) => Err(AgentError::Unauthorized),
            (c, v) => Err(AgentError::Http(
                c,
                v["error"].as_str().unwrap_or("").into(),
            )),
        }
    }

    pub fn send(&self, conv: &str, text: &str) -> Result<(), AgentError> {
        match self.call("/agent/send", Some(&json!({"conv": conv, "text": text})))? {
            (200, _) => Ok(()),
            (409, _) => Err(AgentError::Busy),
            (404, _) => Err(AgentError::UnknownConv),
            (401, _) => Err(AgentError::Unauthorized),
            (c, v) => Err(AgentError::Http(
                c,
                v["error"].as_str().unwrap_or("").into(),
            )),
        }
    }

    pub fn events(&self, conv: &str, since: u64) -> Result<(Vec<Value>, bool), AgentError> {
        let path = format!(
            "/agent/events?conv={}&since={since}",
            crate::util::pct_encode(conv)
        );
        match self.call(&path, None)? {
            (200, v) => Ok((
                v["events"].as_array().cloned().unwrap_or_default(),
                v["busy"].as_bool().unwrap_or(false),
            )),
            (401, _) => Err(AgentError::Unauthorized),
            (c, v) => Err(AgentError::Http(
                c,
                v["error"].as_str().unwrap_or("").into(),
            )),
        }
    }

    /// Deny a confirmation. Room messages never approve anything.
    pub fn deny(&self, conv: &str, id: &str) -> Result<(), AgentError> {
        self.call(
            "/agent/confirm",
            Some(&json!({"conv": conv, "id": id, "ok": false})),
        )
        .and_then(|(c, _)| {
            if c == 401 {
                Err(AgentError::Unauthorized)
            } else {
                Ok(())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeAgent, Script};

    const OLD: &str = "old-key-0123456789abcdef";
    const NEW: &str = "new-key-fedcba9876543210";

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("room-agent-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The `X-Tempo-Key` of every request the fake agent saw (empty: none sent), health included.
    fn keys(a: &FakeAgent) -> Vec<(String, Option<String>)> {
        a.http
            .requests()
            .iter()
            .map(|r| (r.path.clone(), r.header("x-tempo-key").map(str::to_string)))
            .collect()
    }

    #[test]
    fn no_key_and_no_file_sends_nothing_so_an_older_agent_still_works() {
        let a = FakeAgent::start(vec![Script::Answer("hi".into())]);
        let c = AgentClient::with_key(&a.url(), None, Some(dir("nokey").join("control.key")));
        let conv = c.new_conv().unwrap();
        c.send(&conv, "hello").unwrap();
        assert!(keys(&a).iter().all(|(_, k)| k.is_none()), "{:?}", keys(&a));
    }

    #[test]
    fn a_key_is_required_and_missing_gives_unauthorized() {
        let a = FakeAgent::start(vec![]);
        a.require_key(OLD);
        let c = AgentClient::with_key(&a.url(), None, None);
        assert_eq!(c.new_conv(), Err(AgentError::Unauthorized));
        // one try, no retry loop (there is no file to read again)
        assert_eq!(
            keys(&a).iter().filter(|(p, _)| p == "/agent/new").count(),
            1
        );
    }

    #[test]
    fn the_key_file_is_sent_on_every_call_but_health() {
        let d = dir("file");
        std::fs::write(d.join("control.key"), format!("{OLD}\n")).unwrap();
        let a = FakeAgent::start(vec![Script::Answer("hi".into())]);
        a.require_key(OLD);
        let c = AgentClient::with_key(&a.url(), None, Some(d.join("control.key")));
        assert!(c.health(Duration::from_secs(2)));
        let conv = c.new_conv().unwrap();
        c.send(&conv, "hello").unwrap();
        c.events(&conv, 0).unwrap();
        c.deny(&conv, "c-1").unwrap();
        for (path, key) in keys(&a) {
            if path == "/agent/health" {
                assert_eq!(key, None, "health never carries the key");
            } else {
                assert_eq!(key.as_deref(), Some(OLD), "{path}");
            }
        }
    }

    #[test]
    fn a_stale_key_is_replaced_once_from_the_file_after_a_401() {
        let d = dir("stale");
        let file = d.join("control.key");
        std::fs::write(&file, OLD).unwrap();
        let a = FakeAgent::start(vec![Script::Answer("hi".into())]);
        a.require_key(OLD);
        let c = AgentClient::with_key(&a.url(), None, Some(file.clone()));
        let conv = c.new_conv().unwrap();
        // the agent restarts with a new key and rewrites the file
        a.require_key(NEW);
        std::fs::write(&file, NEW).unwrap();
        c.send(&conv, "hello").unwrap();
        let sends: Vec<_> = keys(&a)
            .into_iter()
            .filter(|(p, _)| p == "/agent/send")
            .collect();
        assert_eq!(
            sends,
            vec![
                ("/agent/send".to_string(), Some(OLD.to_string())),
                ("/agent/send".to_string(), Some(NEW.to_string()))
            ]
        );
        // and the fresh key is kept for the next call
        c.events(&conv, 0).unwrap();
        assert_eq!(keys(&a).last().unwrap().1.as_deref(), Some(NEW));
    }

    #[test]
    fn an_unchanged_file_after_a_401_is_not_retried() {
        let d = dir("same");
        let file = d.join("control.key");
        std::fs::write(&file, OLD).unwrap();
        let a = FakeAgent::start(vec![]);
        a.require_key(NEW);
        let c = AgentClient::with_key(&a.url(), None, Some(file));
        assert_eq!(c.new_conv(), Err(AgentError::Unauthorized));
        assert_eq!(
            keys(&a).iter().filter(|(p, _)| p == "/agent/new").count(),
            1
        );
    }

    #[test]
    fn the_environment_key_wins_over_the_file_and_is_never_replaced() {
        let d = dir("env");
        let file = d.join("control.key");
        std::fs::write(&file, OLD).unwrap();
        let a = FakeAgent::start(vec![Script::Answer("hi".into())]);
        a.require_key(NEW);
        let c = AgentClient::with_key(&a.url(), Some(NEW.into()), Some(file.clone()));
        c.new_conv().unwrap();
        assert!(keys(&a)
            .iter()
            .filter(|(p, _)| p != "/agent/health")
            .all(|(_, k)| k.as_deref() == Some(NEW)));
        // a wrong environment key is refused and not swapped for the file's
        let bad = AgentClient::with_key(&a.url(), Some(OLD.into()), Some(file));
        let before = keys(&a).len();
        assert_eq!(bad.new_conv(), Err(AgentError::Unauthorized));
        // health + one /agent/new, nothing more
        assert_eq!(keys(&a).len() - before, 2);
    }

    #[test]
    fn the_key_is_scrubbed_and_never_in_errors_or_the_log() {
        let d = dir("scrub");
        let file = d.join("control.key");
        let secret = "scrub-me-0123456789abcdef";
        std::fs::write(&file, secret).unwrap();
        let a = FakeAgent::start(vec![]);
        a.require_key(NEW);
        let c = AgentClient::with_key(&a.url(), None, Some(file));
        let e = c.new_conv().unwrap_err();
        assert!(!format!("{e:?}").contains(secret));
        assert_eq!(
            crate::util::scrub(&format!("x {secret} y")),
            "x [REDACTED] y"
        );
        let env = AgentClient::with_key(&a.url(), Some("env-secret-0123456789".into()), None);
        drop(env);
        assert_eq!(
            crate::util::scrub("a env-secret-0123456789"),
            "a [REDACTED]"
        );
    }
    #[test]
    fn a_teammates_model_and_role_go_only_to_an_agent_that_takes_them() {
        let old = FakeAgent::start(vec![]);
        let c = AgentClient::with_key(&old.url(), None, None);
        assert!(!c.roles_supported());
        c.new_conv_with(Some("qwen3:4b"), Some("You are Sage."))
            .unwrap();
        assert_eq!(old.new_bodies(), [json!({"mode": "guest"})]);
        let new = FakeAgent::start(vec![]);
        new.with_roles();
        let c = AgentClient::with_key(&new.url(), None, None);
        assert!(c.roles_supported());
        c.new_conv_with(Some("qwen3:4b"), Some("You are Sage."))
            .unwrap();
        c.new_conv_with(None, None).unwrap();
        c.new_conv_with(Some("qwen3:4b"), None).unwrap();
        assert_eq!(
            new.new_bodies(),
            [
                json!({"mode": "guest", "model": "qwen3:4b", "role": "You are Sage."}),
                json!({"mode": "guest"}),
                json!({"mode": "guest", "model": "qwen3:4b"}),
            ]
        );
        // what the agent would refuse comes back as an error, not a conversation
        assert!(matches!(
            c.new_conv_with(Some("bad model"), None),
            Err(AgentError::Http(400, _))
        ));
        // an agent without guest mode is never used, roles or not
        let nog = FakeAgent::start(vec![]);
        nog.without_guest_mode();
        assert_eq!(
            AgentClient::with_key(&nog.url(), None, None).new_conv_with(None, Some("x")),
            Err(AgentError::NoGuestMode)
        );
        // the key still goes along; health needs none
        let keyed = FakeAgent::start(vec![]);
        keyed.with_roles();
        keyed.require_key(OLD);
        let c = AgentClient::with_key(&keyed.url(), Some(OLD.into()), None);
        assert!(c.roles_supported(), "health answers without the key");
        c.new_conv_with(Some("m"), Some("r")).unwrap();
    }
}
