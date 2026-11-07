//! Ollama /api/chat streaming (NDJSON) with tool calls, timing log, make_room, embeddings.
use crate::config::Config;
use crate::util::{self, http_json, post_stream};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ChatInfo {
    pub model: String,
    /// The engine that answered, when it was not `ollama_url`.
    pub engine: Option<String>,
    /// The engine that answered, always (`engine` leaves out the main one).
    pub served_by: String,
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
    let extra = Extra::default();
    chat_at(
        cfg,
        &cfg.ollama_url,
        model,
        messages,
        tools,
        think,
        &extra,
        on_text,
    )
}

/// What a request adds to the usual ones.
#[derive(Clone, Copy, Debug, Default)]
pub struct Extra<'a> {
    /// A JSON Schema the answer must follow (Ollama's `format`). An engine or model that
    /// refuses it is asked again without, once, and not asked with it again.
    pub format: Option<&'a Value>,
    /// Instead of the config's `keep_alive` ("0": unload as soon as the answer is out).
    pub keep_alive: Option<&'a str>,
    /// The conversation, so its context size only ever steps up.
    pub conv: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
fn chat_at(
    cfg: &Config,
    engine: &str,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    think: bool,
    extra: &Extra,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    let mut extra = *extra;
    if extra.format.is_some() && format_refused(engine, model) {
        extra.format = None;
    }
    let mut go = |extra: &Extra| match chat_raw(
        cfg, engine, model, messages, tools, think, extra, on_text,
    ) {
        Err(e) if think && e.contains("does not support thinking") => {
            chat_raw(cfg, engine, model, messages, tools, false, extra, on_text)
        }
        r => r,
    };
    match go(&extra) {
        Err(e) if extra.format.is_some() && format_rejected(&e) => {
            if let Ok(mut r) = refused().lock() {
                r.insert((engine.to_string(), model.to_string()));
            }
            extra.format = None;
            go(&extra)
        }
        r => r,
    }
}

fn refused() -> &'static Mutex<HashSet<(String, String)>> {
    static R: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn format_refused(engine: &str, model: &str) -> bool {
    refused()
        .lock()
        .is_ok_and(|r| r.contains(&(engine.to_string(), model.to_string())))
}

/// An engine's answer that it will not take a `format` (an old Ollama, or an OpenAI-style
/// server that does not know the field, or a model that cannot do structured output).
pub fn format_rejected(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    !unreachable(&e)
        && ["format", "schema", "grammar", "structured"]
            .iter()
            .any(|k| e.contains(k))
}

/// The context sizes a conversation steps through.
pub const CTX_STEPS: [u64; 4] = [4096, 8192, 16384, 32768];

/// The context for a request needing about `need` tokens: the smallest step that holds it,
/// never below `floor` (what the conversation already used: a new size makes Ollama read the
/// whole prompt again) and never above `cap` (the config's, then the model's own). A cap that
/// shrank wins over the floor, since the model cannot take more.
pub fn ctx_step(need: u64, floor: u64, cap: u64) -> u64 {
    let step = CTX_STEPS
        .iter()
        .copied()
        .find(|s| *s >= need)
        .unwrap_or(u64::MAX);
    step.max(floor).min(cap.max(512))
}

fn ctx_seen() -> &'static Mutex<HashMap<String, u64>> {
    static C: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

type Lens = Mutex<HashMap<(String, String), Option<u64>>>;

fn model_lens() -> &'static Lens {
    static L: OnceLock<Lens> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// The model's own context length (`/api/show`), asked once per engine and model.
fn model_ctx_len(engine: &str, model: &str) -> Option<u64> {
    let key = (engine.to_string(), model.to_string());
    if let Some(v) = model_lens().lock().ok().and_then(|m| m.get(&key).copied()) {
        return v;
    }
    let len = http_json(
        &format!("{engine}/api/show"),
        Some(&json!({"model": model})),
        Duration::from_secs(5),
    )
    .ok()
    .and_then(|v| {
        v.get("model_info")?
            .as_object()?
            .iter()
            .find(|(k, _)| k.ends_with(".context_length"))
            .and_then(|(_, n)| n.as_u64())
    });
    if let Ok(mut m) = model_lens().lock() {
        m.insert(key, len);
    }
    len
}

