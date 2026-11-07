//! MCP server: newline-delimited JSON-RPC 2.0 on stdin/stdout with `initialize`, `tools/list`,
//! `tools/call` (and `ping`). Tools: studio_analyze, studio_master, studio_mix, reaper_locate,
//! reaper_open, reaper_build_session. Every path must be absolute and inside an allowed folder
//! (`--allow-root`, default the current folder). Results are JSON text. Only JSON-RPC goes to
//! stdout; nothing else is ever printed there.
use crate::config::Engine;
use crate::master::{self, MasterSettings};
use crate::ops::{self, Ctx, MasterArgs, MixArgs};
use crate::util::Roots;
use crate::wav::OutFormat;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// What `reaper_open` accepts from MCP: a project, or audio and MIDI. Never a script or program.
const OPENABLE: [&str; 11] = [
    "rpp", "wav", "flac", "mp3", "ogg", "aif", "aiff", "m4a", "mid", "midi", "opus",
];

pub const PROTOCOL: &str = "2024-11-05";
const SUPPORTED: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

fn engine_prop() -> Value {
    json!({"type": "string", "enum": ["auto", "reaper", "builtin"],
           "description": "auto (default) and builtin: the built-in engine, no window. reaper: opens REAPER's window; use it only when the user asks for REAPER"})
}

fn preset_prop(what: &str) -> Value {
    json!({"type": "string", "enum": ["streaming", "apple", "club", "podcast", "broadcast"],
           "description": format!("{what}: streaming -14 LUFS/-1 dBTP, apple -16/-1, club -9/-0.3, podcast -16/-1, broadcast -23/-1")})
}

pub fn tools() -> Value {
    json!([
        {
            "name": "studio_analyze",
            "description": "Measure WAV files (ITU-R BS.1770-4 / EBU R128): integrated loudness, momentary and short-term max, loudness range, sample peak and true peak.",
            "inputSchema": {"type": "object", "properties": {
                "paths": {"type": "array", "items": {"type": "string"}, "description": "Absolute paths of WAV files"}
            }, "required": ["paths"]}
        },
        {
            "name": "studio_master",
            "description": "Master a song to a loudness target with a true-peak ceiling. Uses REAPER (ReaLimit) when installed, else the built-in limiter; the result is measured again and reported. Never overwrites a file.",
            "inputSchema": {"type": "object", "properties": {
                "input": {"type": "string", "description": "Absolute path of the WAV to master"},
                "output": {"type": "string", "description": "Absolute output path (default <input>-master.wav; a numbered name if taken)"},
                "preset": preset_prop("Target preset (default streaming)"),
                "target_lufs": {"type": "number", "description": "Integrated loudness target, overrides the preset"},
                "ceiling_dbtp": {"type": "number", "description": "True-peak ceiling, overrides the preset"},
                "engine": engine_prop(),
                "format": {"type": "string", "enum": ["pcm24", "float32"]},
                "force": {"type": "boolean", "description": "Replace an existing output file (never an input)"}
            }, "required": ["input"]}
        },
        {
            "name": "studio_mix",
            "description": "Mix stems (a folder of WAVs or a list) into a stereo mix: each stem gain-staged by loudness and name role (kick, drums, bass, vocal, guitar, keys, pad, lead, fx), centred, with -6 dBFS headroom; writes the mix WAV and a JSON plan. With REAPER it also builds and renders a REAPER session. Optionally masters the mix.",
            "inputSchema": {"type": "object", "properties": {
                "stems": {"type": "array", "items": {"type": "string"}, "description": "Absolute paths: stem WAV files or folders of them"},
                "output": {"type": "string", "description": "Absolute path of the mix WAV"},
                "master_preset": preset_prop("Also master the mix"),
                "engine": engine_prop(),
                "reference_lufs": {"type": "number", "description": "Loudness each stem is staged to (default -23)"},
                "format": {"type": "string", "enum": ["pcm24", "float32"]},
                "force": {"type": "boolean"}
            }, "required": ["stems", "output"]}
        },
        {
            "name": "reaper_locate",
            "description": "Find the REAPER program (--reaper, env TEMPO_REAPER, config reaper_exe, standard install paths, PATH). Lists every path tried when not found.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "reaper_open",
            "description": "Open REAPER visibly with a project (.rpp), a media file or a ReaScript (.lua).",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Absolute path of the file to open"}
            }, "required": ["path"]}
        },
        {
            "name": "reaper_build_session",
            "description": "Turn a mix plan JSON (written by studio_mix) into a REAPER session script: one track per stem with volume and media, ReaEQ high-pass, gentle ReaComp on vocals and drums, ReaLimit on the master, WAV 24-bit render settings; saves the .rpp. With run=true it opens REAPER with the script and waits for its status.",
            "inputSchema": {"type": "object", "properties": {
                "plan": {"type": "string", "description": "Absolute path of the plan JSON"},
                "output": {"type": "string", "description": "Absolute path of the .lua script (default <plan>-session.lua)"},
                "render": {"type": "boolean", "description": "The script also renders"},
                "close": {"type": "boolean", "description": "The script closes the project tab after rendering"},
                "run": {"type": "boolean", "description": "Open REAPER with the script now and wait for its status"}
            }, "required": ["plan"]}
        }
    ])
}

