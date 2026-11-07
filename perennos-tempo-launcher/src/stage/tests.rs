//! Stage's rehearsal: a fake ComfyUI over real HTTP (no GPU here), fake fetchers, and the whole
//! run: machine choice, Pedals into the graph, the API calls, the workbench, takes, resume, MCP.

use super::catalog::{embedded, Catalog};
use super::comfy::{Comfy, HttpComfy};
use super::install::tests::{comfy_folder, inpaint_graph, tiny};
use super::install::workflow_path;
use super::mcp::Server;
use super::route::Machine;
use super::run::{play, Check, RunOpts, World};
use super::state::State;
use super::world::check_media;
use crate::bank::detect::tests::{ctx_in, tmp};
use crate::setup::json::{parse, J};
use crate::Ctx;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- a fake ComfyUI

#[derive(Default)]
pub struct Fake {
    pub gpu_gb: f64,
    pub prompts: Vec<J>,
    pub uploads: Vec<String>,
    pub api_classes: Vec<String>,
    pub unknown_classes: Vec<String>,
    pub cached: usize,
    /// Fail the n-th prompt (1 based) with a refusal.
    pub fail_nth: Option<usize>,
    /// What /view hands back; a PNG unless a test says otherwise.
    pub body: Vec<u8>,
}

pub struct FakeComfy {
    pub port: u16,
    pub st: Arc<Mutex<Fake>>,
    stop: Arc<AtomicBool>,
}

pub fn png() -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
    b.resize(4000, 7);
    b
}

fn respond(s: &mut std::net::TcpStream, code: &str, body: &[u8]) {
    let _ = write!(
        s,
        "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(body);
}

impl FakeComfy {
    pub fn start(gpu_gb: f64) -> FakeComfy {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let st = Arc::new(Mutex::new(Fake {
            gpu_gb,
            body: png(),
            ..Fake::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (st2, stop2) = (st.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in l.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut s) = conn else { continue };
                let st = st2.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head_end, len) = loop {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            break (i + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let body = buf[head_end..].to_vec();
                    let line = head.lines().next().unwrap_or("").to_string();
                    let (method, path) = {
                        let mut p = line.split(' ');
                        (
                            p.next().unwrap_or("").to_string(),
                            p.next().unwrap_or("").to_string(),
                        )
                    };
                    let mut g = st.lock().unwrap();
                    if path == "/system_stats" {
                        let gb = g.gpu_gb * 1e9;
                        respond(&mut s, "200 OK", format!("{{\"system\":{{\"comfyui_version\":\"0.42.0\"}},\"devices\":[{{\"vram_total\":{gb},\"vram_free\":{gb}}}]}}").as_bytes());
                    } else if let Some(c) = path.strip_prefix("/object_info/") {
                        let c = c.to_string();
                        if g.unknown_classes.contains(&c) {
                            respond(&mut s, "200 OK", b"{}");
                        } else {
                            let api = g.api_classes.contains(&c);
                            respond(
                                &mut s,
                                "200 OK",
                                format!("{{\"{c}\":{{\"api_node\":{api}}}}}").as_bytes(),
                            );
                        }
                    } else if path == "/upload/image" && method == "POST" {
                        let text = String::from_utf8_lossy(&body).into_owned();
                        let name = text
                            .split("filename=\"")
                            .nth(1)
                            .and_then(|r| r.split('"').next())
                            .unwrap_or("x")
                            .to_string();
                        g.uploads.push(name.clone());
                        respond(
                            &mut s,
                            "200 OK",
                            format!(
                                "{{\"name\":\"{name}\",\"subfolder\":\"\",\"type\":\"input\"}}"
                            )
                            .as_bytes(),
                        );
                    } else if path == "/prompt" && method == "POST" {
                        let j = parse(&String::from_utf8_lossy(&body)).unwrap_or(J::Null);
                        g.prompts.push(j.get("prompt").cloned().unwrap_or(J::Null));
                        let n = g.prompts.len();
                        if g.fail_nth == Some(n) {
                            respond(&mut s, "400 Bad Request", b"{\"error\":{\"message\":\"Prompt outputs failed validation\"},\"node_errors\":{}}");
                        } else {
                            respond(&mut s, "200 OK", format!("{{\"prompt_id\":\"p{n}\",\"number\":{n},\"node_errors\":{{}}}}").as_bytes());
                        }
                    } else if let Some(id) = path.strip_prefix("/history/") {
                        let nodes: Vec<String> =
                            (0..g.cached).map(|i| format!("\"{i}\"")).collect();
                        respond(
                            &mut s,
                            "200 OK",
                            format!(
                                "{{\"{id}\":{{\"outputs\":{{\"9\":{{\"images\":[{{\"filename\":\"out_{id}.png\",\"subfolder\":\"stage\",\"type\":\"output\"}}]}}}},\"status\":{{\"status_str\":\"success\",\"messages\":[[\"execution_cached\",{{\"nodes\":[{}]}}]]}}}}}}",
                                nodes.join(",")
                            )
                            .as_bytes(),
                        );
                    } else if path.starts_with("/view") {
                        let b = g.body.clone();
                        respond(&mut s, "200 OK", &b);
                    } else {
                        respond(&mut s, "404 Not Found", b"{}");
                    }
                });
            }
        });
        FakeComfy { port, st, stop }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn machine(&self, name: &str, local: bool, gpu: f64) -> Machine {
        Machine {
            name: name.into(),
            local,
            base: self.base(),
            gpu_gb: gpu,
            free_gb: gpu,
            busy: 0.0,
        }
    }
}

impl Drop for FakeComfy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
    }
}

// ---- a world to play in

pub struct FakeWorld {
    pub local: Option<Machine>,
    pub fleet: Vec<Machine>,
    pub roots: Option<Vec<PathBuf>>,
    pub lease: Mutex<Vec<&'static str>>,
    /// This computer's messenger, when the run may use the Fleet.
    pub node: Option<Arc<crate::fleet::msg::Node>>,
    /// How many times a ComfyUI was asked for by address (a Fleet machine never is).
    pub comfy_calls: std::sync::atomic::AtomicUsize,
}

