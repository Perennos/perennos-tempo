//! The room's data folder: `config.json` (owner token, bridges, personality defaults, limits),
//! `rooms.json`, `members.json`, `invites.json` and one JSONL message file per room under
//! `messages/`. Every rewrite is atomic and keeps the previous copy; removed members go to
//! `trash/`. Message text is redacted before it is stored. Session tokens and invite codes are
//! kept only as SHA-256 hashes.
use crate::util::{
    ct_eq, expand_home, has_hidden, name_key, new_invite_code, new_token, normalize_code,
    random_bytes, read_json, register_secret, sha256_hex, slug, to_trash, unix_now, valid_room_id,
    write_atomic, write_private,
};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 8767;
pub const DEFAULT_AGENT: &str = "http://127.0.0.1:8765";
/// Messages kept in memory per room (older ones are read from the file when asked for).
pub const TAIL: usize = 1000;
/// Longest message a person can post, in characters.
pub const MAX_TEXT: usize = 4000;
/// An owner link works once, for this long.
pub const OWNER_LINK_SECS: f64 = 7.0 * 86_400.0;
/// An owner session (the cookie the owner link gives the browser) lasts this long.
pub const OWNER_SESSION_SECS: f64 = 30.0 * 86_400.0;
/// Owner sessions kept (the newest); more than a few means something is wrong.
const MAX_OWNER_SESSIONS: usize = 20;
/// A member's `last_seen` is written at most this often (seconds), so ordinary use does not
/// rewrite `members.json` on every request.
pub const SEEN_WRITE_SECS: f64 = 3600.0;
/// Sessions (devices) one member can hold; the least recently used go first.
pub const MAX_MEMBER_SESSIONS: usize = 20;
/// A session unused for this many days stops working unless `session_days` says otherwise.
pub const DEFAULT_SESSION_DAYS: f64 = 180.0;

/// The data folder: `--data`, else `TEMPO_ROOM_DATA`, else `<Tempo home>/room` (the one Tempo home
/// per machine, see `tempo_home.rs`).
pub fn data_dir(flag: Option<&str>) -> PathBuf {
    flag.filter(|s| !s.trim().is_empty())
        .map(expand_home)
        .or_else(|| {
            std::env::var("TEMPO_ROOM_DATA")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| expand_home(&s))
        })
        .unwrap_or_else(|| crate::tempo_home::tempo_home().dir.join("room"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Tempo answers when someone writes `@tempo`.
    Mention,
    /// Tempo answers every message posted in the app.
    Always,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mention" => Some(Mode::Mention),
            "always" => Some(Mode::Always),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Mention => "mention",
            Mode::Always => "always",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Personality {
    Plain,
    Warm,
    Playful,
}

impl Personality {
    pub fn parse(s: &str) -> Option<Personality> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plain" => Some(Personality::Plain),
            "warm" => Some(Personality::Warm),
            "playful" => Some(Personality::Playful),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Personality::Plain => "plain",
            Personality::Warm => "warm",
            Personality::Playful => "playful",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A person (in the app, the CLI or a bridge).
    User,
    /// Perennos Tempo.
    Tempo,
    /// A note from the room itself (someone joined, the mode changed).
    System,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Tempo => "tempo",
            Kind::System => "system",
        }
    }
    fn parse(s: &str) -> Kind {
        match s {
            "tempo" => Kind::Tempo,
            "system" => Kind::System,
            _ => Kind::User,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub id: u64,
    pub t: f64,
    pub room: String,
    /// `owner`, a member id (`m_...`), `tempo`, `system`, or `discord:<id>` and the like.
    pub from: String,
    pub name: String,
    pub kind: Kind,
    /// `app`, `cli`, `mcp`, `discord`, `telegram` or `whatsapp`.
    pub via: String,
    pub text: String,
    pub reply_to: Option<u64>,
    pub reply_name: Option<String>,
    pub reply_text: Option<String>,
}

impl Msg {
    pub fn new(room: &str, from: &str, name: &str, kind: Kind, via: &str, text: &str) -> Msg {
        Msg {
            id: 0,
            t: 0.0,
            room: room.to_string(),
            from: from.to_string(),
            name: name.to_string(),
            kind,
            via: via.to_string(),
            text: text.to_string(),
            reply_to: None,
            reply_name: None,
            reply_text: None,
        }
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("t".into(), json!(self.t));
        m.insert("room".into(), json!(self.room));
        m.insert("from".into(), json!(self.from));
        m.insert("name".into(), json!(self.name));
        m.insert("kind".into(), json!(self.kind.as_str()));
        m.insert("via".into(), json!(self.via));
        m.insert("text".into(), json!(self.text));
        if let Some(r) = self.reply_to {
            m.insert("reply_to".into(), json!(r));
            m.insert("reply_name".into(), json!(self.reply_name));
            m.insert("reply_text".into(), json!(self.reply_text));
        }
        Value::Object(m)
    }

    pub fn from_json(v: &Value) -> Option<Msg> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Some(Msg {
            id: v["id"].as_u64()?,
            t: v["t"].as_f64().unwrap_or(0.0),
            room: s("room"),
            from: s("from"),
            name: s("name"),
            kind: Kind::parse(v["kind"].as_str().unwrap_or("")),
            via: s("via"),
            text: s("text"),
            reply_to: v["reply_to"].as_u64(),
            reply_name: v["reply_name"].as_str().map(str::to_string),
            reply_text: v["reply_text"].as_str().map(str::to_string),
        })
    }
}

pub struct Room {
    pub id: String,
    pub name: String,
    pub mode: Mode,
    pub personality: Personality,
    pub created: f64,
    /// The Perennos Tempo agent conversation this room talks in (one per room).
    pub conv: Option<String>,
    /// The id the next message gets.
    pub next: u64,
    pub tail: VecDeque<Msg>,
}

impl Room {
    fn to_json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "mode": self.mode.as_str(), "personality": self.personality.as_str(), "created": self.created, "conv": self.conv})
    }
}

#[derive(Clone, Debug)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub rooms: Vec<String>,
    pub muted: Vec<String>,
    /// One per signed-in device.
    pub sessions: Vec<Session>,
    pub joined: f64,
}

/// One signed-in device of a member. Only the SHA-256 of its cookie value is kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub hash: String,
    pub created: f64,
    /// When it was last used (written at most once an hour, see `SEEN_WRITE_SECS`).
    pub last_seen: f64,
}

impl Member {
    fn to_json(&self) -> Value {
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|s| json!({"hash": s.hash, "created": s.created, "last_seen": s.last_seen}))
            .collect();
        json!({"id": self.id, "name": self.name, "rooms": self.rooms, "muted": self.muted, "sessions": sessions, "joined": self.joined})
    }
    /// Reads both shapes. A members file written before devices existed has one `token_hash`;
    /// it becomes one session, last seen `now` (so nobody is signed out by the upgrade). The
    /// flag says the old shape was read (so the caller writes the new one).
    fn from_json(v: &Value, now: f64) -> Option<(Member, bool)> {
        let list = |k: &str| -> Vec<String> {
            v[k].as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        };
        let joined = v["joined"].as_f64().unwrap_or(0.0);
        let (sessions, old_shape) = match v["sessions"].as_array() {
            Some(a) => (
                a.iter()
                    .filter_map(|x| {
                        let created = x["created"].as_f64().unwrap_or(joined);
                        Some(Session {
                            hash: x["hash"].as_str()?.to_string(),
                            created,
                            last_seen: x["last_seen"].as_f64().unwrap_or(created),
                        })
                    })
                    .collect(),
                false,
            ),
            None => (
                vec![Session {
                    hash: v["token_hash"].as_str()?.to_string(),
                    created: joined,
                    last_seen: now,
                }],
                true,
            ),
        };
        Some((
            Member {
                id: v["id"].as_str()?.to_string(),
                name: v["name"].as_str()?.to_string(),
                rooms: list("rooms"),
                muted: list("muted"),
                sessions,
                joined,
            },
            old_shape,
        ))
    }
}

