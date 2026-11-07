//! Tests that run a whole command: setting a band up, putting it in a fake Room, running a flow
//! through a fake agent, the dashboard, proposals, and the rule that nothing leaves the computer.

use super::*;
use crate::setup::json::{parse, J};
use crate::Cmd;
use net::{Agent, RoomLink};
use schedule::{Entry, Scheduler};
use std::cell::RefCell;
use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const NOW: u64 = 1_793_000_000;

pub fn tmp(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "tempo-band-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A Tempo home with a Vault folder (made when `vault` is true).
pub fn ctx_in(root: &Path, vault: bool) -> Ctx {
    let ctx = Ctx {
        home: root.join("home"),
        agent_data: root.join("home").join("agent"),
        vault: root.join("Tempo Vault"),
        bin_dirs: vec![root.join("bin")],
        browser: false,
        opened: RefCell::new(Vec::new()),
    };
    std::fs::create_dir_all(&ctx.home).unwrap();
    if vault {
        std::fs::create_dir_all(&ctx.vault).unwrap();
    }
    ctx
}

fn models() -> Models {
    Models {
        heavy: "qwen3:8b".into(),
        fast: "qwen3:4b".into(),
    }
}

// ---- fakes

#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> J {
        parse(&self.body).unwrap_or(J::Null)
    }
}

type Responder = Box<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

/// A little HTTP server on 127.0.0.1 that records what it is asked and answers from a closure.
pub struct Fake {
    pub port: u16,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub fn start(respond: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let respond: Responder = Box::new(respond);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, len) = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (None, 0);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (Some(i + 4), len);
                    }
                };
                let Some(he) = head_end else { continue };
                while buf.len() < he + len {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..he - 4]).into_owned();
                let mut lines = head.split("\r\n");
                let first = lines.next().unwrap_or("");
                let mut parts = first.split_whitespace();
                let seen_one = Seen {
                    method: parts.next().unwrap_or("").into(),
                    path: parts.next().unwrap_or("").into(),
                    headers: lines
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                        .collect(),
                    body: String::from_utf8_lossy(&buf[he..]).into_owned(),
                };
                let (code, body) = respond(&seen_one);
                log.lock().unwrap().push(seen_one);
                let reply = format!(
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes());
            }
        });
        Fake { port, seen }
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// A fake agent: health with roles and engines, a conversation per `new`, and an answer per `send`.
pub fn fake_agent(roles: bool) -> Fake {
    fake_agent_with(roles, true)
}

/// `fake_agent`, saying whether its health advertises engines. With engines on, a conversation
/// made with an engine says that engine answered, as the real agent does.
pub fn fake_agent_with(roles: bool, engines: bool) -> Fake {
    let convs = Arc::new(Mutex::new(Vec::<(String, String, String)>::new()));
    Fake::start(move |r| {
        let key = r.header("x-tempo-key").unwrap_or("").to_string();
        match (r.method.as_str(), r.path.split('?').next().unwrap_or("")) {
            ("GET", "/agent/health") => (
                200,
                if engines {
                    format!("{{\"ok\":true,\"guest\":true,\"stream\":true,\"roles\":{roles},\"engines\":true}}")
                } else {
                    format!("{{\"ok\":true,\"guest\":true,\"stream\":true,\"roles\":{roles}}}")
                },
            ),
            _ if key != "test-key" => (401, "{\"error\":\"this needs the agent's key\"}".into()),
            ("POST", "/agent/new") => {
                let mut c = convs.lock().unwrap();
                let id = format!("conv-{}", c.len() + 1);
                let pinned = r.json().str("engine").unwrap_or("").to_string();
                c.push((id.clone(), String::new(), pinned));
                (200, format!("{{\"conv\":\"{id}\"}}"))
            }
            ("POST", "/agent/send") => {
                let j = r.json();
                let conv = j.str("conv").unwrap_or("").to_string();
                let text = j.str("text").unwrap_or("").to_string();
                for c in convs.lock().unwrap().iter_mut() {
                    if c.0 == conv {
                        c.1 = text.clone();
                    }
                }
                (200, "{\"ok\":true}".into())
            }
            ("GET", "/agent/events") => {
                let c = convs.lock().unwrap();
                let n = c.len();
                let said = c.last().map(|c| c.1.clone()).unwrap_or_default();
                let engine = c.last().map(|c| c.2.clone()).unwrap_or_default();
                // the answer repeats what it was told, so a test can see what a step was given
                let text = format!("Answer {n}.\nI was told: {said}");
                (
                    200,
                    format!(
                        "{{\"events\":[{{\"type\":\"status\",\"text\":\"memory: off (guest conversation)\"}},{{\"type\":\"assistant\",\"text\":\"{}\"}}],\"busy\":false,\"engine\":\"{}\"}}",
                        crate::json_escape(&text),
                        crate::json_escape(&engine)
                    ),
                )
            }
            _ => (404, "{\"error\":\"not found\"}".into()),
        }
    })
}

/// A fake Room with the owner's control key on disk. `teammates` is what `GET /room/team` says.
pub fn fake_room(dir: &Path, teammates: &'static str, rooms: &'static str) -> Fake {
    let room = Fake::start(move |r| {
        let owner = r.header("x-tempo-control") == Some("room-control-key");
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/room/health") => (200, "{\"app\":\"tempo-room\",\"ok\":true}".into()),
            _ if !owner => (403, "{\"error\":\"owner only\"}".into()),
            ("GET", "/room/rooms") => (200, rooms.into()),
            ("GET", "/room/team") => (200, teammates.into()),
            ("POST", "/room/create") => {
                let name = r.json().str("name").unwrap_or("").to_string();
                (
                    200,
                    format!("{{\"ok\":true,\"room\":{{\"id\":\"{name}\"}}}}"),
                )
            }
            ("POST", "/room/team/add") => (200, "{\"ok\":true}".into()),
            ("POST", "/room/team/room") => (200, "{\"ok\":true}".into()),
            _ => (404, "{\"error\":\"no such endpoint\"}".into()),
        }
    });
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        format!("{{\"addr\":\"127.0.0.1:{}\"}}", room.port),
    )
    .unwrap();
    std::fs::write(dir.join("control.key"), "room-control-key\n").unwrap();
    room
}

pub struct FakeChat {
    pub calls: RefCell<Vec<(String, String, String)>>,
    pub fail_on: Option<usize>,
    /// The engine asked for in each call, in order (empty: none).
    pub asked: RefCell<Vec<String>>,
    /// Report the engine asked for as the one that answered, as an agent that can pin does.
    pub reports: bool,
}

impl Default for FakeChat {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeChat {
    pub fn new() -> FakeChat {
        FakeChat {
            calls: RefCell::new(vec![]),
            fail_on: None,
            asked: RefCell::new(vec![]),
            reports: false,
        }
    }
}

impl Chat for FakeChat {
    fn say(&self, model: &str, role: &str, text: &str) -> Result<String, String> {
        let n = self.calls.borrow().len() + 1;
        self.calls
            .borrow_mut()
            .push((model.into(), role.into(), text.into()));
        if self.fail_on == Some(n) {
            return Err("the model fell over".into());
        }
        Ok(format!("Output of call {n}."))
    }

    fn say_on(&self, engine: &str, model: &str, role: &str, text: &str) -> Result<Said, String> {
        self.asked.borrow_mut().push(engine.to_string());
        let text = self.say(model, role, text)?;
        let engine = (self.reports && !engine.is_empty()).then(|| engine.to_string());
        Ok(Said { text, engine })
    }
}

#[derive(Default)]
pub struct RecSched {
    pub calls: RefCell<Vec<(String, Vec<String>)>>,
    pub fail: bool,
}

impl Scheduler for RecSched {
    fn install(&self, band: &str, entries: &[Entry]) -> Result<(), String> {
        self.calls.borrow_mut().push((
            band.into(),
            entries.iter().map(|e| e.cron.clone()).collect(),
        ));
        if self.fail {
            Err("no crontab here".into())
        } else {
            Ok(())
        }
    }
}

/// The engines and apps as a test says they are.
#[derive(Default)]
pub struct FakeOutside {
    pub engines: Vec<engines::Engine>,
    pub apps: Vec<page::AppState>,
}

impl Outside for FakeOutside {
    fn engines(&self) -> Vec<engines::Engine> {
        self.engines.clone()
    }

    fn apps(&self) -> Vec<page::AppState> {
        self.apps.clone()
    }
}

static NO_OUTSIDE: std::sync::OnceLock<FakeOutside> = std::sync::OnceLock::new();

fn no_outside() -> &'static FakeOutside {
    NO_OUTSIDE.get_or_init(FakeOutside::default)
}

struct World<'a> {
    chat: &'a FakeChat,
    sched: &'a RecSched,
    room_dir: PathBuf,
}

impl World<'_> {
    fn deps(&self) -> Deps<'_> {
        Deps {
            chat: self.chat,
            outside: no_outside(),
            room_dir: self.room_dir.clone(),
            scheduler: self.sched,
            models: models(),
            exe: PathBuf::from("/opt/tempo/tempo-launch"),
            now: NOW,
        }
    }
}

fn args(v: &[&str]) -> ConductorCmd {
    parse_args(v).unwrap()
}

