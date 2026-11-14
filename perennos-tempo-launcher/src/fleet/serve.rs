//! `tempo-launch fleet serve`: the messenger's listener, its senders and its heartbeat.
//!
//! It listens only on this computer's private and tailnet addresses and on 127.0.0.1, never on
//! 0.0.0.0. The one wildcard socket in the program, mDNS on UDP 5353, is off unless `--announce`. One port carries three things, told apart by the first bytes: signed frames, a plain
//! HTTP (`/hello` to any private address; `/fleet.json` and `POST /call` to this computer only) and
//! pairing, which starts with `PAIR`.

use super::msg::{lock, Node};
use super::store::{self, now_s, Fleet, Machine, Paths};
use crate::setup::json::{parse, J};
use perennos_dead_air_silence_core::{transition, Transition};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HEARTBEAT: Duration = Duration::from_secs(10);
/// A machine that has not spoken for this long (three heartbeats missed) is shown as away.
pub const AWAY_S: u64 = 35;
pub const SILENCE_ALARM_MS: u64 = 10 * 60 * 1000;
pub(super) const SILENCE_LOG: &str = "fleet-silence.log";
const MESSENGER_HEARTBEAT: &str = "fleet-messenger.json";
const ALARM_MISSED_INTERVALS: u64 = 60;

// ---- what this computer says about itself

fn load_per_cpu() -> Option<f64> {
    let t = std::fs::read_to_string("/proc/loadavg").ok().or_else(|| {
        crate::setup::probe::run_with_timeout(
            "sysctl",
            &["-n", "vm.loadavg"],
            Duration::from_secs(1),
        )
        .map(|s| s.replace(['{', '}'], ""))
    })?;
    let one: f64 = t.split_whitespace().next()?.parse().ok()?;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    Some((one / cpus * 100.0).round() / 100.0)
}

fn loaded_models() -> Vec<String> {
    let port = crate::setup::apply::ollama_port();
    crate::http_get_at(
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        &format!("127.0.0.1:{port}"),
        "/api/ps",
        Duration::from_millis(600),
    )
    .filter(|(c, _)| *c == 200)
    .and_then(|(_, b)| parse(&b).ok())
    .map(|j| {
        j.arr("models")
            .iter()
            .filter_map(|m| m.str("name").map(String::from))
            .collect()
    })
    .unwrap_or_default()
}

/// Free graphics memory on an NVIDIA card, if there is one.
fn vram_free() -> Option<f64> {
    match crate::setup::probe::probe_gpu(&crate::setup::probe::RealHost) {
        crate::setup::probe::Gpu::Nvidia { free_gb, .. } => Some((free_gb * 10.0).round() / 10.0),
        _ => None,
    }
}

/// The last measured speed of the model loaded now: one fixed short prompt, timed once, kept
/// until the model (or Ollama's version) changes. Not measured while this computer is busy.
static TPS: Mutex<Option<(String, f64)>> = Mutex::new(None);

fn tps_now() -> Option<f64> {
    let model = loaded_models().into_iter().next()?;
    let port = crate::setup::apply::ollama_port();
    let version = crate::http_get_at(
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        &format!("127.0.0.1:{port}"),
        "/api/version",
        Duration::from_millis(600),
    )
    .and_then(|(_, b)| crate::json_str(&b, "version"))
    .unwrap_or_default();
    let key = format!("{model}@{version}");
    if let Some((k, v)) = lock(&TPS).as_ref() {
        if *k == key {
            return Some(*v);
        }
    }
    let idle = super::route::IN_FLIGHT.load(Ordering::SeqCst) == 0
        && load_per_cpu().is_none_or(|l| l < 0.5);
    if !idle {
        return lock(&TPS).as_ref().map(|x| x.1);
    }
    let v = super::route::measure_tps(port, &model)?;
    *lock(&TPS) = Some((key, v));
    Some(v)
}

fn agent_up() -> bool {
    let a = SocketAddr::from((Ipv4Addr::LOCALHOST, crate::AGENT.port));
    TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok()
}

/// The heartbeat body: load, memory, the models loaded, the queue, and what this computer offers.
pub fn health(node: &Node, with_tools: bool) -> J {
    let tools = with_tools
        .then(|| super::tools::advert(&node.paths, false))
        .flatten();
    health_with(node.queued(None), tools)
}

pub fn health_with(queued: usize, tools: Option<J>) -> J {
    let (total, free) = crate::setup::probe::probe_memory(&crate::setup::probe::RealHost);
    let r1 = |v: Option<f64>| v.map_or(J::Null, |x| J::n((x * 10.0).round() / 10.0));
    let mut o = vec![
        ("load".to_string(), load_per_cpu().map_or(J::Null, J::n)),
        (
            "cpus".into(),
            J::n(std::thread::available_parallelism().map_or(1, |n| n.get()) as f64),
        ),
        ("mem_free_gb".into(), r1(free)),
        ("mem_total_gb".into(), r1(total)),
        ("models".into(), J::strings(&loaded_models())),
        ("queue".into(), J::n(queued as f64)),
        ("jobs".into(), J::Bool(agent_up())),
        (
            "in_flight".into(),
            J::n(super::route::IN_FLIGHT.load(Ordering::SeqCst) as f64),
        ),
        ("vram_free_gb".into(), vram_free().map_or(J::Null, J::n)),
        ("tps".into(), tps_now().map_or(J::Null, J::n)),
        ("ver".into(), J::s(crate::VERSION)),
        ("at".into(), J::n(now_s() as f64)),
    ];
    if let Some(t) = tools {
        o.push(("tools".into(), t));
    }
    J::Obj(o)
}

