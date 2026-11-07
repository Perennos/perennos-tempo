//! Talking to the agent and to Room, both on this computer: a small HTTP client (std only), the
//! agent's guest conversations, and Room's owner calls.
//!
//! Only 127.0.0.1 is ever called, so the agent's key and Room's control key never leave this
//! computer. No proxy, no redirect is followed.

use super::spec::{Band, Member};
use super::{Chat, Said};
use crate::setup::json::{parse, J};
use crate::{json_escape, parse_response, signed, Ctx, VERSION};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_ANSWER: usize = 4 << 20;

/// One request to `127.0.0.1:port`: the status code and the body.
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut s = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("could not reach 127.0.0.1:{port} ({e})"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nUser-Agent: tempo-launch/{VERSION}\r\nConnection: close\r\n"
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    s.write_all(req.as_bytes())
        .map_err(|e| format!("could not send to 127.0.0.1:{port} ({e})"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                if buf.is_empty() {
                    return Err(format!("no answer from 127.0.0.1:{port} ({e})"));
                }
                break;
            }
        }
        if buf.len() > MAX_ANSWER {
            return Err("the answer was too big".into());
        }
    }
    parse_response(&buf, true).ok_or_else(|| format!("127.0.0.1:{port} did not answer with HTTP"))
}

fn error_text(code: u16, body: &str) -> String {
    parse(body)
        .ok()
        .and_then(|j| j.str("error").map(String::from))
        .unwrap_or_else(|| format!("HTTP {code}"))
}

// ---- the agent

/// The agent's HTTP API: guest conversations with a model and a role, so no tools.
pub struct Agent {
    pub port: u16,
    pub key: Option<String>,
    /// How often to look for the answer.
    pub poll: Duration,
    /// How long one answer may take (a local model can be slow).
    pub patience: Duration,
}