impl World for FakeWorld {
    fn now(&self) -> u64 {
        1_700_000_000
    }
    fn random(&self) -> i64 {
        7
    }
    fn local(&self) -> Option<Machine> {
        self.local.clone()
    }
    fn fleet(&self, _: &str) -> Vec<Machine> {
        self.fleet.clone()
    }
    fn comfy(&self, base: &str) -> Result<Box<dyn Comfy>, String> {
        self.comfy_calls.fetch_add(1, Ordering::SeqCst);
        let mut c = HttpComfy::new(base)?;
        c.poll = Duration::from_millis(5);
        Ok(Box::new(c))
    }
    fn lease_begin(&self) -> Result<(), String> {
        self.lease.lock().unwrap().push("begin");
        Ok(())
    }
    fn lease_end(&self) {
        self.lease.lock().unwrap().push("end");
    }
    fn check(&self, file: &Path, kind: &str) -> Check {
        check_media(file, kind, None)
    }
    fn roots(&self) -> Option<Vec<PathBuf>> {
        self.roots.clone()
    }
    fn send_inputs(
        &self,
        machine: &str,
        files: &[(String, PathBuf)],
    ) -> Result<Vec<(String, crate::fleet::blob::Sent)>, String> {
        let n = self.node.as_ref().ok_or("no messenger in this world")?;
        crate::fleet::stage::send_inputs(n, machine, files)
    }
    fn remote_take(
        &self,
        machine: &str,
        workflow: &str,
        inputs: &[(String, crate::fleet::stage::Input)],
        pedals: &[(String, String)],
        seed: i64,
    ) -> Result<crate::fleet::stage::Done, String> {
        let n = self.node.as_ref().ok_or("no messenger in this world")?;
        crate::fleet::stage::call(
            n,
            machine,
            workflow,
            inputs,
            pedals,
            Some(seed),
            Duration::from_secs(60),
        )
    }
}

pub fn world(local: Option<Machine>, fleet: Vec<Machine>) -> FakeWorld {
    FakeWorld {
        local,
        fleet,
        roots: None,
        lease: Mutex::new(vec![]),
        node: None,
        comfy_calls: Default::default(),
    }
}

// ---- another computer, through the Fleet messenger

const KEY: [u8; 32] = [7; 32];

/// What the other computer does when asked: its own Stage, its own home, its own ComfyUI.
struct StudioRunner {
    home: PathBuf,
    world: Arc<FakeWorld>,
    cat: Catalog,
}

impl crate::fleet::stage::StageRunner for StudioRunner {
    fn ready(&self, workflow: &str) -> bool {
        State::load(&ctx_in(&self.home)).enabled(workflow)
    }
    fn play(&self, p: &crate::fleet::stage::Play) -> Result<crate::fleet::stage::Played, String> {
        super::remote::play_for_fleet(&ctx_in(&self.home), &*self.world, &self.cat, p)
    }
}

/// Two messenger nodes on this computer: `a` plays the run, `studio` is another computer with
/// its own home, its own ComfyUI (a fake on 127.0.0.1, behind the messenger and nowhere else)
/// and its own GPU lease.
pub struct Studio {
    pub a: crate::fleet::tests::Rig,
    pub b: crate::fleet::tests::Rig,
    pub ctx_b: Ctx,
    pub world_b: Arc<FakeWorld>,
}

impl Studio {
    pub fn new(tag: &str, fake: &FakeComfy, gpu: f64, allow: bool, cat: &Catalog) -> Studio {
        use crate::fleet::{blob, stage as fs, store, tests as ft};
        let a = ft::rig(&format!("lead-{tag}"), KEY);
        let b = ft::rig("studio", KEY);
        ft::link(&a, &b);
        if allow {
            store::update(&b.paths, |f| f.allow.push("stage".into())).unwrap();
        }
        blob::attach(&a.node);
        blob::attach(&b.node);
        let (ctx_b, _, _) = ready(&format!("{tag}-studio"));
        let world_b = Arc::new(world(
            Some(fake.machine("this computer", true, gpu)),
            vec![],
        ));
        crate::fleet::msg::lock(&b.node.handlers).push(Arc::new(fs::StageHandler::new(
            b.paths.clone(),
            Arc::new(StudioRunner {
                home: ctx_b.home.clone(),
                world: world_b.clone(),
                cat: cat.clone(),
            }),
        )));
        Studio {
            a,
            b,
            ctx_b,
            world_b,
        }
    }

    /// The studio as the lead computer's Fleet view reports it.
    pub fn machine(&self, gpu: f64) -> Machine {
        Machine {
            name: "studio".into(),
            local: false,
            base: String::new(),
            gpu_gb: gpu,
            free_gb: gpu,
            busy: 0.0,
        }
    }

    /// A world for the lead computer: it may use the Fleet, and has `local` if given.
    pub fn lead(&self, local: Option<Machine>, gpu: f64) -> FakeWorld {
        FakeWorld {
            node: Some(self.a.node.clone()),
            ..world(local, vec![self.machine(gpu)])
        }
    }
}

/// A home with paint-edit-photo added and on: its workflow file in place, its models in place.
pub fn ready(tag: &str) -> (Ctx, Catalog, PathBuf) {
    let d = tmp(tag);
    let ctx = ctx_in(&d.join("home"));
    std::fs::create_dir_all(&ctx.home).unwrap();
    let comfy = comfy_folder(&d);
    let (mut cat, _) = tiny(inpaint_graph().as_bytes(), false);
    cat.workflows[0].models.clear();
    let p = workflow_path(&ctx, "paint-edit-photo");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, inpaint_graph()).unwrap();
    let mut st = State::load(&ctx);
    st.set_folder(&comfy.to_string_lossy());
    st.set(
        "paint-edit-photo",
        vec![("installed", J::Bool(true)), ("enabled", J::Bool(true))],
    );
    st.save(&ctx).unwrap();
    (ctx, cat, d)
}

pub fn pics(d: &Path) -> (PathBuf, PathBuf) {
    let (a, b) = (d.join("photo.png"), d.join("mask.png"));
    std::fs::write(&a, png()).unwrap();
    std::fs::write(&b, png()).unwrap();
    (a, b)
}