fn ok_resp(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err_resp(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize_result(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(|x| x.as_str())
        .unwrap_or(PROTOCOL);
    let version = if SUPPORTED.contains(&asked) {
        asked
    } else {
        PROTOCOL
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "tempo-studio", "version": env!("CARGO_PKG_VERSION")},
        "instructions": "Perennos Tempo studio: measure loudness (studio_analyze), master (studio_master), mix stems (studio_mix) and drive REAPER (reaper_locate, reaper_open, reaper_build_session). Paths must be absolute and inside the allowed folders. Outputs never overwrite existing files.",
    })
}

/// Answer one message; `None` for notifications.
pub fn handle(ctx: &Ctx, roots: &Roots, m: &Value) -> Option<Value> {
    let id = m.get("id").cloned();
    let Some(method) = m.get("method").and_then(|x| x.as_str()) else {
        return id.map(|id| err_resp(id, -32600, "invalid request: no method"));
    };
    let id = id?;
    let params = m.get("params").cloned().unwrap_or_else(|| json!({}));
    Some(match method {
        "initialize" => ok_resp(id, initialize_result(&params)),
        "ping" => ok_resp(id, json!({})),
        "tools/list" => ok_resp(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let known = tools()
                .as_array()
                .is_some_and(|t| t.iter().any(|x| x["name"] == name));
            if !known {
                return Some(err_resp(id, -32602, &format!("unknown tool: {name}")));
            }
            let args = params
                .get("arguments")
                .cloned()
                .filter(|a| a.is_object())
                .unwrap_or_else(|| json!({}));
            // A bug in a tool answers with an error; it must not end the server.
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                call_tool(ctx, roots, name, &args)
            }));
            let (v, is_error) = match run {
                Ok(Ok(v)) => (v, false),
                Ok(Err(e)) => (json!({ "error": e }), true),
                Err(_) => (
                    json!({ "error": format!("internal error in {name}; the server is still running") }),
                    true,
                ),
            };
            let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
            ok_resp(
                id,
                json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
            )
        }
        other => err_resp(id, -32601, &format!("method not found: {other}")),
    })
}

