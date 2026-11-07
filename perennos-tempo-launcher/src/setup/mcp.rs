//! `tempo-launch mcp`: a stdio MCP server (JSON-RPC 2.0, one message per line) so the person's
//! own AI (Claude Desktop, Claude Code, any MCP client) can help set Tempo up.
//!
//! What it can see: machine facts only (`setup_scan`), the recommended models (`setup_plan`) and
//! which Tempo parts run (`tempo_status`). No user name, folder, host name or address is ever in
//! an answer. What it can do: write a proposal into the Tempo home (`setup_propose`); the person
//! applies it with `tempo-launch setup --apply-proposal`, after a diff and a y/N. The AI never
//! changes a setting itself.
//!
//! Two more tools let the AI work with the Tempo Conductor to design a band with the owner:
//! `conductor_templates` (read-only: the ready-made bands and the rules) and `conductor_propose`
//! (writes a proposal; the owner sets it up with `tempo-launch conductor apply-proposal`, after a
//! y/N, and the Conductor picks each member's engine then). The older names `band_templates` and
//! `band_propose` still work but are not listed.

use super::apply::{self, Opts};
use super::json::{parse, J};
use super::plan::{Run, Status};
use super::probe::{Gpu, Host, Machine};
use super::propose;
use super::runtime::RuntimePlan;
use crate::{answers, conductor, Ctx, APPS};
use std::io::{BufRead, Write};

pub const SERVER_NAME: &str = "tempo-setup";

fn o(pairs: Vec<(&str, J)>) -> J {
    J::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn opt_n(v: Option<f64>) -> J {
    v.map_or(J::Null, |x| J::n((x * 10.0).round() / 10.0))
}

/// Machine facts and nothing else: the same on every computer of the same kind.
pub fn scan_facts(m: &Machine) -> J {
    let cpu_class = match m.cpus {
        0..=4 => "entry (4 threads or fewer)",
        5..=8 => "mid (5 to 8 threads)",
        9..=16 => "high (9 to 16 threads)",
        _ => "workstation (more than 16 threads)",
    };
    let gpu = match &m.gpu {
        Gpu::None => o(vec![("class", J::s("none"))]),
        Gpu::Nvidia { name, total_gb, .. } => o(vec![
            ("class", J::s("discrete NVIDIA")),
            ("name", J::s(name)),
            ("memory_gb", opt_n(Some(*total_gb))),
        ]),
        Gpu::Apple { chip } => o(vec![
            (
                "class",
                J::s("Apple silicon, memory shared with the computer"),
            ),
            ("name", J::s(chip)),
        ]),
    };
    o(vec![
        ("os", J::s(&m.os)),
        ("arch", J::s(&m.arch)),
        ("logical_cpus", J::n(m.cpus as f64)),
        ("cpu_class", J::s(cpu_class)),
        ("memory_gb_total", opt_n(m.ram_total_gb)),
        ("memory_gb_free", opt_n(m.ram_avail_gb)),
        ("gpu", gpu),
        ("disk_gb_free", opt_n(m.disk_free_gb)),
        (
            "ollama",
            o(vec![
                (
                    "present",
                    J::Bool(m.ollama.running || m.ollama.binary.is_some()),
                ),
                ("running", J::Bool(m.ollama.running)),
                ("version", m.ollama.version.as_deref().map_or(J::Null, J::s)),
                ("models_installed", J::n(m.ollama.models.len() as f64)),
            ]),
        ),
    ])
}

/// Replace this computer's folders in a text with a name, as a last guard.
fn scrub(ctx: &Ctx, text: &str) -> String {
    let mut t = text.replace(&ctx.home.to_string_lossy().into_owned(), "<Tempo home>");
    for k in ["HOME", "USERPROFILE"] {
        if let Some(h) = std::env::var_os(k).map(|v| v.to_string_lossy().into_owned()) {
            if h.len() > 3 {
                t = t.replace(&h, "~");
            }
        }
    }
    t
}

fn plan_facts(ctx: &Ctx, host: &dyn Host, port: u16, now: u64) -> Result<J, String> {
    let c = apply::compute(ctx, host, &Opts::default(), false, port, now)?;
    let p = &c.plan;
    let picks: Vec<J> = p
        .picks
        .iter()
        .map(|k| {
            o(vec![
                ("job", J::s(k.role)),
                ("model", J::s(&k.model)),
                ("size_gb", opt_n(k.gb)),
                ("licence", J::s(&k.licence)),
                (
                    "runs_on",
                    J::s(if k.run == Run::Gpu {
                        "graphics card"
                    } else {
                        "CPU"
                    }),
                ),
                ("already_installed", J::Bool(k.status == Status::Installed)),
                ("why", J::s(&scrub(ctx, &k.why))),
            ])
        })
        .collect();
    let runtime = match &p.runtime {
        None => "Ollama is available on this computer".to_string(),
        Some(RuntimePlan::Installed(_)) => "Tempo has its own copy of Ollama installed".into(),
        Some(RuntimePlan::Unsupported(why)) => scrub(ctx, why),
        Some(RuntimePlan::Missing(_)) => {
            "No Ollama here. With the owner's yes, setup can fetch Tempo's own pinned copy".into()
        }
    };
    Ok(o(vec![
        ("goals", J::Arr(p.goals.iter().map(|g| J::s(g)).collect())),
        ("models", J::Arr(picks)),
        ("runtime", J::s(&runtime)),
        ("download_gb", opt_n(Some(p.fetch_gb))),
        ("disk_gb_free", opt_n(p.disk_free_gb)),
        (
            "blocked",
            p.refused
                .as_deref()
                .map_or(J::Null, |r| J::s(&scrub(ctx, r))),
        ),
        (
            "the_owner_applies_it_with",
            J::s("tempo-launch setup --apply"),
        ),
    ]))
}

fn status_facts(ctx: &Ctx, host: &dyn Host, port: u16) -> J {
    let apps: Vec<J> = APPS
        .iter()
        .map(|a| {
            o(vec![
                ("app", J::s(a.id)),
                ("installed", J::Bool(ctx.find(a.bin).is_some())),
                ("running", J::Bool(answers(a, a.port).is_some())),
            ])
        })
        .collect();
    let ollama = super::probe::probe_ollama(host, port);
    let cfg = ctx
        .agent_config_file()
        .or_else(|| Some(ctx.agent_data.join("agent-config.json")).filter(|p| p.is_file()));
    let engines = cfg
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| parse(&t).ok())
        .map_or(0, |j| j.strs("ollama_fallbacks").len());
    o(vec![
        ("apps", J::Arr(apps)),
        ("ollama_running", J::Bool(ollama.running)),
        (
            "tempo_has_its_own_ollama",
            J::Bool(super::runtime::read_record(&ctx.home).is_some()),
        ),
        (
            "agent_settings_exist",
            J::Bool(
                ctx.agent_config_file().is_some()
                    || ctx.agent_data.join("agent-config.json").is_file(),
            ),
        ),
        ("other_computers_linked", J::n(engines as f64)),
        (
            "proposal_waiting_for_the_owner",
            J::Bool(propose::proposal_waiting(ctx)),
        ),
    ])
}