/// A heartbeat arrived: keep what it said in fleet.json.
pub fn on_heartbeat(node: &Node, from: &str, body: &[u8]) {
    let Some(j) = std::str::from_utf8(body).ok().and_then(|t| parse(t).ok()) else {
        return;
    };
    let _ = store::update(&node.paths, |f| {
        if let Some(m) = f.machines.iter_mut().find(|m| m.name == from) {
            m.last_seen = now_s();
            // the offer rides on every sixth heartbeat: between those the last one stands
            if let Some(t) = j.get("tools") {
                m.tools = t.clone();
            } else if j.get("tools_none") == Some(&J::Bool(true)) {
                m.tools = J::Null;
            }
            let mut h = j;
            if let J::Obj(o) = &mut h {
                o.retain(|(k, _)| k != "tools" && k != "tools_none");
            }
            m.health = h;
        }
    });
}

fn machine_signal_ms(machine: &Machine) -> Option<u64> {
    let seconds = if machine.last_seen > 0 {
        machine.last_seen
    } else {
        machine.joined_at
    };
    (seconds > 0).then(|| seconds.saturating_mul(1000))
}

fn clock_label(seconds: u64) -> String {
    let minute_of_day = (seconds % 86_400) / 60;
    format!("{:02}:{:02} UTC", minute_of_day / 60, minute_of_day % 60)
}

fn display_name(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SilenceLevel {
    Healthy,
    Alert,
    Unknown,
}

#[derive(Clone, Debug)]
struct SilenceObservation {
    level: SilenceLevel,
    message: Option<String>,
}

fn peer_observation(machine: &Machine, now_ms: u64) -> SilenceObservation {
    let signal_ms = machine_signal_ms(machine);
    match transition(now_ms, signal_ms, SILENCE_ALARM_MS, false) {
        Transition::Healthy | Transition::Recovered => SilenceObservation {
            level: SilenceLevel::Healthy,
            message: None,
        },
        Transition::Unknown => SilenceObservation {
            level: SilenceLevel::Unknown,
            message: Some(format!(
                "I can't read a valid check-in time for {}. Its status is unknown.",
                display_name(&machine.name)
            )),
        },
        Transition::Alert | Transition::Quiet => {
            let Some(signal_ms) = signal_ms else {
                return SilenceObservation {
                    level: SilenceLevel::Unknown,
                    message: Some(format!(
                        "I can't read a valid check-in time for {}. Its status is unknown.",
                        display_name(&machine.name)
                    )),
                };
            };
            let age_minutes = now_ms.saturating_sub(signal_ms) / 60_000;
            let last_seen = if machine.last_seen > 0 {
                format!("Last seen {}.", clock_label(machine.last_seen))
            } else {
                "Last seen: never.".to_string()
            };
            SilenceObservation {
                level: SilenceLevel::Alert,
                message: Some(format!(
                    "{} hasn't checked in for {age_minutes} minutes. {last_seen} I'm retrying; nothing for you to do yet.",
                    display_name(&machine.name)
                )),
            }
        }
    }
}

/// Current plain-language silence alerts for `tempo-launch status`.
pub fn silence_status(paths: &Paths) -> Vec<String> {
    let now_ms = store::now_ms();
    let (mut alerts, messenger_expected) = match store::load_checked(paths) {
        Ok(fleet) => {
            let mut alerts = Vec::new();
            let mut messenger_expected = false;
            for machine in fleet.machines.iter().filter(|machine| machine.enabled) {
                messenger_expected = true;
                if let Some(message) = peer_observation(machine, now_ms).message {
                    alerts.push(message);
                }
            }
            (alerts, messenger_expected)
        }
        Err(_) => (
            vec!["The fleet setup can't be read, so this computer's status is unknown.".into()],
            false,
        ),
    };
    if let Some(message) = messenger_observation_at(paths, now_ms, messenger_expected)
        .0
        .message
    {
        alerts.push(message);
    }
    if let Some(message) = super::relay::pending_message(paths) {
        alerts.push(message);
    }
    alerts
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MessengerHealth {
    pid: u32,
    started_at: u64,
    last_loop: u64,
    loop_interval_s: u64,
    peer_count: u64,
    last_delivery_attempt: Option<u64>,
    last_delivery_ok: Option<u64>,
    last_delivery_error: Option<u64>,
}

impl MessengerHealth {
    fn new(at: u64) -> Self {
        Self {
            pid: std::process::id(),
            started_at: at,
            last_loop: at,
            loop_interval_s: HEARTBEAT.as_secs(),
            peer_count: 0,
            last_delivery_attempt: None,
            last_delivery_ok: None,
            last_delivery_error: None,
        }
    }
}

#[derive(Default)]
pub(super) struct SilenceMonitor {
    peers: HashMap<String, SilenceLevel>,
    messenger: Option<SilenceLevel>,
    missing_messenger_since_ms: Option<u64>,
    fleet_unknown: bool,
}

fn messenger_json(health: &MessengerHealth) -> Vec<u8> {
    let number = |value: Option<u64>| value.map_or(J::Null, |n| J::n(n as f64));
    J::Obj(vec![
        ("pid".into(), J::n(health.pid as f64)),
        ("started_at".into(), J::n(health.started_at as f64)),
        ("last_loop".into(), J::n(health.last_loop as f64)),
        (
            "loop_interval_s".into(),
            J::n(health.loop_interval_s as f64),
        ),
        ("peer_count".into(), J::n(health.peer_count as f64)),
        (
            "last_delivery_attempt".into(),
            number(health.last_delivery_attempt),
        ),
        ("last_delivery_ok".into(), number(health.last_delivery_ok)),
        (
            "last_delivery_error".into(),
            number(health.last_delivery_error),
        ),
    ])
    .pretty()
    .into_bytes()
}

fn json_u64(json: &J, key: &str) -> std::io::Result<u64> {
    json.f64(key)
        .filter(|value| {
            value.is_finite()
                && (0.0..=9_007_199_254_740_991.0).contains(value)
                && value.fract() == 0.0
        })
        .map(|value| value as u64)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "heartbeat number is invalid",
            )
        })
}

