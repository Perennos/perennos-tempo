//! Stage over the Fleet: a take played on another computer, through the messenger.
//!
//! ComfyUI has no password, so it stays on 127.0.0.1 on every computer and nothing here ever
//! opens it to the network. The controller sends the inputs as blob streams (sealed, signed,
//! SHA-256 checked, see `blob.rs`), then one `stage` message that names the workflow, the Pedals
//! and the seed. The other computer's messenger plays it against its own local ComfyUI, under its
//! own GPU lease, and only when its owner allowed `stage` (`tempo-launch fleet allow stage`) and
//! the workflow is added and on there. What came out goes back the same way, and then a
//! `stageres` message says what each file is. The other computer never takes a path from the
//! message: a file input is the id of a stream it has already received and checked.
//!
//! The play itself is behind `StageRunner`, so this file holds only the wire.

use super::blob::{self, Sent};
use super::msg::{lock, Handler, Node};
use super::store::{self, Paths};
use crate::setup::json::{parse, J};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a take needs, as the other computer plays it.
pub enum PlayIn {
    Text(String),
    /// A file already in place on this computer (a received and checked stream).
    File(PathBuf),
}

pub struct Play {
    pub workflow: String,
    pub inputs: Vec<(String, PlayIn)>,
    pub pedals: Vec<(String, String)>,
    pub seed: Option<i64>,
    /// This computer's name in the fleet, for what it writes in the proof beside the file.
    pub label: String,
}

pub struct Played {
    /// (file, role): `output` or `sidecar`, in the order made.
    pub files: Vec<(PathBuf, String)>,
    pub secs: f64,
    pub cached: usize,
    pub notes: Vec<String>,
}

pub trait StageRunner: Send + Sync {
    /// Whether this computer has the workflow added, ready and on.
    fn ready(&self, workflow: &str) -> bool;
    fn play(&self, p: &Play) -> Result<Played, String>;
}

/// One at a time: a graphics card plays one workflow at once.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

pub struct StageHandler {
    paths: Paths,
    runner: Arc<dyn StageRunner>,
}

impl StageHandler {
    pub fn new(paths: Paths, runner: Arc<dyn StageRunner>) -> StageHandler {
        StageHandler { paths, runner }
    }
}

impl Handler for StageHandler {
    fn on(&self, node: &Arc<Node>, from: &str, topic: &str, _id: &str, body: &[u8]) {
        if topic != "stage" {
            return;
        }
        let Some(j) = std::str::from_utf8(body).ok().and_then(|t| parse(t).ok()) else {
            return;
        };
        let (node, runner, paths, from) = (
            node.clone(),
            self.runner.clone(),
            self.paths.clone(),
            from.to_string(),
        );
        std::thread::spawn(move || serve(&node, &*runner, &paths, &from, &j));
    }
}

fn log(p: &Paths, line: &str) {
    super::tools::log_call(p, line);
}

