//! The Conductor's engines: every Ollama this person has, and who plays what on them.
//!
//! The engines are the ones the agent already knows: `ollama_url` (this computer's Ollama, or the
//! one Tempo started; OLLAMA_URL and TEMPO_OLLAMA win, as in the agent), every address in
//! `ollama_fallbacks` (what `tempo-launch link` writes) and every address of a `hosts` entry.
//!
//! Looking at an engine is one `GET /api/tags` with a two second limit, and only at addresses
//! `link` would accept (private, tailnet or loopback). An address that is not one is refused
//! before anything is sent. Nothing is ever started or changed on an engine.
//!
//! Choosing: the heavy members (writing, planning, building) go to the strongest reachable engine
//! that has their model; the light ones (sorting, scheduling) go to the fastest one that has
//! theirs. When the chosen engine is down at run time the next one plays the part, and the run's
//! index note says so.
//!
//! What the agent allows: a conversation can name a model but not an engine. The agent tries
//! `ollama_url` first, then `ollama_fallbacks` in order, and keeps a conversation on the engine
//! that answered. So the Conductor's pick is the engine it expects to answer and the model it
//! passes; the agent's own failover decides where the request really goes.

use super::spec::{Band, Member};
use crate::setup::json::{parse, J};
use crate::setup::propose::engine_url;
use crate::{parse_remote_url, private_ip, Ctx, VERSION};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// How long one engine gets to answer.
pub const LOOK: Duration = Duration::from_secs(2);
const MAX_ANSWER: usize = 4 << 20;
/// The id of this computer's own engine.
pub const LOCAL: &str = "local";

/// An engine as the agent's settings name it, not yet looked at.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub id: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    /// Billions of parameters, from what the engine says about the model.
    pub billions: f64,
}

/// A rough class from the biggest model an engine holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Strong,
    Capable,
    Light,
    Empty,
}

impl Class {
    pub fn word(self) -> &'static str {
        match self {
            Class::Strong => "strong",
            Class::Capable => "capable",
            Class::Light => "light",
            Class::Empty => "no models",
        }
    }
}

/// One engine, looked at.
#[derive(Clone, Debug, PartialEq)]
pub struct Engine {
    pub id: String,
    pub url: String,
    pub up: bool,
    /// How long `/api/tags` took, in milliseconds (0 when the engine was not reached).
    pub ms: u64,
    pub models: Vec<Model>,
    /// Why an engine is not up, in words (empty when it is).
    pub why: String,
}

impl Engine {
    /// The address without its scheme, for people.
    pub fn at(&self) -> &str {
        self.url.split_once("://").map_or(&self.url, |x| x.1)
    }

    /// The biggest model, in billions of parameters (embedding models do not count).
    pub fn strength(&self) -> f64 {
        self.models
            .iter()
            .filter(|m| !is_embedding(&m.name))
            .map(|m| m.billions)
            .fold(0.0, f64::max)
    }

    pub fn class(&self) -> Class {
        match self.strength() {
            s if s >= 20.0 => Class::Strong,
            s if s >= 7.0 => Class::Capable,
            s if s > 0.0 => Class::Light,
            _ => Class::Empty,
        }
    }

    pub fn has(&self, model: &str) -> bool {
        !model.is_empty() && self.models.iter().any(|m| same_model(&m.name, model))
    }

    pub fn size_of(&self, model: &str) -> Option<f64> {
        self.models
            .iter()
            .find(|m| same_model(&m.name, model))
            .map(|m| m.billions)
    }
}

fn is_embedding(name: &str) -> bool {
    name.to_ascii_lowercase().contains("embed")
}

/// `qwen3:8b` is `qwen3:8b`; `llama3` is `llama3:latest`.
fn same_model(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().to_ascii_lowercase();
        if s.contains(':') {
            s
        } else {
            format!("{s}:latest")
        }
    };
    norm(a) == norm(b)
}

// ---- which engines there are

fn id_from(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let id: String = rest
        .trim_end_matches('/')
        .chars()
        .filter(|c| !matches!(c, '[' | ']'))
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".:_-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    id
}

fn name_from(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".:_-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect()
}