fn opts(d: &Path, extra: &[(&str, &str)]) -> RunOpts {
    let (a, b) = pics(d);
    RunOpts {
        inputs: vec![
            ("image".into(), a.to_string_lossy().into_owned()),
            ("mask".into(), b.to_string_lossy().into_owned()),
            ("instruction".into(), "a red leather jacket".into()),
        ],
        pedals: extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        takes: 1,
        ..RunOpts::default()
    }
}

fn widget(g: &J, node: &str, field: &str) -> J {
    g.get(node)
        .unwrap()
        .get("inputs")
        .unwrap()
        .get(field)
        .unwrap()
        .clone()
}

// ---- the run

#[test]
fn a_run_goes_through_the_api_and_lands_in_the_workbench_with_a_sidecar() {
    let (ctx, cat, d) = ready("run-ok");
    let fake = FakeComfy::start(24.0);
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    let out = play(
        &ctx,
        &w,
        &cat,
        "paint-edit-photo",
        &opts(&d, &[("strength", "20")]),
    )
    .unwrap();
    assert!(
        out.status.starts_with("Edited on this computer, "),
        "{}",
        out.status
    );
    let g = fake.st.lock().unwrap();
    assert_eq!(g.uploads.len(), 2, "both pictures went up: {:?}", g.uploads);
    assert!(g.uploads.iter().all(|n| n.starts_with("stage_")));
    // the Pedal and the inputs landed on the widgets the catalogue names
    let sent = &g.prompts[0];
    assert_eq!(widget(sent, "7", "steps"), J::n(20.0));
    assert_eq!(widget(sent, "7", "seed"), J::n(7.0));
    match widget(sent, "4", "text") {
        J::Str(t) => assert!(t.contains("a red leather jacket"), "{t}"),
        _ => panic!(),
    }
    // the output is in the workbench, with the sidecar that says where it came from
    let file = &out.takes[0].files[0];
    assert!(file.starts_with(super::run::workbench(&ctx)) && file.is_file());
    let side =
        parse(&std::fs::read_to_string(format!("{}.stage.json", file.display())).unwrap()).unwrap();
    assert_eq!(
        side.get("workflow").unwrap().str("file_sha256"),
        Some(cat.workflows[0].file_sha256.as_str())
    );
    assert_eq!(
        side.get("workflow").unwrap().str("commit"),
        Some(cat.workflows[0].commit.as_str())
    );
    assert_eq!(side.get("pedals").unwrap().str("strength"), Some("20"));
    assert_eq!(side.f64("seed"), Some(7.0));
    assert_eq!(
        side.get("machine").unwrap().str("name"),
        Some("this computer")
    );
    assert!(side.get("output").unwrap().str("sha256").unwrap().len() == 64);
    assert!(side.str("make_it_again").unwrap().contains("--again"));
    assert!(side
        .arr("inputs")
        .iter()
        .any(|i| i.str("name") == Some("image") && i.str("sha256").is_some()));
    // the graphics card was taken and given back
    assert_eq!(*w.lease.lock().unwrap(), ["begin", "end"]);
}

#[test]
fn an_output_never_replaces_a_file() {
    let (ctx, cat, d) = ready("run-nooverwrite");
    let fake = FakeComfy::start(24.0);
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    let first = play(
        &ctx,
        &w,
        &cat,
        "paint-edit-photo",
        &opts(&d, &[("strength", "20")]),
    )
    .unwrap();
    let f1 = first.takes[0].files[0].clone();
    std::fs::write(&f1, b"MINE, do not touch").unwrap();
    // a different Pedal is a different run, on the same seed and the same second: the name is taken
    let second = play(
        &ctx,
        &w,
        &cat,
        "paint-edit-photo",
        &opts(&d, &[("strength", "21")]),
    )
    .unwrap();
    let f2 = second.takes[0].files[0].clone();
    assert_ne!(f1, f2);
    assert!(f2.to_string_lossy().ends_with("-2.png"), "{}", f2.display());
    assert_eq!(std::fs::read(&f1).unwrap(), b"MINE, do not touch");
    // and its sidecar is new too
    assert!(Path::new(&format!("{}.stage.json", f2.display())).is_file());
}

#[test]
fn a_machine_with_too_little_graphics_memory_is_not_used() {
    let (ctx, cat, d) = ready("run-route");
    let small = FakeComfy::start(8.0);
    let big = FakeComfy::start(16.0);
    let studio = Studio::new("route", &big, 16.0, true, &cat);
    let w = studio.lead(Some(small.machine("this computer", true, 8.0)), 16.0);
    let out = play(&ctx, &w, &cat, "paint-edit-photo", &opts(&d, &[])).unwrap();
    assert!(
        out.status.starts_with("Edited on the studio box, "),
        "{}",
        out.status
    );
    assert!(small.st.lock().unwrap().prompts.is_empty());
    assert_eq!(big.st.lock().unwrap().prompts.len(), 1);
    // the owner can pin a machine, and a machine that does not fit is said to
    let only_small = world(Some(small.machine("this computer", true, 8.0)), vec![]);
    let e = play(&ctx, &only_small, &cat, "paint-edit-photo", &opts(&d, &[])).unwrap_err();
    assert!(
        e.contains("will not fit anywhere") && e.contains("8 GB") && e.contains("12 GB"),
        "{e}"
    );
    let mut o = opts(&d, &[]);
    o.on = Some("nowhere".into());
    assert!(play(&ctx, &w, &cat, "paint-edit-photo", &o)
        .unwrap_err()
        .contains("not a machine"));
}