fn tool(
    name: &str,
    title: &str,
    description: &str,
    props: J,
    required: &[&str],
    read_only: bool,
) -> J {
    o(vec![
        ("name", J::s(name)),
        ("title", J::s(title)),
        ("description", J::s(description)),
        (
            "inputSchema",
            o(vec![
                ("type", J::s("object")),
                ("properties", props),
                (
                    "required",
                    J::Arr(required.iter().map(|r| J::s(r)).collect()),
                ),
            ]),
        ),
        (
            "annotations",
            o(vec![
                ("title", J::s(title)),
                ("readOnlyHint", J::Bool(read_only)),
                ("destructiveHint", J::Bool(false)),
                ("idempotentHint", J::Bool(true)),
                ("openWorldHint", J::Bool(false)),
            ]),
        ),
    ])
}

pub fn tools() -> Vec<J> {
    let none = || J::Obj(vec![]);
    vec![
        tool(
            "setup_scan",
            "Scan this computer",
            "What this computer is, as machine facts only: operating system, CPU and graphics card class, memory, free disk, whether Ollama is present. It holds no user name, folder, host name or address.",
            none(),
            &[],
            true,
        ),
        tool(
            "setup_plan",
            "Recommended models",
            "The models and the runtime Tempo recommends for this computer, with size, licence and why. Nothing is fetched; the owner applies it with `tempo-launch setup --apply`.",
            none(),
            &[],
            true,
        ),
        tool(
            "tempo_status",
            "Which Tempo parts run",
            "Which Tempo apps are installed and running, whether the agent has settings, how many other computers are linked, and whether a proposal is waiting. No addresses or folders.",
            none(),
            &[],
            true,
        ),
        tool(
            "setup_propose",
            "Propose a settings change",
            "Write a proposed change to the agent's settings for the owner to review. Nothing is changed: it returns the command the owner runs (`tempo-launch setup --apply-proposal`), which shows a diff and asks y/N. Allowed settings: models, fast_models (lists of model names), embed_model (a model name), ollama_url and ollama_fallbacks (addresses on a private, tailnet or local network only).",
            o(vec![
                (
                    "settings",
                    o(vec![
                        ("type", J::s("object")),
                        ("description", J::s("The settings to change, e.g. {\"models\":[\"qwen3:8b\"],\"ollama_fallbacks\":[\"http://192.168.0.20:11434\"]}")),
                        (
                            "properties",
                            o(vec![
                                ("models", o(vec![("type", J::s("array")), ("items", o(vec![("type", J::s("string"))]))])),
                                ("fast_models", o(vec![("type", J::s("array")), ("items", o(vec![("type", J::s("string"))]))])),
                                ("embed_model", o(vec![("type", J::s("string"))])),
                                ("ollama_url", o(vec![("type", J::s("string"))])),
                                ("ollama_fallbacks", o(vec![("type", J::s("array")), ("items", o(vec![("type", J::s("string"))]))])),
                            ]),
                        ),
                        ("additionalProperties", J::Bool(false)),
                    ]),
                ),
                (
                    "note",
                    o(vec![
                        ("type", J::s("string")),
                        ("description", J::s("One or two plain sentences for the owner: why you propose this.")),
                    ]),
                ),
            ]),
            &["settings"],
            false,
        ),
        tool(
            "conductor_templates",
            "Ready-made bands",
            "Tempo Conductor sets up bands for the owner: a small team of AI members for the owner's own workflow (a social media team, a release team, a study group...). This lists the ready-made bands with their members, roles and flows, and the rules a band must follow. Use it to design a band with the owner. Nothing is changed.",
            none(),
            &[],
            true,
        ),
        tool(
            "conductor_propose",
            "Propose a band",
            "Write a proposed band for the owner to review. The Conductor picks each member's engine when the owner sets it up, so leave engines out. Nothing is set up: it returns the command the owner runs (`tempo-launch conductor apply-proposal`), which shows the band in plain words and asks y/N before anything is saved, added to Room or scheduled. Members have no tools: they only talk and write drafts. Every problem with the band is returned so you can fix it and propose again.",
            o(vec![
                (
                    "band",
                    schema(
                        r#"{"type":"object","description":"The band. Use conductor_templates for the shape and the rules.","properties":{"name":{"type":"string","description":"2 to 40 lowercase letters, digits, hyphens"},"about":{"type":"string","description":"one line, at most 200 characters"},"members":{"type":"array","items":{"type":"object","properties":{"handle":{"type":"string"},"name":{"type":"string"},"instrument":{"type":"string","description":"one fun word, like Metronome"},"role":{"type":"string","description":"the member's instruction, at most 2000 characters"},"may":{"type":"string","enum":["talk","draft"]},"model":{"type":"string","description":"fast, heavy, or leave out"}},"required":["handle","name","instrument","role","may"],"additionalProperties":false}},"flows":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"goal":{"type":"string"},"steps":{"type":"array","items":{"type":"string"}},"every":{"type":"string","description":"manual, daily HH:MM, or weekly <mon..sun> HH:MM"},"notes_to":{"type":"string","description":"leave out: Bands/<name>/"}},"required":["name","goal","steps","every"],"additionalProperties":false}}},"required":["name","about","members","flows"],"additionalProperties":false}"#,
                    ),
                ),
                (
                    "note",
                    o(vec![
                        ("type", J::s("string")),
                        ("description", J::s("One or two plain sentences for the owner: why you propose this band.")),
                    ]),
                ),
            ]),
            &["band"],
            false,
        ),
    ]
}