fn serve(node: &Arc<Node>, runner: &dyn StageRunner, p: &Paths, from: &str, j: &J) {
    let _busy = super::route::Busy::new();
    let reply = j.str("reply").unwrap_or("").to_string();
    let workflow = j.str("workflow").unwrap_or("").to_string();
    let fleet = store::load(p);
    let Some(m) = fleet.peers().find(|m| m.name == from).cloned() else {
        return;
    };
    let fail = |why: String| {
        log(p, &format!("stage for={from} workflow={workflow} ok=false"));
        let res = J::Obj(vec![
            ("call".into(), J::s(&reply)),
            ("ok".into(), J::Bool(false)),
            ("text".into(), J::s(&why)),
            ("node".into(), J::s(&node.me)),
        ]);
        let _ = node.send(&m, "stageres", res.compact().as_bytes());
    };
    if !blob::xfer_ok(&reply) || reply.contains('.') {
        return fail("the request had no call id".into());
    }
    if !super::tools::allowed(&fleet.allow, "stage") {
        return fail(format!("refused: {} doesn't offer Stage to the fleet. Its owner can allow it with `tempo-launch fleet allow stage` on that computer.", node.me));
    }
    if !runner.ready(&workflow) {
        return fail(format!("refused: {workflow} is not added and on at {}. There, `tempo-launch stage add {workflow}` and `stage enable {workflow}`.", node.me));
    }
    let started = std::time::Instant::now();
    let mut inputs: Vec<(String, PlayIn)> = vec![];
    for i in j.arr("inputs") {
        let Some(name) = i.str("name") else { continue };
        if let Some(t) = i.str("text") {
            inputs.push((name.to_string(), PlayIn::Text(t.to_string())));
        } else if let Some(x) = i.str("xfer") {
            let (size, sha) = (
                i.f64("size").unwrap_or(0.0) as u64,
                i.str("sha").unwrap_or(""),
            );
            // only a stream that came from this computer's peer and was checked
            match blob::wait_landed(node, x, size, sha, Duration::from_secs(120)) {
                Ok(l) => inputs.push((name.to_string(), PlayIn::File(l.path))),
                Err(e) => return fail(format!("the input \"{name}\" is not here: {e}")),
            }
        }
    }
    let pedals: Vec<(String, String)> = j
        .arr("pedals")
        .iter()
        .filter_map(|x| Some((x.str("name")?.to_string(), x.str("value")?.to_string())))
        .collect();
    let play = Play {
        workflow: workflow.clone(),
        inputs,
        pedals,
        seed: j.f64("seed").map(|s| s as i64),
        label: node.me.clone(),
    };
    let played = {
        let _one = lock(&ONE_AT_A_TIME);
        runner.play(&play)
    };
    // the copies of the inputs are not needed any more: the trash, as with everything
    let now = store::now_s();
    for (_, i) in &play.inputs {
        if let PlayIn::File(f) = i {
            let _ = crate::setup::runtime::to_trash(&p.home, f, now);
        }
    }
    let played = match played {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    let mut metas = vec![];
    let mut notes = played.notes.clone();
    for (k, (path, role)) in played.files.iter().enumerate() {
        let xfer = format!("{reply}.{k}");
        let name = path
            .file_name()
            .map_or("file".to_string(), |n| n.to_string_lossy().into_owned());
        match blob::send_file(node, &m, &xfer, path, &name, Duration::from_secs(900)) {
            Ok(s) => metas.push(file_j(&s, role)),
            Err(e) => notes.push(format!("a file could not be sent back: {e}")),
        }
    }
    let ok = metas.iter().any(|f| f.str("role") == Some("output"));
    log(
        p,
        &format!(
            "stage for={from} workflow={workflow} ok={ok} secs={:.0} files={}",
            started.elapsed().as_secs_f64(),
            metas.len()
        ),
    );
    let res = J::Obj(vec![
        ("call".into(), J::s(&reply)),
        ("ok".into(), J::Bool(ok)),
        (
            "text".into(),
            J::s(if ok {
                ""
            } else {
                "no output could be sent back"
            }),
        ),
        ("files".into(), J::Arr(metas)),
        ("secs".into(), J::n(played.secs)),
        ("cached".into(), J::n(played.cached as f64)),
        ("notes".into(), J::strings(&notes)),
        ("node".into(), J::s(&node.me)),
    ]);
    let _ = node.send(&m, "stageres", res.compact().as_bytes());
}

fn file_j(s: &Sent, role: &str) -> J {
    let mut j = s.to_j();
    j.set("role", J::s(role));
    j
}

// ---- the controller's side

/// An input as the controller sends it.
#[derive(Clone)]
pub enum Input {
    Text(String),
    /// Already sent with `send_inputs`.
    Sent(Sent),
}

pub struct Got {
    pub path: PathBuf,
    /// The name the other computer gave it.
    pub name: String,
    pub role: String,
}

pub struct Done {
    pub files: Vec<Got>,
    pub secs: f64,
    pub cached: usize,
    pub notes: Vec<String>,
    pub node: String,
}

fn machine(node: &Node, who: &str) -> Result<store::Machine, String> {
    store::load(&node.paths)
        .peers()
        .find(|m| m.name == who)
        .cloned()
        .ok_or_else(|| format!("{who} is not in the fleet, or is switched off"))
}

/// Send the input files of a run to `who`, once, so every take on that machine can use them.
pub fn send_inputs(
    node: &Node,
    who: &str,
    files: &[(String, PathBuf)],
) -> Result<Vec<(String, Sent)>, String> {
    let m = machine(node, who)?;
    let mut out = vec![];
    for (name, path) in files {
        let xfer = format!("in{}", super::crypto::hex(&super::crypto::random(6)));
        let file = path
            .file_name()
            .map_or("input".into(), |n| n.to_string_lossy().into_owned());
        let sent = blob::send_file(node, &m, &xfer, path, &file, Duration::from_secs(900))?;
        out.push((name.clone(), sent));
    }
    Ok(out)
}

/// Play one take on `who` and bring back what it made: each file in place on this computer
/// (`fleet/in`), already checked against its SHA-256. Waits up to `limit` for the take.
pub fn call(
    node: &Node,
    who: &str,
    workflow: &str,
    inputs: &[(String, Input)],
    pedals: &[(String, String)],
    seed: Option<i64>,
    limit: Duration,
) -> Result<Done, String> {
    let m = machine(node, who)?;
    let id = super::crypto::hex(&super::crypto::random(8));
    blob::expect(&node.paths, &id, who)?;
    let ins: Vec<J> = inputs
        .iter()
        .map(|(n, i)| match i {
            Input::Text(t) => J::Obj(vec![("name".into(), J::s(n)), ("text".into(), J::s(t))]),
            Input::Sent(s) => {
                let mut j = s.to_j();
                j.set("name", J::s(n));
                j
            }
        })
        .collect();
    let mut body = vec![
        ("reply".to_string(), J::s(&id)),
        ("workflow".into(), J::s(workflow)),
        ("inputs".into(), J::Arr(ins)),
        (
            "pedals".into(),
            J::Arr(
                pedals
                    .iter()
                    .map(|(k, v)| J::Obj(vec![("name".into(), J::s(k)), ("value".into(), J::s(v))]))
                    .collect(),
            ),
        ),
    ];
    if let Some(s) = seed {
        body.push(("seed".into(), J::n(s as f64)));
    }
    let body = J::Obj(body).compact();
    if body.len() > super::tools::MAX_ARGS {
        return Err("the request is too big for one message".into());
    }
    node.send(&m, "stage", body.as_bytes())?;
    let r = node
        .wait_for("stageres", "call", &id, limit)
        .ok_or_else(|| format!("{who} did not answer in time"))?;
    let j = parse(&String::from_utf8_lossy(&r.body)).map_err(|e| e.to_string())?;
    if j.bool("ok") != Some(true) {
        return Err(format!(
            "{who}: {}",
            j.str("text").unwrap_or("the take failed there")
        ));
    }
    let mut files = vec![];
    for f in j.arr("files") {
        let land = blob::wait_landed(
            node,
            f.str("xfer").unwrap_or(""),
            f.f64("size").unwrap_or(0.0) as u64,
            f.str("sha").unwrap_or(""),
            Duration::from_secs(600),
        )?;
        files.push(Got {
            path: land.path,
            name: land.name,
            role: f.str("role").unwrap_or("output").to_string(),
        });
    }
    Ok(Done {
        files,
        secs: j.f64("secs").unwrap_or(0.0),
        cached: j.f64("cached").unwrap_or(0.0) as usize,
        notes: j.strs("notes"),
        node: who.to_string(),
    })
}

/// Where a received file goes: never over one that is there.
pub fn place(from: &Path, dir: &Path, name: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let dest = super::tools::free_path(dir, name);
    std::fs::rename(from, &dest)
        .or_else(|_| std::fs::copy(from, &dest).map(|_| ()))
        .map_err(|e| format!("could not keep {}: {e}", dest.display()))?;
    Ok(dest)
}