#[test]
fn takes_are_spread_over_the_fleets_cards_and_the_one_that_passes_the_check_wins() {
    let (ctx, cat, d) = ready("run-takes");
    let a = FakeComfy::start(24.0);
    let b = FakeComfy::start(16.0);
    // the studio box makes a broken file
    b.st.lock().unwrap().body = vec![b'x'; 5000];
    let studio = Studio::new("takes", &b, 16.0, true, &cat);
    let w = studio.lead(Some(a.machine("this computer", true, 24.0)), 16.0);
    let mut o = opts(&d, &[]);
    o.takes = 2;
    let out = play(&ctx, &w, &cat, "paint-edit-photo", &o).unwrap();
    assert_eq!(a.st.lock().unwrap().prompts.len(), 1);
    assert_eq!(b.st.lock().unwrap().prompts.len(), 1);
    assert_eq!(out.takes.len(), 2);
    assert!(
        out.status.contains("2 takes")
            && out.status.contains("this computer")
            && out.status.contains("the studio box"),
        "{}",
        out.status
    );
    // take 1 ran here and is a good picture; take 2 is not
    let good = out
        .takes
        .iter()
        .position(|t| t.check.ok == Some(true))
        .unwrap();
    assert_eq!(out.winner, Some(good));
    assert_eq!(out.takes[good].machine, "this computer");
    // different seeds for different takes
    assert_ne!(out.takes[0].seed, out.takes[1].seed);
    // the takes file, like any other part
    let lines = crate::conductor::takes_file::read_all(&crate::conductor::takes_file::path(&ctx));
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().filter(|l| l.winner).count() == 1);
    assert!(lines
        .iter()
        .all(|l| l.part.as_deref() == Some("stage") && l.engine == "comfyui"));
}

#[test]
fn a_run_that_stopped_partway_picks_up_where_it_was() {
    let (ctx, cat, d) = ready("run-resume");
    let fake = FakeComfy::start(24.0);
    fake.st.lock().unwrap().fail_nth = Some(2);
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    let mut o = opts(&d, &[]);
    o.takes = 3;
    let first = play(&ctx, &w, &cat, "paint-edit-photo", &o).unwrap();
    assert_eq!(first.takes.len(), 2, "take 2 failed");
    assert!(
        first
            .notes
            .iter()
            .any(|n| n.contains("a take failed") && n.contains("failed validation")),
        "{:?}",
        first.notes
    );
    let before = fake.st.lock().unwrap().prompts.len();
    // the same command again: only the missing take is played
    fake.st.lock().unwrap().fail_nth = None;
    let second = play(&ctx, &w, &cat, "paint-edit-photo", &o).unwrap();
    assert_eq!(fake.st.lock().unwrap().prompts.len(), before + 1);
    assert_eq!(second.takes.len(), 3);
    assert_eq!(second.takes.iter().filter(|t| t.resumed).count(), 2);
    // seeds are the same ones the first go chose
    assert_eq!(first.takes[0].seed, second.takes[0].seed);
    // and a third time, everything is done and nothing is sent
    let n = fake.st.lock().unwrap().prompts.len();
    let third = play(&ctx, &w, &cat, "paint-edit-photo", &o).unwrap();
    assert_eq!(fake.st.lock().unwrap().prompts.len(), n);
    assert!(third.status.starts_with("Already done"), "{}", third.status);
}

#[test]
fn again_keeps_the_seed_changes_a_pedal_and_says_what_comfyui_reused() {
    let (ctx, cat, d) = ready("run-again");
    let fake = FakeComfy::start(24.0);
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    play(
        &ctx,
        &w,
        &cat,
        "paint-edit-photo",
        &opts(&d, &[("strength", "16")]),
    )
    .unwrap();
    fake.st.lock().unwrap().cached = 3;
    let again = RunOpts {
        again: true,
        pedals: vec![("strength".into(), "22".into())],
        takes: 1,
        ..RunOpts::default()
    };
    let out = play(&ctx, &w, &cat, "paint-edit-photo", &again).unwrap();
    let g = fake.st.lock().unwrap();
    assert_eq!(g.prompts.len(), 2);
    assert_eq!(
        widget(&g.prompts[1], "7", "seed"),
        widget(&g.prompts[0], "7", "seed"),
        "the same seed, so the unchanged work can be reused"
    );
    assert_eq!(widget(&g.prompts[1], "7", "steps"), J::n(22.0));
    // the inputs came from the last run: both pictures went up again
    assert!(widget(&g.prompts[1], "11", "image") == widget(&g.prompts[0], "11", "image"));
    assert!(out.status.contains("reused 3 nodes"), "{}", out.status);
    // nothing to repeat for a workflow that never ran
    let (ctx2, cat2, _) = ready("run-again-none");
    let e = play(&ctx2, &w, &cat2, "paint-edit-photo", &again).unwrap_err();
    assert!(e.contains("no earlier run"), "{e}");
}

#[test]
fn a_partner_node_is_refused_unless_the_owner_turned_it_on_for_that_workflow() {
    let (ctx, cat, d) = ready("run-api");
    let fake = FakeComfy::start(24.0);
    fake.st.lock().unwrap().api_classes = vec!["KSampler".into()];
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    let e = play(&ctx, &w, &cat, "paint-edit-photo", &opts(&d, &[])).unwrap_err();
    assert!(
        e.contains("refused")
            && e.contains("KSampler")
            && e.contains("paid or cloud service")
            && e.contains("stage cloud paint-edit-photo on"),
        "{e}"
    );
    assert!(
        fake.st.lock().unwrap().prompts.is_empty(),
        "nothing was sent"
    );
    let mut st = State::load(&ctx);
    st.set("paint-edit-photo", vec![("allow_cloud", J::Bool(true))]);
    st.save(&ctx).unwrap();
    assert!(play(&ctx, &w, &cat, "paint-edit-photo", &opts(&d, &[])).is_ok());
    // a graph that names a partner node itself is refused before ComfyUI is asked
    let (ctx, mut cat, d) = ready("run-api-static");
    let paid = inpaint_graph().replace(
        "\"9\":{",
        "\"2\":{\"class_type\":\"OpenAIGPTImage1\",\"inputs\":{}},\"9\":{",
    );
    std::fs::write(workflow_path(&ctx, "paint-edit-photo"), &paid).unwrap();
    cat.workflows[0].file_sha256 = crate::setup::sha256::sha256_hex(paid.as_bytes());
    let e = play(&ctx, &w, &cat, "paint-edit-photo", &opts(&d, &[])).unwrap_err();
    assert!(
        e.contains("OpenAIGPTImage1") && e.contains("paid or cloud service"),
        "{e}"
    );
}