impl Agent {
    pub fn new(port: u16, key: Option<String>) -> Agent {
        Agent {
            port,
            key,
            poll: Duration::from_millis(700),
            patience: Duration::from_secs(900),
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<J, String> {
        let key = self.key.as_deref().unwrap_or("");
        let headers: Vec<(&str, &str)> = if key.is_empty() {
            vec![]
        } else {
            vec![("X-Tempo-Key", key)]
        };
        let (code, text) = request(
            self.port,
            method,
            path,
            &headers,
            body,
            Duration::from_secs(30),
        )?;
        if code != 200 {
            return Err(format!("the agent said: {}", error_text(code, &text)));
        }
        parse(&text).map_err(|e| format!("the agent's answer was not JSON ({e})"))
    }

    /// What the agent's health says it can do: (own model and role, own engine).
    fn abilities(&self) -> (bool, bool) {
        request(
            self.port,
            "GET",
            "/agent/health",
            &[],
            None,
            Duration::from_secs(5),
        )
        .ok()
        .filter(|(c, b)| *c == 200 && signed(b, &["\"ok\":true"]))
        .map_or((false, false), |(_, b)| {
            (
                signed(&b, &["\"roles\":true"]),
                signed(&b, &["\"engines\":true"]),
            )
        })
    }

    /// Whether this agent can hold a conversation with its own model and role.
    pub fn knows_roles(&self) -> bool {
        self.abilities().0
    }

    /// Whether a conversation can ask for an engine of its own (an older agent only has its own
    /// order: the main engine, then the fallbacks).
    pub fn knows_engines(&self) -> bool {
        self.abilities().1
    }

    /// A new guest conversation: no tools, no memory of the owner's files.
    pub fn new_guest(&self, model: &str, role: &str) -> Result<String, String> {
        self.new_guest_on(model, role, "")
    }

    /// `new_guest`, asking for `engine` (an address; empty: the agent's own order).
    pub fn new_guest_on(&self, model: &str, role: &str, engine: &str) -> Result<String, String> {
        self.new_guest_with(model, role, engine, "")
    }

    /// `new_guest_on`, and every answer must follow `schema` (JSON Schema text; empty: none).
    /// An older agent ignores the field and answers as it always did.
    pub fn new_guest_with(
        &self,
        model: &str,
        role: &str,
        engine: &str,
        schema: &str,
    ) -> Result<String, String> {
        let mut body = format!("{{\"mode\":\"guest\",\"role\":\"{}\"", json_escape(role));
        if !model.is_empty() {
            body.push_str(&format!(",\"model\":\"{}\"", json_escape(model)));
        }
        if !engine.is_empty() {
            body.push_str(&format!(",\"engine\":\"{}\"", json_escape(engine)));
        }
        if !schema.is_empty() {
            body.push_str(&format!(",\"format\":{schema}"));
        }
        body.push('}');
        let j = self.call("POST", "/agent/new", Some(&body))?;
        j.str("conv")
            .map(String::from)
            .ok_or_else(|| "the agent made no conversation".into())
    }

    /// Send one message and wait for the turn to end; the answer is the last thing it said.
    pub fn ask(&self, conv: &str, text: &str) -> Result<String, String> {
        self.ask_on(conv, text).map(|a| a.text)
    }

    /// `ask`, with the engine the agent says answered (an agent that does not say, gives none).
    pub fn ask_on(&self, conv: &str, text: &str) -> Result<Said, String> {
        let body = format!(
            "{{\"conv\":\"{}\",\"text\":\"{}\"}}",
            json_escape(conv),
            json_escape(text)
        );
        self.call("POST", "/agent/send", Some(&body))?;
        let end = Instant::now() + self.patience;
        loop {
            std::thread::sleep(self.poll);
            let j = self.call(
                "GET",
                &format!("/agent/events?conv={}&since=0", json_escape(conv)),
                None,
            )?;
            if j.bool("busy") != Some(true) {
                let engine = j.str("engine").filter(|e| !e.is_empty()).map(String::from);
                return answer_of(&j).map(|text| Said { text, engine });
            }
            if Instant::now() > end {
                return Err("the model took too long to answer".into());
            }
        }
    }
}

/// What a finished turn said: its last answer, or its error.
fn answer_of(events: &J) -> Result<String, String> {
    let evs = events.arr("events");
    if let Some(e) = evs.iter().rev().find(|e| e.str("type") == Some("error")) {
        return Err(format!(
            "the agent could not answer: {}",
            e.str("text").unwrap_or("unknown error")
        ));
    }
    evs.iter()
        .rev()
        .find(|e| e.str("type") == Some("assistant"))
        .and_then(|e| e.str("text"))
        .filter(|t| !t.trim().is_empty())
        .map(String::from)
        .ok_or_else(|| "the model sent back nothing".into())
}

impl Chat for Agent {
    fn say(&self, model: &str, role: &str, text: &str) -> Result<String, String> {
        let conv = self.new_guest(model, role)?;
        self.ask(&conv, text)
    }

    fn say_json(
        &self,
        model: &str,
        role: &str,
        text: &str,
        schema: &str,
    ) -> Result<String, String> {
        let conv = self.new_guest_with(model, role, "", schema)?;
        self.ask(&conv, text)
    }

    /// The engine is only sent when the agent's health says it can take one. An agent that
    /// refuses the address (its settings do not list it) is asked again without it, so the part
    /// is still played, on the agent's own order.
    fn say_on(&self, engine: &str, model: &str, role: &str, text: &str) -> Result<Said, String> {
        let pin = !engine.is_empty() && self.knows_engines();
        let conv = match self.new_guest_on(model, role, if pin { engine } else { "" }) {
            Err(e) if pin && e.starts_with("the agent said: engine:") => {
                self.new_guest(model, role)?
            }
            r => r?,
        };
        self.ask_on(&conv, text)
    }
}

// ---- Room

/// A running Room on this computer and the key that makes this program its owner.
pub struct RoomLink {
    pub port: u16,
    pub key: String,
}

/// Room's data folder: TEMPO_ROOM_DATA, else `<Tempo home>/room`.
pub fn room_dir(ctx: &Ctx) -> PathBuf {
    std::env::var_os("TEMPO_ROOM_DATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.home.join("room"))
}

/// The running Room in `dir`: `server.json` names 127.0.0.1:PORT, `control.key` is the owner key
/// Room writes at every start. Nothing is sent unless Room answers as tempo-room.
pub fn find_room(dir: &Path) -> Option<RoomLink> {
    let text = std::fs::read_to_string(dir.join("server.json")).ok()?;
    let addr = parse(&text).ok()?.str("addr")?.to_string();
    let port: u16 = addr.strip_prefix("127.0.0.1:")?.parse().ok()?;
    let (code, body) = request(
        port,
        "GET",
        "/room/health",
        &[],
        None,
        Duration::from_secs(3),
    )
    .ok()?;
    if code != 200 || !signed(&body, &["\"app\":\"tempo-room\""]) {
        return None;
    }
    let key = std::fs::read_to_string(dir.join("control.key")).ok()?;
    let key = key.trim();
    (!key.is_empty() && key.len() <= 512 && key.bytes().all(|b| b.is_ascii_graphic())).then(|| {
        RoomLink {
            port,
            key: key.to_string(),
        }
    })
}

impl RoomLink {
    fn call(&self, method: &str, path: &str, body: Option<&J>) -> Result<J, String> {
        let text = body.map(J::compact);
        let (code, answer) = request(
            self.port,
            method,
            path,
            &[("X-Tempo-Control", &self.key)],
            text.as_deref(),
            Duration::from_secs(20),
        )?;
        if code != 200 {
            return Err(error_text(code, &answer));
        }
        parse(&answer).map_err(|e| format!("Room's answer was not JSON ({e})"))
    }
}

/// The request that adds one member as an agent teammate (it answers through the local agent in
/// guest mode, so it has no tools).
pub fn teammate_request(m: &Member, room: &str) -> J {
    let mut o = vec![
        ("handle".to_string(), J::s(&m.handle)),
        ("name".to_string(), J::s(&m.name)),
        ("role".to_string(), J::s(&super::with_guard(&m.role))),
        ("kind".to_string(), J::s("agent")),
    ];
    if !m.model.is_empty() {
        o.push(("model".into(), J::s(&m.model)));
    }
    o.push(("rooms".into(), J::strings(&[room.to_string()])));
    J::Obj(o)
}

/// What came of putting a band into Room.
#[derive(Debug, Default, PartialEq)]
pub struct RoomReport {
    pub room: String,
    pub added: Vec<String>,
    /// Members that could not be added, each with the reason in words.
    pub left_out: Vec<(String, String)>,
}

/// Make (or find) a room named after the band and add each member as a teammate. A handle that
/// already belongs to a teammate with the same role just joins the room; one with a different role
/// is left alone and reported.
pub fn register_band(link: &RoomLink, band: &Band) -> Result<RoomReport, String> {
    let rooms = link.call("GET", "/room/rooms", None)?;
    let room = match rooms
        .arr("rooms")
        .iter()
        .find(|r| {
            r.str("name")
                .is_some_and(|n| n.eq_ignore_ascii_case(&band.name))
        })
        .and_then(|r| r.str("id"))
    {
        Some(id) => id.to_string(),
        None => {
            let made = link.call(
                "POST",
                "/room/create",
                Some(&J::Obj(vec![("name".into(), J::s(&band.name))])),
            )?;
            made.get("room")
                .and_then(|r| r.str("id"))
                .ok_or("Room made no room")?
                .to_string()
        }
    };
    let team = link.call("GET", "/room/team", None)?;
    let mut report = RoomReport {
        room: room.clone(),
        ..RoomReport::default()
    };
    for m in &band.members {
        let want = super::with_guard(&m.role);
        let existing = team
            .arr("team")
            .iter()
            .find(|t| t.str("handle") == Some(&m.handle));
        let result = match existing {
            // already in this room: nothing to do (so `conductor room` can be run again)
            Some(t) if t.strs("rooms").contains(&room) => Ok(()),
            Some(t) if t.str("role").map(str::trim) == Some(want.trim()) => link
                .call(
                    "POST",
                    "/room/team/room",
                    Some(&J::Obj(vec![
                        ("room".into(), J::s(&room)),
                        ("handle".into(), J::s(&m.handle)),
                        ("on".into(), J::Bool(true)),
                    ])),
                )
                .map(|_| ()),
            Some(_) => Err(format!(
                "@{} is already a teammate with a different role (another band's, or one you wrote); rename this member in band.json and run conductor room again",
                m.handle
            )),
            None => link
                .call("POST", "/room/team/add", Some(&teammate_request(m, &room)))
                .map(|_| ()),
        };
        match result {
            Ok(()) => report.added.push(m.handle.clone()),
            Err(e) => report.left_out.push((m.handle.clone(), e)),
        }
    }
    Ok(report)
}
