//! The band's tools, anywhere: other computers' tools as the agent's own.
//!
//! `tempo-launch fleet serve` on this computer knows the other computers and what each one offers
//! (its owner allows each tool on that computer; with nothing allowed it offers nothing). This
//! module asks it, over this computer only (127.0.0.1, with a token that only this user can read),
//! and does two things. `comfy_image` goes to another computer's ComfyUI when this one has none:
//! this computer's own ComfyUI always wins, otherwise the node with the biggest graphics card; the
//! picture comes back in pieces, checked against its SHA-256, into the output folder. Remote MCP
//! tools show up as `<node>__<server>__<tool>`. Nothing here skips the owner's rules: plan mode,
//! the deny list (`mcp_blocked`) and the questions (`mcp_confirm`, and a question for anything
//! that is not read-only) all apply to a remote call as to a local one, and results are redacted.
//! Without a messenger running, all of it is silent: no tools are added.

use crate::config::Config;
use crate::util;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 8770;

fn port() -> u16 {
    std::env::var("TEMPO_FLEET_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_PORT)
}

/// The Tempo home the launcher uses (it writes `fleet-local.token` there).
fn home() -> PathBuf {
    std::env::var_os("TEMPO_LAUNCH_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::tempo_home::tempo_home().dir)
}

fn token() -> Option<String> {
    let t = std::fs::read_to_string(home().join("fleet-local.token")).ok()?;
    let t = t.trim().to_string();
    (t.len() == 64).then_some(t)
}

fn base() -> String {
    format!("http://127.0.0.1:{}", port())
}

static VIEW: Mutex<Option<(Instant, Option<Value>)>> = Mutex::new(None);

/// The fleet as the messenger tells it (`/fleet.json`), asked at most every ten seconds.
pub fn view() -> Option<Value> {
    let mut g = VIEW.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, v)) = g.as_ref() {
        if t.elapsed() < Duration::from_secs(10) {
            return v.clone();
        }
    }
    let v = util::http_json(
        &format!("{}/fleet.json", base()),
        None,
        Duration::from_secs(2),
    )
    .ok();
    *g = Some((Instant::now(), v.clone()));
    v
}

#[cfg(test)]
pub fn forget_view() {
    *VIEW.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub node: String,
    pub server: String,
    pub tool: String,
    pub desc: String,
    pub schema: Value,
    pub read_only: bool,
}

impl Remote {
    /// What the model calls it.
    pub fn name(&self) -> String {
        format!("{}__{}__{}", self.node, self.server, self.tool)
    }
}

fn machines(v: &Value) -> impl Iterator<Item = &Value> {
    v["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["up"] == true)
}

/// Every MCP tool the computers that are up offer.
pub fn remotes_in(v: &Value) -> Vec<Remote> {
    let mut out = Vec::new();
    for m in machines(v) {
        let node = m["name"].as_str().unwrap_or("");
        for (server, tools) in m["tools"]["mcp"].as_object().into_iter().flatten() {
            for t in tools.as_array().into_iter().flatten() {
                if let Some(tool) = t["n"].as_str() {
                    out.push(Remote {
                        node: node.into(),
                        server: server.clone(),
                        tool: tool.into(),
                        desc: t["d"].as_str().unwrap_or("").into(),
                        schema: t["s"].clone(),
                        read_only: t["ro"] == true,
                    });
                }
            }
        }
    }
    out
}

pub fn remotes() -> Vec<Remote> {
    view().map(|v| remotes_in(&v)).unwrap_or_default()
}

pub fn find(name: &str) -> Option<Remote> {
    if !name.contains("__") {
        return None;
    }
    remotes().into_iter().find(|r| r.name() == name)
}

/// The tools to offer the model. Plan mode offers only what reads; the deny list is honoured.
pub fn schemas(cfg: &Config, plan: bool) -> Vec<Value> {
    remotes()
        .iter()
        .filter(|r| !(plan && !r.read_only) && !blocked(cfg, r))
        .map(|r| {
            let params = if r.schema.is_object() {
                crate::mcp::shrink(&r.schema)
            } else {
                json!({"type": "object", "properties": {}})
            };
            json!({"type": "function", "function": {
                "name": r.name(),
                "description": format!("{} (runs on {})", util::head_chars(&r.desc, 250).trim(), r.node),
                "parameters": params}})
        })
        .collect()
}

/// On the owner's deny list: by the server's name, or the node's and the server's.
pub fn blocked(cfg: &Config, r: &Remote) -> bool {
    [r.server.clone(), format!("{}__{}", r.node, r.server)]
        .iter()
        .any(|k| {
            cfg.mcp_blocked
                .get(k)
                .is_some_and(|l| l.iter().any(|t| t == &r.tool || t == "*"))
        })
}

/// Whether the owner is asked first: always for a tool that is not read-only, and for any the
/// owner named in `mcp_confirm`, unless `mcp_trust` names it.
pub fn needs_confirm(cfg: &Config, r: &Remote) -> bool {
    let names = |l: &[(String, String)]| {
        l.iter().any(|(s, t)| {
            (s == &r.server || s == &format!("{}__{}", r.node, r.server) || s == "fleet")
                && (t == &r.tool || t == "*")
        })
    };
    let trusted = cfg
        .mcp_trust
        .iter()
        .any(|(s, t)| (s == &r.server || s == "fleet") && t == &r.tool);
    !trusted && (!r.read_only || names(&cfg.mcp_confirm))
}

fn out_dir(cfg: &Config) -> PathBuf {
    cfg.comfy_output_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.data_dir.join("fleet-images"))
}

