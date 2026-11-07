//! The band spec (a band is what the Tempo Conductor sets up): `<Tempo home>/bands/<name>/band.json`.
//!
//! A band is a few AI members and the flows they work through. The spec is plain data, checked
//! strictly: an unknown key, a wrong type, an overlong role or a flow that points at nobody is a
//! problem, and every problem is reported in words (never just the first one). Nothing in a spec
//! can give a member a tool: members chat as guests and the launcher writes their notes.

use crate::setup::json::{parse, J};
use crate::setup::propose::model_name_ok;

/// The longest role, in characters (the agent and Room take the same).
pub const MAX_ROLE: usize = 2000;
pub const MAX_MEMBERS: usize = 8;
pub const MAX_FLOWS: usize = 4;
pub const MAX_STEPS: usize = 8;
pub const MAX_GOAL: usize = 600;

/// Words that would pass for Tempo, the room or the owner in Room.
const RESERVED_HANDLES: [&str; 8] = [
    "tempo", "room", "owner", "everyone", "all", "here", "channel", "band",
];

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const DAY_WORDS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// What a member may do. Both kinds are chat only; the difference is the vault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum May {
    /// Answers when spoken to in Room. Never a step in a flow.
    Talk,
    /// Also writes drafts: its flow output becomes a note in the vault.
    Draft,
}

impl May {
    pub fn text(self) -> &'static str {
        match self {
            May::Talk => "talk",
            May::Draft => "draft",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    /// What you type after the @ in Room.
    pub handle: String,
    pub name: String,
    /// A fun one-word title: Metronome, Piccolo, Tuning-fork.
    pub instrument: String,
    pub role: String,
    /// A model from this computer's plan. Empty: whatever the agent uses by default.
    pub model: String,
    /// The engine the Conductor picked to play this part (an id from `conductor engines`).
    /// Empty: the agent's own order.
    pub engine: String,
    pub may: May,
}

/// When a flow runs by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Every {
    Manual,
    Daily(u8, u8),
    /// Day (0 is Monday), hour, minute.
    Weekly(u8, u8, u8),
}

impl Every {
    /// `manual`, `daily 09:00` or `weekly mon 09:00` (full day names work too).
    pub fn parse(s: &str) -> Result<Every, String> {
        let bad = || {
            format!(
                "\"{s}\" is not a schedule: use manual, daily HH:MM (like daily 08:30) or weekly <day> HH:MM (like weekly mon 09:00)"
            )
        };
        let low = s.trim().to_ascii_lowercase();
        let w: Vec<&str> = low.split_whitespace().collect();
        let clock = |t: &str| -> Option<(u8, u8)> {
            let (h, m) = t.split_once(':')?;
            if h.len() != 2 || m.len() != 2 {
                return None;
            }
            let (h, m): (u8, u8) = (h.parse().ok()?, m.parse().ok()?);
            (h < 24 && m < 60).then_some((h, m))
        };
        match w.as_slice() {
            ["manual"] => Ok(Every::Manual),
            ["daily", t] => clock(t).map(|(h, m)| Every::Daily(h, m)).ok_or_else(bad),
            ["weekly", d, t] => {
                let day = DAYS
                    .iter()
                    .zip(DAY_WORDS)
                    .position(|(short, long)| *d == *short || *d == long.to_ascii_lowercase())
                    .ok_or_else(bad)?;
                clock(t)
                    .map(|(h, m)| Every::Weekly(day as u8, h, m))
                    .ok_or_else(bad)
            }
            _ => Err(bad()),
        }
    }

    /// The text kept in band.json.
    pub fn text(&self) -> String {
        match *self {
            Every::Manual => "manual".into(),
            Every::Daily(h, m) => format!("daily {h:02}:{m:02}"),
            Every::Weekly(d, h, m) => format!("weekly {} {h:02}:{m:02}", DAYS[d as usize % 7]),
        }
    }

