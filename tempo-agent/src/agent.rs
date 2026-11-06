//! The agent loop. One user turn: pick a lane, then up to `max_steps` model calls,
//! running the tool calls each one returns, until the model answers without tools.
//!
//! - fast lane: small model, no tools; `NEEDS_TOOLS` (or an empty answer) hands the
//!   turn to the heavy lane
//! - heavy lane: built-ins plus the MCP tools that best match the prompt
//! - engine errors: one retry after an HTTP error, then the next model in the lane
//! - repeat guard: an identical call (same name and arguments) in the same turn is
//!   answered from the first result instead of running again
//! - step cap: at `max_steps` the model is asked once, without tools, for what is done
//!   and what is next
use crate::config::{AgentConfig, Lane};
use crate::error::{Error, Result};
use crate::lanes::pick_lane;
use crate::mcp::{route_name, to_ollama_tool, McpTool, ToolHost};
use crate::ollama::{ChatRequest, ChatResult, Engine, Message, Options, Tool, ToolCall};
use crate::text::{cap, today, truncate_chars, words};
use crate::tools::{builtin_tools, run_builtin, BuiltinCtx};
use crate::workflow::Workflow;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::Duration;

pub const FAST_NOTE: &str = "\nFAST LANE: no tools this turn. If the request needs a tool, a file, a command, another machine or a long careful analysis, reply with exactly NEEDS_TOOLS and nothing else.";
pub const STEP_CAP_PROMPT: &str =
    "Step limit reached. Without calling tools, report what you finished, what is left, and the next step. Be brief.";
pub const STEP_CAP_NOTE: &str = "(step limit reached; say 'continue' to keep going, the workflow file keeps the plan.)";

/// What the loop reports while it works (the GUI and CLI render these).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    User(String),
    Status(String),
    /// A streamed piece of assistant text.
    Delta(String),
    /// Text the model wrote alongside tool calls.
    AssistantPartial(String),
    /// The turn's final answer.
    Assistant { text: String, model: String },
    ToolCall { name: String, args: String },
    ToolResult { name: String, text: String },
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub text: String,
    pub model: String,
    pub lane: Lane,
    /// Model calls made, including a fast-lane hand-off.
    pub steps: usize,
    pub hit_step_cap: bool,
}

pub fn system_prompt(cfg: &AgentConfig, today: &str, workflow: &str) -> String {
    let owner = &cfg.owner;
    let note = match cfg.owner_note.trim() {
        "" => String::new(),
        n => format!("{n}\n"),
    };
    format!(
        "You are Perennos Tempo, {owner}'s local agent: the harness around a local model. You run on this machine with no cloud and may have no internet and no other machines.
{note}Today is {today}.
Rules:
- Use tools to do things (list folders, read and write files, call MCP tools). Check results before claiming success. Report failures plainly.
- If you do not know something about {owner}'s work, say so; never invent facts.
- Never delete anything. Never touch secrets.
- Keep a task list for multi-step jobs with workflow_update. The saved workflow is below; continue from it.
- Be brief and direct: under 120 words unless {owner} asks for detail; do not dump whole lists. No disclaimers.
WORKFLOW FILE (persistent):
{workflow}
"
    )
}

fn message_chars(m: &Message) -> usize {
    serde_json::to_string(m).map_or(0, |s| s.chars().count())
}

/// Drop the oldest messages until the history (system prompt excluded) fits in
/// `max_chars`. Order: earlier turns first, then the oldest steps of the current turn.
/// The system prompt, the current turn's user message (`turn_start`) and the last two
/// messages are always kept; tool results whose call was dropped go with it.
/// Returns how many messages before `turn_start` were dropped.
pub fn trim_history(messages: &mut Vec<Message>, max_chars: usize, turn_start: usize) -> usize {
    let n = messages.len();
    if n < 2 {
        return 0;
    }
    let turn_start = turn_start.clamp(1, n);
    let sizes: Vec<usize> = messages.iter().map(message_chars).collect();
    let mut total: usize = sizes[1..].iter().sum();
    let mut keep = vec![true; n];
    let tail = n.saturating_sub(2).max(1);
    for i in (1..turn_start).chain(turn_start + 1..tail) {
        if total <= max_chars {
            break;
        }
        if !keep[i] {
            continue;
        }
        keep[i] = false;
        total -= sizes[i];
        let mut j = i + 1;
        while j < tail && messages[j].role == "tool" && keep[j] {
            keep[j] = false;
            total -= sizes[j];
            j += 1;
        }
    }
    let dropped_before = (1..turn_start).filter(|&i| !keep[i]).count();
    let mut idx = 0;
    messages.retain(|_| {
        idx += 1;
        keep[idx - 1]
    });
    dropped_before
}

