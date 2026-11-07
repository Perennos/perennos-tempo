//! `tempo-local-agent soundcheck-mcp`: the soundcheck as an MCP tool on stdio, so Claude Code,
//! Codex and other MCP clients can ask Tempo for a local review.
//!
//! The style follows the launcher's setup server: newline-delimited JSON-RPC, one reply per
//! request, no reply to notifications. The one tool only reads. It cannot write a SARIF file,
//! and it will not look outside the folder the server was started in.

use crate::soundcheck::{self, Cli, Llm, Options, Source};
use crate::util;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub const TOOL: &str = "tempo_soundcheck";

pub struct Server<'a> {
    /// The folder the server was started in; every path a client names must stay inside it.
    pub root: PathBuf,
    pub llm: Option<&'a dyn Llm>,
    pub chunk_chars: usize,
}

fn tool_definition() -> Value {
    json!({
        "name": TOOL,
        "description": "Tempo Soundcheck: review a code change (or a whole folder) with local scanners and local models, with nothing leaving this computer. Finds secrets, dangerous calls, risky dependency changes and weakened tests, then asks the model through four lenses (does it work, security, do the tests prove it, simplicity) and tries to refute each finding before keeping it. Returns a verdict and findings with evidence. Read-only.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Folder to review, inside the folder the server started in. Default: that folder."},
                "base": {"type": "string", "description": "Review this branch against a ref (REF...HEAD), such as main."},
                "staged": {"type": "boolean", "description": "Review only staged changes."},
                "patch": {"type": "string", "description": "Review a unified diff file."},
                "all": {"type": "string", "description": "Audit every file under this folder, in pieces."},
                "focus": {"type": "string", "description": "What to pay special attention to."},
                "verbose": {"type": "boolean", "description": "Also list findings the second listen set aside."},
                "no_model": {"type": "boolean", "description": "Scanners only; no model is asked."},
                "format": {"type": "string", "enum": ["markdown", "json"], "description": "Report format. Default markdown."}
            }
        },
        "annotations": {
            "title": "Tempo Soundcheck",
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

/// `p` joined to `base`, as long as the real place it names is still inside `root`.
fn inside(root: &Path, base: &Path, p: &str) -> Result<PathBuf, String> {
    let full = util::resolve(&base.join(p));
    if full.starts_with(util::resolve(root)) {
        Ok(full)
    } else {
        Err(format!(
            "{p} is outside the folder this server was started in ({}). Name a path inside it.",
            root.display()
        ))
    }
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

impl Server<'_> {
    fn arg<'v>(args: &'v Value, k: &str) -> Option<&'v str> {
        args.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    fn flag(args: &Value, k: &str) -> bool {
        args.get(k).and_then(Value::as_bool).unwrap_or(false)
    }

    /// Turn the tool's arguments into the same options the command line uses.
    fn cli_from(&self, args: &Value) -> Result<(Cli, PathBuf), String> {
        let cwd = match Self::arg(args, "path") {
            Some(p) => inside(&self.root, &self.root, p)?,
            None => self.root.clone(),
        };
        let mut opts = Options {
            chunk_chars: self.chunk_chars,
            ..Options::default()
        };
        let mut chosen = 0;
        if Self::flag(args, "staged") {
            opts.source = Source::Staged;
            chosen += 1;
        }
        if let Some(r) = Self::arg(args, "base") {
            opts.source = Source::Base(r.to_string());
            chosen += 1;
        }
        if let Some(p) = Self::arg(args, "patch") {
            opts.source = Source::Patch(inside(&self.root, &cwd, p)?);
            chosen += 1;
        }
        if let Some(p) = Self::arg(args, "all") {
            opts.source = Source::All(inside(&self.root, &cwd, p)?);
            chosen += 1;
        }
        if chosen > 1 {
            return Err("Choose one of staged, base, patch or all.".into());
        }
        opts.focus = Self::arg(args, "focus").unwrap_or("").to_string();
        opts.verbose = Self::flag(args, "verbose");
        opts.no_model = Self::flag(args, "no_model");
        let json = Self::arg(args, "format") == Some("json");
        Ok((
            Cli {
                opts,
                json,
                sarif: None,
                fail_on: None,
                model: None,
                help: false,
            },
            cwd,
        ))
    }

    fn call(&self, args: &Value) -> Result<String, String> {
        let (cli, cwd) = self.cli_from(args)?;
        let report = soundcheck::soundcheck(&cli.opts, &cwd, self.llm, &mut |_| {})?;
        Ok(soundcheck::render(&cli, &report))
    }

    /// The reply to one message, or None for a notification.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let fail = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        Some(
            match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                "initialize" => ok(json!({
                    "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05"),
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "tempo-soundcheck", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Tempo Soundcheck reviews code changes on this computer with local models. Call tempo_soundcheck before you tell the owner a change is done, and show them the verdict and any findings. It only reads."
                })),
                "ping" => ok(json!({})),
                "tools/list" => ok(json!({"tools": [tool_definition()]})),
                "tools/call" => {
                    if params.get("name").and_then(Value::as_str) != Some(TOOL) {
                        return Some(ok(text_result(
                            format!("unknown tool; this server has {TOOL}"),
                            true,
                        )));
                    }
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    ok(match self.call(&args) {
                        Ok(t) => text_result(t, false),
                        Err(e) => text_result(e, true),
                    })
                }
                "resources/list" => ok(json!({"resources": []})),
                "prompts/list" => ok(json!({"prompts": []})),
                _ => fail(-32601, "method not found"),
            },
        )
    }

    /// Serve until the client closes the input.
    pub fn run(&self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(msg) if msg.is_object() => self.handle(&msg),
                _ => Some(
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}),
                ),
            };
            if let Some(r) = reply {
                writeln!(output, "{r}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::soundcheck::OllamaLlm;
    use crate::util::testutil::*;
    use std::process::Command;

    fn repo_with_eval() -> PathBuf {
        let d = tmpdir("sc-mcp");
        let git = |args: &[&str]| {
            let st = Command::new("git")
                .current_dir(&d)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
                .args(args)
                .status()
                .unwrap();
            assert!(st.success());
        };
        git(&["init", "-q"]);
        std::fs::write(d.join("a.py"), "x = 1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(d.join("a.py"), "x = eval(text)\n").unwrap();
        d
    }

    fn talk(server: &Server, lines: &[Value]) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut out = vec![];
        server.run(input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn call(args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": TOOL, "arguments": args}})
    }

    #[test]
    fn lists_one_read_only_tool() {
        let server = Server {
            root: repo_with_eval(),
            llm: None,
            chunk_chars: 8000,
        };
        let replies = talk(
            &server,
            &[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
            ],
        );
        assert_eq!(replies.len(), 3, "a notification gets no reply");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            replies[0]["result"]["serverInfo"]["name"],
            "tempo-soundcheck"
        );
        let tools = replies[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "tempo_soundcheck");
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
        assert_eq!(tools[0]["annotations"]["destructiveHint"], false);
        assert!(
            tools[0]["inputSchema"]["properties"].get("sarif").is_none(),
            "it cannot write files"
        );
        assert_eq!(replies[2]["error"]["code"], -32601);
    }

    #[test]
    fn runs_a_soundcheck_and_answers_in_markdown_or_json() {
        let server = Server {
            root: repo_with_eval(),
            llm: None,
            chunk_chars: 8000,
        };
        let r = talk(
            &server,
            &[
                call(json!({"no_model": true})),
                call(json!({"no_model": true, "format": "json"})),
            ],
        );
        let text = |v: &Value| {
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(text(&r[0]).contains("Feedback in the monitors") && text(&r[0]).contains("eval"));
        assert_eq!(r[0]["result"]["isError"], false);
        let v: Value = serde_json::from_str(&text(&r[1])).unwrap();
        assert_eq!(v["verdict"], "needs_changes");
    }

    #[test]
    fn asks_the_local_model_when_one_is_given() {
        let m = Mock::start(|r| {
            if r.path != "/api/chat" {
                return (404, "{}".into());
            }
            text_resp("{\"findings\":[]}")
        });
        let mut cfg = Config::defaults(Some(tmpdir("sc-data").to_string_lossy().into_owned()));
        cfg.ollama_url = m.url.clone();
        cfg.models = vec!["m".into()];
        let llm = OllamaLlm::new(&cfg, cfg.models.clone());
        let server = Server {
            root: repo_with_eval(),
            llm: Some(&llm),
            chunk_chars: 8000,
        };
        let r = talk(&server, &[call(json!({"format": "json"}))]);
        let v: Value =
            serde_json::from_str(r[0]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(v["model_calls"], 4);
        assert_eq!(m.bodies("/api/chat").len(), 4);
    }

    #[test]
    fn stays_inside_its_folder_and_reports_errors_as_tool_errors() {
        let root = repo_with_eval();
        let server = Server {
            root: root.clone(),
            llm: None,
            chunk_chars: 8000,
        };
        let r = talk(
            &server,
            &[
                call(json!({"path": "..", "no_model": true})),
                call(json!({"patch": "../../etc/passwd"})),
                call(json!({"staged": true, "base": "main"})),
                json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "other"}}),
            ],
        );
        for reply in &r {
            assert_eq!(reply["result"]["isError"], true, "{reply}");
        }
        assert!(r[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("outside the folder"));
        // a bad line gets a parse error and the server keeps going
        let mut out = vec![];
        server.run("not json\n".as_bytes(), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("-32700"));
    }
}
