//! `tempo-launch retune bench <model>`: the bench, as an MCP server on stdio (the same line-by-line
//! JSON-RPC the Crate speaks), with a small Python helper behind it.
//!
//! The tool set below is fixed, and that is on purpose: a cloud model must never get a shell on the
//! owner's machine. Nothing here takes a path, a command or a script. Every argument is checked and
//! rebuilt from the keys a tool knows before the helper sees it, the helper checks again, and the
//! helper has no door that runs code. What the model can do is read numbers, run forward passes
//! while the budget lasts, and write edits to a list that sits beside the untouched original.
//!
//! The held-out set is not reachable from here: no tool names a file, `practice_set` only opens
//! the owner's `practice/` folder (or Woodshed takes) and leaves out anything that is in the
//! held-out set, and every prompt it hands out is written down so `retune check` can prove it.

use super::session::{self, norm, plain, Session};
use crate::bank::mcp::{o, serve, tool_json, Spec};
use crate::setup::json::{parse, J};
use crate::woodshed::takes;
use crate::Ctx;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const SERVER_NAME: &str = "tempo-retune";
const READ_CAP: usize = 20_000;
const MAX_PROMPTS: usize = 512;
const MAX_PROMPT_CHARS: usize = 8_000;
const MAX_TOKENS: i64 = 2_048;
const MAX_COLUMNS: usize = 4_096;

const NO_IDS: &[&str] = &[];

macro_rules! tool {
    ($name:expr, $title:expr, $ro:expr, $desc:expr, $schema:expr) => {
        Spec {
            name: $name,
            title: $title,
            ids: NO_IDS,
            read_only: $ro,
            description: $desc,
            schema: $schema,
        }
    };
}