#[derive(Clone, Debug)]
pub struct Invite {
    pub hash: String,
    /// The room it opens. Empty for a sign-in link (a device invite).
    pub room: String,
    /// Set for a sign-in link: the member whose new device it signs in, and nobody else.
    pub member: Option<String>,
    pub created: f64,
    pub expires: f64,
    pub used: Option<f64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InviteError {
    NotFound,
    Used,
    Expired,
}

impl InviteError {
    pub fn message(&self) -> &'static str {
        match self {
            InviteError::NotFound => {
                "That invite code didn't work. Check it, or ask for a new one."
            }
            InviteError::Used => {
                "That invite was already used. Invites work once; ask for a new one."
            }
            InviteError::Expired => "That invite has expired. Ask for a new one.",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    /// Messages a person can post per minute (the owner is not limited).
    pub post_per_min: usize,
    /// Questions to Tempo per person per `ask_member_secs`.
    pub ask_per_member: usize,
    pub ask_member_secs: u64,
    /// Questions to Tempo per room per `ask_room_secs`.
    pub ask_per_room: usize,
    pub ask_room_secs: u64,
    /// Join attempts per address per ten minutes (invite codes cannot be guessed).
    pub join_per_ip: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            post_per_min: 20,
            ask_per_member: 5,
            ask_member_secs: 300,
            ask_per_room: 20,
            ask_room_secs: 600,
            join_per_ip: 10,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The whole file, so keys this version does not know survive a rewrite.
    pub raw: Value,
    pub owner_name: String,
    /// `MM-DD` (or `YYYY-MM-DD`); empty means no birthday greeting.
    pub birthday: String,
    pub vault: Option<String>,
    pub agent: String,
    pub utc_offset: Option<String>,
    /// Longest Tempo reply, in characters.
    pub reply_cap: usize,
    /// How long Tempo waits for the agent's answer.
    pub answer_secs: u64,
    pub invite_hours: f64,
    /// A member's session unused for this many days stops working.
    pub session_days: f64,
    pub limits: Limits,
}

impl Config {
    pub fn from_json(v: &Value) -> Config {
        let s = |k: &str| v[k].as_str().unwrap_or("").trim().to_string();
        let l = &v["limits"];
        let d = Limits::default();
        let n = |k: &str, def: usize| l[k].as_u64().map_or(def, |x| x as usize);
        let limits = Limits {
            post_per_min: n("post_per_min", d.post_per_min),
            ask_per_member: n("ask_per_member", d.ask_per_member),
            ask_member_secs: l["ask_member_secs"].as_u64().unwrap_or(d.ask_member_secs),
            ask_per_room: n("ask_per_room", d.ask_per_room),
            ask_room_secs: l["ask_room_secs"].as_u64().unwrap_or(d.ask_room_secs),
            join_per_ip: n("join_per_ip", d.join_per_ip),
        };
        let vault = Some(s("vault")).filter(|x| !x.is_empty()).or_else(|| {
            std::env::var("TEMPO_VAULT")
                .ok()
                .filter(|x| !x.trim().is_empty())
        });
        Config {
            raw: v.clone(),
            owner_name: Some(s("owner_name"))
                .filter(|x| !x.is_empty())
                .unwrap_or_else(|| "Owner".into()),
            birthday: s("birthday"),
            vault,
            agent: Some(s("agent"))
                .filter(|x| !x.is_empty())
                .unwrap_or_else(|| DEFAULT_AGENT.into()),
            utc_offset: Some(s("utc_offset")).filter(|x| !x.is_empty()),
            reply_cap: v["reply_cap"].as_u64().unwrap_or(1200).clamp(200, 4000) as usize,
            answer_secs: v["answer_secs"].as_u64().unwrap_or(300).clamp(5, 3600),
            invite_hours: v["invite_hours"]
                .as_f64()
                .unwrap_or(48.0)
                .clamp(0.01, 24.0 * 30.0),
            session_days: v["session_days"]
                .as_f64()
                .filter(|d| d.is_finite())
                .unwrap_or(DEFAULT_SESSION_DAYS)
                .clamp(1.0, 3650.0),
            limits,
        }
    }

    /// `MM-DD` of the birthday, if one is set and well formed.
    pub fn birthday_md(&self) -> Option<String> {
        let b = self.birthday.trim();
        let md = if b.len() == 10 { &b[5..] } else { b };
        let ok = md.len() == 5
            && md.as_bytes()[2] == b'-'
            && md[..2].parse::<u32>().is_ok_and(|m| (1..=12).contains(&m))
            && md[3..].parse::<u32>().is_ok_and(|d| (1..=31).contains(&d));
        ok.then(|| md.to_string())
    }
}

pub struct Store {
    pub dir: PathBuf,
    pub cfg: Config,
    pub rooms: Vec<Room>,
    pub members: Vec<Member>,
    pub invites: Vec<Invite>,
    /// Owner sessions, by hash: the browser cookie is never the owner link.
    pub owner_sessions: Vec<OwnerSession>,
}

/// A signed-in owner browser. Only the SHA-256 of its cookie value is kept.
#[derive(Clone, Debug)]
pub struct OwnerSession {
    pub hash: String,
    pub created: f64,
    pub expires: f64,
}

/// The local control key: `control.key` in the data folder (owner-only file), written by
/// `serve` at each start. The CLI and MCP send it in `X-Tempo-Control`, the server accepts it
/// only from this machine.
pub fn control_key_path(dir: &Path) -> PathBuf {
    dir.join("control.key")
}

pub fn read_control_key(dir: &Path) -> Option<String> {
    let k = std::fs::read_to_string(control_key_path(dir)).ok()?;
    let k = k.trim().to_string();
    (k.len() >= 32).then_some(k)
}

/// A fresh control key, saved owner-only (the previous one stops working).
pub fn new_control_key(dir: &Path) -> Result<String, String> {
    let k = new_token()?;
    write_private(&control_key_path(dir), k.as_bytes())?;
    register_secret(&k);
    Ok(k)
}

fn msg_path(dir: &Path, room: &str) -> PathBuf {
    dir.join("messages").join(format!("{room}.jsonl"))
}

/// The last `max` messages of a room file and the highest id in it.
fn load_tail(path: &Path, max: usize) -> (VecDeque<Msg>, u64) {
    let mut out = VecDeque::new();
    let Ok(mut f) = std::fs::File::open(path) else {
        return (out, 0);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let window = 1024 * 1024u64;
    let start = len.saturating_sub(window);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (out, 0);
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        lines.next(); // a partial line
    }
    let mut last = 0;
    for l in lines {
        if let Some(m) = serde_json::from_str::<Value>(l)
            .ok()
            .and_then(|v| Msg::from_json(&v))
        {
            last = last.max(m.id);
            out.push_back(m);
            if out.len() > max {
                out.pop_front();
            }
        }
    }
    (out, last)
}

pub fn valid_name(name: &str) -> Result<String, String> {
    let n: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = n.chars().count();
    if count == 0 || count > 32 {
        return Err("A name needs 1 to 32 characters.".into());
    }
    if n.chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '@' | '"' | '[' | ']' | ':'))
    {
        return Err("A name cannot contain < > [ ] : @ or quotes.".into());
    }
    if has_hidden(&n) {
        return Err("A name cannot contain hidden or direction-changing characters.".into());
    }
    if name_key(&n).is_empty() {
        return Err("A name needs at least one letter or number.".into());
    }
    Ok(n)
}