#[test]
fn a_machine_that_lacks_a_node_class_sits_out() {
    let (ctx, cat, d) = ready("run-missingnode");
    let a = FakeComfy::start(24.0);
    let b = FakeComfy::start(24.0);
    a.st.lock().unwrap().unknown_classes = vec!["KSampler".into()];
    let studio = Studio::new("missingnode", &b, 24.0, true, &cat);
    let w = studio.lead(Some(a.machine("this computer", true, 24.0)), 24.0);
    let mut o = opts(&d, &[]);
    o.takes = 2;
    let out = play(&ctx, &w, &cat, "paint-edit-photo", &o).unwrap();
    assert!(
        out.notes
            .iter()
            .any(|n| n.contains("this computer does not have the node(s) KSampler")),
        "{:?}",
        out.notes
    );
    assert!(a.st.lock().unwrap().prompts.is_empty());
    assert!(out.takes.iter().all(|t| t.machine == "studio"));
}

#[test]
fn a_changed_workflow_file_stops_the_run_before_anything_is_sent() {
    let (ctx, cat, d) = ready("run-tamper");
    let fake = FakeComfy::start(24.0);
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    std::fs::write(workflow_path(&ctx, "paint-edit-photo"), "{}").unwrap();
    let e = play(&ctx, &w, &cat, "paint-edit-photo", &opts(&d, &[])).unwrap_err();
    assert!(e.contains("no longer matches its pinned SHA-256"), "{e}");
    assert!(fake.st.lock().unwrap().prompts.is_empty());
}

#[test]
fn bad_pedals_and_inputs_are_refused_in_words_before_the_card_is_touched() {
    let (ctx, cat, d) = ready("run-badpedal");
    let fake = FakeComfy::start(24.0);
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    let e = play(
        &ctx,
        &w,
        &cat,
        "paint-edit-photo",
        &opts(&d, &[("strength", "99")]),
    )
    .unwrap_err();
    assert!(e.contains("8 to 30") && e.contains("not 99"), "{e}");
    let e = play(
        &ctx,
        &w,
        &cat,
        "paint-edit-photo",
        &opts(&d, &[("camera", "orbit_left")]),
    )
    .unwrap_err();
    assert!(e.contains("no \"camera\" Pedal"), "{e}");
    let mut o = opts(&d, &[]);
    o.inputs.retain(|(k, _)| k != "mask");
    assert!(play(&ctx, &w, &cat, "paint-edit-photo", &o)
        .unwrap_err()
        .contains("needs the input \"mask\""));
    o.inputs.push((
        "mask".into(),
        d.join("nope.png").to_string_lossy().into_owned(),
    ));
    assert!(play(&ctx, &w, &cat, "paint-edit-photo", &o)
        .unwrap_err()
        .contains("is not a file that exists"));
    assert!(fake.st.lock().unwrap().prompts.is_empty());
    assert!(w.lease.lock().unwrap().is_empty());
}

