//! The plan: which model for which job on this computer. Pure: a `Machine`, the goals and the
//! catalog go in, a `Plan` comes out, so every choice is covered by a table test.
//!
//! The rules, in the order they apply:
//! 1. Memory budget. Graphics card: its memory minus 1 GB for the screen. No card: RAM minus what
//!    the system and your apps need (25% of RAM, at least 3 GB). Apple Silicon shares one pool, so
//!    the RAM figure is used for both.
//! 2. A model fits when its catalog `min_ram_gb` / `min_vram_gb` (download size + 1.5 GB) is within
//!    the budget. A model whose size is unknown is never chosen by size.
//! 3. With a graphics card, models that fit its memory come first; if none does, the plan falls
//!    back to the CPU and says so.
//! 4. Perennos's own model on this computer wins (UMBRA-2 for quick answers, UMBRA-3 for thinking).
//!    Then an installed model, unless a model to fetch has more than 1.5 times its parameters.
//!    Then the most parameters.

use super::catalog::{Model, Proof, Source, HEADROOM_GB};
use super::probe::{Family, Gpu, Local, Machine, Where};
use super::runtime::RuntimePlan;
use std::path::PathBuf;

/// The quick-answer lane only takes downloads this small.
pub const FAST_MAX_GB: f64 = 3.0;
/// Without a usable GPU the thinking lane only takes models that use this many parameters per
/// answer or fewer (a mixture-of-experts model counts its active parameters), so it stays quick.
pub const CPU_ACTIVE_CAP_B: f64 = 8.0;
/// Kept free of the graphics card's memory for the screen.
pub const VRAM_RESERVE_GB: f64 = 1.0;
/// Free disk kept after the downloads.
pub const DISK_MARGIN_GB: f64 = 1.0;
const GB: f64 = 1e9;

/// Which kind of installed models to prefer for the quick and thinking lanes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Prefer {
    /// The owner's preferred models when such are installed here (see `Machine::preferred_prefix`),
    /// otherwise no change.
    #[default]
    Auto,
    /// The same, and say so when none is installed.
    Preferred,
    Mainstream,
    /// Lift the CPU speed limit (8B active parameters) on the thinking lane: bigger, slower.
    Quality,
}

impl Prefer {
    pub fn parse(s: &str) -> Result<Prefer, String> {
        match s.trim().to_lowercase().as_str() {
            "preferred" => Ok(Prefer::Preferred),
            "mainstream" => Ok(Prefer::Mainstream),
            "quality" => Ok(Prefer::Quality),
            other => Err(format!(
                "unknown --prefer value \"{other}\" (use preferred, mainstream or quality)"
            )),
        }
    }
}

/// An installed model whose name starts with the owner's preferred prefix (Ollama may add a
/// registry path). An empty prefix prefers nothing.
pub fn is_preferred(name: &str, prefix: &str) -> bool {
    let n = name.to_lowercase();
    let base = n.rsplit('/').next().unwrap_or(&n);
    !prefix.is_empty() && base.starts_with(prefix) && !base.contains("embed")
}

