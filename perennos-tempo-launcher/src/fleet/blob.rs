//! Big files between computers: a blob stream.
//!
//! A file goes as 256 KiB pieces, each in its own sealed and signed frame (the same frames as
//! everything else the messenger sends), straight from the sender's file to the receiver's file.
//! Nothing is held in memory or in SQLite: the sender reads a piece and sends it, the receiver
//! writes it at its place in `<home>/fleet/blobs/<xfer>.part`. One `begin` says the name, the size,
//! and the SHA-256 the whole file must have; one `end` asks the receiver to check. The receiver
//! hashes the temp file, and only a file with the right size and hash is renamed into
//! `<home>/fleet/in/`. A wrong one goes to the trash and the sender is told why. Nothing is ever
//! put in place half done, and a piece that was changed on the way fails the frame's seal before
//! it gets here.
//!
//! One file is at most the cap in `fleet.json` (`blob_cap_gib`, 4 GiB unless the owner says
//! otherwise): the receiver turns away a bigger `begin` and a piece past the size it was promised.
//! A computer takes files only from a paired computer, and only when its owner allowed `stage`
//! (`tempo-launch fleet allow stage`) or when it is waiting for them (`expect`): the answer to a
//! call it made.
//!
//! The topics: `bs:<xfer>:begin`, `bs:<xfer>:<i>`, `bs:<xfer>:end` (taken by `raw`, never
//! stored) and, back to the sender, `bsdone` (`{xfer, ok, why}`). When a file is in place the
//! receiver also writes `blobin` to its own inbox, so another program on this computer (the
//! command that made the call) can find the file.

use super::msg::{lock, Handler, Node};
use super::store::{self, now_ms, Machine, Paths};
use crate::setup::json::{parse, J};
use crate::setup::sha256::{file_sha256, sha256_hex};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use super::tools::CHUNK;

/// Pieces sent in one write: 4 MiB.
const BATCH: usize = 16;
/// Files being taken at once.
const MAX_ACTIVE: usize = 4;
/// A transfer that has not moved for this long is given up on.
const STALE: Duration = Duration::from_secs(3600);
/// An `expect` file older than this is ignored.
const EXPECT_S: u64 = 6 * 3600;

pub fn blobs_dir(p: &Paths) -> PathBuf {
    p.home.join("fleet").join("blobs")
}

pub fn in_dir(p: &Paths) -> PathBuf {
    p.home.join("fleet").join("in")
}

fn expect_dir(p: &Paths) -> PathBuf {
    p.home.join("fleet").join("expect")
}

pub fn xfer_ok(x: &str) -> bool {
    !x.is_empty()
        && x.len() <= 64
        && x.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// The call an xfer belongs to: what comes before its first dot.
fn call_of(x: &str) -> &str {
    x.split('.').next().unwrap_or(x)
}

/// This computer is waiting for files from `from`, whose names start `<call>.`.
pub fn expect(p: &Paths, call: &str, from: &str) -> Result<(), String> {
    if !xfer_ok(call) || call.contains('.') {
        return Err("not a call id".into());
    }
    std::fs::create_dir_all(expect_dir(p)).map_err(|e| e.to_string())?;
    std::fs::write(expect_dir(p).join(call), from).map_err(|e| e.to_string())
}

fn expected(p: &Paths, x: &str, from: &str) -> bool {
    let f = expect_dir(p).join(call_of(x));
    let fresh = std::fs::metadata(&f)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|a| a.as_secs() < EXPECT_S);
    fresh && std::fs::read_to_string(&f).is_ok_and(|n| n == from)
}

struct Incoming {
    from: String,
    name: String,
    size: u64,
    sha: String,
    n: usize,
    got: Vec<bool>,
    count: usize,
    part: PathBuf,
    file: std::fs::File,
    /// Something was wrong with a piece: the end says no.
    bad: Option<String>,
    last: Instant,
}

/// Takes the files other computers send. One per messenger; the `Node` it is added to calls it.
pub struct BlobHandler {
    me: std::sync::Weak<BlobHandler>,
    paths: Paths,
    live: Mutex<HashMap<String, Incoming>>,
    /// xfer to (final path, name, size, sha) of those already put in place.
    done: Mutex<HashMap<String, (PathBuf, String, u64, String)>>,
}

impl BlobHandler {
    pub fn new(paths: Paths) -> Arc<BlobHandler> {
        Arc::new_cyclic(|me| BlobHandler {
            me: me.clone(),
            paths,
            live: Mutex::new(HashMap::new()),
            done: Mutex::new(HashMap::new()),
        })
    }

