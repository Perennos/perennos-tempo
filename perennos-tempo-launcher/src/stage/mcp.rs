//! `tempo-launch stage mcp`: every workflow that is added, ready and turned on becomes an MCP
//! tool, `stage_<id>`, with its Pedals as typed parameters (a choice is an enum, a number has
//! its range). The agent registers it like the Crate and asks the owner before every call
//! unless the owner trusted it, so the engines call it the way they call any tool. A run goes
//! through `run::play`, so the machine choice, the safety rules and the no-overwrite rule are
//! the same as at the command line; the one difference is that inputs must sit in a folder the
//! owner allowed.

use super::catalog::{tool_name, Catalog, Kind, Pedal, Via, Workflow};
use super::run::{play, Outcome, RunOpts, World};
use super::state::State;
use crate::bank::mcp::{o, serve};
use crate::setup::json::J;
use crate::Ctx;
use std::io::{BufRead, Write};

pub const SERVER_NAME: &str = "tempo-stage";

pub struct Server<'a> {
    pub ctx: &'a Ctx,
    pub world: &'a dyn World,
    pub cat: &'a Catalog,
}

fn prop(kind: &str, desc: &str) -> J {
    o(vec![("type", J::s(kind)), ("description", J::s(desc))])
}

fn pedal_schema(cat: &Catalog, p: &Pedal) -> J {
    let word = cat.word(&p.name);
    let about = word.map_or(String::new(), |w| w.about.clone());
    let dflt = p
        .default
        .as_ref()
        .map_or(String::new(), |d| format!(" Default {d}."));
    match &p.via {
        Via::Choices(c) => {
            let mut j = prop("string", &format!("{about}{dflt}"));
            j.set("enum", J::Arr(c.iter().map(|(k, _)| J::s(k)).collect()));
            j
        }
        Via::Prompt(_) => {
            let mut j = prop("string", &format!("{about}{dflt}"));
            if let Some(w) = word {
                j.set(
                    "enum",
                    J::Arr(w.choices.iter().map(|c| J::s(&c.id)).collect()),
                );
            }
            j
        }
        Via::Number { min, max, .. } => {
            let integer = word.is_some_and(|w| w.kind == Kind::Integer);
            let unit = word
                .filter(|w| !w.unit.is_empty())
                .map_or(String::new(), |w| format!(" In {}.", w.unit));
            let mut j = prop(
                if integer { "integer" } else { "number" },
                &format!("{about}{unit}{dflt}"),
            );
            j.set("minimum", J::n(*min));
            j.set("maximum", J::n(*max));
            j
        }
    }
}

/// The MCP tool for one workflow.
pub fn tool_json(cat: &Catalog, wf: &Workflow) -> J {
    let mut props: Vec<(&str, J)> = vec![];
    let mut required: Vec<J> = vec![];
    // keep the strings alive for the borrow
    let names: Vec<String> = wf
        .inputs
        .iter()
        .map(|i| i.name.clone())
        .chain(wf.pedals.iter().map(|p| p.name.clone()))
        .collect();
    let mut k = 0;
    for i in &wf.inputs {
        let d = if i.kind == "text" {
            format!(
                "{} {}",
                i.about,
                if i.default.is_empty() {
                    String::new()
                } else {
                    format!("Default: {}", i.default)
                }
            )
        } else {
            format!(
                "Path of the {} file, in the workbench or a folder the owner allowed. {}",
                i.kind, i.about
            )
        };
        props.push((&names[k], prop("string", d.trim())));
        if i.required && i.default.is_empty() {
            required.push(J::s(&i.name));
        }
        k += 1;
    }
    for p in &wf.pedals {
        props.push((&names[k], pedal_schema(cat, p)));
        k += 1;
    }
    props.push(("takes", {
        let mut j = prop("integer", "Play this many takes (different seeds), spread over the Fleet's graphics cards. The best passes a quick local check and wins.");
        j.set("minimum", J::n(1.0));
        j.set("maximum", J::n(12.0));
        j
    }));
    props.push(("again", prop("boolean", "Start from the last run of this workflow (same inputs, Pedals and seed) with what you give here on top. ComfyUI reuses what did not change.")));
    props.push(("on", prop("string", "Play it on this Fleet machine, by name. Left out, Stage picks the best one that has the memory.")));
    let schema = o(vec![
        ("type", J::s("object")),
        ("properties", o(props)),
        ("required", J::Arr(required)),
        ("additionalProperties", J::Bool(false)),
    ]);
    o(vec![
        ("name", J::s(&tool_name(&wf.id))),
        ("title", J::s(&wf.title)),
        (
            "description",
            J::s(&format!(
                "{} Runs on ComfyUI on one of the owner's own machines (needs {:.0} GB of graphics memory), never replaces a file, and writes the result to the workbench with a .stage.json beside it that says how it was made. It can take minutes.",
                wf.what, wf.min_vram_gb
            )),
        ),
        ("inputSchema", schema),
        (
            "annotations",
            o(vec![
                ("title", J::s(&wf.title)),
                ("readOnlyHint", J::Bool(false)),
                ("destructiveHint", J::Bool(false)),
                ("idempotentHint", J::Bool(false)),
                ("openWorldHint", J::Bool(false)),
            ]),
        ),
    ])
}