/// Parameters in billions from a name like `mine-sample-8b:latest`: the last `<n>b` token.
pub fn params_from_name(name: &str) -> Option<f64> {
    name.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .filter_map(|t| t.strip_suffix('b')?.parse::<f64>().ok())
        .rfind(|p| *p > 0.0)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Goals {
    pub chat: bool,
    pub code: bool,
    pub notes: bool,
    pub music: bool,
    pub rooms: bool,
    pub private: bool,
}

impl Goals {
    pub const ALL: [&'static str; 6] = ["chat", "code", "notes", "music", "rooms", "private"];

    pub fn add(&mut self, name: &str) -> Result<(), String> {
        match name.trim().to_lowercase().as_str() {
            "chat" => self.chat = true,
            "code" => self.code = true,
            "notes" => self.notes = true,
            "music" => self.music = true,
            "rooms" => self.rooms = true,
            "private" => self.private = true,
            "" => {}
            other => {
                return Err(format!(
                    "unknown goal \"{other}\" (goals: {})",
                    Self::ALL.join(", ")
                ))
            }
        }
        Ok(())
    }

    /// The goals named with `--goal chat,code`, plus what the installed programs imply (agent:
    /// chat, vault: notes, room: rooms, studio: music). Nothing at all means chat and notes.
    pub fn build(parts: &[&str], named: &[String]) -> Result<Goals, String> {
        let mut g = Goals::default();
        for p in parts {
            g.add(p)?;
        }
        for n in named {
            for w in n.split(',') {
                g.add(w)?;
            }
        }
        if g == Goals::default() {
            g.chat = true;
            g.notes = true;
        }
        Ok(g)
    }

    pub fn names(&self) -> Vec<&'static str> {
        let flags = [
            self.chat,
            self.code,
            self.notes,
            self.music,
            self.rooms,
            self.private,
        ];
        Self::ALL
            .iter()
            .zip(flags)
            .filter(|(_, on)| *on)
            .map(|(n, _)| *n)
            .collect()
    }

    fn wants_heavy(&self) -> bool {
        self.chat || self.code || self.notes || self.private
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Installed,
    Fetch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Run {
    Gpu,
    Cpu,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    /// `fast`, `heavy`, `embed` or `router`.
    pub role: &'static str,
    /// The name the agent is given (an installed tag, or the name to pull).
    pub model: String,
    pub catalog_name: String,
    pub status: Status,
    pub gb: Option<f64>,
    pub licence: String,
    /// How size and licence were established (empty for Perennos's own models).
    pub proof: String,
    pub run: Run,
    pub why: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fetch {
    pub name: String,
    pub catalog_name: String,
    pub gb: Option<f64>,
    pub licence: String,
    pub proof: String,
    pub link: Option<String>,
    pub opt_in: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub goals: Vec<&'static str>,
    pub summary: Vec<String>,
    pub picks: Vec<Pick>,
    pub fetch: Vec<Fetch>,
    pub fetch_gb: f64,
    pub unknown_sizes: Vec<String>,
    pub disk_free_gb: Option<f64>,
    /// Set when the downloads do not fit the disk: `--apply` stops.
    pub refused: Option<String>,
    pub notes: Vec<String>,
    /// Set (by `apply::compute`) when this computer has no Ollama: Tempo's own copy, or why not.
    pub runtime: Option<RuntimePlan>,
}

impl Plan {
    pub fn pick(&self, role: &str) -> Option<&Pick> {
        self.picks.iter().find(|p| p.role == role)
    }
}

struct Cand<'a> {
    model: String,
    cat: &'a Model,
    status: Status,
    gb: Option<f64>,
    need_ram: Option<f64>,
    need_vram: Option<f64>,
    own: bool,
    /// An installed model with the owner's preferred prefix.
    unc: bool,
}

struct Budget {
    ram: Option<f64>,
    gpu: Option<f64>,
    unified: bool,
}

fn budget(m: &Machine) -> Budget {
    let ram = m.ram_total_gb.map(|t| (t - (t * 0.25).max(3.0)).max(0.0));
    match &m.gpu {
        Gpu::Nvidia { total_gb, .. } => Budget {
            ram,
            gpu: Some((total_gb - VRAM_RESERVE_GB).max(0.0)),
            unified: false,
        },
        Gpu::Apple { .. } => Budget {
            ram,
            gpu: ram,
            unified: true,
        },
        Gpu::None => Budget {
            ram,
            gpu: None,
            unified: false,
        },
    }
}

fn norm(tag: &str) -> String {
    let t = tag.to_lowercase();
    t.strip_suffix(":latest").unwrap_or(&t).to_string()
}

fn family_for(name: &str) -> Option<Family> {
    match name {
        "UMBRA-2" => Some(Family::Umbra2),
        "UMBRA-3" => Some(Family::Umbra3),
        _ => None,
    }
}

fn eligible(m: &Model, role: &str) -> bool {
    match role {
        "fast" => m.has_role("fast") || m.has_role("inference"),
        "heavy" => m.has_role("heavy") || m.has_role("thinking") || m.has_role("writing-code"),
        other => m.has_role(other),
    }
}

fn candidates<'a>(
    role: &str,
    m: &Machine,
    catalog: &'a [Model],
    with_d1: bool,
    notes: &mut Vec<String>,
) -> Vec<Cand<'a>> {
    let mut out = Vec::new();
    for cat in catalog.iter().filter(|c| eligible(c, role)) {
        match cat.source {
            Source::LocalOnly => {
                let Some(fam) = family_for(&cat.name) else {
                    continue;
                };
                let found: Vec<&Local> = m.local.iter().filter(|l| l.family == fam).collect();
                if let Some(Local {
                    at: Where::OllamaTag(tag),
                    size_bytes,
                    ..
                }) = found.iter().find(|l| matches!(l.at, Where::OllamaTag(_)))
                {
                    let gb = cat
                        .gb
                        .or((*size_bytes > 0).then(|| *size_bytes as f64 / GB));
                    let need = cat
                        .min_ram_gb
                        .or(gb.map(|g| g + super::catalog::HEADROOM_GB));
                    out.push(Cand {
                        model: tag.clone(),
                        cat,
                        status: Status::Installed,
                        gb,
                        need_ram: need,
                        need_vram: cat.min_vram_gb.or(need),
                        own: true,
                        unc: false,
                    });
                } else if let Some(Local {
                    at: Where::File(p), ..
                }) = found.first()
                {
                    notes.push(format!(
                        "{} is on this computer as {}, but Ollama cannot load it. It needs its llama.cpp service, which setup does not start. It is not added to the agent's settings.",
                        cat.name,
                        p.display()
                    ));
                }
            }
            Source::Huggingface => {
                if !(with_d1 && cat.opt_in) {
                    continue;
                }
                let tag = m.local.iter().find_map(|l| match (&l.at, l.family) {
                    (Where::OllamaTag(t), Family::D1) => Some(t.clone()),
                    _ => None,
                });
                out.push(Cand {
                    model: tag.clone().unwrap_or_else(|| cat.pull_name().to_string()),
                    cat,
                    status: if tag.is_some() {
                        Status::Installed
                    } else {
                        Status::Fetch
                    },
                    gb: cat.gb,
                    need_ram: cat.min_ram_gb,
                    need_vram: cat.min_vram_gb,
                    own: false,
                    unc: is_preferred(&cat.name, &m.preferred_prefix),
                });
            }
            Source::Ollama => {
                if cat.opt_in {
                    continue;
                }
                let have = m
                    .ollama
                    .models
                    .iter()
                    .find(|i| norm(&i.name) == norm(&cat.name));
                if role == "fast" && have.is_none() && cat.gb.is_none_or(|g| g > FAST_MAX_GB) {
                    continue;
                }
                out.push(Cand {
                    model: have.map_or_else(|| cat.pull_name().to_string(), |i| i.name.clone()),
                    cat,
                    status: if have.is_some() {
                        Status::Installed
                    } else {
                        Status::Fetch
                    },
                    gb: cat.gb,
                    need_ram: cat.min_ram_gb,
                    need_vram: cat.min_vram_gb,
                    own: false,
                    unc: is_preferred(&cat.name, &m.preferred_prefix),
                });
            }
        }
    }
    out
}

fn params(c: &Cand) -> f64 {
    c.cat.params_b.unwrap_or(0.0)
}

/// Parameters used per answer (what decides speed on a CPU).
fn active(c: &Cand) -> f64 {
    c.cat.active_params_b.or(c.cat.params_b).unwrap_or(0.0)
}

/// The first item with the biggest key (so the earlier one wins a tie).
fn top<T>(items: impl Iterator<Item = T>, key: impl Fn(&T) -> f64) -> Option<T> {
    let mut best: Option<(f64, T)> = None;
    for it in items {
        let k = key(&it);
        if best.as_ref().is_none_or(|(b, _)| k > *b) {
            best = Some((k, it));
        }
    }
    best.map(|b| b.1)
}

/// The best of the candidates that fit `limit` (None when the limit is unknown: nothing is chosen
/// by size). Perennos's own model comes first, then an installed preferred model (the smallest
/// for the quick lane, the biggest for thinking), then the installed-model rule.
fn best<'a, 'b>(
    cands: &'b [Cand<'a>],
    limit: Option<f64>,
    gpu: bool,
    code: bool,
    fast: bool,
    active_cap: Option<f64>,
) -> Option<&'b Cand<'a>> {
    let limit = limit?;
    let fits: Vec<&Cand> = cands
        .iter()
        .filter(|c| {
            c.need_ram.is_some()
                && c.need_vram.is_some()
                && (if gpu { c.need_vram } else { c.need_ram }).is_some_and(|n| n <= limit + 1e-9)
        })
        .collect();
    if let Some(own) = top(fits.iter().filter(|c| c.own), |c| params(c)) {
        return Some(own);
    }
    let gb = |c: &&&Cand| c.gb.unwrap_or(0.0);
    let unc = fits.iter().filter(|c| c.unc);
    let pick = if fast {
        top(unc, |c| -gb(c))
    } else {
        top(unc, gb)
    };
    if let Some(u) = pick {
        return Some(u);
    }
    // a code-only model serves thinking only when the user wants code
    let usable = |c: &&&Cand| {
        active_cap.is_none_or(|cap| active(c) <= cap + 1e-9)
            && (code
                || c.cat.has_role("thinking")
                || c.cat.has_role("heavy")
                || !c.cat.has_role("writing-code"))
    };
    let wants_code = code && fits.iter().any(|c| c.cat.has_role("writing-code"));
    let pool: Vec<&&Cand> = fits
        .iter()
        .filter(|c| usable(c) && (!wants_code || c.cat.has_role("writing-code")))
        .collect();
    let best_of = top(pool.iter(), |c| params(c))?;
    let installed = top(pool.iter().filter(|c| c.status == Status::Installed), |c| {
        params(c)
    });
    Some(match installed {
        Some(i) if params(i) * 1.5 >= params(best_of) => i,
        _ => best_of,
    })
}

fn size_text(gb: Option<f64>) -> String {
    gb.map_or("size unknown".into(), |g| format!("{g:.1} GB"))
}

fn purpose(role: &str) -> &'static str {
    match role {
        "fast" => "quick answers",
        "heavy" => "thinking and code",
        "embed" => "memory",
        _ => "routing and judging",
    }
}

fn hardware(m: &Machine) -> String {
    let ram = m.ram_total_gb.map_or("unknown RAM".to_string(), |r| {
        format!("{:.0} GB RAM", r.round())
    });
    match &m.gpu {
        Gpu::None => format!("{ram}, no GPU"),
        Gpu::Nvidia { name, total_gb, .. } => format!(
            "{ram}, {} {:.0} GB",
            name.trim_start_matches("NVIDIA ")
                .trim_start_matches("GeForce "),
            total_gb.round()
        ),
        Gpu::Apple { chip } => format!("{ram}, {chip} (shared memory)"),
    }
}