    fn tell(node: &Arc<Node>, to: &str, xfer: &str, ok: bool, why: &str) {
        let Some(m) = store::load(&node.paths)
            .machines
            .into_iter()
            .find(|m| m.name == to)
        else {
            return;
        };
        let body = J::Obj(vec![
            ("xfer".into(), J::s(xfer)),
            ("ok".into(), J::Bool(ok)),
            ("why".into(), J::s(why)),
        ])
        .compact();
        let _ = node.send(&m, "bsdone", body.as_bytes());
    }

    fn begin(&self, node: &Arc<Node>, from: &str, xfer: &str, body: &[u8]) {
        let refuse = |why: &str| Self::tell(node, from, xfer, false, why);
        let Some(j) = std::str::from_utf8(body).ok().and_then(|t| parse(t).ok()) else {
            return refuse("the start of the file made no sense");
        };
        let fleet = store::load(&self.paths);
        if !fleet.peers().any(|m| m.name == from) {
            return refuse("this computer does not know you");
        }
        if !(super::tools::allowed(&fleet.allow, "stage") || expected(&self.paths, xfer, from)) {
            return refuse("this computer is not taking files: its owner can allow it with `tempo-launch fleet allow stage`");
        }
        let (size, n) = (
            j.f64("size").unwrap_or(-1.0),
            j.f64("n").unwrap_or(-1.0) as i64,
        );
        let sha = j.str("sha").unwrap_or("").to_string();
        if size < 1.0 || size.fract() != 0.0 {
            return refuse("an empty file, or a size that makes no sense");
        }
        let size = size as u64;
        let cap = fleet.blob_cap();
        if size > cap {
            return refuse(&format!("the file is {size} bytes and this computer takes at most {cap} (`tempo-launch fleet cap GIB` changes it)"));
        }
        if n != size.div_ceil(CHUNK as u64) as i64
            || sha.len() != 64
            || !sha.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return refuse("the start of the file does not add up");
        }
        let name = super::tools::safe_name(j.str("name").unwrap_or("file"));
        let mut live = lock(&self.live);
        live.retain(|_, t| t.last.elapsed() < STALE);
        if let Some(t) = live.get_mut(xfer) {
            // the same start again (its answer was lost): carry on
            t.last = Instant::now();
            if t.from == from && t.size == size && t.sha == sha {
                return;
            }
            return refuse("that transfer is already running with other details");
        }
        if lock(&self.done).contains_key(xfer) {
            return refuse("that transfer is already finished");
        }
        if live.len() >= MAX_ACTIVE {
            return refuse("too many files are coming in at once; try again in a minute");
        }
        let dir = blobs_dir(&self.paths);
        let part = dir.join(format!("{xfer}.part"));
        let opened = std::fs::create_dir_all(&dir).and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)
        });
        match opened {
            Ok(file) => {
                live.insert(
                    xfer.to_string(),
                    Incoming {
                        from: from.to_string(),
                        name,
                        size,
                        sha,
                        n: n as usize,
                        got: vec![false; n as usize],
                        count: 0,
                        part,
                        file,
                        bad: None,
                        last: Instant::now(),
                    },
                );
            }
            Err(e) => refuse(&format!("could not start the file here ({e})")),
        }
    }

    fn piece(&self, from: &str, xfer: &str, i: usize, body: &[u8]) {
        let mut live = lock(&self.live);
        let Some(t) = live.get_mut(xfer) else { return };
        if t.from != from || t.bad.is_some() {
            return;
        }
        t.last = Instant::now();
        let want = if i + 1 == t.n {
            (t.size - (t.n as u64 - 1) * CHUNK as u64) as usize
        } else {
            CHUNK
        };
        if i >= t.n || body.len() != want {
            t.bad = Some(format!("piece {i} is not the size it should be"));
            return;
        }
        let r = t
            .file
            .seek(SeekFrom::Start(i as u64 * CHUNK as u64))
            .and_then(|_| t.file.write_all(body));
        match r {
            Ok(()) => {
                if !t.got[i] {
                    t.got[i] = true;
                    t.count += 1;
                }
            }
            Err(e) => t.bad = Some(format!("could not write piece {i} ({e})")),
        }
    }

    /// The end: check the file on a thread of its own (a big one takes seconds to hash), put it
    /// in place or send it to the trash, and say which.
    fn end(&self, node: &Arc<Node>, from: &str, xfer: &str) {
        let Some(me) = self.me.upgrade() else { return };
        if let Some(d) = lock(&self.done).get(xfer) {
            // asked twice: the answer was lost
            let _ = d;
            Self::tell(node, from, xfer, true, "");
            return;
        }
        let Some(t) = lock(&self.live).remove(xfer) else {
            return Self::tell(node, from, xfer, false, "no such transfer here");
        };
        if t.from != from {
            lock(&self.live).insert(xfer.to_string(), t);
            return;
        }
        let (node, xfer, from) = (node.clone(), xfer.to_string(), from.to_string());
        std::thread::spawn(move || {
            let r = me.finish(t, &xfer);
            let (ok, why) = match &r {
                Ok(rec) => {
                    lock(&me.done).insert(xfer.clone(), rec.clone());
                    let j = J::Obj(vec![
                        ("xfer".into(), J::s(&xfer)),
                        ("path".into(), J::s(&rec.0.to_string_lossy())),
                        ("name".into(), J::s(&rec.1)),
                        ("size".into(), J::n(rec.2 as f64)),
                        ("sha".into(), J::s(&rec.3)),
                        ("from".into(), J::s(&from)),
                    ]);
                    node.store(
                        &node.me,
                        &format!("blobin-{xfer}"),
                        "blobin",
                        now_ms(),
                        j.compact().as_bytes(),
                    );
                    (true, String::new())
                }
                Err(e) => (false, e.clone()),
            };
            Self::tell(&node, &from, &xfer, ok, &why);
        });
    }

    fn finish(&self, t: Incoming, xfer: &str) -> Result<(PathBuf, String, u64, String), String> {
        let Incoming {
            name,
            size,
            sha,
            n,
            count,
            part,
            file,
            bad,
            ..
        } = t;
        let _ = file.sync_all();
        drop(file);
        let reject = |why: String| -> String {
            let now = now_ms() / 1000;
            let _ = crate::setup::runtime::to_trash(&self.paths.home, &part, now);
            why
        };
        if let Some(b) = bad {
            return Err(reject(format!("{b}; the file was not kept")));
        }
        if count != n {
            return Err(reject(format!(
                "{} of {n} pieces arrived; the file was not kept",
                count
            )));
        }
        let got = file_sha256(&part, &mut |_| {}).map_err(|e| reject(e.to_string()))?;
        let len = std::fs::metadata(&part).map_or(0, |m| m.len());
        if len != size || got != sha {
            return Err(reject(
                "the file arrived damaged (its SHA-256 does not match), so it was not kept".into(),
            ));
        }
        let dir = in_dir(&self.paths);
        std::fs::create_dir_all(&dir).map_err(|e| reject(e.to_string()))?;
        let dest = super::tools::free_path(&dir, &format!("{xfer}-{name}"));
        std::fs::rename(&part, &dest)
            .map_err(|e| reject(format!("could not put the file in place ({e})")))?;
        Ok((dest, name, size, sha))
    }
}

