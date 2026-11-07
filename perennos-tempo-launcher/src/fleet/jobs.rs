//! The `job` topic: the Conductor hands a member's step to the machine whose engine plays it. The
//! other machine runs it through its own agent (a guest conversation with the member's model and
//! role, so no tools) and the answer comes back as a `result` message.

use super::msg::{Handler, Node};
use super::store::{self, Paths};
use crate::conductor::Said;
use crate::setup::json::{parse, J};
use std::sync::Arc;
use std::time::Duration;

pub trait Runner: Send + Sync {
    fn run(&self, model: &str, role: &str, prompt: &str) -> Result<String, String>;
}

/// This computer's own agent.
pub struct AgentRunner(pub Paths);

impl Runner for AgentRunner {
    fn run(&self, model: &str, role: &str, prompt: &str) -> Result<String, String> {
        let ctx = self.0.ctx();
        crate::conductor::net::Agent::new(crate::AGENT.port, crate::agent_key(&ctx))
            .say(model, role, prompt)
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
            let job = j.str("job").unwrap_or("").to_string();
            let r = runner.run(
                j.str("model").unwrap_or(""),
                j.str("role").unwrap_or(""),
                j.str("prompt").unwrap_or(""),
            );
            let (ok, text) = match r {
                Ok(t) => (true, t),
                Err(e) => (false, e),
            };
            let reply = J::Obj(vec![
                ("job".into(), J::s(&job)),
                ("ok".into(), J::Bool(ok)),
                ("text".into(), J::s(&text)),
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

/// Hand a step to `m`; returns the job's id.
pub fn submit(
    node: &Node,
    m: &store::Machine,
    model: &str,
    role: &str,
    prompt: &str,
) -> Result<String, String> {
    let job = super::crypto::hex(&super::crypto::random(8));
    let body = J::Obj(vec![
        ("job".into(), J::s(&job)),
        ("model".into(), J::s(model)),
        ("role".into(), J::s(role)),
        ("prompt".into(), J::s(prompt)),
    ]);
    node.send(m, "job", body.compact().as_bytes())?;
    Ok(job)
}

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

/// When `engine` is the Ollama of a fleet machine that is up and takes jobs, let that machine's
/// own agent play the step. `None`: do it the usual way.
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
    let m = fleet
        .peers()
        .find(|m| m.engine() && m.engine_url() == want)?;
    let fresh = store::now_s().saturating_sub(m.last_seen) < 30;
    if !fresh || m.health.bool("jobs") != Some(true) {
        return None;
    }
    let node = Node::open(&p, store::read_key(&p)?).ok()?;
    let r = submit(&node, m, model, role, text)
        .and_then(|job| wait(&node, &job, Duration::from_secs(900)));
    Some(r.map(|text| Said {
        text,
        engine: Some(format!("{} (fleet)", m.name)),
    }))
}
