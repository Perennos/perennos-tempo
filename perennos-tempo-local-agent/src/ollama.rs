//! Ollama /api/chat streaming (NDJSON) with tool calls, timing log, make_room, embeddings.
use crate::config::Config;
use crate::util::{self, http_json, post_stream};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ChatInfo {
    pub model: String,
    /// The engine that answered, when it was not `ollama_url`.
    pub engine: Option<String>,
    /// How the request moved off an engine that could not be reached, when it did.
    pub failover: Option<String>,
    pub first_s: f64,
    pub total_s: f64,
    pub load_s: f64,
    pub prompt_tok: Option<u64>,
    pub prompt_s: f64,
    pub out_tok: Option<u64>,
}

impl ChatInfo {
    pub fn line(&self) -> String {
        format!(
            "{}{}: first token {:.2} s, total {:.2} s (load {:.2} s, prompt {} tok in {:.2} s, {} out)",
            self.model,
            self.engine
                .as_deref()
                .map(|e| format!(" on {e}"))
                .unwrap_or_default(),
            self.first_s,
            self.total_s,
            self.load_s,
            self.prompt_tok.map(|n| n.to_string()).unwrap_or("?".into()),
            self.prompt_s,
            self.out_tok.map(|n| n.to_string()).unwrap_or("?".into())
        )
    }
}

pub struct ChatResult {
    pub message: Value,
    pub info: ChatInfo,
}

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// One streaming request. Any HTTP error, transport error or in-band `{"error":..}` line is Err.
/// `on_text` gets each text piece as it arrives (an error line is never passed to it).
pub fn chat_once(
    cfg: &Config,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    chat_with(cfg, model, messages, tools, false, on_text)
}

/// `chat_once`, thinking first when `think` (models that cannot think answer as usual: the
/// engine's "does not support thinking" is retried once without it).
pub fn chat_with(
    cfg: &Config,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    think: bool,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    chat_at(cfg, &cfg.ollama_url, model, messages, tools, think, on_text)
}

#[allow(clippy::too_many_arguments)]
fn chat_at(
    cfg: &Config,
    engine: &str,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    think: bool,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    match chat_raw(cfg, engine, model, messages, tools, think, on_text) {
        Err(e) if think && e.contains("does not support thinking") => {
            chat_raw(cfg, engine, model, messages, tools, false, on_text)
        }
        r => r,
    }
}

/// How long an engine that failed twice in a row is passed over before it is tried again.
pub const ENGINE_REST: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Health {
    fails: u32,
    resting_until: Option<Instant>,
}

/// Engines' recent health, as a load balancer's circuit breaker keeps it, and which engine
/// served each conversation last (its prompt is cached there).
fn health() -> &'static Mutex<HashMap<String, Health>> {
    static H: OnceLock<Mutex<HashMap<String, Health>>> = OnceLock::new();
    H.get_or_init(Default::default)
}

fn affinity() -> &'static Mutex<HashMap<String, String>> {
    static A: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

fn mark(engine: &str, ok: bool) {
    let Ok(mut h) = health().lock() else { return };
    let e = h.entry(engine.to_string()).or_default();
    if ok {
        *e = Health::default();
    } else {
        e.fails += 1;
        if e.fails >= 2 {
            e.resting_until = Some(Instant::now() + ENGINE_REST);
        }
    }
}

/// The engines to try, in order: the one that served this conversation last (sticky, so the
/// engine's prompt cache keeps working), then `ollama_url`, then `ollama_fallbacks`. Engines
/// that failed twice in a row lately go last, not away: with nothing else up they are still tried.
pub fn engine_order(cfg: &Config, conv: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = vec![cfg.ollama_url.clone()];
    for u in &cfg.ollama_fallbacks {
        if !v.contains(u) {
            v.push(u.clone());
        }
    }
    let sticky = conv.and_then(|c| affinity().lock().ok()?.get(c).cloned());
    if let Some(i) = sticky.and_then(|s| v.iter().position(|x| *x == s)) {
        let s = v.remove(i);
        v.insert(0, s);
    }
    let now = Instant::now();
    let resting = |u: &String| {
        health()
            .lock()
            .ok()
            .and_then(|h| h.get(u).and_then(|x| x.resting_until))
            .is_some_and(|t| t > now)
    };
    let (rest, up): (Vec<String>, Vec<String>) = v.into_iter().partition(|u| resting(u));
    up.into_iter().chain(rest).collect()
}

