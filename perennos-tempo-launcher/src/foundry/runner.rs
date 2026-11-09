use super::exam::{self, Case, Decision};
use super::{
    exact_fields, infer, recipe_path, unix_millis, unix_secs, write_new, Design, Role,
    BASELINE_PROMPT, HOST_GUARD,
};
use crate::conductor::budget::{Budget, Meter};
use crate::conductor::runlog::{Ev, RunLog};
use crate::conductor::takes_file::{self, Author, Take};
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::Ctx;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const ENGINE: &str = "openai-compatible-loopback";

pub(super) struct Recipe {
    pub(super) id: String,
    pub(super) recipe_sha256: String,
    pub(super) role: Role,
    pub(super) design: Design,
    pub(super) port: u16,
    pub(super) model: String,
    pub(super) design_tokens: u64,
    pub(super) design_seconds: u64,
    pub(super) design_usage: String,
    pub(super) design_receipt: String,
}

struct Attempt {
    arm: &'static str,
    case: String,
    status: &'static str,
    answer: String,
    body: String,
    error: Option<String>,
    correct: Option<bool>,
    prompt_tokens: u64,
    completion_tokens: u64,
    usage_source: &'static str,
    response_model: Option<String>,
    elapsed_ms: u64,
    prompt_ref: String,
    answer_ref: String,
    artifact: Option<String>,
    review_artifact: Option<String>,
}