#[test]
fn a_finished_render_leaves_a_vault_note_and_remembers_itself() {
    let (ctx, cat, d) = ready("run-vault");
    std::fs::create_dir_all(&ctx.vault).unwrap();
    let fake = FakeComfy::start(24.0);
    let w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    play(
        &ctx,
        &w,
        &cat,
        "paint-edit-photo",
        &opts(&d, &[("strength", "18")]),
    )
    .unwrap();
    let notes: Vec<_> = std::fs::read_dir(ctx.vault.join("Stage"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(notes.len(), 1);
    let text = std::fs::read_to_string(notes[0].path()).unwrap();
    assert!(
        text.contains("workflow: paint-edit-photo")
            && text.contains("seed: 7")
            && text.contains("strength=18")
            && text.contains("--again"),
        "{text}"
    );
    let st = State::load(&ctx);
    assert_eq!(st.last_workflow().as_deref(), Some("paint-edit-photo"));
    assert!(st.last_run("paint-edit-photo").is_some());
    // no Vault folder, no note, and no new folder made
    let (ctx2, cat2, d2) = ready("run-novault");
    play(&ctx2, &w, &cat2, "paint-edit-photo", &opts(&d2, &[])).unwrap();
    assert!(!ctx2.vault.exists());
}

// ---- the MCP server

fn rpc(s: &Server, line: &str) -> J {
    let mut out = Vec::new();
    s.run(std::io::Cursor::new(format!("{line}\n")), &mut out)
        .unwrap();
    parse(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap()
}

#[test]
fn the_tool_list_is_every_workflow_that_is_on_with_typed_pedals() {
    let (ctx, _, d) = ready("mcp-list");
    let cat = embedded();
    let w = world(None, vec![]);
    let s = Server {
        ctx: &ctx,
        world: &w,
        cat: &cat,
    };
    // the shipped catalogue's paint-edit-photo is on (the test state says so)
    let r = rpc(&s, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tools = r.get("result").unwrap().arr("tools");
    let names: Vec<&str> = tools.iter().filter_map(|t| t.str("name")).collect();
    assert_eq!(
        names,
        ["stage_paint_edit_photo", "stage_again", "stage_list"]
    );
    let t = &tools[0];
    let props = t.get("inputSchema").unwrap().get("properties").unwrap();
    let strength = props.get("strength").unwrap();
    assert_eq!(strength.str("type"), Some("number"));
    assert_eq!(strength.f64("minimum"), Some(8.0));
    assert_eq!(strength.f64("maximum"), Some(30.0));
    assert_eq!(props.get("seed").unwrap().str("type"), Some("integer"));
    assert_eq!(props.get("takes").unwrap().f64("maximum"), Some(12.0));
    let req: Vec<String> = t.get("inputSchema").unwrap().strs("required");
    assert_eq!(req, ["image", "mask"]);
    assert_eq!(
        t.get("inputSchema").unwrap().bool("additionalProperties"),
        Some(false)
    );
    assert_eq!(
        t.get("annotations").unwrap().bool("destructiveHint"),
        Some(false)
    );
    // turned off: nothing is offered
    let mut st = State::load(&ctx);
    st.set("paint-edit-photo", vec![("enabled", J::Bool(false))]);
    st.save(&ctx).unwrap();
    let r = rpc(&s, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert!(r.get("result").unwrap().arr("tools").is_empty());
    let r = rpc(
        &s,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"stage_paint_edit_photo","arguments":{}}}"#,
    );
    assert_eq!(r.get("result").unwrap().bool("isError"), Some(true));
    let _ = d;
    // every workflow in the shipped catalogue makes a tool with a valid schema
    for wf in &cat.workflows {
        let j = super::mcp::tool_json(&cat, wf);
        assert_eq!(
            j.str("name"),
            Some(super::catalog::tool_name(&wf.id).as_str())
        );
        assert!(parse(&j.compact()).is_ok());
        for p in &wf.pedals {
            assert!(
                j.get("inputSchema")
                    .unwrap()
                    .get("properties")
                    .unwrap()
                    .get(&p.name)
                    .is_some(),
                "{} {}",
                wf.id,
                p.name
            );
        }
    }
}

#[test]
fn a_tool_call_plays_the_workflow_and_only_on_folders_the_owner_allowed() {
    let (ctx, _, d) = ready("mcp-call");
    let cat = embedded();
    let fake = FakeComfy::start(24.0);
    let (a, b) = pics(&d);
    let mut w = world(Some(fake.machine("this computer", true, 24.0)), vec![]);
    // the shipped catalogue's workflow hash is not the test graph's, so use the tiny one
    let (tcat, _) = {
        let (mut c, s) = tiny(inpaint_graph().as_bytes(), false);
        c.workflows[0].models.clear();
        (c, s)
    };
    let _ = cat;
    w.roots = Some(vec![std::fs::canonicalize(&d).unwrap()]);
    let s = Server {
        ctx: &ctx,
        world: &w,
        cat: &tcat,
    };
    let call = |args: &str| {
        rpc(
            &s,
            &format!(
                r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"stage_paint_edit_photo","arguments":{args}}}}}"#
            ),
        )
    };
    let r = call(&format!(
        r#"{{"image":"{}","mask":"{}","instruction":"a blue coat","strength":12}}"#,
        a.display().to_string().replace('\\', "\\\\"),
        b.display().to_string().replace('\\', "\\\\")
    ));
    let res = r.get("result").unwrap();
    assert_eq!(res.bool("isError"), Some(false), "{}", r.compact());
    let text = res.arr("content")[0].str("text").unwrap().to_string();
    assert!(
        text.starts_with("Edited on this computer, ")
            && text.contains("Take 1 (seed 7, passed the check")
            && text.contains("Nothing was replaced"),
        "{text}"
    );
    assert_eq!(
        widget(&fake.st.lock().unwrap().prompts[0], "7", "steps"),
        J::n(12.0)
    );
    // a path outside the allowed folders
    let outside = tmp("mcp-outside").join("x.png");
    std::fs::write(&outside, png()).unwrap();
    let r = call(&format!(
        r#"{{"image":"{}","mask":"{}","instruction":"x"}}"#,
        outside.display().to_string().replace('\\', "\\\\"),
        b.display().to_string().replace('\\', "\\\\")
    ));
    assert_eq!(r.get("result").unwrap().bool("isError"), Some(true));
    assert!(
        r.compact().contains("not in a folder you allowed"),
        "{}",
        r.compact()
    );
    // a value out of range, and a name that is not there
    let r = call(r#"{"strength":500}"#);
    assert!(r.get("result").unwrap().bool("isError") == Some(true));
    let r = call(r#"{"bogus":1}"#);
    assert!(
        r.compact().contains("has no \\\"bogus\\\""),
        "{}",
        r.compact()
    );
    // "make it ..." is a Pedal change on the last run
    let r = rpc(
        &s,
        r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"stage_again","arguments":{"pedals":{"strength":"25"}}}}"#,
    );
    assert_eq!(
        r.get("result").unwrap().bool("isError"),
        Some(false),
        "{}",
        r.compact()
    );
    let g = fake.st.lock().unwrap();
    assert_eq!(widget(g.prompts.last().unwrap(), "7", "steps"), J::n(25.0));
    assert_eq!(
        widget(g.prompts.last().unwrap(), "7", "seed"),
        widget(&g.prompts[0], "7", "seed")
    );
    drop(g);
    let r = rpc(
        &s,
        r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"stage_list","arguments":{}}}"#,
    );
    assert!(
        r.compact().contains("stage_paint_edit_photo"),
        "{}",
        r.compact()
    );
    // the server speaks the protocol
    let r = rpc(
        &s,
        r#"{"jsonrpc":"2.0","id":12,"method":"initialize","params":{}}"#,
    );
    assert_eq!(
        r.get("result")
            .unwrap()
            .get("serverInfo")
            .unwrap()
            .str("name"),
        Some("tempo-stage")
    );
    assert!(rpc(&s, r#"{"jsonrpc":"2.0","id":13,"method":"nope"}"#)
        .get("error")
        .is_some());
}

// ---- the commands

#[test]
fn the_command_line_is_parsed() {
    use super::{parse_args as p, AddOpts, RunSpec, StageCmd as C};
    assert_eq!(p(&[]), Ok(C::List));
    assert_eq!(
        p(&["show", "relight-photo"]),
        Ok(C::Show("relight-photo".into()))
    );
    assert_eq!(p(&["check"]), Ok(C::Check(None)));
    assert_eq!(
        p(&["add", "effect-clip", "--yes", "--with", "effect=crush"]),
        Ok(C::Add(
            "effect-clip".into(),
            AddOpts {
                yes: true,
                with: vec![("effect".into(), "crush".into())]
            }
        ))
    );
    assert_eq!(
        p(&["remove", "effect-clip"]),
        Ok(C::Remove("effect-clip".into(), false))
    );
    assert_eq!(
        p(&["cloud", "effect-clip", "on"]),
        Ok(C::Cloud("effect-clip".into(), true))
    );
    assert_eq!(
        p(&[
            "run",
            "re-angle-video",
            "--input",
            "video=a.mp4",
            "--pedal",
            "camera=orbit_left",
            "--pedal",
            "angle=15",
            "--takes",
            "3",
            "--on",
            "studio"
        ]),
        Ok(C::Run(
            "re-angle-video".into(),
            RunSpec {
                inputs: vec![("video".into(), "a.mp4".into())],
                pedals: vec![
                    ("camera".into(), "orbit_left".into()),
                    ("angle".into(), "15".into())
                ],
                takes: 3,
                on: Some("studio".into()),
                ..RunSpec::default()
            }
        ))
    );
    assert_eq!(
        p(&["run", "x", "--again"]).map(|c| matches!(c, C::Run(_, s) if s.again)),
        Ok(true)
    );
    assert!(p(&["run", "x", "--takes", "99"]).is_err());
    assert!(p(&["run", "x", "--pedal", "nope"])
        .unwrap_err()
        .contains("NAME=VALUE"));
    assert!(p(&["add"]).is_err() && p(&["frobnicate"]).is_err() && p(&["cloud", "x"]).is_err());
}