/// An error that says the engine could not be reached at all (as opposed to an answer from it).
pub fn unreachable(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    [
        "connection refused",
        "connection reset",
        "connection failed",
        "host not found",
        "no route to host",
        "unreachable",
        "timeout: connect",
        "timeout: resolve",
        // Windows words these differently (WSAECONNREFUSED, WSAECONNRESET, WSAETIMEDOUT,
        // WSAENETUNREACH, WSAEHOSTUNREACH)
        "actively refused",
        "forcibly closed",
        "did not properly respond",
        "os error 10061",
        "os error 10054",
        "os error 10060",
        "os error 10051",
        "os error 10065",
    ]
    .iter()
    .any(|k| e.contains(k))
}

/// `chat_with` on the first engine that can be reached (see `engine_order`). Only "cannot be
/// reached" moves a request to another engine; an engine's own error (a model it does not have,
/// a crash) comes back as it is, for the caller to retry and fall back as before. `conv` keeps
/// the conversation on the engine that answered.
#[allow(clippy::too_many_arguments)]
pub fn chat_pooled(
    cfg: &Config,
    conv: Option<&str>,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    think: bool,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    let order = engine_order(cfg, conv);
    let mut moved: Vec<String> = vec![];
    let mut last = String::from("no engine");
    for engine in &order {
        match chat_at(cfg, engine, model, messages, tools, think, &mut *on_text) {
            Ok(mut r) => {
                mark(engine, true);
                if let (Some(c), Ok(mut a)) = (conv, affinity().lock()) {
                    a.insert(c.to_string(), engine.clone());
                }
                r.info.engine = (*engine != cfg.ollama_url).then(|| engine.clone());
                if !moved.is_empty() {
                    r.info.failover = Some(format!("{}; {engine} answered", moved.join("; ")));
                }
                return Ok(r);
            }
            Err(e) if unreachable(&e) && order.len() > 1 => {
                mark(engine, false);
                moved.push(format!(
                    "engine {engine} cannot be reached ({})",
                    util::head_chars(&e, 80)
                ));
                last = e;
            }
            Err(e) => return Err(e),
        }
    }
    Err(format!("{} (every engine: {})", last, order.join(", ")))
}

#[allow(clippy::too_many_arguments)]
fn chat_raw(
    cfg: &Config,
    engine: &str,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    think: bool,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    let mut body = json!({"model": model, "messages": messages, "stream": true, "keep_alive": cfg.keep_alive,
        "options": {"num_ctx": cfg.num_ctx_for(model), "temperature": cfg.temperature}});
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if think {
        body["think"] = json!(true);
    } else if Config::is_qwen(model) {
        body["think"] = json!(false); // other families never get think:false
    }
    make_room_at(cfg, engine, model);
    let t0 = Instant::now();
    let resp = post_stream(
        &format!("{engine}/api/chat"),
        &body,
        Duration::from_secs(cfg.chat_timeout_s),
    )?;
    let mut rd = BufReader::new(resp.into_body().into_reader());
    let (mut first, mut parts, mut calls, mut fin) =
        (None::<f64>, String::new(), Vec::<Value>::new(), Value::Null);
    let mut line = String::new();
    loop {
        line.clear();
        match rd.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("stream read: {e}")),
        }
        if line.trim().is_empty() {
            continue;
        }
        let d: Value = serde_json::from_str(&line).map_err(|e| format!("bad stream line: {e}"))?;
        if let Some(e) = d.get("error").filter(|e| !e.is_null() && *e != &json!("")) {
            return Err(util::head_chars(
                &e.as_str().map(String::from).unwrap_or(e.to_string()),
                200,
            ));
        }
        let m = d.get("message").cloned().unwrap_or(Value::Null);
        let piece = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
        let tc = m
            .get("tool_calls")
            .and_then(|c| c.as_array())
            .filter(|a| !a.is_empty());
        if let Some(a) = tc {
            calls.extend(a.iter().cloned());
        }
        if (!piece.is_empty() || tc.is_some()) && first.is_none() {
            first = Some(t0.elapsed().as_secs_f64());
        }
        if !piece.is_empty() {
            parts.push_str(piece);
            on_text(piece);
        }
        if d.get("done").and_then(|x| x.as_bool()).unwrap_or(false) {
            fin = d;
            break;
        }
    }
    let total = t0.elapsed().as_secs_f64();
    let mut message = json!({"role": "assistant", "content": parts});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let ns = |k: &str| fin.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) / 1e9;
    let info = ChatInfo {
        model: model.to_string(),
        engine: None,
        failover: None,
        first_s: r2(first.unwrap_or(total)),
        total_s: r2(total),
        load_s: r2(ns("load_duration")),
        prompt_tok: fin.get("prompt_eval_count").and_then(|x| x.as_u64()),
        prompt_s: r2(ns("prompt_eval_duration")),
        out_tok: fin.get("eval_count").and_then(|x| x.as_u64()),
    };
    log_timing(cfg, &info);
    Ok(ChatResult { message, info })
}

