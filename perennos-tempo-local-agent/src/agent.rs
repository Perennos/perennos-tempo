//! The agent loop: lanes, tool loop, repeat guard, engine retry/fallback, step-cap summary.
use crate::config::Config;
use crate::conv::{Conv, Sink};
use crate::mcp::{self, Offered};
use crate::ollama::{self, ChatResult};
use crate::tools::{builtin_schemas, Env};
use crate::util::{self, cap, head_chars, tail_chars, RESULT_CAP};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

fn toolish(cfg: &Config) -> Regex {
    let mut hosts: Vec<String> = cfg.hosts.keys().map(|h| regex::escape(h)).collect();
    hosts.sort();
    let extra = if hosts.is_empty() {
        String::new()
    } else {
        format!("|{}", hosts.join("|"))
    };
    Regex::new(&format!(
        r"(?i)\b(list|ls|dir|folder|directory|read|open|run|execute|command|write|save|create|make|generate|render|image|picture|delegate|trash|delete|remove|fetch|download|install|restart|start|stop|comfy|workflow|task list|todo|fix|edit|change|replace|find|search|grep|glob|patch|status of|check (the )?(status|disk|gpu){extra})\b|[A-Za-z]:\\|\.(py|rs|md|json|ps1|txt|log)\b"
    ))
    .expect("regex")
}

fn hardish() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)\b(analy[sz]e|compare|plan|design|architecture|debug|step by step|prove|calculate|code|script|refactor|review|draft)\b").expect("regex"))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lane {
    Fast,
    Heavy,
}

/// fast = small model, no tools; heavy = tools. (The /api/ps "sticky" check is not ported.)
pub fn pick_lane(cfg: &Config, text: &str) -> Lane {
    if text.chars().count() > 600
        || toolish(cfg).is_match(text)
        || hardish().is_match(text)
        || cfg.fast_models.is_empty()
    {
        Lane::Heavy
    } else {
        Lane::Fast
    }
}

/// Drop oldest messages until the history fits the char budget; never start the history with a tool message.
/// Prefix-stable tool choice: names offered before stay (while they exist), new matches fill free
/// slots up to `cap`, and the result is sorted so the tool block renders identically every turn.
pub fn sticky_tools(previous: &[String], ranked: &[String], cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in previous.iter().chain(ranked.iter()) {
        if out.len() < cap && !out.contains(n) {
            out.push(n.clone());
        }
    }
    out.sort();
    out
}

pub fn trim(messages: Vec<Value>, budget: usize) -> Vec<Value> {
    trim_to(messages, budget, budget)
}

/// Trim only when the history exceeds `budget`, and then down to `target` (<= budget), so that in
/// prefix-stable mode a trim (which invalidates the engine's cached prefix) happens rarely.
pub fn trim_to(messages: Vec<Value>, budget: usize, target: usize) -> Vec<Value> {
    if messages.is_empty() {
        return messages;
    }
    let mut it = messages.into_iter();
    let sysm = it.next().expect("non-empty");
    let mut rest: Vec<Value> = it.collect();
    let size = |ms: &[Value]| {
        ms.iter()
            .map(|m| m.to_string().chars().count())
            .sum::<usize>()
    };
    if size(&rest) <= budget {
        let mut out = vec![sysm];
        out.extend(rest);
        return out;
    }
    while size(&rest) > target && rest.len() > 2 {
        rest.remove(0);
        while rest.first().is_some_and(|m| m["role"] == "tool") {
            rest.remove(0);
        }
    }
    let mut out = vec![sysm];
    out.extend(rest);
    out
}

const SYSTEM: &str = "You are Perennos Tempo, a local superagent: the harness around a local model. You run on the user's own machine with no cloud and may have no internet and no other machines.\n{owner}Today is {today}.\nRules:\n- Answer from the MEMORY excerpts below when they cover the question, and cite each source as [filename]. If they do not cover it, say so; never invent facts.\n- Use tools to do things (list folders, run commands, make images, ask other machines with delegate). Check results before claiming success. Report failures plainly.\n- Never delete anything; the only removal is the trash tool (it moves into a trash folder and needs approval). Never touch secrets.\n- To change part of a file use edit_file (exact text), not write_file; use grep/glob to find things.\n- Keep a task list for multi-step jobs with workflow_update. The saved workflow is below; continue from it.\n- Be brief and direct: under 120 words unless asked for detail; do not dump whole lists. No disclaimers.\nWORKFLOW FILE (persistent):\n{wf}\n";

