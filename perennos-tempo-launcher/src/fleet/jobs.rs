//! The `job` topic: the Conductor hands a member's step to the machine whose engine plays it. The
//! other machine runs it through its own agent (a guest conversation with the member's model and
//! role, so no tools) and the answer comes back as a `result` message.

use super::msg::{Handler, Node};
use super::store::{self, Paths};
use crate::conductor::Said;
use crate::setup::json::{parse, J};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerOutput {
    pub text: String,
    pub tool_calls: usize,
    pub tool_digest: String,
}

pub trait Runner: Send + Sync {
    fn run(&self, model: &str, role: &str, prompt: &str) -> Result<RunnerOutput, String>;
}

/// This computer's own agent.
pub struct AgentRunner(pub Paths);

impl Runner for AgentRunner {
    fn run(&self, _model: &str, role: &str, prompt: &str) -> Result<RunnerOutput, String> {
        let ctx = self.0.ctx();
        crate::conductor::net::Agent::new(crate::AGENT.port, crate::agent_key(&ctx))
            .say_fleet_job(role, prompt)
            .map(|answer| RunnerOutput {
                text: answer.text,
                tool_calls: answer.tool_calls,
                tool_digest: answer.tool_digest,
            })
    }
}

use crate::conductor::Chat;

pub struct JobHandler(pub Arc<dyn Runner>);

impl Handler for JobHandler {
    fn on(&self, node: &Arc<Node>, from: &str, topic: &str, _id: &str, body: &[u8]) {
        if topic != "job" {
            return;
        }
        let Some(j) = std::str::from_utf8(body).ok().and_then(|t| parse(t).ok()) else {
            return;
        };
        let (node, runner, from) = (node.clone(), self.0.clone(), from.to_string());
        std::thread::spawn(move || {
            let _busy = super::route::Busy::new();
            let job = j.str("job").unwrap_or("").to_string();
            let r = runner.run(
                j.str("model").unwrap_or(""),
                j.str("role").unwrap_or(""),
                j.str("prompt").unwrap_or(""),
            );
            let (ok, text, tool_calls, tool_digest) = match r {
                Ok(out) => (true, out.text, out.tool_calls, out.tool_digest),
                Err(e) => (false, e, 0, String::new()),
            };
            let reply = J::Obj(vec![
                ("job".into(), J::s(&job)),
                ("ok".into(), J::Bool(ok)),
                ("text".into(), J::s(&text)),
                ("tool_calls".into(), J::Num(tool_calls.to_string())),
                ("tool_digest".into(), J::s(&tool_digest)),
            ]);
            if let Some(m) = store::load(&node.paths)
                .machines
                .into_iter()
                .find(|m| m.name == from)
            {
                let _ = node.send(&m, "result", reply.compact().as_bytes());
            }
        });
    }
}

/// Hand a step to `m` under the request id `job`.
pub fn submit_as(
    node: &Node,
    m: &store::Machine,
    job: &str,
    model: &str,
    role: &str,
    prompt: &str,
) -> Result<(), String> {
    if role.len() > 2000 || prompt.len() > 16 * 1024 {
        return Err("fleet job role or prompt exceeds its byte cap".into());
    }
    let body = J::Obj(vec![
        ("job".into(), J::s(job)),
        ("model".into(), J::s(model)),
        ("role".into(), J::s(role)),
        ("prompt".into(), J::s(prompt)),
    ]);
    node.send(m, "job", body.compact().as_bytes()).map(|_| ())
}