impl Handler for BlobHandler {
    fn on(&self, _: &Arc<Node>, _: &str, _: &str, _: &str, _: &[u8]) {}

    fn raw(&self, node: &Arc<Node>, from: &str, topic: &str, body: &[u8]) -> bool {
        let Some(rest) = topic.strip_prefix("bs:") else {
            return false;
        };
        let Some((xfer, what)) = rest.rsplit_once(':') else {
            return true;
        };
        if !xfer_ok(xfer) {
            return true;
        }
        match what {
            "begin" => self.begin(node, from, xfer, body),
            "end" => self.end(node, from, xfer),
            i => {
                if let Ok(i) = i.parse::<usize>() {
                    self.piece(from, xfer, i, body);
                }
            }
        }
        true
    }
}

/// Give the node a blob handler (once per node: `fleet serve` does it).
pub fn attach(node: &Arc<Node>) -> Arc<BlobHandler> {
    let h = BlobHandler::new(node.paths.clone());
    lock(&node.handlers).push(h.clone());
    h
}

// ---- sending

/// What a sent file was.
#[derive(Clone, Debug, PartialEq)]
pub struct Sent {
    pub xfer: String,
    pub name: String,
    pub size: u64,
    pub sha: String,
}

impl Sent {
    pub fn to_j(&self) -> J {
        J::Obj(vec![
            ("xfer".into(), J::s(&self.xfer)),
            ("name".into(), J::s(&self.name)),
            ("size".into(), J::n(self.size as f64)),
            ("sha".into(), J::s(&self.sha)),
        ])
    }
}

fn refused(node: &Node, xfer: &str) -> Option<String> {
    node.inbox(Some("bsdone"), 50).into_iter().find_map(|m| {
        let j = parse(&String::from_utf8_lossy(&m.body)).ok()?;
        (j.str("xfer") == Some(xfer) && j.bool("ok") == Some(false))
            .then(|| j.str("why").unwrap_or("it was refused").to_string())
    })
}

fn direct(node: &Node, m: &Machine, items: &[(&str, &[u8])]) -> Result<(), String> {
    let mut last = String::new();
    for k in 0..4 {
        match node.send_direct(m, items) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = e;
                std::thread::sleep(Duration::from_millis(200 << k));
            }
        }
    }
    Err(last)
}