fn extra_tools() -> Vec<J> {
    vec![
        o(vec![
            ("name", J::s("stage_again")),
            ("title", J::s("Play the last Stage run again, changed")),
            (
                "description",
                J::s("Play the last Stage run again with Pedals changed, on the same seed, so ComfyUI reuses what did not change. For \"make it golden hour\": pedals {\"light\": \"golden_hour\"}. Pedal values are checked against the workflow's own list."),
            ),
            (
                "inputSchema",
                o(vec![
                    ("type", J::s("object")),
                    (
                        "properties",
                        o(vec![
                            ("pedals", o(vec![("type", J::s("object")), ("description", J::s("Pedal name to its new value, like {\"light\": \"golden_hour\", \"camera\": \"push_in\"}.")), ("additionalProperties", o(vec![("type", J::s("string"))]))])),
                            ("workflow", prop("string", "Which workflow. Left out: the one that ran last.")),
                        ]),
                    ),
                    ("additionalProperties", J::Bool(false)),
                ]),
            ),
            ("annotations", o(vec![("readOnlyHint", J::Bool(false)), ("destructiveHint", J::Bool(false)), ("openWorldHint", J::Bool(false))])),
        ]),
        o(vec![
            ("name", J::s("stage_list")),
            ("title", J::s("List what Stage can play")),
            ("description", J::s("The workflows that are turned on, with their Pedals.")),
            ("inputSchema", o(vec![("type", J::s("object")), ("properties", o(vec![])), ("additionalProperties", J::Bool(false))])),
            ("annotations", o(vec![("readOnlyHint", J::Bool(true)), ("destructiveHint", J::Bool(false)), ("openWorldHint", J::Bool(false))])),
        ]),
    ]
}