/// Indices of the MCP tools worth offering for `prompt`, best first: name words count
/// three times, description words once, a server hint word adds one; 3 points needed.
pub fn select_tools(prompt: &str, catalog: &[(String, McpTool)], n_builtin: usize, cfg: &AgentConfig) -> Vec<usize> {
    let room = cfg.tool_cap.saturating_sub(n_builtin).min(cfg.mcp_tools_per_turn);
    let qt = words(prompt);
    let lower = prompt.to_lowercase();
    let mut ranked: Vec<(usize, usize)> = catalog
        .iter()
        .enumerate()
        .filter_map(|(i, (srv, t))| {
            let spaced = t.name.replace('_', " ");
            let name_hits = qt.intersection(&words(&spaced)).count();
            let all_hits = qt.intersection(&words(&format!("{spaced} {}", truncate_chars(&t.description, 200)))).count();
            let hint = cfg.mcp_hints.get(srv).is_some_and(|ws| ws.iter().any(|w| lower.contains(&w.to_lowercase())));
            let score = 3 * name_hits + all_hits + usize::from(hint);
            (score >= 3).then_some((score, i))
        })
        .collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.0)); // stable: ties keep catalog order
    ranked.into_iter().take(room).map(|(_, i)| i).collect()
}

/// Asks the person: (what, detail) -> approved?
pub type ConfirmFn = Box<dyn FnMut(&str, &str) -> bool>;

/// The tools offered for one turn and how MCP names route back to (server, tool).
#[derive(Default)]
struct Offer {
    tools: Vec<Tool>,
    route: HashMap<String, (String, String)>,
    n_mcp: usize,
}

pub struct Agent {
    pub cfg: AgentConfig,
    /// The conversation so far; `messages[0]` is the system prompt once a turn ran.
    pub messages: Vec<Message>,
    engine: Box<dyn Engine>,
    host: Box<dyn ToolHost>,
    workflow: Workflow,
    on_event: Box<dyn FnMut(&Event)>,
    confirm: ConfirmFn,
    reported_mcp_errors: bool,
}

impl Agent {
    /// Confirmations are denied until `with_confirm` says otherwise.
    pub fn new(cfg: AgentConfig, engine: Box<dyn Engine>, host: Box<dyn ToolHost>, workflow: Workflow) -> Agent {
        Agent {
            cfg,
            messages: Vec::new(),
            engine,
            host,
            workflow,
            on_event: Box::new(|_| {}),
            confirm: Box::new(|_, _| false),
            reported_mcp_errors: false,
        }
    }

    pub fn with_events(mut self, f: impl FnMut(&Event) + 'static) -> Agent {
        self.on_event = Box::new(f);
        self
    }