/// The engines in the agent's order: the main one (`local`), the fallbacks, then the hosts.
/// `config` is the agent's settings, `env_url` what OLLAMA_URL or TEMPO_OLLAMA says (it wins over
/// the file, as in the agent), `default_port` where this computer's Ollama listens.
pub fn configured_with(config: Option<&J>, env_url: Option<&str>, default_port: u16) -> Vec<Spec> {
    let main = env_url
        .map(str::to_string)
        .or_else(|| config.and_then(|c| c.str("ollama_url").map(String::from)))
        .unwrap_or_else(|| format!("http://127.0.0.1:{default_port}"));
    let mut out = vec![Spec {
        id: LOCAL.into(),
        url: main.trim().trim_end_matches('/').to_string(),
    }];
    let mut add = |id: String, url: &str| {
        let url = url.trim().trim_end_matches('/').to_string();
        if url.is_empty() || id.is_empty() || out.iter().any(|s| s.url == url || s.id == id) {
            return;
        }
        out.push(Spec { id, url });
    };
    if let Some(c) = config {
        for u in c.strs("ollama_fallbacks") {
            add(id_from(&u), &u);
        }
        if let Some(J::Obj(hosts)) = c.get("hosts") {
            for (name, h) in hosts {
                if name == LOCAL {
                    continue;
                }
                for u in h.strs("urls") {
                    add(name_from(name), &u);
                }
            }
        }
    }
    out
}

/// The engines this computer's agent is set up with. Reads the agent's settings file (the one
/// TEMPO_AGENT_CONFIG names, else the launcher's usual one); contacts nothing.
pub fn configured(ctx: &Ctx) -> Vec<Spec> {
    let path = crate::setup::apply::pinned_agent_settings()
        .unwrap_or_else(|| crate::setup::apply::agent_settings_path(&ctx.agent_data));
    let config = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| parse(&t).ok());
    let env_url = ["OLLAMA_URL", "TEMPO_OLLAMA"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty());
    configured_with(
        config.as_ref(),
        env_url.as_deref(),
        crate::setup::apply::ollama_port(),
    )
}

/// The engine ids a member's `engine` may name.
pub fn known_ids(ctx: &Ctx) -> Vec<String> {
    configured(ctx).into_iter().map(|s| s.id).collect()
}

/// What is wrong with the engines a band names: each must be one of `known`.
pub fn unknown(band: &Band, known: &[String]) -> Vec<String> {
    band.members
        .iter()
        .filter(|m| !m.engine.is_empty() && !known.contains(&m.engine))
        .map(|m| {
            format!(
                "@{}: engine \"{}\" is not one of your engines ({}). Remove the line, or add the engine with `tempo-launch link`",
                m.handle,
                m.engine,
                known.join(", ")
            )
        })
        .collect()
}

// ---- looking at an engine

fn down(spec: &Spec, why: impl Into<String>) -> Engine {
    Engine {
        id: spec.id.clone(),
        url: spec.url.clone(),
        up: false,
        ms: 0,
        models: Vec::new(),
        why: why.into(),
    }
}

/// `8.2B`, `137M`, `1.5T` as billions.
fn billions_of(text: &str) -> Option<f64> {
    let t = text.trim();
    let (num, unit) = t.split_at(t.find(|c: char| c.is_ascii_alphabetic())?);
    let n: f64 = num.trim().parse().ok()?;
    let k = match unit.trim().to_ascii_uppercase().as_str() {
        "T" => 1000.0,
        "B" => 1.0,
        "M" => 0.001,
        "K" => 0.000_001,
        _ => return None,
    };
    Some(n * k)
}

/// The models in an `/api/tags` answer. A model that does not say its size in parameters is
/// sized from its bytes (about 0.6 GB per billion at the usual 4-bit packing).
pub fn models_in(body: &str) -> Option<Vec<Model>> {
    let j = parse(body).ok()?;
    j.get("models")?;
    Some(
        j.arr("models")
            .iter()
            .filter_map(|m| {
                let name = m.str("name").or_else(|| m.str("model"))?;
                if name.is_empty() || name.len() > 100 {
                    return None;
                }
                let billions = m
                    .get("details")
                    .and_then(|d| d.str("parameter_size"))
                    .and_then(billions_of)
                    .or_else(|| m.f64("size").map(|b| b / 0.6e9))
                    .unwrap_or(0.0);
                Some(Model {
                    name: name.to_string(),
                    billions: (billions * 10.0).round() / 10.0,
                })
            })
            .collect(),
    )
}