fn make_pick(
    role: &'static str,
    c: &Cand,
    run: Run,
    m: &Machine,
    cpu_note: Option<String>,
) -> Pick {
    let place = match run {
        Run::Gpu if matches!(m.gpu, Gpu::Apple { .. }) => "runs on the Apple chip",
        Run::Gpu => "runs on the graphics card",
        Run::Cpu => "runs on CPU",
    };
    let state = match (c.own, &c.status) {
        (true, _) => "yours, already on this computer, never uploaded",
        (false, Status::Installed) if c.unc => "already installed, one of your preferred models",
        (false, Status::Installed) => "already installed",
        (false, Status::Fetch) => "will be fetched",
    };
    let mut why = format!(
        "{}: {} ({}) for {}, {place}; {state}",
        hardware(m),
        c.model,
        size_text(c.gb),
        purpose(role)
    );
    if let Some(n) = cpu_note {
        why.push_str(&format!(" ({n})"));
    }
    why.push_str(&format!(". Licence: {}.", c.cat.licence));
    let proof = c.cat.proof.text().unwrap_or_default();
    if !proof.is_empty() {
        why.push_str(&format!(" {proof}."));
    }
    Pick {
        role,
        model: c.model.clone(),
        catalog_name: c.cat.name.clone(),
        status: c.status.clone(),
        gb: c.gb,
        licence: c.cat.licence.clone(),
        proof,
        run,
        why,
    }
}

fn choose_role(
    role: &'static str,
    m: &Machine,
    goals: &Goals,
    catalog: &[Model],
    with_d1: bool,
    prefer: Prefer,
    notes: &mut Vec<String>,
) -> Option<Pick> {
    let cands = candidates(role, m, catalog, with_d1, notes);
    if role == "embed" {
        // memory always runs on the CPU, and its size is not what picks it
        let b = budget(m);
        // memory models are small and run on the CPU: their own size must fit, no headroom
        let small = |c: &&Cand| match (b.ram, c.gb) {
            (Some(r), Some(n)) => n <= r + 1e-9,
            _ => true,
        };
        let c = cands
            .iter()
            .find(|c| c.status == Status::Installed)
            .or_else(|| cands.iter().find(small))?;
        return Some(make_pick(role, c, Run::Cpu, m, None));
    }
    let b = budget(m);
    if b.ram.is_none() {
        notes.push(format!(
            "This computer's memory could not be read, so no model was recommended for {} (a model is never chosen without its size).",
            purpose(role)
        ));
        return None;
    }
    let code = goals.code && role == "heavy";
    if m.gpu != Gpu::None {
        if let Some(c) = best(&cands, b.gpu, true, code, role == "fast", None) {
            return Some(make_pick(role, c, Run::Gpu, m, None));
        }
    }
    // On a CPU the thinking lane is ranked by speed first: only models that use 8B parameters or
    // fewer per answer, unless the user asked for quality.
    let cpu = !b.unified;
    let cap = (cpu && role == "heavy" && prefer != Prefer::Quality).then_some(CPU_ACTIVE_CAP_B);
    let c = best(&cands, b.ram, false, code, role == "fast", cap)?;
    let run = if b.unified { Run::Gpu } else { Run::Cpu };
    let mut why: Vec<String> = Vec::new();
    if matches!(m.gpu, Gpu::Nvidia { .. }) {
        why.push("too big for the graphics card's memory".into());
    }
    if cpu && role == "heavy" {
        let a = active(c);
        if a + 1e-9 < params(c) {
            why.push(format!(
                "mixture of experts, about {a:.0}B active, so it runs fast on CPU"
            ));
        } else if a > CPU_ACTIVE_CAP_B + 1e-9 {
            why.push(format!(
                "{a:.0}B active parameters: slower on CPU, chosen because of --prefer quality"
            ));
        } else if cap.is_some() {
            why.push(format!(
                "{a:.0}B or fewer active parameters, so it stays quick on CPU"
            ));
        }
    }
    let note = (!why.is_empty()).then(|| why.join("; "));
    Some(make_pick(role, c, run, m, note))
}

pub fn summary(m: &Machine) -> Vec<String> {
    let mut s = vec![format!("{} {}, {} logical CPUs", m.os, m.arch, m.cpus)];
    s.push(match (m.ram_total_gb, m.ram_avail_gb) {
        (Some(t), Some(a)) => format!("Memory: {t:.1} GB, {a:.1} GB free"),
        (Some(t), None) => format!("Memory: {t:.1} GB"),
        _ => "Memory: unknown".into(),
    });
    s.push(match &m.gpu {
        Gpu::None => "Graphics: no dedicated GPU (Tempo plans for the CPU)".into(),
        Gpu::Nvidia {
            name,
            total_gb,
            free_gb,
        } => format!("Graphics: {name}, {total_gb:.0} GB memory, {free_gb:.1} GB free"),
        Gpu::Apple { chip } => format!("Graphics: {chip}, shares the computer's memory"),
    });
    s.push(
        m.disk_free_gb
            .map_or("Free disk in the Tempo home: unknown".into(), |d| {
                format!("Free disk in the Tempo home: {d:.1} GB")
            }),
    );
    s.push(if m.ollama.running {
        format!(
            "Ollama: running{}, {} models installed",
            m.ollama
                .version
                .as_ref()
                .map_or(String::new(), |v| format!(" ({v})")),
            m.ollama.models.len()
        )
    } else if m.ollama.binary.is_some() {
        "Ollama: installed, not running".into()
    } else {
        "Ollama: not found".into()
    });
    let own: Vec<String> = m
        .local
        .iter()
        .map(|l| match &l.at {
            Where::OllamaTag(t) => t.clone(),
            Where::File(p) => p.display().to_string(),
        })
        .collect();
    if !own.is_empty() {
        s.push(format!("Perennos models found here: {}", own.join(", ")));
    }
    s
}

pub fn plan(m: &Machine, goals: &Goals, with_d1: bool, catalog: &[Model]) -> Plan {
    plan_with(m, goals, with_d1, catalog, Prefer::Auto)
}

/// Installed models with the owner's preferred prefix as catalog entries, so the lanes can choose them. Their size is
/// Ollama's own; their licence is unknown to Tempo. They are never fetched: they only exist here
/// because they are installed.
fn preferred_models(m: &Machine) -> Vec<Model> {
    m.ollama
        .models
        .iter()
        .filter(|i| is_preferred(&i.name, &m.preferred_prefix))
        .map(|i| {
            let gb = (i.size_bytes > 0).then(|| (i.size_bytes as f64 / GB * 100.0).round() / 100.0);
            let need = gb.map(|g| ((g + HEADROOM_GB) * 10.0).round() / 10.0);
            Model {
                name: i.name.clone(),
                source: Source::Ollama,
                gb,
                params_b: params_from_name(&i.name),
                active_params_b: None,
                roles: vec!["fast".into(), "heavy".into()],
                licence: "unknown (not checked: see the model's own page)".into(),
                min_ram_gb: need,
                min_vram_gb: need,
                opt_in: false,
                pull: None,
                link: None,
                notes: "Installed here; setup never fetches these.".into(),
                proof: Proof::Installed,
            }
        })
        .collect()
}