    /// For people.
    pub fn words(&self) -> String {
        match *self {
            Every::Manual => "only when you run it".into(),
            Every::Daily(h, m) => format!("every day at {h:02}:{m:02}"),
            Every::Weekly(d, h, m) => {
                format!("every {} at {h:02}:{m:02}", DAY_WORDS[d as usize % 7])
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Flow {
    pub name: String,
    /// What the flow is for; every step is told it.
    pub goal: String,
    /// Handles, in order.
    pub steps: Vec<String>,
    pub every: Every,
    /// The vault folder the notes go to: `Bands/<band>/` or a folder inside it.
    pub notes_to: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    pub name: String,
    pub about: String,
    pub members: Vec<Member>,
    pub flows: Vec<Flow>,
}

impl Band {
    pub fn member(&self, handle: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.handle == handle)
    }

    pub fn flow(&self, name: &str) -> Option<&Flow> {
        self.flows.iter().find(|f| f.name == name)
    }
}

/// The folder a band's notes go to unless a flow says otherwise.
pub fn default_notes_to(band: &str) -> String {
    format!("Bands/{band}/")
}

// ---- checks

/// Lower case letters, digits and single hyphens inside; 2 to `max` characters.
pub fn slug_ok(s: &str, max: usize) -> bool {
    let n = s.chars().count();
    (2..=max).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

/// The shape of an engine id (`local`, `192.168.0.20:11434`, a host's name). Whether it names an
/// engine this computer knows is checked where the engines are known (`engines::unknown`).
pub fn engine_id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b))
}

/// Text for one line: no control characters, not blank, at most `max` characters.
fn line_ok(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
}

/// Longer text: line breaks and tabs are fine, other control characters are not.
fn text_ok(s: &str, max: usize) -> Result<(), String> {
    let n = s.chars().count();
    if s.trim().is_empty() {
        return Err("is empty".into());
    }
    if n > max {
        return Err(format!("is {n} characters; the most is {max}"));
    }
    if s.chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err("holds a control character".into());
    }
    Ok(())
}

fn notes_to_problem(band: &str, p: &str) -> Option<String> {
    let base = default_notes_to(band);
    let Some(rest) = p.strip_prefix(&base) else {
        return Some(format!(
            "notes_to must be {base} or a folder inside it (it is \"{p}\")"
        ));
    };
    if !p.ends_with('/') {
        return Some("notes_to must end with a /".into());
    }
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let ok = parts.len() <= 3
        && !p.contains("//")
        && parts.iter().all(|s| {
            s.chars().count() <= 40
                && !s.starts_with('.')
                && !s.starts_with(' ')
                && !s.ends_with(' ')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        });
    (!ok).then(|| {
        format!("notes_to \"{p}\": folders inside {base} use letters, digits, spaces, - and _ only, at most three deep")
    })
}