pub const TOOLS: &[Spec] = &[
    tool!("model_info", "The model's layout", true,
        "Architecture, layer count, and every tensor's name, shape and number format. Start here.",
        r#"{"type":"object","properties":{},"additionalProperties":false}"#),
    tool!("tensor_stats", "A tensor's numbers in summary", true,
        "Norm, mean, std, largest value and top singular values of one tensor (with your edits on it). With `vector` (one number per row of a matrix) it also lists the 16 columns that write that direction the most: the ones to ablate.",
        r#"{"type":"object","properties":{"tensor":{"type":"string"},"vector":{"type":"array","items":{"type":"number"},"maxItems":20000}},"required":["tensor"],"additionalProperties":false}"#),
    tool!("read_slice", "Read some weights", true,
        "Up to 20000 numbers from a tensor. `index` has one part per dimension, like \"0:4, 100:164\".",
        r#"{"type":"object","properties":{"tensor":{"type":"string"},"index":{"type":"string"}},"required":["tensor","index"],"additionalProperties":false}"#),
    tool!("top_directions", "Strongest directions of a matrix", true,
        "The top singular directions of a matrix. side=output: directions it writes into (for a layer's output matrix that is the residual stream); side=input: directions it reads.",
        r#"{"type":"object","properties":{"tensor":{"type":"string"},"k":{"type":"integer","minimum":1,"maximum":16,"default":3},"side":{"type":"string","enum":["output","input"],"default":"output"}},"required":["tensor"],"additionalProperties":false}"#),
    tool!("forward", "Run the model on prompts", false,
        "Runs the model on up to 512 prompts and returns each answer with whether it hit the token limit or looped, plus loop and length stats. Costs 1 forward pass of the budget. Your edits are in.",
        r#"{"type":"object","properties":{"prompts":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":512},"max_tokens":{"type":"integer","minimum":1,"maximum":2048,"default":256}},"required":["prompts"],"additionalProperties":false}"#),
    tool!("activation_diff", "Compare the residual stream on two prompt sets", false,
        "Mean residual-stream difference (last token, after `layer`) between two prompt sets, as a unit direction and its size, plus the strongest principal directions of both sets together. Costs 2 forward passes.",
        r#"{"type":"object","properties":{"prompts_a":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":512},"prompts_b":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":512},"layer":{"type":"integer","minimum":0}},"required":["prompts_a","prompts_b","layer"],"additionalProperties":false}"#),
    tool!("ablate_direction", "Take a direction out of columns", false,
        "On the working copy: subtract the direction `vector` (one number per row, any length: it is normalised) from each listed column of a matrix, so those columns no longer write it. Recorded in the edit list.",
        r#"{"type":"object","properties":{"tensor":{"type":"string"},"vector":{"type":"array","items":{"type":"number"},"maxItems":20000},"columns":{"type":"array","items":{"type":"integer","minimum":0},"minItems":1,"maxItems":4096}},"required":["tensor","vector","columns"],"additionalProperties":false}"#),
    tool!("scale", "Scale rows or columns", false,
        "On the working copy: multiply the listed rows or the listed columns (one of them) of a matrix by `factor`.",
        r#"{"type":"object","properties":{"tensor":{"type":"string"},"rows":{"type":"array","items":{"type":"integer","minimum":0},"maxItems":4096},"cols":{"type":"array","items":{"type":"integer","minimum":0},"maxItems":4096},"factor":{"type":"number"}},"required":["tensor","factor"],"additionalProperties":false}"#),
    tool!("add_rank1", "Add a rank-one change", false,
        "On the working copy: add alpha * outer(u, v) to a matrix. u has one number per row, v one per column.",
        r#"{"type":"object","properties":{"tensor":{"type":"string"},"u":{"type":"array","items":{"type":"number"},"maxItems":20000},"v":{"type":"array","items":{"type":"number"},"maxItems":20000},"alpha":{"type":"number"}},"required":["tensor","u","v","alpha"],"additionalProperties":false}"#),
    tool!("set_slice", "Set some weights", false,
        "On the working copy: write `values` (flat, row-major) into the slice `index` of a tensor. Up to 20000 numbers.",
        r#"{"type":"object","properties":{"tensor":{"type":"string"},"index":{"type":"string"},"values":{"type":"array","items":{"type":"number"},"maxItems":20000}},"required":["tensor","index","values"],"additionalProperties":false}"#),
    tool!("practice_set", "Practice prompts", true,
        "Up to 512 practice prompts the owner supplied (or the band's own winning takes from the Woodshed, name woodshed:<band>). Never the held-out set. An unknown name lists the ones there are.",
        r#"{"type":"object","properties":{"name":{"type":"string"},"n":{"type":"integer","minimum":1,"maximum":512,"default":32}},"required":["name"],"additionalProperties":false}"#),
    tool!("budget", "Forward passes left", true,
        "Forward passes used and left. forward costs 1, activation_diff 2.",
        r#"{"type":"object","properties":{},"additionalProperties":false}"#),
];

const INSTRUCTIONS: &str = "The Retune bench: you can read a local model's weights and edit them, on the owner's computer. Reads and edits are free; forward and activation_diff cost passes from a budget (see budget). Edits go to a working copy and are recorded in an edit list; the original weights are never changed and the owner can undo any edit. There is no shell and no way to run code here, and no tool takes a path. Practice prompts come from practice_set; the owner keeps a held-out set you will never see, and the edit is kept only if it wins on that. Editing removes obstacles (a loop, a bad habit); it does not add skill.";

/// Whatever answers for the weights: the Python helper in the program, a stand-in in tests.
pub trait Helper {
    fn call(&mut self, tool: &str, args: &J) -> Result<J, String>;
}

/// The helper as a child process: one JSON line in, one out.
pub struct PyHelper {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
}

impl PyHelper {
    pub fn spawn(argv: &[String], log: &std::path::Path) -> Result<PyHelper, String> {
        let (prog, args) = argv.split_first().ok_or("nothing to run")?;
        let err = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {prog} ({e})"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let out = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        Ok(PyHelper { child, stdin, out })
    }
}

impl Helper for PyHelper {
    fn call(&mut self, tool: &str, args: &J) -> Result<J, String> {
        let line = o(vec![("tool", J::s(tool)), ("args", args.clone())]).compact();
        writeln!(self.stdin, "{line}")
            .and_then(|()| self.stdin.flush())
            .map_err(|_| "the helper has stopped (its log is in the session folder)".to_string())?;
        let mut reply = String::new();
        let n = self
            .out
            .read_line(&mut reply)
            .map_err(|e| format!("the helper stopped answering: {e}"))?;
        if n == 0 {
            return Err("the helper has stopped (its log is in the session folder)".into());
        }
        let j = parse(reply.trim()).map_err(|_| "the helper's answer was not JSON".to_string())?;
        if j.bool("ok") == Some(true) {
            Ok(j.get("result").cloned().unwrap_or(J::Null))
        } else {
            Err(j.str("error").unwrap_or("the helper said no").to_string())
        }
    }
}

impl Drop for PyHelper {
    fn drop(&mut self) {
        // closing its stdin ends its loop; this only makes sure it is gone
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---- checking what the model sent

fn num(j: &J) -> Option<f64> {
    match j {
        J::Num(n) => n.parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

fn bad(key: &str, why: &str) -> String {
    format!("{key}: {why}")
}

fn text(a: &J, key: &str, max: usize) -> Result<String, String> {
    match a.str(key) {
        Some(s) if !s.is_empty() && s.len() <= max => Ok(s.to_string()),
        _ => Err(bad(
            key,
            &format!("a short text is needed (up to {max} characters)"),
        )),
    }
}

fn tensor_name(a: &J) -> Result<J, String> {
    let t = text(a, "tensor", 200)?;
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(bad("tensor", "a tensor name has only letters, digits, dots, dashes and underscores; model_info lists them"));
    }
    Ok(J::s(&t))
}

fn numbers(a: &J, key: &str, max: usize) -> Result<J, String> {
    let v = a.arr(key);
    if v.is_empty() || v.len() > max || !v.iter().all(|x| num(x).is_some()) {
        return Err(bad(
            key,
            &format!("a list of 1 to {max} finite numbers is needed"),
        ));
    }
    Ok(J::Arr(v.to_vec()))
}

fn ints(a: &J, key: &str) -> Result<J, String> {
    let v = a.arr(key);
    let ok = |x: &J| num(x).is_some_and(|n| n >= 0.0 && n.fract() == 0.0 && n < 1e9);
    if v.is_empty() || v.len() > MAX_COLUMNS || !v.iter().all(ok) {
        return Err(bad(
            key,
            &format!("a list of 1 to {MAX_COLUMNS} whole numbers, 0 or more, is needed"),
        ));
    }
    Ok(J::Arr(v.to_vec()))
}

fn prompts(a: &J, key: &str) -> Result<J, String> {
    let v = a.arr(key);
    let ok = |x: &J| matches!(x, J::Str(s) if !s.trim().is_empty() && s.len() <= MAX_PROMPT_CHARS);
    if v.is_empty() || v.len() > MAX_PROMPTS || !v.iter().all(ok) {
        return Err(bad(
            key,
            &format!(
                "1 to {MAX_PROMPTS} prompts of up to {MAX_PROMPT_CHARS} characters are needed"
            ),
        ));
    }
    Ok(J::Arr(v.to_vec()))
}

fn int_in(a: &J, key: &str, lo: i64, hi: i64, default: Option<i64>) -> Result<J, String> {
    match (a.get(key), default) {
        (None, Some(d)) => Ok(J::n(d as f64)),
        (Some(x), _) => match num(x) {
            Some(n) if n.fract() == 0.0 && (lo as f64..=hi as f64).contains(&n) => Ok(J::n(n)),
            _ => Err(bad(
                key,
                &format!("a whole number from {lo} to {hi} is needed"),
            )),
        },
        (None, None) => Err(bad(key, "is missing")),
    }
}

fn number(a: &J, key: &str) -> Result<J, String> {
    a.get(key)
        .filter(|x| num(x).is_some())
        .cloned()
        .ok_or_else(|| bad(key, "a finite number is needed"))
}

/// The arguments the helper gets: only the keys the tool knows, each checked. Anything else the
/// model sent is dropped, not passed on.
pub fn clean(tool: &str, a: &J) -> Result<J, String> {
    let mut v: Vec<(&str, J)> = vec![];
    match tool {
        "model_info" => {}
        "tensor_stats" => {
            v.push(("tensor", tensor_name(a)?));
            if a.get("vector").is_some() {
                v.push(("vector", numbers(a, "vector", READ_CAP)?));
            }
        }
        "read_slice" => {
            v.push(("tensor", tensor_name(a)?));
            let i = text(a, "index", 200)?;
            if !i
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ':' | ',' | ' '))
            {
                return Err(bad(
                    "index",
                    "only digits, colons and commas, like \"0:4, 100:164\"",
                ));
            }
            v.push(("index", J::s(&i)));
        }
        "top_directions" => {
            v.push(("tensor", tensor_name(a)?));
            v.push(("k", int_in(a, "k", 1, 16, Some(3))?));
            let side = a.str("side").unwrap_or("output");
            if !matches!(side, "output" | "input") {
                return Err(bad("side", "output or input"));
            }
            v.push(("side", J::s(side)));
        }
        "forward" => {
            v.push(("prompts", prompts(a, "prompts")?));
            v.push((
                "max_tokens",
                int_in(a, "max_tokens", 1, MAX_TOKENS, Some(256))?,
            ));
        }
        "activation_diff" => {
            v.push(("prompts_a", prompts(a, "prompts_a")?));
            v.push(("prompts_b", prompts(a, "prompts_b")?));
            v.push(("layer", int_in(a, "layer", 0, 1000, None)?));
        }
        "ablate_direction" => {
            v.push(("tensor", tensor_name(a)?));
            v.push(("vector", numbers(a, "vector", READ_CAP)?));
            v.push(("columns", ints(a, "columns")?));
        }
        "scale" => {
            v.push(("tensor", tensor_name(a)?));
            match (a.get("rows"), a.get("cols")) {
                (Some(_), None) => v.push(("rows", ints(a, "rows")?)),
                (None, Some(_)) => v.push(("cols", ints(a, "cols")?)),
                _ => return Err("give rows or cols, one of them".into()),
            }
            v.push(("factor", number(a, "factor")?));
        }
        "add_rank1" => {
            v.push(("tensor", tensor_name(a)?));
            v.push(("u", numbers(a, "u", READ_CAP)?));
            v.push(("v", numbers(a, "v", READ_CAP)?));
            v.push(("alpha", number(a, "alpha")?));
        }
        "set_slice" => {
            v.push(("tensor", tensor_name(a)?));
            let i = text(a, "index", 200)?;
            if !i
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ':' | ',' | ' '))
            {
                return Err(bad(
                    "index",
                    "only digits, colons and commas, like \"0:4, 100:164\"",
                ));
            }
            v.push(("index", J::s(&i)));
            v.push(("values", numbers(a, "values", READ_CAP)?));
        }
        other => return Err(format!("there is no tool called {other}")),
    }
    Ok(o(v))
}

/// Forward passes a call costs.
pub fn cost(tool: &str) -> u32 {
    match tool {
        "forward" => 1,
        "activation_diff" => 2,
        _ => 0,
    }
}

// ---- practice prompts

fn practice_dir(ctx: &Ctx) -> std::path::PathBuf {
    session::root(ctx).join("practice")
}

fn prompt_of(line: &str, jsonl: bool) -> Option<String> {
    if !jsonl {
        return Some(line.trim().to_string()).filter(|s| !s.is_empty());
    }
    let j = parse(line.trim()).ok()?;
    ["prompt", "question", "input"]
        .iter()
        .find_map(|k| j.str(k))
        .map(str::to_string)
}

/// Every prompt in the owner's practice set `name`, or the band's winning takes.
fn practice_all(ctx: &Ctx, name: &str) -> Result<Vec<String>, String> {
    if let Some(band) = name.strip_prefix("woodshed:") {
        if !plain(band) {
            return Err(format!("\"{band}\" is not a band name"));
        }
        let (all, _) = takes::read_takes(ctx);
        return Ok(all
            .iter()
            .filter(|t| t.winner && takes::key(t) == band)
            .filter_map(|t| takes::prompt_at(ctx, &t.prompt_ref))
            .filter_map(|m| {
                m.into_iter()
                    .rev()
                    .find(|(r, _)| r == "user")
                    .map(|(_, c)| c)
            })
            .collect());
    }
    let have = || {
        let mut v: Vec<String> = std::fs::read_dir(practice_dir(ctx))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .collect();
        v.sort();
        v.dedup();
        v.push("woodshed:<band>".into());
        v
    };
    if !plain(name) {
        return Err(format!("Practice sets: {}.", have().join(", ")));
    }
    for (ext, jsonl) in [("txt", false), ("jsonl", true)] {
        if let Ok(t) = std::fs::read_to_string(practice_dir(ctx).join(format!("{name}.{ext}"))) {
            return Ok(t.lines().filter_map(|l| prompt_of(l, jsonl)).collect());
        }
    }
    Err(format!(
        "No practice set called {name}. Practice sets: {}.",
        have().join(", ")
    ))
}

// ---- the server

pub struct Bench<'a> {
    pub ctx: &'a Ctx,
    pub session: Session,
    pub helper: RefCell<Box<dyn Helper + 'a>>,
}

impl Bench<'_> {
    fn practice(&self, a: &J) -> Result<String, String> {
        let name = text(a, "name", 80)?;
        let n = int_in(a, "n", 1, MAX_PROMPTS as i64, Some(32))?;
        let n = num(&n).unwrap_or(32.0) as usize;
        // the held-out set is read here only to leave its prompts out
        let held: Vec<String> = self
            .session
            .held_out
            .as_deref()
            .and_then(|p| session::read_held_out(p).ok())
            .map(|v| v.into_iter().map(|(p, _)| norm(&p)).collect())
            .unwrap_or_default();
        let all: Vec<String> = practice_all(self.ctx, &name)?
            .into_iter()
            .filter(|p| !held.contains(&norm(p)))
            .collect();
        if all.is_empty() {
            return Err(format!("{name} has no prompts to practise on."));
        }
        let seen: Vec<String> = self.session.seen().iter().map(|p| norm(p)).collect();
        // new ones first, so asking again gives fresh prompts until the set runs out
        let (fresh, old): (Vec<&String>, Vec<&String>) =
            all.iter().partition(|p| !seen.contains(&norm(p)));
        let pick: Vec<String> = fresh.into_iter().chain(old).take(n).cloned().collect();
        self.session.note_seen(&pick);
        Ok(o(vec![
            ("name", J::s(&name)),
            ("available", J::n(all.len() as f64)),
            ("prompts", J::strings(&pick)),
        ])
        .compact())
    }

    pub fn call(&self, name: &str, args: &J) -> Result<String, String> {
        match name {
            "budget" => Ok(o(vec![
                ("used", J::n(f64::from(self.session.used()))),
                ("left", J::n(f64::from(self.session.left()))),
                ("limit", J::n(f64::from(self.session.passes))),
            ])
            .compact()),
            "practice_set" => self.practice(args),
            _ => {
                if !TOOLS.iter().any(|t| t.name == name) {
                    return Err(format!("there is no tool called {name}"));
                }
                let clean = clean(name, args)?;
                let left = match cost(name) {
                    0 => None,
                    c => Some(self.session.charge(c)?),
                };
                let mut r = self.helper.borrow_mut().call(name, &clean)?;
                if let Some(left) = left {
                    r.set("passes_left", J::n(f64::from(left)));
                }
                Ok(r.compact())
            }
        }
    }

    pub fn handle(&self, msg: &J) -> Option<J> {
        let id = msg.get("id").cloned()?;
        let reply = |r: J| {
            Some(o(vec![
                ("jsonrpc", J::s("2.0")),
                ("id", id.clone()),
                ("result", r),
            ]))
        };
        let params = msg.get("params").cloned().unwrap_or(J::Obj(vec![]));
        match msg.str("method").unwrap_or("") {
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
                ("instructions", J::s(INSTRUCTIONS)),
            ])),
            "ping" => reply(J::Obj(vec![])),
            "tools/list" => reply(o(vec![(
                "tools",
                J::Arr(TOOLS.iter().map(tool_json).collect()),
            )])),
            "tools/call" => {
                let args = params.get("arguments").cloned().unwrap_or(J::Obj(vec![]));
                let (t, bad) = match self.call(params.str("name").unwrap_or(""), &args) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                reply(o(vec![
                    (
                        "content",
                        J::Arr(vec![o(vec![("type", J::s("text")), ("text", J::s(&t))])]),
                    ),
                    ("isError", J::Bool(bad)),
                ]))
            }
            "resources/list" => reply(o(vec![("resources", J::Arr(vec![]))])),
            "prompts/list" => reply(o(vec![("prompts", J::Arr(vec![]))])),
            _ => Some(o(vec![
                ("jsonrpc", J::s("2.0")),
                ("id", id),
                (
                    "error",
                    o(vec![
                        ("code", J::n(-32601.0)),
                        ("message", J::s("method not found")),
                    ]),
                ),
            ])),
        }
    }

    pub fn run(&self, input: impl BufRead, output: impl Write) -> std::io::Result<()> {
        serve(input, output, &|m| self.handle(m))
    }
}