/// Send `path` to `m` as the stream `xfer`, and wait until `m` says it checked the file and put
/// it in place. Streams from the file: memory use is one batch.
pub fn send_file(
    node: &Node,
    m: &Machine,
    xfer: &str,
    path: &Path,
    name: &str,
    limit: Duration,
) -> Result<Sent, String> {
    if !xfer_ok(xfer) {
        return Err("not a transfer id".into());
    }
    let cap = store::load(&node.paths).blob_cap();
    let size = std::fs::metadata(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?
        .len();
    if size == 0 {
        return Err(format!("{} is empty", path.display()));
    }
    if size > cap {
        return Err(format!("{} is {size} bytes and a transfer is at most {cap}. `tempo-launch fleet cap GIB` changes it, here and on {}.", path.display(), m.name));
    }
    let sha = file_sha256(path, &mut |_| {}).map_err(|e| e.to_string())?;
    let n = size.div_ceil(CHUNK as u64) as usize;
    let sent = Sent {
        xfer: xfer.into(),
        name: super::tools::safe_name(name),
        size,
        sha,
    };
    let mut begin = sent.to_j();
    begin.set("n", J::n(n as f64));
    direct(
        node,
        m,
        &[(&format!("bs:{xfer}:begin"), begin.compact().as_bytes())],
    )?;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut i = 0usize;
    while i < n {
        let mut bufs: Vec<Vec<u8>> = Vec::new();
        for _ in 0..BATCH.min(n - i) {
            let mut b = vec![0u8; CHUNK];
            let mut have = 0;
            while have < CHUNK {
                match f.read(&mut b[have..]) {
                    Ok(0) => break,
                    Ok(k) => have += k,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(format!("could not read {}: {e}", path.display())),
                }
            }
            b.truncate(have);
            bufs.push(b);
        }
        // one frame per piece, each under its own topic, all in one write
        let topics: Vec<String> = (0..bufs.len())
            .map(|j| format!("bs:{xfer}:{}", i + j))
            .collect();
        let items: Vec<(&str, &[u8])> = topics
            .iter()
            .zip(&bufs)
            .map(|(t, b)| (t.as_str(), b.as_slice()))
            .collect();
        direct(node, m, &items).map_err(|e| format!("could not send to {} ({e})", m.name))?;
        i += bufs.len();
        if let Some(why) = refused(node, xfer) {
            return Err(format!("{} turned the file away: {why}", m.name));
        }
    }
    direct(node, m, &[(&format!("bs:{xfer}:end"), b"")])?;
    let r = node
        .wait_for("bsdone", "xfer", xfer, limit)
        .ok_or_else(|| format!("{} did not say it had the file in time", m.name))?;
    let j = parse(&String::from_utf8_lossy(&r.body)).map_err(|e| e.to_string())?;
    if j.bool("ok") == Some(true) {
        Ok(sent)
    } else {
        Err(format!(
            "{} could not keep the file: {}",
            m.name,
            j.str("why").unwrap_or("no reason given")
        ))
    }
}

/// A file that has arrived here: where it is, and what it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Landed {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub sha: String,
}

/// Wait for the file `xfer` to be in place on this computer (`blobin`, written by the program
/// that is listening), and check what it says against what the sender promised.
pub fn wait_landed(
    node: &Node,
    xfer: &str,
    size: u64,
    sha: &str,
    limit: Duration,
) -> Result<Landed, String> {
    let end = Instant::now() + limit;
    loop {
        if let Some(m) = node.wait_for("blobin", "xfer", xfer, Duration::from_millis(50)) {
            let j = parse(&String::from_utf8_lossy(&m.body)).map_err(|e| e.to_string())?;
            let l = Landed {
                path: PathBuf::from(j.str("path").unwrap_or("")),
                name: j.str("name").unwrap_or("file").to_string(),
                size: j.f64("size").unwrap_or(0.0) as u64,
                sha: j.str("sha").unwrap_or("").to_string(),
            };
            // what the sender listed must be what was checked
            return if l.size == size && l.sha == sha && l.path.is_file() {
                Ok(l)
            } else {
                Err(format!(
                    "{} is not the file that was promised, so it was not used",
                    l.name
                ))
            };
        }
        if let Some(why) = refused(node, xfer) {
            return Err(format!("a file was turned away: {why}"));
        }
        if Instant::now() > end {
            return Err("a file never arrived".into());
        }
    }
}

/// A whole-file SHA-256 of a small thing, for tests and checks.
pub fn sha_of(b: &[u8]) -> String {
    sha256_hex(b)
}
