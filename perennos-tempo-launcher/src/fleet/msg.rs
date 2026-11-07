//! The fleet messenger: signed frames over persistent connections, and a durable outbox and inbox.
//!
//! A frame is `u32 length | 32-byte HMAC-SHA256 | 64-byte Ed25519 signature | header line | body`.
//! The header is one line of compact JSON (kind, id, from, to, topic, time, nonce, and the
//! 96-bit seal nonce); the body is sealed with ChaCha20-Poly1305 and the header is its associated
//! data, so the routing fields stay readable and cannot be changed. The MAC covers header and body and uses
//! the fleet key; the signature is the sender's own identity, which the receiver pinned when the
//! two computers paired. A frame whose MAC is wrong, from a computer that is not pinned, signed by
//! another identity, whose clock is more than two minutes off, whose nonce was seen, or that is
//! addressed to someone else, closes the connection. One connection per peer is kept and
//! reused, and many frames go out in one write with their answers read after.
//!
//! Messages are written to the outbox (SQLite, WAL) before anything else; they are sent at once
//! when the peer is up and again, with backoff, when it is not. The receiver keeps each id once, so
//! a retry after a lost answer is never a second message. Heartbeats are the exception: a stale
//! one is worth nothing, so they are sent and forgotten.

use super::crypto::{aead_open, aead_seal, frame_keys, hex, hmac, random, same, unhex};
use super::store::{now_ms, now_s, Machine, Paths};
use crate::setup::json::{parse, J};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const SKEW_MS: u64 = 120_000;
/// The biggest frame body: a piece of a picture is 256 KiB, a note is far less.
pub const MAX_BODY: usize = 512 << 10;
const MAX_FRAME: usize = MAX_BODY + 4096;
const BATCH: usize = 64;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    /// `m` message, `a` answer, `p` ping, `o` pong.
    pub kind: char,
    pub id: String,
    pub from: String,
    pub to: String,
    pub topic: String,
    pub ts: u64,
    pub nonce: String,
    pub created: u64,
    /// Not kept anywhere: sent and forgotten (a heartbeat).
    pub eph: bool,
    pub body: Vec<u8>,
}

/// Frames that could not be opened (wrong seal, changed header or body): dropped, never parsed.
static UNOPENED: AtomicU64 = AtomicU64::new(0);

/// How many frames have been dropped for failing to open since this program started.
pub fn unopened() -> u64 {
    UNOPENED.load(Ordering::Relaxed)
}

pub fn encode(key: &[u8; 32], sk: &SigningKey, f: &Frame) -> Vec<u8> {
    build(key, sk, f, false)
}

/// The frame as bytes. The body is sealed under a fresh random 96-bit nonce that rides in the
/// header (`x`); the header line is the associated data, so who it is from and for, the time and
/// the id stay readable to routing but cannot be changed. `spoil` flips one bit of the sealed body
/// before the MAC and signature are made (tests only: a frame that is well formed but does not open).
pub(super) fn build(key: &[u8; 32], sk: &SigningKey, f: &Frame, spoil: bool) -> Vec<u8> {
    let (mac_key, seal_key) = frame_keys(key);
    let x: [u8; 12] = random(12).try_into().unwrap_or([0; 12]);
    let mut o = vec![
        ("k".to_string(), J::s(&f.kind.to_string())),
        ("id".into(), J::s(&f.id)),
        ("f".into(), J::s(&f.from)),
        ("t".into(), J::s(&f.to)),
        ("ts".into(), J::n(f.ts as f64)),
        ("n".into(), J::s(&f.nonce)),
        ("x".into(), J::s(&hex(&x))),
    ];
    if !f.topic.is_empty() {
        o.push(("tp".into(), J::s(&f.topic)));
    }
    if f.created != 0 {
        o.push(("c".into(), J::n(f.created as f64)));
    }
    if f.eph {
        o.push(("e".into(), J::Bool(true)));
    }
    let mut payload = J::Obj(o).compact().into_bytes();
    let mut sealed = aead_seal(&seal_key, &x, &payload, &f.body);
    if spoil {
        if let Some(b) = sealed.first_mut() {
            *b ^= 1;
        }
    }
    payload.push(b'\n');
    payload.extend_from_slice(&sealed);
    let mac = hmac(&mac_key, &[&payload]);
    let sig = sk.sign(&payload).to_bytes();
    let mut out = Vec::with_capacity(payload.len() + 100);
    out.extend_from_slice(&((payload.len() + 96) as u32).to_be_bytes());
    out.extend_from_slice(&mac);
    out.extend_from_slice(&sig);
    out.extend_from_slice(&payload);
    out
}

