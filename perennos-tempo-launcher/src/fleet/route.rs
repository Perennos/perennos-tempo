//! Which computer plays a request. Whole requests go to one machine; a model is never split
//! across machines (that decodes about twice as slowly, even over a fast cable). Candidates are
//! ordered by: the model already loaded there, then free graphics memory (or memory) to spare,
//! then the fewest requests already running (an Ollama instance does about one generation at a
//! time), then the measured speed. A machine that has missed three heartbeats is not a candidate.
//! A request that failed on the wire may go to the next machine once; nothing that writes does.

use super::serve::AWAY_S;
use super::store::{Fleet, Machine};
use crate::setup::json::J;
use std::cmp::Ordering;
use std::sync::atomic::{AtomicUsize, Ordering as O};

/// Requests running on this computer right now (jobs and tool calls), for the heartbeat.
pub static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Counts one running request until it is dropped.
pub struct Busy;

impl Busy {
    pub fn new() -> Busy {
        IN_FLIGHT.fetch_add(1, O::SeqCst);
        Busy
    }
}

impl Default for Busy {
    fn default() -> Self {
        Busy::new()
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, O::SeqCst);
    }
}

fn up(m: &Machine, now: u64) -> bool {
    m.enabled && m.last_seen > 0 && now.saturating_sub(m.last_seen) < AWAY_S
}

fn key(m: &Machine, model: &str) -> (u8, f64, f64, f64) {
    let h = &m.health;
    let loaded = h.strs("models").iter().any(|x| x == model);
    let room = h
        .f64("vram_free_gb")
        .or(h.f64("mem_free_gb"))
        .unwrap_or(0.0);
    let busy = h.f64("in_flight").unwrap_or(0.0) + h.f64("queue").unwrap_or(0.0);
    (u8::from(!loaded), -room, busy, -h.f64("tps").unwrap_or(0.0))
}

/// The machines that are up and take jobs, best first for `model`.
pub fn rank<'a>(f: &'a Fleet, model: &str, now: u64) -> Vec<&'a Machine> {
    let mut v: Vec<&Machine> = f
        .peers()
        .filter(|m| up(m, now) && m.health.bool("jobs") == Some(true))
        .collect();
    v.sort_by(|a, b| {
        let (x, y) = (key(a, model), key(b, model));
        x.0.cmp(&y.0)
            .then(x.1.partial_cmp(&y.1).unwrap_or(Ordering::Equal))
            .then(x.2.partial_cmp(&y.2).unwrap_or(Ordering::Equal))
            .then(x.3.partial_cmp(&y.3).unwrap_or(Ordering::Equal))
            .then(a.name.cmp(&b.name))
    });
    v
}

/// A failure of the wire (no answer, a dropped connection), which another machine may do better
/// on. A refusal, or an answer that says the work failed, is not.
pub fn retryable(err: &str) -> bool {
    ["did not answer in time", "is not answering", "connection"]
        .iter()
        .any(|w| err.contains(w))
        && !err.starts_with("refused:")
}

/// Time one short fixed prompt on this computer's Ollama: tokens a second.
pub fn measure_tps(port: u16, model: &str) -> Option<f64> {
    let body = J::Obj(vec![
        ("model".into(), J::s(model)),
        ("prompt".into(), J::s("Count from one to twenty in words.")),
        ("stream".into(), J::Bool(false)),
        (
            "options".into(),
            J::Obj(vec![
                ("num_predict".into(), J::n(48.0)),
                ("temperature".into(), J::n(0.0)),
            ]),
        ),
    ])
    .compact();
    let (c, text) = crate::conductor::net::request(
        port,
        "POST",
        "/api/generate",
        &[],
        Some(&body),
        std::time::Duration::from_secs(60),
    )
    .ok()?;
    let j = crate::setup::json::parse(&text).ok().filter(|_| c == 200)?;
    let (n, ns) = (j.f64("eval_count")?, j.f64("eval_duration")?);
    (ns > 0.0 && n > 0.0).then(|| (n / (ns / 1e9) * 10.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::store::now_s;

    fn node(name: &str, models: &[&str], vram: f64, busy: f64, tps: f64) -> Machine {
        let mut m = Machine::new(name, "10.0.0.2", "engine");
        m.last_seen = now_s();
        m.health = crate::setup::json::parse(&format!(
            "{{\"jobs\":true,\"models\":{},\"vram_free_gb\":{vram},\"in_flight\":{busy},\"queue\":0,\"tps\":{tps}}}",
            J::strings(&models.iter().map(|s| s.to_string()).collect::<Vec<_>>()).compact()
        ))
        .unwrap();
        m
    }

    #[test]
    fn the_model_already_loaded_wins_then_room_then_the_shortest_line_then_speed() {
        let f = Fleet {
            machines: vec![
                node("big-empty", &[], 24.0, 0.0, 90.0),
                node("small-loaded", &["qwen3:8b"], 6.0, 2.0, 40.0),
                node("big-loaded-busy", &["qwen3:8b"], 20.0, 3.0, 80.0),
                node("big-loaded-idle", &["qwen3:8b"], 20.0, 0.0, 70.0),
                node("twin-slow", &["qwen3:8b"], 20.0, 0.0, 50.0),
            ],
            ..Fleet::default()
        };
        let order: Vec<&str> = rank(&f, "qwen3:8b", now_s())
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(
            order,
            [
                "big-loaded-idle",
                "twin-slow",
                "big-loaded-busy",
                "small-loaded",
                "big-empty"
            ]
        );
        // for a model nobody has loaded, the roomiest and quickest comes first
        let order: Vec<&str> = rank(&f, "other", now_s())
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(order[0], "big-empty");
    }

    #[test]
    fn a_machine_that_missed_three_heartbeats_or_takes_no_jobs_is_not_a_candidate() {
        let mut gone = node("gone", &["m"], 10.0, 0.0, 1.0);
        gone.last_seen = now_s() - 40;
        let mut off = node("off", &["m"], 10.0, 0.0, 1.0);
        off.enabled = false;
        let mut nojobs = node("nojobs", &["m"], 10.0, 0.0, 1.0);
        nojobs.health = J::Null;
        let f = Fleet {
            machines: vec![gone, off, nojobs, node("here", &[], 1.0, 0.0, 1.0)],
            ..Fleet::default()
        };
        let order: Vec<&str> = rank(&f, "m", now_s())
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(order, ["here"]);
        assert_eq!(AWAY_S, 35, "three beats of ten seconds, and a little");
    }

    #[test]
    fn only_wire_failures_are_retried() {
        assert!(retryable("beta did not answer in time"));
        assert!(retryable("beta is not answering (connection refused)"));
        assert!(retryable("the connection closed"));
        assert!(!retryable("refused: this computer doesn't offer x"));
        assert!(!retryable("the model said no"));
    }

    #[test]
    fn the_busy_count_goes_up_and_down() {
        let before = IN_FLIGHT.load(O::SeqCst);
        {
            let _a = Busy::new();
            assert!(IN_FLIGHT.load(O::SeqCst) > before);
        }
        assert_eq!(IN_FLIGHT.load(O::SeqCst), before);
    }
}