pub fn run(ctx: &Ctx, args: &[String]) -> i32 {
    if args.first().is_some_and(|arg| arg == "--artifacts") {
        return show_artifacts(ctx, &args[1..]);
    }
    if args.is_empty() {
        eprintln!("Use: tempo-foundry exam RECIPE_ID [--code-check FILE]");
        return 2;
    }
    let id = args[0].as_str();
    let code_check = match args.get(1).map(String::as_str) {
        None => None,
        Some("--code-check") if args.len() == 3 => Some(args[2].as_str()),
        _ => {
            eprintln!("Expected only --code-check FILE after the recipe id");
            return 2;
        }
    };
    let recipe = match load_recipe(ctx, id) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if let Err(e) = verify_current_recipe(ctx, &recipe) {
        eprintln!("{e}");
        return 1;
    }
    if let Some(proof) = code_check {
        return super::code_check::import(ctx, &recipe, proof);
    }
    match execute(ctx, &recipe) {
        Ok((run, keep, reason)) => {
            println!(
                "Exam {}. {}. Run: {run}",
                if keep { "kept" } else { "rejected or untested" },
                reason
            );
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

pub(super) fn load_recipe(ctx: &Ctx, id: &str) -> Result<Recipe, String> {
    if !safe_id(id) {
        return Err("recipe id is not a safe local id".into());
    }
    let path = recipe_path(ctx, id);
    let raw = fs::read_to_string(&path).map_err(|e| format!("could not read recipe: {e}"))?;
    let recipe_sha256 = sha256_hex(raw.as_bytes());
    let value = parse(&raw).map_err(|e| format!("recipe JSON is invalid: {e}"))?;
    exact_fields(
        &value,
        &[
            "v",
            "id",
            "design",
            "host_guard",
            "engine",
            "requested_model",
            "port",
            "exam_status",
            "design_cost",
        ],
    )?;
    if value.str("id") != Some(id)
        || value.f64("v") != Some(1.0)
        || value.str("engine") != Some(ENGINE)
        || value.str("host_guard") != Some(HOST_GUARD)
        || value.str("exam_status") != Some("untested")
    {
        return Err("recipe identity or host guard does not match".into());
    }
    let model = checked_text(
        value
            .str("requested_model")
            .ok_or("recipe model is missing")?,
        128,
        "model",
    )?;
    let port = integer(value.get("port").ok_or("recipe port is missing")?)
        .filter(|p| (1..=65535).contains(p))
        .ok_or("recipe loopback port is invalid")? as u16;
    let design_value = value.get("design").ok_or("recipe design is missing")?;
    let role = Role::parse(design_value.str("role").ok_or("design role is missing")?)?;
    let design = super::validate_design(&design_value.compact(), role)
        .map_err(|e| format!("saved design failed revalidation: {e}"))?;
    if role == Role::Code && exam::source_hash(Role::Code) != exam::CODE_SOURCE_SHA256 {
        return Err("frozen budget.rs source hash changed; refusing code exam".into());
    }
    let cost = value
        .get("design_cost")
        .ok_or("recipe design cost is missing")?;
    exact_fields(
        cost,
        &[
            "receipt",
            "baseline_sha256",
            "prompt_tokens",
            "completion_tokens",
            "usage_source",
            "elapsed_ms",
        ],
    )?;
    if cost.str("baseline_sha256") != Some(&sha256_hex(BASELINE_PROMPT.as_bytes())) {
        return Err("recipe baseline identity changed".into());
    }
    let design_prompt_tokens = integer(cost.get("prompt_tokens").ok_or("design usage is missing")?)
        .ok_or("design prompt token count is invalid")?;
    let design_completion_tokens = integer(
        cost.get("completion_tokens")
            .ok_or("design usage is missing")?,
    )
    .ok_or("design completion token count is invalid")?;
    let design_tokens = design_prompt_tokens
        .checked_add(design_completion_tokens)
        .ok_or("design token usage overflowed")?;
    let design_elapsed_ms = integer(cost.get("elapsed_ms").ok_or("design time is missing")?)
        .ok_or("design elapsed time is invalid")?;
    let design_seconds = design_elapsed_ms.saturating_add(999) / 1000;
    let design_usage = cost
        .str("usage_source")
        .ok_or("design usage source is missing")?;
    if !matches!(design_usage, "actual" | "estimated_chars_over_4") {
        return Err("design usage source is unknown".into());
    }
    let design_receipt = checked_text(
        cost.str("receipt")
            .ok_or("design receipt reference is missing")?,
        240,
        "receipt ref",
    )?;
    Ok(Recipe {
        id: id.into(),
        recipe_sha256,
        role,
        design,
        port,
        model,
        design_tokens,
        design_seconds,
        design_usage: design_usage.into(),
        design_receipt,
    })
}

pub(super) fn verify_current_recipe(ctx: &Ctx, recipe: &Recipe) -> Result<(), String> {
    verify_recipe_in_dir(&ctx.agent_data, recipe)
}

pub(super) fn verify_recipe_in_dir(agent_data: &Path, recipe: &Recipe) -> Result<(), String> {
    let path = agent_data
        .join("foundry")
        .join("recipes")
        .join(format!("{}.json", recipe.id));
    let bytes = fs::read(&path).map_err(|e| format!("could not re-read saved recipe: {e}"))?;
    if recipe_sha256_matches(&recipe.recipe_sha256, &bytes) {
        Ok(())
    } else {
        Err("saved recipe bytes changed since they were loaded; refusing stale qualification or dispatch".into())
    }
}

fn recipe_sha256_matches(expected: &str, exact_bytes: &[u8]) -> bool {
    sha256_hex(exact_bytes) == expected
}

pub(super) fn receipt_recipe_matches(stored: Option<&str>, current: &str) -> bool {
    stored == Some(current)
}

fn execute(ctx: &Ctx, recipe: &Recipe) -> Result<(String, bool, String), String> {
    verify_current_recipe(ctx, recipe)?;
    let run = format!("foundry-exam-{}-{}", unix_millis(), std::process::id());
    let out_dir = ctx.agent_data.join("foundry").join("exams").join(&run);
    fs::create_dir_all(&out_dir).map_err(|e| format!("could not create exam folder: {e}"))?;
    let log = RunLog::create(ctx, &run, unix_secs())
        .map_err(|e| format!("could not open exam log: {e}"))?;
    let wall_s = recipe
        .design
        .budget
        .seconds
        .saturating_sub(recipe.design_seconds);
    let remaining_tokens = recipe
        .design
        .budget
        .tokens
        .saturating_sub(recipe.design_tokens);
    let budget_refusal = if wall_s == 0 {
        Some(
            "design cost exhausted the total wall-time budget; no exam calls were made".to_string(),
        )
    } else if remaining_tokens < 5 {
        Some("fewer than five post-design tokens remain, so finite practice/baseline/candidate child budgets cannot be allocated".to_string())
    } else {
        None
    };
    let root = Meter::root(Budget {
        tokens: recipe.design.budget.tokens,
        wall_time: Duration::from_secs(wall_s).max(Duration::from_millis(1)),
        tool_calls: 0,
        cloud_cents: 0,
    });
    root.charge_tokens(recipe.design_tokens);
    let remaining = remaining_tokens;
    // The shared request helper reads the active meter's wall budget. Give each child the same
    // overall window, and start them together, so baseline/candidate do not expire on a slice.
    let shared_window = Duration::from_secs(wall_s).max(Duration::from_millis(1));
    let practice_meter = root.child(Budget {
        tokens: remaining / 5,
        wall_time: shared_window,
        tool_calls: 0,
        cloud_cents: 0,
    });
    let baseline_meter = root.child(Budget {
        tokens: remaining.saturating_mul(2) / 5,
        wall_time: shared_window,
        tool_calls: 0,
        cloud_cents: 0,
    });
    let candidate_meter = root.child(Budget {
        tokens: remaining
            .saturating_sub(remaining / 5)
            .saturating_sub(remaining.saturating_mul(2) / 5),
        wall_time: shared_window,
        tool_calls: 0,
        cloud_cents: 0,
    });
    let started = Instant::now();
    let mut attempts = Vec::new();
    let mut practice_stop = budget_refusal;
    let mut practice_provider_error = false;
    if practice_stop.is_none() {
        for (i, practice) in recipe.design.practice.iter().enumerate() {
            if root.halt().is_some() {
                practice_stop = Some("shared budget ended".to_string());
                break;
            }
            let prompt = format!("Practice case {}. Task: {}\nUse this recorded rubric as a checklist, but do not grade yourself: {}", i + 1, practice.task, practice.rubric.join("; "));
            let a = call_one(
                &log,
                &out_dir,
                recipe,
                "practice",
                &format!("practice-{}", i + 1),
                &recipe.design.runtime_system_prompt(),
                &prompt,
                false,
                recipe.design.budget.request_tokens,
                &practice_meter,
            )?;
            let failed = a.error.is_some();
            attempts.push(a);
            if failed {
                practice_provider_error = true;
                practice_stop =
                    Some("practice provider returned an error; remaining calls stopped".into());
                break;
            }
        }
    }

    let mut baseline_results = Vec::new();
    let mut candidate_results = Vec::new();
    let mut take_rows = Vec::new();
    let mut code_artifact_error = None;
    let mut code_artifacts = Vec::new();
    let code = recipe.role == Role::Code;
    if practice_provider_error || practice_stop.is_some() {
        // A failed practice provider call is a stop condition; do not spend the remaining budget.
    } else if !code {
        let cases = exam::cases(recipe.role);
        for case in &cases {
            if root.halt().is_some() {
                break;
            }
            let prompt = exam::task_prompt(case);
            let base = call_one(
                &log,
                &out_dir,
                recipe,
                "baseline",
                case.id,
                BASELINE_PROMPT,
                &prompt,
                true,
                recipe.design.budget.request_tokens,
                &baseline_meter,
            )?;
            let base_ok = base.error.is_none() && base.correct == Some(true);
            push_take(&mut take_rows, &run, recipe, case, "baseline", &base);
            attempts.push(base);
            if attempts.last().is_some_and(|a| a.error.is_some()) {
                break;
            }
            let cand = call_one(
                &log,
                &out_dir,
                recipe,
                "candidate",
                case.id,
                &recipe.design.runtime_system_prompt(),
                &prompt,
                true,
                recipe.design.budget.request_tokens,
                &candidate_meter,
            )?;
            let cand_ok = cand.error.is_none() && cand.correct == Some(true);
            push_take(&mut take_rows, &run, recipe, case, "candidate", &cand);
            attempts.push(cand);
            if attempts.last().is_some_and(|a| a.error.is_some()) {
                break;
            }
            baseline_results.push(base_ok);
            candidate_results.push(cand_ok);
        }
    } else {
        let code_prompt = super::code_check::prompt()?;
        let base = call_one(
            &log,
            &out_dir,
            recipe,
            "baseline",
            "budget-counter-fix",
            BASELINE_PROMPT,
            &code_prompt,
            false,
            recipe.design.budget.request_tokens,
            &baseline_meter,
        )?;
        match super::code_check::save_artifact(
            &out_dir,
            "baseline",
            &base.answer,
            exam::BUDGET_SOURCE,
        ) {
            Ok(Some(artifact)) => code_artifacts.push(super::code_check::artifact_receipt(
                "baseline", &out_dir, &artifact,
            )?),
            Ok(None) => {
                code_artifact_error = Some(
                    "baseline response did not contain exactly three complete marked methods"
                        .to_string(),
                )
            }
            Err(e) => code_artifact_error = Some(e),
        }
        attempts.push(base);
        if attempts.last().is_some_and(|a| a.error.is_none()) && root.halt().is_none() {
            let cand = call_one(
                &log,
                &out_dir,
                recipe,
                "candidate",
                "budget-counter-fix",
                &recipe.design.runtime_system_prompt(),
                &code_prompt,
                false,
                recipe.design.budget.request_tokens,
                &candidate_meter,
            )?;
            match super::code_check::save_artifact(
                &out_dir,
                "candidate",
                &cand.answer,
                exam::BUDGET_SOURCE,
            ) {
                Ok(Some(artifact)) => code_artifacts.push(super::code_check::artifact_receipt(
                    "candidate",
                    &out_dir,
                    &artifact,
                )?),
                Ok(None) => {
                    code_artifact_error = Some(
                        "candidate response did not contain exactly three complete marked methods"
                            .to_string(),
                    )
                }
                Err(e) => code_artifact_error = Some(e),
            }
            attempts.push(cand);
        }
    }
    let complete_text_exam = !code
        && !practice_provider_error
        && baseline_results.len() == exam::cases(recipe.role).len()
        && candidate_results.len() == exam::cases(recipe.role).len();
    let decision = if code || !complete_text_exam {
        None
    } else {
        let cases = exam::cases(recipe.role);
        let base_hold: Vec<bool> = cases
            .iter()
            .filter(|c| !c.canary)
            .zip(baseline_results.iter())
            .map(|(_, b)| *b)
            .collect();
        let cand_hold: Vec<bool> = cases
            .iter()
            .filter(|c| !c.canary)
            .zip(candidate_results.iter())
            .map(|(_, b)| *b)
            .collect();
        let base_can: Vec<bool> = cases
            .iter()
            .filter(|c| c.canary)
            .zip(baseline_results.iter().skip(base_hold.len()))
            .map(|(_, b)| *b)
            .collect();
        let cand_can: Vec<bool> = cases
            .iter()
            .filter(|c| c.canary)
            .zip(candidate_results.iter().skip(cand_hold.len()))
            .map(|(_, b)| *b)
            .collect();
        Some(exam::decide(&base_hold, &cand_hold, &base_can, &cand_can))
    };
    let expected_log_lines = attempts
        .iter()
        .filter_map(|a| line_number(&a.answer_ref))
        .max()
        .unwrap_or(0);
    let chain_error = match crate::conductor::runlog::verify(&log.path) {
        Ok(lines) if lines >= expected_log_lines => None,
        Ok(lines) => Some(format!("run log is shorter than emitted result refs: verified {lines}, expected at least {expected_log_lines}")),
        Err(e) => Some(format!("run log chain verification failed: {e}")),
    };
    let take_error = if let Some(error) = &chain_error {
        Some(format!(
            "takes were not written because the run log was not verified: {error}"
        ))
    } else if take_rows.is_empty() {
        None
    } else {
        takes_file::append(&takes_file::path(ctx), &take_rows)
            .err()
            .map(|e| e.to_string())
    };
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let used = root.used();
    let mut final_decision = decision.clone();
    if let Some(d) = final_decision.as_mut() {
        if let Some(error) = &chain_error {
            d.keep = false;
            d.reason = format!("receipt chain could not be verified; no exam verdict: {error}");
        } else if let Some(error) = &take_error {
            d.keep = false;
            d.reason = format!("unapproved takes could not be preserved; no exam keep: {error}");
        }
    }
    let keep = final_decision.as_ref().is_some_and(|d| d.keep);
    let reason = if code {
        "compiled code proof is untested".to_string()
    } else if let Some(d) = &final_decision {
        d.reason.clone()
    } else if let Some(stop) = &practice_stop {
        stop.clone()
    } else {
        "paired exam did not complete every frozen case".to_string()
    };
    let status = if code || final_decision.is_none() {
        "Untested"
    } else if keep {
        "kept"
    } else {
        "rejected"
    };
    let report = report_json(
        recipe,
        &run,
        status,
        &reason,
        &attempts,
        final_decision.as_ref(),
        &practice_stop,
        take_error.as_deref(),
        chain_error.as_deref(),
        code_artifact_error.as_deref(),
        &code_artifacts,
        used.tokens,
        elapsed_ms,
        recipe.design_tokens,
    );
    let report_path = out_dir.join("exam.json");
    write_new(&report_path, &report.compact())
        .map_err(|e| format!("could not save exam report: {e}"))?;
    let receipt_path = ctx
        .agent_data
        .join("foundry")
        .join("receipts")
        .join(format!("{run}.json"));
    write_new(&receipt_path, &report.compact())
        .map_err(|e| format!("could not save exam receipt: {e}"))?;
    Ok((
        run,
        keep,
        format!("{}; report: {}", reason, report_path.display()),
    ))
}

#[allow(clippy::too_many_arguments)]
fn call_one(
    log: &RunLog,
    out_dir: &Path,
    recipe: &Recipe,
    arm: &'static str,
    case: &str,
    system: &str,
    prompt: &str,
    json_mode: bool,
    max_tokens: u64,
    meter: &Meter,
) -> Result<Attempt, String> {
    let member = if arm == "practice" { "practice" } else { arm };
    let plan = log.emit(
        unix_secs(),
        Ev {
            parent: None,
            depth: 0,
            member,
            engine: ENGINE,
            model: &recipe.model,
            kind: "plan",
            fields: vec![
                ("case", J::s(case)),
                ("system", J::s(system)),
                ("prompt", J::s(prompt)),
            ],
        },
    );
    let started = Instant::now();
    let reply = infer(
        recipe.port,
        &recipe.model,
        system,
        prompt,
        max_tokens,
        json_mode,
        meter,
    );
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let correct = if arm == "baseline" || arm == "candidate" {
        exam::cases(recipe.role)
            .into_iter()
            .find(|c| c.id == case)
            .map(|c| reply.error.is_none() && exam::correct(&c, &reply.content))
    } else {
        None
    };
    let truncated = reply.finish_reason == "length";
    let error = reply
        .error
        .or_else(|| truncated.then(|| "answer was truncated at its output limit".into()));
    let status = if error.is_some() {
        if truncated {
            "truncated"
        } else {
            "error"
        }
    } else {
        "complete"
    };
    let artifact = save_answer(out_dir, arm, case, &reply.content)?;
    let review_artifact = if arm == "baseline" || arm == "candidate" {
        match exam::cases(recipe.role).into_iter().find(|c| c.id == case) {
            Some(c) => save_review_artifact(out_dir, arm, &c, &reply.content)?,
            None => None,
        }
    } else {
        None
    };
    let result = log.emit(
        unix_secs(),
        Ev {
            parent: None,
            depth: 0,
            member,
            engine: ENGINE,
            model: &recipe.model,
            kind: "result",
            fields: vec![
                ("case", J::s(case)),
                ("status", J::s(status)),
                ("answer", J::s(&reply.content)),
                ("provider_body", J::s(&reply.body)),
                ("error", error.as_deref().map_or(J::Null, J::s)),
                ("prompt_tokens", J::Num(reply.usage.prompt.to_string())),
                (
                    "completion_tokens",
                    J::Num(reply.usage.completion.to_string()),
                ),
                ("usage_source", J::s(reply.usage.source)),
                (
                    "response_model",
                    reply.response_model.as_deref().map_or(J::Null, J::s),
                ),
                ("elapsed_ms", J::Num(elapsed_ms.to_string())),
            ],
        },
    );
    Ok(Attempt {
        arm,
        case: case.into(),
        status,
        answer: reply.content,
        body: reply.body,
        error,
        correct,
        prompt_tokens: reply.usage.prompt,
        completion_tokens: reply.usage.completion,
        usage_source: reply.usage.source,
        response_model: reply.response_model,
        elapsed_ms,
        prompt_ref: log.reference(plan),
        answer_ref: log.reference(result),
        artifact,
        review_artifact,
    })
}

fn save_answer(
    out_dir: &Path,
    arm: &str,
    case: &str,
    answer: &str,
) -> Result<Option<String>, String> {
    let safe_case = case
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>();
    let rel = format!("{arm}/{safe_case}.txt");
    write_new(&out_dir.join(&rel), answer)
        .map_err(|e| format!("could not preserve {arm} answer: {e}"))?;
    Ok(Some(rel))
}

fn save_review_artifact(
    out_dir: &Path,
    arm: &str,
    case: &Case,
    answer: &str,
) -> Result<Option<String>, String> {
    let Ok(value) = parse(answer) else {
        return Ok(None);
    };
    let content = if let Some(brief) = value.str("brief") {
        format!(
            "# {}\n\nSource: {}\n\n{}\n",
            case.task, case.source_ref, brief
        )
    } else if let Some(copy) = value.str("copy") {
        let headline = value.str("headline").unwrap_or("");
        let facts = value
            .get("facts")
            .map_or_else(|| "[]".to_string(), J::compact);
        let limit = value.str("limit").unwrap_or("");
        format!(
            "# {headline}\n\nSource: {}\n\n{copy}\n\nFacts: {facts}\n\nLimit: {limit}\n",
            case.source_ref
        )
    } else {
        return Ok(None);
    };
    let safe_case = case
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>();
    let rel = format!("{arm}/{safe_case}.md");
    write_new(&out_dir.join(&rel), &content)
        .map_err(|e| format!("could not preserve human-readable {arm} output: {e}"))?;
    Ok(Some(rel))
}

fn push_take(
    rows: &mut Vec<Take>,
    run: &str,
    recipe: &Recipe,
    case: &Case,
    arm: &str,
    attempt: &Attempt,
) {
    let id = format!("{run}-{}-{arm}", case.id);
    let other = format!(
        "{run}-{}-{}",
        case.id,
        if arm == "baseline" {
            "candidate"
        } else {
            "baseline"
        }
    );
    rows.push(Take {
        id,
        run: run.into(),
        band: None,
        part: Some(recipe.role.name().into()),
        task: case.task.into(),
        engine: ENGINE.into(),
        model: recipe.model.clone(),
        author: Author::Local,
        prompt_ref: attempt.prompt_ref.clone(),
        answer_ref: attempt.answer_ref.clone(),
        tests: attempt.correct,
        soundcheck: None,
        judge: None,
        owner: None,
        winner: false,
        pair: Some(other),
        ts: crate::conductor::runlog::rfc3339(unix_secs()),
    });
}

#[allow(clippy::too_many_arguments)]
fn report_json(
    recipe: &Recipe,
    run: &str,
    status: &str,
    reason: &str,
    attempts: &[Attempt],
    decision: Option<&Decision>,
    practice_stop: &Option<String>,
    take_error: Option<&str>,
    chain_error: Option<&str>,
    code_artifact_error: Option<&str>,
    code_artifacts: &[J],
    tokens: u64,
    elapsed_ms: u64,
    design_tokens: u64,
) -> J {
    let attempt_values = J::Arr(attempts.iter().map(attempt_json).collect());
    let practice = J::Arr(
        recipe
            .design
            .practice
            .iter()
            .enumerate()
            .map(|(i, case)| {
                let id = format!("practice-{}", i + 1);
                let answer = attempts
                    .iter()
                    .find(|a| a.arm == "practice" && a.case == id);
                J::Obj(vec![
                    ("task".into(), J::s(&case.task)),
                    (
                        "rubric".into(),
                        J::Arr(case.rubric.iter().map(|r| J::s(r)).collect()),
                    ),
                    ("rubric_role".into(), J::s("advisory_only")),
                    (
                        "status".into(),
                        answer.map_or(J::s("not_run"), |a| J::s(a.status)),
                    ),
                    (
                        "answer_ref".into(),
                        answer.map_or(J::Null, |a| J::s(&a.answer_ref)),
                    ),
                    (
                        "answer_artifact".into(),
                        answer
                            .and_then(|a| a.artifact.as_deref())
                            .map_or(J::Null, J::s),
                    ),
                ])
            })
            .collect(),
    );
    let decision = decision.map_or(J::Null, |d| {
        J::Obj(vec![
            ("keep".into(), J::Bool(d.keep)),
            ("reason".into(), J::s(&d.reason)),
            ("baseline_correct".into(), u64j(d.baseline_correct as u64)),
            ("candidate_correct".into(), u64j(d.candidate_correct as u64)),
            ("held_out".into(), u64j(d.held_out as u64)),
            ("baseline_canary".into(), u64j(d.baseline_canary as u64)),
            ("candidate_canary".into(), u64j(d.candidate_canary as u64)),
            ("canaries".into(), u64j(d.canaries as u64)),
        ])
    });
    J::Obj(vec![
        ("v".into(), J::n(1.0)), ("kind".into(), J::s("foundry_exam")), ("run".into(), J::s(run)),
        ("recipe_id".into(), J::s(&recipe.id)), ("role".into(), J::s(recipe.role.name())), ("status".into(), J::s(status)),
        ("recipe_sha256".into(), J::s(&recipe.recipe_sha256)),
        ("reason".into(), J::s(reason)), ("engine".into(), J::s(ENGINE)), ("requested_model".into(), J::s(&recipe.model)),
        ("baseline_prompt".into(), J::s(BASELINE_PROMPT)), ("baseline_sha256".into(), J::s(&sha256_hex(BASELINE_PROMPT.as_bytes()))),
        ("exam_version".into(), u64j(exam::EXAM_VERSION as u64)), ("exam_sha256".into(), J::s(&exam::exam_hash(recipe.role))),
        ("source_sha256".into(), J::s(&exam::source_hash(recipe.role))),
        ("source_base".into(), if recipe.role == Role::Code { J::s(exam::CODE_SOURCE_BASE) } else { J::Null }),
        ("design_receipt".into(), J::s(&recipe.design_receipt)), ("design_prompt_tokens".into(), u64j(recipe.design_tokens)),
        ("design_usage_source".into(), J::s(&recipe.design_usage)), ("design_elapsed_s".into(), u64j(recipe.design_seconds)),
        ("practice_stop".into(), practice_stop.as_deref().map_or(J::Null, J::s)),
        ("practice_cases".into(), practice),
        ("attempts".into(), attempt_values), ("decision".into(), decision),
        ("take_write_error".into(), take_error.map_or(J::Null, J::s)),
        ("run_log_error".into(), chain_error.map_or(J::Null, J::s)),
        ("code_artifact_error".into(), code_artifact_error.map_or(J::Null, J::s)),
        ("code_artifacts".into(), J::Arr(code_artifacts.to_vec())),
        ("exam_prompt_completion_tokens".into(), u64j(tokens.saturating_sub(design_tokens))), ("exam_elapsed_ms".into(), u64j(elapsed_ms)),
        ("meter_total_prompt_completion_tokens".into(), u64j(tokens)),
        ("total_prompt_completion_tokens".into(), u64j(tokens)),
        ("total_elapsed_ms".into(), u64j(elapsed_ms.saturating_add(recipe.design_seconds.saturating_mul(1000)))),
        ("usage_note".into(), J::s("Per-attempt token source is retained; total figures include design cost and are not cache-adjusted.")),
    ])
}

fn attempt_json(a: &Attempt) -> J {
    J::Obj(vec![
        ("arm".into(), J::s(a.arm)),
        ("case".into(), J::s(&a.case)),
        ("status".into(), J::s(a.status)),
        ("answer".into(), J::s(&a.answer)),
        ("provider_body".into(), J::s(&a.body)),
        ("error".into(), a.error.as_deref().map_or(J::Null, J::s)),
        ("correct".into(), a.correct.map_or(J::Null, J::Bool)),
        ("prompt_tokens".into(), u64j(a.prompt_tokens)),
        ("completion_tokens".into(), u64j(a.completion_tokens)),
        ("usage_source".into(), J::s(a.usage_source)),
        (
            "response_model".into(),
            a.response_model.as_deref().map_or(J::Null, J::s),
        ),
        ("elapsed_ms".into(), u64j(a.elapsed_ms)),
        ("prompt_ref".into(), J::s(&a.prompt_ref)),
        ("answer_ref".into(), J::s(&a.answer_ref)),
        (
            "artifact".into(),
            a.artifact.as_deref().map_or(J::Null, J::s),
        ),
        (
            "review_artifact".into(),
            a.review_artifact.as_deref().map_or(J::Null, J::s),
        ),
    ])
}

fn show_artifacts(ctx: &Ctx, args: &[String]) -> i32 {
    if args.len() != 1 || !safe_id(&args[0]) {
        eprintln!("Use: tempo-foundry exam --artifacts RUN_ID");
        return 2;
    }
    let path = ctx
        .agent_data
        .join("foundry")
        .join("exams")
        .join(&args[0])
        .join("exam.json");
    match fs::read_to_string(&path).ok().and_then(|s| parse(&s).ok()) {
        Some(report) if report.str("run") == Some(args[0].as_str()) => {
            println!(
                "{}",
                J::Obj(vec![
                    ("run".into(), J::s(&args[0])),
                    (
                        "recipe_id".into(),
                        report.get("recipe_id").cloned().unwrap_or(J::Null)
                    ),
                    (
                        "status".into(),
                        report.get("status").cloned().unwrap_or(J::Null)
                    ),
                    (
                        "code_artifacts".into(),
                        report
                            .get("code_artifacts")
                            .cloned()
                            .unwrap_or(J::Arr(vec![]))
                    ),
                    (
                        "attempts".into(),
                        report.get("attempts").cloned().unwrap_or(J::Arr(vec![]))
                    ),
                    ("report".into(), J::s(&path.to_string_lossy())),
                ])
                .compact()
            );
            0
        }
        _ => {
            eprintln!("saved exam artifacts were not found or did not match this run");
            1
        }
    }
}

fn integer(value: &J) -> Option<u64> {
    if let J::Num(n) = value {
        n.parse().ok()
    } else {
        None
    }
}

fn line_number(reference: &str) -> Option<usize> {
    reference.rsplit_once('#')?.1.parse().ok()
}

fn u64j(n: u64) -> J {
    J::Num(n.to_string())
}
fn safe_id(s: &str) -> bool {
    (2..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}
fn checked_text(s: &str, max: usize, field: &str) -> Result<String, String> {
    if s.is_empty() || s.chars().count() > max || s.chars().any(char::is_control) {
        Err(format!("{field} is invalid"))
    } else {
        Ok(s.into())
    }
}

#[cfg(test)]
mod recipe_binding_tests {
    use super::{receipt_recipe_matches, recipe_sha256_matches};
    use crate::setup::sha256::sha256_hex;

    #[test]
    fn same_recipe_id_with_prompt_model_or_budget_changes_cannot_reuse_keep_receipt() {
        let original = r#"{"id":"research-1","requested_model":"model-a","port":8101,"prompt":"Produce grounded brief","budget_tokens":100,"budget_seconds":90}"#;
        let expected = sha256_hex(original.as_bytes());
        assert!(recipe_sha256_matches(&expected, original.as_bytes()));
        assert!(receipt_recipe_matches(Some(&expected), &expected));

        for changed in [
            original.replace("Produce grounded brief", "Produce a longer grounded brief"),
            original.replace("model-a", "model-b"),
            original.replace("\"budget_tokens\":100", "\"budget_tokens\":101"),
        ] {
            assert!(changed.contains("\"id\":\"research-1\""));
            let changed_hash = sha256_hex(changed.as_bytes());
            assert!(!recipe_sha256_matches(&expected, changed.as_bytes()));
            assert!(!receipt_recipe_matches(Some(&expected), &changed_hash));
        }
    }
}