pub fn plan_with(
    m: &Machine,
    goals: &Goals,
    with_d1: bool,
    catalog: &[Model],
    prefer: Prefer,
) -> Plan {
    let mut notes = Vec::new();
    let mut cat: Vec<Model> = catalog.to_vec();
    let unc = preferred_models(m);
    match (prefer, unc.len()) {
        (Prefer::Mainstream, 0) | (Prefer::Auto, 0) | (Prefer::Quality, 0) => {}
        (Prefer::Mainstream, n) => notes.push(format!(
            "{n} of your preferred model(s) installed here were left out because of --prefer mainstream."
        )),
        (_, _) if m.preferred_prefix.is_empty() => notes.push("--prefer preferred: no preferred prefix is set (setup --preferred-prefix P, or TEMPO_PREFERRED_PREFIX), so mainstream models are recommended.".into()),
        (_, _) if unc.is_empty() => notes.push(format!("--prefer preferred: no model whose name starts with {} is installed here, and setup never fetches them, so mainstream models are recommended.", m.preferred_prefix)),
        (_, _) => cat.extend(unc),
    }
    if prefer == Prefer::Quality {
        notes.push(format!("--prefer quality: without a GPU the thinking lane may use models above {CPU_ACTIVE_CAP_B:.0}B active parameters. They answer more slowly on a CPU."));
    }
    let catalog = &cat[..];
    let mut picks = Vec::new();
    let mut roles: Vec<&'static str> = vec!["fast"];
    if goals.wants_heavy() {
        roles.push("heavy");
    }
    roles.push("embed");
    if with_d1 {
        roles.push("router");
    }
    for role in roles {
        match choose_role(role, m, goals, catalog, with_d1, prefer, &mut notes) {
            Some(p) => picks.push(p),
            None if role == "embed" => {
                notes.push("No memory model is in the catalog, so notes search stays off.".into())
            }
            None if role == "router" => {
                notes.push("d1 does not fit this computer's memory, so it is left out.".to_string())
            }
            None => {}
        }
    }
    if !with_d1 {
        notes.push("d1 (routing and judging) is optional and has a licence condition: add --with-d1 to include it.".into());
    }
    if let (Some(f), Some(h)) = (
        picks.iter().find(|p| p.role == "fast"),
        picks.iter().find(|p| p.role == "heavy"),
    ) {
        if f.model == h.model {
            notes.push(format!(
                "{} serves quick answers and thinking: it is the only model that fits both jobs.",
                f.model
            ));
        }
    }
    if goals.code && !goals.chat && picks.iter().all(|p| p.role != "heavy") {
        notes.push("No model that fits was found for code.".into());
    }
    let mut fetch: Vec<Fetch> = Vec::new();
    for p in picks.iter().filter(|p| p.status == Status::Fetch) {
        if fetch.iter().any(|f| f.name == p.model) {
            continue;
        }
        if let Some(c) = catalog.iter().find(|c| c.name == p.catalog_name) {
            fetch.push(Fetch {
                name: p.model.clone(),
                catalog_name: c.name.clone(),
                gb: c.gb,
                licence: c.licence.clone(),
                proof: c.proof.text().unwrap_or_default(),
                link: c.link.clone(),
                opt_in: c.opt_in,
            });
        }
    }
    let fetch_gb: f64 = fetch.iter().filter_map(|f| f.gb).sum();
    let unknown_sizes: Vec<String> = fetch
        .iter()
        .filter(|f| f.gb.is_none())
        .map(|f| f.name.clone())
        .collect();
    let refused = match m.disk_free_gb {
        Some(free) if !fetch.is_empty() && fetch_gb + DISK_MARGIN_GB > free => Some(format!(
            "Not enough free disk: {fetch_gb:.1} GB to fetch and {DISK_MARGIN_GB:.0} GB to keep free, but only {free:.1} GB is free. Free some space, then run setup again."
        )),
        _ => None,
    };
    Plan {
        goals: goals.names(),
        summary: summary(m),
        picks,
        fetch,
        fetch_gb,
        unknown_sizes,
        disk_free_gb: m.disk_free_gb,
        refused,
        notes,
        runtime: None,
    }
}