fn json_optional_u64(json: &J, key: &str) -> std::io::Result<Option<u64>> {
    match json.get(key) {
        Some(J::Null) => Ok(None),
        Some(_) => json_u64(json, key).map(Some),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "heartbeat field is missing",
        )),
    }
}

fn validate_messenger_json(bytes: &[u8]) -> std::io::Result<MessengerHealth> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let json = parse(text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let J::Obj(fields) = &json else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "heartbeat must be an object",
        ));
    };
    const KEYS: [&str; 8] = [
        "pid",
        "started_at",
        "last_loop",
        "loop_interval_s",
        "peer_count",
        "last_delivery_attempt",
        "last_delivery_ok",
        "last_delivery_error",
    ];
    if fields.len() != KEYS.len() || fields.iter().any(|(key, _)| !KEYS.contains(&key.as_str())) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "heartbeat has an unfamiliar schema",
        ));
    }
    let health = MessengerHealth {
        pid: u32::try_from(json_u64(&json, "pid")?).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "heartbeat process id is invalid",
            )
        })?,
        started_at: json_u64(&json, "started_at")?,
        last_loop: json_u64(&json, "last_loop")?,
        loop_interval_s: json_u64(&json, "loop_interval_s")?,
        peer_count: json_u64(&json, "peer_count")?,
        last_delivery_attempt: json_optional_u64(&json, "last_delivery_attempt")?,
        last_delivery_ok: json_optional_u64(&json, "last_delivery_ok")?,
        last_delivery_error: json_optional_u64(&json, "last_delivery_error")?,
    };
    if health.pid == 0
        || health.started_at == 0
        || health.last_loop < health.started_at
        || !(1..=300).contains(&health.loop_interval_s)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "heartbeat values are invalid",
        ));
    }
    Ok(health)
}

fn read_messenger_bytes(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4096 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "messenger check-in is not a bounded regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "messenger check-in is a reparse point",
            ));
        }
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(4097)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "messenger check-in exceeded its read limit",
        ));
    }
    Ok(Some(bytes))
}

fn update_messenger_heartbeat(paths: &Paths, health: &MessengerHealth) -> std::io::Result<()> {
    let path = paths.file(MESSENGER_HEARTBEAT);
    let previous = match read_messenger_bytes(&path)? {
        Some(bytes) => {
            if validate_messenger_json(&bytes).is_err() && !is_legacy_messenger_json(&bytes) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "messenger check-in has an unfamiliar schema",
                ));
            }
            Some(bytes)
        }
        None => None,
    };
    crate::apply_json_atomic(
        &path,
        previous.as_deref(),
        &messenger_json(health),
        None,
        |bytes| validate_messenger_json(bytes).map(|_| ()),
    )
}

fn is_legacy_messenger_json(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Ok(json) = parse(text) else {
        return false;
    };
    matches!(&json, J::Obj(fields) if fields.len() == 1 && fields[0].0 == "at")
        && json_u64(&json, "at").is_ok_and(|at| at > 0)
}

fn messenger_observation(health: &MessengerHealth, now_ms: u64) -> SilenceObservation {
    let threshold_ms = health
        .loop_interval_s
        .saturating_mul(1000)
        .saturating_mul(ALARM_MISSED_INTERVALS);
    match transition(
        now_ms,
        Some(health.last_loop.saturating_mul(1000)),
        threshold_ms,
        false,
    ) {
        Transition::Alert | Transition::Quiet => {
            let age_minutes = now_ms.saturating_sub(health.last_loop.saturating_mul(1000)) / 60_000;
            return SilenceObservation {
                level: SilenceLevel::Alert,
                message: Some(format!(
                    "The fleet messenger hasn't checked in for {age_minutes} minutes. Last seen {}.",
                    clock_label(health.last_loop)
                )),
            };
        }
        Transition::Unknown => {
            return SilenceObservation {
                level: SilenceLevel::Unknown,
                message: Some(
                    "The fleet messenger check-in time is invalid, so its health is unknown."
                        .into(),
                ),
            };
        }
        Transition::Healthy | Transition::Recovered => {}
    }
    if health.peer_count == 0 {
        return SilenceObservation {
            level: SilenceLevel::Healthy,
            message: None,
        };
    }
    let delivery_at = health.last_delivery_ok.unwrap_or(health.started_at);
    match transition(
        now_ms,
        Some(delivery_at.saturating_mul(1000)),
        threshold_ms,
        false,
    ) {
        Transition::Healthy | Transition::Recovered => SilenceObservation {
            level: SilenceLevel::Healthy,
            message: None,
        },
        Transition::Alert | Transition::Quiet => {
            let age_minutes = now_ms.saturating_sub(delivery_at.saturating_mul(1000)) / 60_000;
            SilenceObservation {
                level: SilenceLevel::Alert,
                message: Some(format!(
                    "The fleet messenger is running but hasn't reached a peer for {age_minutes} minutes. I'm retrying; nothing for you to do yet."
                )),
            }
        }
        Transition::Unknown => SilenceObservation {
            level: SilenceLevel::Unknown,
            message: Some(
                "The fleet messenger delivery time is invalid, so its health is unknown.".into(),
            ),
        },
    }
}