fn go(ctx: &Ctx, w: &World, cmd: ConductorCmd, answer: bool) -> (i32, String, usize) {
    let mut out = Vec::new();
    let mut asked = 0;
    let code = dispatch(ctx, cmd, &w.deps(), &mut out, &mut |q| {
        assert!(q.contains("[y/N]"), "{q}");
        asked += 1;
        answer
    });
    (code, String::from_utf8(out).unwrap(), asked)
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

// ---- the command line

#[test]
fn the_command_line_is_parsed_strictly() {
    assert_eq!(parse_args(&[]), Ok(ConductorCmd::List));
    assert_eq!(parse_args(&["templates"]), Ok(ConductorCmd::Templates));
    assert_eq!(
        parse_args(&["new", "--template", "study", "--yes"]),
        Ok(ConductorCmd::New {
            template: Some("study".into()),
            describe: None,
            name: None,
            yes: true
        })
    );
    assert_eq!(
        parse_args(&["new", "--describe", "I bake", "--name", "bakery-posts"]),
        Ok(ConductorCmd::New {
            template: None,
            describe: Some("I bake".into()),
            name: Some("bakery-posts".into()),
            yes: false
        })
    );
    assert_eq!(
        parse_args(&["run", "bakery", "weekly-plan", "--note", "focus on bread"]),
        Ok(ConductorCmd::Run {
            band: "bakery".into(),
            flow: Some("weekly-plan".into()),
            note: Some("focus on bread".into())
        })
    );
    assert_eq!(
        parse_args(&["run", "bakery"]),
        Ok(ConductorCmd::Run {
            band: "bakery".into(),
            flow: None,
            note: None
        })
    );
    assert_eq!(
        parse_args(&["apply-proposal"]),
        Ok(ConductorCmd::ApplyProposal { name: None })
    );
    assert_eq!(parse_args(&["engines"]), Ok(ConductorCmd::Engines));
    assert_eq!(parse_args(&["page"]), Ok(ConductorCmd::Page { band: None }));
    assert_eq!(
        parse_args(&["page", "bakery"]),
        Ok(ConductorCmd::Page {
            band: Some("bakery".into())
        })
    );
    for bad in [
        &["new"][..],
        &["new", "--template", "nope"],
        &["new", "--template", "study", "--describe", "x"],
        &["new", "--describe", "  "],
        &["new", "--describe"],
        &["new", "--template", "study", "--name", "Bad Name"],
        &["new", "--template", "study", "--what"],
        &["run"],
        &["run", "../escape"],
        &["run", "bakery", "Bad Flow"],
        &["run", "bakery", "a", "--note"],
        &["page", "a", "b"],
        &["page", "Bad Band"],
        &["engines", "x"],
        &["room", "A_B"],
        &["templates", "x"],
        &["apply-proposal", "a", "b"],
        &["dance"],
    ] {
        assert!(parse_args(bad).is_err(), "{bad:?}");
    }
    // and through the launcher's own parser
    assert!(matches!(
        crate::parse(&["band".to_string(), "templates".to_string()]),
        Ok(Cmd::Conductor(ConductorCmd::Templates))
    ));
    assert!(crate::USAGE.contains("conductor new (--template NAME | --describe"));
    for verb in [
        "conductor templates",
        "conductor run",
        "conductor room",
        "conductor engines",
        "conductor page",
        "conductor apply-proposal",
    ] {
        assert!(crate::USAGE.contains(verb), "{verb}");
    }
}

// ---- new

#[test]
fn a_template_band_is_shown_asked_about_saved_registered_and_scheduled() {
    let root = tmp("new");
    let ctx = ctx_in(&root, true);
    let room_dir = root.join("room");
    let room = fake_room(&room_dir, "{\"team\":[]}", "{\"rooms\":[]}");
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir,
    };
    let (code, out, asked) = go(
        &ctx,
        &w,
        args(&["new", "--template", "small-business"]),
        true,
    );
    assert_eq!((code, asked), (0, 1), "{out}");
    // shown in plain words before the question
    for want in [
        "@sorter  Sorty, the Accordion",
        "morning-inbox",
        "every day at 08:30",
        "every Friday at 15:00",
        "Bands/small-business/ in your Vault",
        "never post, send, buy or delete",
    ] {
        assert!(out.contains(want), "{want}\n{out}");
    }
    // saved, and what is saved is a valid band with models from the plan
    let band = load(&ctx, "small-business").unwrap();
    assert_eq!(band.members.len(), 4);
    assert_eq!(band.member("followup").unwrap().model, "qwen3:8b");
    assert_eq!(band.member("sorter").unwrap().model, "qwen3:4b");
    // Room: the room, then one teammate per member, all as the owner, all agent teammates
    let reqs = room.requests();
    assert!(reqs.iter().all(
        |r| r.header("x-tempo-control") == Some("room-control-key") || r.path == "/room/health"
    ));
    let create: Vec<_> = reqs.iter().filter(|r| r.path == "/room/create").collect();
    assert_eq!(create.len(), 1);
    assert_eq!(create[0].method, "POST");
    assert_eq!(create[0].header("content-type"), Some("application/json"));
    assert_eq!(create[0].json().str("name"), Some("small-business"));
    let adds: Vec<_> = reqs.iter().filter(|r| r.path == "/room/team/add").collect();
    assert_eq!(adds.len(), 4);
    for (a, m) in adds.iter().zip(&band.members) {
        let j = a.json();
        assert_eq!(a.method, "POST");
        assert_eq!(j.str("handle"), Some(m.handle.as_str()));
        assert_eq!(j.str("name"), Some(m.name.as_str()));
        assert_eq!(j.str("kind"), Some("agent"));
        assert_eq!(j.str("model"), Some(m.model.as_str()));
        assert_eq!(j.strs("rooms"), ["small-business"]);
        let role = j.str("role").unwrap();
        assert!(role.starts_with(&m.role) && role.contains("cannot post, send, buy"));
        assert!(role.chars().count() <= 2000);
        // nothing that could hand a teammate a tool
        for key in ["tools", "tool", "permissions", "engine"] {
            assert!(j.get(key).is_none(), "{key}");
        }
    }
    assert!(out.contains("@sorter @billing @followup @briefer"), "{out}");
    // scheduled: both flows, through the scheduler
    let calls = sched.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "small-business");
    assert_eq!(calls[0].1.len(), 2);
    assert!(calls[0].1[0].contains("conductor run small-business morning-inbox"));
    assert!(out.contains("Scheduled"), "{out}");
    // the agent was not needed for a template
    assert!(chat.calls.borrow().is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn saying_no_changes_nothing() {
    let root = tmp("no");
    let ctx = ctx_in(&root, true);
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("room"),
    };
    let (code, out, asked) = go(&ctx, &w, args(&["new", "--template", "study"]), false);
    assert_eq!((code, asked), (0, 1));
    assert!(out.contains("Nothing was changed"));
    assert!(!super::band_dir(&ctx, "study").exists());
    assert!(sched.calls.borrow().is_empty());
    // --yes skips the question
    let (code, _, asked) = go(
        &ctx,
        &w,
        args(&["new", "--template", "study", "--yes"]),
        false,
    );
    assert_eq!((code, asked), (0, 0));
    assert!(super::band_dir(&ctx, "study").join("band.json").is_file());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn without_room_it_says_how_to_add_the_members_later_and_a_scheduler_failure_prints_the_lines() {
    let root = tmp("noroom");
    let ctx = ctx_in(&root, true);
    let (chat, sched) = (
        FakeChat::new(),
        RecSched {
            fail: true,
            ..RecSched::default()
        },
    );
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("no-room-here"),
    };
    let (code, out, _) = go(
        &ctx,
        &w,
        args(&[
            "new",
            "--template",
            "social-media",
            "--name",
            "bakery-posts",
            "--yes",
        ]),
        true,
    );
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Room is not running"), "{out}");
    assert!(
        out.contains("tempo-launch conductor room bakery-posts"),
        "{out}"
    );
    assert!(out.contains("not scheduled (no crontab here)"), "{out}");
    assert!(
        out.contains(
            "conductor run bakery-posts weekly-plan # perennos-tempo-band bakery-posts/weekly-plan"
        ) || out.contains("schtasks /Create"),
        "{out}"
    );
    // a second run with the same name says it replaces, and keeps the old file
    let (_, out2, _) = go(
        &ctx,
        &w,
        args(&[
            "new",
            "--template",
            "social-media",
            "--name",
            "bakery-posts",
            "--yes",
        ]),
        true,
    );
    assert!(out2.contains("already here"), "{out2}");
    assert!(files_under(&ctx.home.join("trash"))
        .iter()
        .any(|p| p.to_string_lossy().contains("band-bakery-posts")));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn describe_with_junk_from_the_model_falls_back_to_the_template() {
    let root = tmp("describe");
    let ctx = ctx_in(&root, true);
    struct Junk;
    impl Chat for Junk {
        fn say(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
            Ok("I would be delighted to help! {not json".into())
        }
    }
    let sched = RecSched::default();
    let d = Deps {
        chat: &Junk,
        outside: no_outside(),
        room_dir: root.join("room"),
        scheduler: &sched,
        models: models(),
        exe: PathBuf::from("/x/tempo-launch"),
        now: NOW,
    };
    let mut out = Vec::new();
    let code = dispatch(
        &ctx,
        args(&[
            "new",
            "--describe",
            "I run a bakery's Instagram and want posts planned weekly",
            "--yes",
        ]),
        &d,
        &mut out,
        &mut |_| true,
    );
    let out = String::from_utf8(out).unwrap();
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("not usable"), "{out}");
    assert!(out.contains("social-media template as it is"), "{out}");
    let band = load(&ctx, "social-media").unwrap();
    assert_eq!(band.members.len(), 4);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bands_and_templates_are_listed() {
    let root = tmp("list");
    let ctx = ctx_in(&root, true);
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("room"),
    };
    let (_, out, _) = go(&ctx, &w, ConductorCmd::List, true);
    assert!(
        out.contains("No bands yet") && out.contains("--describe"),
        "{out}"
    );
    go(
        &ctx,
        &w,
        args(&["new", "--template", "study", "--yes"]),
        true,
    );
    // a hand-edited band that no longer passes is flagged, not hidden
    let broken = super::band_dir(&ctx, "broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("band.json"), "{\"name\":\"broken\"}").unwrap();
    let (_, out, _) = go(&ctx, &w, ConductorCmd::List, true);
    assert!(
        out.contains("study:") && out.contains("@planner @explainer @quizzer @coach"),
        "{out}"
    );
    assert!(
        out.contains("flow weekly-plan: every Sunday at 17:00"),
        "{out}"
    );
    assert!(out.contains("broken: needs attention"), "{out}");
    let (_, out, _) = go(&ctx, &w, ConductorCmd::Templates, true);
    for t in templates::all() {
        assert!(out.contains(t.id));
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_saved_band_is_checked_again_when_it_is_read() {
    let root = tmp("load");
    let ctx = ctx_in(&root, false);
    assert!(load(&ctx, "../etc")
        .unwrap_err()
        .contains("not a band name"));
    assert!(load(&ctx, "ghost")
        .unwrap_err()
        .contains("no band called ghost"));
    let d = band_dir(&ctx, "study");
    std::fs::create_dir_all(&d).unwrap();
    let good = templates::find("study").unwrap().band("study", &models());
    // the folder name and the band's name must agree
    let other = templates::find("study").unwrap().band("other", &models());
    std::fs::write(d.join("band.json"), other.to_json()).unwrap();
    assert!(load(&ctx, "study").unwrap_err().contains("folder is study"));
    // a flow that writes outside the band's folder is refused
    std::fs::write(
        d.join("band.json"),
        good.to_json().replace("Bands/study/", "Bands/elsewhere/"),
    )
    .unwrap();
    assert!(load(&ctx, "study").unwrap_err().contains("notes_to"));
    let _ = std::fs::remove_dir_all(root);
}

// ---- Room

#[test]
fn room_reuses_a_room_and_a_teammate_and_reports_a_clash() {
    let root = tmp("room");
    let dir = root.join("room");
    let band = templates::find("study").unwrap().band("study", &models());
    let same_role = with_guard(&band.member("planner").unwrap().role);
    // Room already has the room, @planner with this very role (in the room already for
    // @explainer's sake), and an @explainer someone wrote differently
    let teammates: &'static str = Box::leak(
        format!(
            "{{\"team\":[{{\"handle\":\"planner\",\"role\":\"{}\",\"rooms\":[]}},{{\"handle\":\"explainer\",\"role\":\"Something else\",\"rooms\":[]}},{{\"handle\":\"quizzer\",\"role\":\"x\",\"rooms\":[\"study-room\"]}}]}}",
            crate::json_escape(&same_role)
        )
        .into_boxed_str(),
    );
    let room = fake_room(
        &dir,
        teammates,
        "{\"rooms\":[{\"id\":\"study-room\",\"name\":\"Study\"}]}",
    );
    let link = net::find_room(&dir).unwrap();
    let r = net::register_band(&link, &band).unwrap();
    assert_eq!(r.room, "study-room");
    assert_eq!(r.added, ["planner", "quizzer", "coach"]);
    assert_eq!(r.left_out.len(), 1);
    assert_eq!(r.left_out[0].0, "explainer");
    assert!(r.left_out[0].1.contains("different role"));
    let paths: Vec<(String, String)> = room
        .requests()
        .into_iter()
        .map(|s| (s.method, s.path))
        .collect();
    // no new room; planner only joins; coach is added fresh
    assert!(!paths.iter().any(|(_, p)| p == "/room/create"));
    assert_eq!(
        paths.iter().filter(|(_, p)| p == "/room/team/room").count(),
        1
    );
    assert_eq!(
        paths.iter().filter(|(_, p)| p == "/room/team/add").count(),
        1
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn room_is_found_only_on_this_computer_and_only_if_it_answers_as_room() {
    let root = tmp("findroom");
    let dir = root.join("room");
    std::fs::create_dir_all(&dir).unwrap();
    assert!(net::find_room(&dir).is_none());
    // an address that is not this computer is never called, and its key never sent
    std::fs::write(dir.join("server.json"), "{\"addr\":\"192.168.0.9:8767\"}").unwrap();
    std::fs::write(dir.join("control.key"), "k").unwrap();
    assert!(net::find_room(&dir).is_none());
    // something else on the port is not Room
    let other = Fake::start(|_| (200, "{\"app\":\"something-else\"}".into()));
    std::fs::write(
        dir.join("server.json"),
        format!("{{\"addr\":\"127.0.0.1:{}\"}}", other.port),
    )
    .unwrap();
    assert!(net::find_room(&dir).is_none());
    assert!(other
        .requests()
        .iter()
        .all(|r| r.header("x-tempo-control").is_none()));
    // server.json but no control key
    let room = Fake::start(|_| (200, "{\"app\":\"tempo-room\"}".into()));
    std::fs::write(
        dir.join("server.json"),
        format!("{{\"addr\":\"127.0.0.1:{}\"}}", room.port),
    )
    .unwrap();
    std::fs::remove_file(dir.join("control.key")).unwrap();
    assert!(net::find_room(&dir).is_none());
    std::fs::write(dir.join("control.key"), "good-key\n").unwrap();
    let RoomLink { port, key } = net::find_room(&dir).unwrap();
    assert_eq!((port, key.as_str()), (room.port, "good-key"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn band_room_needs_room_to_be_running() {
    let root = tmp("bandroom");
    let ctx = ctx_in(&root, true);
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let mut w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("room"),
    };
    go(
        &ctx,
        &w,
        args(&["new", "--template", "study", "--yes"]),
        true,
    );
    let (code, ..) = go(&ctx, &w, args(&["room", "study"]), true);
    assert_eq!(code, 1);
    let room = fake_room(&root.join("room2"), "{\"team\":[]}", "{\"rooms\":[]}");
    w.room_dir = root.join("room2");
    let (code, out, _) = go(&ctx, &w, args(&["room", "study"]), true);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("@planner @explainer @quizzer @coach"), "{out}");
    assert_eq!(
        room.requests()
            .iter()
            .filter(|r| r.path == "/room/team/add")
            .count(),
        4
    );
    let (code, ..) = go(&ctx, &w, args(&["room", "ghost"]), true);
    assert_eq!(code, 1);
    let _ = std::fs::remove_dir_all(root);
}

// ---- the agent

#[test]
fn guests_have_a_role_a_model_and_no_tools() {
    let agent_srv = fake_agent(true);
    let mut agent = Agent::new(agent_srv.port, Some("test-key".into()));
    agent.poll = Duration::from_millis(5);
    assert!(agent.knows_roles());
    let answer = agent
        .say("qwen3:8b", "You are \"Quill\".\nWrite.", "hello there")
        .unwrap();
    assert!(answer.starts_with("Answer 1."), "{answer}");
    assert!(answer.contains("I was told: hello there"));
    let reqs = agent_srv.requests();
    let new = reqs.iter().find(|r| r.path == "/agent/new").unwrap();
    let j = new.json();
    assert_eq!(j.str("mode"), Some("guest"));
    assert_eq!(j.str("model"), Some("qwen3:8b"));
    assert_eq!(j.str("role"), Some("You are \"Quill\".\nWrite."));
    for key in ["tools", "tool", "allow", "permissions"] {
        assert!(j.get(key).is_none(), "{key}");
    }
    assert_eq!(new.header("content-type"), Some("application/json"));
    assert_eq!(new.header("x-tempo-key"), Some("test-key"));
    // an empty model leaves the agent's own choice alone
    agent.say("", "r", "t").unwrap();
    let reqs = agent_srv.requests();
    let second = reqs
        .iter()
        .filter(|r| r.path == "/agent/new")
        .nth(1)
        .unwrap();
    assert!(second.json().get("model").is_none());
}

#[test]
fn an_agent_without_roles_a_wrong_key_and_an_error_turn_are_each_reported() {
    let old = fake_agent(false);
    assert!(!Agent::new(old.port, Some("test-key".into())).knows_roles());
    let srv = fake_agent(true);
    let bad_key = Agent::new(srv.port, Some("wrong".into()));
    assert!(bad_key
        .new_guest("", "r")
        .unwrap_err()
        .contains("needs the agent's key"));
    assert!(Agent::new(1, None).new_guest("", "r").is_err());

    let n = Arc::new(Mutex::new(0));
    let busy_then_error = Fake::start(move |r| match r.path.split('?').next().unwrap() {
        "/agent/new" => (200, "{\"conv\":\"c\"}".into()),
        "/agent/send" => (200, "{\"ok\":true}".into()),
        _ => {
            let mut n = n.lock().unwrap();
            *n += 1;
            if *n < 3 {
                (200, "{\"events\":[],\"busy\":true}".into())
            } else {
                (
                    200,
                    "{\"events\":[{\"type\":\"error\",\"text\":\"no model could be reached\"}],\"busy\":false}".into(),
                )
            }
        }
    });
    let mut a = Agent::new(busy_then_error.port, None);
    a.poll = Duration::from_millis(5);
    let e = a.say("", "r", "t").unwrap_err();
    assert!(e.contains("no model could be reached"), "{e}");

    // a turn that never ends gives up
    let stuck = Fake::start(|r| match r.path.split('?').next().unwrap() {
        "/agent/new" => (200, "{\"conv\":\"c\"}".into()),
        "/agent/send" => (200, "{\"ok\":true}".into()),
        _ => (200, "{\"events\":[],\"busy\":true}".into()),
    });
    let mut a = Agent::new(stuck.port, None);
    a.poll = Duration::from_millis(5);
    a.patience = Duration::from_millis(40);
    assert!(a.say("", "r", "t").unwrap_err().contains("too long"));
}

#[test]
fn the_engine_is_sent_only_to_an_agent_whose_health_says_it_takes_one() {
    let engine = "http://10.0.0.5:11434";
    // a newer agent: the address goes with the new conversation, and the answer names the engine
    let newer = fake_agent_with(true, true);
    let mut agent = Agent::new(newer.port, Some("test-key".into()));
    agent.poll = Duration::from_millis(5);
    assert!(agent.knows_engines());
    let said = agent.say_on(engine, "qwen3:8b", "r", "hello").unwrap();
    assert_eq!(said.engine.as_deref(), Some(engine));
    assert!(said.text.starts_with("Answer 1."));
    let reqs = newer.requests();
    let new = reqs.iter().find(|r| r.path == "/agent/new").unwrap();
    assert_eq!(new.json().str("engine"), Some(engine));
    assert_eq!(new.json().str("model"), Some("qwen3:8b"));
    // no engine to ask for: none is sent
    agent.say_on("", "qwen3:8b", "r", "hello").unwrap();
    let reqs = newer.requests();
    let second = reqs
        .iter()
        .filter(|r| r.path == "/agent/new")
        .nth(1)
        .unwrap();
    assert!(second.json().get("engine").is_none());

    // an older agent: today's request, no engine, and nothing reported back
    let older = fake_agent_with(true, false);
    let mut agent = Agent::new(older.port, Some("test-key".into()));
    agent.poll = Duration::from_millis(5);
    assert!(agent.knows_roles() && !agent.knows_engines());
    let said = agent.say_on(engine, "qwen3:8b", "r", "hello").unwrap();
    assert_eq!(said.engine, None);
    let reqs = older.requests();
    let new = reqs.iter().find(|r| r.path == "/agent/new").unwrap();
    assert!(new.json().get("engine").is_none(), "{}", new.body);

    // an agent that does not list the address (its settings changed): the part is still played
    let strict = Fake::start(|r| match r.path.split('?').next().unwrap() {
        "/agent/health" => (200, "{\"ok\":true,\"roles\":true,\"engines\":true}".into()),
        "/agent/new" if r.body.contains("\"engine\"") => (
            400,
            "{\"error\":\"engine: not one of this agent's engines\"}".into(),
        ),
        "/agent/new" => (200, "{\"conv\":\"c\"}".into()),
        "/agent/send" => (200, "{\"ok\":true}".into()),
        _ => (
            200,
            "{\"events\":[{\"type\":\"assistant\",\"text\":\"played\"}],\"busy\":false}".into(),
        ),
    });
    let mut agent = Agent::new(strict.port, None);
    agent.poll = Duration::from_millis(5);
    let said = agent.say_on(engine, "m", "r", "t").unwrap();
    assert_eq!((said.text.as_str(), said.engine), ("played", None));
    assert_eq!(
        strict
            .requests()
            .iter()
            .filter(|r| r.path == "/agent/new")
            .count(),
        2
    );
    // any other refusal is still an error
    let broken = Fake::start(|r| match r.path.split('?').next().unwrap() {
        "/agent/health" => (200, "{\"ok\":true,\"roles\":true,\"engines\":true}".into()),
        _ => (400, "{\"error\":\"role: at most 2000 characters\"}".into()),
    });
    let e = Agent::new(broken.port, None)
        .say_on(engine, "m", "r", "t")
        .unwrap_err();
    assert!(e.contains("role: at most"), "{e}");
}

// ---- run

#[test]
fn a_run_writes_linked_notes_in_order_through_the_agent() {
    let root = tmp("run");
    let ctx = ctx_in(&root, true);
    let room_dir = root.join("room");
    let agent_srv = fake_agent(true);
    let mut agent = Agent::new(agent_srv.port, Some("test-key".into()));
    agent.poll = Duration::from_millis(5);
    let band = templates::find("social-media")
        .unwrap()
        .band("social-media", &models());
    let flow = band.flow("weekly-plan").unwrap();
    // the owner left material in the Inbox
    let inbox = ctx.vault.join("Bands/social-media/Inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    std::fs::write(inbox.join("shop.md"), "We bake sourdough on Saturdays.").unwrap();
    std::fs::write(inbox.join("skip.pdf"), "not read").unwrap();
    let before = files_under(&root);
    let mut out = Vec::new();
    let done = run::run_flow(
        &ctx,
        &band,
        flow,
        Some("push the new rye loaf"),
        &agent,
        NOW,
        &mut out,
    )
    .unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("step 1 of 4") && out.contains("step 4 of 4"),
        "{out}"
    );

    // notes: <date> <flow>/<n> <member>.md, in order, under the band's folder in the Vault
    let date = &crate::stamp(NOW)[..10];
    let folder = ctx
        .vault
        .join("Bands/social-media")
        .join(format!("{date} weekly-plan"));
    assert_eq!(done.folder, folder);
    let names: Vec<String> = files_under(&folder)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "1 researcher.md",
            "2 writer.md",
            "3 editor.md",
            "4 scheduler.md",
            "Index.md"
        ]
    );
    let note = |n: &str| std::fs::read_to_string(folder.join(n)).unwrap();

    // front matter and links: step 2 points back to step 1 and to the index
    let two = note("2 writer.md");
    assert!(
        two.starts_with("---\nband: social-media\nmember: writer\nflow: weekly-plan\nstep: 2\n"),
        "{two}"
    );
    let base = format!("Bands/social-media/{date} weekly-plan");
    assert!(
        two.contains(&format!("[[{base}/1 researcher|researcher (Scout)]]")),
        "{two}"
    );
    assert!(
        two.contains(&format!("[[{base}/Index|weekly-plan, {date}]]")),
        "{two}"
    );
    assert!(
        note("1 researcher.md").contains("Flow: [[")
            && !note("1 researcher.md").contains("Step before")
    );
    assert!(note("4 scheduler.md").contains(&format!("[[{base}/3 editor|editor (Polish)]]")));
    let index = note("Index.md");
    for n in 1..=4 {
        assert!(index.contains(&format!("{n}. [[{base}/{n} ")), "{index}");
    }
    assert!(!index.contains("stopped early"));
    // every link points at a note that exists
    for text in files_under(&folder)
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
    {
        for l in text.split("[[").skip(1) {
            let target = l.split(['|', ']']).next().unwrap();
            assert!(ctx.vault.join(format!("{target}.md")).is_file(), "{target}");
        }
    }

    // what each step was told: the goal, the owner's note, the inbox (first step only), and the
    // step before it
    let reqs = agent_srv.requests();
    let sends: Vec<J> = reqs
        .iter()
        .filter(|r| r.path == "/agent/send")
        .map(Seen::json)
        .collect();
    assert_eq!(sends.len(), 4);
    let t1 = sends[0].str("text").unwrap();
    assert!(t1.contains("Plan next week's posts") && t1.contains("push the new rye loaf"));
    assert!(t1.contains("We bake sourdough on Saturdays.") && !t1.contains("not read"));
    assert!(t1.contains("You are first"));
    let t2 = sends[1].str("text").unwrap();
    assert!(
        !t2.split("The step before you")
            .next()
            .unwrap()
            .contains("Material the owner left"),
        "the inbox goes to the first step only"
    );
    assert!(t2.contains("written by Scout") && t2.contains("Answer 1."));
    assert!(sends[3].str("text").unwrap().contains("written by Polish"));
    // each member spoke as itself, on the model of its job
    let news: Vec<J> = reqs
        .iter()
        .filter(|r| r.path == "/agent/new")
        .map(Seen::json)
        .collect();
    assert_eq!(news.len(), 4);
    assert_eq!(news[0].str("model"), Some("qwen3:4b"));
    assert_eq!(news[1].str("model"), Some("qwen3:8b"));
    assert!(news[1]
        .str("role")
        .unwrap()
        .starts_with("You are the writer"));

    // nothing was written anywhere but the run folder and what was there before
    let after = files_under(&root);
    for p in after.iter().filter(|p| !before.contains(p)) {
        assert!(
            p.starts_with(&folder),
            "{} is outside the run folder",
            p.display()
        );
    }
    let _ = room_dir;
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn members_never_get_tools_and_a_run_writes_only_under_the_bands_notes() {
    let root = tmp("never");
    let ctx = ctx_in(&root, true);
    let agent_srv = fake_agent(true);
    let mut agent = Agent::new(agent_srv.port, Some("test-key".into()));
    agent.poll = Duration::from_millis(5);
    // every template, every flow
    let snapshot = |dir: &Path| files_under(dir);
    for t in templates::all() {
        let band = t.band(t.id, &models());
        for flow in &band.flows {
            let before = snapshot(&root);
            run::run_flow(&ctx, &band, flow, None, &agent, NOW, &mut Vec::new()).unwrap();
            let allowed = ctx.vault.join("Bands").join(t.id);
            for p in snapshot(&root).iter().filter(|p| !before.contains(p)) {
                assert!(p.starts_with(&allowed), "{} {}", t.id, p.display());
            }
        }
    }
    // every conversation the agent saw was a guest one
    let reqs = agent_srv.requests();
    let news: Vec<Seen> = reqs
        .into_iter()
        .filter(|r| r.path == "/agent/new")
        .collect();
    assert!(news.len() > 20);
    for r in &news {
        let j = r.json();
        assert_eq!(j.str("mode"), Some("guest"), "{}", r.body);
        let keys: Vec<&str> = match &j {
            J::Obj(o) => o.iter().map(|(k, _)| k.as_str()).collect(),
            _ => vec![],
        };
        assert!(
            keys.iter().all(|k| ["mode", "role", "model"].contains(k)),
            "{keys:?}"
        );
    }
    // the agent is only ever asked to create conversations and to send and read them
    let paths: std::collections::BTreeSet<String> = agent_srv
        .requests()
        .into_iter()
        .map(|r| r.path.split('?').next().unwrap().to_string())
        .collect();
    assert_eq!(
        paths.into_iter().collect::<Vec<_>>(),
        ["/agent/events", "/agent/new", "/agent/send"]
    );
    // and the band's own code names no way to give a member a tool or to act outside
    for src in [
        include_str!("net.rs"),
        include_str!("run.rs"),
        include_str!("draft.rs"),
    ] {
        let code = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "\"mode\\\":\\\"agent",
            "/agent/confirm",
            "run_command",
            "\"tools\"",
        ] {
            assert!(!code.contains(forbidden), "{forbidden}");
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn without_a_vault_the_notes_go_to_the_bands_own_runs_folder() {
    let root = tmp("novault");
    let ctx = ctx_in(&root, false);
    assert!(!ctx.vault.exists());
    let band = templates::find("study").unwrap().band("study", &models());
    let flow = band.flow("quiz-me").unwrap();
    let chat = FakeChat::new();
    let done = run::run_flow(&ctx, &band, flow, None, &chat, NOW, &mut Vec::new()).unwrap();
    let date = &crate::stamp(NOW)[..10];
    assert_eq!(
        done.folder,
        ctx.home
            .join("bands/study/runs")
            .join(format!("{date} quiz-me"))
    );
    assert!(!ctx.vault.exists(), "no vault is made");
    let note = std::fs::read_to_string(&done.notes[0]).unwrap();
    assert!(
        note.contains(&format!("[[{date} quiz-me/Index|quiz-me, {date}]]")),
        "{note}"
    );
    // the same flow the same day gets its own folder
    let again = run::run_flow(&ctx, &band, flow, None, &chat, NOW, &mut Vec::new()).unwrap();
    assert!(again.folder.to_string_lossy().ends_with("quiz-me (2)"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_member_that_cannot_answer_stops_the_flow_and_the_index_says_so() {
    let root = tmp("stop");
    let ctx = ctx_in(&root, true);
    let band = templates::find("social-media")
        .unwrap()
        .band("social-media", &models());
    let flow = band.flow("weekly-plan").unwrap();
    let chat = FakeChat {
        fail_on: Some(3),
        ..FakeChat::new()
    };
    let mut out = Vec::new();
    let e = run::run_flow(&ctx, &band, flow, None, &chat, NOW, &mut out).unwrap_err();
    assert!(
        e.contains("Polish could not answer (the model fell over)"),
        "{e}"
    );
    let date = &crate::stamp(NOW)[..10];
    let folder = ctx
        .vault
        .join("Bands/social-media")
        .join(format!("{date} weekly-plan"));
    let names: Vec<String> = files_under(&folder)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["1 researcher.md", "2 writer.md", "Index.md"]);
    let index = std::fs::read_to_string(folder.join("Index.md")).unwrap();
    assert!(
        index.contains("stopped early: Polish could not answer"),
        "{index}"
    );
    assert_eq!(chat.calls.borrow().len(), 3);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn run_picks_the_flow_and_says_when_it_cannot() {
    let root = tmp("pick");
    let ctx = ctx_in(&root, true);
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("room"),
    };
    go(
        &ctx,
        &w,
        args(&["new", "--template", "study", "--yes"]),
        true,
    );
    // two flows and none named: asks which
    assert_eq!(go(&ctx, &w, args(&["run", "study"]), true).0, 2);
    assert_eq!(go(&ctx, &w, args(&["run", "study", "nope"]), true).0, 2);
    assert_eq!(go(&ctx, &w, args(&["run", "ghost"]), true).0, 1);
    assert_eq!(chat.calls.borrow().len(), 0);
    let (code, out, _) = go(
        &ctx,
        &w,
        args(&["run", "study", "quiz-me", "--note", "chapter 3"]),
        true,
    );
    assert_eq!(code, 0, "{out}");
    let calls = chat.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].2.contains("chapter 3"));
    assert!(
        calls[0].1.contains("cannot post, send, buy"),
        "the house rule goes with every role"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_inbox_reads_text_files_only_and_not_links() {
    let root = tmp("inbox");
    let dir = root.join("Inbox");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    assert_eq!(run::read_inbox(&dir), "");
    std::fs::write(dir.join("a.md"), "first note").unwrap();
    // a key-shaped string, built here so no key-like text sits in the source
    let key = format!("sk-{}", "q".repeat(30));
    std::fs::write(dir.join("b.txt"), format!("second note with {key}")).unwrap();
    std::fs::write(dir.join("c.png"), "no").unwrap();
    std::fs::write(dir.join("sub/d.md"), "deeper").unwrap();
    std::fs::write(dir.join("big.md"), "z".repeat(9000)).unwrap();
    #[cfg(unix)]
    {
        let secret = root.join("outside.md");
        std::fs::write(&secret, "outside the inbox").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("link.md")).unwrap();
    }
    let t = run::read_inbox(&dir);
    assert!(t.contains("--- a ---\nfirst note") && t.contains("second note"));
    assert!(!t.contains("deeper") && !t.contains("outside") && !t.contains(&key));
    assert!(t.contains("[cut]"));
    let _ = std::fs::remove_dir_all(root);
}

// ---- the page

#[test]
fn the_dashboard_holds_nothing_external_and_shows_the_band() {
    let root = tmp("page");
    let ctx = ctx_in(&root, true);
    let band = templates::find("social-media")
        .unwrap()
        .band("social-media", &models());
    let chat = FakeChat::new();
    run::run_flow(
        &ctx,
        &band,
        band.flow("one-post").unwrap(),
        None,
        &chat,
        NOW,
        &mut Vec::new(),
    )
    .unwrap();
    let runs = run::recent_runs(&ctx, &band);
    let views = [page::BandView { band: &band, runs }];
    let html = page::render(&views, &[], &[]);
    // strict policy, no script, nothing fetched
    assert!(html.contains("Content-Security-Policy"));
    assert!(html.contains(
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; base-uri 'none'; form-action 'none'"
    ));
    for external in [
        "http://",
        "https://",
        "<script",
        "<link rel=\"stylesheet\"",
        "src=\"http",
        "url(http",
        "href=\"http",
        "@import",
        "<iframe",
        "<img",
        "<form",
        "onclick",
        "javascript:",
        "//cdn",
    ] {
        assert!(!html.contains(external), "{external}");
    }
    // members as cards with instrument and what they do, flows with their schedule
    for want in [
        "Theremin",
        "Scout",
        "@researcher",
        "Piccolo",
        "@writer",
        "Tuning-fork",
        "Metronome",
        "every Monday at 09:00",
        "only when you run it",
        "Bands/social-media/",
        "tempo-launch conductor run social-media weekly-plan",
    ] {
        assert!(html.contains(want), "{want}");
    }
    // the latest run, with links to its notes (local files only)
    let date = &crate::stamp(NOW)[..10];
    assert!(html.contains(&format!("{date} one-post")), "{html}");
    assert!(
        html.matches("href=\"file://").count() == 3,
        "index and two steps"
    );
    assert!(html.contains("1 writer") && html.contains("2 editor") && html.contains(">Index<"));
    // how to talk to them in Room
    assert!(html.contains("Talk to them in Room"));
    for h in ["researcher", "writer", "editor", "scheduler"] {
        assert!(html.contains(&format!("<code>@{h}</code>")), "{h}");
    }
    // no runs: says so
    let none = page::render(
        &[page::BandView {
            band: &band,
            runs: vec![],
        }],
        &[],
        &[],
    );
    assert!(none.contains("No runs yet"));
    // written into the band's folder and opened (as a local file)
    let (chat2, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat2,
        sched: &sched,
        room_dir: root.join("room"),
    };
    go(
        &ctx,
        &w,
        args(&["new", "--template", "social-media", "--yes"]),
        true,
    );
    let (code, out, _) = go(&ctx, &w, args(&["page", "social-media"]), true);
    assert_eq!(code, 0, "{out}");
    let file = ctx
        .home
        .join("bands")
        .join("social-media")
        .join("band.html");
    assert!(file.is_file());
    assert_eq!(
        ctx.opened.borrow().last().map(String::as_str),
        Some(file.to_string_lossy().as_ref())
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_page_escapes_what_a_spec_says() {
    let mut band = templates::find("study").unwrap().band("study", &models());
    band.members[0].name = "<b>Bold</b>".into();
    band.about = "a & b \"quoted\"".into();
    let html = page::render(
        &[page::BandView {
            band: &band,
            runs: vec![],
        }],
        &[],
        &[],
    );
    assert!(!html.contains("<b>Bold</b>"));
    assert!(html.contains("&lt;b&gt;Bold&lt;/b&gt;"));
    assert!(html.contains("a &amp; b &quot;quoted&quot;"));
    assert_eq!(
        page::file_url(Path::new("/home/a b/#1.md")),
        "file:///home/a%20b/%231.md"
    );
    assert_eq!(
        page::file_url(Path::new("C:\\Tempo\\n.md")),
        "file:///C:/Tempo/n.md"
    );
}

// ---- proposals

fn proposal_json(name: &str) -> J {
    let mut j = templates::find("study")
        .unwrap()
        .band(name, &Models::default())
        .to_j();
    // an AI may say fast or heavy, or leave the model out
    if let J::Obj(o) = &mut j {
        if let Some((_, J::Arr(ms))) = o.iter_mut().find(|(k, _)| k == "members") {
            for (i, m) in ms.iter_mut().enumerate() {
                if let J::Obj(m) = m {
                    m.retain(|(k, _)| k != "model");
                    if i == 0 {
                        m.push(("model".into(), J::s("fast")));
                    }
                    if i == 1 {
                        m.push(("model".into(), J::s("heavy")));
                    }
                }
            }
        }
    }
    j
}

#[test]
fn a_proposal_is_written_and_applied_only_after_a_yes() {
    let root = tmp("proposal");
    let ctx = ctx_in(&root, true);
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("room"),
    };
    // nothing waiting
    let (code, out, _) = go(&ctx, &w, args(&["apply-proposal"]), true);
    assert_eq!(code, 1);
    assert!(out.contains("No band proposal is waiting"));
    // the AI proposes: a file, nothing else
    let name = propose::write(
        &ctx,
        &proposal_json("exam-crew"),
        "Built for your exams.\nGood luck.",
        NOW,
    )
    .unwrap();
    assert_eq!(name, "exam-crew");
    assert_eq!(propose::waiting(&ctx), ["exam-crew"]);
    assert!(!band_dir(&ctx, "exam-crew").join("band.json").exists());
    assert!(sched.calls.borrow().is_empty());
    // the person says no: still nothing, and the proposal stays
    let (code, out, asked) = go(&ctx, &w, args(&["apply-proposal"]), false);
    assert_eq!((code, asked), (0, 1));
    assert!(
        out.contains("Your AI says: Built for your exams. Good luck.")
            && out.contains("not Tempo's"),
        "{out}"
    );
    assert!(out.contains("@planner") && out.contains("Nothing was changed"));
    assert!(!band_dir(&ctx, "exam-crew").join("band.json").exists());
    assert_eq!(propose::waiting(&ctx), ["exam-crew"]);
    // the person says yes: saved with real models, scheduled, the proposal moved aside
    let (code, out, asked) = go(&ctx, &w, args(&["apply-proposal", "exam-crew"]), true);
    assert_eq!((code, asked), (0, 1), "{out}");
    let band = load(&ctx, "exam-crew").unwrap();
    assert_eq!(band.members[0].model, "qwen3:4b", "fast");
    assert_eq!(band.members[1].model, "qwen3:8b", "heavy");
    assert_eq!(
        band.members[2].model, "qwen3:8b",
        "left out: the thinking model"
    );
    assert_eq!(sched.calls.borrow().len(), 1);
    assert!(propose::waiting(&ctx).is_empty());
    assert!(files_under(&propose::proposals_dir(&ctx).join("applied"))
        .iter()
        .any(|p| p.to_string_lossy().contains("exam-crew")));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_bad_proposal_is_refused_with_every_problem_and_a_tampered_file_is_left_alone() {
    let root = tmp("badproposal");
    let ctx = ctx_in(&root, true);
    let mut bad = proposal_json("exam-crew");
    if let J::Obj(o) = &mut bad {
        o.push(("tools".into(), J::strings(&["send_email".to_string()])));
        if let Some((_, J::Arr(fs))) = o.iter_mut().find(|(k, _)| k == "flows") {
            fs[0].set("notes_to", J::s("Documents/"));
        }
    }
    let e = propose::write(&ctx, &bad, "", NOW).unwrap_err();
    assert!(e.contains("\"tools\" is not something a band has"), "{e}");
    assert!(e.contains("notes_to"), "{e}");
    assert!(propose::waiting(&ctx).is_empty());
    assert!(!propose::proposals_dir(&ctx).exists());

    // a proposal edited by hand into something unsafe after it was written
    propose::write(&ctx, &proposal_json("exam-crew"), "", NOW).unwrap();
    let file = propose::proposals_dir(&ctx).join("exam-crew.json");
    let text = std::fs::read_to_string(&file)
        .unwrap()
        .replace("Bands/exam-crew/", "../../outside/");
    std::fs::write(&file, text).unwrap();
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("room"),
    };
    let (code, _, asked) = go(&ctx, &w, args(&["apply-proposal"]), true);
    assert_eq!((code, asked), (1, 0));
    assert!(!band_dir(&ctx, "exam-crew").join("band.json").exists());
    // several waiting: asks which
    propose::write(&ctx, &proposal_json("other-crew"), "", NOW).unwrap();
    let (code, out, _) = go(&ctx, &w, args(&["apply-proposal"]), true);
    assert_eq!(code, 2);
    assert!(out.contains("exam-crew, other-crew"));
    let _ = std::fs::remove_dir_all(root);
}

// ---- Tempo Conductor: the name, and the old name still working

#[test]
fn conductor_and_band_are_the_same_command() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    for rest in [
        &[][..],
        &["templates"],
        &["engines"],
        &["new", "--template", "study", "--yes"],
        &["run", "bakery", "weekly-plan"],
        &["room", "bakery"],
        &["page"],
        &["page", "bakery"],
        &["apply-proposal", "exam-crew"],
    ] {
        let mut new = s(&["conductor"]);
        new.extend(s(rest));
        let mut old = s(&["band"]);
        old.extend(s(rest));
        let (a, b) = (crate::parse(&new), crate::parse(&old));
        assert!(matches!(a, Ok(Cmd::Conductor(_))), "{rest:?}: {a:?}");
        assert_eq!(a, b, "{rest:?}");
    }
    assert!(crate::parse(&s(&["conductor", "dance"])).is_err());
    assert!(crate::parse(&s(&["band", "dance"])).is_err());
    // the help leads with the new name and says the old one works
    assert!(crate::USAGE.contains("Tempo Conductor") && crate::USAGE.contains("`band` works too"));
}

#[test]
fn the_product_is_named_conductor_everywhere_a_person_reads() {
    let root = tmp("names");
    let ctx = ctx_in(&root, true);
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let w = World {
        chat: &chat,
        sched: &sched,
        room_dir: root.join("room"),
    };
    let (_, empty, _) = go(&ctx, &w, ConductorCmd::List, true);
    let (_, made, _) = go(
        &ctx,
        &w,
        args(&["new", "--template", "study", "--yes"]),
        true,
    );
    let (_, tpl, _) = go(&ctx, &w, ConductorCmd::Templates, true);
    for text in [&empty, &made, &tpl] {
        assert!(!text.contains("tempo-launch band"), "{text}");
        assert!(!text.contains("Tempo Band"), "{text}");
    }
    assert!(made.contains("tempo-launch conductor run study"), "{made}");
    // the data stays where it was: bands/<name>/band.json, notes under Bands/<name>/ in the Vault
    assert!(ctx.home.join("bands/study/band.json").is_file());
    let _ = go(&ctx, &w, args(&["run", "study", "weekly-plan"]), true);
    let page = std::fs::read_dir(ctx.vault.join("Bands/study")).is_ok();
    assert!(page);
    let _ = std::fs::remove_dir_all(root);
}

// ---- engines: which there are, and which can be reached

fn tags_body() -> String {
    r#"{"models":[
        {"name":"qwen3:8b","size":5200000000,"details":{"parameter_size":"8.2B"}},
        {"name":"qwen3:4b","size":2600000000,"details":{"parameter_size":"4.0B"}},
        {"name":"nomic-embed-text:latest","size":274000000,"details":{"parameter_size":"137M"}}]}"#
        .to_string()
}

fn engine(id: &str, up: bool, ms: u64, models: &[(&str, f64)]) -> engines::Engine {
    engines::Engine {
        id: id.into(),
        url: format!("http://{id}"),
        up,
        ms,
        models: models
            .iter()
            .map(|(n, b)| engines::Model {
                name: n.to_string(),
                billions: *b,
            })
            .collect(),
        why: if up {
            String::new()
        } else {
            "could not reach it".into()
        },
    }
}

#[test]
fn the_engines_are_the_ones_the_agent_knows() {
    let cfg = parse(
        r#"{"ollama_url":"http://127.0.0.1:11500/","ollama_fallbacks":["http://192.168.0.20:11434","http://127.0.0.1:11500","http://10.0.0.5:11434/"],
            "hosts":{"local":{"urls":["http://127.0.0.1:11500"]},"gpu box":{"urls":["http://10.0.0.9:11434","http://192.168.0.20:11434"]}}}"#,
    )
    .unwrap();
    let specs = engines::configured_with(Some(&cfg), None, 11434);
    let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    // this computer first, then the fallbacks in order, then the hosts; nothing twice
    assert_eq!(
        ids,
        ["local", "192.168.0.20:11434", "10.0.0.5:11434", "gpu-box"]
    );
    assert_eq!(specs[0].url, "http://127.0.0.1:11500");
    assert_eq!(specs[3].url, "http://10.0.0.9:11434");
    // the environment wins over the file, as in the agent
    let env = engines::configured_with(Some(&cfg), Some("http://10.1.1.1:11434"), 11434);
    assert_eq!(env[0].url, "http://10.1.1.1:11434");
    // no settings at all: just this computer's Ollama
    let none = engines::configured_with(None, None, 11434);
    assert_eq!(
        none,
        [engines::Spec {
            id: "local".into(),
            url: "http://127.0.0.1:11434".into()
        }]
    );
    // from the real settings file of a Tempo home
    let root = tmp("configured");
    let ctx = ctx_in(&root, false);
    std::fs::create_dir_all(&ctx.agent_data).unwrap();
    std::fs::write(
        ctx.agent_data.join("config.json"),
        r#"{"ollama_fallbacks":["http://10.0.0.5:11434"]}"#,
    )
    .unwrap();
    let ids = engines::known_ids(&ctx);
    assert!(ids.contains(&"local".to_string()) && ids.contains(&"10.0.0.5:11434".to_string()));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn an_engine_up_one_down_and_a_public_address_refused() {
    let up = Fake::start(|r| match r.path.as_str() {
        "/api/tags" => (200, tags_body()),
        _ => (404, "{}".into()),
    });
    // a port nobody listens on
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let specs = vec![
        engines::Spec {
            id: "local".into(),
            url: format!("http://127.0.0.1:{}", up.port),
        },
        engines::Spec {
            id: "away".into(),
            url: format!("http://127.0.0.1:{dead}"),
        },
        // a public address (a documentation one): never contacted
        engines::Spec {
            id: "public".into(),
            url: "http://203.0.113.7:11434".into(),
        },
    ];
    let started = std::time::Instant::now();
    let seen = engines::probe_all(&specs);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "two seconds each, together"
    );
    assert!(seen[0].up && seen[0].why.is_empty());
    assert_eq!(seen[0].models.len(), 3);
    assert_eq!(seen[0].models[0].name, "qwen3:8b");
    assert_eq!(seen[0].models[0].billions, 8.2);
    assert_eq!(seen[0].models[2].billions, 0.1);
    // embedding models do not make an engine stronger: 8.2B, so capable
    assert_eq!(seen[0].strength(), 8.2);
    assert_eq!(seen[0].class(), engines::Class::Capable);
    assert!(seen[0].has("qwen3:8b") && !seen[0].has("qwen3:70b") && !seen[0].has(""));
    assert!(seen[0].has("nomic-embed-text"), ":latest is implied");
    assert!(
        !seen[1].up && seen[1].why.contains("could not reach"),
        "{:?}",
        seen[1]
    );
    assert!(
        !seen[2].up && seen[2].why.starts_with("refused:"),
        "{:?}",
        seen[2]
    );
    assert!(seen[2].why.contains("not a tailnet or private address"));
    assert_eq!(seen[2].ms, 0);
    // the fake saw one request, from the engine that was up, and it was only the model list
    let asked = up.requests();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        (asked[0].method.as_str(), asked[0].path.as_str()),
        ("GET", "/api/tags")
    );
    assert!(asked[0].body.is_empty());
    // not Ollama, and an error status, and https
    let odd = Fake::start(|_| (200, "{\"hello\":1}".into()));
    let sick = Fake::start(|_| (500, "{}".into()));
    let odd = engines::probe(&engines::Spec {
        id: "odd".into(),
        url: format!("http://127.0.0.1:{}", odd.port),
    });
    let sick = engines::probe(&engines::Spec {
        id: "sick".into(),
        url: format!("http://127.0.0.1:{}", sick.port),
    });
    assert!(!odd.up && odd.why.contains("not like an Ollama"));
    assert!(!sick.up && sick.why.contains("HTTP 500"));
    let tls = engines::probe(&engines::Spec {
        id: "tls".into(),
        url: "https://10.0.0.5:11434".into(),
    });
    assert!(!tls.up && tls.why.contains("https"));
    // a name that is not a private one is refused too, and so is a user name in the address
    for url in [
        "http://example.com:11434",
        "http://u:p@10.0.0.5:11434",
        "http://8.8.8.8",
    ] {
        let e = engines::probe(&engines::Spec {
            id: "x".into(),
            url: url.into(),
        });
        assert!(!e.up && e.why.starts_with("refused:"), "{url}: {e:?}");
    }
}

#[test]
fn sizes_come_from_the_parameters_or_from_the_bytes() {
    let m = engines::models_in(
        r#"{"models":[{"name":"a:1b","details":{"parameter_size":"1.5B"}},{"name":"b:big","size":6000000000},{"name":"c:t","details":{"parameter_size":"1T"}},{"model":"d:x"}]}"#,
    )
    .unwrap();
    let sizes: Vec<f64> = m.iter().map(|x| x.billions).collect();
    assert_eq!(sizes, [1.5, 10.0, 1000.0, 0.0]);
    assert!(engines::models_in("{\"hello\":1}").is_none());
    assert!(engines::models_in("nope").is_none());
    assert_eq!(engines::size_words(0.1), "100M");
    assert_eq!(engines::size_words(8.2), "8.2B");
    assert_eq!(engines::size_words(0.0), "size unknown");
    let class = |b: f64| engine("e", true, 1, &[("m", b)]).class();
    assert_eq!(class(70.0), engines::Class::Strong);
    assert_eq!(class(8.0), engines::Class::Capable);
    assert_eq!(class(3.0), engines::Class::Light);
    assert_eq!(engine("e", true, 1, &[]).class(), engines::Class::Empty);
}

// ---- engines: who plays what

fn team() -> Vec<engines::Engine> {
    vec![
        engine("local", true, 5, &[("qwen3:8b", 8.2), ("qwen3:4b", 4.0)]),
        engine("box", true, 30, &[("qwen3:8b", 8.2), ("llama3:70b", 70.0)]),
        engine("off", false, 0, &[]),
    ]
}

#[test]
fn heavy_parts_go_to_the_strongest_engine_and_light_ones_to_a_quick_one() {
    let mut band = templates::find("small-business")
        .unwrap()
        .band("shop", &models());
    engines::assign(&mut band, &team());
    let on = |h: &str| band.member(h).unwrap().engine.clone();
    // followup is heavy (qwen3:8b): both have it, box is the stronger
    assert_eq!(on("followup"), "box");
    // sorter and billing are light (qwen3:4b): only local has it
    assert_eq!(on("sorter"), "local");
    assert_eq!(on("billing"), "local");
    let mut study = templates::find("study").unwrap().band("study", &models());
    engines::assign(&mut study, &team());
    // the writer-like parts: explainer is heavy, quizzer light
    assert_eq!(study.member("explainer").unwrap().engine, "box");
    assert_eq!(study.member("quizzer").unwrap().engine, "local");
    // a light part whose model both engines have goes to the quicker one
    let mut both = team();
    both[1].models.push(engines::Model {
        name: "qwen3:4b".into(),
        billions: 4.0,
    });
    engines::assign(&mut band, &both);
    assert_eq!(on_of(&band, "sorter"), "local", "5 ms beats 30 ms");
    both[0].ms = 90;
    engines::assign(&mut band, &both);
    assert_eq!(on_of(&band, "sorter"), "box", "now box is quicker");
    // a down engine is never picked, and a model nobody has leaves the member to the agent
    engines::assign(&mut band, &[engine("off", false, 0, &[])]);
    assert!(band.members.iter().all(|m| m.engine.is_empty()));
    let mut odd = templates::find("study").unwrap().band("study", &models());
    odd.members[0].model = "nobody-has-this:1b".into();
    engines::assign(&mut odd, &team());
    assert_eq!(odd.members[0].engine, "");
    // a handle the templates do not know is judged by its model's size
    let mut custom = templates::find("study").unwrap().band("study", &models());
    custom.members[0].handle = "stranger".into();
    custom.members[0].model = "qwen3:4b".into();
    assert_eq!(
        engines::weight(&custom.members[0], &team()),
        engines::Weight::Light
    );
    custom.members[0].model = "llama3:70b".into();
    assert_eq!(
        engines::weight(&custom.members[0], &team()),
        engines::Weight::Heavy
    );
}

fn on_of(band: &Band, h: &str) -> String {
    band.member(h).unwrap().engine.clone()
}

#[test]
fn a_down_engine_falls_back_to_the_next_and_says_so() {
    let mut band = templates::find("small-business")
        .unwrap()
        .band("shop", &models());
    engines::assign(&mut band, &team());
    // everything as planned: no notes
    let cast = engines::cast(&band, &team());
    assert!(cast.iter().all(|c| c.note.is_empty()), "{cast:?}");
    assert_eq!(
        cast.iter().find(|c| c.handle == "followup").unwrap().engine,
        "box"
    );
    // box goes down: local plays the heavy part, and the note names both
    let mut now = team();
    now[1].up = false;
    let cast = engines::cast(&band, &now);
    let f = cast.iter().find(|c| c.handle == "followup").unwrap();
    assert_eq!(f.engine, "local");
    assert_eq!(f.note, "engine box is down, so local played it");
    let s = cast.iter().find(|c| c.handle == "sorter").unwrap();
    assert!(s.note.is_empty() && s.engine == "local");
    // the engine is gone from the settings
    let cast = engines::cast(&band, &now[..1]);
    assert!(cast
        .iter()
        .any(|c| c.note == "engine box is no longer one of yours, so local played it"));
    // the engine is up but lost the model
    let mut lost = team();
    lost[1].models.retain(|m| m.name != "qwen3:8b");
    let cast = engines::cast(&band, &lost);
    assert!(cast
        .iter()
        .any(|c| c.note == "engine box does not have qwen3:8b, so local played it"));
    // nobody can play it
    let cast = engines::cast(&band, &[engine("off", false, 0, &[])]);
    assert!(cast
        .iter()
        .all(|c| c.engine.is_empty() && c.note.contains("no other engine has")));
    // a band with no engine saved just gets the best one, without a note
    for m in &mut band.members {
        m.engine.clear();
    }
    let cast = engines::cast(&band, &team());
    assert!(cast
        .iter()
        .all(|c| c.note.is_empty() && !c.engine.is_empty()));
    let words = engines::cast_words(&cast);
    assert!(words.contains("@followup (") && words.contains("plays on engine box, qwen3:8b"));
}

#[test]
fn the_engine_is_saved_per_member_checked_and_only_the_conductor_picks_it() {
    let root = tmp("engine-saved");
    let ctx = ctx_in(&root, true);
    std::fs::create_dir_all(&ctx.agent_data).unwrap();
    std::fs::write(
        ctx.agent_data.join("config.json"),
        r#"{"ollama_fallbacks":["http://10.0.0.5:11434"]}"#,
    )
    .unwrap();
    let outside = FakeOutside {
        engines: vec![
            engine("local", true, 5, &[("qwen3:8b", 8.2), ("qwen3:4b", 4.0)]),
            engine(
                "10.0.0.5:11434",
                true,
                20,
                &[("qwen3:8b", 8.2), ("big:70b", 70.0)],
            ),
        ],
        apps: vec![],
    };
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let d = Deps {
        chat: &chat,
        outside: &outside,
        room_dir: root.join("room"),
        scheduler: &sched,
        models: models(),
        exe: PathBuf::from("/x/tempo-launch"),
        now: NOW,
    };
    let mut out = Vec::new();
    let code = dispatch(
        &ctx,
        args(&["new", "--template", "small-business", "--yes"]),
        &d,
        &mut out,
        &mut |_| true,
    );
    let out = String::from_utf8(out).unwrap();
    assert_eq!(code, 0, "{out}");
    // shown in the plain words, and kept in band.json
    assert!(out.contains("on engine 10.0.0.5:11434"), "{out}");
    let text = std::fs::read_to_string(ctx.home.join("bands/small-business/band.json")).unwrap();
    assert!(
        text.contains("\"engine\": \"10.0.0.5:11434\"") && text.contains("\"engine\": \"local\"")
    );
    let band = load(&ctx, "small-business").unwrap();
    assert_eq!(band.member("sorter").unwrap().engine, "local");
    assert_eq!(band.member("followup").unwrap().engine, "10.0.0.5:11434");
    // a band.json that names an engine nobody has is held to the rule, in words
    let file = ctx.home.join("bands/small-business/band.json");
    std::fs::write(
        &file,
        text.replacen("\"engine\": \"local\"", "\"engine\": \"ghost\"", 1),
    )
    .unwrap();
    let e = load(&ctx, "small-business").unwrap_err();
    assert!(
        e.contains("engine \"ghost\" is not one of your engines (local, 10.0.0.5:11434)"),
        "{e}"
    );
    assert!(e.contains("tempo-launch link"), "{e}");
    // a badly shaped one too
    std::fs::write(
        &file,
        text.replacen("\"engine\": \"local\"", "\"engine\": \"a b;c\"", 1),
    )
    .unwrap();
    assert!(load(&ctx, "small-business")
        .unwrap_err()
        .contains("the engine is an id"));
    // an AI cannot name an engine in a draft or a proposal: the key is dropped, the Conductor picks
    let mut j = templates::find("study")
        .unwrap()
        .band("exam-crew", &Models::default())
        .to_j();
    if let J::Obj(o) = &mut j {
        if let Some((_, J::Arr(ms))) = o.iter_mut().find(|(k, _)| k == "members") {
            ms[0].set("engine", J::s("local"));
        }
    }
    assert!(spec::from_j(&j).is_ok(), "the file format allows it");
    propose::write(&ctx, &j, "", NOW).unwrap();
    let proposed =
        std::fs::read_to_string(propose::proposals_dir(&ctx).join("exam-crew.json")).unwrap();
    assert!(!proposed.contains("\"engine\""), "{proposed}");
    // with no engine answering, a band is still set up, and the words say why
    let none = FakeOutside::default();
    let d2 = Deps {
        outside: &none,
        ..d
    };
    let mut out = Vec::new();
    dispatch(
        &ctx,
        args(&["new", "--template", "study", "--yes"]),
        &d2,
        &mut out,
        &mut |_| true,
    );
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("No engine answered just now"), "{out}");
    assert!(
        !std::fs::read_to_string(ctx.home.join("bands/study/band.json"))
            .unwrap()
            .contains("\"engine\"")
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_run_checks_the_engines_again_and_the_index_note_says_who_played() {
    let root = tmp("engine-run");
    let ctx = ctx_in(&root, true);
    std::fs::create_dir_all(&ctx.agent_data).unwrap();
    std::fs::write(
        ctx.agent_data.join("config.json"),
        r#"{"ollama_fallbacks":["http://10.0.0.5:11434"]}"#,
    )
    .unwrap();
    let both = |box_up: bool| FakeOutside {
        engines: vec![
            engine("local", true, 5, &[("qwen3:8b", 8.2), ("qwen3:4b", 4.0)]),
            engine(
                "10.0.0.5:11434",
                box_up,
                20,
                &[("qwen3:8b", 8.2), ("big:70b", 70.0)],
            ),
        ],
        apps: vec![],
    };
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let deps = |o: &'static FakeOutside| -> Deps<'_> {
        Deps {
            chat: &chat,
            outside: o,
            room_dir: root.join("room"),
            scheduler: &sched,
            models: models(),
            exe: PathBuf::from("/x/tempo-launch"),
            now: NOW,
        }
    };
    let up: &'static FakeOutside = Box::leak(Box::new(both(true)));
    let down: &'static FakeOutside = Box::leak(Box::new(both(false)));
    dispatch(
        &ctx,
        args(&["new", "--template", "study", "--yes"]),
        &deps(up),
        &mut Vec::new(),
        &mut |_| true,
    );
    let band = load(&ctx, "study").unwrap();
    assert_eq!(band.member("explainer").unwrap().engine, "10.0.0.5:11434");
    // a run with everything up: each part named with its engine, no notes
    let mut out = Vec::new();
    let code = dispatch(
        &ctx,
        args(&["run", "study", "weekly-plan"]),
        &deps(up),
        &mut out,
        &mut |_| true,
    );
    let out = String::from_utf8(out).unwrap();
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("The Conductor raises the baton"), "{out}");
    assert!(
        out.contains("@explainer (") && out.contains("plays on engine 10.0.0.5:11434"),
        "{out}"
    );
    let date = &crate::stamp(NOW)[..10];
    let index = ctx
        .vault
        .join(format!("Bands/study/{date} weekly-plan/Index.md"));
    let text = std::fs::read_to_string(&index).unwrap();
    assert!(
        text.contains("Engines, as the Conductor cast them for this run:"),
        "{text}"
    );
    assert!(text.contains("explainer") && text.contains("10.0.0.5:11434, model qwen3:8b"));
    assert!(!text.contains("Note:"), "{text}");
    // the box goes down: the next run is cast again, local plays, and the note says so
    let mut out = Vec::new();
    let code = dispatch(
        &ctx,
        args(&["run", "study", "weekly-plan"]),
        &deps(down),
        &mut out,
        &mut |_| true,
    );
    let out = String::from_utf8(out).unwrap();
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("engine 10.0.0.5:11434 is down, so local played it"),
        "{out}"
    );
    let text = std::fs::read_to_string(
        ctx.vault
            .join(format!("Bands/study/{date} weekly-plan (2)/Index.md")),
    )
    .unwrap();
    assert!(
        text.contains("Note: engine 10.0.0.5:11434 is down, so local played it"),
        "{text}"
    );
    // the model passed to the agent is the member's own
    assert!(chat
        .calls
        .borrow()
        .iter()
        .all(|(m, _, _)| m == "qwen3:8b" || m == "qwen3:4b"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_run_asks_for_each_members_engine_and_the_index_note_names_who_played() {
    let root = tmp("engine-pin");
    let ctx = ctx_in(&root, true);
    let mut band = templates::find("study").unwrap().band("study", &models());
    let outside = vec![
        engine("local", true, 5, &[("qwen3:8b", 8.2), ("qwen3:4b", 4.0)]),
        engine(
            "10.0.0.5:11434",
            true,
            20,
            &[("qwen3:8b", 8.2), ("big:70b", 70.0)],
        ),
    ];
    engines::assign(&mut band, &outside);
    let cast = engines::cast(&band, &outside);
    let flow = band.flow("weekly-plan").unwrap();
    let go = |chat: &FakeChat| {
        let mut out = Vec::new();
        let done = run::run_flow_cast(&ctx, &band, flow, None, chat, NOW, &cast, &mut out).unwrap();
        std::fs::read_to_string(done.index).unwrap()
    };
    // an agent that pins: each step is asked for its cast engine's address, and the note says
    // which engine answered
    let chat = FakeChat {
        reports: true,
        ..FakeChat::new()
    };
    let text = go(&chat);
    let asked = chat.asked.borrow().clone();
    assert_eq!(asked.len(), flow.steps.len());
    for (handle, url) in flow.steps.iter().zip(&asked) {
        let c = cast.iter().find(|c| &c.handle == handle).unwrap();
        assert_eq!((url, url.is_empty()), (&c.url, false), "{handle}");
    }
    assert!(
        text.contains("played on 10.0.0.5:11434, model qwen3:8b"),
        "{text}"
    );
    assert!(
        !text.contains("The agent decides which engine answers"),
        "{text}"
    );
    // an agent that cannot say: the note keeps today's words
    let text = go(&FakeChat::new());
    assert!(
        text.contains("The agent decides which engine answers"),
        "{text}"
    );
    assert!(!text.contains("played on"), "{text}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_engines_command_lists_each_engine_with_its_models_and_class() {
    let root = tmp("engines-cmd");
    let ctx = ctx_in(&root, false);
    let outside = FakeOutside {
        engines: vec![
            engine("local", true, 5, &[("qwen3:8b", 8.2)]),
            engine("box", true, 30, &[("llama3:70b", 70.0)]),
            engine("away", false, 0, &[]),
        ],
        apps: vec![],
    };
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let d = Deps {
        chat: &chat,
        outside: &outside,
        room_dir: root.join("room"),
        scheduler: &sched,
        models: models(),
        exe: PathBuf::from("/x/tempo-launch"),
        now: NOW,
    };
    let mut out = Vec::new();
    assert_eq!(
        dispatch(&ctx, ConductorCmd::Engines, &d, &mut out, &mut |_| true),
        0
    );
    let out = String::from_utf8(out).unwrap();
    for want in [
        "local",
        "up, 5 ms, capable",
        "qwen3:8b (8.2B)",
        "up, 30 ms, strong",
        "llama3:70b (70B)",
        "away",
        "down: could not reach it",
        "2 of 3 up",
        "tempo-launch link",
    ] {
        assert!(out.contains(want), "{want}\n{out}");
    }
    let _ = std::fs::remove_dir_all(root);
}

// ---- the Conductor page

fn app(title: &str, state: &str, up: bool) -> page::AppState {
    page::AppState {
        title: title.into(),
        state: state.into(),
        detail: String::new(),
        up,
    }
}

#[test]
fn the_conductor_page_shows_engines_bands_runs_and_apps_and_holds_nothing_external() {
    let root = tmp("conductor-page");
    let ctx = ctx_in(&root, true);
    let mut shop = templates::find("small-business")
        .unwrap()
        .band("shop", &models());
    let mut study = templates::find("study").unwrap().band("study", &models());
    let three = vec![
        engine("local", true, 5, &[("qwen3:8b", 8.2), ("qwen3:4b", 4.0)]),
        engine("box", true, 30, &[("llama3:70b", 70.0)]),
        engine("away", false, 0, &[]),
    ];
    engines::assign(&mut shop, &three);
    shop.members[0].engine = "away".into();
    engines::assign(&mut study, &three);
    let chat = FakeChat::new();
    run::run_flow(
        &ctx,
        &study,
        &study.flows[0],
        None,
        &chat,
        NOW,
        &mut Vec::new(),
    )
    .unwrap();
    let apps = vec![
        app("Tempo Agent", "running", true),
        app("Tempo Vault", "stopped", false),
        app("Tempo Room", "running", true),
        app("Tempo Studio", "ready", true),
    ];
    let views = [
        page::BandView {
            runs: run::recent_runs(&ctx, &shop),
            band: &shop,
        },
        page::BandView {
            runs: run::recent_runs(&ctx, &study),
            band: &study,
        },
    ];
    let html = page::render(&views, &three, &apps);
    assert!(
        html.contains("<title>Tempo Conductor</title>")
            && html.contains("<h1>Tempo Conductor</h1>")
    );
    for h in ["Engines", "Bands", "Last runs", "Apps"] {
        assert!(html.contains(&format!("<h2>{h}</h2>")), "{h}");
    }
    // engines: up or down, with the models
    for want in [
        "2 of 3 up",
        "class=\"inst up\">up<",
        "class=\"inst down\">down<",
        "qwen3:8b",
        "llama3:70b",
        "70B",
        "127.0.0.1:11434",
        "could not reach it",
    ] {
        assert!(html.contains(want) || want == "127.0.0.1:11434", "{want}");
    }
    // bands: both of them, members with instrument, role and engine; flows with schedule
    for want in [
        "shop",
        "study",
        "Accordion",
        "@sorter",
        "Plays on engine local",
        "Plays on engine away (down now)",
        "every Friday at 15:00",
        "tempo-launch conductor run study",
    ] {
        assert!(html.contains(want), "{want}");
    }
    // last runs link to the notes in the Vault; apps say what they are doing
    assert!(html.matches("href=\"file://").count() >= 3);
    for want in [
        "Tempo Agent",
        ">stopped<",
        ">running<",
        "Tempo Studio",
        ">ready<",
    ] {
        assert!(html.contains(want), "{want}");
    }
    // strict, static, nothing fetched, and it fits a phone
    assert!(html.contains(
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; base-uri 'none'; form-action 'none'"
    ));
    for external in [
        "http://",
        "https://",
        "<script",
        "<link rel=\"stylesheet\"",
        "src=\"http",
        "url(http",
        "href=\"http",
        "@import",
        "<iframe",
        "<img",
        "<form",
        "onclick",
        "javascript:",
        "//cdn",
        "ftp:",
    ] {
        assert!(!html.contains(external), "{external}");
    }
    assert!(html.contains("name=\"viewport\"") && html.contains("@media (max-width:720px)"));
    assert!(html.contains("minmax(min(230px,100%),1fr)"));
    // the page for one band is titled by the band
    let one = page::render(&views[..1], &three, &apps);
    assert!(one.contains("<title>shop - Tempo Conductor</title>"));
    // what a spec says is escaped, engine names included
    let mut evil = three.clone();
    evil[0].id = "<i>x</i>".into();
    evil[0].url = "http://<i>x</i>:1".into();
    assert!(!page::render(&views, &evil, &apps).contains("<i>x</i>"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn conductor_page_writes_one_page_for_everything_or_one_band() {
    let root = tmp("conductor-page-cmd");
    let ctx = ctx_in(&root, true);
    let outside = FakeOutside {
        engines: vec![
            engine("local", true, 5, &[("qwen3:8b", 8.2), ("qwen3:4b", 4.0)]),
            engine("box", false, 0, &[]),
        ],
        apps: vec![app("Tempo Agent", "running", true)],
    };
    let (chat, sched) = (FakeChat::new(), RecSched::default());
    let d = Deps {
        chat: &chat,
        outside: &outside,
        room_dir: root.join("room"),
        scheduler: &sched,
        models: models(),
        exe: PathBuf::from("/x/tempo-launch"),
        now: NOW,
    };
    for t in ["study", "social-media"] {
        dispatch(
            &ctx,
            args(&["new", "--template", t, "--yes"]),
            &d,
            &mut Vec::new(),
            &mut |_| true,
        );
    }
    // no band named: every band, in <home>/conductor.html
    let mut out = Vec::new();
    assert_eq!(
        dispatch(&ctx, args(&["page"]), &d, &mut out, &mut |_| true),
        0
    );
    let all = ctx.home.join("conductor.html");
    let html = std::fs::read_to_string(&all).unwrap();
    assert!(
        html.contains("study") && html.contains("social-media") && html.contains("Tempo Agent")
    );
    assert_eq!(
        ctx.opened.borrow().last().map(String::as_str),
        Some(all.to_string_lossy().as_ref())
    );
    // one band: where it always went
    assert_eq!(
        dispatch(
            &ctx,
            args(&["page", "study"]),
            &d,
            &mut Vec::new(),
            &mut |_| true
        ),
        0
    );
    let one = std::fs::read_to_string(ctx.home.join("bands/study/band.html")).unwrap();
    assert!(
        one.contains("<title>study - Tempo Conductor</title>") && !one.contains("social-media")
    );
    // no bands at all: the page still shows the engines and says how to start
    let empty = ctx_in(&tmp("conductor-empty"), true);
    assert_eq!(
        dispatch(&empty, args(&["page"]), &d, &mut Vec::new(), &mut |_| true),
        0
    );
    let html = std::fs::read_to_string(empty.home.join("conductor.html")).unwrap();
    assert!(html.contains("No bands yet") && html.contains("<h2>Engines</h2>"));
    // an unknown band is an error
    assert_eq!(
        dispatch(
            &ctx,
            args(&["page", "nope"]),
            &d,
            &mut Vec::new(),
            &mut |_| true
        ),
        1
    );
    let _ = std::fs::remove_dir_all(root);
}
