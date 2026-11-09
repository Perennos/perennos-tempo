use super::runtime::Reply;
use super::*;
use crate::conductor::budget::{Halt, Limit};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const VALID: &str = r#"{"model":"model","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;
const MISMATCH: &str = r#"{"model":"other","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;
const LENGTH: &str = r#"{"model":"model","choices":[{"message":{"content":"partial"},"finish_reason":"length"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;
const EMPTY: &str = r#"{"model":"model","choices":[{"message":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;
const OVER_BUDGET: &str = r#"{"model":"model","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2}}"#;
const MISSING_USAGE: &str = r#"{"model":"model","choices":[{"message":{"content":"fallback answer content"},"finish_reason":"stop"}]}"#;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

fn accept_bounded(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("fixture accept failed or timed out: {e}"),
        }
    }
}

fn read_full_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).expect("fixture request read");
        assert_ne!(n, 0, "client closed before full request");
        request.extend_from_slice(&chunk[..n]);
        assert!(
            request.len() <= MAX_REQUEST_BYTES,
            "fixture request exceeded 64 KiB"
        );
        let Some(end) = request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
        else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]);
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .expect("request Content-Length");
        assert!(length <= MAX_REQUEST_BYTES, "request body exceeded 64 KiB");
        if request.len() >= end + length {
            return;
        }
    }
}

fn fake_http(status: u16, body: &'static str) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback fixture bind");
    listener
        .set_nonblocking(true)
        .expect("nonblocking fixture listener");
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let mut stream = accept_bounded(&listener);
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        read_full_request(&mut stream);
        let reason = if status == 200 {
            "OK"
        } else {
            "Internal Server Error"
        };
        write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    });
    (port, server)
}

fn meter(tokens: u64) -> Meter {
    Meter::root(Budget {
        tokens,
        wall_time: Duration::from_secs(10),
        ..Budget::unlimited()
    })
}

fn call_with_messages(
    status: u16,
    body: &'static str,
    tokens: u64,
    system: &str,
    prompt: &str,
) -> (Reply, Meter) {
    let (port, server) = fake_http(status, body);
    let meter = meter(tokens);
    let reply = infer(port, "model", system, prompt, 16, false, &meter);
    server.join().expect("fixture server");
    (reply, meter)
}

fn call(status: u16, body: &'static str, tokens: u64) -> (Reply, Meter) {
    call_with_messages(status, body, tokens, "system", "prompt")
}

#[test]
fn http_500_preserves_raw_body() {
    let raw = "upstream failure body";
    let (reply, _) = call(500, raw, 100);
    assert_eq!(reply.body, raw);
    assert!(reply.error.is_some());
}

#[test]
fn invalid_json_preserves_raw_body() {
    let raw = "{not valid json";
    let (reply, _) = call(200, raw, 100);
    assert_eq!(reply.body, raw);
    assert!(reply.error.is_some());
}

#[test]
fn returned_model_mismatch_is_rejected() {
    let (reply, _) = call(200, MISMATCH, 100);
    assert_eq!(reply.response_model.as_deref(), Some("other"));
    assert!(reply
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("model identity"));
}

#[test]
fn length_finish_reason_is_rejected() {
    let (reply, _) = call(200, LENGTH, 100);
    assert_eq!(reply.finish_reason, "length");
    assert!(reply.error.is_some());
}

#[test]
fn empty_content_is_rejected() {
    let (reply, _) = call(200, EMPTY, 100);
    assert!(reply.content.is_empty());
    assert!(reply.error.is_some());
}

#[test]
fn valid_reply_uses_provider_token_counts() {
    let (reply, _) = call(200, VALID, 100);
    assert_eq!(reply.requested_model, "model");
    assert_eq!(reply.content, "ok");
    assert_eq!(reply.response_model.as_deref(), Some("model"));
    assert_eq!(reply.finish_reason, "stop");
    assert_eq!((reply.usage.prompt, reply.usage.completion), (2, 1));
    assert_eq!(reply.usage.source, "actual");
    assert!(reply.error.is_none());
}

#[test]
fn missing_provider_usage_estimates_system_user_and_answer() {
    let system = "S".repeat(4096);
    let prompt = "user request";
    let answer = "fallback answer content";
    let (reply, _) = call_with_messages(200, MISSING_USAGE, 2000, &system, prompt);
    let estimated_all_bytes = system.len() + prompt.len() + answer.len();

    assert_eq!(reply.content, answer);
    assert_eq!(reply.usage.source, "estimated_chars_over_4");
    assert!(reply.usage.prompt > 0);
    assert!(reply.usage.completion > 0);
    assert!(reply.usage.prompt + reply.usage.completion >= estimated_all_bytes.div_ceil(4) as u64);
    assert!(reply.error.is_none());
}

#[test]
fn parser_error_estimate_includes_system_and_user() {
    let system = "S".repeat(4096);
    let prompt = "user request";
    let (reply, _) = call_with_messages(200, "{not valid json", 2000, &system, prompt);
    let estimated_prompt_bytes = system.len() + prompt.len();

    assert!(reply.error.is_some());
    assert_eq!(reply.usage.source, "estimated_chars_over_4");
    assert!(reply.usage.prompt >= estimated_prompt_bytes.div_ceil(4) as u64);
}

#[test]
fn over_budget_reply_preserves_payload_usage_and_halt() {
    let (reply, meter) = call(200, OVER_BUDGET, 2);
    assert_eq!(reply.body, OVER_BUDGET);
    assert_eq!(reply.content, "ok");
    assert_eq!((reply.usage.prompt, reply.usage.completion), (2, 2));
    assert_eq!(reply.usage.source, "actual");
    assert!(reply
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("token budget"));
    assert_eq!(meter.halt(), Some(Halt::Hit(Limit::Tokens)));
}

#[test]
fn exhausted_meter_refuses_before_loopback_io() {
    let meter = meter(1);
    meter.charge_tokens(1);
    let reply = infer(9, "model", "system", "prompt", 16, false, &meter);
    assert_eq!(reply.error.as_deref(), Some("request budget is exhausted"));
    assert!(reply.body.is_empty());
}

#[test]
fn overflowing_provider_usage_is_rejected_without_counter_wrap() {
    let raw = r#"{"model":"model","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":18446744073709551615,"completion_tokens":0}}"#;
    let (port, server) = fake_http(200, raw);
    let meter = meter(100);
    meter.charge_tokens(2);
    let reply = infer(port, "model", "system", "prompt", 16, false, &meter);
    server.join().expect("fixture server");

    assert!(reply.error.is_some());
    assert_eq!(reply.body, raw);
    assert_eq!(reply.content, "ok");
    assert_eq!((reply.usage.prompt, reply.usage.completion), (u64::MAX, 0));
    assert_eq!(reply.usage.source, "actual");
    assert_eq!(meter.used().tokens, 100);
    assert_eq!(meter.halt(), Some(Halt::Hit(Limit::Tokens)));
}