impl Store {
    /// Open (or start) the data folder. A missing owner token is minted and saved.
    pub fn open(dir: &Path) -> Result<Store, String> {
        #[cfg(unix)]
        let is_new = !dir.exists();
        std::fs::create_dir_all(dir.join("messages"))
            .map_err(|e| format!("cannot use the data folder {}: {e}", dir.display()))?;
        #[cfg(unix)]
        if is_new {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        crate::util::tighten(dir);
        let cfg_path = dir.join("config.json");
        let raw = match read_json(&cfg_path) {
            Some(v) if v.is_object() => v,
            None if !cfg_path.exists() => json!({}),
            _ => {
                return Err(format!(
                    "{} is not valid JSON; fix it or move it aside",
                    cfg_path.display()
                ))
            }
        };
        let mut cfg = Config::from_json(&raw);
        // An older version kept the owner token, in the clear, in config.json (and it was also
        // the owner cookie). It is dropped here: it no longer opens anything, and `serve` prints a
        // new one-time owner link.
        let legacy = cfg
            .raw
            .as_object_mut()
            .and_then(|o| o.remove("owner_token"));
        let mut s = Store {
            dir: dir.to_path_buf(),
            cfg,
            rooms: vec![],
            members: vec![],
            invites: vec![],
            owner_sessions: vec![],
        };
        if legacy.is_some() {
            s.save_config()?;
        }
        s.owner_sessions = read_json(&dir.join("owner-sessions.json"))
            .and_then(|v| v["sessions"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| {
                Some(OwnerSession {
                    hash: v["hash"].as_str()?.to_string(),
                    created: v["created"].as_f64().unwrap_or(0.0),
                    expires: v["expires"].as_f64().unwrap_or(0.0),
                })
            })
            .collect();
        for r in read_json(&dir.join("rooms.json"))
            .and_then(|v| v["rooms"].as_array().cloned())
            .unwrap_or_default()
        {
            let Some(id) = r["id"].as_str().filter(|i| valid_room_id(i)) else {
                continue;
            };
            let (tail, last) = load_tail(&msg_path(dir, id), TAIL);
            s.rooms.push(Room {
                id: id.to_string(),
                name: r["name"].as_str().unwrap_or(id).to_string(),
                mode: Mode::parse(r["mode"].as_str().unwrap_or("")).unwrap_or(Mode::Mention),
                personality: Personality::parse(r["personality"].as_str().unwrap_or(""))
                    .unwrap_or(Personality::Warm),
                created: r["created"].as_f64().unwrap_or(0.0),
                conv: r["conv"].as_str().map(str::to_string),
                next: last + 1,
                tail,
            });
        }
        let now = unix_now();
        let mut old_shape = false;
        s.members = read_json(&dir.join("members.json"))
            .and_then(|v| v["members"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| Member::from_json(v, now))
            .map(|(m, old)| {
                old_shape |= old;
                m
            })
            .collect();
        if old_shape {
            // a members file from before devices: write the new shape now
            s.save_members()?;
        }
        s.invites = read_json(&dir.join("invites.json"))
            .and_then(|v| v["invites"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| {
                let member = v["member"].as_str().map(str::to_string);
                let room = v["room"].as_str().unwrap_or("").to_string();
                if room.is_empty() && member.is_none() {
                    return None;
                }
                Some(Invite {
                    hash: v["hash"].as_str()?.to_string(),
                    room,
                    member,
                    created: v["created"].as_f64().unwrap_or(0.0),
                    expires: v["expires"].as_f64().unwrap_or(0.0),
                    used: v["used"].as_f64(),
                })
            })
            .collect();
        Ok(s)
    }

    pub fn save_config(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.cfg.raw).map_err(|e| e.to_string())?;
        write_atomic(&self.dir.join("config.json"), text.as_bytes())
    }

    fn save_owner_sessions(&self) -> Result<(), String> {
        let v = json!({"sessions": self.owner_sessions.iter().map(|o| json!({"hash": o.hash, "created": o.created, "expires": o.expires})).collect::<Vec<_>>()});
        write_atomic(
            &self.dir.join("owner-sessions.json"),
            v.to_string().as_bytes(),
        )
    }

    // ---- the owner: a one-time link, then a session

    fn link_live(&self, now: f64) -> bool {
        self.cfg.raw["owner_link_hash"].as_str().is_some()
            && self.cfg.raw["owner_link_expires"]
                .as_f64()
                .is_some_and(|e| e > now)
    }

    /// A new one-time owner link token (valid for a week; older links stop working). Only its
    /// hash is kept.
    pub fn new_owner_link(&mut self) -> Result<String, String> {
        let t = new_token()?;
        register_secret(&t);
        self.cfg.raw["owner_link_hash"] = json!(sha256_hex(&t));
        self.cfg.raw["owner_link_expires"] = json!(unix_now() + OWNER_LINK_SECS);
        self.save_config()?;
        Ok(t)
    }

    /// On `serve`: if nobody can sign in as the owner (no live link, no live session), make a
    /// link. Returns it when it did (to print once).
    pub fn ensure_owner_access(&mut self) -> Result<Option<String>, String> {
        let now = unix_now();
        if self.link_live(now) || self.owner_sessions.iter().any(|o| o.expires > now) {
            return Ok(None);
        }
        self.new_owner_link().map(Some)
    }

    /// Trade the owner link for a session: the link is used up, the session token is returned
    /// (the browser's cookie; only its hash is kept). Wrong, used or expired links are refused.
    pub fn redeem_owner_link(&mut self, token: &str, now: f64) -> Result<String, String> {
        let stored = self.cfg.raw["owner_link_hash"].as_str().unwrap_or("");
        let live = self.cfg.raw["owner_link_expires"]
            .as_f64()
            .is_some_and(|e| e > now);
        // compare even when nothing is stored, so the time does not tell
        let same = ct_eq(
            &sha256_hex(token),
            if stored.is_empty() { "-" } else { stored },
        );
        if !same || !live || token.len() < 32 {
            return Err("That owner link is not right, or was used already. Run `tempo-room owner-link` on this machine for a new one.".into());
        }
        if let Some(o) = self.cfg.raw.as_object_mut() {
            o.remove("owner_link_hash");
            o.remove("owner_link_expires");
        }
        self.save_config()?;
        self.new_owner_session(now)
    }

    /// A signed-in owner browser (30 days).
    pub fn new_owner_session(&mut self, now: f64) -> Result<String, String> {
        let t = new_token()?;
        self.owner_sessions.retain(|o| o.expires > now);
        self.owner_sessions.push(OwnerSession {
            hash: sha256_hex(&t),
            created: now,
            expires: now + OWNER_SESSION_SECS,
        });
        while self.owner_sessions.len() > MAX_OWNER_SESSIONS {
            self.owner_sessions.remove(0);
        }
        self.save_owner_sessions()?;
        Ok(t)
    }

    pub fn owner_session_ok(&self, token: &str, now: f64) -> bool {
        if token.len() < 32 {
            return false;
        }
        let h = sha256_hex(token);
        self.owner_sessions
            .iter()
            .any(|o| ct_eq(&o.hash, &h) && o.expires > now)
    }

    /// Sign out for good: the session no longer works, even with the cookie still in hand.
    pub fn end_owner_session(&mut self, token: &str) -> bool {
        let h = sha256_hex(token);
        let before = self.owner_sessions.len();
        self.owner_sessions.retain(|o| !ct_eq(&o.hash, &h));
        let gone = self.owner_sessions.len() != before;
        if gone {
            let _ = self.save_owner_sessions();
        }
        gone
    }

    pub fn save_rooms(&self) -> Result<(), String> {
        let v = json!({"rooms": self.rooms.iter().map(Room::to_json).collect::<Vec<_>>()});
        write_atomic(&self.dir.join("rooms.json"), v.to_string().as_bytes())
    }

    pub fn save_members(&self) -> Result<(), String> {
        let v = json!({"members": self.members.iter().map(Member::to_json).collect::<Vec<_>>()});
        write_atomic(&self.dir.join("members.json"), v.to_string().as_bytes())
    }

    pub fn save_invites(&self) -> Result<(), String> {
        let v = json!({"invites": self.invites.iter().map(|i| json!({"hash": i.hash, "room": i.room, "member": i.member, "created": i.created, "expires": i.expires, "used": i.used})).collect::<Vec<_>>()});
        write_atomic(&self.dir.join("invites.json"), v.to_string().as_bytes())
    }

    pub fn room(&self, id: &str) -> Option<&Room> {
        self.rooms.iter().find(|r| r.id == id)
    }

    pub fn room_mut(&mut self, id: &str) -> Option<&mut Room> {
        self.rooms.iter_mut().find(|r| r.id == id)
    }

    /// A room id from a name or id the person typed (`#Family`, `family`, `The Family`).
    pub fn find_room(&self, q: &str) -> Option<&Room> {
        let q = q.trim().trim_start_matches('#');
        self.room(q)
            .or_else(|| self.room(&slug(q)))
            .or_else(|| self.rooms.iter().find(|r| r.name.eq_ignore_ascii_case(q)))
    }

    pub fn create_room(
        &mut self,
        name: &str,
        mode: Mode,
        personality: Personality,
    ) -> Result<String, String> {
        let name = name.trim();
        let count = name.chars().count();
        if count == 0 || count > 60 || name.chars().any(char::is_control) {
            return Err("A room name needs 1 to 60 characters.".into());
        }
        let base = slug(name);
        let mut id = base.clone();
        let mut n = 2;
        while self.room(&id).is_some() {
            id = format!("{}-{n}", crate::util::head_chars(&base, 36));
            n += 1;
        }
        self.rooms.push(Room {
            id: id.clone(),
            name: name.to_string(),
            mode,
            personality,
            created: unix_now(),
            conv: None,
            next: 1,
            tail: VecDeque::new(),
        });
        self.save_rooms()?;
        Ok(id)
    }

    /// Store a message: its id and time are assigned here, its text must already be redacted.
    pub fn append(&mut self, mut m: Msg) -> Result<Msg, String> {
        let dir = self.dir.clone();
        let room = self
            .room_mut(&m.room)
            .ok_or_else(|| format!("no room {}", m.room))?;
        m.id = room.next;
        if m.t == 0.0 {
            m.t = unix_now();
        }
        crate::util::append_line(&msg_path(&dir, &m.room), &m.to_json().to_string())?;
        room.next += 1;
        room.tail.push_back(m.clone());
        if room.tail.len() > TAIL {
            room.tail.pop_front();
        }
        Ok(m)
    }

    /// Oldest first. `since`: newer than that id. `before`: older than that id (read from the
    /// file when the memory tail does not reach back far enough). Neither: the newest.
    pub fn messages(
        &self,
        room: &str,
        since: Option<u64>,
        before: Option<u64>,
        limit: usize,
    ) -> Vec<Msg> {
        let Some(r) = self.room(room) else {
            return vec![];
        };
        let limit = limit.clamp(1, 500);
        if let Some(s) = since {
            return r
                .tail
                .iter()
                .filter(|m| m.id > s)
                .take(limit)
                .cloned()
                .collect();
        }
        let older: Vec<&Msg> = r
            .tail
            .iter()
            .filter(|m| before.is_none_or(|b| m.id < b))
            .collect();
        let tail_reaches_start = r.tail.front().is_none_or(|m| m.id <= 1);
        if older.len() >= limit || tail_reaches_start || before.is_none() {
            return older[older.len().saturating_sub(limit)..]
                .iter()
                .map(|m| (*m).clone())
                .collect();
        }
        let b = before.unwrap_or(u64::MAX);
        let mut ring: VecDeque<Msg> = VecDeque::new();
        if let Ok(text) = std::fs::read_to_string(msg_path(&self.dir, room)) {
            for l in text.lines() {
                if let Some(m) = serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| Msg::from_json(&v))
                    .filter(|m| m.id < b)
                {
                    ring.push_back(m);
                    if ring.len() > limit {
                        ring.pop_front();
                    }
                }
            }
        }
        ring.into_iter().collect()
    }

    pub fn find_msg(&self, room: &str, id: u64) -> Option<Msg> {
        self.room(room)?.tail.iter().find(|m| m.id == id).cloned()
    }

    // ---- invites

    pub fn new_invite(&mut self, room: &str, hours: Option<f64>) -> Result<(String, f64), String> {
        if self.room(room).is_none() {
            return Err(format!("no room {room}"));
        }
        self.push_invite(room, None, hours, unix_now())
    }

    /// A one-time sign-in link and code for an existing member's new device (same lifetime and
    /// code format as an invite). It never asks for a name and never touches the member's rooms.
    pub fn new_device_invite(
        &mut self,
        member: &str,
        hours: Option<f64>,
        now: f64,
    ) -> Result<(String, f64), String> {
        if self.member(member).is_none() {
            return Err(format!("no member {member}"));
        }
        self.push_invite("", Some(member.to_string()), hours, now)
    }

    fn push_invite(
        &mut self,
        room: &str,
        member: Option<String>,
        hours: Option<f64>,
        now: f64,
    ) -> Result<(String, f64), String> {
        let code = new_invite_code()?;
        let expires = now
            + hours
                .unwrap_or(self.cfg.invite_hours)
                .clamp(0.01, 24.0 * 30.0)
                * 3600.0;
        // Keep the file small: invites used or expired more than a week ago are dropped.
        self.invites
            .retain(|i| i.used.map_or(i.expires, |u| u.max(i.expires)) > now - 7.0 * 86_400.0);
        self.invites.push(Invite {
            hash: sha256_hex(&normalize_code(&code)),
            room: room.to_string(),
            member,
            created: now,
            expires,
            used: None,
        });
        self.save_invites()?;
        Ok((code, expires))
    }

    /// The invite this code opens (unused and unexpired), without using it up.
    pub fn check_invite(&self, code: &str, now: f64) -> Result<usize, InviteError> {
        let h = sha256_hex(&normalize_code(code));
        let i = self
            .invites
            .iter()
            .position(|i| ct_eq(&i.hash, &h))
            .ok_or(InviteError::NotFound)?;
        let inv = &self.invites[i];
        // a sign-in link for someone who was removed is as good as unknown
        if inv
            .member
            .as_deref()
            .is_some_and(|m| self.member(m).is_none())
        {
            return Err(InviteError::NotFound);
        }
        if inv.used.is_some() {
            return Err(InviteError::Used);
        }
        if inv.expires <= now {
            return Err(InviteError::Expired);
        }
        Ok(i)
    }

    // ---- members

    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    pub fn member_mut(&mut self, id: &str) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.id == id)
    }

