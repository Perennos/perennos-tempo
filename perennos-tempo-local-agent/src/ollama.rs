//! Ollama /api/chat streaming (NDJSON) with tool calls, timing log, make_room, embeddings.
use crate::config::Config;
use crate::util::{self, http_json, post_stream};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ChatInfo {
    pub model: String,
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
            "{}: first token {:.2} s, total {:.2} s (load {:.2} s, prompt {} tok in {:.2} s, {} out)",
            self.model,
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
    match chat_raw(cfg, model, messages, tools, think, on_text) {
        Err(e) if think && e.contains("does not support thinking") => {
            chat_raw(cfg, model, messages, tools, false, on_text)
        }
        r => r,
    }
}

fn chat_raw(
    cfg: &Config,
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
    make_room(cfg, model);
    let t0 = Instant::now();
    let resp = post_stream(
        &format!("{}/api/chat", cfg.ollama_url),
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
    let rec = json!({"model": i.model, "first_s": i.first_s, "total_s": i.total_s, "load_s": i.load_s,
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

fn unload_loaded(cfg: &Config, keep: Option<&str>, only_ours: bool) -> usize {
    let Ok(ps) = http_json(
        &format!("{}/api/ps", cfg.ollama_url),
        None,
        Duration::from_secs(5),
    ) else {
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
            &format!("{}/api/generate", cfg.ollama_url),
            Some(&json!({"model": name, "keep_alive": 0})),
            Duration::from_secs(60),
        );
        n += 1;
    }
    n
}

/// Before a model whose name matches `exclusive_models`, unload our other models.
pub fn make_room(cfg: &Config, model: &str) {
    if !cfg
        .exclusive_models
        .iter()
        .any(|x| !x.is_empty() && model.contains(x.as_str()))
    {
        return;
    }
    if unload_loaded(cfg, Some(model), true) > 0 {
        std::thread::sleep(Duration::from_millis(2000));
    }
}

/// Unload only our own models (used before ComfyUI takes the GPU).
pub fn unload_ours(cfg: &Config) -> usize {
    unload_loaded(cfg, None, true)
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