/// Every problem with a band, in words. Empty means the band is fine.
pub fn problems(b: &Band) -> Vec<String> {
    let mut p = Vec::new();
    if !slug_ok(&b.name, 40) {
        p.push(format!(
            "the band's name \"{}\" must be 2 to 40 lowercase letters, digits or hyphens (like bakery-posts)",
            b.name
        ));
    }
    if !line_ok(&b.about, 200) {
        p.push("about must be one line of 1 to 200 characters".into());
    }
    if !(2..=MAX_MEMBERS).contains(&b.members.len()) {
        p.push(format!(
            "a band has 2 to {MAX_MEMBERS} members (this one has {})",
            b.members.len()
        ));
    }
    for (i, m) in b.members.iter().enumerate() {
        let at = format!("member {} (@{})", i + 1, m.handle);
        if !slug_ok(&m.handle, 24) {
            p.push(format!(
                "{at}: a handle is 2 to 24 lowercase letters, digits or hyphens"
            ));
        } else if RESERVED_HANDLES.contains(&m.handle.as_str()) {
            p.push(format!(
                "{at}: @{} would pass for Tempo, the room or you; pick another",
                m.handle
            ));
        }
        if b.members[..i].iter().any(|o| o.handle == m.handle) {
            p.push(format!("{at}: another member already has that handle"));
        }
        if !line_ok(&m.name, 40) {
            p.push(format!(
                "{at}: the name must be one line of 1 to 40 characters"
            ));
        } else if b.members[..i]
            .iter()
            .any(|o| o.name.eq_ignore_ascii_case(&m.name))
        {
            p.push(format!(
                "{at}: another member already has the name {}",
                m.name
            ));
        }
        let word = m.instrument.chars().count();
        if !(2..=24).contains(&word) || !m.instrument.chars().all(|c| c.is_alphabetic() || c == '-')
        {
            p.push(format!(
                "{at}: the instrument is one word of 2 to 24 letters (like Metronome)"
            ));
        }
        if let Err(e) = text_ok(&m.role, MAX_ROLE) {
            p.push(format!("{at}: the role {e}"));
        }
        if !m.engine.is_empty() && !engine_id_ok(&m.engine) {
            p.push(format!(
                "{at}: the engine is an id from `tempo-launch conductor engines` (letters, digits and . : _ - only, at most 64 characters)"
            ));
        }
        if !m.model.is_empty() && !model_name_ok(&m.model) {
            p.push(format!(
                "{at}: the model is letters, digits and : . - _ / only, at most 100 characters"
            ));
        }
    }
    if !(1..=MAX_FLOWS).contains(&b.flows.len()) {
        p.push(format!(
            "a band has 1 to {MAX_FLOWS} flows (this one has {})",
            b.flows.len()
        ));
    }
    for (i, f) in b.flows.iter().enumerate() {
        let at = format!("flow {} ({})", i + 1, f.name);
        if !slug_ok(&f.name, 40) {
            p.push(format!(
                "{at}: a flow's name is 2 to 40 lowercase letters, digits or hyphens (like weekly-plan)"
            ));
        }
        if b.flows[..i].iter().any(|o| o.name == f.name) {
            p.push(format!("{at}: another flow already has that name"));
        }
        if let Err(e) = text_ok(&f.goal, MAX_GOAL) {
            p.push(format!("{at}: the goal {e}"));
        }
        if !(1..=MAX_STEPS).contains(&f.steps.len()) {
            p.push(format!("{at}: a flow has 1 to {MAX_STEPS} steps"));
        }
        for h in &f.steps {
            match b.member(h) {
                None => p.push(format!("{at}: step @{h} is not a member of this band")),
                Some(m) if m.may != May::Draft => p.push(format!(
                    "{at}: @{h} may only talk, so it cannot be a step (set may to draft)"
                )),
                Some(_) => {}
            }
        }
        if let Some(e) = notes_to_problem(&b.name, &f.notes_to) {
            p.push(format!("{at}: {e}"));
        }
    }
    p
}

// ---- reading and writing

fn only_keys(j: &J, allowed: &[&str], at: &str, p: &mut Vec<String>) {
    if let J::Obj(o) = j {
        for (k, _) in o {
            if !allowed.contains(&k.as_str()) {
                p.push(format!("{at}: \"{k}\" is not something a band has"));
            }
        }
    }
}

fn text_of(o: &J, key: &str, at: &str, p: &mut Vec<String>) -> String {
    match o.get(key) {
        Some(J::Str(s)) => s.clone(),
        Some(_) => {
            p.push(format!("{at}: {key} must be text"));
            String::new()
        }
        None => String::new(),
    }
}

fn list_of<'a>(o: &'a J, key: &str, at: &str, p: &mut Vec<String>) -> &'a [J] {
    match o.get(key) {
        Some(J::Arr(a)) => a,
        Some(_) => {
            p.push(format!("{at}: {key} must be a list"));
            &[]
        }
        None => &[],
    }
}

fn member_from(j: &J, i: usize, p: &mut Vec<String>) -> Member {
    let at = format!("member {}", i + 1);
    if !matches!(j, J::Obj(_)) {
        p.push(format!("{at} must be an object"));
    }
    only_keys(
        j,
        &[
            "handle",
            "name",
            "instrument",
            "role",
            "model",
            "engine",
            "may",
        ],
        &at,
        p,
    );
    let may = match j.get("may") {
        Some(J::Str(s)) if s == "talk" => May::Talk,
        Some(J::Str(s)) if s == "draft" => May::Draft,
        _ => {
            p.push(format!("{at}: may must be \"talk\" or \"draft\""));
            May::Talk
        }
    };
    Member {
        handle: text_of(j, "handle", &at, p),
        name: text_of(j, "name", &at, p),
        instrument: text_of(j, "instrument", &at, p),
        role: text_of(j, "role", &at, p),
        model: text_of(j, "model", &at, p),
        engine: text_of(j, "engine", &at, p),
        may,
    }
}