fn get_tags(host: &str, port: u16) -> Result<(u16, String), String> {
    let end = Instant::now() + LOOK;
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|_| format!("{host} did not resolve"))?
        .collect();
    // a name must still lead to a private address when it is looked up
    let Some(addr) = addrs.iter().find(|a| private_ip(a.ip())) else {
        return Err(format!("{host} does not lead to a private address"));
    };
    let mut s = TcpStream::connect_timeout(addr, LOOK)
        .map_err(|e| format!("could not reach {host}:{port} ({e})"))?;
    s.set_write_timeout(Some(LOOK)).ok();
    let req = format!(
        "GET /api/tags HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\nUser-Agent: tempo-launch/{VERSION}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes())
        .map_err(|e| format!("could not ask {host}:{port} ({e})"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("{host}:{port} did not answer in two seconds"));
        }
        s.set_read_timeout(Some(left)).ok();
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if buf.is_empty() => {
                return Err(format!("{host}:{port} did not answer ({e})"));
            }
            Err(_) => break,
        }
        if buf.len() > MAX_ANSWER {
            return Err("the answer was too big".into());
        }
    }
    crate::parse_response(&buf, true).ok_or_else(|| format!("{host}:{port} did not speak HTTP"))
}

/// Look at one engine. A refused address costs no connection at all.
pub fn probe(spec: &Spec) -> Engine {
    let url = match engine_url(&spec.url) {
        Ok(u) => u,
        Err(e) => return down(spec, format!("refused: {e}")),
    };
    let (https, host, port) = match parse_remote_url(&url) {
        Ok(x) => x,
        Err(e) => return down(spec, format!("refused: {e}")),
    };
    if https {
        return down(
            spec,
            "an https address: Tempo looks at plain http engines only, so it cannot tell",
        );
    }
    let started = Instant::now();
    match get_tags(&host, port) {
        Ok((200, body)) => match models_in(&body) {
            Some(models) => Engine {
                id: spec.id.clone(),
                url,
                up: true,
                ms: started.elapsed().as_millis() as u64,
                models,
                why: String::new(),
            },
            None => down(spec, "answered, but not like an Ollama"),
        },
        Ok((code, _)) => down(spec, format!("answered HTTP {code}")),
        Err(e) => down(spec, e),
    }
}

/// Every engine, looked at together (the slowest sets the wait: two seconds at most).
pub fn probe_all(specs: &[Spec]) -> Vec<Engine> {
    std::thread::scope(|s| {
        let hs: Vec<_> = specs.iter().map(|sp| s.spawn(|| probe(sp))).collect();
        hs.into_iter()
            .zip(specs)
            .map(|(h, sp)| {
                h.join()
                    .unwrap_or_else(|_| down(sp, "could not be looked at"))
            })
            .collect()
    })
}

// ---- who plays what

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    /// Writing, planning, judging: wants the strongest engine.
    Heavy,
    /// Sorting, scheduling: wants a quick one.
    Light,
}

/// Heavy or light. A handle the templates know decides by what the templates say (a handle that
/// is heavy in as many templates as it is light counts as heavy); any other handle by the size of
/// its model, light under 6 billion parameters.
pub fn weight(m: &Member, engines: &[Engine]) -> Weight {
    let (mut heavy, mut light) = (0, 0);
    for t in super::templates::all() {
        for tm in t.members.iter().filter(|tm| tm.handle == m.handle) {
            if tm.heavy {
                heavy += 1;
            } else {
                light += 1;
            }
        }
    }
    if heavy + light > 0 {
        return if heavy >= light {
            Weight::Heavy
        } else {
            Weight::Light
        };
    }
    match engines.iter().find_map(|e| e.size_of(&m.model)) {
        Some(b) if b < 6.0 => Weight::Light,
        _ => Weight::Heavy,
    }
}