    /// A session that has been used within `session_days` still works.
    fn session_live(&self, s: &Session, now: f64) -> bool {
        now - s.last_seen < self.cfg.session_days * 86_400.0
    }

    /// The member this session cookie belongs to, if the session is live at `now`.
    pub fn member_by_token(&self, token: &str, now: f64) -> Option<&Member> {
        if token.len() < 32 {
            return None;
        }
        let h = sha256_hex(token);
        let mut found = None;
        // look at every session (no early exit) so the time does not tell which one matched
        for m in &self.members {
            for s in &m.sessions {
                if ct_eq(&s.hash, &h) && self.session_live(s, now) {
                    found = Some(m);
                }
            }
        }
        found
    }

    /// Like `member_by_token`, and records that the session was used: `last_seen` moves on only
    /// when the stored value is at least `SEEN_WRITE_SECS` old, so the file is rewritten at most
    /// once an hour per session. Returns the member's id.
    pub fn touch_session(&mut self, token: &str, now: f64) -> Option<String> {
        let id = self.member_by_token(token, now)?.id.clone();
        let h = sha256_hex(token);
        let mut stale = false;
        if let Some(s) = self
            .member_mut(&id)
            .and_then(|m| m.sessions.iter_mut().find(|s| ct_eq(&s.hash, &h)))
        {
            if now - s.last_seen >= SEEN_WRITE_SECS {
                s.last_seen = now;
                stale = true;
            }
        }
        if stale && self.save_members().is_err() {
            crate::util::log("could not save when a device was last used");
        }
        Some(id)
    }

    /// Sign out one device for good: its session is removed, the member and the other devices
    /// stay. True when a session was found for this cookie value.
    pub fn end_member_session(&mut self, token: &str) -> bool {
        if token.len() < 32 {
            return false;
        }
        let h = sha256_hex(token);
        let mut gone = false;
        for m in &mut self.members {
            let before = m.sessions.len();
            m.sessions.retain(|s| !ct_eq(&s.hash, &h));
            gone |= m.sessions.len() != before;
        }
        if gone && self.save_members().is_err() {
            crate::util::log("could not save a sign-out");
        }
        gone
    }