fn flow_from(j: &J, i: usize, band: &str, p: &mut Vec<String>) -> Flow {
    let at = format!("flow {}", i + 1);
    if !matches!(j, J::Obj(_)) {
        p.push(format!("{at} must be an object"));
    }
    only_keys(j, &["name", "goal", "steps", "every", "notes_to"], &at, p);
    let steps = list_of(j, "steps", &at, p)
        .iter()
        .map(|s| match s {
            J::Str(h) => h.clone(),
            _ => {
                p.push(format!("{at}: each step is a member's handle, as text"));
                String::new()
            }
        })
        .collect();
    let every = match j.get("every") {
        Some(J::Str(s)) => Every::parse(s).unwrap_or_else(|e| {
            p.push(format!("{at}: {e}"));
            Every::Manual
        }),
        _ => {
            p.push(format!(
                "{at}: every is needed (manual, daily HH:MM or weekly <day> HH:MM)"
            ));
            Every::Manual
        }
    };
    let notes_to = match j.get("notes_to") {
        None => default_notes_to(band),
        Some(_) => text_of(j, "notes_to", &at, p),
    };
    Flow {
        name: text_of(j, "name", &at, p),
        goal: text_of(j, "goal", &at, p),
        steps,
        every,
        notes_to,
    }
}

/// A band from JSON, strictly: every problem comes back, not only the first.
pub fn from_j(j: &J) -> Result<Band, Vec<String>> {
    let mut p = Vec::new();
    if !matches!(j, J::Obj(_)) {
        return Err(vec!["a band is a JSON object".into()]);
    }
    only_keys(
        j,
        &["version", "name", "about", "members", "flows"],
        "the band",
        &mut p,
    );
    if let Some(v) = j.get("version") {
        if !matches!(v, J::Num(n) if n == "1") {
            p.push("version must be 1".into());
        }
    }
    let name = text_of(j, "name", "the band", &mut p);
    let about = text_of(j, "about", "the band", &mut p);
    let members: Vec<Member> = list_of(j, "members", "the band", &mut p)
        .iter()
        .enumerate()
        .map(|(i, m)| member_from(m, i, &mut p))
        .collect();
    let flows: Vec<Flow> = list_of(j, "flows", "the band", &mut p)
        .iter()
        .enumerate()
        .map(|(i, f)| flow_from(f, i, &name, &mut p))
        .collect();
    let band = Band {
        name,
        about,
        members,
        flows,
    };
    p.extend(problems(&band));
    if p.is_empty() {
        Ok(band)
    } else {
        Err(p)
    }
}

/// A band's JSON without any member's `engine`: the Conductor picks engines itself, so a draft or
/// a proposal written by an AI never gets to name one.
pub fn without_engines(mut j: J) -> J {
    if let J::Obj(o) = &mut j {
        if let Some((_, J::Arr(members))) = o.iter_mut().find(|(k, _)| k == "members") {
            for m in members {
                if let J::Obj(fields) = m {
                    fields.retain(|(k, _)| k != "engine");
                }
            }
        }
    }
    j
}

pub fn from_json(text: &str) -> Result<Band, Vec<String>> {
    match parse(text) {
        Ok(j) => from_j(&j),
        Err(e) => Err(vec![format!("not valid JSON ({e})")]),
    }
}

impl Band {
    pub fn to_j(&self) -> J {
        let s = J::s;
        let members = self
            .members
            .iter()
            .map(|m| {
                let mut o = vec![
                    ("handle".into(), s(&m.handle)),
                    ("name".into(), s(&m.name)),
                    ("instrument".into(), s(&m.instrument)),
                    ("role".into(), s(&m.role)),
                    ("model".into(), s(&m.model)),
                ];
                if !m.engine.is_empty() {
                    o.push(("engine".into(), s(&m.engine)));
                }
                o.push(("may".into(), s(m.may.text())));
                J::Obj(o)
            })
            .collect();
        let flows = self
            .flows
            .iter()
            .map(|f| {
                J::Obj(vec![
                    ("name".into(), s(&f.name)),
                    ("goal".into(), s(&f.goal)),
                    ("steps".into(), J::strings(&f.steps)),
                    ("every".into(), s(&f.every.text())),
                    ("notes_to".into(), s(&f.notes_to)),
                ])
            })
            .collect();
        J::Obj(vec![
            ("version".into(), J::n(1.0)),
            ("name".into(), s(&self.name)),
            ("about".into(), s(&self.about)),
            ("members".into(), J::Arr(members)),
            ("flows".into(), J::Arr(flows)),
        ])
    }