fn log_timing(cfg: &Config, i: &ChatInfo) {
    let rec = json!({"model": i.model, "engine": i.engine, "first_s": i.first_s, "total_s": i.total_s, "load_s": i.load_s,
        "prompt_tok": i.prompt_tok, "prompt_s": i.prompt_s, "out_tok": i.out_tok, "t": util::now_str()});
    let _ = std::fs::create_dir_all(cfg.logs_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.logs_dir().join("timing.jsonl"))
    {
        let _ = writeln!(f, "{rec}");
    }
}

fn base_name(n: &str) -> &str {
    n.split(':').next().unwrap_or(n)
}

fn ours(cfg: &Config) -> Vec<String> {
    let mut v: Vec<String> = cfg
        .models
        .iter()
        .chain(cfg.fast_models.iter())
        .cloned()
        .collect();
    v.push(cfg.embed_model.clone());
    v
}

fn unload_loaded(cfg: &Config, engine: &str, keep: Option<&str>, only_ours: bool) -> usize {
    let Ok(ps) = http_json(&format!("{engine}/api/ps"), None, Duration::from_secs(5)) else {
        return 0;
    };
    let mine = ours(cfg);
    let mut n = 0;
    for m in ps
        .get("models")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let name = m.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if Some(base_name(name)) == keep
            || (only_ours && !mine.iter().any(|o| o == base_name(name)))
        {
            continue;
        }
        let _ = http_json(
            &format!("{engine}/api/generate"),
            Some(&json!({"model": name, "keep_alive": 0})),
            Duration::from_secs(60),
        );
        n += 1;
    }
    n
}

/// Before a model whose name matches `exclusive_models`, unload our other models.
pub fn make_room(cfg: &Config, model: &str) {
    make_room_at(cfg, &cfg.ollama_url, model)
}

/// `make_room` on the engine that will run the model.
fn make_room_at(cfg: &Config, engine: &str, model: &str) {
    if !cfg
        .exclusive_models
        .iter()
        .any(|x| !x.is_empty() && model.contains(x.as_str()))
    {
        return;
    }
    if unload_loaded(cfg, engine, Some(model), true) > 0 {
        std::thread::sleep(Duration::from_millis(2000));
    }
}

/// Unload only our own models (used before ComfyUI takes the GPU).
pub fn unload_ours(cfg: &Config) -> usize {
    unload_loaded(cfg, &cfg.ollama_url, None, true)
}