    /// Add a device to `member`: drops sessions idle past `session_days`, keeps at most
    /// `MAX_MEMBER_SESSIONS` (least recently used go first) and returns the new cookie value.
    fn add_session(&mut self, member: &str, now: f64) -> Result<String, String> {
        let token = new_token()?;
        let days = self.cfg.session_days * 86_400.0;
        let m = self
            .member_mut(member)
            .ok_or_else(|| format!("no member {member}"))?;
        m.sessions.retain(|s| now - s.last_seen < days);
        m.sessions.push(Session {
            hash: sha256_hex(&token),
            created: now,
            last_seen: now,
        });
        while m.sessions.len() > MAX_MEMBER_SESSIONS {
            let oldest = m
                .sessions
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.last_seen.total_cmp(&b.1.last_seen))
                .map(|(i, _)| i)
                .unwrap_or(0);
            m.sessions.remove(oldest);
        }
        Ok(token)
    }

    /// Use the sign-in link at `idx`: a new session for the member it was made for (never anyone
    /// else), the link is used up. Returns the member id and the new cookie value.
    pub fn redeem_device(&mut self, idx: usize, now: f64) -> Result<(String, String), String> {
        let id = self
            .invites
            .get(idx)
            .and_then(|i| i.member.clone())
            .ok_or("that is not a sign-in link")?;
        if self.member(&id).is_none() {
            return Err("That sign-in link no longer works. Ask for a new one.".into());
        }
        let token = self.add_session(&id, now)?;
        self.invites[idx].used = Some(now);
        self.save_invites()?;
        self.save_members()?;
        Ok((id, token))
    }

    /// Someone in `room` (a member, or the owner) already goes by this name.
    pub fn name_taken(&self, room: &str, name: &str, except: Option<&str>) -> bool {
        let n = name_key(name);
        self.reserved_name(name)
            || self.members.iter().any(|m| {
                Some(m.id.as_str()) != except
                    && m.rooms.iter().any(|r| r == room)
                    && name_key(&m.name) == n
            })
    }

    /// Names that would pass for Tempo, the room itself or the owner (look-alikes included).
    pub fn reserved_name(&self, name: &str) -> bool {
        let n = name_key(name);
        n.is_empty()
            || ["tempo", "perennostempo", "system", "owner", "room"].contains(&n.as_str())
            || name_key(&self.cfg.owner_name) == n
    }

    /// Use the invite at `idx`: an existing member gains the room; otherwise a new member is made.
    /// Returns the member id and, for a new member, the session token (shown to nobody else).
    pub fn redeem(
        &mut self,
        idx: usize,
        name: &str,
        existing: Option<&str>,
    ) -> Result<(String, Option<String>), String> {
        if self.invites[idx].member.is_some() {
            return Err("that is a sign-in link, not a room invite".into());
        }
        let room = self.invites[idx].room.clone();
        if self.room(&room).is_none() {
            return Err("that room no longer exists".into());
        }
        let now = unix_now();
        let out = match existing
            .and_then(|id| self.member(id))
            .map(|m| m.id.clone())
        {
            Some(id) => {
                let taken = {
                    let m = self.member(&id).map(|m| m.name.clone()).unwrap_or_default();
                    self.name_taken(&room, &m, Some(&id))
                };
                if taken {
                    return Err("Someone in that room already has your name.".into());
                }
                if let Some(m) = self.member_mut(&id) {
                    if !m.rooms.contains(&room) {
                        m.rooms.push(room.clone());
                    }
                }
                (id, None)
            }
            None => {
                let name = valid_name(name)?;
                if self.name_taken(&room, &name, None) {
                    return Err(format!("Someone in this room is already called {name}."));
                }
                let token = new_token()?;
                let mut id;
                loop {
                    id = format!("m_{}", crate::util::hex(&random_bytes(4)?));
                    if self.member(&id).is_none() {
                        break;
                    }
                }
                self.members.push(Member {
                    id: id.clone(),
                    name,
                    rooms: vec![room.clone()],
                    muted: vec![],
                    sessions: vec![Session {
                        hash: sha256_hex(&token),
                        created: now,
                        last_seen: now,
                    }],
                    joined: now,
                });
                (id, Some(token))
            }
        };
        self.invites[idx].used = Some(now);
        self.save_invites()?;
        self.save_members()?;
        Ok(out)
    }

    pub fn set_muted(&mut self, member: &str, room: &str, on: bool) -> Result<String, String> {
        let m = self
            .member_mut(member)
            .ok_or_else(|| format!("no member {member}"))?;
        m.muted.retain(|r| r != room);
        if on {
            m.muted.push(room.to_string());
        }
        let name = m.name.clone();
        self.save_members()?;
        Ok(name)
    }

    /// Take a member out (every room); their record goes to `trash/`, their session ends.
    pub fn remove_member(&mut self, member: &str) -> Result<(String, PathBuf), String> {
        let i = self
            .members
            .iter()
            .position(|m| m.id == member)
            .ok_or_else(|| format!("no member {member}"))?;
        let rec = self.members[i].to_json();
        let to = to_trash(&self.dir, &format!("member-{member}"), &rec.to_string())?;
        let m = self.members.remove(i);
        // their pending sign-in links go with them
        let before = self.invites.len();
        self.invites
            .retain(|inv| inv.member.as_deref() != Some(member));
        if self.invites.len() != before {
            self.save_invites()?;
        }
        self.save_members()?;
        Ok((m.name, to))
    }

    /// The member called `name` in `room` (case-insensitive, a leading @ ignored).
    pub fn member_named(&self, room: &str, name: &str) -> Option<&Member> {
        let n = name.trim().trim_start_matches('@').to_lowercase();
        self.members
            .iter()
            .find(|m| m.rooms.iter().any(|r| r == room) && m.name.to_lowercase() == n)
    }

    pub fn set_conv(&mut self, room: &str, conv: Option<String>) -> Result<(), String> {
        if let Some(r) = self.room_mut(room) {
            r.conv = conv;
        }
        self.save_rooms()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::util::testdir::tmpdir;

    pub fn fresh(tag: &str) -> Store {
        Store::open(&tmpdir(tag)).unwrap()
    }

    #[test]
    fn owner_link_is_made_once() {
        let d = tmpdir("owner");
        let mut s = Store::open(&d).unwrap();
        let link = s
            .ensure_owner_access()
            .unwrap()
            .expect("first start makes a link");
        assert_eq!(link.len(), 64);
        let mut again = Store::open(&d).unwrap();
        assert!(again.ensure_owner_access().unwrap().is_none());
        assert_eq!(again.cfg.agent, DEFAULT_AGENT);
        assert_eq!(again.cfg.owner_name, "Owner");
        std::fs::write(d.join("config.json"), "{broken").unwrap();
        assert!(Store::open(&d).err().unwrap().contains("not valid JSON"));
    }

    #[test]
    fn rooms_and_messages_survive_a_restart() {
        let d = tmpdir("rooms");
        let mut s = Store::open(&d).unwrap();
        let id = s
            .create_room("The Family", Mode::Mention, Personality::Warm)
            .unwrap();
        assert_eq!(id, "the-family");
        assert_eq!(
            s.create_room("The Family", Mode::Always, Personality::Plain)
                .unwrap(),
            "the-family-2"
        );
        assert!(s
            .create_room("  ", Mode::Mention, Personality::Warm)
            .is_err());
        for i in 0..5 {
            let m = s
                .append(Msg::new(
                    &id,
                    "owner",
                    "Riley",
                    Kind::User,
                    "app",
                    &format!("m{i}"),
                ))
                .unwrap();
            assert_eq!(m.id, i + 1);
        }
        let mut r = Msg::new(&id, "tempo", "Tempo", Kind::Tempo, "app", "reply");
        r.reply_to = Some(2);
        r.reply_name = Some("Riley".into());
        r.reply_text = Some("m1".into());
        s.append(r).unwrap();
        s.set_conv(&id, Some("conv-1".into())).unwrap();
        let s2 = Store::open(&d).unwrap();
        let room = s2.room(&id).unwrap();
        assert_eq!((room.next, room.tail.len()), (7, 6));
        assert_eq!(room.conv.as_deref(), Some("conv-1"));
        assert_eq!(room.tail[5].reply_to, Some(2));
        assert_eq!(room.tail[5].kind, Kind::Tempo);
        let newest = s2.messages(&id, None, None, 2);
        assert_eq!(newest.iter().map(|m| m.id).collect::<Vec<_>>(), [5, 6]);
        let since = s2.messages(&id, Some(4), None, 50);
        assert_eq!(since.iter().map(|m| m.id).collect::<Vec<_>>(), [5, 6]);
        let before = s2.messages(&id, None, Some(3), 50);
        assert_eq!(before.iter().map(|m| m.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(s2.find_room("#The Family").unwrap().id, "the-family");
        assert_eq!(s2.find_room("THE-FAMILY-2").unwrap().id, "the-family-2");
    }

    #[test]
    fn paging_reads_the_file_beyond_the_memory_tail() {
        let d = tmpdir("paging");
        let mut s = Store::open(&d).unwrap();
        let id = s
            .create_room("Big", Mode::Mention, Personality::Plain)
            .unwrap();
        let mut lines = String::new();
        for i in 1..=(TAIL as u64 + 30) {
            let mut m = Msg::new(&id, "owner", "Riley", Kind::User, "app", &format!("n{i}"));
            m.id = i;
            m.t = i as f64;
            lines.push_str(&m.to_json().to_string());
            lines.push('\n');
        }
        lines.push_str("{\"id\": 99999, \"broken"); // a torn last line is skipped
        std::fs::write(d.join("messages").join(format!("{id}.jsonl")), lines).unwrap();
        let s = Store::open(&d).unwrap();
        let r = s.room(&id).unwrap();
        assert_eq!(r.tail.len(), TAIL);
        assert_eq!(r.next, TAIL as u64 + 31);
        let old = s.messages(&id, None, Some(5), 10);
        assert_eq!(old.iter().map(|m| m.id).collect::<Vec<_>>(), [1, 2, 3, 4]);
    }

    #[test]
    fn invites_are_one_time_and_expire() {
        let d = tmpdir("invites");
        let mut s = Store::open(&d).unwrap();
        let room = s
            .create_room("Team", Mode::Mention, Personality::Plain)
            .unwrap();
        let (code, exp) = s.new_invite(&room, Some(1.0)).unwrap();
        let now = unix_now();
        assert!(exp > now + 3500.0 && exp < now + 3700.0);
        let file = std::fs::read_to_string(d.join("invites.json")).unwrap();
        assert!(
            !file.contains(&code) && !file.contains(&normalize_code(&code)),
            "codes are kept only as hashes"
        );
        assert_eq!(
            s.check_invite("ZZZZZ-ZZZZZ", now),
            Err(InviteError::NotFound)
        );
        assert_eq!(s.check_invite(&code, exp + 1.0), Err(InviteError::Expired));
        let idx = s
            .check_invite(&code.to_lowercase().replace('-', " "), now)
            .unwrap();
        let (mid, token) = s.redeem(idx, "  Mia  ", None).unwrap();
        let token = token.unwrap();
        assert_eq!(s.member(&mid).unwrap().name, "Mia");
        assert_eq!(s.check_invite(&code, now), Err(InviteError::Used));
        assert_eq!(s.member_by_token(&token, now).unwrap().id, mid);
        assert!(s.member_by_token("not-a-token", now).is_none());
        let members = std::fs::read_to_string(d.join("members.json")).unwrap();
        assert!(
            !members.contains(&token),
            "session tokens are kept only as hashes"
        );
        // A second invite: the name must be free in the room; an existing member keeps theirs.
        let (code2, _) = s.new_invite(&room, None).unwrap();
        let idx2 = s.check_invite(&code2, now).unwrap();
        assert!(s
            .redeem(idx2, "mia", None)
            .unwrap_err()
            .contains("already called"));
        assert!(s.redeem(idx2, "Tempo", None).is_err());
        assert!(s.redeem(idx2, "<b>", None).is_err());
        let other = s
            .create_room("Family", Mode::Mention, Personality::Warm)
            .unwrap();
        let (code3, _) = s.new_invite(&other, None).unwrap();
        let idx3 = s.check_invite(&code3, now).unwrap();
        let (same, no_token) = s.redeem(idx3, "ignored", Some(&mid)).unwrap();
        assert_eq!((same.as_str(), no_token), (mid.as_str(), None));
        assert_eq!(s.member(&mid).unwrap().rooms, [room.clone(), other.clone()]);
        // Reload keeps everything.
        let s2 = Store::open(&d).unwrap();
        assert_eq!(s2.member_by_token(&token, now).unwrap().rooms.len(), 2);
        assert_eq!(s2.check_invite(&code, now), Err(InviteError::Used));
    }

    #[test]
    fn mute_and_remove_go_through_trash() {
        let d = tmpdir("remove");
        let mut s = Store::open(&d).unwrap();
        let room = s
            .create_room("Team", Mode::Mention, Personality::Plain)
            .unwrap();
        let (code, _) = s.new_invite(&room, None).unwrap();
        let idx = s.check_invite(&code, unix_now()).unwrap();
        let (mid, token) = s.redeem(idx, "Sam", None).unwrap();
        assert_eq!(s.member_named(&room, "@sam").unwrap().id, mid);
        assert_eq!(s.set_muted(&mid, &room, true).unwrap(), "Sam");
        assert_eq!(s.member(&mid).unwrap().muted, std::slice::from_ref(&room));
        s.set_muted(&mid, &room, false).unwrap();
        assert!(s.member(&mid).unwrap().muted.is_empty());
        let (name, to) = s.remove_member(&mid).unwrap();
        assert_eq!(name, "Sam");
        assert!(std::fs::read_to_string(&to).unwrap().contains(&mid));
        assert!(s.member_by_token(&token.unwrap(), unix_now()).is_none());
        assert!(Store::open(&d).unwrap().member(&mid).is_none());
    }

    #[test]
    fn config_values() {
        let c = Config::from_json(
            &json!({"birthday": "03-14", "owner_name": "Riley", "reply_cap": 5, "limits": {"post_per_min": 3}}),
        );
        assert_eq!(c.birthday_md().as_deref(), Some("03-14"));
        assert_eq!(c.owner_name, "Riley");
        assert_eq!(c.reply_cap, 200, "clamped");
        assert_eq!(c.limits.post_per_min, 3);
        assert_eq!(c.limits.ask_per_member, 5);
        assert_eq!(
            Config::from_json(&json!({"birthday": "1990-03-14"}))
                .birthday_md()
                .as_deref(),
            Some("03-14")
        );
        assert_eq!(
            Config::from_json(&json!({"birthday": "14/03"})).birthday_md(),
            None
        );
        assert_eq!(Config::from_json(&json!({})).birthday_md(), None);
    }

    #[test]
    fn an_old_owner_token_is_dropped_when_the_folder_is_opened() {
        // Older versions kept `owner_token` in config.json in the clear, and it was also the
        // owner cookie. Opening drops it (it opens nothing now); `serve` then makes a link.
        let d = tmpdir("legacy-owner");
        let old = "ab".repeat(32);
        std::fs::write(
            d.join("config.json"),
            json!({"owner_token": old, "owner_name": "Riley", "agent": "http://127.0.0.1:1"})
                .to_string(),
        )
        .unwrap();
        let mut s = Store::open(&d).unwrap();
        let cfg = std::fs::read_to_string(d.join("config.json")).unwrap();
        assert!(!cfg.contains(&old) && !cfg.contains("owner_token"), "{cfg}");
        assert!(
            cfg.contains("Riley") && cfg.contains("127.0.0.1:1"),
            "other settings survive: {cfg}"
        );
        assert!(
            s.redeem_owner_link(&old, unix_now()).is_err(),
            "the old token opens nothing"
        );
        assert!(!s.owner_session_ok(&old, unix_now()));
        let link = s
            .ensure_owner_access()
            .unwrap()
            .expect("a new link for the owner");
        assert_ne!(link, old);
        assert!(s.redeem_owner_link(&link, unix_now()).is_ok());
    }

    #[test]
    fn owner_links_expire_and_sessions_are_capped() {
        let d = tmpdir("owner-expiry");
        let mut s = Store::open(&d).unwrap();
        let link = s.new_owner_link().unwrap();
        let week = OWNER_LINK_SECS;
        assert!(
            s.redeem_owner_link(&link, unix_now() + week + 60.0)
                .is_err(),
            "a week old"
        );
        assert!(s.redeem_owner_link("short", unix_now()).is_err());
        assert!(s.redeem_owner_link(&link, unix_now()).is_ok());
        let mut first = String::new();
        for i in 0..(MAX_OWNER_SESSIONS + 5) {
            let t = s.new_owner_session(unix_now()).unwrap();
            if i == 0 {
                first = t;
            }
        }
        assert!(s.owner_sessions.len() <= MAX_OWNER_SESSIONS);
        assert!(
            !s.owner_session_ok(&first, unix_now()),
            "the oldest were dropped"
        );
        // With every session expired and no link, `serve` makes a way in again.
        let mut s2 = Store::open(&tmpdir("owner-expiry2")).unwrap();
        let t = s2
            .new_owner_session(unix_now() - OWNER_SESSION_SECS - 5.0)
            .unwrap();
        assert!(!s2.owner_session_ok(&t, unix_now()));
        assert!(s2.ensure_owner_access().unwrap().is_some());
        assert!(s2.ensure_owner_access().unwrap().is_none(), "once");
    }

    #[cfg(unix)]
    #[test]
    fn data_files_are_readable_only_by_their_owner() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let d = tmpdir("perms").join("room-data");
        let mut s = Store::open(&d).unwrap();
        assert_eq!(mode(&d), 0o700, "a new data folder is private");
        s.ensure_owner_access().unwrap();
        s.new_owner_session(unix_now()).unwrap();
        s.create_room("Family", Mode::Mention, Personality::Warm)
            .unwrap();
        s.new_invite("family", None).unwrap();
        s.save_config().unwrap(); // a second write: the .prev copy
        s.append(Msg::new(
            "family",
            "owner",
            "Riley",
            Kind::User,
            "app",
            "hi",
        ))
        .unwrap();
        crate::store::new_control_key(&d).unwrap();
        s.remove_member("none").err();
        for f in [
            "config.json",
            "config.json.prev",
            "rooms.json",
            "invites.json",
            "owner-sessions.json",
            "control.key",
            "messages/family.jsonl",
        ] {
            assert_eq!(mode(&d.join(f)), 0o600, "{f}");
        }
        // Files an older version wrote with the default mode are tightened on open.
        std::fs::set_permissions(
            d.join("config.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::set_permissions(
            d.join("config.json.prev"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        Store::open(&d).unwrap();
        assert_eq!(mode(&d.join("config.json")), 0o600);
        assert_eq!(mode(&d.join("config.json.prev")), 0o600);
    }

    const DAY: f64 = 86_400.0;

    /// A store with #family and a member (Mia) with one device. Returns the cookie value too.
    fn with_mia(tag: &str) -> (Store, PathBuf, String, String) {
        let d = tmpdir(tag);
        let mut s = Store::open(&d).unwrap();
        let room = s
            .create_room("Family", Mode::Mention, Personality::Warm)
            .unwrap();
        let (code, _) = s.new_invite(&room, None).unwrap();
        let idx = s.check_invite(&code, unix_now()).unwrap();
        let (mid, token) = s.redeem(idx, "Mia", None).unwrap();
        (s, d, mid, token.unwrap())
    }

    #[test]
    fn a_members_file_from_before_devices_still_loads_and_is_rewritten() {
        let d = tmpdir("migrate-members");
        let old_token = "cd".repeat(32);
        let old = json!({"members": [
            {"id": "m_0a0a0a0a", "name": "Mia", "rooms": ["family"], "muted": ["team"],
             "token_hash": sha256_hex(&old_token), "joined": 1_700_000_000.0},
            {"id": "m_0b0b0b0b", "name": "Sam", "rooms": ["family"], "muted": [],
             "token_hash": sha256_hex("another"), "joined": 1_700_000_100.0},
        ]});
        std::fs::write(d.join("members.json"), old.to_string()).unwrap();
        let s = Store::open(&d).unwrap();
        let now = unix_now();
        let mia = s
            .member_by_token(&old_token, now)
            .expect("the old cookie works");
        assert_eq!(mia.id, "m_0a0a0a0a");
        assert_eq!(
            (mia.rooms.clone(), mia.muted.clone()),
            (vec!["family".to_string()], vec!["team".to_string()])
        );
        assert_eq!(mia.sessions.len(), 1);
        assert_eq!(mia.sessions[0].hash, sha256_hex(&old_token));
        assert_eq!(mia.sessions[0].created, 1_700_000_000.0);
        assert!(
            (mia.sessions[0].last_seen - now).abs() < 60.0,
            "the upgrade does not sign anyone out"
        );
        // The new shape was written; the single hash is now a list.
        let text = std::fs::read_to_string(d.join("members.json")).unwrap();
        assert!(!text.contains("token_hash"), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["members"][0]["sessions"][0]["hash"],
            sha256_hex(&old_token)
        );
        assert!(!text.contains(&old_token), "only hashes are stored");
        // Opening the new shape again changes nothing.
        let again = Store::open(&d).unwrap();
        assert_eq!(again.members[0].sessions, s.members[0].sessions);
        assert_eq!(again.members.len(), 2);
    }

    #[test]
    fn a_member_has_one_session_per_device() {
        let (mut s, d, mid, one) = with_mia("devices");
        let now = unix_now();
        let (code, _) = s.new_device_invite(&mid, None, now).unwrap();
        let idx = s.check_invite(&code, now).unwrap();
        let (who, two) = s.redeem_device(idx, now).unwrap();
        assert_eq!(who, mid);
        assert_ne!(one, two);
        assert_eq!(s.member(&mid).unwrap().sessions.len(), 2);
        assert_eq!(s.member_by_token(&one, now).unwrap().id, mid);
        assert_eq!(s.member_by_token(&two, now).unwrap().id, mid);
        // signing out one device leaves the other and the member
        assert!(s.end_member_session(&one));
        assert!(!s.end_member_session(&one), "already gone");
        assert!(!s.end_member_session("short"));
        assert!(s.member_by_token(&one, now).is_none());
        assert_eq!(s.member_by_token(&two, now).unwrap().id, mid);
        assert_eq!(s.member(&mid).unwrap().rooms, ["family"]);
        let reread = Store::open(&d).unwrap();
        assert!(reread.member_by_token(&one, now).is_none());
        assert!(reread.member_by_token(&two, now).is_some());
        let file = std::fs::read_to_string(d.join("members.json")).unwrap();
        assert!(!file.contains(&one) && !file.contains(&two), "only hashes");
        // removing the member ends every session
        s.remove_member(&mid).unwrap();
        assert!(s.member_by_token(&two, now).is_none());
    }

    #[test]
    fn a_session_unused_for_session_days_stops_working() {
        let (mut s, _d, mid, token) = with_mia("expiry");
        assert_eq!(s.cfg.session_days, 180.0, "the default");
        let t0 = s.member(&mid).unwrap().sessions[0].last_seen;
        assert!(s.member_by_token(&token, t0 + 179.0 * DAY).is_some());
        assert!(s.member_by_token(&token, t0 + 181.0 * DAY).is_none());
        // use moves the clock on: 100 days, then 100 more is fine, 200 days of silence is not
        assert!(s.touch_session(&token, t0 + 100.0 * DAY).is_some());
        assert!(s.member_by_token(&token, t0 + 200.0 * DAY).is_some());
        assert!(s.touch_session(&token, t0 + 400.0 * DAY).is_none());
        assert!(
            s.member_by_token(&token, t0 + 400.0 * DAY).is_none(),
            "an expired session is not revived by trying it"
        );
        // the setting is `session_days`
        s.cfg.session_days = 7.0;
        assert!(s.member_by_token(&token, t0 + 200.0 * DAY).is_none());
        assert_eq!(
            Config::from_json(&json!({"session_days": 30})).session_days,
            30.0
        );
        assert_eq!(
            Config::from_json(&json!({"session_days": 0})).session_days,
            1.0
        );
        assert_eq!(
            Config::from_json(&json!({"session_days": "x"})).session_days,
            180.0
        );
        // idle sessions are dropped when the member gets a new device
        let later = t0 + 200.0 * DAY;
        let (code, _) = s.new_device_invite(&mid, None, later).unwrap();
        let idx = s.check_invite(&code, later).unwrap();
        let (_, fresh) = s.redeem_device(idx, later).unwrap();
        assert_eq!(s.member(&mid).unwrap().sessions.len(), 1);
        assert!(s.member_by_token(&fresh, later).is_some());
    }

    #[test]
    fn last_seen_is_written_at_most_once_an_hour() {
        let (mut s, d, mid, token) = with_mia("throttle");
        let file = d.join("members.json");
        let t0 = s.member(&mid).unwrap().sessions[0].last_seen;
        let seen = |s: &Store| s.member(&mid).unwrap().sessions[0].last_seen;
        // a write that would show: delete the file; a throttled touch must not bring it back
        std::fs::remove_file(&file).unwrap();
        for minute in [1.0, 5.0, 30.0, 59.0] {
            assert!(s.touch_session(&token, t0 + minute * 60.0).is_some());
            assert_eq!(seen(&s), t0, "no change at {minute} minutes");
            assert!(!file.exists(), "no write at {minute} minutes");
        }
        // an hour on: one write
        assert!(s.touch_session(&token, t0 + 3600.0).is_some());
        assert_eq!(seen(&s), t0 + 3600.0);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["members"][0]["sessions"][0]["last_seen"], t0 + 3600.0);
        // and quiet again for the next hour
        std::fs::remove_file(&file).unwrap();
        assert!(s.touch_session(&token, t0 + 3600.0 + 1800.0).is_some());
        assert!(!file.exists());
        assert!(s.touch_session(&token, t0 + 7200.0).is_some());
        assert!(file.exists());
        // a wrong cookie writes nothing
        std::fs::remove_file(&file).unwrap();
        assert!(s.touch_session(&"0".repeat(64), t0 + 20_000.0).is_none());
        assert!(!file.exists());
    }

    #[test]
    fn a_sign_in_link_is_one_time_expires_and_is_only_for_its_member() {
        let (mut s, d, mia, mia_token) = with_mia("device-link");
        let now = unix_now();
        // a second member, in a second room
        let team = s
            .create_room("Team", Mode::Mention, Personality::Plain)
            .unwrap();
        let (c, _) = s.new_invite(&team, None).unwrap();
        let i = s.check_invite(&c, now).unwrap();
        let (sam, sam_token) = s.redeem(i, "Sam", None).unwrap();
        let sam_token = sam_token.unwrap();
        assert!(s.new_device_invite("m_nobody", None, now).is_err());
        let rooms_before = (
            s.member(&mia).unwrap().rooms.clone(),
            s.member(&sam).unwrap().rooms.clone(),
        );

        let (code, exp) = s.new_device_invite(&mia, Some(1.0), now).unwrap();
        assert_eq!(code.len(), 11);
        assert_eq!(
            code.as_bytes()[5],
            b'-',
            "same format as an invite: XXXXX-XXXXX"
        );
        assert_eq!(exp, now + 3600.0, "expires like an invite");
        let default_exp = s.new_device_invite(&mia, None, now).unwrap().1;
        assert_eq!(default_exp, now + 48.0 * 3600.0, "invite_hours by default");
        let file = std::fs::read_to_string(d.join("invites.json")).unwrap();
        assert!(
            !file.contains(&code) && !file.contains(&normalize_code(&code)),
            "stored as a hash"
        );
        // expired, then fine, then used
        assert_eq!(s.check_invite(&code, exp + 1.0), Err(InviteError::Expired));
        let idx = s.check_invite(&code.to_lowercase(), now).unwrap();
        assert!(s.redeem(idx, "Eve", None).is_err(), "not a room invite");
        let (who, token) = s.redeem_device(idx, now).unwrap();
        assert_eq!(who, mia, "the link signs in Mia, whoever holds it");
        assert_ne!(who, sam);
        assert_eq!(s.member_by_token(&token, now).unwrap().id, mia);
        assert_ne!(s.member_by_token(&sam_token, now).unwrap().id, mia);
        assert_eq!(s.member_by_token(&mia_token, now).unwrap().id, mia);
        assert_eq!(s.check_invite(&code, now), Err(InviteError::Used));
        // no name asked, no member made, no room changed
        assert_eq!(s.members.len(), 2);
        assert_eq!(s.member(&sam).unwrap().sessions.len(), 1);
        assert_eq!(
            (
                s.member(&mia).unwrap().rooms.clone(),
                s.member(&sam).unwrap().rooms.clone()
            ),
            rooms_before
        );
        // a room invite is not a sign-in link: it has no member and still works as before
        let (rc, _) = s.new_invite(&team, None).unwrap();
        let ri = s.check_invite(&rc, now).unwrap();
        assert!(s.redeem_device(ri, now).is_err());
        let (same, none) = s.redeem(ri, "ignored", Some(&mia)).unwrap();
        assert_eq!((same, none), (mia.clone(), None));
        assert_eq!(s.member(&mia).unwrap().rooms.len(), 2);
        // reload keeps the link's member and its used mark
        let s2 = Store::open(&d).unwrap();
        assert_eq!(s2.check_invite(&code, now), Err(InviteError::Used));
        assert_eq!(s2.member(&mia).unwrap().sessions.len(), 2);
        // a removed member's pending link fails
        let (pending, _) = s.new_device_invite(&sam, None, now).unwrap();
        assert!(s.check_invite(&pending, now).is_ok());
        s.remove_member(&sam).unwrap();
        assert_eq!(s.check_invite(&pending, now), Err(InviteError::NotFound));
        assert!(s
            .invites
            .iter()
            .all(|i| i.member.as_deref() != Some(sam.as_str())));
        assert!(s.new_device_invite(&sam, None, now).is_err());
        assert!(s.member_by_token(&sam_token, now).is_none());
    }

    #[test]
    fn a_member_keeps_at_most_twenty_devices_the_idlest_go_first() {
        let (mut s, _d, mid, first) = with_mia("device-cap");
        let t0 = unix_now();
        let mut last = String::new();
        for i in 1..=MAX_MEMBER_SESSIONS {
            let now = t0 + i as f64 * 10.0;
            let (code, _) = s.new_device_invite(&mid, None, now).unwrap();
            let idx = s.check_invite(&code, now).unwrap();
            last = s.redeem_device(idx, now).unwrap().1;
        }
        assert_eq!(s.member(&mid).unwrap().sessions.len(), MAX_MEMBER_SESSIONS);
        let now = t0 + 1000.0;
        assert!(s.member_by_token(&first, now).is_none(), "the idlest went");
        assert!(s.member_by_token(&last, now).is_some());
    }

    #[test]
    fn names_cannot_pass_for_tempo_the_room_or_the_owner() {
        let mut s = fresh("names");
        s.cfg.owner_name = "Riley".into();
        for bad in [
            "[Perennos Tempo room #x] Riley", // a fake header
            "Mia: yes",
            "Mia\u{200b}", // zero width space
            "Mia\u{202e}", // right-to-left override
            "\u{feff}Mia",
            "...",
        ] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(valid_name("  Mia   R  ").unwrap(), "Mia R");
        for lookalike in [
            "Tempo",
            "tempo",
            "T.e.m.p.o",
            "Tempo!",
            "Тempo", // Cyrillic Te
            "Tеmpo", // Cyrillic ie
            "Temp0",
            "Perennos Tempo",
            "perennostempo",
            "System",
            "Room",
            "Owner",
            "RILEY",
            "R i l e y",
            "Rilеy", // Cyrillic ie
        ] {
            assert!(s.name_taken("family", lookalike, None), "{lookalike}");
        }
        assert!(!s.name_taken("family", "Mia", None));
        assert!(!s.name_taken("family", "Rileys", None));
    }
}