#[derive(Debug, PartialEq)]
pub enum Bad {
    /// The other side ended the connection between frames.
    Closed,
    /// Anything else: a wrong MAC, a broken frame, a read that failed.
    Refused(&'static str),
}

/// The pinned identity of a computer, by name.
pub type Pins<'a> = &'a dyn Fn(&str) -> Option<VerifyingKey>;

pub fn read_frame(r: &mut impl Read, key: &[u8; 32], pins: Pins) -> Result<Frame, Bad> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Bad::Closed),
        Err(_) => return Err(Bad::Refused("read failed")),
    }
    let n = u32::from_be_bytes(len) as usize;
    if !(98..=MAX_FRAME).contains(&n) {
        return Err(Bad::Refused("frame size"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)
        .map_err(|_| Bad::Refused("short frame"))?;
    let (mac, rest) = buf.split_at(32);
    let (sig, payload) = rest.split_at(64);
    let (mac_key, seal_key) = frame_keys(key);
    if !same(mac, &hmac(&mac_key, &[payload])) {
        return Err(Bad::Refused("wrong key"));
    }
    let nl = payload
        .iter()
        .position(|b| *b == b'\n')
        .ok_or(Bad::Refused("no header"))?;
    let h = std::str::from_utf8(&payload[..nl])
        .ok()
        .and_then(|t| parse(t).ok())
        .ok_or(Bad::Refused("header"))?;
    let s = |k: &str| h.str(k).unwrap_or("").to_string();
    let vk = pins(&s("f")).ok_or(Bad::Refused("not a pinned computer"))?;
    let sig = Signature::from_slice(sig).map_err(|_| Bad::Refused("signature"))?;
    vk.verify(payload, &sig)
        .map_err(|_| Bad::Refused("wrong identity"))?;
    // from here the header is trusted to be the sender's; the body is read only if it opens
    let opened = h
        .str("x")
        .and_then(unhex)
        .and_then(|x| <[u8; 12]>::try_from(x).ok())
        .and_then(|x| aead_open(&seal_key, &x, &payload[..nl], &payload[nl + 1..]));
    let Some(body) = opened else {
        UNOPENED.fetch_add(1, Ordering::Relaxed);
        return Err(Bad::Refused("did not open"));
    };
    Ok(Frame {
        kind: h.str("k").and_then(|k| k.chars().next()).unwrap_or('?'),
        id: s("id"),
        from: s("f"),
        to: s("t"),
        topic: s("tp"),
        ts: h.f64("ts").unwrap_or(0.0) as u64,
        nonce: s("n"),
        created: h.f64("c").unwrap_or(0.0) as u64,
        eph: h.bool("e").unwrap_or(false),
        body,
    })
}

/// Remembers the nonces of the last few minutes: a frame seen twice is a replay.
#[derive(Default)]
pub struct Guard {
    seen: HashMap<String, u64>,
}

impl Guard {
    pub fn check(&mut self, ts: u64, nonce: &str, now: u64) -> Result<(), &'static str> {
        if ts.abs_diff(now) > SKEW_MS {
            return Err("clock");
        }
        if nonce.is_empty() {
            return Err("no nonce");
        }
        if self.seen.len() > 4096 {
            self.seen.retain(|_, t| t.abs_diff(now) <= SKEW_MS);
        }
        if self.seen.insert(nonce.to_string(), ts).is_some() {
            return Err("replay");
        }
        Ok(())
    }
}