fn go(
    ctx: &Ctx,
    cat: &Catalog,
    w: &dyn World,
    cmd: super::StageCmd,
    answers: &[&str],
) -> (i32, String) {
    use crate::bank::install::RealPm;
    let sys = crate::bank::detect::Sys::real();
    let pm = RealPm(&sys);
    let sc = super::guard::tests::FakeSoundcheck(Ok(vec![]));
    let host = super::install::Host {
        pm: &pm,
        os: "linux",
        sc: &sc,
        token: None,
    };
    let mut out = Vec::new();
    let mut it = answers
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .into_iter();
    let code = super::run_with(
        ctx,
        &host,
        w,
        cat,
        cmd,
        1_700_000_000,
        &mut out,
        &mut |_| it.next().unwrap_or_default(),
    );
    (code, String::from_utf8(out).unwrap())
}

#[test]
fn list_show_and_check_say_what_there_is_and_where_it_fits() {
    use super::StageCmd as C;
    let cat = embedded();
    let ctx = ctx_in(&tmp("cmd-list"));
    let w = world(None, vec![]);
    let (code, out) = go(&ctx, &cat, &w, C::List, &[]);
    assert_eq!(code, 0);
    assert!(
        out.contains("Stage (video and image workflows)")
            && out.contains("Pedals (modifiers)")
            && out.contains("re-angle-video")
            && out.contains("not added"),
        "{out}"
    );
    for wf in &cat.workflows {
        assert!(out.contains(&wf.id), "{}", wf.id);
    }
    let (_, out) = go(&ctx, &cat, &w, C::Show("re-angle-video".into()), &[]);
    assert!(
        out.contains("Tekholms-LW/ReAngle")
            && out.contains("Apache-2.0")
            && out.contains("Pedal:      camera: orbit_left")
            && out.contains("stage_re_angle_video")
            && out.contains("12 GB"),
        "{out}"
    );
    assert!(out.contains("SHA-256 not pinned"), "{out}");
    assert_eq!(go(&ctx, &cat, &w, C::Show("nope".into()), &[]).0, 1);
    // check: one machine fits, one does not
    let big = FakeComfy::start(24.0);
    let small = FakeComfy::start(8.0);
    let w = world(
        Some(small.machine("this computer", true, 8.0)),
        vec![big.machine("studio", false, 24.0)],
    );
    let (_, out) = go(&ctx, &cat, &w, C::Check(Some("re-angle-video".into())), &[]);
    assert!(
        out.contains("this computer: no. 8 GB of graphics memory; this needs 12 GB")
            && out.contains("studio: 24 GB, plays the tier")
            && out.contains("It would run on the studio box"),
        "{out}"
    );
    // no ComfyUI anywhere is said plainly
    let (_, out) = go(
        &ctx,
        &cat,
        &world(None, vec![]),
        C::Check(Some("re-angle-video".into())),
        &[],
    );
    assert!(
        out.contains("no ComfyUI is answering") && out.contains("fleet allow stage"),
        "{out}"
    );
}

#[test]
fn enable_needs_a_ready_workflow_and_cloud_asks_first() {
    use super::StageCmd as C;
    let (ctx, cat, _) = ready("cmd-enable");
    let w = world(None, vec![]);
    let (code, out) = go(&ctx, &cat, &w, C::Disable("paint-edit-photo".into()), &[]);
    assert_eq!(code, 0, "{out}");
    assert!(!State::load(&ctx).enabled("paint-edit-photo"));
    let (code, out) = go(&ctx, &cat, &w, C::Enable("paint-edit-photo".into()), &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("stage_paint_edit_photo") && out.contains("asks you each time"),
        "{out}"
    );
    // a workflow that was never added
    let (code, out) = go(
        &ctx,
        &embedded(),
        &w,
        C::Enable("relight-photo".into()),
        &[],
    );
    assert_eq!(code, 1);
    assert!(out.contains("not added yet"), "{out}");
    // cloud: a plain n leaves it off
    let (_, out) = go(
        &ctx,
        &cat,
        &w,
        C::Cloud("paint-edit-photo".into(), true),
        &["n"],
    );
    assert!(
        out.contains("Left off") && !State::load(&ctx).cloud_ok("paint-edit-photo"),
        "{out}"
    );
    let (_, out) = go(
        &ctx,
        &cat,
        &w,
        C::Cloud("paint-edit-photo".into(), true),
        &["y"],
    );
    assert!(State::load(&ctx).cloud_ok("paint-edit-photo"), "{out}");
    go(
        &ctx,
        &cat,
        &w,
        C::Cloud("paint-edit-photo".into(), false),
        &[],
    );
    assert!(!State::load(&ctx).cloud_ok("paint-edit-photo"));
}

#[test]
fn the_fleet_heartbeat_can_say_which_workflows_are_ready() {
    let (ctx, _, _) = ready("ready-ids");
    // the shipped catalogue's hash is not the test graph's, so nothing is ready there
    assert!(super::ready_ids(&ctx).is_empty());
    assert!(super::any_on(&ctx));
    let mut st = State::load(&ctx);
    st.set("paint-edit-photo", vec![("enabled", J::Bool(false))]);
    st.save(&ctx).unwrap();
    assert!(!super::any_on(&ctx));
}