/// Embeddings on the CPU only (`num_gpu: 0`, always). None on any failure.
pub fn embed(cfg: &Config, texts: &[String]) -> Option<Vec<Vec<f64>>> {
    let body = json!({"model": cfg.embed_model, "input": texts, "keep_alive": "30m", "options": {"num_gpu": 0}});
    let r = http_json(
        &format!("{}/api/embed", cfg.ollama_url),
        Some(&body),
        Duration::from_secs(120),
    )
    .ok()?;
    let arr = r.get("embeddings")?.as_array()?;
    let out: Vec<Vec<f64>> = arr
        .iter()
        .map(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
                .unwrap_or_default()
        })
        .collect();
    (out.len() == texts.len()).then_some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_engine_that_is_off_counts_as_unreachable_on_every_system() {
        for e in [
            "Connection refused (os error 111)",
            "Connection refused (os error 61)",
            "io: No connection could be made because the target machine actively refused it. (os error 10061)",
            "io: An existing connection was forcibly closed by the remote host. (os error 10054)",
            "io: A connection attempt failed because the connected party did not properly respond after a period of time (os error 10060)",
            "timeout: connect",
        ] {
            assert!(unreachable(e), "{e}");
        }
        for e in [
            "model 'x' not found",
            "HTTP 500: out of memory",
            "bad stream line",
        ] {
            assert!(!unreachable(e), "{e}");
        }
    }

    use super::*;
    use crate::util::testutil::*;

    fn cfg_for(m: &Mock) -> Config {
        let mut c = Config::defaults(Some(tmpdir("oll").to_string_lossy().into_owned()));
        c.ollama_url = m.url.clone();
        c
    }

    #[test]
    fn streams_text_and_collects_tool_calls_across_chunks() {
        let body = ndjson(&[
            json!({"message": {"role":"assistant","content":"Hel"}, "done": false}),
            json!({"message": {"role":"assistant","content":"lo","tool_calls":[{"function":{"name":"a","arguments":{"x":1}}}]}, "done": false}),
            json!({"message": {"role":"assistant","content":"","tool_calls":[{"function":{"name":"b","arguments":{}}}]}, "done": false}),
            json!({"done": true, "prompt_eval_count": 7, "eval_count": 2, "load_duration": 1_500_000_000u64}),
        ]);
        let m = Mock::start(chat_script(vec![(200, body)]));
        let c = cfg_for(&m);
        let mut got = String::new();
        let r = chat_once(
            &c,
            "qwen-x",
            &[json!({"role":"user","content":"hi"})],
            &[json!({"type":"function"})],
            &mut |t| got.push_str(t),
        )
        .unwrap();
        assert_eq!(got, "Hello");
        assert_eq!(r.message["content"], "Hello");
        assert_eq!(r.message["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(r.info.prompt_tok, Some(7));
        assert_eq!(r.info.load_s, 1.5);
        let b = &m.bodies("/api/chat")[0];
        assert_eq!(b["think"], false);
        assert_eq!(b["stream"], true);
        assert_eq!(b["options"]["num_ctx"], 8192);
        assert!(b["tools"].is_array());
        let log = std::fs::read_to_string(c.logs_dir().join("timing.jsonl")).unwrap();
        assert!(log.contains("\"prompt_tok\":7"));
    }

    #[test]
    fn no_think_flag_for_other_families_and_no_tools_key_when_empty() {
        let m = Mock::start(chat_script(vec![text_resp("ok")]));
        let c = cfg_for(&m);
        chat_once(&c, "gpt-oss", &[], &[], &mut |_| {}).unwrap();
        let b = &m.bodies("/api/chat")[0];
        assert!(b.get("think").is_none());
        assert!(b.get("tools").is_none());
    }

    #[test]
    fn in_band_error_is_error_and_never_text() {
        let m = Mock::start(chat_script(vec![err_resp()]));
        let c = cfg_for(&m);
        let mut got = String::new();
        let e = chat_once(&c, "m", &[], &[], &mut |t| got.push_str(t))
            .err()
            .unwrap();
        assert!(e.contains("CUDA error"));
        assert_eq!(got, "");
    }

    #[test]
    fn http_error_is_error() {
        let m = Mock::start(chat_script(vec![(500, "boom".into())]));
        let c = cfg_for(&m);
        let e = chat_once(&c, "m", &[], &[], &mut |_| {}).err().unwrap();
        assert!(e.contains("500") && e.contains("boom"));
    }

    #[test]
    fn embed_always_cpu() {
        let m = Mock::start(|_| {
            (
                200,
                json!({"embeddings": [[1.0, 0.0], [0.0, 1.0]]}).to_string(),
            )
        });
        let c = cfg_for(&m);
        let v = embed(&c, &["a".into(), "b".into()]).unwrap();
        assert_eq!(v.len(), 2);
        let b = &m.bodies("/api/embed")[0];
        assert_eq!(b["options"]["num_gpu"], 0);
        assert_eq!(b["model"], "embeddinggemma");
        assert_eq!(b["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn embed_failure_is_none() {
        let m = Mock::start(|_| (500, "no".into()));
        assert!(embed(&cfg_for(&m), &["a".into()]).is_none());
    }

    #[test]
    fn make_room_unloads_only_our_other_models() {
        let m = Mock::start(|r| {
            if r.path == "/api/ps" {
                (200, json!({"models": [{"name": "big-27b:latest"}, {"name": "small:latest"}, {"name": "stranger:1"}]}).to_string())
            } else {
                (200, "{}".into())
            }
        });
        let mut c = cfg_for(&m);
        c.models = vec!["big-27b".into()];
        c.fast_models = vec!["small".into()];
        c.exclusive_models = vec!["27b".into()];
        make_room(&c, "other-model"); // not exclusive: no calls
        assert!(m.bodies("/api/ps").is_empty());
        make_room(&c, "big-27b");
        let gens = m.bodies("/api/generate");
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0]["model"], "small:latest");
        assert_eq!(gens[0]["keep_alive"], 0);
    }
}