fn messenger_observation_at(
    paths: &Paths,
    now_ms: u64,
    messenger_expected: bool,
) -> (SilenceObservation, bool) {
    let bytes = match read_messenger_bytes(&paths.file(MESSENGER_HEARTBEAT)) {
        Ok(Some(bytes)) => bytes,
        Ok(None) if messenger_expected => {
            return (
                SilenceObservation {
                    level: SilenceLevel::Unknown,
                    message: Some(
                        "The fleet messenger hasn't checked in yet, so this computer's status is unknown."
                            .into(),
                    ),
                },
                true,
            );
        }
        Ok(None) => return (healthy_observation(), false),
        Err(_) => {
            return (
                SilenceObservation {
                    level: SilenceLevel::Unknown,
                    message: Some(
                        "I can't read the fleet messenger check-in, so its status is unknown."
                            .into(),
                    ),
                },
                false,
            );
        }
    };
    let health = match validate_messenger_json(&bytes) {
        Ok(health) => health,
        Err(_) => {
            return (
                SilenceObservation {
                    level: SilenceLevel::Unknown,
                    message: Some(
                        "I can't read the fleet messenger check-in, so its status is unknown."
                            .into(),
                    ),
                },
                false,
            );
        }
    };
    (messenger_observation(&health, now_ms), false)
}

fn healthy_observation() -> SilenceObservation {
    SilenceObservation {
        level: SilenceLevel::Healthy,
        message: None,
    }
}

fn log_silence_change(paths: &Paths, message: &str) {
    let queued = super::relay::queue_alert(paths, message);
    let line = format!("{} {message}", crate::stamp(now_s()));
    let log = paths.file("logs").join(SILENCE_LOG);
    let result = (|| {
        std::fs::create_dir_all(log.parent().expect("log has parent"))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        writeln!(file, "{line}")?;
        file.sync_data()
    })();
    if let Err(error) = result {
        eprintln!("Could not save the fleet alert to the local log: {error}. Check that Perennos Tempo can write to its home folder.");
    }
    eprintln!("{message}");
    if queued {
        super::relay::send_pending_once(paths);
    }
}

pub(super) fn monitor_silence(paths: &Paths, state: &mut SilenceMonitor, now_ms: u64) {
    let fleet = match store::load_checked(paths) {
        Ok(fleet) => {
            if state.fleet_unknown {
                log_silence_change(
                    paths,
                    "The fleet configuration is readable again; node monitoring resumed.",
                );
                state.fleet_unknown = false;
            }
            fleet
        }
        Err(_) => {
            if !state.fleet_unknown {
                log_silence_change(
                    paths,
                    "The fleet setup can't be read, so this computer's status is unknown.",
                );
                state.fleet_unknown = true;
            }
            return;
        }
    };
    let mut current_peers = HashSet::new();
    for machine in fleet.machines.iter().filter(|machine| machine.enabled) {
        current_peers.insert(machine.name.clone());
        let observation = peer_observation(machine, now_ms);
        let previous = state
            .peers
            .get(&machine.name)
            .copied()
            .unwrap_or(SilenceLevel::Healthy);
        if observation.level == previous {
            continue;
        }
        match observation.level {
            SilenceLevel::Alert | SilenceLevel::Unknown => {
                if let Some(message) = observation.message {
                    log_silence_change(paths, &message);
                }
            }
            SilenceLevel::Healthy if previous == SilenceLevel::Alert => log_silence_change(
                paths,
                &format!(
                    "{} has checked in again. The silence alert is clear.",
                    machine.name
                ),
            ),
            SilenceLevel::Healthy if previous == SilenceLevel::Unknown => log_silence_change(
                paths,
                &format!(
                    "{} has a valid check-in time again. Its status is known.",
                    machine.name
                ),
            ),
            SilenceLevel::Healthy => {}
        }
        state.peers.insert(machine.name.clone(), observation.level);
    }
    for name in state
        .peers
        .keys()
        .filter(|name| !current_peers.contains(*name))
        .cloned()
        .collect::<Vec<_>>()
    {
        let previous = state.peers.remove(&name).unwrap_or(SilenceLevel::Healthy);
        if previous != SilenceLevel::Healthy {
            log_silence_change(
                paths,
                &format!("Stopped watching {name} because it is no longer enabled."),
            );
        }
    }

    let messenger_expected = fleet.machines.iter().any(|machine| machine.enabled);
    let (messenger, missing_checkin) = messenger_observation_at(paths, now_ms, messenger_expected);
    if missing_checkin {
        let since = *state.missing_messenger_since_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) < HEARTBEAT.as_millis() as u64 {
            return;
        }
    } else {
        state.missing_messenger_since_ms = None;
    }
    let previous = state.messenger;
    if previous != Some(messenger.level) {
        match messenger.level {
            SilenceLevel::Alert | SilenceLevel::Unknown => {
                if let Some(message) = messenger.message {
                    log_silence_change(paths, &message);
                }
            }
            SilenceLevel::Healthy if previous == Some(SilenceLevel::Alert) => log_silence_change(
                paths,
                "The fleet messenger has reached a peer again. Its silence alert is clear.",
            ),
            SilenceLevel::Healthy if previous == Some(SilenceLevel::Unknown) => log_silence_change(
                paths,
                "The fleet messenger check-in is readable again. Its status is known.",
            ),
            SilenceLevel::Healthy => {}
        }
        state.messenger = Some(messenger.level);
    }
}

// ---- the page of facts the GUI and the fleet page read

fn num(v: u64) -> J {
    J::n(v as f64)
}

pub fn view(p: &Paths, node: Option<&Node>) -> J {
    let f = store::load(p);
    let now = now_s();
    let machines: Vec<J> = f
        .machines
        .iter()
        .map(|m| {
            let age = now.saturating_sub(m.last_seen);
            let mut o = vec![
                ("name".to_string(), J::s(&m.name)),
                ("addr".into(), J::s(&m.addr)),
                ("role".into(), J::s(&m.role)),
                ("enabled".into(), J::Bool(m.enabled)),
                ("last_seen".into(), num(m.last_seen)),
                (
                    "up".into(),
                    J::Bool(m.enabled && m.last_seen > 0 && age < AWAY_S),
                ),
                ("health".into(), m.health.clone()),
                ("tools".into(), m.tools.clone()),
                ("ssh".into(), m.ssh.clone()),
            ];
            if m.last_seen > 0 {
                o.push(("age_s".into(), num(age)));
            }
            J::Obj(o)
        })
        .collect();
    J::Obj(vec![
        ("heading".into(), J::s(&super::heading(f.count()))),
        ("count".into(), num(f.count() as u64)),
        ("self".into(), J::s(&f.me)),
        (
            "queued".into(),
            num(node.map_or(0, |n| n.queued(None)) as u64),
        ),
        ("machines".into(), J::Arr(machines)),
    ])
}