    pub fn to_json(&self) -> String {
        self.to_j().pretty()
    }
}

// ---- in words

/// What a member does, for a card or a list: the first sentence of its role that says more than
/// "You are the ..." (which only names the job), shortened.
pub fn what_it_does(role: &str) -> String {
    let flat = role.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut sentences = Vec::new();
    let mut from = 0;
    for (i, c) in flat.char_indices() {
        if matches!(c, '.' | '!' | '?') && flat[i + 1..].starts_with(' ') {
            sentences.push(&flat[from..=i]);
            from = i + 2;
        }
    }
    sentences.push(&flat[from.min(flat.len())..]);
    let one = sentences
        .iter()
        .find(|s| !s.starts_with("You are "))
        .or(sentences.first())
        .copied()
        .unwrap_or("");
    if one.chars().count() > 160 {
        format!("{}...", one.chars().take(157).collect::<String>())
    } else {
        one.to_string()
    }
}

/// The spec as a person would read it: members, what each does, flows, when they run, and where
/// the notes go.
pub fn in_words(b: &Band, vault_ready: bool) -> String {
    let mut o = format!("The band \"{}\": {}\n\nMembers\n", b.name, b.about);
    for m in &b.members {
        let model = if m.model.is_empty() {
            "the agent's usual model".to_string()
        } else {
            m.model.clone()
        };
        let may = match m.may {
            May::Talk => "talks with you in Room",
            May::Draft => "talks with you and writes drafts",
        };
        let on = if m.engine.is_empty() {
            String::new()
        } else {
            format!(" on engine {}", m.engine)
        };
        o.push_str(&format!(
            "  @{}  {}, the {} ({may}; {model}{on})\n      {}\n",
            m.handle,
            m.name,
            m.instrument,
            what_it_does(&m.role)
        ));
    }
    o.push_str("\nFlows\n");
    for f in &b.flows {
        let who: Vec<String> = f.steps.iter().map(|h| format!("@{h}")).collect();
        o.push_str(&format!(
            "  {}: {}\n      {}\n      runs {}; notes go to {} in your Vault{}\n",
            f.name,
            who.join(" then "),
            f.goal.split_whitespace().collect::<Vec<_>>().join(" "),
            f.every.words(),
            f.notes_to,
            if vault_ready {
                ""
            } else {
                " (no Vault is set up yet, so they go to the band's own runs folder)"
            }
        ));
    }
    o.push_str(
        "\nNothing here leaves this computer. Members only talk and write drafts into your Vault: they never post, send, buy or delete anything.\n",
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> Band {
        let m = |h: &str, n: &str, may| Member {
            handle: h.into(),
            name: n.into(),
            instrument: "Piccolo".into(),
            role: "You write short posts. Keep them warm.".into(),
            model: "qwen3:4b".into(),
            engine: String::new(),
            may,
        };
        Band {
            name: "bakery-posts".into(),
            about: "Weekly posts for a bakery.".into(),
            members: vec![
                m("writer", "Quill", May::Draft),
                m("editor", "Polish", May::Draft),
                m("coach", "Cowbell", May::Talk),
            ],
            flows: vec![Flow {
                name: "weekly-plan".into(),
                goal: "Plan next week.".into(),
                steps: vec!["writer".into(), "editor".into()],
                every: Every::Weekly(0, 9, 0),
                notes_to: "Bands/bakery-posts/".into(),
            }],
        }
    }

    fn with(f: impl FnOnce(&mut Band)) -> Vec<String> {
        let mut b = sample();
        f(&mut b);
        problems(&b)
    }

    #[test]
    fn a_good_band_round_trips_through_json() {
        let b = sample();
        assert!(problems(&b).is_empty());
        let back = from_json(&b.to_json()).unwrap();
        assert_eq!(back, b);
        assert!(b.to_json().contains("\"every\": \"weekly mon 09:00\""));
    }

    #[test]
    fn an_engine_is_kept_per_member_and_left_out_when_there_is_none() {
        let mut b = sample();
        assert!(!b.to_json().contains("\"engine\""));
        b.members[0].engine = "192.168.0.20:11434".into();
        let text = b.to_json();
        assert!(text.contains("\"engine\": \"192.168.0.20:11434\""));
        assert_eq!(from_json(&text).unwrap(), b);
        assert!(in_words(&b, true).contains("on engine 192.168.0.20:11434"));
        for bad in ["a b", "x;y", "a/b", &"x".repeat(65)] {
            assert!(with(|b| b.members[0].engine = bad.into())[0].contains("the engine is an id"));
        }
        assert!(engine_id_ok("local") && engine_id_ok("gpu-box") && engine_id_ok("10.0.0.5:11434"));
        let stripped = without_engines(b.to_j());
        assert!(from_j(&stripped).unwrap().members[0].engine.is_empty());
        let wrong = crate::conductor::engines::unknown(&b, &["local".to_string()]);
        assert!(wrong[0].contains("@writer") && wrong[0].contains("not one of your engines"));
    }

    #[test]
    fn schedules_are_strict() {
        assert_eq!(Every::parse("manual"), Ok(Every::Manual));
        assert_eq!(Every::parse("Daily 08:30"), Ok(Every::Daily(8, 30)));
        assert_eq!(
            Every::parse("weekly Friday 16:00"),
            Ok(Every::Weekly(4, 16, 0))
        );
        assert_eq!(Every::Weekly(6, 7, 5).text(), "weekly sun 07:05");
        assert_eq!(Every::Weekly(0, 9, 0).words(), "every Monday at 09:00");
        for bad in [
            "",
            "daily",
            "daily 8:30",
            "daily 24:00",
            "daily 12:60",
            "weekly 09:00",
            "weekly someday 09:00",
            "hourly",
            "daily 08:30 extra",
            "every day",
        ] {
            assert!(Every::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bad_names_handles_and_roles_are_each_reported() {
        assert!(with(|b| b.name = "Bakery Posts".into())[0].contains("band's name"));
        assert!(with(|b| b.name = "a".into())[0].contains("band's name"));
        assert!(with(|b| b.about = "two\nlines".into())[0].contains("about"));
        assert!(with(|b| b.members[0].handle = "Quill".into())[0].contains("handle"));
        assert!(with(|b| b.members[0].handle = "tempo".into())[0].contains("pass for Tempo"));
        assert!(with(|b| b.members[1].handle = "writer".into())
            .iter()
            .any(|p| p.contains("already has that handle")));
        assert!(with(|b| b.members[1].name = "QUILL".into())
            .iter()
            .any(|p| p.contains("already has the name")));
        assert!(with(|b| b.members[0].instrument = "two words".into())[0].contains("instrument"));
        assert!(with(|b| b.members[0].role = "x".repeat(2001))[0].contains("2001 characters"));
        assert!(with(|b| b.members[0].role = "  ".into())[0].contains("is empty"));
        assert!(with(|b| b.members[0].role = "a\u{7}b".into())[0].contains("control"));
        assert!(with(|b| b.members[0].model = "bad name; rm".into())[0].contains("model"));
        assert!(with(|b| b.members[0].role = "x".repeat(2000)).is_empty());
        // many problems at once are all listed
        let all = with(|b| {
            b.members[0].handle = "X".into();
            b.members[0].role.clear();
            b.flows[0].steps.push("ghost".into());
        });
        assert!(all.len() >= 3, "{all:?}");
    }

    #[test]
    fn counts_and_flow_rules() {
        assert!(with(|b| b.members.truncate(1))[0].contains("2 to 8 members"));
        assert!(with(|b| b.flows.clear())[0].contains("1 to 4 flows"));
        assert!(with(|b| b.flows[0].steps.clear())[0].contains("1 to 8 steps"));
        assert!(with(|b| b.flows[0].steps = vec!["ghost".into()])[0].contains("not a member"));
        // a member who may only talk is not a step
        assert!(with(|b| b.flows[0].steps = vec!["coach".into()])[0].contains("may only talk"));
        assert!(with(|b| b.flows[0].goal = "x".repeat(601))[0].contains("goal"));
        assert!(with(|b| b.flows[0].name = "Weekly Plan".into())[0].contains("flow's name"));
        let dup = with(|b| {
            let f = b.flows[0].clone();
            b.flows.push(f);
        });
        assert!(dup.iter().any(|p| p.contains("already has that name")));
    }

    #[test]
    fn notes_stay_inside_the_bands_folder() {
        for bad in [
            "Bands/other/",
            "Bands/bakery-posts",
            "Notes/",
            "/Bands/bakery-posts/",
            "Bands/bakery-posts/../x/",
            "Bands/bakery-posts/.hidden/",
            "Bands/bakery-posts//x/",
            "Bands/bakery-posts/a\\b/",
            "Bands/bakery-posts/a/b/c/d/",
            "C:/Bands/bakery-posts/",
        ] {
            let p = with(|b| b.flows[0].notes_to = bad.into());
            assert!(!p.is_empty(), "{bad} was accepted");
        }
        for good in [
            "Bands/bakery-posts/",
            "Bands/bakery-posts/Weekly plans/",
            "Bands/bakery-posts/a/b/",
        ] {
            assert!(
                with(|b| b.flows[0].notes_to = good.into()).is_empty(),
                "{good}"
            );
        }
    }

    #[test]
    fn json_is_read_strictly() {
        let good = sample().to_json();
        assert!(from_json(&good).is_ok());
        let e = from_json("[1]").unwrap_err();
        assert!(e[0].contains("object"));
        assert!(from_json("{nope").unwrap_err()[0].contains("not valid JSON"));
        // an unknown key anywhere, a wrong type, a bad version
        let extra = good.replacen("\"about\"", "\"tools\": [\"post\"],\n  \"about\"", 1);
        assert!(from_json(&extra).unwrap_err()[0].contains("\"tools\" is not something"));
        let mem = good.replacen("\"instrument\"", "\"can_send\": true,\n \"instrument\"", 1);
        assert!(from_json(&mem).unwrap_err()[0].contains("\"can_send\""));
        let typed = good.replacen(
            "\"role\": \"You write",
            "\"role\": 7, \"x\": \"You write",
            1,
        );
        assert!(from_json(&typed).is_err());
        let ver = good.replacen("\"version\": 1", "\"version\": 2", 1);
        assert!(from_json(&ver).unwrap_err()[0].contains("version"));
        let may = good.replacen("\"may\": \"draft\"", "\"may\": \"send\"", 1);
        assert!(from_json(&may).unwrap_err()[0].contains("may must be"));
        let every = good.replacen("weekly mon 09:00", "whenever", 1);
        assert!(from_json(&every).unwrap_err()[0].contains("not a schedule"));
        // notes_to may be left out: it defaults to the band's folder
        let none = good.replace("\"notes_to\": \"Bands/bakery-posts/\"", "\"x\": 1");
        assert!(from_json(&none).is_err());
        let j = parse(&good).unwrap();
        let mut flows = j.arr("flows").to_vec();
        if let J::Obj(o) = &mut flows[0] {
            o.retain(|(k, _)| k != "notes_to");
        }
        let mut j2 = j.clone();
        j2.set("flows", J::Arr(flows));
        assert_eq!(
            from_j(&j2).unwrap().flows[0].notes_to,
            "Bands/bakery-posts/"
        );
    }

    #[test]
    fn the_plain_words_name_everyone_and_the_promise() {
        let w = in_words(&sample(), true);
        for want in [
            "bakery-posts",
            "@writer  Quill, the Piccolo",
            "You write short posts.",
            "@writer then @editor",
            "every Monday at 09:00",
            "Bands/bakery-posts/ in your Vault",
            "never post, send, buy or delete",
        ] {
            assert!(w.contains(want), "{want}\n{w}");
        }
        assert!(!w.contains("no Vault is set up"));
        assert!(in_words(&sample(), false).contains("no Vault is set up yet"));
        assert_eq!(what_it_does("One. Two."), "One.");
        assert_eq!(what_it_does("No stop"), "No stop");
        assert_eq!(
            what_it_does("You are the editor of a team. You fix drafts. You never add facts."),
            "You fix drafts."
        );
        assert_eq!(what_it_does("You are alone here."), "You are alone here.");
        assert_eq!(what_it_does(""), "");
        assert!(what_it_does(&"word ".repeat(80)).ends_with("..."));
    }
}