/// What a topic's receiver does with a message that is new. Heartbeats never reach it.
pub trait Handler: Send + Sync {
    fn on(&self, node: &Arc<Node>, from: &str, topic: &str, id: &str, body: &[u8]);
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inbound {
    pub id: String,
    pub peer: String,
    pub topic: String,
    pub body: Vec<u8>,
    pub created: u64,
}

struct Link {
    stream: Option<BufReader<TcpStream>>,
}

pub struct Node {
    pub paths: Paths,
    pub me: String,
    /// The fleet key, read again when `fleet.key` changes (a rotation), checked every two seconds.
    key: Mutex<KeyState>,
    sk: SigningKey,
    pins: Mutex<(Instant, HashMap<String, VerifyingKey>)>,
    db: Mutex<Connection>,
    guard: Mutex<Guard>,
    links: Mutex<HashMap<String, Arc<Mutex<Link>>>>,
    prefix: String,
    counter: AtomicU64,
    pub handlers: Mutex<Vec<Arc<dyn Handler>>>,
    /// Woken when something is waiting in the outbox, so a sender need not poll slowly.
    pub kick: (Mutex<bool>, Condvar),
    /// How long to wait for a connection to open.
    pub connect: Duration,
}

struct KeyState {
    key: [u8; 32],
    mtime: Option<std::time::SystemTime>,
    checked: Instant,
}

fn key_mtime(p: &Paths) -> Option<std::time::SystemTime> {
    std::fs::metadata(p.file("fleet.key"))
        .and_then(|m| m.modified())
        .ok()
}

/// The identities pinned in fleet.json, by machine name.
pub fn pins_of(p: &Paths) -> HashMap<String, VerifyingKey> {
    super::store::load(p)
        .machines
        .into_iter()
        .filter_map(|m| {
            let k = <[u8; 32]>::try_from(super::crypto::unhex(&m.id)?.as_slice()).ok()?;
            Some((m.name, VerifyingKey::from_bytes(&k).ok()?))
        })
        .collect()
}

pub fn backoff(tries: u32) -> u64 {
    (1u64 << tries.min(9)).min(300)
}

impl Node {
    pub fn open(paths: &Paths, key: [u8; 32]) -> Result<Arc<Node>, String> {
        let me = super::store::load(paths).me;
        let db = Connection::open(paths.file("fleet.db")).map_err(|e| format!("fleet.db: {e}"))?;
        db.busy_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("fleet.db: {e}"))?;
        db.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS outbox(id TEXT PRIMARY KEY, peer TEXT NOT NULL, topic TEXT NOT NULL,
                body BLOB NOT NULL, created INTEGER NOT NULL, tries INTEGER NOT NULL DEFAULT 0,
                next_try INTEGER NOT NULL DEFAULT 0, delivered INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS outbox_due ON outbox(delivered, peer, next_try);
             CREATE TABLE IF NOT EXISTS inbox(id TEXT PRIMARY KEY, peer TEXT NOT NULL, topic TEXT NOT NULL,
                body BLOB NOT NULL, created INTEGER NOT NULL, received INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS inbox_topic ON inbox(topic, received);",
        )
        .map_err(|e| format!("fleet.db: {e}"))?;
        let sk = super::pair::identity(paths)?;
        Ok(Arc::new(Node {
            paths: paths.clone(),
            me,
            key: Mutex::new(KeyState {
                key,
                mtime: key_mtime(paths),
                checked: Instant::now(),
            }),
            sk,
            pins: Mutex::new((Instant::now(), pins_of(paths))),
            db: Mutex::new(db),
            guard: Mutex::new(Guard::default()),
            links: Mutex::new(HashMap::new()),
            prefix: super::crypto::hex(&super::crypto::random(6)),
            counter: AtomicU64::new(1),
            handlers: Mutex::new(Vec::new()),
            kick: (Mutex::new(false), Condvar::new()),
            connect: Duration::from_secs(1),
        }))
    }

    pub fn key(&self) -> [u8; 32] {
        let mut g = lock(&self.key);
        if g.checked.elapsed() > Duration::from_secs(2) {
            g.checked = Instant::now();
            let now = key_mtime(&self.paths);
            if now != g.mtime {
                g.mtime = now;
                if let Some(k) = super::store::read_key(&self.paths) {
                    g.key = k;
                }
            }
        }
        g.key
    }

    /// This computer's identity as a pinned peer would hold it (hex).
    pub fn identity_hex(&self) -> String {
        super::crypto::hex(self.sk.verifying_key().as_bytes())
    }

    /// Who `name` is, as pinned in fleet.json (read again when the list is two seconds old or
    /// the name is not in it, so a computer paired a moment ago is known at once).
    fn pin(&self, name: &str) -> Option<VerifyingKey> {
        let mut g = lock(&self.pins);
        if g.0.elapsed() > Duration::from_secs(2) || !g.1.contains_key(name) {
            *g = (Instant::now(), pins_of(&self.paths));
        }
        g.1.get(name).copied()
    }