// ---- connections

fn respond(s: &mut TcpStream, code: &str, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// A plain GET. `/hello` for a private address (the name only to this fleet or an inviting joiner); `/fleet.json` for this computer only, and only
/// with a Host header that names it (a web page cannot reach it by a name it controls).
fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
}

fn http(node: &Arc<Node>, mut r: BufReader<TcpStream>, peer: SocketAddr, port: u16) {
    let mut head = String::new();
    let mut line = String::new();
    while head.len() < 8192 && r.read_line(&mut line).unwrap_or(0) > 0 {
        let end = line == "\r\n" || line == "\n";
        head.push_str(&line);
        line.clear();
        if end {
            break;
        }
    }
    let first = head.lines().next().unwrap_or("");
    let path = first.split_whitespace().nth(1).unwrap_or("");
    let post = first.starts_with("POST ");
    let host = header(&head, "host")
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    let token = header(&head, "x-tempo-token").unwrap_or_default();
    let want: usize = header(&head, "content-length")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; want.min(1 << 17)];
    if post && want <= 1 << 17 {
        let _ = std::io::Read::read_exact(&mut r, &mut body);
    }
    let mut s = r.into_inner();
    let local = peer.ip().is_loopback()
        && [format!("127.0.0.1:{port}"), format!("localhost:{port}")].contains(&host);
    if post {
        // the agent's door to the fleet's tools: this computer, a Host that names it, and the token
        let ok = local
            && want <= 1 << 17
            && super::crypto::same(
                token.as_bytes(),
                super::tools::local_token(&node.paths).as_bytes(),
            );
        if path == "/call" && ok {
            let (code, out) = super::tools::http_call(node, &body);
            respond(&mut s, code, &out);
        } else {
            respond(
                &mut s,
                "403 Forbidden",
                "{\"error\":\"this computer only, with its token\"}",
            );
        }
        return;
    }
    match path {
        "/hello" if crate::private_ip(peer.ip()) => {
            let invite = super::pair::invite_open(&node.paths);
            // This computer's name is for its own fleet, and for a computer joining while an invite is open.
            // Anyone else on the same network (a cafe's wifi, a shared tailnet) learns that Tempo is here, not who owns it.
            let own = store::load(&node.paths)
                .machines
                .iter()
                .any(|m| m.addr.parse::<IpAddr>().is_ok_and(|a| a == peer.ip()));
            let mut o = vec![("app".to_string(), J::s("tempo-fleet"))];
            if invite || own {
                o.push(("name".into(), J::s(&node.me)));
            }
            o.push(("v".into(), J::n(1.0)));
            o.push(("invite".into(), J::Bool(invite)));
            respond(&mut s, "200 OK", &J::Obj(o).compact());
        }
        "/fleet.json" if local => {
            respond(&mut s, "200 OK", &view(&node.paths, Some(node)).compact())
        }
        "/fleet.json" => respond(
            &mut s,
            "403 Forbidden",
            "{\"error\":\"this computer only\"}",
        ),
        _ => respond(&mut s, "404 Not Found", "{\"error\":\"not found\"}"),
    }
}

pub fn conn(node: &Arc<Node>, s: TcpStream) {
    let Ok(peer) = s.peer_addr() else { return };
    let port = s.local_addr().map_or(0, |a| a.port());
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(300)));
    let mut r = BufReader::new(s);
    let first = match r.fill_buf() {
        Ok(b) if b.len() >= 4 => [b[0], b[1], b[2], b[3]],
        _ => return,
    };
    match &first {
        b"GET " | b"POST" => http(node, r, peer, port),
        b"PAIR" => super::pair::serve_pair(node, r, peer),
        _ => node.frames(r),
    }
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct Completion {
    state: Mutex<CompletionState>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct CompletionState {
    active: usize,
    failure: Option<&'static str>,
}

#[cfg(test)]
struct CompletionToken(Arc<Completion>);

#[cfg(test)]
impl Drop for CompletionToken {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        self.0.changed.notify_all();
    }
}

// If spawning fails, dropping this wrapper drops all work captures before the token.
#[cfg(test)]
struct CompletionWork<F> {
    work: Option<F>,
    token: Option<CompletionToken>,
}

#[cfg(test)]
impl<F> Drop for CompletionWork<F> {
    fn drop(&mut self) {
        drop(self.work.take());
        drop(self.token.take());
    }
}

#[cfg(test)]
impl Completion {
    fn token(self: &Arc<Self>) -> CompletionToken {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active += 1;
        CompletionToken(self.clone())
    }

    fn fail(&self, reason: &'static str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.failure.get_or_insert(reason);
        self.changed.notify_all();
    }

    fn spawn_with<F, S>(self: &Arc<Self>, work: F, start: S)
    where
        F: FnOnce() + Send + 'static,
        S: FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    {
        // The launch token prevents a failed spawn from appearing complete before failure is set.
        let launch = self.token();
        let task = CompletionWork {
            work: Some(work),
            token: Some(self.token()),
        };
        let tracker = self.clone();
        let wrapped = Box::new(move || {
            let mut task = task;
            let work = task.work.take().expect("registered test work");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            if result.is_err() {
                tracker.fail("server thread panicked");
            }
            drop(result);
            drop(task);
        });
        let launched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start(wrapped)));
        if !matches!(launched, Ok(Ok(()))) {
            self.fail("server thread spawn failed");
        }
        drop(launch);
    }

    pub(super) fn spawn<F>(self: &Arc<Self>, work: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.spawn_with(work, |wrapped| {
            std::thread::Builder::new().spawn(wrapped).map(|_| ())
        })
    }

    pub(super) fn wait(&self, limit: Duration) -> Result<(), &'static str> {
        let deadline = Instant::now() + limit;
        let mut state = self.state.lock().map_err(|_| "completion lock poisoned")?;
        loop {
            if let Some(reason) = state.failure {
                return Err(reason);
            }
            if state.active == 0 {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("server completion timed out");
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| "completion lock poisoned")?
                .0;
        }
    }

    pub(super) fn active(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active
    }
}