fn schema(text: &str) -> J {
    parse(text).unwrap_or(J::Obj(vec![]))
}

/// Everything a tool needs from the outside world.
pub struct Server<'a> {
    pub ctx: &'a Ctx,
    pub host: &'a dyn Host,
    pub port: u16,
    pub now: &'a dyn Fn() -> u64,
}

impl Server<'_> {
    /// Run one tool. `Err` is a tool error the model should see (not a protocol error).
    pub fn call(&self, name: &str, args: &J) -> Result<String, String> {
        let m = || super::probe::probe(self.host, &self.ctx.home, self.port);
        match name {
            "setup_scan" => Ok(scan_facts(&m()).pretty()),
            "setup_plan" => Ok(plan_facts(self.ctx, self.host, self.port, (self.now)())?.pretty()),
            "tempo_status" => Ok(status_facts(self.ctx, self.host, self.port).pretty()),
            "setup_propose" => {
                let settings = args
                    .get("settings")
                    .ok_or("settings is required: an object naming the settings to change")?;
                let note = args.str("note").unwrap_or("");
                let cmd = propose::write_proposal(self.ctx, settings, note, (self.now)())?;
                Ok(format!(
                    "Proposal saved in the Tempo home folder as {file}. Nothing has been changed.\nTell the owner to run this in a terminal; it shows what would change and asks y/N:\n\n  {cmd}\n\nYou cannot apply it yourself.",
                    file = propose::PROPOSAL_FILE
                ))
            }
            "conductor_templates" | "band_templates" => {
                Ok(conductor::propose::templates_facts().pretty())
            }
            "conductor_propose" | "band_propose" => {
                let b = args
                    .get("band")
                    .ok_or("band is required: an object with name, about, members and flows (see conductor_templates)")?;
                let name = conductor::propose::write(
                    self.ctx,
                    b,
                    args.str("note").unwrap_or(""),
                    (self.now)(),
                )?;
                Ok(format!(
                    "Band proposal \"{name}\" saved in the Tempo home folder. Nothing has been set up.\nTell the owner to run this in a terminal; it shows the band in plain words and asks y/N:\n\n  {} {name}\n\nYou cannot apply it yourself.",
                    conductor::propose::APPLY_COMMAND
                ))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }

    /// One JSON-RPC message in, at most one out (notifications get no reply).
    pub fn handle(&self, msg: &J) -> Option<J> {
        let id = msg.get("id").cloned();
        let method = msg.str("method").unwrap_or("");
        let reply = |result: J| {
            Some(o(vec![
                ("jsonrpc", J::s("2.0")),
                ("id", id.clone().unwrap_or(J::Null)),
                ("result", result),
            ]))
        };
        let error = |code: i64, m: &str| {
            Some(o(vec![
                ("jsonrpc", J::s("2.0")),
                ("id", id.clone().unwrap_or(J::Null)),
                (
                    "error",
                    o(vec![("code", J::n(code as f64)), ("message", J::s(m))]),
                ),
            ]))
        };
        id.as_ref()?; // notifications (initialized, cancelled, ...) get no reply
        let params = msg.get("params").cloned().unwrap_or(J::Obj(vec![]));
        match method {
            "initialize" => reply(o(vec![
                (
                    "protocolVersion",
                    J::s(params.str("protocolVersion").unwrap_or("2024-11-05")),
                ),
                (
                    "capabilities",
                    o(vec![("tools", o(vec![("listChanged", J::Bool(false))]))]),
                ),
                (
                    "serverInfo",
                    o(vec![
                        ("name", J::s(SERVER_NAME)),
                        ("version", J::s(crate::VERSION)),
                    ]),
                ),
                (
                    "instructions",
                    J::s("Perennos Tempo setup helper. Start with setup_scan and setup_plan, explain the result to the owner in plain words, and ask whether this is one computer or several. You can only read machine facts and write a proposal with setup_propose; the owner applies it themselves with `tempo-launch setup --apply-proposal`. Never ask the owner for files, notes or chats. To help design a band with the Tempo Conductor (a small team of AI members for the owner's own workflow, each played on the engine that suits the part), read conductor_templates, talk through what the owner does, and write conductor_propose; the owner sets it up with `tempo-launch conductor apply-proposal`."),
                ),
            ])),
            "ping" => reply(J::Obj(vec![])),
            "tools/list" => reply(o(vec![("tools", J::Arr(tools()))])),
            "tools/call" => {
                let name = params.str("name").unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(J::Obj(vec![]));
                let (text, is_err) = match self.call(name, &args) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                reply(o(vec![
                    (
                        "content",
                        J::Arr(vec![o(vec![
                            ("type", J::s("text")),
                            ("text", J::s(&scrub(self.ctx, &text))),
                        ])]),
                    ),
                    ("isError", J::Bool(is_err)),
                ]))
            }
            "resources/list" => reply(o(vec![("resources", J::Arr(vec![]))])),
            "prompts/list" => reply(o(vec![("prompts", J::Arr(vec![]))])),
            _ => error(-32601, "method not found"),
        }
    }

    /// Serve MCP on the given streams until input ends.
    pub fn run(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let out = match parse(&line) {
                Ok(msg @ J::Obj(_)) => self.handle(&msg),
                _ => Some(o(vec![
                    ("jsonrpc", J::s("2.0")),
                    ("id", J::Null),
                    (
                        "error",
                        o(vec![
                            ("code", J::n(-32700.0)),
                            ("message", J::s("parse error")),
                        ]),
                    ),
                ])),
            };
            if let Some(out) = out {
                writeln!(output, "{}", out.compact())?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

/// `tempo-launch mcp`: serve on stdin and stdout until the client closes them.
pub fn run_stdio(ctx: &Ctx) -> i32 {
    let s = Server {
        ctx,
        host: &super::probe::RealHost,
        port: apply::ollama_port(),
        now: &crate::unix_now,
    };
    match s.run(std::io::stdin().lock(), std::io::stdout().lock()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// `tempo-launch mcp --scan`: the same facts `setup_scan` gives, to paste into any AI by hand.
pub fn print_scan(ctx: &Ctx) -> i32 {
    let m = super::probe::probe(&super::probe::RealHost, &ctx.home, apply::ollama_port());
    println!("{}", scan_facts(&m).pretty().trim_end());
    0
}

#[cfg(test)]
mod tests {
    use super::super::probe::tests::Fake;
    use super::*;
    use std::path::{Path, PathBuf};

    fn tmp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "tempo-mcp-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx_in(root: &Path) -> Ctx {
        Ctx {
            home: root.join("home-of-someone"),
            agent_data: root.join("home-of-someone").join("agent"),
            vault: root.join("vault"),
            bin_dirs: vec![root.join("bin")],
            browser: false,
            opened: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn pc() -> Fake {
        Fake {
            os: "windows",
            arch: "x86_64",
            cpus: 8,
            win_mem: Some((16 << 30, 10 << 30)),
            win_disk: Some(50 << 30),
            ..Fake::default()
        }
    }

    fn rpc(s: &Server, id: u32, method: &str, params: &str) -> J {
        let msg = parse(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ))
        .unwrap();
        s.handle(&msg).unwrap()
    }

    fn text_of(r: &J) -> (String, bool) {
        let res = r.get("result").unwrap();
        (
            res.arr("content")[0].str("text").unwrap().to_string(),
            res.bool("isError").unwrap(),
        )
    }

    #[test]
    fn handshake_negotiates_the_version_and_notifications_get_no_reply() {
        let root = tmp("hand");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1_793_000_000,
        };
        let r = rpc(&s, 1, "initialize", r#"{"protocolVersion":"2025-06-18"}"#);
        let res = r.get("result").unwrap();
        assert_eq!(res.str("protocolVersion"), Some("2025-06-18"));
        assert_eq!(
            res.get("serverInfo").unwrap().str("name"),
            Some("tempo-setup")
        );
        assert!(res.get("capabilities").unwrap().get("tools").is_some());
        let r = rpc(&s, 2, "initialize", "{}");
        assert_eq!(
            r.get("result").unwrap().str("protocolVersion"),
            Some("2024-11-05")
        );
        assert!(s
            .handle(&parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap())
            .is_none());
        assert_eq!(
            rpc(&s, 3, "ping", "{}").get("result"),
            Some(&J::Obj(vec![]))
        );
        let e = rpc(&s, 4, "nope", "{}");
        assert_eq!(e.get("error").unwrap().f64("code"), Some(-32601.0));
        // the stdio loop: bad JSON gets a parse error, requests get replies, notifications none
        let input = "{bad\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\n{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n";
        let mut out = Vec::new();
        s.run(input.as_bytes(), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert_eq!(
            parse(lines[0]).unwrap().get("error").unwrap().f64("code"),
            Some(-32700.0)
        );
        assert_eq!(parse(lines[1]).unwrap().f64("id"), Some(9.0));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lists_six_tools_and_marks_the_read_only_ones() {
        let root = tmp("list");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1,
        };
        let r = rpc(&s, 1, "tools/list", "{}");
        let t = r.get("result").unwrap().arr("tools");
        let names: Vec<&str> = t.iter().filter_map(|x| x.str("name")).collect();
        assert_eq!(
            names,
            [
                "setup_scan",
                "setup_plan",
                "tempo_status",
                "setup_propose",
                "conductor_templates",
                "conductor_propose"
            ]
        );
        for x in t {
            let ro = x.get("annotations").unwrap().bool("readOnlyHint").unwrap();
            assert_eq!(
                ro,
                !matches!(x.str("name"), Some("setup_propose" | "conductor_propose")),
                "{:?}",
                x.str("name")
            );
            assert!(x.get("inputSchema").is_some());
        }
        // one line per message
        assert!(!r.compact().contains('\n'));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_scan_holds_machine_facts_and_no_personal_data() {
        let root = tmp("scan");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1_793_000_000,
        };
        let r = rpc(
            &s,
            1,
            "tools/call",
            r#"{"name":"setup_scan","arguments":{}}"#,
        );
        let (text, err) = text_of(&r);
        assert!(!err, "{text}");
        let j = parse(&text).unwrap();
        // exactly these facts
        let J::Obj(o) = &j else { panic!() };
        let keys: Vec<&str> = o.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "os",
                "arch",
                "logical_cpus",
                "cpu_class",
                "memory_gb_total",
                "memory_gb_free",
                "gpu",
                "disk_gb_free",
                "ollama"
            ]
        );
        assert_eq!(j.f64("memory_gb_total"), Some(16.0));
        assert_eq!(j.get("gpu").unwrap().str("class"), Some("none"));
        assert_eq!(j.get("ollama").unwrap().bool("present"), Some(false));
        // nothing about the person: not the user, the home path, the Tempo folder, the host
        let mut personal: Vec<String> = vec![
            ctx.home.to_string_lossy().into_owned(),
            root.to_string_lossy().into_owned(),
            "home-of-someone".into(),
        ];
        for k in [
            "USER",
            "USERNAME",
            "LOGNAME",
            "HOME",
            "USERPROFILE",
            "HOSTNAME",
            "COMPUTERNAME",
        ] {
            if let Ok(v) = std::env::var(k) {
                if v.len() > 3 {
                    personal.push(v);
                }
            }
        }
        if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
            if h.trim().len() > 3 {
                personal.push(h.trim().to_string());
            }
        }
        for what in ["setup_scan", "setup_plan", "tempo_status"] {
            let r = rpc(
                &s,
                2,
                "tools/call",
                &format!(r#"{{"name":"{what}","arguments":{{}}}}"#),
            );
            let (t, _) = text_of(&r);
            for p in &personal {
                assert!(!t.contains(p.as_str()), "{what} leaks {p}: {t}");
            }
            assert!(
                !t.contains("127.0.0.1") && !t.contains("http://"),
                "{what}: {t}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_plan_and_status_answer_without_changing_anything() {
        let root = tmp("plan");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1_793_000_000,
        };
        let (t, err) = text_of(&rpc(&s, 1, "tools/call", r#"{"name":"setup_plan"}"#));
        assert!(!err, "{t}");
        let j = parse(&t).unwrap();
        assert!(!j.arr("models").is_empty());
        assert_eq!(
            j.str("the_owner_applies_it_with"),
            Some("tempo-launch setup --apply")
        );
        let (t, err) = text_of(&rpc(&s, 2, "tools/call", r#"{"name":"tempo_status"}"#));
        assert!(!err, "{t}");
        assert_eq!(
            parse(&t).unwrap().bool("proposal_waiting_for_the_owner"),
            Some(false)
        );
        let (t, err) = text_of(&rpc(&s, 3, "tools/call", r#"{"name":"nope"}"#));
        assert!(err && t.contains("unknown tool"));
        assert!(!ctx.home.join(propose::PROPOSAL_FILE).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_proposal_is_written_and_only_the_person_applies_it() {
        let root = tmp("prop");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1_793_000_000,
        };
        std::fs::create_dir_all(&ctx.agent_data).unwrap();
        let cfg = ctx.agent_data.join("config.json");
        std::fs::write(&cfg, r#"{"models":["old:7b"],"temperature":0.2}"#).unwrap();
        let args = r#"{"name":"setup_propose","arguments":{"note":"The GPU box is faster.\nUse it.","settings":{"models":["qwen3:8b"],"ollama_fallbacks":["http://192.168.0.20"]}}}"#;
        let (t, err) = text_of(&rpc(&s, 1, "tools/call", args));
        assert!(!err, "{t}");
        assert!(
            t.contains("tempo-launch setup --apply-proposal")
                && t.contains("Nothing has been changed")
        );
        assert!(!t.contains(&root.to_string_lossy().into_owned()));
        // the proposal is a file; the settings are untouched
        let p = parse(&std::fs::read_to_string(ctx.home.join("proposed-settings.json")).unwrap())
            .unwrap();
        assert_eq!(
            p.get("settings").unwrap().strs("ollama_fallbacks"),
            ["http://192.168.0.20:11434"]
        );
        assert_eq!(p.str("note"), Some("The GPU box is faster. Use it."));
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            r#"{"models":["old:7b"],"temperature":0.2}"#
        );
        // status says one is waiting
        let (t, _) = text_of(&rpc(&s, 2, "tools/call", r#"{"name":"tempo_status"}"#));
        assert_eq!(
            parse(&t).unwrap().bool("proposal_waiting_for_the_owner"),
            Some(true)
        );

        // the person says no: nothing changes and the proposal stays
        let mut out = Vec::new();
        let code = propose::apply_proposal(&ctx, None, 1_793_000_100, &mut out, &mut |_| false);
        let shown = String::from_utf8(out).unwrap();
        assert_eq!(code, 0);
        assert!(
            shown.contains("- models: [\"old:7b\"]") && shown.contains("+ models: [\"qwen3:8b\"]"),
            "{shown}"
        );
        assert!(shown.contains("+ ollama_fallbacks") && shown.contains("Nothing changed"));
        assert!(shown.contains("not by Tempo"));
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            r#"{"models":["old:7b"],"temperature":0.2}"#
        );
        assert!(ctx.home.join("proposed-settings.json").exists());

        // a TEMPO_AGENT_CONFIG file is left alone
        let mut out = Vec::new();
        let code = propose::apply_proposal(
            &ctx,
            Some(cfg.clone()),
            1_793_000_100,
            &mut out,
            &mut |_| panic!("no question"),
        );
        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            r#"{"models":["old:7b"],"temperature":0.2}"#
        );

        // the person says yes: written, old file kept, proposal moved aside, other keys kept
        let mut out = Vec::new();
        let mut asked = 0;
        let code = propose::apply_proposal(&ctx, None, 1_793_000_100, &mut out, &mut |q| {
            asked += 1;
            assert!(q.contains("[y/N]"));
            true
        });
        assert_eq!((code, asked), (0, 1));
        let now = parse(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(now.strs("models"), ["qwen3:8b"]);
        assert_eq!(now.strs("ollama_fallbacks"), ["http://192.168.0.20:11434"]);
        assert_eq!(now.f64("temperature"), Some(0.2));
        assert!(!ctx.home.join("proposed-settings.json").exists());
        assert!(ctx.home.join("trash").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_proposal_cannot_name_a_public_address_or_a_setting_outside_the_list() {
        let root = tmp("bad");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1,
        };
        for args in [
            r#"{"settings":{"ollama_fallbacks":["http://example.com:11434"]}}"#,
            r#"{"settings":{"ollama_url":"https://8.8.8.8"}}"#,
            r#"{"settings":{"tools_allowed":["shell"]}}"#,
            r#"{"settings":{"models":["a b"]}}"#,
            r#"{"settings":{}}"#,
            r#"{"note":"no settings"}"#,
        ] {
            let (t, err) = text_of(&rpc(
                &s,
                1,
                "tools/call",
                &format!(r#"{{"name":"setup_propose","arguments":{args}}}"#),
            ));
            assert!(err, "{args}: {t}");
        }
        assert!(!ctx.home.join("proposed-settings.json").exists());
        // and a hand-edited bad file is refused at apply time
        std::fs::create_dir_all(&ctx.home).unwrap();
        std::fs::write(
            ctx.home.join("proposed-settings.json"),
            r#"{"settings":{"ollama_url":"https://8.8.8.8"}}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        assert_eq!(
            propose::apply_proposal(&ctx, None, 1, &mut out, &mut |_| panic!("no question")),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // ---- Tempo Conductor

    fn call(s: &Server, name: &str, args: &str) -> (String, bool) {
        text_of(&rpc(
            s,
            1,
            "tools/call",
            &format!(r#"{{"name":"{name}","arguments":{args}}}"#),
        ))
    }

    const BAND_ARGS: &str = r#"{"note":"Built around your bakery.\nEnjoy.","band":{"name":"bakery-posts","about":"Weekly posts for the bakery.","members":[{"handle":"writer","name":"Crumb","instrument":"Piccolo","role":"You write short warm captions for a bakery.","may":"draft","model":"heavy"},{"handle":"editor","name":"Glaze","instrument":"Triangle","role":"You tidy captions and check them.","may":"draft"}],"flows":[{"name":"weekly-plan","goal":"Plan the week.","steps":["writer","editor"],"every":"weekly mon 09:00"}]}}"#;

    #[test]
    fn the_conductor_templates_tool_is_read_only_and_holds_no_personal_data() {
        let root = tmp("bandtpl");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1,
        };
        let (t, err) = call(&s, "conductor_templates", "{}");
        assert!(!err, "{t}");
        let j = parse(&t).unwrap();
        assert_eq!(j.arr("templates").len(), 7);
        for want in [
            "social-media",
            "dev-team",
            "Soundcheck",
            "never post, send, buy or delete",
        ] {
            assert!(t.contains(want), "{want}");
        }
        assert!(j.arr("rules").len() >= 4);
        assert!(!t.contains(&root.to_string_lossy().into_owned()));
        // it changed nothing
        assert!(!ctx.home.join("bands").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_older_band_tool_names_still_work_as_aliases() {
        let root = tmp("bandalias");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1,
        };
        let (new, _) = call(&s, "conductor_templates", "{}");
        let (old, err) = call(&s, "band_templates", "{}");
        assert!(!err);
        assert_eq!(new, old);
        let (t, err) = call(&s, "band_propose", BAND_ARGS);
        assert!(!err, "{t}");
        assert!(
            t.contains("tempo-launch conductor apply-proposal bakery-posts"),
            "{t}"
        );
        // listed under the new names only
        let listed = rpc(&s, 1, "tools/list", "{}").compact();
        assert!(listed.contains("conductor_propose") && !listed.contains("band_propose"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn conductor_propose_writes_a_proposal_and_only_the_owner_sets_it_up() {
        let root = tmp("bandprop");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1_793_000_000,
        };
        let (t, err) = call(&s, "conductor_propose", BAND_ARGS);
        assert!(!err, "{t}");
        assert!(
            t.contains("tempo-launch conductor apply-proposal bakery-posts")
                && t.contains("Nothing has been set up")
                && t.contains("cannot apply it yourself")
        );
        assert!(!t.contains(&root.to_string_lossy().into_owned()));
        // a proposal file, and no band
        let p = parse(
            &std::fs::read_to_string(ctx.home.join("bands/proposals/bakery-posts.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(p.str("note"), Some("Built around your bakery. Enjoy."));
        assert_eq!(p.get("band").unwrap().str("name"), Some("bakery-posts"));
        assert!(!ctx.home.join("bands/bakery-posts").exists());
        // the same proposal is what apply-proposal reads
        assert_eq!(conductor::propose::waiting(&ctx), ["bakery-posts"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn conductor_propose_refuses_a_band_with_tools_a_bad_folder_or_no_band() {
        let root = tmp("bandbad");
        let ctx = ctx_in(&root);
        let host = pc();
        let s = Server {
            ctx: &ctx,
            host: &host,
            port: 1,
            now: &|| 1,
        };
        let with = |from: &str, to: &str| BAND_ARGS.replacen(from, to, 1);
        for (what, args) in [
            (
                "a tools key",
                with(
                    r#""may":"draft","model":"heavy""#,
                    r#""may":"draft","tools":["send"]"#,
                ),
            ),
            (
                "a folder elsewhere",
                with(
                    r#""every":"weekly mon 09:00""#,
                    r#""every":"weekly mon 09:00","notes_to":"Documents/""#,
                ),
            ),
            (
                "a step nobody is",
                with(
                    r#""steps":["writer","editor"]"#,
                    r#""steps":["writer","ghost"]"#,
                ),
            ),
            (
                "a schedule that is not one",
                with("weekly mon 09:00", "whenever"),
            ),
            (
                "a long role",
                with(
                    "You write short warm captions for a bakery.",
                    &"x".repeat(2001),
                ),
            ),
            (
                "a name with capitals",
                with(r#""name":"bakery-posts""#, r#""name":"Bakery""#),
            ),
            ("no band", r#"{"note":"hello"}"#.to_string()),
            (
                "a band that is not an object",
                r#"{"band":"bakery"}"#.to_string(),
            ),
        ] {
            let (t, err) = call(&s, "conductor_propose", &args);
            assert!(err, "{what}: {t}");
        }
        // the answer lists the problems so the AI can fix them
        let (t, _) = call(
            &s,
            "conductor_propose",
            &with("weekly mon 09:00", "whenever"),
        );
        assert!(t.contains("not a schedule"), "{t}");
        assert!(!ctx.home.join("bands").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