    fn encode(&self, f: &Frame) -> Vec<u8> {
        encode(&self.key(), &self.sk, f)
    }

    fn next_id(&self) -> String {
        format!(
            "{}{:x}",
            self.prefix,
            self.counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn frame(&self, kind: char, to: &str, topic: &str, id: &str, body: &[u8]) -> Frame {
        Frame {
            kind,
            id: id.into(),
            from: self.me.clone(),
            to: to.into(),
            topic: topic.into(),
            ts: now_ms(),
            nonce: self.next_id(),
            created: 0,
            eph: false,
            body: body.to_vec(),
        }
    }

    // ---- the outbox

    /// Write a message to the outbox. It is sent by `deliver` (at once, or later).
    pub fn enqueue(&self, peer: &str, topic: &str, body: &[u8]) -> Result<String, String> {
        if body.len() > MAX_BODY {
            return Err(format!("a message body is at most {MAX_BODY} bytes"));
        }
        let id = self.next_id();
        lock(&self.db)
            .execute(
                "INSERT INTO outbox(id,peer,topic,body,created) VALUES(?1,?2,?3,?4,?5)",
                params![id, peer, topic, body, now_ms() as i64],
            )
            .map_err(|e| e.to_string())?;
        *lock(&self.kick.0) = true;
        self.kick.1.notify_all();
        Ok(id)
    }

    pub fn queued(&self, peer: Option<&str>) -> usize {
        let n: i64 = lock(&self.db)
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE delivered=0 AND (?1 IS NULL OR peer=?1)",
                params![peer],
                |r| r.get(0),
            )
            .unwrap_or(0);
        n as usize
    }

    /// The oldest messages for `peer` that are due.
    fn due(&self, peer: &str) -> Vec<(String, String, Vec<u8>, u64)> {
        let db = lock(&self.db);
        let Ok(mut st) = db.prepare_cached(
            "SELECT id,topic,body,created FROM outbox WHERE delivered=0 AND peer=?1 AND next_try<=?2 ORDER BY created,rowid LIMIT ?3",
        ) else {
            return vec![];
        };
        st.query_map(params![peer, now_s() as i64, BATCH as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u64))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    fn mark(&self, ids: &[String]) {
        let mut db = lock(&self.db);
        let Ok(tx) = db.transaction() else { return };
        for id in ids {
            let _ = tx.execute("UPDATE outbox SET delivered=1 WHERE id=?1", params![id]);
        }
        let _ = tx.commit();
    }

    fn failed(&self, ids: &[String]) {
        let db = lock(&self.db);
        for id in ids {
            let tries: i64 = db
                .query_row("SELECT tries FROM outbox WHERE id=?1", params![id], |r| {
                    r.get(0)
                })
                .unwrap_or(0);
            let _ = db.execute(
                "UPDATE outbox SET tries=?2, next_try=?3 WHERE id=?1",
                params![id, tries + 1, (now_s() + backoff(tries as u32 + 1)) as i64],
            );
        }
    }

    /// Take back a message that was not delivered (a step handed to another machine instead).
    pub fn cancel_queued(&self, peer: &str, topic: &str, needle: &str) {
        let _ = lock(&self.db).execute(
            "DELETE FROM outbox WHERE delivered=0 AND peer=?1 AND topic=?2 AND instr(body, ?3)>0",
            params![peer, topic, needle.as_bytes()],
        );
    }

    pub fn tries(&self, id: &str) -> u32 {
        lock(&self.db)
            .query_row("SELECT tries FROM outbox WHERE id=?1", params![id], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0) as u32
    }

    /// Make a message due now (what a person asking for a retry means).
    pub fn retry_now(&self) {
        let _ = lock(&self.db).execute("UPDATE outbox SET next_try=0 WHERE delivered=0", []);
    }

    // ---- connections

    fn link(&self, peer: &str) -> Arc<Mutex<Link>> {
        lock(&self.links)
            .entry(peer.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Link { stream: None })))
            .clone()
    }