// ---- a remote run: through the Fleet messenger, never at the other computer's ComfyUI

#[test]
fn a_remote_run_goes_through_the_messenger_to_a_comfyui_that_answers_only_on_127_0_0_1() {
    let (ctx, cat, d) = ready("rm-ok");
    let fake = FakeComfy::start(24.0);
    // the lead computer has no ComfyUI at all; the studio's is a fake on 127.0.0.1
    assert!(fake.base().starts_with("http://127.0.0.1:"));
    let studio = Studio::new("rm-ok", &fake, 24.0, true, &cat);
    let w = studio.lead(None, 24.0);
    let (pic, _) = pics(&d);
    let mut o = opts(&d, &[("strength", "20")]);
    o.inputs[0].1 = pic.to_string_lossy().into_owned();
    let out = play(&ctx, &w, &cat, "paint-edit-photo", &o).unwrap();
    assert!(
        out.status.starts_with("Edited on the studio box, "),
        "{}",
        out.status
    );
    // the studio's own ComfyUI took the files and the graph
    let f = fake.st.lock().unwrap();
    assert_eq!(f.prompts.len(), 1);
    assert_eq!(f.uploads.len(), 2, "{:?}", f.uploads);
    drop(f);
    // and the lead never asked for a ComfyUI by address
    assert_eq!(w.comfy_calls.load(Ordering::SeqCst), 0);
    // the result is in the lead's workbench, with a proof that says where it was made
    let t = &out.takes[0];
    assert_eq!(t.machine, "studio");
    let bench = super::run::workbench(&ctx);
    assert!(t.files[0].starts_with(&bench) && t.files[0].is_file());
    let side =
        parse(&std::fs::read_to_string(format!("{}.stage.json", t.files[0].display())).unwrap())
            .unwrap();
    assert_eq!(side.get("machine").unwrap().str("name"), Some("studio"));
    assert_eq!(
        side.get("machine").unwrap().str("via"),
        Some("fleet messenger")
    );
    assert_eq!(
        side.get("output").unwrap().str("sha256").unwrap(),
        crate::setup::sha256::sha256_hex(&std::fs::read(&t.files[0]).unwrap())
    );
    // the input's hash in the proof is the hash of the file that was sent
    let ins = side.arr("inputs");
    assert!(ins.iter().any(|i| i.str("sha256")
        == Some(&crate::setup::sha256::sha256_hex(
            &std::fs::read(&pic).unwrap()
        ))));
    // the studio ran it under its own lease, and kept no takes file or "again" memory of its own
    assert_eq!(*studio.world_b.lease.lock().unwrap(), vec!["begin", "end"]);
    assert!(
        w.lease.lock().unwrap().is_empty(),
        "the lead never took its own card"
    );
    assert!(!crate::conductor::takes_file::path(&studio.ctx_b).exists());
    assert!(State::load(&studio.ctx_b)
        .last_run("paint-edit-photo")
        .is_none());
    // the lead's takes file has the take, as for any other
    let lines = crate::conductor::takes_file::read_all(&crate::conductor::takes_file::path(&ctx));
    assert_eq!(lines.len(), 1);
    // the copies that crossed are gone from the way: the input went to the studio's trash
    assert!(std::fs::read_dir(studio.b.paths.home.join("trash")).is_ok_and(|d| d.count() >= 1));
}

#[test]
fn a_remote_run_needs_the_other_owner_to_have_allowed_stage_and_the_workflow_on() {
    let (ctx, cat, d) = ready("rm-deny");
    let fake = FakeComfy::start(24.0);
    let studio = Studio::new("rm-deny", &fake, 24.0, false, &cat);
    let w = studio.lead(None, 24.0);
    let e = play(&ctx, &w, &cat, "paint-edit-photo", &opts(&d, &[])).unwrap_err();
    assert!(
        e.contains("fleet allow stage") && e.contains("studio"),
        "{e}"
    );
    assert!(fake.st.lock().unwrap().prompts.is_empty());
    // allowed, but the workflow is off there
    crate::fleet::store::update(&studio.b.paths, |f| f.allow.push("stage".into())).unwrap();
    let mut st = State::load(&studio.ctx_b);
    st.set("paint-edit-photo", vec![("enabled", J::Bool(false))]);
    st.save(&studio.ctx_b).unwrap();
    let e = play(&ctx, &w, &cat, "paint-edit-photo", &opts(&d, &[])).unwrap_err();
    assert!(e.contains("not added and on"), "{e}");
    assert!(fake.st.lock().unwrap().prompts.is_empty());
}

#[test]
fn a_computer_without_the_models_can_still_play_on_the_studio() {
    let (ctx, cat, d) = ready("rm-thin");
    // the lead has the workflow file but lacks a model: it cannot play it itself
    let mut cat2 = cat.clone();
    cat2.workflows[0].models.push(super::catalog::Model {
        name: "absent.safetensors".into(),
        dir: "vae".into(),
        licence: "MIT".into(),
        gb: Some(1.0),
        url: "https://example.invalid/x".into(),
        gated: false,
        sha256: None,
        verified: false,
        only_for: None,
    });
    let fake = FakeComfy::start(24.0);
    let studio = Studio::new("rm-thin", &fake, 24.0, true, &cat);
    let local = FakeComfy::start(24.0);
    let w = studio.lead(Some(local.machine("this computer", true, 24.0)), 24.0);
    let out = play(&ctx, &w, &cat2, "paint-edit-photo", &opts(&d, &[])).unwrap();
    assert_eq!(out.takes[0].machine, "studio");
    assert!(local.st.lock().unwrap().prompts.is_empty());
    // with no studio either, it says what is missing and where to get it played
    let alone = world(Some(local.machine("this computer", true, 24.0)), vec![]);
    let e = play(&ctx, &alone, &cat2, "paint-edit-photo", &opts(&d, &[])).unwrap_err();
    assert!(
        e.contains("absent.safetensors") && e.contains("Fleet"),
        "{e}"
    );
}