const SYSTEM_STABLE: &str = "You are Perennos Tempo, a local superagent: the harness around a local model. You run on the user's own machine with no cloud and may have no internet and no other machines.\n{owner}Rules:\n- Each user message may end with a [context] block: the latest WORKFLOW FILE and MEMORY excerpts for that message. Answer from those excerpts when they cover the question, and cite each source as [filename]. If they do not cover it, say so; never invent facts.\n- Use tools to do things (list folders, run commands, make images, ask other machines with delegate). Check results before claiming success. Report failures plainly.\n- Never delete anything; the only removal is the trash tool (it moves into a trash folder and needs approval). Never touch secrets.\n- To change part of a file use edit_file (exact text), not write_file; use grep/glob to find things.\n- Keep a task list for multi-step jobs with workflow_update, and continue from the saved workflow.\n- Be brief and direct: under 120 words unless asked for detail; do not dump whole lists. No disclaimers.\n";

const FAST_NOTE: &str = "\nFAST LANE: no tools this turn. If the request needs a tool, a file, a command, another machine or a long careful analysis, reply with exactly NEEDS_TOOLS and nothing else.";

pub struct Agent {
    pub env: Env,
    core: (f64, String),
}

/// Holds back streamed text of a no-tools fast-lane reply while it could still turn out to be NEEDS_TOOLS.
struct Hold {
    active: bool,
    acc: String,
    released: bool,
}

impl Hold {
    fn push(&mut self, piece: &str, sink: &mut dyn Sink) {
        if !self.active || self.released {
            sink.text(piece);
            return;
        }
        self.acc.push_str(piece);
        if !"NEEDS_TOOLS".starts_with(self.acc.trim_start()) {
            self.released = true;
            sink.text(&self.acc.clone());
        }
    }
    /// After the reply is complete and is the final answer: show anything still held.
    fn flush(&mut self, sink: &mut dyn Sink) {
        if self.active && !self.released && !self.acc.is_empty() {
            self.released = true;
            sink.text(&self.acc.clone());
        }
    }
}

impl Agent {
    pub fn new(env: Env) -> Agent {
        Agent {
            env,
            core: (-1.0, String::new()),
        }
    }

    fn cfg(&self) -> &Config {
        &self.env.cfg
    }

    /// The pinned core file, re-read when its mtime changes.
    fn core_text(&mut self) -> String {
        if let Some(p) = self.env.cfg.core_file.clone() {
            let m = util::mtime_of(Path::new(&p));
            if m != self.core.0 {
                if let Ok(b) = std::fs::read(&p) {
                    self.core = (m, String::from_utf8_lossy(&b).trim().to_string());
                }
            }
        }
        self.core.1.clone()
    }

    fn system_message(&mut self, conv: &Conv, mem: &str, fast: bool) -> Value {
        let cfg = self.cfg();
        let owner = if cfg.owner_context.is_empty() {
            String::new()
        } else {
            format!("{}\n", cfg.owner_context)
        };
        if cfg.prefix_stable {
            // Fixed for the whole conversation (the core file changes rarely), so the engine's
            // cached prefix covers the system message, the tools and all earlier turns.
            let mut c = SYSTEM_STABLE.replace("{owner}", &owner);
            let core = self.core_text();
            if !core.is_empty() {
                c.push_str(&format!("\nCORE (pinned, always true):\n{core}\n"));
            }
            return json!({"role": "system", "content": c});
        }
        let mut c = SYSTEM
            .replace("{owner}", &owner)
            .replace("{today}", &util::today_long())
            .replace("{wf}", &tail_chars(&conv.wf_text(), 2500));
        let core = self.core_text();
        if !core.is_empty() {
            c.push_str(&format!("\nCORE (pinned, always true):\n{core}\n"));
        }
        c.push_str(&format!("\nMEMORY EXCERPTS:\n{mem}"));
        if fast {
            c.push_str(FAST_NOTE);
        }
        json!({"role": "system", "content": c})
    }

    /// The user message for this turn. In prefix-stable mode the per-turn context (date, workflow
    /// tail, memory excerpts, fast-lane note) rides here instead of in the system message.
    fn user_message(&self, conv: &Conv, text: &str, mem: &str, fast: bool) -> Value {
        if !self.cfg().prefix_stable {
            return json!({"role": "user", "content": text});
        }
        let mut c = format!(
            "{text}\n\n[context]\nToday is {}.\nWORKFLOW FILE (latest):\n{}\nMEMORY EXCERPTS:\n{mem}",
            util::today_long(),
            tail_chars(&conv.wf_text(), 1500)
        );
        if fast {
            c.push_str(FAST_NOTE);
        }
        json!({"role": "user", "content": c})
    }