    fn connect(&self, m: &Machine) -> Result<BufReader<TcpStream>, String> {
        if !crate::allowed_remote_host(&m.addr) {
            return Err(format!("{} is not a private address", m.addr));
        }
        // the first private address only: a name that resolves to a public one is never used
        let addr: SocketAddr = (m.addr.as_str(), m.port)
            .to_socket_addrs()
            .map_err(|e| format!("{}: {e}", m.addr))?
            .find(|a| crate::private_ip(a.ip()))
            .ok_or_else(|| format!("{} has no private address", m.addr))?;
        let s = TcpStream::connect_timeout(&addr, self.connect)
            .map_err(|e| format!("{} is not answering ({e})", m.name))?;
        let _ = s.set_nodelay(true);
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
        Ok(BufReader::new(s))
    }

    /// Send `frames` on the peer's connection and read one frame back for each. Returns the ids
    /// answered, and why it stopped early, if it did. A connection that has gone stale (a
    /// laptop that slept) is replaced once, without counting a failed try.
    fn exchange(&self, m: &Machine, frames: &[Frame], want: char) -> (Vec<Frame>, Option<String>) {
        let link = self.link(&m.name);
        let mut link = lock(&link);
        let mut reused = link.stream.is_some();
        loop {
            let mut got = Vec::new();
            let r = (|| -> Result<(), String> {
                if link.stream.is_none() {
                    link.stream = Some(self.connect(m)?);
                }
                let s = link.stream.as_mut().ok_or("no connection")?;
                let mut wire = Vec::new();
                for f in &frames[got.len()..] {
                    wire.extend(self.encode(f));
                }
                s.get_mut().write_all(&wire).map_err(|e| e.to_string())?;
                while got.len() < frames.len() {
                    match read_frame(s, &self.key(), &|n| self.pin(n)) {
                        Ok(f) if f.kind == want => got.push(f),
                        Ok(_) => return Err("unexpected answer".into()),
                        Err(Bad::Closed) => return Err("the connection closed".into()),
                        Err(Bad::Refused(w)) => return Err(format!("refused: {w}")),
                    }
                }
                Ok(())
            })();
            match r {
                Ok(()) => return (got, None),
                Err(e) => {
                    link.stream = None;
                    if reused && got.is_empty() {
                        reused = false;
                        continue;
                    }
                    return (got, Some(e));
                }
            }
        }
    }

    /// Send what is due for `m`. Returns how many were delivered; an error says why the rest
    /// was not (and each is scheduled to be tried again later).
    pub fn deliver(&self, m: &Machine) -> Result<usize, String> {
        let rows = self.due(&m.name);
        if rows.is_empty() {
            return Ok(0);
        }
        let frames: Vec<Frame> = rows
            .iter()
            .map(|(id, topic, body, created)| Frame {
                created: *created,
                ..self.frame('m', &m.name, topic, id, body)
            })
            .collect();
        let (acks, err) = self.exchange(m, &frames, 'a');
        let done: Vec<String> = acks.into_iter().map(|a| a.id).collect();
        self.mark(&done);
        let left: Vec<String> = rows
            .iter()
            .map(|r| r.0.clone())
            .filter(|id| !done.contains(id))
            .collect();
        match err {
            None => Ok(done.len()),
            Some(e) => {
                self.failed(&left);
                Err(e)
            }
        }
    }

    /// Write to the outbox and try at once. `Ok(true)`: the peer has it. `Ok(false)`: it is
    /// kept and will be sent when the peer is up.
    pub fn send(&self, m: &Machine, topic: &str, body: &[u8]) -> Result<bool, String> {
        let id = self.enqueue(&m.name, topic, body)?;
        let _ = self.deliver(m);
        Ok(!self.is_pending(&id))
    }