fn call_tool(ctx: &Ctx, roots: &Roots, name: &str, a: &Value) -> Result<Value, String> {
    let s = |k: &str| -> Result<Option<String>, String> {
        match a.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(x)) => Ok(Some(x.clone())),
            Some(_) => Err(format!("{k} must be a string")),
        }
    };
    let num = |k: &str| -> Result<Option<f64>, String> {
        match a.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(x) => x.as_f64().map(Some).ok_or(format!("{k} must be a number")),
        }
    };
    let flag = |k: &str| a.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    let path = |k: &str| -> Result<PathBuf, String> {
        roots.check(&s(k)?.ok_or(format!("{k} is required"))?)
    };
    let opt_path =
        |k: &str| -> Result<Option<PathBuf>, String> { s(k)?.map(|p| roots.check(&p)).transpose() };
    let paths = |k: &str| -> Result<Vec<PathBuf>, String> {
        let list = a
            .get(k)
            .and_then(|x| x.as_array())
            .filter(|l| !l.is_empty())
            .ok_or(format!("{k} must be a non-empty list of absolute paths"))?;
        list.iter()
            .map(|p| {
                p.as_str()
                    .ok_or(format!("{k} must hold strings"))
                    .and_then(|p| roots.check(p))
            })
            .collect()
    };
    let engine = || -> Result<Engine, String> {
        s("engine")?
            .map(|e| Engine::parse(&e))
            .unwrap_or(Ok(ctx.cfg.engine))
    };
    let format = || -> Result<OutFormat, String> {
        s("format")?
            .map(|f| OutFormat::parse(&f))
            .unwrap_or(Ok(ctx.cfg.output_format))
    };
    match name {
        "studio_analyze" => Ok(ops::analyze(&paths("paths")?).0),
        "studio_master" => {
            let mut settings = ops::settings_from(
                s("preset")?.as_deref(),
                num("target_lufs")?,
                num("ceiling_dbtp")?,
                &ctx.cfg.preset,
            )?;
            settings.format = format()?;
            ops::master(
                ctx,
                &MasterArgs {
                    input: path("input")?,
                    out: opt_path("output")?,
                    settings,
                    engine: engine()?,
                    force: flag("force"),
                },
            )
        }
        "studio_mix" => {
            let master = s("master_preset")?
                .map(|p| master::preset(&p).map(|p| MasterSettings::from_preset(&p)))
                .transpose()?;
            let master = master.map(|mut m| {
                m.format = format().unwrap_or(m.format);
                m
            });
            ops::mix(
                ctx,
                &MixArgs {
                    inputs: paths("stems")?,
                    out: path("output")?,
                    engine: engine()?,
                    master,
                    reference_lufs: num("reference_lufs")?.unwrap_or(ctx.cfg.reference_lufs),
                    format: format()?,
                    force: flag("force"),
                },
            )
        }
        "reaper_locate" => Ok(ops::reaper_locate(ctx).0),
        "reaper_open" => {
            let p = path("path")?;
            // REAPER runs a script it is started with, so only projects and audio go through here;
            // `reaper_build_session` is the way to run a script (one this tool wrote).
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if !OPENABLE.contains(&ext.as_str()) {
                return Err(format!(
                    "{} is not opened from here: reaper_open takes a project (.rpp) or an audio file ({}). A script runs with reaper_build_session",
                    p.display(),
                    OPENABLE[1..].join(", ")
                ));
            }
            ops::reaper_open(ctx, &p)
        }
        "reaper_build_session" => {
            let plan = path("plan")?;
            let script = opt_path("output")?.unwrap_or_else(|| ops::default_script_for(&plan));
            let allow = |p: &Path| roots.check(&p.to_string_lossy()).map(|_| ());
            let session = ops::session_for(&plan, &script, flag("render"), flag("close"), &allow)?;
            ops::reaper_build_session(ctx, &session, &script, flag("run"))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Serve until stdin closes. Each line is one message (or a batch array).
pub fn serve(
    ctx: &Ctx,
    roots: &Roots,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<(), String> {
    for line in input.lines() {
        let line = line.map_err(|e| format!("reading stdin: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => Some(err_resp(Value::Null, -32700, &format!("parse error: {e}"))),
            Ok(Value::Array(batch)) => {
                let rs: Vec<Value> = batch.iter().filter_map(|m| handle(ctx, roots, m)).collect();
                (!rs.is_empty()).then_some(Value::Array(rs))
            }
            Ok(m) => handle(ctx, roots, &m),
        };
        if let Some(r) = reply {
            writeln!(output, "{r}")
                .and_then(|_| output.flush())
                .map_err(|e| format!("writing stdout: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::testctx::{ctx, found, not_found};
    use crate::reaper::testfake::{tone_at, FakeReaper};
    use crate::util::testutil::tmpdir;

    fn run(c: &Ctx, roots: &Roots, lines: &[Value]) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut out = Vec::new();
        serve(c, roots, std::io::Cursor::new(input), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn call(id: u64, name: &str, args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
    }

    fn text(r: &Value) -> Value {
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn handshake_list_and_protocol_errors() {
        let d = tmpdir("mcp1");
        let fake = FakeReaper::new(-14.0);
        let c = ctx(&fake, &not_found);
        let roots = Roots::new(std::slice::from_ref(&d));
        let out = run(
            &c,
            &roots,
            &[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
                json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope"}}),
                json!({"jsonrpc": "2.0", "id": 5, "method": "ping"}),
                json!([{"jsonrpc": "2.0", "id": 6, "method": "ping"}, {"jsonrpc": "2.0", "method": "notifications/x"}]),
                json!({"jsonrpc": "2.0", "id": 7}),
            ],
        );
        assert_eq!(out.len(), 7, "the notification gets no answer: {out:?}");
        assert_eq!(out[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(out[0]["result"]["serverInfo"]["name"], "tempo-studio");
        let names: Vec<&str> = out[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "studio_analyze",
                "studio_master",
                "studio_mix",
                "reaper_locate",
                "reaper_open",
                "reaper_build_session"
            ]
        );
        assert_eq!(out[2]["error"]["code"], -32601);
        assert_eq!(out[3]["error"]["code"], -32602);
        assert_eq!(out[4]["result"], json!({}));
        assert_eq!(out[5][0]["id"], 6);
        assert_eq!(out[6]["error"]["code"], -32600);
        // Garbage and an unknown protocol version.
        let mut o = Vec::new();
        serve(&c, &roots, std::io::Cursor::new("{not json\n\n"), &mut o).unwrap();
        let r: Value = serde_json::from_slice(&o).unwrap();
        assert_eq!(
            (r["error"]["code"].as_i64(), &r["id"]),
            (Some(-32700), &Value::Null)
        );
        let out = run(
            &c,
            &roots,
            &[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}),
            ],
        );
        assert_eq!(out[0]["result"]["protocolVersion"], PROTOCOL);
    }

    #[test]
    fn tools_check_paths_and_return_json_text() {
        let d = tmpdir("mcp2");
        let inside = d.join("in");
        std::fs::create_dir_all(&inside).unwrap();
        let song = inside.join("song.wav");
        tone_at(&song, -20.0);
        let outside = d.join("outside.wav");
        tone_at(&outside, -20.0);
        let fake = FakeReaper::new(-14.0);
        let c = ctx(&fake, &not_found);
        let roots = Roots::new(std::slice::from_ref(&inside));
        let s = song.display().to_string();
        let out = run(
            &c,
            &roots,
            &[
                call(1, "studio_analyze", json!({"paths": [s]})),
                call(
                    2,
                    "studio_analyze",
                    json!({"paths": [outside.display().to_string()]}),
                ),
                call(3, "studio_analyze", json!({"paths": ["in/song.wav"]})),
                call(
                    4,
                    "studio_master",
                    json!({"input": s, "engine": "builtin", "preset": "apple"}),
                ),
                call(
                    5,
                    "studio_master",
                    json!({"input": s, "output": outside.with_file_name("x.wav").display().to_string()}),
                ),
                call(6, "reaper_locate", json!({})),
                call(
                    7,
                    "studio_master",
                    json!({"input": s, "target_lufs": "loud"}),
                ),
                call(8, "studio_analyze", json!({})),
            ],
        );
        let a = text(&out[0]);
        assert_eq!(out[0]["result"]["isError"], false);
        assert!(
            (a["files"][0]["loudness"]["integrated_lufs"]
                .as_f64()
                .unwrap()
                + 20.0)
                .abs()
                < 0.1
        );
        for (i, want) in [
            (1, "outside the allowed folders"),
            (2, "absolute"),
            (4, "outside the allowed folders"),
            (6, "must be a number"),
            (7, "non-empty list"),
        ] {
            assert_eq!(out[i]["result"]["isError"], true, "{i}");
            assert!(
                text(&out[i])["error"].as_str().unwrap().contains(want),
                "{i}: {}",
                out[i]
            );
        }
        let m = text(&out[3]);
        assert_eq!(
            (m["engine"].as_str(), m["within_target"].as_bool()),
            (Some("builtin"), Some(true)),
            "{m:#}"
        );
        assert_eq!(m["settings"]["preset"], "apple");
        assert_eq!(
            m["path"],
            inside.join("song-master.wav").display().to_string()
        );
        assert_eq!(text(&out[5])["found"], false);
    }

    #[test]
    fn mix_and_session_tools() {
        let d = tmpdir("mcp3");
        let stems = d.join("stems");
        std::fs::create_dir_all(&stems).unwrap();
        tone_at(&stems.join("kick.wav"), -12.0);
        tone_at(&stems.join("vox.wav"), -18.0);
        let fake = FakeReaper::new(-14.0);
        let c = ctx(&fake, &found);
        let roots = Roots::new(std::slice::from_ref(&d));
        let out = run(
            &c,
            &roots,
            &[
                call(
                    1,
                    "studio_mix",
                    json!({"stems": [stems.display().to_string()], "output": d.join("mix.wav").display().to_string(), "engine": "builtin"}),
                ),
                call(
                    2,
                    "reaper_build_session",
                    json!({"plan": d.join("mix.plan.json").display().to_string(), "render": true}),
                ),
                call(
                    3,
                    "reaper_open",
                    json!({"path": d.join("mix.wav").display().to_string()}),
                ),
            ],
        );
        let m = text(&out[0]);
        assert_eq!(out[0]["result"]["isError"], false, "{m:#}");
        assert!(m["mix"]["loudness"]["sample_peak_dbfs"].as_f64().unwrap() < -6.0);
        let b = text(&out[1]);
        assert_eq!(
            b["script"],
            d.join("mix-session.lua").display().to_string(),
            "{b:#}"
        );
        assert_eq!(b["session"]["render"], true);
        assert_eq!(text(&out[2])["pid"], 4242);
    }
}