macro_rules! spawn_fleet {
    ($tracker:expr, $work:expr) => {{
        #[cfg(test)]
        {
            $tracker.spawn($work);
        }
        #[cfg(not(test))]
        {
            std::thread::spawn($work);
        }
    }};
}

pub struct Handle {
    pub addrs: Vec<SocketAddr>,
    /// Addresses that could not be listened on, with why. Never skipped without a word.
    pub failed: Vec<(SocketAddr, String)>,
    stop: Arc<AtomicBool>,
    #[cfg(test)]
    completion: Arc<Completion>,
}

impl Handle {
    #[cfg(test)]
    pub(super) fn completion(&self) -> Arc<Completion> {
        self.completion.clone()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        for a in &self.addrs {
            let _ = TcpStream::connect_timeout(a, Duration::from_millis(200));
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Listen on `addrs` and nothing else (what a test of the receiving side wants).
pub fn listen(node: &Arc<Node>, addrs: &[SocketAddr]) -> Result<Handle, String> {
    #[cfg(test)]
    let completion = Arc::new(Completion::default());
    #[cfg(test)]
    super::tests::track_server(node, completion.clone())?;
    let _ = super::tools::local_token(&node.paths);
    let stop = Arc::new(AtomicBool::new(false));
    let (mut bound, mut failed) = (Vec::new(), Vec::new());
    for a in addrs {
        match TcpListener::bind(a) {
            Ok(l) => {
                bound.push(l.local_addr().map_err(|e| e.to_string())?);
                let (node, stop) = (node.clone(), stop.clone());
                #[cfg(test)]
                let handlers = completion.clone();
                spawn_fleet!(completion, move || {
                    for c in l.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        if let Ok(c) = c {
                            let node = node.clone();
                            spawn_fleet!(handlers, move || conn(&node, c));
                        }
                    }
                });
            }
            Err(e) => failed.push((*a, e.to_string())),
        }
    }
    if bound.is_empty() {
        return Err("no address could be listened on (is it already running?)".into());
    }
    Ok(Handle {
        addrs: bound,
        failed,
        stop,
        #[cfg(test)]
        completion,
    })
}

/// Listen, send what waits in the outbox, send heartbeats. Returns when listening.
pub fn start(node: &Arc<Node>, addrs: &[SocketAddr]) -> Result<Handle, String> {
    let h = listen(node, addrs)?;
    let stop = h.stop.clone();
    let hb: Arc<Mutex<(u64, Vec<u8>)>> = Arc::new(Mutex::new((0, Vec::new())));
    let messenger = Arc::new(Mutex::new(MessengerHealth::new(now_s())));
    {
        let (node, stop, hb, messenger) =
            (node.clone(), stop.clone(), hb.clone(), messenger.clone());
        spawn_fleet!(h.completion, move || {
            let mut tick = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let mut h = health(&node, tick.is_multiple_of(6));
                if tick.is_multiple_of(6) && h.get("tools").is_none() {
                    h.set("tools_none", J::Bool(true));
                }
                tick += 1;
                let body = h.compact().into_bytes();
                {
                    let mut g = lock(&hb);
                    *g = (g.0 + 1, body);
                }
                let snapshot = {
                    let mut status = lock(&messenger);
                    status.last_loop = now_s();
                    status.clone()
                };
                if let Err(error) = update_messenger_heartbeat(&node.paths, &snapshot) {
                    eprintln!("Fleet messenger check-in could not be saved: {error}");
                }
                let end = Instant::now() + HEARTBEAT;
                while Instant::now() < end && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
    }
    {
        let (node, stop, messenger) = (node.clone(), stop.clone(), messenger.clone());
        #[cfg(test)]
        let senders = h.completion.clone();
        spawn_fleet!(h.completion, move || supervise(
            &node,
            &stop,
            &hb,
            &messenger,
            #[cfg(test)]
            senders,
        ));
    }
    Ok(h)
}

/// One sender per machine in the fleet: it delivers what is due and sends the heartbeat.
fn supervise(
    node: &Arc<Node>,
    stop: &Arc<AtomicBool>,
    hb: &Arc<Mutex<(u64, Vec<u8>)>>,
    messenger: &Arc<Mutex<MessengerHealth>>,
    #[cfg(test)] completion: Arc<Completion>,
) {
    let running: Arc<Mutex<HashSet<String>>> = Arc::default();
    let cell: Arc<Mutex<Fleet>> = Arc::default();
    while !stop.load(Ordering::SeqCst) {
        let fleet = store::load(&node.paths);
        lock(messenger).peer_count = fleet.peers().filter(|m| m.name != node.me).count() as u64;
        for m in fleet.peers().filter(|m| m.name != node.me) {
            if lock(&running).insert(m.name.clone()) {
                let (node, stop, hb, messenger, cell, running, name) = (
                    node.clone(),
                    stop.clone(),
                    hb.clone(),
                    messenger.clone(),
                    cell.clone(),
                    running.clone(),
                    m.name.clone(),
                );
                spawn_fleet!(completion, move || {
                    sender(&node, &stop, &hb, &messenger, &cell, &name);
                    lock(&running).remove(&name);
                });
            }
        }
        *lock(&cell) = fleet;
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn sender(
    node: &Arc<Node>,
    stop: &AtomicBool,
    hb: &Mutex<(u64, Vec<u8>)>,
    messenger: &Mutex<MessengerHealth>,
    cell: &Mutex<Fleet>,
    name: &str,
) {
    let mut sent = 0;
    while !stop.load(Ordering::SeqCst) {
        let Some(m) = lock(cell).peers().find(|m| m.name == name).cloned() else {
            return;
        };
        match node.deliver(&m) {
            Ok(count) if count > 0 => {
                let at = now_s();
                let mut status = lock(messenger);
                status.last_delivery_attempt = Some(at);
                status.last_delivery_ok = Some(at);
            }
            Err(_) => {
                let at = now_s();
                let mut status = lock(messenger);
                status.last_delivery_attempt = Some(at);
                status.last_delivery_error = Some(at);
            }
            _ => {}
        }
        let (seq, body) = lock(hb).clone();
        if seq != sent && !body.is_empty() {
            sent = seq;
            let attempt = now_s();
            lock(messenger).last_delivery_attempt = Some(attempt);
            match node.ephemeral(&m, "heartbeat", &body) {
                Ok(()) => lock(messenger).last_delivery_ok = Some(now_s()),
                Err(_) => lock(messenger).last_delivery_error = Some(now_s()),
            }
        }
        let g = lock(&node.kick.0);
        let _ = node.kick.1.wait_timeout(g, Duration::from_millis(50));
    }
}

/// Where the messenger may listen: the private and tailnet addresses of this computer, and
/// 127.0.0.1. Never 0.0.0.0.
pub fn listen_on(port: u16) -> Vec<SocketAddr> {
    let mut v: Vec<IpAddr> = super::scan::listen_addrs(&super::scan::local_addrs())
        .into_iter()
        .map(IpAddr::V4)
        .collect();
    v.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    v.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()
}

/// Where the messenger listens for this fleet: 127.0.0.1 always, and only the private or tailnet
/// addresses of this computer that the fleet needs. A home or office address is opened only when a
/// computer in the fleet is on one; a tailnet address only when one is on the tailnet. A fleet made
/// of tailnet computers leaves the Wi-Fi and the virtual networks alone. With nobody in the fleet
/// yet (an invite is open), or a computer whose address cannot be placed (a name), both are kept.
pub fn listen_for(f: &Fleet, own: &[Ipv4Addr], port: u16) -> Vec<SocketAddr> {
    let peers: Vec<&store::Machine> = f.machines.iter().filter(|m| m.enabled).collect();
    let (mut lan, mut tail) = (peers.is_empty(), peers.is_empty());
    for m in peers {
        match m.addr.parse::<Ipv4Addr>() {
            Ok(a) if super::scan::is_tailnet(a) => tail = true,
            Ok(a) if super::scan::is_lan(a) => lan = true,
            _ => (lan, tail) = (true, true),
        }
    }
    let mut v: Vec<IpAddr> = super::scan::listen_addrs(own)
        .into_iter()
        .filter(|ip| {
            if super::scan::is_tailnet(*ip) {
                tail
            } else {
                lan
            }
        })
        .map(IpAddr::V4)
        .collect();
    v.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    v.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()
}

/// `fleet serve`: listen until the program is ended.
pub fn run(p: &Paths, quiet: bool, announce: bool) -> i32 {
    let Some(key) = store::read_key(p) else {
        eprintln!(
            "tempo-launch: fleet serve: there is no fleet key yet; run tempo-launch fleet init"
        );
        return 1;
    };
    let node = match Node::open(p, key) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("tempo-launch: fleet serve: {e}");
            return 1;
        }
    };
    lock(&node.handlers).push(Arc::new(super::jobs::JobHandler(Arc::new(
        super::jobs::AgentRunner(p.clone()),
    ))));
    lock(&node.handlers).push(Arc::new(super::tools::CallHandler::new(
        p.clone(),
        Arc::new(super::tools::RealRunner(p.clone())),
    )));
    super::blob::attach(&node);
    lock(&node.handlers).push(Arc::new(super::stage::StageHandler::new(
        p.clone(),
        Arc::new(crate::stage::remote::RealRunner),
    )));
    let addrs = listen_for(
        &store::load(p),
        &super::scan::local_addrs(),
        store::fleet_port(),
    );
    let h = match start(&node, &addrs) {
        Ok(h) => h,
        Err(_) if quiet => return 0, // the messenger is already running
        Err(e) => {
            eprintln!("tempo-launch: fleet serve: {e}");
            return 1;
        }
    };
    for (a, why) in &h.failed {
        eprintln!(
            "fleet: could not listen on {a}: {why}{}",
            if a.ip().is_loopback() {
                " (the agent reaches the messenger through this address, so its fleet tools will not work until it is free)"
            } else {
                ""
            }
        );
    }
    let stop = Arc::new(AtomicBool::new(false));
    let own = super::scan::listen_addrs(&super::scan::local_addrs());
    if announce {
        // answering mDNS needs UDP 5353 on every interface, which is what makes a firewall ask: only when asked for
        let (name, port, stop) = (node.me.clone(), store::fleet_port(), stop.clone());
        spawn_fleet!(h.completion, move || {
            if let Err(e) = super::mdns::announce(&stop, &name, port, &own, &["v=1".to_string()]) {
                eprintln!("fleet: {e}");
            }
        });
    }
    if !quiet {
        let list: Vec<String> = h.addrs.iter().map(|a| a.to_string()).collect();
        println!(
            "Fleet messenger {} listening on {} (private addresses and this computer only).",
            node.me,
            list.join(", ")
        );
    }
    let mut monitor = SilenceMonitor::default();
    loop {
        monitor_silence(p, &mut monitor, store::now_ms());
        super::relay::send_pending_once(p);
        let delay = super::relay::next_retry_delay(p, now_s())
            .map_or(HEARTBEAT, |delay| delay.min(HEARTBEAT));
        std::thread::park_timeout(delay);
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    #[test]
    fn rust_alarm_uses_the_same_golden_traces_as_dead_air() {
        for line in perennos_dead_air_silence_core::GOLDEN_TRACES.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(fields.len(), 6, "invalid trace row: {line}");
            let last = if fields[1] == "-" {
                None
            } else {
                Some(fields[1].parse::<u64>().expect("last signal time"))
            };
            let now = fields[2].parse::<u64>().expect("current time");
            let threshold = fields[3].parse::<u64>().expect("threshold");
            let already_quiet = fields[4].parse::<bool>().expect("quiet flag");
            assert_eq!(
                transition(now, last, threshold, already_quiet).as_str(),
                fields[5],
                "trace {}",
                fields[0]
            );
        }
    }

    #[test]
    fn peer_alarm_starts_at_ten_minutes_and_names_last_seen_time() {
        let mut machine = Machine::new("worker", "10.0.0.8", "worker");
        machine.last_seen = 1_800_000_000;
        machine.joined_at = machine.last_seen - 30;
        let at_boundary = machine.last_seen * 1000 + SILENCE_ALARM_MS;
        assert_eq!(
            peer_observation(&machine, at_boundary - 1).level,
            SilenceLevel::Healthy
        );
        assert_eq!(
            peer_observation(&machine, at_boundary).message.as_deref(),
            Some("Worker hasn't checked in for 10 minutes. Last seen 08:00 UTC. I'm retrying; nothing for you to do yet.")
        );
    }

    fn messenger(
        started_at: u64,
        last_loop: u64,
        peer_count: u64,
        last_delivery_ok: Option<u64>,
    ) -> MessengerHealth {
        MessengerHealth {
            pid: 42,
            started_at,
            last_loop,
            loop_interval_s: 10,
            peer_count,
            last_delivery_attempt: last_delivery_ok,
            last_delivery_ok,
            last_delivery_error: None,
        }
    }

    #[test]
    fn messenger_detects_a_stale_work_loop_at_ten_minutes() {
        let health = messenger(100, 100, 1, Some(100));
        let (level, message) = messenger_observation(&health, 700_000);
        assert_eq!(level, MessengerLevel::Alert);
        assert_eq!(
            message.as_deref(),
            Some("The fleet messenger hasn't checked in for 10 minutes. Last seen 00:01 UTC.")
        );
    }

    #[test]
    fn messenger_detects_fresh_loop_with_stale_delivery() {
        let health = messenger(100, 700, 2, None);
        let (level, message) = messenger_observation(&health, 700_000);
        assert_eq!(level, MessengerLevel::Alert);
        assert_eq!(message.as_deref(), Some("The fleet messenger is running but hasn't reached a peer for 10 minutes. I'm retrying; nothing for you to do yet."));
    }

    #[test]
    fn messenger_does_not_warn_without_enabled_peers() {
        let health = messenger(100, 700, 0, None);
        assert_eq!(
            messenger_observation(&health, 700_000).0,
            MessengerLevel::Healthy
        );
    }

    #[test]
    fn future_messenger_and_peer_timestamps_are_unknown() {
        let health = messenger(101, 101, 1, Some(101));
        assert_eq!(
            messenger_observation(&health, 100_000).0,
            MessengerLevel::Unknown
        );
        let mut machine = Machine::new("worker", "10.0.0.8", "worker");
        machine.last_seen = 101;
        machine.joined_at = 100;
        assert_eq!(
            peer_observation(&machine, 100_000).level,
            SilenceLevel::Unknown
        );
    }

    #[test]
    fn messenger_checkin_has_a_bounded_exact_schema_and_reads_old_version_only_for_migration() {
        let health = messenger(100, 110, 1, Some(105));
        let bytes = messenger_json(&health);
        assert_eq!(validate_messenger_json(&bytes).unwrap(), health);
        assert!(is_legacy_messenger_json(br#"{"at":100}"#));
        assert!(validate_messenger_json(br#"{"at":100}"#).is_err());
        assert!(validate_messenger_json(br#"{"pid":1,"extra":1}"#).is_err());
    }

    struct OnDrop(std::sync::mpsc::Sender<()>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn completion_follows_capture_destruction() {
        let tracker = Arc::new(Completion::default());
        let (sent, received) = std::sync::mpsc::channel();
        let guard = OnDrop(sent);
        tracker.spawn(move || {
            let _guard = guard;
        });
        tracker.wait(Duration::from_secs(2)).unwrap();
        assert_eq!(received.try_recv(), Ok(()));
    }

    #[test]
    fn blocked_worker_times_out_then_completes() {
        let tracker = Arc::new(Completion::default());
        let (started, ready) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        tracker.spawn(move || {
            started.send(()).unwrap();
            let _ = blocked.recv();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            tracker.wait(Duration::from_millis(20)),
            Err("server completion timed out")
        );
        release.send(()).unwrap();
        tracker.wait(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn panic_and_spawn_failure_are_reported() {
        let panicked = Arc::new(Completion::default());
        panicked.spawn(|| panic!("fixture panic"));
        assert_eq!(
            panicked.wait(Duration::from_secs(2)),
            Err("server thread panicked")
        );
        let failed = Arc::new(Completion::default());
        let (sent, dropped) = std::sync::mpsc::channel();
        let guard = OnDrop(sent);
        failed.spawn_with(
            move || {
                let _guard = guard;
            },
            |work| {
                drop(work);
                Err(std::io::Error::other("fixture spawn failure"))
            },
        );
        assert_eq!(dropped.try_recv(), Ok(()));
        assert_eq!(
            failed.wait(Duration::ZERO),
            Err("server thread spawn failed")
        );
    }
}