/// The reachable engines that have `model`, best first for this weight: the strongest for a heavy
/// part, the quickest to answer for a light one (the agent's order breaks ties).
pub fn candidates<'a>(model: &str, w: Weight, engines: &'a [Engine]) -> Vec<&'a Engine> {
    let mut c: Vec<(usize, &Engine)> = engines
        .iter()
        .enumerate()
        .filter(|(_, e)| e.up && e.has(model))
        .collect();
    match w {
        Weight::Heavy => c.sort_by(|a, b| {
            b.1.strength()
                .partial_cmp(&a.1.strength())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        }),
        Weight::Light => c.sort_by(|a, b| a.1.ms.cmp(&b.1.ms).then(a.0.cmp(&b.0))),
    }
    c.into_iter().map(|(_, e)| e).collect()
}

/// Pick an engine for every member of a new band and keep it in `engine`. A member that no
/// reachable engine can play (its model is nowhere) is left without one: the agent decides.
pub fn assign(band: &mut Band, engines: &[Engine]) {
    for m in &mut band.members {
        let w = weight(m, engines);
        m.engine = candidates(&m.model, w, engines)
            .first()
            .map_or(String::new(), |e| e.id.clone());
    }
}

/// Who plays one part in this run.
#[derive(Clone, Debug, PartialEq)]
pub struct Cast {
    pub handle: String,
    pub name: String,
    pub model: String,
    /// The engine expected to answer (empty: the agent's own order).
    pub engine: String,
    /// What changed from the band's plan, in words (empty when nothing did).
    pub note: String,
}

/// The cast for a run, checked against the engines as they are now. The engine saved in band.json
/// plays the part when it is up and has the model. If not, the next best one steps in and the
/// note says why.
pub fn cast(band: &Band, engines: &[Engine]) -> Vec<Cast> {
    band.members
        .iter()
        .map(|m| {
            let w = weight(m, engines);
            let best = candidates(&m.model, w, engines);
            let mut c = Cast {
                handle: m.handle.clone(),
                name: m.name.clone(),
                model: m.model.clone(),
                engine: String::new(),
                note: String::new(),
            };
            let planned = engines.iter().find(|e| e.id == m.engine);
            match (m.engine.is_empty(), planned) {
                (false, Some(p)) if p.up && p.has(&m.model) => c.engine = p.id.clone(),
                (false, p) => {
                    let why = match p {
                        None => format!("engine {} is no longer one of yours", m.engine),
                        Some(p) if !p.up => format!("engine {} is down", p.id),
                        Some(_) => format!("engine {} does not have {}", m.engine, m.model),
                    };
                    match best.first() {
                        Some(next) => {
                            c.engine = next.id.clone();
                            c.note = format!("{why}, so {} played it", next.id);
                        }
                        None => c.note = format!("{why}, and no other engine has {}", m.model),
                    }
                }
                (true, _) => {
                    if let Some(first) = best.first() {
                        c.engine = first.id.clone();
                    }
                }
            }
            c
        })
        .collect()
}

// ---- for people

/// The engines as a list, for `conductor engines`.
pub fn describe(engines: &[Engine]) -> String {
    let mut o = String::new();
    for e in engines {
        if e.up {
            o.push_str(&format!(
                "{:<22} up, {} ms, {} ({})\n",
                e.id,
                e.ms,
                e.class().word(),
                e.at()
            ));
            if e.models.is_empty() {
                o.push_str("    no models yet\n");
            }
            for m in &e.models {
                o.push_str(&format!("    {} ({})\n", m.name, size_words(m.billions)));
            }
        } else {
            o.push_str(&format!("{:<22} down: {}\n", e.id, e.why));
        }
    }
    o
}

pub fn size_words(billions: f64) -> String {
    if billions <= 0.0 {
        "size unknown".into()
    } else if billions < 1.0 {
        format!("{:.0}M", billions * 1000.0)
    } else {
        format!("{billions}B")
    }
}

/// A line for each member saying where it plays, for `new` and `run`.
pub fn cast_words(c: &[Cast]) -> String {
    let mut o = String::new();
    for x in c {
        let on = if x.engine.is_empty() {
            "the agent's own order".to_string()
        } else {
            format!("engine {}", x.engine)
        };
        o.push_str(&format!(
            "  @{} ({}) plays on {on}, {}",
            x.handle, x.name, x.model
        ));
        if !x.note.is_empty() {
            o.push_str(&format!(" ({})", x.note));
        }
        o.push('\n');
    }
    o
}