    fn tools_for(
        &self,
        text: &str,
        conv: &mut Conv,
    ) -> (Vec<Value>, HashMap<String, (String, String)>, usize) {
        let builtins = builtin_schemas();
        let cfg = self.cfg();
        let mut host = self.env.mcp.borrow_mut();
        let mut sel: Vec<Offered> = host.select(text, builtins.len(), cfg.tool_cap);
        if cfg.prefix_stable {
            // Sticky and sorted: tools offered earlier in this conversation stay offered (the tool
            // block sits before the history in the prompt, so any change would void the cache);
            // new matches only fill free slots.
            let cap = cfg.tool_cap.saturating_sub(builtins.len()).min(8);
            let ranked = sel.iter().map(Offered::exposed_name).collect::<Vec<_>>();
            conv.mcp_offered = sticky_tools(&conv.mcp_offered, &ranked, cap);
            sel = conv
                .mcp_offered
                .iter()
                .filter_map(|n| host.find(n))
                .collect();
        }
        let mut tools = builtins;
        for o in &sel {
            tools.push(json!({"type": "function", "function": {
                "name": o.exposed_name(),
                "description": head_chars(o.tool.get("description").and_then(|d| d.as_str()).unwrap_or(""), 300).trim(),
                "parameters": mcp::shrink(o.tool.get("inputSchema").unwrap_or(&Value::Null))}}));
        }
        let route = host.route();
        (tools, route, sel.len())
    }

    /// Engine call with retry on the SAME model, then fallback to the next model in the lane.
    #[allow(clippy::too_many_arguments)]
    fn chat_resilient(
        &self,
        models: &[String],
        idx: &mut usize,
        messages: &[Value],
        tools: &[Value],
        conv: &mut Conv,
        sink: &mut dyn Sink,
        hold: &mut Hold,
    ) -> Result<ChatResult, String> {
        let cfg = self.cfg();
        loop {
            let model = models[*idx].clone();
            let mut last = match ollama::chat_once(cfg, &model, messages, tools, &mut |t| {
                hold.push(t, sink)
            }) {
                Ok(r) => return Ok(r),
                Err(e) => e,
            };
            for attempt in 1..=cfg.retries {
                let msg = format!(
                    "engine {model} error: {}; retry {attempt}/{}",
                    head_chars(&last, 160),
                    cfg.retries
                );
                conv.emit("status", &msg);
                sink.status(&msg);
                std::thread::sleep(Duration::from_millis(cfg.retry_delay_ms));
                match ollama::chat_once(cfg, &model, messages, tools, &mut |t| hold.push(t, sink)) {
                    Ok(r) => return Ok(r),
                    Err(e) => last = e,
                }
            }
            let msg = format!(
                "engine {model} failed ({}); trying fallback",
                head_chars(&last, 160)
            );
            conv.emit("status", &msg);
            sink.status(&msg);
            *idx += 1;
            if *idx >= models.len() {
                return Err("all engines failed".into());
            }
        }
    }

    fn run_tool(
        &self,
        conv: &Conv,
        route: &HashMap<String, (String, String)>,
        name: &str,
        args: &Value,
    ) -> String {
        let r = if Env::is_builtin(name) {
            self.env.run_builtin(conv, name, args)
        } else if let Some((srv, tn)) = route.get(name) {
            let needs = self.env.mcp.borrow().needs_confirm(srv, tn);
            if needs
                && !self.env.confirm.confirm(
                    &format!("MCP {srv}:{tn}"),
                    &head_chars(&args.to_string(), 300),
                )
            {
                "denied by user".to_string()
            } else {
                self.env
                    .mcp
                    .borrow_mut()
                    .call(srv, tn, args)
                    .unwrap_or_else(|e| format!("ERROR: {e}"))
            }
        } else {
            format!("unknown tool {name}")
        };
        cap(&r, RESULT_CAP)
    }

    /// One user turn. Returns the final assistant text. Streams text to `sink` as it arrives.
    pub fn turn(
        &mut self,
        conv: &mut Conv,
        text: &str,
        sink: &mut dyn Sink,
    ) -> Result<String, String> {
        let r = self.turn_inner(conv, text, sink);
        if let Err(e) = &r {
            conv.emit("error", e);
        }
        conv.save();
        r
    }

