//! Agent designs are data. The model can propose a role, prompt, narrowed tools, practice cases,
//! and a budget; this module decides whether that proposal is safe to keep.

use crate::conductor::budget::{Budget, Meter};
use crate::conductor::runlog::{Ev, RunLog};
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::Ctx;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod code_check;
pub mod exam;
#[cfg(test)]
mod provider_tests;
mod runner;
mod runtime;
use runtime::{infer, infer_for_sections, Usage};
mod task;
mod team;

const MAX_TOKENS: u64 = 16_000;
const MAX_REQUEST_TOKENS: u64 = 2_048;
const MAX_SECONDS: u64 = 600;
const DESIGN_MAX_TOKENS: u64 = 4_096;
const DESIGN_MAX_OUTPUT: u64 = 1_024;
const DESIGN_MAX_SECONDS: u64 = 180;
const MAX_MODEL_REPLY: usize = 64_000;
pub const BASELINE_PROMPT: &str = "Complete the task accurately. Follow the requested format. Use only the supplied information. Say when the evidence is insufficient.";
pub const HOST_GUARD: &str = "Use only supplied local data and fixed owner checks. Do not send, post, buy, delete, execute generated commands, change routes, or widen tools.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Research,
    Code,
    Content,
}

impl Role {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "research" => Ok(Self::Research),
            "code" => Ok(Self::Code),
            "content" => Ok(Self::Content),
            _ => Err("role must be research, code, or content".into()),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Code => "code",
            Self::Content => "content",
        }
    }

    pub fn owner_tools(self) -> &'static [&'static str] {
        match self {
            Self::Research => &["sections.read", "woodshed.facts"],
            Self::Code => &["budget.patch"],
            Self::Content => &["product.facts"],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetSpec {
    pub tokens: u64,
    pub request_tokens: u64,
    pub seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PracticeCase {
    pub task: String,
    pub rubric: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Design {
    pub role: Role,
    pub name: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub guards: Vec<String>,
    pub practice: Vec<PracticeCase>,
    pub budget: BudgetSpec,
}

impl Design {
    /// The immutable owner guard comes from the host, not from model-authored prompt text.
    pub fn runtime_system_prompt(&self) -> String {
        let role_guardrails = self
            .guards
            .iter()
            .map(|guard| format!("- {guard}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "{}\n\nRole guardrails:\n{}\n\nOwner guard: {}",
            self.system_prompt, role_guardrails, HOST_GUARD
        )
    }
}

/// Parse one exact design object. Unknown fields, duplicate keys, coercions, and widened tools
/// reject the whole proposal; callers should keep the raw answer in a rejection receipt.
pub fn validate_design(text: &str, requested: Role) -> Result<Design, String> {
    let value = parse(text).map_err(|e| format!("design is not valid JSON: {e}"))?;
    exact_fields(
        &value,
        &[
            "role",
            "name",
            "system_prompt",
            "tools",
            "guards",
            "practice",
            "budget",
        ],
    )?;
    let role = Role::parse(required_str(&value, "role")?)?;
    if role != requested {
        return Err("design changed the requested role".into());
    }
    let name = required_str(&value, "name")?.to_string();
    if !slug(&name) {
        return Err("name must be a short lowercase slug".into());
    }
    let system_prompt = bounded_prompt(required_str(&value, "system_prompt")?)?;
    let tools = string_list(
        value.get("tools").ok_or("tools is required")?,
        0,
        4,
        "tools",
    )?;
    let allowed = role.owner_tools();
    if tools.iter().any(|tool| !allowed.contains(&tool.as_str())) {
        return Err("design asked for a tool outside this role's owner allowlist".into());
    }
    let guards = string_list(
        value.get("guards").ok_or("guards is required")?,
        2,
        6,
        "guards",
    )?;
    let practice_value = value.get("practice").ok_or("practice is required")?;
    let practice_rows = match practice_value {
        J::Arr(rows) if (1..=3).contains(&rows.len()) => rows,
        J::Arr(_) => return Err("practice must have 1 to 3 cases".into()),
        _ => return Err("practice must be an array".into()),
    };
    let mut practice = Vec::with_capacity(practice_rows.len());
    for row in practice_rows {
        exact_fields(row, &["task", "rubric"])?;
        practice.push(PracticeCase {
            task: bounded_text(required_str(row, "task")?, 1, 600, "practice task")?,
            rubric: string_list(
                row.get("rubric").ok_or("practice rubric is required")?,
                1,
                4,
                "rubric",
            )?,
        });
    }
    let budget_value = value.get("budget").ok_or("budget is required")?;
    exact_fields(budget_value, &["tokens", "request_tokens", "seconds"])?;
    let budget = BudgetSpec {
        tokens: positive_integer(budget_value, "tokens", MAX_TOKENS)?,
        request_tokens: positive_integer(budget_value, "request_tokens", MAX_REQUEST_TOKENS)?,
        seconds: positive_integer(budget_value, "seconds", MAX_SECONDS)?,
    };
    if budget.request_tokens > budget.tokens {
        return Err("request_tokens cannot exceed the total token budget".into());
    }
    Ok(Design {
        role,
        name,
        system_prompt,
        tools,
        guards,
        practice,
        budget,
    })
}

/// The designer sees the goal, role contract, and practice format. Frozen exam material belongs
/// to the examiner and is deliberately absent here.
pub fn designer_prompt(role: Role, goal: &str) -> Result<String, String> {
    let goal = bounded_text(goal, 1, 2_000, "goal")?;
    let tools = role.owner_tools().join(", ");
    let role_name = role.name();
    Ok(format!(
        "Design one {role_name} agent for this goal. Return only one JSON object with exactly these keys: role, name, system_prompt, tools, guards, practice, budget. The role must be \"{role_name}\". The name must be 2-32 bytes (ASCII only) and match [a-z0-9]+(?:-[a-z0-9]+)*: lowercase ASCII letters and digits separated by single hyphens, with no leading, trailing, or doubled hyphen. For example, \"source-guide-2\" is valid; underscores are invalid. system_prompt must be 30-4000 characters, not blank, with no control characters except newline and tab. tools must be an array of 0-4 unique plain strings, each 1-300 characters, selected only from this owner list: [{tools}]; tools are metadata, not callable functions. guards must be 2-6 unique plain strings, each 1-300 characters. practice must be an array of 1-3 objects, each exactly {{\"task\": string, \"rubric\": [1-4 strings]}}. Each practice task must be 1-600 plain characters; each rubric must contain 1-4 unique plain strings, each 1-300 characters. Each practice task must include all supplied facts it needs so it can be answered from that task alone; practice is advisory only and never proves a recipe should be kept. Do not reproduce fixed exam cases or include exam answers. budget must contain exactly {{\"tokens\": positive integer <= {MAX_TOKENS}, \"request_tokens\": positive integer <= {MAX_REQUEST_TOKENS}, \"seconds\": positive integer <= {MAX_SECONDS}}}; request_tokens cannot exceed tokens. Do not add fields, choose an engine, or request a tool outside the list. Keep every task and rubric grounded in the goal.\n\nGoal: {goal}"
    ))
}

/// Design a recipe with the named local model. The only network destination is 127.0.0.1.
pub fn design_local(
    ctx: &Ctx,
    role: Role,
    port: u16,
    model: &str,
    goal: &str,
) -> Result<String, String> {
    let model = bounded_text(model, 1, 128, "model")?;
    if model.chars().any(char::is_control) || port == 0 {
        return Err("model or loopback port is invalid".into());
    }
    let prompt = designer_prompt(role, goal)?;
    let run = format!("foundry-design-{}-{}", unix_millis(), std::process::id());
    let log = RunLog::create(ctx, &run, unix_secs())
        .map_err(|e| format!("could not open run receipt: {e}"))?;
    let meter = Meter::root(Budget {
        tokens: DESIGN_MAX_TOKENS,
        wall_time: Duration::from_secs(DESIGN_MAX_SECONDS),
        tool_calls: 0,
        cloud_cents: 0,
    });
    let child = meter.child(Budget {
        tokens: DESIGN_MAX_TOKENS,
        wall_time: Duration::from_secs(DESIGN_MAX_SECONDS),
        tool_calls: 0,
        cloud_cents: 0,
    });
    let plan_line = log.emit(
        unix_secs(),
        Ev {
            parent: None,
            depth: 0,
            member: "designer",
            engine: "openai-compatible-loopback",
            model: &model,
            kind: "plan",
            fields: vec![("prompt", J::s(&prompt))],
        },
    );
    let started = Instant::now();
    let reply = infer(
        port,
        &model,
        "You are Perennos Tempo's agent designer. Follow the requested JSON shape exactly. Return data only.",
        &prompt,
        DESIGN_MAX_OUTPUT,
        true,
        &child,
    );
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let used = meter.used();
    let raw_answer = reply.content.clone();
    let raw_body = reply.body.clone();
    let usage = reply.usage;
    let finish_reason = reply.finish_reason.clone();
    let call_error = reply.error.clone();
    let mut design = None;
    let mut validation_error = call_error.clone();
    if validation_error.is_none() {
        if finish_reason == "length" {
            validation_error = Some("model output was truncated at the token limit".into());
        } else if raw_answer.trim().is_empty() {
            validation_error = Some("model returned empty content".into());
        } else {
            match validate_design(&raw_answer, role) {
                Ok(d) => design = Some(d),
                Err(e) => validation_error = Some(e),
            }
        }
    }
    let mut recipe_id = String::new();
    if let Some(d) = design.as_ref() {
        recipe_id = format!("{}-{}-{}", role.name(), unix_millis(), std::process::id());
        let path = recipe_path(ctx, &recipe_id);
        if let Err(e) = write_new(
            &path,
            &design_json(d, &recipe_id, port, &model, &run, usage, elapsed_ms).compact(),
        ) {
            validation_error = Some(format!("valid design could not be saved: {e}"));
            design = None;
            recipe_id.clear();
        }
    }
    let status = if design.is_some() {
        "designed"
    } else {
        "rejected"
    };
    let result_line = log.emit(
        unix_secs(),
        Ev {
            parent: None,
            depth: 0,
            member: "designer",
            engine: "openai-compatible-loopback",
            model: &model,
            kind: "result",
            fields: vec![
                ("status", J::s(status)),
                ("answer", J::s(&raw_answer)),
                ("provider_body", J::s(&raw_body)),
                ("error", validation_error.as_deref().map_or(J::Null, J::s)),
                ("prompt_tokens", j_u64(usage.prompt)),
                ("completion_tokens", j_u64(usage.completion)),
                ("usage_source", J::s(usage.source)),
                ("elapsed_ms", J::n(elapsed_ms as f64)),
                (
                    "response_model",
                    reply.response_model.as_deref().map_or(J::Null, J::s),
                ),
                ("requested_model", J::s(&reply.requested_model)),
            ],
        },
    );
    let receipt = J::Obj(vec![
        ("v".into(), J::n(1.0)),
        ("kind".into(), J::s("design")),
        ("status".into(), J::s(status)),
        ("role".into(), J::s(role.name())),
        ("recipe_id".into(), J::s(&recipe_id)),
        ("engine".into(), J::s("openai-compatible-loopback")),
        ("model".into(), J::s(&model)),
        ("port".into(), J::n(f64::from(port))),
        ("requested_model".into(), J::s(&reply.requested_model)),
        (
            "response_model".into(),
            reply.response_model.as_deref().map_or(J::Null, J::s),
        ),
        ("baseline_prompt".into(), J::s(BASELINE_PROMPT)),
        (
            "baseline_sha256".into(),
            J::s(&sha256_hex(BASELINE_PROMPT.as_bytes())),
        ),
        ("goal".into(), J::s(goal)),
        ("designer_prompt".into(), J::s(&prompt)),
        ("answer".into(), J::s(&raw_answer)),
        ("provider_body".into(), J::s(&raw_body)),
        (
            "error".into(),
            validation_error.as_deref().map_or(J::Null, J::s),
        ),
        ("prompt_ref".into(), J::s(&log.reference(plan_line))),
        ("answer_ref".into(), J::s(&log.reference(result_line))),
        ("prompt_tokens".into(), j_u64(usage.prompt)),
        ("completion_tokens".into(), j_u64(usage.completion)),
        ("usage_source".into(), J::s(usage.source)),
        ("elapsed_ms".into(), J::n(elapsed_ms as f64)),
        ("root_tokens".into(), J::n(used.tokens as f64)),
        ("root_seconds".into(), J::n(used.secs as f64)),
    ]);
    let receipt_path = ctx
        .agent_data
        .join("foundry")
        .join("receipts")
        .join(format!("{run}.json"));
    write_new(&receipt_path, &receipt.compact())
        .map_err(|e| format!("could not save design receipt: {e}"))?;
    if let Some(error) = validation_error {
        Err(format!(
            "design rejected; receipt: {} ({error})",
            receipt_path.display()
        ))
    } else {
        Ok(format!(
            "Design saved for its exam; {recipe_id}; recipe: {}",
            recipe_path(ctx, &recipe_id).display()
        ))
    }
}

fn j_u64(n: u64) -> J {
    J::Num(n.to_string())
}
pub(super) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(super) fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
pub(super) fn recipe_path(ctx: &Ctx, id: &str) -> std::path::PathBuf {
    ctx.agent_data
        .join("foundry")
        .join("recipes")
        .join(format!("{id}.json"))
}

pub(super) fn write_new(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")
}

fn design_json(
    d: &Design,
    id: &str,
    port: u16,
    model: &str,
    design_run: &str,
    usage: Usage,
    elapsed_ms: u64,
) -> J {
    J::Obj(vec![
        ("v".into(), J::n(1.0)),
        ("id".into(), J::s(id)),
        ("design".into(), design_data(d)),
        ("host_guard".into(), J::s(HOST_GUARD)),
        ("engine".into(), J::s("openai-compatible-loopback")),
        ("requested_model".into(), J::s(model)),
        ("port".into(), J::n(f64::from(port))),
        ("exam_status".into(), J::s("untested")),
        (
            "design_cost".into(),
            J::Obj(vec![
                (
                    "receipt".into(),
                    J::s(&format!("foundry/receipts/{design_run}.json")),
                ),
                (
                    "baseline_sha256".into(),
                    J::s(&sha256_hex(BASELINE_PROMPT.as_bytes())),
                ),
                ("prompt_tokens".into(), j_u64(usage.prompt)),
                ("completion_tokens".into(), j_u64(usage.completion)),
                ("usage_source".into(), J::s(usage.source)),
                ("elapsed_ms".into(), j_u64(elapsed_ms)),
            ]),
        ),
    ])
}

fn design_data(d: &Design) -> J {
    J::Obj(vec![
        ("role".into(), J::s(d.role.name())),
        ("name".into(), J::s(&d.name)),
        ("system_prompt".into(), J::s(&d.system_prompt)),
        ("tools".into(), J::strings(&d.tools)),
        ("guards".into(), J::strings(&d.guards)),
        (
            "practice".into(),
            J::Arr(
                d.practice
                    .iter()
                    .map(|p| {
                        J::Obj(vec![
                            ("task".into(), J::s(&p.task)),
                            ("rubric".into(), J::strings(&p.rubric)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "budget".into(),
            J::Obj(vec![
                ("tokens".into(), J::n(d.budget.tokens as f64)),
                (
                    "request_tokens".into(),
                    J::n(d.budget.request_tokens as f64),
                ),
                ("seconds".into(), J::n(d.budget.seconds as f64)),
            ]),
        ),
    ])
}

/// Console command for the independent foundry binary.
pub fn run(args: &[String]) -> i32 {
    if args.first().is_some_and(|x| x == "task") {
        return task::run(&Ctx::from_env(), &args[1..]);
    }
    if args.first().is_some_and(|x| x == "team") {
        return team::run(&Ctx::from_env(), &args[1..]);
    }
    if args.first().is_some_and(|x| x == "stop") {
        return task::stop(&Ctx::from_env(), &args[1..]);
    }
    if args.first().is_some_and(|x| x == "exam") {
        return runner::run(&Ctx::from_env(), &args[1..]);
    }
    if args
        .first()
        .is_none_or(|x| x == "help" || x == "--help" || x == "-h")
    {
        if args.len() > 1 {
            eprintln!("help takes no extra arguments");
            return 2;
        }
        println!("tempo-foundry design ROLE --port PORT --model MODEL --goal TEXT\ntempo-foundry exam RECIPE_ID\ntempo-foundry task RECIPE_ID --task-file FILE [--baseline]\ntempo-foundry team --research ID --code ID --content ID --task-file FILE\ntempo-foundry stop RUN_ID");
        return 0;
    }
    if args.first().is_none_or(|x| x != "design") || args.len() < 2 {
        eprintln!("Use: tempo-foundry design ROLE --port PORT --model MODEL --goal TEXT");
        return 2;
    }
    let role = match Role::parse(&args[1]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut options: Vec<(&str, &str)> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        if i + 1 >= args.len() || !matches!(args[i].as_str(), "--port" | "--model" | "--goal") {
            eprintln!("Expected each supported option exactly once with a value");
            return 2;
        }
        let key = args[i].as_str();
        let value = args[i + 1].as_str();
        if value.is_empty()
            || value.starts_with("--")
            || options.iter().any(|(seen, _)| *seen == key)
        {
            eprintln!("Option is empty, duplicated, or missing its value");
            return 2;
        }
        options.push((key, value));
        i += 2;
    }
    if options.len() != 3 {
        eprintln!("--port, --model, and --goal are required");
        return 2;
    }
    let option = |name: &str| {
        options
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    };
    let Some(port) = option("--port")
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p != 0)
    else {
        eprintln!("--port must be a loopback service port");
        return 2;
    };
    let Some(model) = option("--model") else {
        eprintln!("--model is required");
        return 2;
    };
    let Some(goal) = option("--goal") else {
        eprintln!("--goal is required");
        return 2;
    };
    match design_local(&Ctx::from_env(), role, port, model, goal) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

pub(super) fn exact_fields(value: &J, expected: &[&str]) -> Result<(), String> {
    let J::Obj(fields) = value else {
        return Err("expected a JSON object".into());
    };
    if fields.len() != expected.len() {
        return Err("object has missing or unknown fields".into());
    }
    for (i, (key, _)) in fields.iter().enumerate() {
        if !expected.contains(&key.as_str()) || fields[..i].iter().any(|(prior, _)| prior == key) {
            return Err("object has an unknown or duplicate field".into());
        }
    }
    Ok(())
}

fn required_str<'a>(value: &'a J, key: &str) -> Result<&'a str, String> {
    value
        .str(key)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn bounded_text(s: &str, min: usize, max: usize, field: &str) -> Result<String, String> {
    let trimmed = s.trim();
    let len = trimmed.chars().count();
    if !(min..=max).contains(&len) || trimmed.chars().any(char::is_control) || trimmed.is_empty() {
        return Err(format!("{field} must be {min}-{max} plain characters"));
    }
    Ok(trimmed.to_string())
}

fn bounded_prompt(s: &str) -> Result<String, String> {
    let len = s.chars().count();
    if !(30..=4_000).contains(&len)
        || s.trim().is_empty()
        || s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("system_prompt must be 30-4000 characters without unsafe controls".into());
    }
    Ok(s.trim().to_string())
}

fn string_list(value: &J, min: usize, max: usize, field: &str) -> Result<Vec<String>, String> {
    let J::Arr(items) = value else {
        return Err(format!("{field} must be an array"));
    };
    if !(min..=max).contains(&items.len()) {
        return Err(format!("{field} must have {min}-{max} items"));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let J::Str(s) = item else {
            return Err(format!("{field} items must be strings"));
        };
        let s = bounded_text(s, 1, 300, field)?;
        if out.contains(&s) {
            return Err(format!("{field} items must be unique"));
        }
        out.push(s);
    }
    Ok(out)
}

fn positive_integer(value: &J, key: &str, max: u64) -> Result<u64, String> {
    let Some(J::Num(text)) = value.get(key) else {
        return Err(format!("{key} must be a positive integer"));
    };
    let n = text
        .parse::<u64>()
        .map_err(|_| format!("{key} must be a positive integer"))?;
    if n == 0 || n > max {
        return Err(format!("{key} must be between 1 and {max}"));
    }
    Ok(n)
}

fn slug(s: &str) -> bool {
    (2..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        r#"{"role":"research","name":"source-guide","system_prompt":"Use only supplied facts and explain what remains unknown.","tools":["sections.read"],"guards":["No external side effects.","Stop when the budget ends."],"practice":[{"task":"Summarize this supplied section.","rubric":["Name the limit."]}],"budget":{"tokens":3000,"request_tokens":1000,"seconds":120}}"#.into()
    }

    #[test]
    fn accepts_a_bounded_design_and_keeps_practice_separate() {
        let d = validate_design(&sample(), Role::Research).unwrap();
        assert_eq!(d.tools, ["sections.read"]);
        assert_eq!(d.practice.len(), 1);
        assert_eq!(d.budget.tokens, 3000);
    }

    #[test]
    fn runtime_prompt_includes_role_guards_before_the_fixed_host_guard() {
        let design = validate_design(&sample(), Role::Research).unwrap();
        let prompt = design.runtime_system_prompt();
        assert!(prompt.contains("Use only supplied facts"));
        let first_role_guard = prompt.find("No external side effects.").unwrap();
        let second_role_guard = prompt.find("Stop when the budget ends.").unwrap();
        let host_guard = prompt.find(HOST_GUARD).unwrap();
        assert!(first_role_guard < host_guard);
        assert!(second_role_guard < host_guard);
        assert!(prompt.ends_with(HOST_GUARD));
    }

    #[test]
    fn rejects_unknown_duplicate_and_wrongly_typed_fields() {
        assert!(validate_design(
            &sample().replace(
                "\"name\":\"source-guide\",",
                "\"name\":\"source-guide\",\"extra\":true,"
            ),
            Role::Research
        )
        .is_err());
        assert!(validate_design(
            &sample().replace(
                "\"name\":\"source-guide\",",
                "\"name\":\"source-guide\",\"name\":\"second\","
            ),
            Role::Research
        )
        .is_err());
        assert!(validate_design(
            &sample().replace(
                "\"tools\":[\"sections.read\"]",
                "\"tools\":\"sections.read\""
            ),
            Role::Research
        )
        .is_err());
        assert!(validate_design(
            &sample().replace("\"tokens\":3000", "\"tokens\":0"),
            Role::Research
        )
        .is_err());
        assert!(validate_design(
            &sample().replace("\"tokens\":3000", "\"tokens\":18446744073709551616"),
            Role::Research
        )
        .is_err());
        for bad in ["3000.5", "-1", "3e3", "NaN", "Infinity"] {
            assert!(validate_design(
                &sample().replace("\"tokens\":3000", &format!("\"tokens\":{bad}")),
                Role::Research
            )
            .is_err());
        }
        assert!(validate_design(
            &sample().replace("\"tokens\":3000", "\"tokens\":500"),
            Role::Research
        )
        .is_err());
        assert!(validate_design(
            &sample().replace("\"tokens\":3000,", "\"extra\":1,\"tokens\":3000,"),
            Role::Research
        )
        .is_err());
    }

    #[test]
    fn rejects_role_or_tool_escalation_and_bad_practice() {
        assert!(validate_design(&sample(), Role::Code).is_err());
        assert!(validate_design(
            &sample().replace("sections.read", "budget.patch"),
            Role::Research
        )
        .is_err());
        assert!(validate_design(
            &sample().replace("\"rubric\":[\"Name the limit.\"]", "\"rubric\":[]"),
            Role::Research
        )
        .is_err());
    }

    #[test]
    fn designer_prompt_discloses_design_bounds_and_keeps_exam_material_separate() {
        let p = designer_prompt(Role::Content, "Explain the supplied facts plainly.").unwrap();
        assert!(p.contains("Goal: Explain the supplied facts plainly."));
        assert!(p.contains("product.facts"));
        assert!(p.contains("practice task must be 1-600 plain characters"));
        assert!(p.contains("rubric must contain 1-4 unique plain strings, each 1-300 characters"));
        assert!(p.contains("guards must be 2-6 unique plain strings, each 1-300 characters"));
        assert!(p.contains("tools must be an array of 0-4 unique plain strings, each 1-300 characters"));
        assert!(p.contains("Each practice task must include all supplied facts"));
        assert!(p.contains("practice is advisory only"));
        assert!(p.contains("fixed exam cases"));
        assert!(!p.to_lowercase().contains("held-out example"));
        assert!(designer_prompt(Role::Code, "\n").is_err());
        let multiline = sample().replace(
            "and explain what remains unknown.",
            "and explain\\nwhat remains unknown.",
        );
        assert!(validate_design(&multiline, Role::Research).is_ok());
    }

    #[test]
    fn designer_slug_prompt_matches_the_validator() {
        let prompt = designer_prompt(Role::Research, "Read the supplied facts.").unwrap();
        assert!(prompt.contains("2-32 bytes"));
        assert!(prompt.contains("[a-z0-9]+(?:-[a-z0-9]+)*"));
        assert!(prompt.contains("source-guide-2"));
        assert!(validate_design(
            &sample().replace("source-guide", "source-guide-2"),
            Role::Research
        )
        .is_ok());
        assert!(validate_design(
            &sample().replace("source-guide", "source_guide"),
            Role::Research
        )
        .is_err());
    }
}