    fn is_pending(&self, id: &str) -> bool {
        lock(&self.db)
            .query_row(
                "SELECT delivered FROM outbox WHERE id=?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .is_ok_and(|d| d == 0)
    }

    /// Send and forget (a heartbeat): nothing is written to the outbox.
    pub fn ephemeral(&self, m: &Machine, topic: &str, body: &[u8]) -> Result<(), String> {
        let mut f = self.frame('m', &m.name, topic, &self.next_id(), body);
        f.eph = true;
        match self.exchange(m, &[f], 'a') {
            (_, None) => Ok(()),
            (_, Some(e)) => Err(e),
        }
    }

    /// One round trip on the peer's connection, in microseconds.
    pub fn ping(&self, m: &Machine) -> Result<u64, String> {
        let f = self.frame('p', &m.name, "", &self.next_id(), b"");
        let t = Instant::now();
        match self.exchange(m, &[f], 'o') {
            (_, None) => Ok(t.elapsed().as_micros() as u64),
            (_, Some(e)) => Err(e),
        }
    }

    // ---- the inbox

    /// Keep a message once. Returns whether it was new.
    pub fn store(&self, from: &str, id: &str, topic: &str, created: u64, body: &[u8]) -> bool {
        lock(&self.db)
            .execute(
                "INSERT OR IGNORE INTO inbox(id,peer,topic,body,created,received) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, from, topic, body, created as i64, now_ms() as i64],
            )
            .map(|n| n == 1)
            .unwrap_or(false)
    }

    pub fn inbox(&self, topic: Option<&str>, limit: usize) -> Vec<Inbound> {
        let db = lock(&self.db);
        let Ok(mut st) = db.prepare(
            "SELECT id,peer,topic,body,created FROM inbox WHERE (?1 IS NULL OR topic=?1) ORDER BY received DESC, rowid DESC LIMIT ?2",
        ) else {
            return vec![];
        };
        st.query_map(params![topic, limit as i64], |r| {
            Ok(Inbound {
                id: r.get(0)?,
                peer: r.get(1)?,
                topic: r.get(2)?,
                body: r.get(3)?,
                created: r.get::<_, i64>(4)? as u64,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Drop the messages of one topic (the pieces of a file once it is put together).
    pub fn forget(&self, topic: &str) {
        let _ = lock(&self.db).execute("DELETE FROM inbox WHERE topic=?1", params![topic]);
    }

    pub fn get(&self, id: &str) -> Option<Inbound> {
        lock(&self.db)
            .query_row(
                "SELECT id,peer,topic,body,created FROM inbox WHERE id=?1",
                params![id],
                |r| {
                    Ok(Inbound {
                        id: r.get(0)?,
                        peer: r.get(1)?,
                        topic: r.get(2)?,
                        body: r.get(3)?,
                        created: r.get::<_, i64>(4)? as u64,
                    })
                },
            )
            .ok()
    }

    /// Wait for a message of `topic` whose JSON body has `field` equal to `value`.
    pub fn wait_for(
        &self,
        topic: &str,
        field: &str,
        value: &str,
        limit: Duration,
    ) -> Option<Inbound> {
        let end = Instant::now() + limit;
        loop {
            let hit = self.inbox(Some(topic), 200).into_iter().find(|m| {
                std::str::from_utf8(&m.body)
                    .ok()
                    .and_then(|t| parse(t).ok())
                    .is_some_and(|j| j.str(field) == Some(value))
            });
            if hit.is_some() || Instant::now() >= end {
                return hit;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    // ---- receiving

    fn receive(self: &Arc<Self>, f: &Frame) {
        if f.topic == "heartbeat" {
            super::serve::on_heartbeat(self, &f.from, &f.body);
            return;
        }
        if self.store(&f.from, &f.id, &f.topic, f.created, &f.body) {
            let hs: Vec<Arc<dyn Handler>> = lock(&self.handlers).clone();
            for h in hs {
                h.on(self, &f.from, &f.topic, &f.id, &f.body);
            }
        }
    }

    /// Serve frames on one accepted connection until it ends. Anything wrong ends it at once:
    /// a peer without the key is dropped without an answer.
    pub fn frames(self: &Arc<Self>, mut r: BufReader<TcpStream>) {
        let Ok(mut w) = r.get_ref().try_clone() else {
            return;
        };
        let mut out: Vec<u8> = Vec::new();
        loop {
            let f = match read_frame(&mut r, &self.key(), &|n| self.pin(n)) {
                Ok(f) => f,
                Err(_) => return,
            };
            if f.to != self.me || lock(&self.guard).check(f.ts, &f.nonce, now_ms()).is_err() {
                return;
            }
            match f.kind {
                'p' => out.extend(self.encode(&self.frame('o', &f.from, "", &f.id, b""))),
                'm' => {
                    self.receive(&f);
                    out.extend(self.encode(&self.frame('a', &f.from, "", &f.id, b"")));
                }
                _ => return,
            }
            // answers go out when no more frames wait in the buffer: a batch gets one write
            if r.buffer().is_empty() && !out.is_empty() {
                if w.write_all(&out).is_err() {
                    return;
                }
                out.clear();
            }
        }
    }
}