    pub fn with_confirm(mut self, f: impl FnMut(&str, &str) -> bool + 'static) -> Agent {
        self.confirm = Box::new(f);
        self
    }

    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }

    fn emit(&mut self, e: Event) {
        (self.on_event)(&e);
    }

    fn status(&mut self, s: String) {
        self.emit(Event::Status(s));
    }

    /// Run one user turn. Errors (every engine down) are also sent as `Event::Error`.
    pub fn run_turn(&mut self, text: &str) -> Result<TurnOutcome> {
        let r = self.turn(text);
        if let Err(e) = &r {
            self.emit(Event::Error(e.to_string()));
        }
        r
    }

    fn turn(&mut self, text: &str) -> Result<TurnOutcome> {
        self.emit(Event::User(text.to_string()));
        let sys = Message::system(system_prompt(&self.cfg, &today(), &self.workflow.tail(2500)));
        match self.messages.first_mut() {
            Some(m) if m.role == "system" => *m = sys,
            _ => self.messages.insert(0, sys),
        }
        self.messages.push(Message::user(text));
        let mut turn_start = self.messages.len() - 1;

        let mut lane = match self.cfg.lane {
            Some(l) => l,
            None => {
                let engine = &self.engine;
                pick_lane(text, &self.cfg, || engine.resident_models())
            }
        };
        let mut offer = Offer::default();
        if lane == Lane::Heavy {
            offer = self.offer_heavy(text);
        } else {
            self.messages[0].content.push_str(FAST_NOTE);
        }
        let mut models = self.lane_models(lane)?;
        let mut model_i = 0;
        self.lane_status(lane, &models[0], &offer);
        let mut seen: HashMap<String, (String, u32)> = HashMap::new();

        for step in 1..=self.cfg.max_steps {
            turn_start -= trim_history(&mut self.messages, self.cfg.hist_chars, turn_start);
            let resp = loop {
                let req = self.request(&models[model_i], self.messages.clone(), &offer.tools);
                match self.send_with_retry(&req) {
                    Ok(r) => break r,
                    Err(e) => {
                        let why = truncate_chars(&e.to_string(), 160).to_string();
                        self.status(format!("engine {} failed ({}: {why}); trying fallback", models[model_i], e.kind()));
                        model_i += 1;
                        if model_i >= models.len() {
                            return Err(Error::Engine(format!("all engines failed in the {} lane; last: {e}", lane.as_str())));
                        }
                    }
                }
            };
            self.status(resp.timings.summary());
            let said = resp.message.content.trim();
            if lane == Lane::Fast && (said.starts_with("NEEDS_TOOLS") || said.is_empty()) {
                lane = Lane::Heavy;
                self.status("fast lane handed this turn to the heavy lane".to_string());
                if let Some(i) = self.messages[0].content.find(FAST_NOTE) {
                    self.messages[0].content.truncate(i);
                }
                offer = self.offer_heavy(text);
                models = self.lane_models(lane)?;
                model_i = 0;
                self.lane_status(lane, &models[0], &offer);
                continue;
            }
            let mut m = resp.message;
            m.thinking = None;
            m.role = "assistant".to_string();
            for c in &mut m.tool_calls {
                c.function.arguments = Value::Object(c.function.args());
            }
            let (calls, content, model) = (m.tool_calls.clone(), m.content.clone(), models[model_i].clone());
            self.messages.push(m);
            if calls.is_empty() {
                self.emit(Event::Assistant { text: content.clone(), model: model.clone() });
                let _ = self.workflow.log(&format!("turn done: {}", truncate_chars(text, 80)));
                return Ok(TurnOutcome { text: content, model, lane, steps: step, hit_step_cap: false });
            }
            if !content.is_empty() {
                self.emit(Event::AssistantPartial(content));
            }
            for c in &calls {
                self.run_call(c, &offer.route, &mut seen);
            }
        }

        self.messages.push(Message::user(STEP_CAP_PROMPT));
        let mut history = self.messages.clone();
        trim_history(&mut history, self.cfg.hist_chars, turn_start);
        let model = models[model_i].clone();
        let req = self.request(&model, history, &[]);
        let fin = match self.send(&req) {
            Ok(r) => r.message.content,
            Err(e) => {
                self.status(format!("step-cap summary failed: {e}"));
                String::new()
            }
        };
        if !fin.is_empty() {
            self.messages.push(Message::assistant(fin.clone()));
        }
        let text_out = if fin.is_empty() { STEP_CAP_NOTE.to_string() } else { format!("{fin}\n\n{STEP_CAP_NOTE}") };
        self.emit(Event::Assistant { text: text_out.clone(), model: model.clone() });
        let _ = self.workflow.log(&format!("step cap: {}", truncate_chars(&fin, 200).replace('\n', " ")));
        Ok(TurnOutcome { text: text_out, model, lane, steps: self.cfg.max_steps, hit_step_cap: true })
    }

    fn lane_models(&self, lane: Lane) -> Result<Vec<String>> {
        let models = self.cfg.models_for(lane).to_vec();
        if models.is_empty() {
            return Err(Error::Config(format!("no models configured for the {} lane", lane.as_str())));
        }
        Ok(models)
    }

    fn lane_status(&mut self, lane: Lane, model: &str, offer: &Offer) {
        let (n, k) = (offer.tools.len(), offer.n_mcp);
        self.status(format!("lane: {} ({model}); tools offered: {n} ({k} MCP)", lane.as_str()));
    }

    fn offer_heavy(&mut self, text: &str) -> Offer {
        let catalog: Vec<(String, McpTool)> =
            self.host.catalog().into_iter().filter(|(s, t)| !self.cfg.is_blocked(s, &t.name)).collect();
        if !self.reported_mcp_errors {
            self.reported_mcp_errors = true;
            for (name, why) in self.host.errors() {
                self.status(format!("MCP server {name} unavailable: {why}"));
            }
        }
        let mut tools = builtin_tools(&self.cfg.write_roots, self.cfg.result_cap);
        let picked = select_tools(text, &catalog, tools.len(), &self.cfg);
        tools.extend(picked.iter().map(|&i| to_ollama_tool(&catalog[i].0, &catalog[i].1)));
        let route = catalog.iter().map(|(s, t)| (route_name(s, &t.name), (s.clone(), t.name.clone()))).collect();
        Offer { tools, route, n_mcp: picked.len() }
    }

    fn request(&self, model: &str, messages: Vec<Message>, tools: &[Tool]) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages,
            tools: tools.to_vec(),
            stream: true,
            keep_alive: Some(self.cfg.keep_alive.clone()),
            options: Some(Options { num_ctx: Some(self.cfg.num_ctx_for(model)), temperature: Some(self.cfg.temperature) }),
            think: self.cfg.no_think(model).then_some(false),
        }
    }

    fn send(&mut self, req: &ChatRequest) -> Result<ChatResult> {
        let on_event = &mut self.on_event;
        self.engine.chat(req, &mut |piece| on_event(&Event::Delta(piece.to_string())))
    }

    /// An HTTP error is often a model swap or load in progress: wait, try once more.
    fn send_with_retry(&mut self, req: &ChatRequest) -> Result<ChatResult> {
        match self.send(req) {
            Err(Error::Http { status, body }) => {
                self.status(format!("engine {} HTTP {status}: {}; retrying once", req.model, truncate_chars(&body, 160)));
                std::thread::sleep(self.cfg.retry_delay());
                self.send(req)
            }
            other => other,
        }
    }

    fn run_call(&mut self, call: &ToolCall, route: &HashMap<String, (String, String)>, seen: &mut HashMap<String, (String, u32)>) {
        let name = call.function.name.clone();
        let args = call.function.args();
        let args_json = Value::Object(args.clone()).to_string(); // keys sorted: a stable key
        self.emit(Event::ToolCall { name: name.clone(), args: cap(&args_json, 600) });
        let key = format!("{name}{args_json}");
        if let Some((prev, n)) = seen.get_mut(&key) {
            *n += 1;
            let r = format!(
                "You already ran this exact call (x{n}). Its result was:\n{}\nUse that result: take the next step or give the final answer. Do not repeat it.",
                truncate_chars(prev, 1500)
            );
            self.emit(Event::ToolResult { name: name.clone(), text: "(repeat skipped)".to_string() });
            self.messages.push(Message::tool(&name, r));
            return;
        }
        let r = match self.exec(&name, &args, route) {
            Ok(s) => s,
            Err(e) => format!("ERROR {}: {e}", e.kind()),
        };
        let r = cap(&r, self.cfg.result_cap);
        seen.insert(key, (r.clone(), 1));
        self.emit(Event::ToolResult { name: name.clone(), text: truncate_chars(&r, 1500).to_string() });
        let _ = self.workflow.log(&format!("{name} -> {}", truncate_chars(&r, 100).replace('\n', " ")));
        self.messages.push(Message::tool(&name, r));
    }

    fn exec(&mut self, name: &str, args: &Map<String, Value>, route: &HashMap<String, (String, String)>) -> Result<String> {
        let Agent { cfg, workflow, on_event, confirm, host, .. } = self;
        let mut ask = |what: &str, detail: &str| {
            let ok = confirm(what, detail);
            on_event(&Event::Status(format!("{what}: {}", if ok { "approved" } else { "denied" })));
            ok
        };
        let mut ctx = BuiltinCtx { workflow, write_roots: &cfg.write_roots, result_cap: cfg.result_cap, confirm: &mut ask };
        if let Some(r) = run_builtin(name, args, &mut ctx) {
            return r;
        }
        if let Some((srv, tool)) = route.get(name) {
            let args = Value::Object(args.clone());
            if cfg.needs_confirm(srv, tool) && !ask(&format!("MCP {srv}:{tool}"), truncate_chars(&args.to_string(), 300)) {
                return Ok("denied by user".to_string());
            }
            return host.call(srv, tool, &args, Duration::from_secs(cfg.mcp_call_timeout_secs));
        }
        Ok(format!("unknown tool {name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{McpHost, McpServer, NoTools};
    use crate::ollama::Ollama;
    use crate::testutil::{spawn_mock_mcp, text_reply, tool_reply, MockOllama, Reply, TempDir};
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    fn test_cfg(dir: &TempDir) -> AgentConfig {
        AgentConfig {
            heavy_models: vec!["big".into()],
            fast_models: vec!["small".into()],
            retry_delay_secs: 0.0,
            write_roots: vec![dir.path().join("out")],
            ..Default::default()
        }
    }

    fn mcp_host() -> Box<dyn ToolHost> {
        let mut host = McpHost::new(BTreeMap::new(), vec![], Duration::from_secs(10));
        let (rx, tx) = spawn_mock_mcp();
        host.attach(McpServer::connect("mock-srv", rx, tx, None, Duration::from_secs(10)));
        Box::new(host)
    }

    type Log = Rc<RefCell<Vec<Event>>>;

    fn agent(mock: &MockOllama, cfg: AgentConfig, host: Box<dyn ToolHost>, dir: &TempDir) -> (Agent, Log) {
        let log: Log = Rc::default();
        let sink = log.clone();
        let wf = Workflow::open(dir.path().join("wf.md"), "test").unwrap();
        let a = Agent::new(cfg, Box::new(Ollama::new(&mock.url)), host, wf).with_events(move |e| sink.borrow_mut().push(e.clone()));
        (a, log)
    }

    fn statuses(log: &Log) -> Vec<String> {
        log.borrow().iter().filter_map(|e| if let Event::Status(s) = e { Some(s.clone()) } else { None }).collect()
    }

    fn tool_messages(req: &Value) -> Vec<(String, String)> {
        req["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| (m["tool_name"].as_str().unwrap().to_string(), m["content"].as_str().unwrap().to_string()))
            .collect()
    }

    #[test]
    fn fast_lane_answers_without_tools() {
        let dir = TempDir::new("agent");
        let mock = MockOllama::start(vec![text_reply("Hi! How can I help?")]);
        let (mut a, log) = agent(&mock, test_cfg(&dir), Box::new(NoTools), &dir);
        let out = a.run_turn("hello there").unwrap();
        assert_eq!((out.lane, out.text.as_str(), out.model.as_str(), out.steps), (Lane::Fast, "Hi! How can I help?", "small", 1));
        let req = &mock.chat_requests()[0];
        assert!(req.get("tools").is_none());
        assert_eq!((req["stream"].clone(), req["keep_alive"].clone()), (json!(true), json!("60m")));
        assert_eq!(req["options"], json!({"num_ctx": 8192, "temperature": 0.4}));
        assert!(req.get("think").is_none(), "only configured model families get think:false");
        assert!(req["messages"][0]["content"].as_str().unwrap().contains("FAST LANE: no tools"));
        assert_eq!(req["messages"][1], json!({"role": "user", "content": "hello there"}));
        let log = log.borrow();
        assert_eq!(log[0], Event::User("hello there".into()));
        let deltas: String = log.iter().filter_map(|e| if let Event::Delta(d) = e { Some(d.as_str()) } else { None }).collect();
        assert_eq!(deltas, "Hi! How can I help?");
        assert!(matches!(log.last(), Some(Event::Assistant { model, .. }) if model == "small"));
        assert_eq!(a.messages.len(), 3, "system, user, assistant");
        assert!(a.workflow().text().unwrap().contains("turn done: hello there"));
    }

    #[test]
    fn fast_lane_hands_up_to_heavy() {
        let dir = TempDir::new("agent");
        let mock = MockOllama::start(vec![text_reply("NEEDS_TOOLS"), text_reply("Done in the heavy lane.")]);
        let (mut a, log) = agent(&mock, test_cfg(&dir), mcp_host(), &dir);
        let out = a.run_turn("how about some help").unwrap();
        assert_eq!((out.lane, out.text.as_str(), out.model.as_str(), out.steps), (Lane::Heavy, "Done in the heavy lane.", "big", 2));
        assert_eq!(mock.chat_models(), ["small", "big"]);
        let heavy = &mock.chat_requests()[1];
        assert_eq!(heavy["tools"].as_array().unwrap().len(), 4, "built-ins; no MCP tool matched");
        assert!(!heavy["messages"][0]["content"].as_str().unwrap().contains("FAST LANE"), "the hand-off removes the fast-lane note");
        assert!(heavy["messages"].as_array().unwrap().iter().all(|m| m["content"] != "NEEDS_TOOLS"));
        assert!(statuses(&log).iter().any(|s| s == "fast lane handed this turn to the heavy lane"));
        // The real Qwen aliases get think:false, gpt-oss does not.
        let cfg = AgentConfig::default();
        assert_eq!(a.request("local-qwen3", vec![], &[]).think, Some(false));
        assert_eq!(Agent::new(cfg, Box::new(Ollama::new(&mock.url)), Box::new(NoTools), a.workflow.clone()).request("local-gpt-oss-32k", vec![], &[]).think, None);
    }

    #[test]
    fn heavy_lane_runs_builtin_and_mcp_tools() {
        let dir = TempDir::new("agent");
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "the plan is ready").unwrap();
        let notes = notes.to_string_lossy().into_owned();
        let mock = MockOllama::start(vec![
            tool_reply(&[("read_file", json!({"path": notes})), ("mock_srv__echo", json!({"text": "hello"}))]),
            tool_reply(&[("read_file", json!({"path": "/no/such/file"})), ("nope", json!({}))]),
            text_reply("All done."),
        ]);
        let (mut a, log) = agent(&mock, test_cfg(&dir), mcp_host(), &dir);
        let out = a.run_turn("read my notes and echo hello").unwrap();
        assert_eq!((out.lane, out.text.as_str(), out.steps), (Lane::Heavy, "All done.", 3));
        let reqs = mock.chat_requests();
        let offered: Vec<_> = reqs[0]["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(offered, ["list_dir", "read_file", "write_file", "workflow_update", "mock_srv__echo"]);
        let echo = &reqs[0]["tools"][4]["function"]["parameters"];
        assert!(echo.get("$schema").is_none() && echo["properties"]["text"].get("default").is_none(), "schema shrunk");
        assert_eq!(
            tool_messages(&reqs[1]),
            [("read_file".to_string(), "the plan is ready".to_string()), ("mock_srv__echo".to_string(), "hello".to_string())]
        );
        let second = tool_messages(&reqs[2]);
        assert!(second[2].1.starts_with("ERROR Io: "), "{second:?}");
        assert_eq!(second[3].1, "unknown tool nope");
        // The assistant's tool calls are kept in history so the model sees what it asked.
        let asked = &reqs[1]["messages"].as_array().unwrap()[2];
        assert_eq!(asked["tool_calls"][1]["function"]["name"], "mock_srv__echo");
        assert!(log.borrow().iter().any(|e| matches!(e, Event::ToolCall { name, .. } if name == "mock_srv__echo")));
        let wf = a.workflow().text().unwrap();
        assert!(wf.contains("read_file -> the plan is ready") && wf.contains("mock_srv__echo -> hello") && wf.contains("turn done:"));
    }

    #[test]
    fn repeat_guard_answers_from_the_first_result() {
        let dir = TempDir::new("agent");
        std::fs::write(dir.path().join("a.txt"), "first").unwrap();
        let p = dir.path().join("a.txt").to_string_lossy().into_owned();
        let mock = MockOllama::start(vec![
            tool_reply(&[("read_file", json!({"path": p, "offset": 0}))]),
            tool_reply(&[("read_file", json!({"offset": 0, "path": p}))]),
            tool_reply(&[("read_file", json!({"offset": 0, "path": p}))]),
            text_reply("ok"),
        ]);
        let (mut a, log) = agent(&mock, test_cfg(&dir), Box::new(NoTools), &dir);
        std::fs::write(dir.path().join("a.txt"), "first").unwrap();
        let out = a.run_turn("read a.txt").unwrap();
        assert_eq!(out.text, "ok");
        let reqs = mock.chat_requests();
        let tools = tool_messages(&reqs[3]);
        assert_eq!(tools[0].1, "first");
        assert!(tools[1].1.starts_with("You already ran this exact call (x2). Its result was:\nfirst\n"), "{}", tools[1].1);
        assert!(tools[2].1.starts_with("You already ran this exact call (x3)."));
        let skipped = log.borrow().iter().filter(|e| matches!(e, Event::ToolResult { text, .. } if text == "(repeat skipped)")).count();
        assert_eq!(skipped, 2);
        // A new turn starts with a clean guard: the same call runs again.
        let mock2 = MockOllama::start(vec![tool_reply(&[("read_file", json!({"path": p, "offset": 0}))]), text_reply("again")]);
        a.engine = Box::new(Ollama::new(&mock2.url));
        a.run_turn("read a.txt again").unwrap();
        assert_eq!(tool_messages(&mock2.chat_requests()[1]).last().unwrap().1, "first");
    }

    #[test]
    fn step_cap_asks_once_for_a_summary() {
        let dir = TempDir::new("agent");
        let cfg = AgentConfig { max_steps: 2, ..test_cfg(&dir) };
        let d = dir.path().to_string_lossy().into_owned();
        let mock = MockOllama::start(vec![
            tool_reply(&[("list_dir", json!({"path": d}))]),
            tool_reply(&[("workflow_update", json!({"tasks": [{"text": "look", "done": true}, "write"]}))]),
            text_reply("Listed the folder; next: write the file."),
        ]);
        let (mut a, _) = agent(&mock, cfg, Box::new(NoTools), &dir);
        let out = a.run_turn("list the folder then write a file").unwrap();
        assert!(out.hit_step_cap);
        assert_eq!(out.text, format!("Listed the folder; next: write the file.\n\n{STEP_CAP_NOTE}"));
        let reqs = mock.chat_requests();
        assert_eq!(reqs.len(), 3);
        assert!(reqs[2].get("tools").is_none(), "the summary call offers no tools");
        let last = reqs[2]["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last, json!({"role": "user", "content": STEP_CAP_PROMPT}));
        assert_eq!(a.messages.last().unwrap().content, "Listed the folder; next: write the file.");
        let wf = a.workflow().text().unwrap();
        assert!(wf.contains("- [x] look\n- [ ] write") && wf.contains("step cap: Listed the folder"));
    }

    #[test]
    fn retries_then_falls_back_then_gives_up() {
        let dir = TempDir::new("agent");
        let cfg = AgentConfig { heavy_models: vec!["a".into(), "b".into()], lane: Some(Lane::Heavy), ..test_cfg(&dir) };
        let busy = || Reply::Status(503, r#"{"error":"model is loading"}"#.into());
        let mock = MockOllama::start(vec![busy(), busy(), text_reply("from b")]);
        let (mut a, log) = agent(&mock, cfg.clone(), Box::new(NoTools), &dir);
        assert_eq!(a.run_turn("hi").unwrap().model, "b");
        assert_eq!(mock.chat_models(), ["a", "a", "b"]);
        let st = statuses(&log);
        assert!(st.iter().any(|s| s == "engine a HTTP 503: model is loading; retrying once"), "{st:?}");
        assert!(st.iter().any(|s| s.starts_with("engine a failed (Http: ")));

        let down = MockOllama::start(vec![busy(), busy(), busy(), busy()]);
        let (mut a, log) = agent(&down, cfg, Box::new(NoTools), &dir);
        let err = a.run_turn("hi").unwrap_err();
        assert!(matches!(&err, Error::Engine(e) if e.starts_with("all engines failed in the heavy lane")), "{err}");
        assert!(matches!(log.borrow().last(), Some(Event::Error(_))));
    }

    #[test]
    fn confirmations_and_blocked_tools() {
        let dir = TempDir::new("agent");
        let out_dir = dir.path().join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let existing = out_dir.join("keep.txt");
        std::fs::write(&existing, "original").unwrap();
        let cfg = AgentConfig {
            confirm_tools: vec!["mock-srv:echo".into()],
            blocked_tools: BTreeMap::from([("mock-srv".to_string(), vec!["fail".to_string()])]),
            ..test_cfg(&dir)
        };
        let outside = dir.path().join("elsewhere.txt").to_string_lossy().into_owned();
        let mock = MockOllama::start(vec![
            tool_reply(&[
                ("write_file", json!({"path": existing.to_string_lossy(), "content": "new"})),
                ("write_file", json!({"path": outside, "content": "x"})),
                ("mock_srv__echo", json!({"text": "hi"})),
                ("mock_srv__fail", json!({})),
            ]),
            text_reply("ok"),
        ]);
        let (a, log) = agent(&mock, cfg, mcp_host(), &dir);
        let asked: Rc<RefCell<Vec<String>>> = Rc::default();
        let seen = asked.clone();
        let mut a = a.with_confirm(move |what, _| {
            seen.borrow_mut().push(what.to_string());
            false
        });
        a.run_turn("write the files").unwrap();
        let results: Vec<String> = tool_messages(&mock.chat_requests()[1]).into_iter().map(|(_, c)| c).collect();
        assert_eq!(results[0], "denied by user");
        assert!(results[1].starts_with("refused: outside allowed write folders"));
        assert_eq!(results[2], "denied by user");
        assert_eq!(results[3], "unknown tool mock_srv__fail", "blocked tools are not routable");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert_eq!(*asked.borrow(), ["overwrite file", "MCP mock-srv:echo"]);
        assert!(statuses(&log).iter().any(|s| s == "MCP mock-srv:echo: denied"));
    }

    #[test]
    fn selects_matching_mcp_tools_within_the_cap() {
        let tool = |n: &str, d: &str| McpTool { name: n.into(), description: d.into(), input_schema: Value::Null };
        let catalog = vec![
            ("comfy-media".to_string(), tool("run_workflow", "Run a ComfyUI workflow graph.")),
            ("comfy-media".to_string(), tool("server_info", "Report server status.")),
            ("web".to_string(), tool("browser_navigate", "Open a page in the browser.")),
            ("web".to_string(), tool("browser_click", "Click an element on the page.")),
        ];
        let cfg = AgentConfig::default();
        let pick = |p: &str, n: usize| select_tools(p, &catalog, n, &cfg);
        assert_eq!(pick("run the comfy workflow", 4), [0]);
        assert_eq!(pick("open the browser and click the page", 4), [3, 2]);
        assert!(pick("hello", 4).is_empty());
        assert!(pick("open the browser and click the page", 40).is_empty(), "no room left under tool_cap");
        let one = AgentConfig { mcp_tools_per_turn: 1, ..AgentConfig::default() };
        assert_eq!(select_tools("open the browser and click the page", &catalog, 4, &one), [3]);
    }

    #[test]
    fn trims_old_history_but_keeps_the_current_turn() {
        let big = "x".repeat(400);
        let mut msgs = vec![Message::system("s")];
        for i in 0..4 {
            msgs.push(Message::user(format!("q{i} {big}")));
            msgs.push(Message { tool_calls: vec![ToolCall::new("t", json!({}))], ..Message::assistant("") });
            msgs.push(Message::tool("t", big.clone()));
            msgs.push(Message::assistant(format!("a{i}")));
        }
        let turn_start = msgs.len();
        msgs.push(Message::user("now"));
        msgs.push(Message { tool_calls: vec![ToolCall::new("t", json!({}))], ..Message::assistant("") });
        msgs.push(Message::tool("t", big.clone()));
        let dropped = trim_history(&mut msgs, 1500, turn_start);
        assert_eq!(msgs[0].content, "s");
        assert_eq!(dropped, 16 - (msgs.iter().position(|m| m.content == "now").unwrap() - 1));
        assert_eq!(msgs[msgs.len() - 3].content, "now", "the current turn survives");
        assert!(msgs[1].role != "tool", "no orphan tool result at the front");
        let total: usize = msgs[1..].iter().map(message_chars).sum();
        assert!(total <= 1500 || msgs.len() == 4, "fits, or only the protected messages are left");
        // Under budget: nothing changes.
        let mut small = vec![Message::system("s"), Message::user("q"), Message::assistant("a")];
        assert_eq!(trim_history(&mut small, 10_000, 1), 0);
        assert_eq!(small.len(), 3);
        // Within a long current turn, the oldest steps go first; the question stays.
        let mut long = vec![Message::system("s"), Message::user("q")];
        for _ in 0..5 {
            long.push(Message { tool_calls: vec![ToolCall::new("t", json!({}))], ..Message::assistant("") });
            long.push(Message::tool("t", big.clone()));
        }
        trim_history(&mut long, 1200, 1);
        assert_eq!(long[1].content, "q");
        assert!(long.len() < 12 && long[2].role == "assistant");
    }

    #[test]
    fn system_prompt_names_the_owner_and_carries_the_workflow() {
        let cfg = AgentConfig { owner: "Sam".into(), owner_note: "Sam runs a studio.".into(), ..AgentConfig::default() };
        let p = system_prompt(&cfg, "06 Nov 2026", "## Task list\n- [ ] ship");
        assert!(p.starts_with("You are Perennos Tempo, Sam's local agent"));
        assert!(p.contains("\nSam runs a studio.\nToday is 06 Nov 2026.\n"));
        assert!(p.ends_with("WORKFLOW FILE (persistent):\n## Task list\n- [ ] ship\n"));
        let anon = system_prompt(&AgentConfig::default(), "d", "");
        assert!(anon.starts_with("You are Perennos Tempo, the owner's local agent") && anon.contains("machines.\nToday is d."));
    }
}