/// The paths of UMBRA files that were found but cannot be used through Ollama (for tests and notes).
pub fn unusable_files(m: &Machine) -> Vec<PathBuf> {
    m.local
        .iter()
        .filter_map(|l| match &l.at {
            Where::File(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

/// The plan in plain words.
pub fn render(p: &Plan) -> String {
    let mut s = String::from("Perennos Tempo setup\n\nThis computer\n");
    for l in &p.summary {
        s.push_str(&format!("  {l}\n"));
    }
    s.push_str(&format!("\nGoals: {}\n\nPlan\n", p.goals.join(", ")));
    if p.picks.is_empty() {
        s.push_str("  No model could be chosen (see the notes below).\n");
    }
    for k in &p.picks {
        s.push_str(&format!("  {}\n", k.why));
    }
    if let Some(r) = &p.runtime {
        s.push_str("\nOllama (the program that runs the models)\n");
        for l in r.lines() {
            s.push_str(&format!("  {l}\n"));
        }
    }
    s.push_str("\nTo fetch\n");
    if p.fetch.is_empty() {
        s.push_str("  Nothing: everything chosen is already here.\n");
    }
    for f in &p.fetch {
        let proof = if f.proof.is_empty() {
            String::new()
        } else {
            format!("; {}", f.proof)
        };
        s.push_str(&format!(
            "  {} ({}, licence: {}{proof})\n",
            f.name,
            size_text(f.gb),
            f.licence
        ));
        if f.opt_in {
            if let Some(l) = &f.link {
                s.push_str(&format!("    Licence text: {l}\n"));
            }
        }
    }
    if !p.fetch.is_empty() {
        let unknown = if p.unknown_sizes.is_empty() {
            String::new()
        } else {
            format!(" plus {} of unknown size", p.unknown_sizes.join(", "))
        };
        s.push_str(&format!(
            "  Total {:.1} GB{unknown}; free disk {}\n",
            p.fetch_gb,
            p.disk_free_gb
                .map_or("unknown".into(), |d| format!("{d:.0} GB"))
        ));
    }
    if let Some(r) = &p.refused {
        s.push_str(&format!("\nStopped: {r}\n"));
    }
    if !p.notes.is_empty() {
        s.push_str("\nNotes\n");
        for n in &p.notes {
            s.push_str(&format!("  {n}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::super::catalog::{parse_catalog, EMBEDDED};
    use super::super::probe::Installed;
    use super::*;

    fn machine(ram: f64, gpu: Gpu) -> Machine {
        Machine {
            os: "linux".into(),
            arch: "x86_64".into(),
            cpus: 8,
            ram_total_gb: Some(ram),
            ram_avail_gb: Some(ram * 0.6),
            gpu,
            disk_free_gb: Some(200.0),
            ollama: Default::default(),
            local: vec![],
            preferred_prefix: String::new(),
        }
    }

    fn nvidia(total: f64) -> Gpu {
        Gpu::Nvidia {
            name: "NVIDIA GeForce RTX 3060".into(),
            total_gb: total,
            free_gb: total - 0.5,
        }
    }

    fn real() -> Vec<Model> {
        parse_catalog(EMBEDDED).unwrap()
    }

    /// The real catalog plus test-only models, so the tables can show a bigger machine choosing
    /// a bigger model. (Their names and sizes are made up and never shipped.)
    fn with_test_models() -> Vec<Model> {
        let mut c = real();
        c.extend(
            parse_catalog(
                r#"{"models":[
                {"name":"test-1b","source":"ollama","gb":0.8,"params_b":1,"roles":["inference","thinking"]},
                {"name":"test-8b","source":"ollama","gb":5.0,"params_b":8,"roles":["thinking","inference"]},
                {"name":"test-14b","source":"ollama","gb":9.0,"params_b":14,"roles":["thinking"]},
                {"name":"test-code-9b","source":"ollama","gb":5.5,"params_b":9,"roles":["writing-code","thinking"]},
                {"name":"test-32b","source":"ollama","gb":19.0,"params_b":32,"roles":["thinking"]}]}"#,
            )
            .unwrap(),
        );
        c
    }

    fn goals() -> Goals {
        Goals::build(&[], &[]).unwrap()
    }

    #[test]
    fn eight_gb_no_gpu_gets_the_four_b_on_cpu() {
        let p = plan(&machine(8.0, Gpu::None), &goals(), false, &real());
        assert_eq!(p.pick("heavy").unwrap().model, "qwen3:4b");
        assert_eq!(p.pick("heavy").unwrap().run, Run::Cpu);
        assert!(p
            .pick("heavy")
            .unwrap()
            .why
            .starts_with("8 GB RAM, no GPU: qwen3:4b (2.5 GB) for thinking and code, runs on CPU"));
        assert_eq!(p.pick("embed").unwrap().model, "embeddinggemma");
        assert!(p.fetch.iter().any(|f| f.name == "qwen3:4b"));
        assert!(p.pick("router").is_none());
    }

    #[test]
    fn four_gb_machine_fits_nothing_by_size() {
        // 4 GB: budget is 1 GB; the 4b needs 4
        let p = plan(&machine(4.0, Gpu::None), &goals(), false, &real());
        assert!(p.pick("heavy").is_none());
        assert!(p.pick("fast").is_none());
        assert!(p.pick("embed").is_some());
    }

    #[test]
    fn sixteen_gb_no_gpu_picks_the_biggest_that_fits_the_budget() {
        // budget 12 GB: test-14b needs 10.5 and fits; test-32b needs 20.5 and does not
        let p = plan(
            &machine(16.0, Gpu::None),
            &goals(),
            false,
            &with_test_models(),
        );
        // no GPU: dense models above 8B are too slow, so the 8B wins (changed from test-14b by the CPU speed rule)
        assert_eq!(p.pick("heavy").unwrap().model, "test-8b");
        assert_eq!(p.pick("heavy").unwrap().run, Run::Cpu);
        // --prefer quality lifts the cap: the original choice
        let q = plan_with(
            &machine(16.0, Gpu::None),
            &goals(),
            false,
            &with_test_models(),
            Prefer::Quality,
        );
        assert_eq!(q.pick("heavy").unwrap().model, "test-14b");
        // the quick lane stays small: test-14b is over 3 GB
        assert_eq!(p.pick("fast").unwrap().model, "qwen3:4b");
        let p = plan(&machine(16.0, Gpu::None), &goals(), false, &real());
        assert_eq!(p.pick("heavy").unwrap().model, "qwen3:4b");
    }

    #[test]
    fn thirty_two_gb_with_a_12gb_nvidia_uses_the_card_then_falls_back() {
        // card budget 11 GB: test-14b needs 10.5 and fits the card
        let m = machine(32.0, nvidia(12.0));
        let p = plan(&m, &goals(), false, &with_test_models());
        let h = p.pick("heavy").unwrap();
        assert_eq!((h.model.as_str(), h.run), ("test-14b", Run::Gpu));
        assert!(h.why.contains("RTX 3060 12 GB") && h.why.contains("graphics card"));
        // a 4 GB card: the 4b needs 4 > 3 GB budget, so the CPU plan takes over and says why
        let p = plan(&machine(32.0, nvidia(4.0)), &goals(), false, &real());
        let h = p.pick("heavy").unwrap();
        assert_eq!(h.run, Run::Cpu);
        assert!(h.why.contains("too big for the graphics card"));
    }

    #[test]
    fn apple_silicon_16gb_uses_shared_memory() {
        let m = machine(
            16.0,
            Gpu::Apple {
                chip: "Apple M2".into(),
            },
        );
        let p = plan(&m, &goals(), false, &with_test_models());
        let h = p.pick("heavy").unwrap();
        assert_eq!(h.model, "test-14b");
        assert_eq!(h.run, Run::Gpu);
        assert!(h.why.contains("Apple M2 (shared memory)") && h.why.contains("Apple chip"));
        // memory still on the CPU
        assert_eq!(p.pick("embed").unwrap().run, Run::Cpu);
    }

    #[test]
    fn installed_umbra_2_is_preferred_for_fast_and_is_never_fetched() {
        let mut m = machine(16.0, Gpu::None);
        m.ollama.models = vec![Installed {
            name: "umbra-2:latest".into(),
            size_bytes: 1_100_000_000,
            digest: "x".into(),
        }];
        m.local = vec![Local {
            family: Family::Umbra2,
            at: Where::OllamaTag("umbra-2:latest".into()),
            size_bytes: 1_100_000_000,
        }];
        let p = plan(&m, &goals(), false, &real());
        let f = p.pick("fast").unwrap();
        assert_eq!(f.model, "umbra-2:latest");
        assert_eq!(f.status, Status::Installed);
        assert!(f.why.contains("never uploaded"));
        assert!(p.fetch.iter().all(|x| x.catalog_name != "UMBRA-2"));
    }

    #[test]
    fn umbra_found_only_as_a_gguf_is_not_used_and_not_fetched() {
        let mut m = machine(16.0, Gpu::None);
        m.local = vec![Local {
            family: Family::Umbra2,
            at: Where::File("/h/models/umbra-2.gguf".into()),
            size_bytes: 1,
        }];
        let p = plan(&m, &goals(), false, &real());
        assert_ne!(
            p.pick("fast").map(|k| k.catalog_name.as_str()),
            Some("UMBRA-2")
        );
        assert!(p.notes.iter().any(|n| n.contains("llama.cpp")));
        assert!(p.fetch.iter().all(|x| !x.catalog_name.starts_with("UMBRA")));
        assert_eq!(unusable_files(&m).len(), 1);
    }

    #[test]
    fn umbra_never_appears_in_a_fetch_list_even_when_absent() {
        let p = plan(&machine(64.0, nvidia(24.0)), &goals(), true, &real());
        assert!(p
            .fetch
            .iter()
            .all(|x| !x.name.to_lowercase().contains("umbra")));
        assert!(p
            .picks
            .iter()
            .all(|k| !k.model.to_lowercase().contains("umbra")));
    }

    #[test]
    fn d1_only_with_the_flag_and_with_its_licence_in_the_fetch() {
        let m = machine(16.0, Gpu::None);
        assert!(plan(&m, &goals(), false, &real()).pick("router").is_none());
        let p = plan(&m, &goals(), true, &real());
        let r = p.pick("router").unwrap();
        assert_eq!(r.model, "hf.co/LiquidAI/d1-2B-GGUF:Q4_K_M");
        let f = p.fetch.iter().find(|f| f.opt_in).unwrap();
        assert!(f.licence.contains("USD 10 million"));
        assert!(f
            .link
            .as_deref()
            .unwrap()
            .contains("4a181094d1b6addfaa3a0956f78e7ab2aa406a0c"));
        assert!(render(&p).contains("Licence text: https://huggingface.co/LiquidAI/d1-2B-GGUF"));
        assert!(plan(&m, &goals(), false, &real())
            .notes
            .iter()
            .any(|n| n.contains("--with-d1")));
    }

    #[test]
    fn disk_too_small_is_refused() {
        let mut m = machine(16.0, Gpu::None);
        m.disk_free_gb = Some(2.0);
        let p = plan(&m, &goals(), false, &real());
        assert!(p
            .refused
            .as_deref()
            .unwrap()
            .contains("Not enough free disk"));
        assert!(render(&p).contains("Stopped:"));
        m.disk_free_gb = Some(100.0);
        assert!(plan(&m, &goals(), false, &real()).refused.is_none());
        // nothing to fetch: never refused, however small the disk
        m.disk_free_gb = Some(0.1);
        m.ollama.models = vec![
            Installed {
                name: "qwen3:4b".into(),
                size_bytes: 1,
                digest: String::new(),
            },
            Installed {
                name: "embeddinggemma:latest".into(),
                size_bytes: 1,
                digest: String::new(),
            },
        ];
        let p = plan(&m, &goals(), false, &real());
        assert!(p.fetch.is_empty() && p.refused.is_none());
    }

    #[test]
    fn unknown_sizes_are_never_chosen_by_size() {
        let cat = parse_catalog(
            r#"{"models":[
            {"name":"mystery:70b","source":"ollama","gb":null,"params_b":70,"roles":["thinking","inference"]},
            {"name":"known:3b","source":"ollama","gb":2.0,"params_b":3,"roles":["thinking","inference"]}]}"#,
        )
        .unwrap();
        let p = plan(&machine(64.0, Gpu::None), &goals(), false, &cat);
        assert_eq!(p.pick("heavy").unwrap().model, "known:3b");
        assert_eq!(p.pick("fast").unwrap().model, "known:3b");
        // memory unreadable: nothing is chosen by size at all
        let mut m = machine(8.0, Gpu::None);
        m.ram_total_gb = None;
        let p = plan(&m, &goals(), false, &real());
        assert!(p.pick("heavy").is_none() && p.pick("fast").is_none());
        assert!(p.notes.iter().any(|n| n.contains("could not be read")));
        // embeddinggemma is chosen for memory by policy; its size is shown as unknown
        let e = p.pick("embed").unwrap();
        assert!(e.why.contains("size unknown"));
        assert_eq!(p.unknown_sizes, vec!["embeddinggemma".to_string()]);
    }

    #[test]
    fn installed_wins_unless_a_fetch_has_much_more_parameters() {
        let mut m = machine(16.0, Gpu::None);
        m.ollama.models = vec![Installed {
            name: "test-8b:latest".into(),
            size_bytes: 5_000_000_000,
            digest: String::new(),
        }];
        let cat = with_test_models();
        // test-8b is not named test-8b:latest in the catalog? it is "test-8b": ":latest" matches
        let p = plan_with(&m, &goals(), false, &cat, Prefer::Quality);
        // test-14b has 14 > 8*1.5: the better fetch wins
        assert_eq!(p.pick("heavy").unwrap().model, "test-14b");
        let mut small = cat.clone();
        small.retain(|c| c.name != "test-14b" && c.name != "test-32b");
        small.push(parse_catalog(r#"{"models":[{"name":"test-9b","source":"ollama","gb":6,"params_b":9,"roles":["thinking"]}]}"#).unwrap().remove(0));
        // 9 <= 8*1.5: stay with what is installed
        let p = plan_with(&m, &goals(), false, &small, Prefer::Quality);
        assert_eq!(p.pick("heavy").unwrap().model, "test-8b:latest");
        assert_eq!(p.pick("heavy").unwrap().status, Status::Installed);
    }

    #[test]
    fn code_goal_prefers_a_code_model() {
        let m = machine(16.0, Gpu::None);
        let g = Goals::build(&[], &["code".into()]).unwrap();
        let p = plan_with(&m, &g, false, &with_test_models(), Prefer::Quality);
        assert_eq!(p.pick("heavy").unwrap().model, "test-code-9b");
        // by default a CPU keeps to 8B active: the 9B code model is too slow
        let p = plan(&m, &g, false, &with_test_models());
        assert_ne!(p.pick("heavy").unwrap().model, "test-code-9b");
    }

    #[test]
    fn goals_come_from_parts_and_flags() {
        assert_eq!(goals().names(), vec!["chat", "notes"]);
        let g = Goals::build(&["notes", "rooms"], &["code,music".into()]).unwrap();
        assert_eq!(g.names(), vec!["code", "notes", "music", "rooms"]);
        assert!(Goals::build(&[], &["bogus".into()]).is_err());
        // music and rooms only: a quick lane and memory, no heavy model
        let g = Goals::build(&["music", "rooms"], &[]).unwrap();
        let p = plan(&machine(16.0, Gpu::None), &g, false, &real());
        assert!(p.pick("heavy").is_none() && p.pick("fast").is_some());
    }

    #[test]
    fn the_same_model_for_both_lanes_is_said_plainly() {
        let p = plan(&machine(16.0, Gpu::None), &goals(), false, &real());
        assert!(p
            .notes
            .iter()
            .any(|n| n.contains("serves quick answers and thinking")));
    }

    #[test]
    fn umbra_3_installed_takes_the_heavy_lane_when_it_fits() {
        let mut m = machine(32.0, Gpu::None);
        m.ollama.models = vec![Installed {
            name: "umbra-3:q4".into(),
            size_bytes: 8_000_000_000,
            digest: String::new(),
        }];
        m.local = vec![Local {
            family: Family::Umbra3,
            at: Where::OllamaTag("umbra-3:q4".into()),
            size_bytes: 8_000_000_000,
        }];
        let p = plan(&m, &goals(), false, &real());
        assert_eq!(p.pick("heavy").unwrap().model, "umbra-3:q4");
        // on 8 GB it does not fit (8 GB + 1.5 over the 5 GB budget): the public 4b is used
        let mut small = m.clone();
        small.ram_total_gb = Some(8.0);
        assert_eq!(
            plan(&small, &goals(), false, &real())
                .pick("heavy")
                .unwrap()
                .model,
            "qwen3:4b"
        );
    }

    // ---- the live-registry catalog (mocked: nothing here touches a network)

    use super::super::registry::tests::{cands, table, tmp, MockReg};
    use super::super::registry::{merge, resolve_within};
    use std::sync::Arc;
    use std::time::Duration;

    const NOW: u64 = 1_793_000_000;

    /// The shipped catalog plus everything the mocked registry verifies for these installed models.
    fn verified_catalog(installed: &[Installed]) -> Vec<Model> {
        let home = tmp("plan");
        let r = resolve_within(
            Some(Arc::new(MockReg::new(&table()))),
            &cands(),
            &home,
            NOW,
            true,
            Duration::from_secs(10),
        );
        let mut cat = real();
        merge(&mut cat, &cands(), &r.found, installed);
        let _ = std::fs::remove_dir_all(&home);
        cat
    }

    fn mac(ram: f64) -> Machine {
        machine(
            ram,
            Gpu::Apple {
                chip: "Apple M3".into(),
            },
        )
    }

    fn within_budget(m: &Machine, k: &Pick) -> bool {
        let b = budget(m);
        let limit = if k.run == Run::Gpu && m.gpu != Gpu::None {
            b.gpu
        } else {
            b.ram
        };
        let room = if k.role == "embed" { 0.0 } else { HEADROOM_GB };
        k.gb.is_none_or(|g| g + room <= limit.unwrap() + 1e-9)
    }

    #[test]
    fn mocked_registry_table_bigger_machines_get_bigger_models_within_budget() {
        let cat = verified_catalog(&[]);
        let rows: Vec<(&str, Machine)> = vec![
            ("4 GB", machine(4.0, Gpu::None)),
            ("8 GB", machine(8.0, Gpu::None)),
            ("16 GB", machine(16.0, Gpu::None)),
            ("32 GB + 12 GB card", machine(32.0, nvidia(12.0))),
            ("64 GB", machine(64.0, Gpu::None)),
            ("Apple 24 GB", mac(24.0)),
        ];
        let mut sizes = Vec::new();
        for (label, m) in &rows {
            let p = plan(m, &goals(), false, &cat);
            for k in &p.picks {
                assert!(within_budget(m, k), "{label}: {}", k.why);
                assert!(
                    k.why.contains(". Licence: ")
                        && k.why.contains("Verified with the Ollama registry today"),
                    "{label}: {}",
                    k.why
                );
            }
            let heavy = p.pick("heavy").map(|k| {
                (
                    k.model.clone(),
                    cat.iter()
                        .find(|c| c.name == k.catalog_name)
                        .and_then(|c| c.params_b)
                        .unwrap_or(0.0),
                )
            });
            println!(
                "{label}: heavy={heavy:?} fast={:?} embed={:?}",
                p.pick("fast").map(|k| &k.model),
                p.pick("embed").map(|k| &k.model)
            );
            sizes.push((label.to_string(), heavy));
        }
        // 4 GB: nothing big fits, but the memory model (0.6 GB) still does
        assert!(sizes[0].1.is_none());
        let p4 = plan(&rows[0].1, &goals(), false, &cat);
        assert!(p4.pick("heavy").is_none() && p4.pick("fast").is_none());
        assert_eq!(p4.pick("embed").unwrap().model, "embeddinggemma");
        let params = |i: usize| sizes[i].1.as_ref().map_or(0.0, |h| h.1);
        assert_eq!(sizes[1].1.as_ref().unwrap().0, "qwen3:4b");
        // 16 GB, no GPU: an 8B dense model, not the slower 14B
        assert_eq!(sizes[2].1.as_ref().unwrap().0, "qwen3:8b");
        assert_eq!(sizes[3].1.as_ref().unwrap().0, "qwen3:14b");
        assert_eq!(sizes[4].1.as_ref().unwrap().0, "qwen3:30b");
        for i in 1..5 {
            assert!(params(i) >= params(i - 1), "{sizes:?}");
        }
        assert!(params(4) > params(1) && params(2) > params(1));
        // the card is used when the model fits it
        let p = plan(&rows[3].1, &goals(), false, &cat);
        assert_eq!(p.pick("heavy").unwrap().run, Run::Gpu);
        assert!(p.pick("heavy").unwrap().why.contains("Apache License 2.0"));
        // Apple: the shared pool (24 GB gives an 18 GB budget); the biggest that fits, never over
        let pa = plan(&rows[5].1, &goals(), false, &cat);
        let h = pa.pick("heavy").unwrap();
        assert_eq!(h.model, "mistral-small:24b");
        assert!(h.gb.unwrap() + 1.5 <= 18.0);
        // the quick lane stays small everywhere (fetch of 3 GB or less)
        for (_, m) in &rows[1..] {
            let p = plan(m, &goals(), false, &cat);
            assert!(p.pick("fast").unwrap().gb.unwrap() <= FAST_MAX_GB);
        }
    }

    #[test]
    fn an_unverified_candidate_is_never_recommended_or_fetched() {
        // qwen3:32b and gpt-oss:120b are not in the mocked registry
        let cat = verified_catalog(&[]);
        for m in [machine(128.0, Gpu::None), machine(128.0, nvidia(48.0))] {
            let p = plan(&m, &goals(), true, &cat);
            for k in &p.picks {
                assert!(!["qwen3:32b", "gpt-oss:120b"].contains(&k.model.as_str()));
            }
            assert!(p
                .fetch
                .iter()
                .all(|f| !["qwen3:32b", "gpt-oss:120b"].contains(&f.name.as_str())));
        }
        // with no registry at all, only the shipped verified entries can be chosen
        let p = plan(&machine(64.0, Gpu::None), &goals(), false, &real());
        assert_eq!(p.pick("heavy").unwrap().model, "qwen3:4b");
        assert!(p
            .fetch
            .iter()
            .all(|f| ["qwen3:4b", "embeddinggemma"].contains(&f.name.as_str())));
        assert!(p.pick("heavy").unwrap().why.contains("not re-checked"));
        // the candidate list alone, without verification, adds nothing to the catalog
        let mut c = real();
        merge(&mut c, &cands(), &[], &[]);
        assert_eq!(c, real());
    }

    #[test]
    fn code_goal_uses_a_verified_code_model_and_chat_does_not_pick_one() {
        let cat = verified_catalog(&[]);
        let g = Goals::build(&[], &["code".into()]).unwrap();
        assert_eq!(
            plan(&machine(16.0, Gpu::None), &g, false, &cat)
                .pick("heavy")
                .unwrap()
                .model,
            "qwen2.5-coder:7b"
        );
        assert_eq!(
            plan(&machine(64.0, Gpu::None), &g, false, &cat)
                .pick("heavy")
                .unwrap()
                .model,
            "qwen3-coder:30b"
        );
        // plain chat never takes a code-only model
        let p = plan(&machine(64.0, Gpu::None), &goals(), false, &cat);
        assert_ne!(p.pick("heavy").unwrap().model, "qwen3-coder:30b");
    }

    fn unc_machine(ram: f64) -> Machine {
        let mut m = machine(ram, Gpu::None);
        m.preferred_prefix = "mine-".into();
        m.ollama.models = vec![Installed {
            name: "mine-sample-8b:latest".into(),
            size_bytes: 5_200_000_000,
            digest: String::new(),
        }];
        m
    }

    #[test]
    fn an_installed_preferred_model_is_chosen_and_mainstream_overrides_it() {
        let m = unc_machine(16.0);
        let cat = verified_catalog(&m.ollama.models);
        // default (auto): the preferred model takes both lanes, as installed UMBRA would
        let p = plan_with(&m, &goals(), false, &cat, Prefer::Auto);
        assert_eq!(p.pick("heavy").unwrap().model, "mine-sample-8b:latest");
        assert_eq!(p.pick("fast").unwrap().model, "mine-sample-8b:latest");
        assert!(p
            .pick("heavy")
            .unwrap()
            .why
            .contains("already installed, one of your preferred models"));
        assert!(
            p.fetch.iter().all(|f| !is_preferred(&f.name, "mine-")),
            "never fetched"
        );
        // explicit --prefer preferred: the same
        assert_eq!(
            plan_with(&m, &goals(), false, &cat, Prefer::Preferred)
                .pick("heavy")
                .unwrap()
                .model,
            "mine-sample-8b:latest"
        );
        // --prefer mainstream: left out, and it says so
        let p = plan_with(&m, &goals(), false, &cat, Prefer::Mainstream);
        assert_eq!(p.pick("heavy").unwrap().model, "qwen3:8b");
        assert!(p.picks.iter().all(|k| !is_preferred(&k.model, "mine-")));
        assert!(p
            .notes
            .iter()
            .any(|n| n.contains("left out because of --prefer mainstream")));
        // UMBRA still comes first
        let mut u = m.clone();
        u.ollama.models.push(Installed {
            name: "umbra-3:q4".into(),
            size_bytes: 8_000_000_000,
            digest: String::new(),
        });
        u.local = vec![Local {
            family: Family::Umbra3,
            at: Where::OllamaTag("umbra-3:q4".into()),
            size_bytes: 8_000_000_000,
        }];
        assert_eq!(
            plan_with(&u, &goals(), false, &cat, Prefer::Auto)
                .pick("heavy")
                .unwrap()
                .model,
            "umbra-3:q4"
        );
    }

    #[test]
    fn a_preferred_model_that_does_not_fit_is_not_chosen_and_none_installed_changes_nothing() {
        // 8 GB: budget 5 GB, the 5.2 GB model needs 6.7
        let m = unc_machine(8.0);
        let cat = verified_catalog(&m.ollama.models);
        let p = plan_with(&m, &goals(), false, &cat, Prefer::Auto);
        assert_eq!(p.pick("heavy").unwrap().model, "qwen3:4b");
        // none installed: auto and mainstream give the very same plan, with no note about it
        let bare = machine(16.0, Gpu::None);
        let cat = verified_catalog(&[]);
        let auto = plan_with(&bare, &goals(), false, &cat, Prefer::Auto);
        assert_eq!(
            auto,
            plan_with(&bare, &goals(), false, &cat, Prefer::Mainstream)
        );
        assert!(auto.notes.iter().all(|n| !n.contains("preferred")));
        // asking for preferred models with no prefix set says so
        let p = plan_with(&bare, &goals(), false, &cat, Prefer::Preferred);
        assert_eq!(p.pick("heavy").unwrap().model, "qwen3:8b");
        assert!(p
            .notes
            .iter()
            .any(|n| n.contains("no preferred prefix is set")));
        // with a prefix but none installed: says why mainstream was used
        let mut set = bare.clone();
        set.preferred_prefix = "mine-".into();
        let p = plan_with(&set, &goals(), false, &cat, Prefer::Preferred);
        assert_eq!(p.pick("heavy").unwrap().model, "qwen3:8b");
        assert!(p
            .notes
            .iter()
            .any(|n| n.contains("starts with mine-") && n.contains("never fetches them")));
        // quick lane takes the smallest preferred model, thinking the biggest
        let mut two = machine(32.0, Gpu::None);
        two.preferred_prefix = "mine-".into();
        two.ollama.models = vec![
            Installed {
                name: "mine-big-14b".into(),
                size_bytes: 9_000_000_000,
                digest: String::new(),
            },
            Installed {
                name: "mine-small-3b".into(),
                size_bytes: 2_000_000_000,
                digest: String::new(),
            },
            Installed {
                name: "mine-embed".into(),
                size_bytes: 300_000_000,
                digest: String::new(),
            },
        ];
        let p = plan_with(&two, &goals(), false, &cat, Prefer::Auto);
        assert_eq!(p.pick("fast").unwrap().model, "mine-small-3b");
        assert_eq!(p.pick("heavy").unwrap().model, "mine-big-14b");
        assert_ne!(p.pick("embed").unwrap().model, "mine-embed");
    }

    #[test]
    fn preferred_name_and_size_helpers() {
        assert!(is_preferred("mine-other:8b", "mine-") && is_preferred("hf.co/x/mine-y", "mine-"));
        assert!(!is_preferred("llama3-mine", "mine-") && !is_preferred("qwen3:4b", "mine-"));
        assert!(
            !is_preferred("mine-other:8b", ""),
            "an empty prefix prefers nothing"
        );
        assert_eq!(params_from_name("mine-sample-8b:latest"), Some(8.0));
        assert_eq!(params_from_name("mine-x:3.8b"), Some(3.8));
        assert_eq!(params_from_name("mine-sample"), None);
        assert_eq!(Prefer::parse("Mainstream"), Ok(Prefer::Mainstream));
        assert!(Prefer::parse("both").is_err());
    }

    // ---- speed on a CPU: the thinking lane is ranked by active parameters first

    fn heavy_of(m: &Machine, prefer: Prefer, g: &Goals) -> Pick {
        plan_with(m, g, false, &verified_catalog(&[]), prefer)
            .pick("heavy")
            .unwrap()
            .clone()
    }

    #[test]
    fn cpu_only_16gb_gets_an_8b_dense_model_not_a_14b() {
        let h = heavy_of(&machine(16.0, Gpu::None), Prefer::Auto, &goals());
        assert_eq!(h.model, "qwen3:8b");
        assert!(
            h.why
                .contains("8B or fewer active parameters, so it stays quick on CPU"),
            "{}",
            h.why
        );
        assert_eq!(h.run, Run::Cpu);
    }

    #[test]
    fn cpu_only_32gb_gets_the_30b_mixture_of_experts_and_says_why() {
        let h = heavy_of(&machine(32.0, Gpu::None), Prefer::Auto, &goals());
        assert_eq!(h.model, "qwen3:30b");
        assert!(
            h.why.starts_with(
                "32 GB RAM, no GPU: qwen3:30b (19.0 GB) for thinking and code, runs on CPU"
            ),
            "{}",
            h.why
        );
        assert!(
            h.why
                .contains("mixture of experts, about 3B active, so it runs fast on CPU"),
            "{}",
            h.why
        );
        assert!(h.gb.unwrap() + HEADROOM_GB <= 24.0);
    }

    #[test]
    fn prefer_quality_lifts_the_cap_on_cpu_and_says_it_is_slower() {
        let m = machine(32.0, Gpu::None);
        let p = plan_with(&m, &goals(), false, &verified_catalog(&[]), Prefer::Quality);
        let h = p.pick("heavy").unwrap();
        // the biggest model that fits (a 32B dense one; qwen3:32b is not in the fixture registry)
        assert_eq!(h.model, "deepseek-r1:32b");
        assert!(
            h.why.contains("32B active parameters: slower on CPU"),
            "{}",
            h.why
        );
        assert!(p
            .notes
            .iter()
            .any(|n| n.contains("--prefer quality") && n.contains("more slowly")));
        // on 16 GB it gives the 14B that the default rule leaves out
        assert_eq!(
            heavy_of(&machine(16.0, Gpu::None), Prefer::Quality, &goals()).model,
            "qwen3:14b"
        );
        // the quick lane is unchanged by it
        assert_eq!(
            plan_with(&m, &goals(), false, &verified_catalog(&[]), Prefer::Quality)
                .pick("fast")
                .unwrap()
                .model,
            "qwen3:4b"
        );
    }

    #[test]
    fn gpu_machines_keep_the_biggest_that_fits_the_card() {
        // 12 GB card: the dense 14B (card budget 11), no speed note
        let h = heavy_of(&machine(32.0, nvidia(12.0)), Prefer::Auto, &goals());
        assert_eq!((h.model.as_str(), h.run), ("qwen3:14b", Run::Gpu));
        assert!(!h.why.contains("on CPU"), "{}", h.why);
        // 24 GB card: the biggest that fits, a dense 32B, unchanged by the cap
        let h = heavy_of(&machine(32.0, nvidia(24.0)), Prefer::Auto, &goals());
        assert_eq!((h.model.as_str(), h.run), ("deepseek-r1:32b", Run::Gpu));
        // a card too small for anything falls back to the CPU rule
        let h = heavy_of(&machine(32.0, nvidia(4.0)), Prefer::Auto, &goals());
        assert_eq!((h.model.as_str(), h.run), ("qwen3:30b", Run::Cpu));
        assert!(h.why.contains("too big for the graphics card's memory"));
        // Apple shared memory counts as a GPU
        assert_eq!(
            heavy_of(&mac(24.0), Prefer::Auto, &goals()).model,
            "mistral-small:24b"
        );
    }

    #[test]
    fn active_parameters_come_from_the_candidate_file() {
        let c = cands();
        let a = |n: &str| c.iter().find(|x| x.name == n).unwrap().active_params_b;
        assert_eq!(a("qwen3:30b"), Some(3.3));
        assert_eq!(a("qwen3-coder:30b"), Some(3.3));
        assert_eq!(a("gpt-oss:20b"), Some(3.6));
        assert_eq!(a("gpt-oss:120b"), Some(5.1));
        assert_eq!(a("qwen3:14b"), None);
        assert_eq!(Prefer::parse("quality"), Ok(Prefer::Quality));
    }
}