fn text_of(v: &J) -> Option<String> {
    match v {
        J::Str(s) => Some(s.clone()),
        J::Num(n) => Some(n.clone()),
        J::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// What the tool says back: the status line first, then each file.
pub fn report(out: &Outcome) -> String {
    let mut t = out.status.clone();
    for k in &out.takes {
        let ok = match k.check.ok {
            Some(true) => "passed the check",
            Some(false) => "failed the check",
            None => "not checked",
        };
        t.push_str(&format!(
            "\nTake {} (seed {}, {ok}{}): {}",
            k.take,
            k.seed,
            if k.check.note.is_empty() {
                String::new()
            } else {
                format!(": {}", k.check.note)
            },
            k.files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let (Some(w), true) = (out.winner, out.takes.len() > 1) {
        t.push_str(&format!("\nBest: take {}.", out.takes[w].take));
    }
    for n in &out.notes {
        t.push_str(&format!("\nNote: {n}"));
    }
    t.push_str("\nThe .stage.json beside each file says how it was made. Nothing was replaced.");
    t
}

impl Server<'_> {
    fn enabled(&self) -> Vec<&Workflow> {
        let st = State::load(self.ctx);
        self.cat
            .workflows
            .iter()
            .filter(|w| st.enabled(&w.id))
            .collect()
    }

    /// The tools to offer: one per workflow that is on, and the two helpers when any is.
    pub fn tools(&self) -> Vec<J> {
        let on = self.enabled();
        if on.is_empty() {
            return vec![];
        }
        let mut v: Vec<J> = on.iter().map(|w| tool_json(self.cat, w)).collect();
        v.extend(extra_tools());
        v
    }

    fn opts_from(&self, wf: &Workflow, args: &J) -> Result<RunOpts, String> {
        let J::Obj(a) = args else {
            return Err("the arguments must be an object".into());
        };
        let mut opts = RunOpts {
            guarded: true,
            ..RunOpts::default()
        };
        for (k, v) in a {
            let val = || text_of(v).ok_or_else(|| format!("\"{k}\" must be a plain value"));
            match k.as_str() {
                "takes" => {
                    opts.takes = val()?
                        .parse()
                        .map_err(|_| "takes must be a whole number".to_string())?
                }
                "again" => opts.again = val()? == "true",
                "on" => opts.on = Some(val()?),
                _ if wf.input(k).is_some() => opts.inputs.push((k.clone(), val()?)),
                _ if wf.pedal(k).is_some() => opts.pedals.push((k.clone(), val()?)),
                _ => {
                    return Err(format!(
                        "{} has no \"{k}\". Inputs: {}. Pedals: {}.",
                        wf.id,
                        wf.inputs
                            .iter()
                            .map(|i| i.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        wf.pedals
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
        }
        Ok(opts)
    }

    pub fn call(&self, name: &str, args: &J) -> Result<String, String> {
        let on = self.enabled();
        match name {
            "stage_list" => {
                if on.is_empty() {
                    return Ok("Nothing is turned on. The owner runs `tempo-launch stage list`, then `stage add ID` and `stage enable ID`.".into());
                }
                Ok(on
                    .iter()
                    .map(|w| {
                        format!(
                            "{}: {} Pedals: {}.",
                            tool_name(&w.id),
                            w.title,
                            w.pedals
                                .iter()
                                .map(|p| p.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "stage_again" => {
                let st = State::load(self.ctx);
                let id = args
                    .str("workflow")
                    .map(String::from)
                    .or_else(|| st.last_workflow())
                    .ok_or("There is no earlier Stage run to play again.")?;
                let wf = on
                    .iter()
                    .find(|w| w.id == id)
                    .ok_or_else(|| format!("{id} is not turned on."))?;
                let mut opts = RunOpts {
                    again: true,
                    guarded: true,
                    ..RunOpts::default()
                };
                if let Some(J::Obj(p)) = args.get("pedals") {
                    for (k, v) in p {
                        if wf.pedal(k).is_none() {
                            return Err(format!("{} has no \"{k}\" Pedal.", wf.id));
                        }
                        opts.pedals.push((
                            k.clone(),
                            text_of(v).ok_or("a Pedal value must be plain text")?,
                        ));
                    }
                }
                play(self.ctx, self.world, self.cat, &id, &opts).map(|o| report(&o))
            }
            _ => {
                let wf = on
                    .iter()
                    .find(|w| tool_name(&w.id) == name)
                    .ok_or_else(|| format!("{name} is not available: it is not added, or the owner has not turned it on (`tempo-launch stage enable ID`)."))?;
                let opts = self.opts_from(wf, args)?;
                play(self.ctx, self.world, self.cat, &wf.id, &opts).map(|o| report(&o))
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
                ("protocolVersion", J::s(params.str("protocolVersion").unwrap_or("2024-11-05"))),
                ("capabilities", o(vec![("tools", o(vec![("listChanged", J::Bool(false))]))])),
                ("serverInfo", o(vec![("name", J::s(SERVER_NAME)), ("version", J::s(crate::VERSION))])),
                (
                    "instructions",
                    J::s("Stage (video and image workflows) on the owner's own machines, played through ComfyUI. Each tool is one workflow; its Pedals (modifiers) are typed parameters. Inputs are files in the workbench or a folder the owner allowed. A run can take minutes, never replaces a file, and nothing leaves the computer. If a tool is missing, the owner has not turned it on."),
                ),
            ])),
            "ping" => reply(J::Obj(vec![])),
            "tools/list" => reply(o(vec![("tools", J::Arr(self.tools()))])),
            "tools/call" => {
                let args = params.get("arguments").cloned().unwrap_or(J::Obj(vec![]));
                let (t, bad) = match self.call(params.str("name").unwrap_or(""), &args) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                reply(o(vec![
                    ("content", J::Arr(vec![o(vec![("type", J::s("text")), ("text", J::s(&t))])])),
                    ("isError", J::Bool(bad)),
                ]))
            }
            "resources/list" => reply(o(vec![("resources", J::Arr(vec![]))])),
            "prompts/list" => reply(o(vec![("prompts", J::Arr(vec![]))])),
            _ => Some(o(vec![
                ("jsonrpc", J::s("2.0")),
                ("id", id),
                ("error", o(vec![("code", J::n(-32601.0)), ("message", J::s("method not found"))])),
            ])),
        }
    }

    pub fn run(&self, input: impl BufRead, output: impl Write) -> std::io::Result<()> {
        serve(input, output, &|m| self.handle(m))
    }
}

/// `tempo-launch stage mcp`: serve on stdin and stdout until the client closes them.
pub fn run_stdio(ctx: &Ctx) -> i32 {
    let world = super::world::RealWorld::new(ctx);
    let cat = super::catalog::embedded();
    let s = Server {
        ctx,
        world: &world,
        cat: &cat,
    };
    match s.run(std::io::stdin().lock(), std::io::stdout().lock()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