    fn turn_inner(
        &mut self,
        conv: &mut Conv,
        text: &str,
        sink: &mut dyn Sink,
    ) -> Result<String, String> {
        conv.emit("user", text);
        let hits = self
            .env
            .rag
            .borrow_mut()
            .search(self.cfg(), text, self.cfg().rag_k);
        let rag_chars = self.cfg().rag_chars;
        let mem = hits
            .iter()
            .map(|h| {
                format!(
                    "[{}] {}",
                    Path::new(&h.src)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    head_chars(&h.text, rag_chars)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut srcs: Vec<String> = hits
            .iter()
            .map(|h| {
                Path::new(&h.src)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        srcs.sort();
        srcs.dedup();
        let m = format!("memory: {}", srcs.join(", "));
        conv.emit("status", &m);
        sink.status(&m);

        let mut lane = pick_lane(self.cfg(), text);
        let sysmsg = self.system_message(conv, &mem, lane == Lane::Fast);
        if conv.messages.first().is_some_and(|m| m["role"] == "system") {
            if conv.messages[0] != sysmsg {
                conv.messages[0] = sysmsg;
            }
        } else {
            conv.messages.insert(0, sysmsg);
        }
        let user_at = conv.messages.len();
        conv.messages
            .push(self.user_message(conv, text, &mem, lane == Lane::Fast));

        let (mut tools, mut route, mut n_mcp) = (vec![], HashMap::new(), 0);
        if lane == Lane::Heavy {
            (tools, route, n_mcp) = self.tools_for(text, conv);
        }
        let mut models: Vec<String> = if lane == Lane::Heavy || self.cfg().fast_models.is_empty() {
            self.cfg().models.clone()
        } else {
            self.cfg().fast_models.clone()
        };
        let m = format!(
            "lane: {} ({}); tools offered: {} ({} MCP)",
            if lane == Lane::Heavy { "heavy" } else { "fast" },
            models[0],
            tools.len(),
            n_mcp
        );
        conv.emit("status", &m);
        sink.status(&m);
        let mut idx = 0usize;
        let max_steps = self.cfg().max_steps;
        let hist = self.cfg().hist_chars;

        for _step in 0..max_steps {
            conv.messages = if self.cfg().prefix_stable {
                trim_to(std::mem::take(&mut conv.messages), hist, hist * 6 / 10)
            } else {
                trim(std::mem::take(&mut conv.messages), hist)
            };
            let mut hold = Hold {
                active: lane == Lane::Fast && tools.is_empty(),
                acc: String::new(),
                released: false,
            };
            let snapshot = conv.messages.clone();
            let resp =
                self.chat_resilient(&models, &mut idx, &snapshot, &tools, conv, sink, &mut hold)?;
            let line = resp.info.line();
            conv.emit("status", &line);
            sink.status(&line);
            let content = resp.message["content"].as_str().unwrap_or("").to_string();
            if lane == Lane::Fast
                && (content.trim().starts_with("NEEDS_TOOLS") || content.trim().is_empty())
            {
                lane = Lane::Heavy;
                let m = "fast lane handed this turn to the heavy lane";
                conv.emit("status", m);
                sink.status(m);
                (tools, route, _) = self.tools_for(text, conv);
                let sysmsg = self.system_message(conv, &mem, false);
                conv.messages[0] = sysmsg;
                // drop the fast-lane note from this turn's own user message (only this turn changes)
                let fresh = self.user_message(conv, text, &mem, false);
                if let Some(m) = conv
                    .messages
                    .get_mut(user_at)
                    .filter(|m| m["role"] == "user")
                {
                    *m = fresh;
                }
                models = self.cfg().models.clone();
                idx = 0;
                continue;
            }
            hold.flush(sink);
            let mut msg = resp.message;
            if let Some(o) = msg.as_object_mut() {
                o.remove("thinking");
            }
            conv.messages.push(msg.clone());
            let calls = msg
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            if calls.is_empty() {
                conv.emit("assistant", &content);
                conv.wf_log(&format!("turn done: {}", head_chars(text, 80)));
                return Ok(content);
            }
            for tc in calls {
                let f = &tc["function"];
                let name = f["name"].as_str().unwrap_or("").to_string();
                let args = match f.get("arguments") {
                    Some(Value::String(t)) => serde_json::from_str(t).unwrap_or(json!({})),
                    Some(v) if !v.is_null() => v.clone(),
                    _ => json!({}),
                };
                conv.emit(
                    "tool_call",
                    &format!("{name} {}", cap(&args.to_string(), 600)),
                );
                // sorted keys: serde_json's default Map is a BTreeMap, so to_string() is key-sorted
                let key = format!("{name}{args}");
                if let Some(seen) = conv.seen_calls.get_mut(&key) {
                    seen.1 += 1;
                    let r = format!("You already ran this exact call (x{}). Its result was:\n{}\nUse that result: take the next step or give the final answer. Do not repeat it.", seen.1, head_chars(&seen.0, 1500));
                    conv.emit("tool_result", &format!("{name}: (repeat skipped)"));
                    conv.messages
                        .push(json!({"role": "tool", "tool_name": name, "content": r}));
                    continue;
                }
                let r = self.run_tool(conv, &route, &name, &args);
                conv.seen_calls.insert(key, (r.clone(), 1));
                conv.emit("tool_result", &format!("{name}: {}", head_chars(&r, 1500)));
                conv.messages
                    .push(json!({"role": "tool", "tool_name": name, "content": r}));
                conv.wf_log(&format!(
                    "{name} -> {}",
                    head_chars(&r, 100).replace('\n', " ")
                ));
            }
            conv.save();
        }
        // step cap: one final request with NO tools
        conv.messages.push(json!({"role": "user", "content": "Step limit reached. Without calling tools, report what you finished, what is left, and the next step. Be brief."}));
        let msgs = trim(conv.messages.clone(), hist);
        let mut hold = Hold {
            active: false,
            acc: String::new(),
            released: true,
        };
        let fin = self
            .chat_resilient(&models, &mut idx, &msgs, &[], conv, sink, &mut hold)
            .map(|r| r.message["content"].as_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let out = format!("{fin}\n\n(step limit reached; say 'continue' to keep going, the workflow file keeps the plan.)");
        sink.text("\n(step limit reached; say 'continue' to keep going, the workflow file keeps the plan.)");
        conv.emit("assistant", &out);
        conv.wf_log(&format!(
            "step cap: {}",
            head_chars(&fin, 200).replace('\n', " ")
        ));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conv::{DenyAll, VecSink};
    use crate::mcp::McpHost;
    use crate::util::testutil::*;

    fn agent(m: &Mock, tweak: impl FnOnce(&mut Config)) -> (Agent, Conv) {
        let d = tmpdir("agent");
        let mut c = Config::defaults(Some(d.to_string_lossy().into_owned()));
        c.ollama_url = m.url.clone();
        c.models = vec!["heavy-a".into(), "heavy-b".into()];
        c.fast_models = vec!["fast-a".into()];
        c.retry_delay_ms = 0;
        tweak(&mut c);
        let conv = Conv::open(&c, Some("c1")).unwrap();
        let env = Env::new(c.clone(), Box::new(DenyAll), McpHost::empty(&c));
        (Agent::new(env), conv)
    }

    fn chat_models(m: &Mock) -> Vec<String> {
        m.bodies("/api/chat")
            .iter()
            .map(|b| b["model"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn lane_picking() {
        let c = Config::defaults(None);
        assert_eq!(pick_lane(&c, "hello there"), Lane::Fast);
        assert_eq!(pick_lane(&c, "please list the folder"), Lane::Heavy);
        assert_eq!(pick_lane(&c, "open C:\\x\\y"), Lane::Heavy);
        assert_eq!(pick_lane(&c, "analyze this"), Lane::Heavy);
        assert_eq!(pick_lane(&c, &"a ".repeat(400)), Lane::Heavy);
        assert_eq!(pick_lane(&c, "tell me a joke"), Lane::Fast);
        let mut c2 = Config::defaults(None);
        c2.hosts.insert(
            "depot".into(),
            crate::config::HostCfg {
                urls: vec!["u".into()],
                models: vec![],
            },
        );
        assert_eq!(pick_lane(&c2, "ask depot about it"), Lane::Heavy);
    }

    #[test]
    fn trim_drops_leading_tool_messages() {
        let big = "x".repeat(300);
        let msgs = vec![
            json!({"role": "system", "content": "s"}),
            json!({"role": "user", "content": big}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "a", "arguments": {}}}]}),
            json!({"role": "tool", "content": big}),
            json!({"role": "user", "content": "recent"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        let t = trim(msgs, 150);
        assert_eq!(t[0]["role"], "system");
        assert_ne!(t[1]["role"], "tool");
        assert_eq!(t.last().unwrap()["content"], "ok");
        let t = trim(
            vec![
                json!({"role": "system", "content": "s"}),
                json!({"role": "user", "content": "x".repeat(900)}),
            ],
            10,
        );
        assert_eq!(t.len(), 2); // never trims below two
    }

    #[test]
    fn heavy_text_turn_streams_and_logs_timing() {
        let m = Mock::start(chat_script(vec![text_resp("the answer")]));
        let (mut a, mut conv) = agent(&m, |_| {});
        let mut sink = VecSink::default();
        let r = a
            .turn(&mut conv, "please read the file", &mut sink)
            .unwrap();
        assert_eq!(r, "the answer");
        assert_eq!(sink.text, "the answer");
        assert_eq!(chat_models(&m), ["heavy-a"]);
        assert!(m.bodies("/api/chat")[0]["tools"].as_array().unwrap().len() >= 13);
        assert!(a.env.cfg.logs_dir().join("timing.jsonl").exists());
        assert!(conv.path.exists());
        assert!(conv.wf_text().contains("turn done"));
        assert!(sink.status.iter().any(|s| s.starts_with("lane: heavy")));
    }

    #[test]
    fn cuda_in_band_error_retries_same_model_and_succeeds() {
        let m = Mock::start(chat_script(vec![err_resp(), text_resp("recovered")]));
        let (mut a, mut conv) = agent(&m, |_| {});
        let mut sink = VecSink::default();
        let r = a
            .turn(&mut conv, "please read the file", &mut sink)
            .unwrap();
        assert_eq!(r, "recovered");
        assert_eq!(chat_models(&m), ["heavy-a", "heavy-a"]);
        assert_eq!(sink.text, "recovered"); // the error line never shows as text
        assert!(sink
            .status
            .iter()
            .any(|s| s.contains("CUDA") && s.contains("retry 1/3")));
    }

    #[test]
    fn three_retries_fail_then_fallback_to_next_model() {
        let mut script: Vec<(u16, String)> = (0..4).map(|_| err_resp()).collect();
        script.push(text_resp("from b"));
        let m = Mock::start(chat_script(script));
        let (mut a, mut conv) = agent(&m, |_| {});
        let mut sink = VecSink::default();
        let r = a
            .turn(&mut conv, "please read the file", &mut sink)
            .unwrap();
        assert_eq!(r, "from b");
        assert_eq!(
            chat_models(&m),
            ["heavy-a", "heavy-a", "heavy-a", "heavy-a", "heavy-b"]
        );
        assert!(sink.status.iter().any(|s| s.contains("trying fallback")));
    }

    #[test]
    fn http_errors_also_retry() {
        let m = Mock::start(chat_script(vec![(503, "busy".into()), text_resp("ok")]));
        let (mut a, mut conv) = agent(&m, |_| {});
        assert_eq!(
            a.turn(&mut conv, "please read it", &mut VecSink::default())
                .unwrap(),
            "ok"
        );
        assert_eq!(chat_models(&m), ["heavy-a", "heavy-a"]);
    }

    #[test]
    fn all_engines_failing_is_an_error() {
        let m = Mock::start(chat_script(vec![]));
        let (mut a, mut conv) = agent(&m, |_| {});
        let e = a
            .turn(&mut conv, "please read the file", &mut VecSink::default())
            .unwrap_err();
        assert_eq!(e, "all engines failed");
        assert_eq!(chat_models(&m).len(), 8); // 4 attempts x 2 models
        assert!(conv.events.iter().any(|e| e["type"] == "error"));
        let again = Conv::open(&a.env.cfg, Some("c1")).unwrap();
        assert!(again.events.iter().any(|e| e["type"] == "error")); // saved
    }

    #[test]
    fn tool_loop_runs_builtin_and_feeds_result_back() {
        let d = tmpdir("loopfile");
        let f = d.join("note.txt");
        std::fs::write(&f, "plain content 42").unwrap();
        let m = Mock::start(chat_script(vec![
            call_resp("read_file", json!({"path": f.to_str().unwrap()})),
            text_resp("it says 42"),
        ]));
        let (mut a, mut conv) = agent(&m, |_| {});
        let r = a
            .turn(&mut conv, "read the note", &mut VecSink::default())
            .unwrap();
        assert_eq!(r, "it says 42");
        let second = &m.bodies("/api/chat")[1];
        let msgs = second["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "tool");
        assert_eq!(last["tool_name"], "read_file");
        assert_eq!(last["content"], "plain content 42");
        assert!(msgs
            .iter()
            .any(|m| m["role"] == "assistant" && m["tool_calls"].is_array()));
        assert!(conv.wf_text().contains("read_file ->"));
    }

    #[test]
    fn tool_results_are_redacted_before_the_model_sees_them() {
        let d = tmpdir("redact");
        let f = d.join("cfg.txt");
        std::fs::write(&f, "api_key = abcdef123456789").unwrap();
        let m = Mock::start(chat_script(vec![
            call_resp("read_file", json!({"path": f.to_str().unwrap()})),
            text_resp("done"),
        ]));
        let (mut a, mut conv) = agent(&m, |_| {});
        a.turn(&mut conv, "read the file", &mut VecSink::default())
            .unwrap();
        let last = m.bodies("/api/chat")[1]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["content"], "api_key = [REDACTED]");
    }

    #[test]
    fn repeat_call_guard_does_not_rerun() {
        // workflow_update appends its note on every real run, so a re-run would show up twice
        let call = || call_resp("workflow_update", json!({"note": "ran-once-marker"}));
        let m = Mock::start(chat_script(vec![call(), call(), call(), text_resp("stop")]));
        let (mut a, mut conv) = agent(&m, |_| {});
        a.turn(&mut conv, "update the workflow", &mut VecSink::default())
            .unwrap();
        assert_eq!(conv.wf_text().matches("ran-once-marker").count(), 1);
        let chats = m.bodies("/api/chat");
        let tool_contents: Vec<String> = chats[3]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["content"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(tool_contents.len(), 3);
        assert_eq!(tool_contents[0], "workflow updated");
        assert!(tool_contents[1].starts_with(
            "You already ran this exact call (x2). Its result was:\nworkflow updated"
        ));
        assert!(tool_contents[1].ends_with("Do not repeat it."));
        assert!(tool_contents[2].contains("(x3)"));
    }

    #[test]
    fn repeat_guard_key_ignores_argument_order() {
        let m = Mock::start(chat_script(vec![
            call_resp(
                "grep",
                json!({"pattern": "zzz", "path": "/nonexistent-dir-xyz"}),
            ),
            call_resp(
                "grep",
                json!({"path": "/nonexistent-dir-xyz", "pattern": "zzz"}),
            ),
            text_resp("end"),
        ]));
        let (mut a, mut conv) = agent(&m, |_| {});
        a.turn(&mut conv, "grep for it", &mut VecSink::default())
            .unwrap();
        let msgs = m.bodies("/api/chat")[2]["messages"]
            .as_array()
            .unwrap()
            .clone();
        assert!(msgs.iter().any(|m| m["role"] == "tool"
            && m["content"]
                .as_str()
                .unwrap()
                .starts_with("You already ran")));
    }

    #[test]
    fn step_cap_makes_final_no_tools_summary_request() {
        let mut script: Vec<(u16, String)> = (0..3)
            .map(|i| call_resp("list_dir", json!({"path": format!("/no/such/dir{i}")})))
            .collect();
        script.push(text_resp("Finished A. Left B. Next C."));
        let m = Mock::start(chat_script(script));
        let (mut a, mut conv) = agent(&m, |c| c.max_steps = 3);
        let mut sink = VecSink::default();
        let r = a.turn(&mut conv, "list the folders", &mut sink).unwrap();
        assert!(r.starts_with("Finished A. Left B. Next C.") && r.contains("step limit reached"));
        let chats = m.bodies("/api/chat");
        assert_eq!(chats.len(), 4);
        assert!(chats[0].get("tools").is_some() && chats[2].get("tools").is_some());
        assert!(
            chats[3].get("tools").is_none(),
            "final summary request must carry no tools"
        );
        let last_msg = chats[3]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert!(last_msg["content"]
            .as_str()
            .unwrap()
            .starts_with("Step limit reached."));
        assert!(conv.wf_text().contains("step cap:"));
    }

    #[test]
    fn fast_lane_needs_tools_escalates_to_heavy_with_tools() {
        let m = Mock::start(chat_script(vec![
            text_resp("NEEDS_TOOLS"),
            text_resp("heavy says hi"),
        ]));
        let (mut a, mut conv) = agent(&m, |c| c.prefix_stable = false);
        let mut sink = VecSink::default();
        let r = a.turn(&mut conv, "hello there", &mut sink).unwrap();
        assert_eq!(r, "heavy says hi");
        assert_eq!(sink.text, "heavy says hi"); // NEEDS_TOOLS was held back, never printed
        let chats = m.bodies("/api/chat");
        assert_eq!(chat_models(&m), ["fast-a", "heavy-a"]);
        assert!(chats[0].get("tools").is_none());
        assert!(chats[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("NEEDS_TOOLS"));
        assert!(chats[1]["tools"].as_array().unwrap().len() >= 13);
        assert!(!chats[1]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("FAST LANE"));
        assert!(sink.status.iter().any(|s| s.contains("handed this turn")));
    }

    #[test]
    fn fast_lane_empty_reply_escalates_and_plain_reply_stays_fast() {
        let m = Mock::start(chat_script(vec![text_resp(""), text_resp("heavy")]));
        let (mut a, mut conv) = agent(&m, |_| {});
        assert_eq!(
            a.turn(&mut conv, "hello there", &mut VecSink::default())
                .unwrap(),
            "heavy"
        );
        assert_eq!(chat_models(&m), ["fast-a", "heavy-a"]);
        let m = Mock::start(chat_script(vec![text_resp("Hi!")]));
        let (mut a, mut conv) = agent(&m, |_| {});
        let mut sink = VecSink::default();
        assert_eq!(a.turn(&mut conv, "hello there", &mut sink).unwrap(), "Hi!");
        assert_eq!(sink.text, "Hi!");
        assert_eq!(chat_models(&m), ["fast-a"]);
    }

    #[test]
    fn core_file_and_workflow_go_into_system_prompt_and_reload_on_change() {
        let d = tmpdir("core");
        let core = d.join("core.md");
        std::fs::write(&core, "ALWAYS BE KIND").unwrap();
        let cp = core.to_string_lossy().into_owned();
        let m = Mock::start(chat_script(vec![text_resp("a"), text_resp("b")]));
        let (mut a, mut conv) = agent(&m, |c| {
            c.core_file = Some(cp);
            c.owner_context = "Owner context line.".into();
            c.prefix_stable = false;
        });
        conv.wf_update(Some(&json!([{"text": "pending thing"}])), None);
        a.turn(&mut conv, "please read it", &mut VecSink::default())
            .unwrap();
        let sys = m.bodies("/api/chat")[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            sys.contains("ALWAYS BE KIND")
                && sys.contains("pending thing")
                && sys.contains("Owner context line.")
        );
        assert!(sys.contains("Perennos Tempo"));
        std::fs::write(&core, "NEW CORE TEXT HERE").unwrap();
        let t = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&core)
            .unwrap()
            .set_modified(t)
            .unwrap();
        a.turn(&mut conv, "please read it again", &mut VecSink::default())
            .unwrap();
        let sys2 = m.bodies("/api/chat")[1]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(sys2.contains("NEW CORE TEXT HERE") && !sys2.contains("ALWAYS BE KIND"));
        assert_eq!(
            conv.messages
                .iter()
                .filter(|m| m["role"] == "system")
                .count(),
            1
        );
    }

    #[test]
    fn unknown_tool_and_string_arguments_are_handled() {
        let call = (
            200,
            ndjson(&[
                json!({"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "nope_tool", "arguments": "{\"a\":1}"}}]}, "done": true}),
            ]),
        );
        let m = Mock::start(chat_script(vec![call, text_resp("ok")]));
        let (mut a, mut conv) = agent(&m, |_| {});
        a.turn(&mut conv, "run it", &mut VecSink::default())
            .unwrap();
        let last = m.bodies("/api/chat")[1]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["content"], "unknown tool nope_tool");
    }

    #[test]
    fn prefix_stable_requests_only_ever_append() {
        let d = tmpdir("stable");
        let f = d.join("facts.md");
        std::fs::write(&f, "The launch date is the fourth of May.").unwrap();
        let fp = f.to_string_lossy().into_owned();
        let m = Mock::start(chat_script(vec![
            (
                200,
                ndjson(&[
                    json!({"message": {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "workflow_update", "arguments": {"note": "noted"}}}]}, "done": true}),
                ]),
            ),
            text_resp("done one"),
            text_resp("done two"),
        ]));
        let (mut a, mut conv) = agent(&m, |c| c.rag_files = vec![fp]);
        assert!(a.cfg().prefix_stable, "on by default");
        a.turn(
            &mut conv,
            "read: when is the launch date",
            &mut VecSink::default(),
        )
        .unwrap();
        a.turn(
            &mut conv,
            "read: and the venue please",
            &mut VecSink::default(),
        )
        .unwrap();
        let bodies = m.bodies("/api/chat");
        assert_eq!(bodies.len(), 3);
        // the system message never changes, and memory rides in the user message
        let sys = |b: &Value| b["messages"][0].clone();
        assert_eq!(sys(&bodies[0]), sys(&bodies[2]));
        assert!(!sys(&bodies[0])["content"]
            .as_str()
            .unwrap()
            .contains("fourth of May"));
        assert!(bodies[0]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("[facts.md] The launch date is the fourth of May."));
        // every request's messages start with the previous request's messages: the engine can
        // reuse its cached prefix and only processes the new tokens, across steps AND turns
        for pair in bodies.windows(2) {
            let (prev, next) = (
                pair[0]["messages"].as_array().unwrap(),
                pair[1]["messages"].as_array().unwrap(),
            );
            assert!(next.len() > prev.len());
            assert_eq!(
                &next[..prev.len()],
                &prev[..],
                "a request rewrote earlier messages"
            );
            assert_eq!(pair[0]["tools"], pair[1]["tools"], "the tool block changed");
        }
    }

    #[test]
    fn sticky_tools_keep_earlier_offers_sorted_and_capped() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            sticky_tools(&[], &s(&["b__x", "a__y"]), 8),
            s(&["a__y", "b__x"])
        );
        assert_eq!(
            sticky_tools(&s(&["a__y", "b__x"]), &s(&["c__z"]), 8),
            s(&["a__y", "b__x", "c__z"])
        );
        assert_eq!(
            sticky_tools(&s(&["a__y", "b__x"]), &s(&["c__z", "a__y"]), 2),
            s(&["a__y", "b__x"]),
            "earlier offers win"
        );
        assert_eq!(
            sticky_tools(&s(&["a__y"]), &[], 8),
            s(&["a__y"]),
            "an unrelated prompt keeps the block"
        );
    }

    #[test]
    fn trimming_with_headroom_happens_rarely() {
        let msg =
            |i: usize| json!({"role": "user", "content": format!("{i:04} {}", "x".repeat(90))});
        let mut history = vec![json!({"role": "system", "content": "s"})];
        let mut trims = 0;
        for i in 0..200 {
            history.push(msg(i));
            let before = history.len();
            history = trim_to(history, 2000, 1200);
            if history.len() < before {
                trims += 1;
            }
        }
        assert!(trims <= 25, "{trims} trims in 200 appends");
        let plain = (0..200)
            .fold(
                (vec![json!({"role": "system", "content": "s"})], 0),
                |(h, n), i| {
                    let mut h = h;
                    h.push(msg(i));
                    let before = h.len();
                    let h = trim(h, 2000);
                    let n = n + usize::from(h.len() < before);
                    (h, n)
                },
            )
            .1;
        assert!(
            plain > 150,
            "without headroom almost every append trims ({plain})"
        );
    }

    #[test]
    fn rag_memory_goes_into_system_prompt() {
        let d = tmpdir("ragturn");
        let f = d.join("facts.md");
        std::fs::write(&f, "The launch date is the fourth of May.").unwrap();
        let fp = f.to_string_lossy().into_owned();
        let m = Mock::start(chat_script(vec![text_resp("May 4 [facts.md]")]));
        let (mut a, mut conv) = agent(&m, |c| {
            c.rag_files = vec![fp];
            c.prefix_stable = false;
        });
        a.turn(
            &mut conv,
            "read: when is the launch date",
            &mut VecSink::default(),
        )
        .unwrap();
        let sys = m.bodies("/api/chat")[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(sys.contains("[facts.md] The launch date is the fourth of May."));
        assert!(m.bodies("/api/embed").len() <= 1);
    }
}
