use super::{MAX_MODEL_REPLY, MAX_REQUEST_TOKENS, MAX_TOKENS};
use crate::conductor::budget::Meter;
use crate::conductor::net;
use crate::setup::json::{parse, J};
use std::time::Duration;
#[derive(Clone, Copy)]
pub(super) struct Usage {
    pub prompt: u64,
    pub completion: u64,
    pub source: &'static str,
}

impl Usage {
    fn estimated(system: &str, prompt: &str, answer: &str) -> Self {
        Self {
            prompt: estimate_tokens(system).saturating_add(estimate_tokens(prompt)),
            completion: estimate_tokens(answer),
            source: "estimated_chars_over_4",
        }
    }
}

pub(super) struct Reply {
    pub requested_model: String,
    pub content: String,
    pub body: String,
    pub usage: Usage,
    pub finish_reason: String,
    pub response_model: Option<String>,
    pub error: Option<String>,
}

fn reply_error(model: &str, system: &str, prompt: &str, body: String, error: String) -> Reply {
    Reply {
        requested_model: model.to_string(),
        content: String::new(),
        usage: Usage::estimated(system, prompt, &body),
        body,
        finish_reason: String::new(),
        response_model: None,
        error: Some(error),
    }
}

pub(super) fn infer(
    port: u16,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u64,
    json_mode: bool,
    meter: &Meter,
) -> Reply {
    infer_inner(
        port, model, system, prompt, max_tokens, json_mode, meter, true,
    )
}

/// Sections charges its own established character estimate after `say_run` returns. This wrapper
/// shares the same request, validation, timeout and halt logic while leaving the normal usage
/// charge to that existing single root meter.
pub(super) fn infer_for_sections(
    port: u16,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u64,
    json_mode: bool,
    meter: &Meter,
) -> Reply {
    infer_inner(
        port, model, system, prompt, max_tokens, json_mode, meter, false,
    )
}

#[allow(clippy::too_many_arguments)]
fn infer_inner(
    port: u16,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u64,
    json_mode: bool,
    meter: &Meter,
    charge_usage: bool,
) -> Reply {
    if meter.halt().is_some() {
        return reply_error(
            model,
            system,
            prompt,
            String::new(),
            "request budget is exhausted".into(),
        );
    }
    if max_tokens == 0 || max_tokens > MAX_REQUEST_TOKENS {
        return reply_error(
            model,
            system,
            prompt,
            String::new(),
            "request output cap is outside the owner ceiling".into(),
        );
    }
    let mut request = vec![
        ("model".into(), J::s(model)),
        (
            "messages".into(),
            J::Arr(vec![
                J::Obj(vec![
                    ("role".into(), J::s("system")),
                    ("content".into(), J::s(system)),
                ]),
                J::Obj(vec![
                    ("role".into(), J::s("user")),
                    ("content".into(), J::s(prompt)),
                ]),
            ]),
        ),
        ("temperature".into(), J::n(0.0)),
        ("max_tokens".into(), J::n(max_tokens as f64)),
        ("stream".into(), J::Bool(false)),
    ];
    if json_mode {
        request.push((
            "response_format".into(),
            J::Obj(vec![("type".into(), J::s("json_object"))]),
        ));
    }
    let body = J::Obj(request).compact();
    let budget = meter.budget();
    let remaining = budget
        .wall_time
        .saturating_sub(Duration::from_secs(meter.used().secs));
    if remaining.is_zero() {
        return reply_error(
            model,
            system,
            prompt,
            String::new(),
            "request time budget is exhausted".into(),
        );
    }
    let timeout = remaining.min(Duration::from_secs(90));
    let (status, response) = match net::request(
        port,
        "POST",
        "/v1/chat/completions",
        &[],
        Some(&body),
        timeout,
    ) {
        Ok(result) => result,
        Err(e) => return reply_error(model, system, prompt, String::new(), e),
    };
    if status / 100 != 2 {
        return reply_error(
            model,
            system,
            prompt,
            response,
            format!("local model returned HTTP {status}"),
        );
    }
    if response.len() > MAX_MODEL_REPLY {
        return reply_error(
            model,
            system,
            prompt,
            response,
            "local model response exceeded the foundry limit".into(),
        );
    }
    let json = match parse(&response) {
        Ok(value) => value,
        Err(e) => {
            return reply_error(
                model,
                system,
                prompt,
                response,
                format!("local model returned invalid response JSON: {e}"),
            )
        }
    };
    let response_model = json.str("model").map(str::to_string);
    let choices = match json.get("choices") {
        Some(J::Arr(a)) => a,
        _ => {
            return reply_error(
                model,
                system,
                prompt,
                response,
                "local model response had no choices".into(),
            )
        }
    };
    let Some(first) = choices.first() else {
        return reply_error(
            model,
            system,
            prompt,
            response,
            "local model returned no choices".into(),
        );
    };
    let Some(message) = first.get("message") else {
        return reply_error(
            model,
            system,
            prompt,
            response,
            "local model response had no message".into(),
        );
    };
    let content = message.str("content").unwrap_or_default().to_string();
    let finish_reason = first.str("finish_reason").unwrap_or("").to_string();
    let usage_obj = json.get("usage");
    let actual = usage_obj.and_then(|u| {
        Some((
            usage_integer(u.get("prompt_tokens")?)?,
            usage_integer(u.get("completion_tokens")?)?,
        ))
    });
    let usage = actual.map_or_else(
        || Usage::estimated(system, prompt, &content),
        |(p, c)| Usage {
            prompt: p,
            completion: c,
            source: "actual",
        },
    );
    let mut error = if response_model.is_none() {
        Some("local response omitted model identity".into())
    } else if response_model.as_deref() != Some(model) {
        Some("local response model identity did not match the requested model".into())
    } else if finish_reason == "length" {
        Some("local model response was truncated at the output token limit".into())
    } else if finish_reason != "stop" {
        Some("local response had an unknown finish_reason".into())
    } else if content.trim().is_empty() {
        Some("local model returned empty content".into())
    } else {
        None
    };
    match usage.prompt.checked_add(usage.completion) {
        Some(n)
            if n <= MAX_TOKENS
                && budget.tokens > 0
                && n <= budget.tokens.saturating_sub(meter.used().tokens) =>
        {
            if charge_usage {
                meter.charge_tokens(n)
            }
        }
        _ => {
            let remaining = budget.tokens.saturating_sub(meter.used().tokens);
            meter.charge_tokens(remaining);
            error = Some("local model exceeded the request token budget".into());
        }
    }
    Reply {
        requested_model: model.to_string(),
        content,
        body: response,
        usage,
        finish_reason,
        response_model,
        error,
    }
}

fn usage_integer(value: &J) -> Option<u64> {
    match value {
        J::Num(n) => n.parse().ok(),
        _ => None,
    }
}

fn estimate_tokens(s: &str) -> u64 {
    (s.len() as u64).saturating_add(3) / 4
}