/// One call through the messenger. The answer: ok, text, files, node, ms.
fn post_call(
    node: &str,
    tool: &str,
    args: &Value,
    out: &std::path::Path,
    secs: u64,
) -> Result<Value, String> {
    let token = token().ok_or(
        "the fleet messenger is not set up here (no token): run `tempo-launch fleet serve`",
    )?;
    let body = json!({"node": node, "tool": tool, "args": args, "out_dir": out, "timeout_s": secs});
    let url = format!("{}/call", base());
    let mut resp = util::agent(Duration::from_secs(secs + 60), false)
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Tempo-Token", &token)
        .send(body.to_string())
        .map_err(|e| format!("the fleet messenger did not answer ({e})"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!("the fleet messenger said HTTP {status}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("bad answer from the messenger: {e}"))
}

/// A remote MCP tool, after the agent has asked its questions. The answer is redacted.
pub fn run_remote(cfg: &Config, r: &Remote, args: &Value) -> String {
    let started = Instant::now();
    match post_call(
        &r.node,
        &format!("mcp:{}/{}", r.server, r.tool),
        args,
        &out_dir(cfg),
        300,
    ) {
        Ok(j) => {
            let text = util::redact(j["text"].as_str().unwrap_or(""));
            if j["ok"] == true {
                format!(
                    "{text}\n(Ran on {}, {:.1} s.)",
                    r.node,
                    started.elapsed().as_secs_f64()
                )
            } else {
                format!("ERROR: {text}")
            }
        }
        Err(e) => format!("ERROR: {e}"),
    }
}

/// The node `comfy_image` should go to: the biggest graphics card among those with ComfyUI up.
pub fn pick_comfy(v: &Value) -> Option<String> {
    let mut best: Option<(f64, String)> = None;
    for m in machines(v) {
        let c = &m["tools"]["comfy"];
        if c["up"] != true {
            continue;
        }
        let gb = c["gpu_gb"].as_f64().unwrap_or(0.0);
        let name = m["name"].as_str().unwrap_or("").to_string();
        // ties go to the first name, so the choice does not wander
        if best
            .as_ref()
            .is_none_or(|(g, n)| gb > *g || (gb == *g && name < *n))
        {
            best = Some((gb, name));
        }
    }
    best.map(|b| b.1)
}

fn local_comfy_up(cfg: &Config) -> bool {
    util::http_json(
        &format!("{}/system_stats", cfg.comfy_url),
        None,
        Duration::from_secs(2),
    )
    .is_ok()
}

/// `comfy_image` on another computer, when this one has no ComfyUI. `None`: do it here. The
/// owner is asked first unless `mcp_trust` names `fleet` / `comfy_image`; the deny list
/// (`mcp_blocked`: `fleet` -> `comfy_image`) is honoured.
pub fn comfy_remote(
    cfg: &Config,
    a: &Value,
    ask: &dyn Fn(&str, &str) -> Result<(), String>,
) -> Option<String> {
    if local_comfy_up(cfg) {
        return None;
    }
    let v = view()?;
    let node = pick_comfy(&v)?;
    if cfg
        .mcp_blocked
        .get("fleet")
        .is_some_and(|l| l.iter().any(|t| t == "comfy_image" || t == "*"))
    {
        return Some("refused: remote image making is on your deny list".into());
    }
    let trusted = cfg
        .mcp_trust
        .iter()
        .any(|(s, t)| s == "fleet" && t == "comfy_image");
    let prompt = a["prompt"].as_str().unwrap_or("");
    if !trusted {
        if let Err(no) = ask(&format!("fleet {node}:comfy_image"), prompt) {
            return Some(no);
        }
    }
    let started = Instant::now();
    Some(
        match post_call(&node, "comfy_image", a, &out_dir(cfg), 900) {
            Ok(j) if j["ok"] == true => match j["files"][0].as_str() {
                Some(path) => format!(
                    "image path: {path}\nMade on {node}, {:.1} s.",
                    started.elapsed().as_secs_f64()
                ),
                None => format!(
                    "comfy_image on {node} made no file: {}",
                    util::redact(j["text"].as_str().unwrap_or(""))
                ),
            },
            Ok(j) => format!(
                "comfy_image failed on {node}: {}",
                util::redact(j["text"].as_str().unwrap_or(""))
            ),
            Err(e) => format!("comfy_image failed on {node}: {e}"),
        },
    )
}

/// `tempo-local-agent fleet-run comfy_image`: what another computer's messenger runs here, through
/// this computer's own agent rules (the GPU lease, unloading its models first). Arguments on
/// standard input as JSON; one JSON line out: ok, text, files.
pub fn run_cli(cfg: &Config, tool: &str, args: &Value) -> Value {
    if tool != "comfy_image" {
        return json!({"ok": false, "text": format!("{tool} is not something fleet-run does"), "files": []});
    }
    match crate::tools::comfy_image_cfg(cfg, args) {
        Ok(path) => {
            let p = std::path::Path::new(&path);
            if p.is_absolute() {
                json!({"ok": true, "text": format!("image path: {path}"), "files": [path]})
            } else {
                json!({"ok": false, "text": "ComfyUI saved the picture under a relative path; set comfy_output_dir in the agent's settings so it can be sent", "files": []})
            }
        }
        Err(e) => json!({"ok": false, "text": e, "files": []}),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The environment variables the stand-in messenger is reached by are shared by every test.
    pub(crate) static ENV: Mutex<()> = Mutex::new(());

    pub(crate) fn fleet_json() -> Value {
        json!({"machines": [
            {"name": "studio-box", "up": true, "tools": {
                "comfy": {"up": true, "gpu_gb": 24.0, "checkpoints": ["a"]},
                "mcp": {"notes": [
                    {"n": "search", "ro": true, "d": "Find a note", "s": {"type": "object", "properties": {"q": {"type": "string"}}}},
                    {"n": "append", "ro": false, "d": "Add to a note", "s": {"type": "object"}}]}}},
            {"name": "small-box", "up": true, "tools": {"comfy": {"up": true, "gpu_gb": 8.0}}},
            {"name": "asleep", "up": false, "tools": {"comfy": {"up": true, "gpu_gb": 48.0}, "mcp": {"x": [{"n": "t"}]}}}
        ]})
    }

    fn cfg() -> Config {
        let mut c = Config::defaults(Some(
            util::testutil::tmpdir("fleet")
                .to_string_lossy()
                .into_owned(),
        ));
        c.comfy_url = "http://127.0.0.1:1".into(); // no ComfyUI here
        c
    }

    #[test]
    fn tools_appear_as_node_server_tool_for_computers_that_are_up() {
        let r = remotes_in(&fleet_json());
        let names: Vec<String> = r.iter().map(Remote::name).collect();
        assert_eq!(
            names,
            ["studio-box__notes__search", "studio-box__notes__append"]
        );
        assert!(r[0].read_only && !r[1].read_only);
    }

    #[test]
    fn the_biggest_card_gets_the_picture_and_a_computer_that_is_away_gets_nothing() {
        assert_eq!(pick_comfy(&fleet_json()).as_deref(), Some("studio-box"));
        let none = json!({"machines": [{"name": "a", "up": true, "tools": {}}]});
        assert_eq!(pick_comfy(&none), None);
    }

    #[test]
    fn plan_mode_the_deny_list_and_the_questions_apply_to_a_remote_tool() {
        let r = remotes_in(&fleet_json());
        let mut c = cfg();
        // a tool that is not read-only always asks; a read-only one asks only when named
        assert!(!needs_confirm(&c, &r[0]) && needs_confirm(&c, &r[1]));
        c.mcp_confirm.push(("notes".into(), "search".into()));
        assert!(needs_confirm(&c, &r[0]));
        c.mcp_trust.push(("notes".into(), "append".into()));
        assert!(!needs_confirm(&c, &r[1]), "the owner's own trust list wins");
        // the deny list, by server or by node and server
        assert!(!blocked(&c, &r[0]));
        c.mcp_blocked
            .insert("studio-box__notes".into(), vec!["append".into()]);
        assert!(blocked(&c, &r[1]) && !blocked(&c, &r[0]));
        c.mcp_blocked.insert("notes".into(), vec!["*".into()]);
        assert!(blocked(&c, &r[0]));
    }

    /// A stand-in for `fleet serve`: answers /fleet.json and /call, remembering what it was sent.
    pub(crate) fn fake_messenger(
        files: Vec<String>,
    ) -> (u16, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let (s2, c2) = (seen.clone(), calls.clone());
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                // read the head, then as much body as it promises
                let mut got: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    got.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&got).into_owned();
                    let Some(end) = text.find("\r\n\r\n") else {
                        if n == 0 {
                            break;
                        }
                        continue;
                    };
                    let want: usize = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if got.len() >= end + 4 + want || n == 0 {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&got).into_owned();
                let body = if req.starts_with("GET /fleet.json") {
                    fleet_json().to_string()
                } else {
                    c2.fetch_add(1, Ordering::SeqCst);
                    s2.lock().unwrap().push(req.clone());
                    json!({"ok": true, "text": "echo: password=hunter2xyz", "files": files, "node": "studio-box", "ms": 5}).to_string()
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (port, seen, calls)
    }

    #[test]
    fn a_remote_picture_asks_first_and_the_status_line_says_where_it_was_made() {
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let home = util::testutil::tmpdir("fleethome");
        std::fs::write(home.join("fleet-local.token"), "t".repeat(64)).unwrap();
        let (port, seen, calls) = fake_messenger(vec!["/tmp/images/tempo-agent_00001_.png".into()]);
        // these two settings are the only way the test reaches its stand-in
        std::env::set_var("TEMPO_FLEET_PORT", port.to_string());
        std::env::set_var("TEMPO_LAUNCH_HOME", &home);
        forget_view();
        let c = cfg();
        let asked = Mutex::new(Vec::new());
        let ask = |what: &str, _: &str| {
            asked.lock().unwrap().push(what.to_string());
            Ok(())
        };
        let r = comfy_remote(&c, &json!({"prompt": "a harbour at dawn"}), &ask).unwrap();
        assert!(
            r.starts_with("image path: /tmp/images/tempo-agent_00001_.png\nMade on studio-box, "),
            "{r}"
        );
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            ["fleet studio-box:comfy_image"]
        );
        let req = seen.lock().unwrap()[0].clone();
        assert!(
            req.contains("X-Tempo-Token: tttt")
                || req.to_lowercase().contains("x-tempo-token: tttt"),
            "{req}"
        );
        assert!(req.contains("\"tool\":\"comfy_image\"") && req.contains("a harbour at dawn"));
        // a no from the owner stops it before anything is sent
        let deny = |_: &str, _: &str| Err("denied by user".to_string());
        assert_eq!(
            comfy_remote(&c, &json!({"prompt": "x"}), &deny).as_deref(),
            Some("denied by user")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // the deny list stops it too
        let mut blocked_cfg = cfg();
        blocked_cfg
            .mcp_blocked
            .insert("fleet".into(), vec!["comfy_image".into()]);
        assert!(comfy_remote(&blocked_cfg, &json!({"prompt": "x"}), &ask)
            .unwrap()
            .contains("deny list"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // a remote tool's answer is redacted, and says where it ran
        let r = &remotes()[0];
        let out = run_remote(&c, r, &json!({"q": "x"}));
        assert!(
            out.contains("Ran on studio-box") && !out.contains("hunter2xyz"),
            "{out}"
        );
        // with this computer's own ComfyUI up, nothing is sent anywhere
        let comfy = TcpListener::bind("127.0.0.1:0").unwrap();
        let cport = comfy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in comfy.incoming().flatten() {
                let mut b = [0u8; 512];
                let _ = s.read(&mut b);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });
        let mut local = cfg();
        local.comfy_url = format!("http://127.0.0.1:{cport}");
        assert_eq!(comfy_remote(&local, &json!({"prompt": "x"}), &ask), None);
        // and with no messenger, nothing is added at all
        std::env::set_var("TEMPO_FLEET_PORT", "1");
        forget_view();
        assert!(remotes().is_empty() && schemas(&c, false).is_empty());
        assert_eq!(comfy_remote(&c, &json!({"prompt": "x"}), &ask), None);
        std::env::remove_var("TEMPO_FLEET_PORT");
        std::env::remove_var("TEMPO_LAUNCH_HOME");
        forget_view();
    }

    #[test]
    fn fleet_run_refuses_anything_but_the_picture_tool_and_needs_a_full_path() {
        let c = cfg();
        let r = run_cli(&c, "run_command", &json!({}));
        assert_eq!(r["ok"], false);
        assert_eq!(r["files"], json!([]));
    }
}