/// The `num_ctx` for this request: sized to the messages and tools plus room for the answer,
/// held at the largest size the conversation has used.
fn num_ctx_for_request(
    cfg: &Config,
    engine: &str,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    conv: Option<&str>,
) -> u64 {
    // about 3 bytes a token is a little pessimistic, which is the safe side
    let bytes = Value::from(messages.to_vec()).to_string().len()
        + Value::from(tools.to_vec()).to_string().len();
    let need = bytes as u64 / 3 + 1024;
    let mut cap = cfg.num_ctx_for(model);
    let floor = conv
        .and_then(|c| ctx_seen().lock().ok()?.get(c).copied())
        .unwrap_or(0);
    if need.max(floor) > CTX_STEPS[0] {
        if let Some(n) = model_ctx_len(engine, model) {
            cap = cap.min(n);
        }
    }
    let n = if need > CTX_STEPS[3] {
        cap // more than the last step: as much as the config allows
    } else {
        ctx_step(need, floor, cap)
    };
    if let (Some(c), Ok(mut m)) = (conv, ctx_seen().lock()) {
        if m.len() > 512 {
            m.clear();
        }
        m.insert(c.to_string(), n);
    }
    n
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

/// The engines to try, in order: the one the conversation is pinned to, if any (and the settings
/// still list it), then the one that served this conversation last (sticky, so the engine's
/// prompt cache keeps working), then `ollama_url`, then `ollama_fallbacks`. Engines that failed
/// twice in a row lately go last, not away: with nothing else up they are still tried.
pub fn engine_order(cfg: &Config, conv: Option<&str>, pin: Option<&str>) -> Vec<String> {
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
    // a pin wins over affinity: it says where this conversation belongs, and a pinned engine
    // that is up again takes the conversation back
    if let Some(p) = pin.and_then(|p| cfg.find_engine(p)) {
        let same = |x: &String| x.trim_end_matches('/') == p.trim_end_matches('/');
        v.retain(|x| !same(x));
        v.insert(0, p);
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

/// An engine's answer that it does not have the model (Ollama: `model "x" not found, try
/// pulling it first`, as an in-band error or as the body of an HTTP 404). Another engine may.
pub fn model_missing(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    !unreachable(&e) && e.contains("model") && e.contains("not found")
}

/// The plain-words error for a request that failed because no engine could be reached at all
/// (the one `ollama_url`, or every one of it and `ollama_fallbacks`); `None` for any other
/// error, such as an engine's own answer. Retrying or trying another model cannot help here:
/// every model lives on the same engines.
pub fn nobody_answering(cfg: &Config, pin: Option<&str>, err: &str) -> Option<String> {
    if !unreachable(err) {
        return None;
    }
    let order = engine_order(cfg, None, pin);
    // with several engines, `chat_pooled` says "(every engine: ..)" only after trying them all
    if order.len() > 1 && !err.contains("(every engine: ") {
        return None;
    }
    Some(format!(
        "Ollama isn't answering at {}. Start Ollama, or install it from https://ollama.com/download \
         (tempo-launch setup --apply can fetch it for you), then try again.",
        order.join(", ")
    ))
}

/// `chat_with` on the first engine that can be reached (see `engine_order`). Two answers move a
/// request to the next engine: "cannot be reached", and "this engine does not have the model"
/// (another engine may, which is why a conversation can be pinned to one that does). Any other
/// error of an engine (a crash, out of memory) comes back as it is, for the caller to retry and
/// fall back to another model as before: the next engine would only repeat it. `conv` keeps the
/// conversation on the engine that answered; `pin` is the engine it asked for.
#[allow(clippy::too_many_arguments)]
pub fn chat_pooled(
    cfg: &Config,
    conv: Option<&str>,
    pin: Option<&str>,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    think: bool,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    let extra = Extra {
        conv,
        ..Extra::default()
    };
    chat_pooled_with(
        cfg, conv, pin, model, messages, tools, think, &extra, on_text,
    )
}

/// `chat_pooled` with a `format`, a different `keep_alive`, or both.
#[allow(clippy::too_many_arguments)]
pub fn chat_pooled_with(
    cfg: &Config,
    conv: Option<&str>,
    pin: Option<&str>,
    model: &str,
    messages: &[Value],
    tools: &[Value],
    think: bool,
    extra: &Extra,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    let extra = Extra {
        conv: extra.conv.or(conv),
        ..*extra
    };
    let order = engine_order(cfg, conv, pin);
    let pinned = pin.and_then(|p| {
        order
            .iter()
            .find(|u| u.trim_end_matches('/') == p.trim_end_matches('/'))
    });
    let mut moved: Vec<String> = vec![];
    let mut last = String::from("no engine");
    for engine in &order {
        match chat_at(
            cfg,
            engine,
            model,
            messages,
            tools,
            think,
            &extra,
            &mut *on_text,
        ) {
            Ok(mut r) => {
                mark(engine, true);
                if let (Some(c), Ok(mut a)) = (conv, affinity().lock()) {
                    a.insert(c.to_string(), engine.clone());
                }
                r.info.engine = (*engine != cfg.ollama_url).then(|| engine.clone());
                r.info.served_by = engine.clone();
                if !moved.is_empty() {
                    r.info.failover = Some(format!("{}; {engine} answered", moved.join("; ")));
                }
                return Ok(r);
            }
            Err(e) if unreachable(&e) && order.len() > 1 => {
                mark(engine, false);
                let what = if pinned == Some(engine) {
                    "pinned engine"
                } else {
                    "engine"
                };
                moved.push(format!(
                    "{what} {engine} cannot be reached ({})",
                    util::head_chars(&e, 80)
                ));
                last = e;
            }
            // the engine is up and answered, so it is not marked: it just lacks this model
            Err(e) if model_missing(&e) && order.len() > 1 => {
                moved.push(format!("engine {engine} does not have {model}"));
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
    extra: &Extra,
    on_text: &mut dyn FnMut(&str),
) -> Result<ChatResult, String> {
    let num_ctx = num_ctx_for_request(cfg, engine, model, messages, tools, extra.conv);
    let mut body = json!({"model": model, "messages": messages, "stream": true,
        "keep_alive": extra.keep_alive.unwrap_or(&cfg.keep_alive),
        "options": {"num_ctx": num_ctx, "temperature": cfg.temperature}});
    if let Some(f) = extra.format {
        body["format"] = f.clone();
    }
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
        served_by: engine.to_string(),
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
        assert_eq!(b["options"]["num_ctx"], 4096, "a short chat starts small");
        assert_eq!(b["keep_alive"], "60m");
        assert!(b.get("format").is_none());
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

    fn say(text: &str) -> Vec<Value> {
        vec![json!({"role": "user", "content": text})]
    }

    #[test]
    fn the_context_steps_up_and_never_down() {
        assert_eq!(ctx_step(100, 0, 32768), 4096);
        assert_eq!(ctx_step(5000, 0, 32768), 8192);
        assert_eq!(ctx_step(20000, 0, 32768), 32768);
        assert_eq!(ctx_step(100, 16384, 32768), 16384, "held at what it was");
        assert_eq!(ctx_step(20000, 0, 8192), 8192, "the config caps it");
        assert_eq!(
            ctx_step(100, 16384, 8192),
            8192,
            "a smaller cap wins over the floor"
        );
    }

    #[test]
    fn num_ctx_is_the_same_on_every_turn_of_a_conversation() {
        let m = Mock::start(chat_script(vec![
            text_resp("a"),
            text_resp("b"),
            text_resp("c"),
            text_resp("d"),
        ]));
        let mut c = cfg_for(&m);
        c.default_num_ctx = 32768;
        let big = "word ".repeat(4000); // about 20 KB: wants 8192
        let turns = [
            ("hi", 4096),
            (big.as_str(), 8192),
            ("ok", 8192),
            ("thanks", 8192),
        ];
        for (text, _) in turns {
            chat_pooled(
                &c,
                Some("conv-steady"),
                None,
                "m-steady",
                &say(text),
                &[],
                false,
                &mut |_| {},
            )
            .unwrap();
        }
        let got: Vec<u64> = m
            .bodies("/api/chat")
            .iter()
            .map(|b| b["options"]["num_ctx"].as_u64().unwrap())
            .collect();
        assert_eq!(got, turns.map(|t| t.1));
        // another conversation starts small again
        let m2 = Mock::start(chat_script(vec![text_resp("a")]));
        let mut c2 = cfg_for(&m2);
        c2.default_num_ctx = 32768;
        chat_pooled(
            &c2,
            Some("conv-other"),
            None,
            "m-steady",
            &say("hi"),
            &[],
            false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(m2.bodies("/api/chat")[0]["options"]["num_ctx"], 4096);
    }

    #[test]
    fn num_ctx_never_goes_past_the_models_own_length() {
        let chat = std::sync::Mutex::new(vec![text_resp("a")].into_iter());
        let m = Mock::start(move |r| match r.path.as_str() {
            "/api/show" => (
                200,
                json!({"model_info": {"general.architecture": "x", "x.context_length": 6000}})
                    .to_string(),
            ),
            _ => chat.lock().unwrap().next().unwrap_or((500, "out".into())),
        });
        let mut c = cfg_for(&m);
        c.default_num_ctx = 32768;
        let big = "word ".repeat(6000);
        chat_pooled(
            &c,
            None,
            None,
            "m-short",
            &say(&big),
            &[],
            false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(m.bodies("/api/chat")[0]["options"]["num_ctx"], 6000);
    }

    #[test]
    fn format_and_keep_alive_go_out_when_asked_for() {
        let m = Mock::start(chat_script(vec![text_resp("{}")]));
        let c = cfg_for(&m);
        let schema = json!({"type": "object", "required": ["a"]});
        let extra = Extra {
            format: Some(&schema),
            keep_alive: Some("0"),
            conv: None,
        };
        chat_pooled_with(
            &c,
            None,
            None,
            "m-fmt",
            &say("x"),
            &[],
            false,
            &extra,
            &mut |_| {},
        )
        .unwrap();
        let b = &m.bodies("/api/chat")[0];
        assert_eq!(b["format"], schema);
        assert_eq!(b["keep_alive"], "0");
    }

    #[test]
    fn an_engine_that_refuses_format_is_asked_again_without_it_once() {
        let m = Mock::start(chat_script(vec![
            (400, "json: cannot unmarshal object into Go struct field ChatRequest.format of type string".into()),
            text_resp("fine"),
            text_resp("fine again"),
        ]));
        let c = cfg_for(&m);
        let schema = json!({"type": "object"});
        let extra = Extra {
            format: Some(&schema),
            ..Extra::default()
        };
        let ask = || {
            chat_pooled_with(
                &c,
                None,
                None,
                "m-refuse",
                &say("x"),
                &[],
                false,
                &extra,
                &mut |_| {},
            )
        };
        assert_eq!(ask().unwrap().message["content"], "fine");
        assert_eq!(ask().unwrap().message["content"], "fine again");
        let b = m.bodies("/api/chat");
        assert_eq!(
            b.len(),
            3,
            "one refusal, one retry, then no second try with format"
        );
        assert!(b[0].get("format").is_some());
        assert!(b[1].get("format").is_none());
        assert!(b[2].get("format").is_none());
    }

    #[test]
    fn other_errors_do_not_pass_for_a_format_refusal() {
        assert!(format_rejected("HTTP 400: invalid format"));
        assert!(format_rejected("HTTP 500: failed to load grammar"));
        assert!(!format_rejected("Connection refused (os error 111)"));
        assert!(!format_rejected("HTTP 500: out of memory"));
        let m = Mock::start(chat_script(vec![(500, "out of memory".into())]));
        let schema = json!({});
        let extra = Extra {
            format: Some(&schema),
            ..Extra::default()
        };
        let e = chat_pooled_with(
            &cfg_for(&m),
            None,
            None,
            "m-oom",
            &say("x"),
            &[],
            false,
            &extra,
            &mut |_| {},
        )
        .err()
        .unwrap();
        assert!(e.contains("out of memory"));
        assert_eq!(m.bodies("/api/chat").len(), 1);
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