/// Hand a step to `m`; returns the job's id.
pub fn submit(
    node: &Node,
    m: &store::Machine,
    model: &str,
    role: &str,
    prompt: &str,
) -> Result<String, String> {
    let job = super::crypto::hex(&super::crypto::random(8));
    submit_as(node, m, &job, model, role, prompt)?;
    Ok(job)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobResult {
    pub text: String,
    pub tool_calls: usize,
    pub tool_digest: String,
}

fn receipt_from(j: &J) -> Result<(usize, String), String> {
    let calls = j.f64("tool_calls").ok_or("the fleet job returned no tool count")?;
    if !calls.is_finite() || calls < 0.0 || calls.fract() != 0.0 || calls > 4.0 {
        return Err("the fleet job returned an invalid tool count".into());
    }
    let digest = j
        .str("tool_digest")
        .filter(|digest| crate::conductor::net::valid_fleet_digest(digest))
        .ok_or("the fleet job returned no valid tool digest")?
        .to_string();
    Ok((calls as usize, digest))
}

pub fn wait_result(node: &Node, job: &str, limit: Duration) -> Result<JobResult, String> {
    let m = node
        .wait_for("result", "job", job, limit)
        .ok_or("the other computer did not answer in time")?;
    let j = parse(&String::from_utf8_lossy(&m.body)).map_err(|e| e.to_string())?;
    let text = j.str("text").unwrap_or("").to_string();
    if j.bool("ok") == Some(true) {
        let (tool_calls, tool_digest) = receipt_from(&j)?;
        Ok(JobResult {
            text,
            tool_calls,
            tool_digest,
        })
    } else {
        Err(text)
    }
}

/// Legacy text-only wait remains usable for jobs that do not promise bounded fleet-job receipts.
pub fn wait(node: &Node, job: &str, limit: Duration) -> Result<String, String> {
    let m = node
        .wait_for("result", "job", job, limit)
        .ok_or("the other computer did not answer in time")?;
    let j = parse(&String::from_utf8_lossy(&m.body)).map_err(|e| e.to_string())?;
    let text = j.str("text").unwrap_or("").to_string();
    if j.bool("ok") == Some(true) {
        Ok(text)
    } else {
        Err(text)
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn fleet_job_success_requires_bounded_digest_receipt() {
        let digest = "a".repeat(64);
        let good = parse(&format!("{{\"tool_calls\":2,\"tool_digest\":\"{digest}\"}}")).unwrap();
        assert_eq!(receipt_from(&good), Ok((2, digest)));

        let old_peer = parse("{\"tool_calls\":0}").unwrap();
        assert!(receipt_from(&old_peer).is_err());
        let excess = parse(&format!("{{\"tool_calls\":5,\"tool_digest\":\"{}\"}}", "b".repeat(64))).unwrap();
        assert!(receipt_from(&excess).is_err());
        let non_integer = parse(&format!("{{\"tool_calls\":2.5,\"tool_digest\":\"{}\"}}", "c".repeat(64))).unwrap();
        assert!(receipt_from(&non_integer).is_err());
    }
}

/// When `engine` is the Ollama of a fleet machine that is up and takes jobs, let that machine's
/// own agent play the step. If the wire fails (no answer, a dropped connection) the same request
/// goes once to the next best machine; a step that was answered, even with "no", is final. A
/// step only talks and drafts, so a second try is safe; tool calls are never retried.
/// `None`: do it the usual way.
pub fn try_remote(
    ctx: &crate::Ctx,
    engine: &str,
    model: &str,
    role: &str,
    text: &str,
) -> Option<Result<Said, String>> {
    let p = Paths::of(ctx);
    let want = engine.trim().trim_end_matches('/');
    let fleet = store::load(&p);
    let now = store::now_s();
    let ranked = super::route::rank(&fleet, model, now);
    let first = *ranked
        .iter()
        .find(|m| m.engine() && m.engine_url() == want)?;
    let mut order = vec![first];
    order.extend(ranked.into_iter().filter(|m| m.name != first.name));
    let node = Node::open(&p, store::read_key(&p)?).ok()?;
    let job = super::crypto::hex(&super::crypto::random(8));
    let mut last = String::new();
    for m in order.into_iter().take(2) {
        let r = submit_as(&node, m, &job, model, role, text)
            .and_then(|()| wait(&node, &job, Duration::from_secs(900)));
        match r {
            Ok(text) => {
                return Some(Ok(Said {
                    text,
                    engine: Some(format!("{} (fleet)", m.name)),
                }))
            }
            Err(e) if super::route::retryable(&e) => {
                // the first machine must not run it later, when it comes back
                node.cancel_queued(&m.name, "job", &job);
                last = e;
            }
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Err(last))
}
