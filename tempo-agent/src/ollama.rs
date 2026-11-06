//! Ollama `/api/chat` types and a streaming client.
//!
//! Wire format (Ollama tool calling): an assistant message carries
//! `tool_calls: [{"function": {"name", "arguments": {...}}}]`; tool results go back as
//! `{"role": "tool", "tool_name": ..., "content": ...}`.
use crate::error::{Error, Result};
use crate::http;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::io::BufRead;
use std::time::{Duration, Instant};

pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// `OLLAMA_URL`, then `TEMPO_OLLAMA` (the Python harness's name), if set and non-empty.
pub fn ollama_url_from_env() -> Option<String> {
    ["OLLAMA_URL", "TEMPO_OLLAMA"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

fn null_as_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(default, deserialize_with = "null_as_default")]
    pub role: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub content: String,
    #[serde(default, deserialize_with = "null_as_default", skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Reasoning text some models stream; never kept in history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

impl Message {
    fn new(role: &str, content: impl Into<String>) -> Self {
        Message { role: role.to_string(), content: content.into(), ..Default::default() }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }
    pub fn tool(name: &str, content: impl Into<String>) -> Self {
        Message { tool_name: Some(name.to_string()), ..Self::new("tool", content) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

impl FunctionCall {
    /// Arguments as an object. Ollama sends an object; a JSON string or null is tolerated.
    pub fn args(&self) -> Map<String, Value> {
        match &self.arguments {
            Value::Object(m) => m.clone(),
            Value::String(s) => match serde_json::from_str::<Value>(s) {
                Ok(Value::Object(m)) => m,
                _ => Map::new(),
            },
            _ => Map::new(),
        }
    }
}

impl ToolCall {
    pub fn new(name: &str, arguments: Value) -> Self {
        ToolCall { id: None, function: FunctionCall { index: None, name: name.to_string(), arguments } }
    }
}

/// A tool offered to the model: `{"type": "function", "function": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub parameters: Value,
}

impl Tool {
    pub fn function(name: &str, description: &str, parameters: Value) -> Self {
        Tool {
            kind: "function".to_string(),
            function: ToolFunction { name: name.to_string(), description: description.to_string(), parameters },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Options {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Options>,
    /// `false` turns off thinking for models that support it (Qwen3). Never sent to gpt-oss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
}

/// One NDJSON line of a streamed reply (or the whole body of a non-streamed one).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub done_reason: Option<String>,
    #[serde(default)]
    pub load_duration: u64,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub prompt_eval_duration: u64,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timings {
    pub model: String,
    pub first_s: f64,
    pub total_s: f64,
    pub load_s: f64,
    pub prompt_tok: Option<u64>,
    pub prompt_s: f64,
    pub out_tok: Option<u64>,
}

impl Timings {
    pub fn summary(&self) -> String {
        let n = |v: Option<u64>| v.map_or("?".to_string(), |v| v.to_string());
        format!(
            "{}: first token {:.2} s, total {:.2} s (load {:.2} s, prompt {} tok in {:.2} s, {} out)",
            self.model, self.first_s, self.total_s, self.load_s, n(self.prompt_tok), self.prompt_s, n(self.out_tok)
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatResult {
    pub message: Message,
    pub timings: Timings,
}

/// What the agent loop needs from an engine. `Ollama` is the real one.
pub trait Engine {
    /// One chat call. `on_delta` receives content pieces as they stream in.
    fn chat(&self, req: &ChatRequest, on_delta: &mut dyn FnMut(&str)) -> Result<ChatResult>;
    /// Names (tag stripped) of the models loaded right now, or `None` when unknown.
    fn resident_models(&self) -> Option<Vec<String>>;
}

/// Model name without its `:tag`.
pub fn base_name(model: &str) -> &str {
    model.split(':').next().unwrap_or(model)
}

pub struct Ollama {
    pub base_url: String,
    /// Per-read timeout for chat; a slow first token (model load) must fit in it.
    pub timeout: Duration,
}

impl Ollama {
    pub fn new(base_url: &str) -> Self {
        Ollama { base_url: base_url.trim_end_matches('/').to_string(), timeout: Duration::from_secs(1200) }
    }

    fn get_json(&self, path: &str, timeout: Duration) -> Result<Value> {
        let r = http::request("GET", &format!("{}{path}", self.base_url), None, timeout)?;
        let status = r.status;
        let text = r.text()?;
        if status != 200 {
            return Err(Error::Http { status, body: crate::text::truncate_chars(&text, 300).to_string() });
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// Model names installed on the engine (`GET /api/tags`).
    pub fn tags(&self) -> Result<Vec<String>> {
        let v = self.get_json("/api/tags", Duration::from_secs(10))?;
        Ok(model_names(&v))
    }
}

fn model_names(v: &Value) -> Vec<String> {
    v["models"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

impl Engine for Ollama {
    fn chat(&self, req: &ChatRequest, on_delta: &mut dyn FnMut(&str)) -> Result<ChatResult> {
        let body = serde_json::to_vec(req)?;
        let t0 = Instant::now();
        let r = http::request("POST", &format!("{}/api/chat", self.base_url), Some(&body), self.timeout)?;
        if r.status != 200 {
            let status = r.status;
            let text = r.text().unwrap_or_default();
            let why = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            return Err(Error::Http { status, body: crate::text::truncate_chars(&why, 300).to_string() });
        }
        read_chat_stream(r.body, &req.model, t0, on_delta)
    }

    fn resident_models(&self) -> Option<Vec<String>> {
        let v = self.get_json("/api/ps", Duration::from_secs(3)).ok()?;
        Some(model_names(&v).iter().map(|n| base_name(n).to_string()).collect())
    }
}

/// Fold an NDJSON chat stream into one assistant message plus timings.
pub fn read_chat_stream(
    body: Box<dyn BufRead + Send>,
    model: &str,
    t0: Instant,
    on_delta: &mut dyn FnMut(&str),
) -> Result<ChatResult> {
    let (mut content, mut calls, mut first, mut last) = (String::new(), Vec::new(), None, ChatChunk::default());
    for line in body.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let chunk: ChatChunk = serde_json::from_str(&line)?;
        if let Some(e) = chunk.error {
            return Err(Error::Engine(e));
        }
        if let Some(m) = &chunk.message {
            if (!m.content.is_empty() || !m.tool_calls.is_empty()) && first.is_none() {
                first = Some(t0.elapsed().as_secs_f64());
            }
            if !m.content.is_empty() {
                on_delta(&m.content);
                content.push_str(&m.content);
            }
            calls.extend(m.tool_calls.iter().cloned());
        }
        let done = chunk.done;
        last = chunk;
        if done {
            break;
        }
    }
    let total = t0.elapsed().as_secs_f64();
    let secs = |ns: u64| ns as f64 / 1e9;
    Ok(ChatResult {
        message: Message { tool_calls: calls, ..Message::assistant(content) },
        timings: Timings {
            model: model.to_string(),
            first_s: first.unwrap_or(total),
            total_s: total,
            load_s: secs(last.load_duration),
            prompt_tok: last.prompt_eval_count,
            prompt_s: secs(last.prompt_eval_duration),
            out_tok: last.eval_count,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{text_reply, tool_reply, Framing, MockOllama, Reply};
    use serde_json::json;

    #[test]
    fn message_wire_format() {
        let m = Message::tool("list_dir", "a\nb");
        assert_eq!(serde_json::to_value(&m).unwrap(), json!({"role": "tool", "content": "a\nb", "tool_name": "list_dir"}));
        let m = Message::user("hi");
        assert_eq!(serde_json::to_value(&m).unwrap(), json!({"role": "user", "content": "hi"}));
        // What Ollama sends back for a tool call, including the newer id/index fields and null content.
        let m: Message = serde_json::from_value(json!({"role": "assistant", "content": null,
            "tool_calls": [{"id": "call_1", "function": {"index": 0, "name": "read_file", "arguments": {"path": "x"}}}]}))
        .unwrap();
        assert_eq!(m.content, "");
        assert_eq!(m.tool_calls[0].function.name, "read_file");
        assert_eq!(m.tool_calls[0].function.args()["path"], "x");
        let back = serde_json::to_value(&m).unwrap();
        assert_eq!(back["tool_calls"][0]["function"]["arguments"], json!({"path": "x"}));
        assert!(back.get("thinking").is_none());
    }

    #[test]
    fn arguments_tolerate_strings_and_null() {
        let mut c = ToolCall::new("t", json!("{\"a\": 1}"));
        assert_eq!(c.function.args()["a"], 1);
        c.function.arguments = Value::Null;
        assert!(c.function.args().is_empty());
        c.function.arguments = json!("not json");
        assert!(c.function.args().is_empty());
    }

    #[test]
    fn request_wire_format() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![Message::user("hi")],
            tools: vec![Tool::function("t", "d", json!({"type": "object", "properties": {}}))],
            stream: true,
            keep_alive: Some("60m".into()),
            options: Some(Options { num_ctx: Some(8192), temperature: Some(0.4) }),
            think: Some(false),
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}],
                   "tools": [{"type": "function", "function": {"name": "t", "description": "d",
                              "parameters": {"type": "object", "properties": {}}}}],
                   "stream": true, "keep_alive": "60m", "options": {"num_ctx": 8192, "temperature": 0.4}, "think": false})
        );
        let bare = ChatRequest { model: "m".into(), ..Default::default() };
        assert_eq!(serde_json::to_value(&bare).unwrap(), json!({"model": "m", "messages": [], "stream": false}));
    }

    #[test]
    fn streams_content_and_timings_from_mock_server() {
        let mock = MockOllama::start(vec![text_reply("Hello from the mock engine.")]);
        let engine = Ollama::new(&format!("{}/", mock.url));
        let mut pieces = Vec::new();
        let req = ChatRequest { model: "m".into(), messages: vec![Message::user("hi")], stream: true, ..Default::default() };
        let r = engine.chat(&req, &mut |p| pieces.push(p.to_string())).unwrap();
        assert_eq!(r.message.content, "Hello from the mock engine.");
        assert_eq!(r.message.role, "assistant");
        assert!(pieces.len() > 1, "content should arrive in several deltas");
        assert_eq!(pieces.concat(), r.message.content);
        assert_eq!((r.timings.prompt_tok, r.timings.out_tok), (Some(42), Some(7)));
        assert!((r.timings.load_s - 1.5).abs() < 1e-9);
        assert!(r.timings.summary().starts_with("m: first token"));
        assert_eq!(mock.chat_requests()[0]["messages"][0]["content"], "hi");
    }

    #[test]
    fn collects_tool_calls_and_non_streamed_bodies() {
        let mock = MockOllama::start(vec![
            tool_reply(&[("list_dir", json!({"path": "."})), ("read_file", json!({"path": "a"}))]),
            Reply::Chat {
                lines: vec![json!({"message": {"role": "assistant", "content": "whole"}, "done": true})],
                framing: Framing::Length,
            },
        ]);
        let engine = Ollama::new(&mock.url);
        let req = ChatRequest { model: "m".into(), ..Default::default() };
        let r = engine.chat(&req, &mut |_| {}).unwrap();
        let names: Vec<_> = r.message.tool_calls.iter().map(|c| c.function.name.as_str()).collect();
        assert_eq!(names, ["list_dir", "read_file"]);
        assert_eq!(engine.chat(&req, &mut |_| {}).unwrap().message.content, "whole");
    }

    #[test]
    fn maps_engine_errors() {
        let mock = MockOllama::start(vec![
            Reply::Status(500, r#"{"error":"model requires more system memory"}"#.into()),
            Reply::Chat { lines: vec![json!({"error": "unexpected EOF"})], framing: Framing::Chunked },
        ]);
        let engine = Ollama::new(&mock.url);
        let req = ChatRequest { model: "m".into(), ..Default::default() };
        match engine.chat(&req, &mut |_| {}) {
            Err(Error::Http { status: 500, body }) => assert_eq!(body, "model requires more system memory"),
            other => panic!("expected HTTP 500, got {other:?}"),
        }
        assert!(matches!(engine.chat(&req, &mut |_| {}), Err(Error::Engine(e)) if e == "unexpected EOF"));
    }

    #[test]
    fn lists_resident_and_installed_models() {
        let mock = MockOllama::start(vec![]);
        mock.set_resident(&["qwen-big", "embed"]);
        let engine = Ollama::new(&mock.url);
        assert_eq!(engine.resident_models().unwrap(), ["qwen-big", "embed"]);
        assert_eq!(engine.tags().unwrap(), ["qwen-big:latest", "embed:latest"]);
        assert_eq!(base_name("qwen3:4b"), "qwen3");
        // Nothing listening: unknown, not a panic.
        assert!(Ollama::new("http://127.0.0.1:9").resident_models().is_none());
    }
}
