use super::exam;
use super::runner::{self, Recipe};
use super::{infer, unix_millis, unix_secs, write_new, Role, BASELINE_PROMPT};
use crate::conductor::budget::{Budget, Meter};
use crate::conductor::runlog::{self, Ev, RunLog};
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::Ctx;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ENGINE: &str = "openai-compatible-loopback";
const MAX_TASK_FILE: usize = 64 * 1024;

pub(super) struct Qualification {
    pub(super) run: String,
    pub(super) total_tokens: u64,
    pub(super) total_ms: u64,
    pub(super) receipt: String,
}

struct Output {
    arm: &'static str,
    answer: String,
    body: String,
    error: Option<String>,
    requested_model: String,
    response_model: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    usage_source: &'static str,
    elapsed_ms: u64,
    prompt_ref: String,
    answer_ref: String,
    artifact: Option<String>,
    artifact_error: Option<String>,
}

pub fn run(ctx: &Ctx, args: &[String]) -> i32 {
    let (id, task_path, baseline) = match parse_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let recipe = match runner::load_recipe(ctx, id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if let Err(e) = runner::verify_current_recipe(ctx, &recipe) {
        eprintln!("Task refused: {e}");
        return 1;
    }
    let qualification = match find_qualification(ctx, &recipe) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Task refused: {e}");
            return 1;
        }
    };
    let task_bytes = match fs::read(task_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("could not read task file: {e}");
            return 1;
        }
    };
    if task_bytes.is_empty() || task_bytes.len() > MAX_TASK_FILE {
        eprintln!("task file must contain 1 to 65536 bytes");
        return 2;
    }
    let task = match String::from_utf8(task_bytes) {
        Ok(v)
            if !v.trim().is_empty()
                && v.chars()
                    .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')) =>
        {
            v
        }
        _ => {
            eprintln!("task file must be nonempty UTF-8 without NUL bytes");
            return 2;
        }
    };
    match execute(ctx, &recipe, &qualification, task_path, &task, baseline) {
        Ok((run, status, reason, path)) => {
            println!(
                "Task {status}: {reason}; run {run}; receipt {}",
                path.display()
            );
            if status == "Verified complete" {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("Task failed: {e}");
            1
        }
    }
}

fn parse_args(args: &[String]) -> Result<(&str, &str, bool), String> {
    if !(args.len() == 3 || args.len() == 4)
        || args.get(1).map(String::as_str) != Some("--task-file")
        || (args.len() == 4 && args.get(3).map(String::as_str) != Some("--baseline"))
        || args.first().is_none_or(|id| !safe_id(id))
        || args
            .get(2)
            .is_none_or(|p| p.is_empty() || p.starts_with("--"))
    {
        return Err("Use: tempo-foundry task RECIPE_ID --task-file FILE [--baseline]".into());
    }
    Ok((&args[0], &args[2], args.len() == 4))
}

fn execute(
    ctx: &Ctx,
    recipe: &Recipe,
    qualification: &Qualification,
    task_path: &str,
    task: &str,
    baseline: bool,
) -> Result<(String, &'static str, String, PathBuf), String> {
    runner::verify_current_recipe(ctx, recipe)?;
    let run = format!("foundry-task-{}-{}", unix_millis(), std::process::id());
    let out_dir = ctx.agent_data.join("foundry").join("tasks").join(&run);
    fs::create_dir_all(&out_dir).map_err(|e| format!("could not create task folder: {e}"))?;
    let stop_path = stop_path(ctx, &run);
    if stop_path.exists() {
        return Err("a stop flag already exists for this run id".into());
    }
    let log = RunLog::create(ctx, &run, unix_secs())
        .map_err(|e| format!("could not open task log: {e}"))?;
    let meter = Meter::root(Budget {
        tokens: recipe.design.budget.tokens,
        wall_time: Duration::from_secs(recipe.design.budget.seconds),
        tool_calls: 0,
        cloud_cents: 0,
    });
    let started = Instant::now();
    println!("Task run {run} started; stop with `tempo-foundry stop {run}` (an in-flight request runs until its bounded timeout).");
    let mut outputs = Vec::new();
    if baseline {
        let base = one(
            &log,
            &out_dir,
            recipe,
            &meter,
            "baseline",
            BASELINE_PROMPT,
            task,
        )?;
        let base_error = base.error.is_some();
        outputs.push(base);
        if stop_requested(ctx, &run) {
            meter.stop();
        }
        if !base_error && meter.halt().is_none() && !stop_requested(ctx, &run) {
            let candidate = one(
                &log,
                &out_dir,
                recipe,
                &meter,
                "candidate",
                &recipe.design.runtime_system_prompt(),
                task,
            )?;
            outputs.push(candidate);
        }
    } else {
        let candidate = one(
            &log,
            &out_dir,
            recipe,
            &meter,
            "candidate",
            &recipe.design.runtime_system_prompt(),
            task,
        )?;
        outputs.push(candidate);
    }
    if stop_requested(ctx, &run) {
        meter.stop();
    }
    let expected_lines = outputs
        .iter()
        .filter_map(|o| line_number(&o.answer_ref))
        .max()
        .unwrap_or(0);
    let chain_error = match runlog::verify(&log.path) {
        Ok(lines) if lines >= expected_lines => None,
        Ok(lines) => Some(format!(
            "verified {lines} log lines, expected at least {expected_lines}"
        )),
        Err(e) => Some(e),
    };
    let requested_calls = if baseline { 2 } else { 1 };
    let provider_ok = outputs.len() == requested_calls
        && outputs
            .iter()
            .all(|o| o.error.is_none() && o.artifact_error.is_none());
    let status = if chain_error.is_some() {
        "Untested"
    } else if provider_ok && !stop_requested(ctx, &run) {
        "Verified complete"
    } else {
        "In progress"
    };
    let used = meter.used();
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let report = J::Obj(vec![
        ("v".into(), J::n(1.0)), ("kind".into(), J::s("foundry_task")),
        ("run".into(), J::s(&run)), ("recipe_id".into(), J::s(&recipe.id)),
        ("recipe_sha256".into(), J::s(&recipe.recipe_sha256)),
        ("role".into(), J::s(recipe.role.name())), ("engine".into(), J::s(ENGINE)),
        ("requested_model".into(), J::s(&recipe.model)), ("status".into(), J::s(status)),
        ("task_file".into(), J::s(task_path)), ("task_sha256".into(), J::s(&sha256_hex(task.as_bytes()))),
        ("task".into(), J::s(task)), ("baseline_prompt".into(), J::s(BASELINE_PROMPT)),
        ("baseline_sha256".into(), J::s(&sha256_hex(BASELINE_PROMPT.as_bytes()))),
        ("qualification_run".into(), J::s(&qualification.run)), ("qualification_receipt".into(), J::s(&qualification.receipt)),
        ("design_prompt_tokens".into(), j_u64(recipe.design_tokens)),
        ("qualification_total_tokens".into(), j_u64(qualification.total_tokens)),
        ("execution_prompt_completion_tokens".into(), j_u64(used.tokens)),
        ("total_foundry_prompt_completion_tokens".into(), j_u64(qualification.total_tokens.saturating_add(used.tokens))),
        ("qualification_elapsed_ms".into(), j_u64(qualification.total_ms)),
        ("execution_elapsed_ms".into(), j_u64(elapsed_ms)),
        ("total_foundry_elapsed_ms".into(), j_u64(qualification.total_ms.saturating_add(elapsed_ms))),
        ("root_budget_tokens".into(), j_u64(recipe.design.budget.tokens)),
        ("root_budget_seconds".into(), j_u64(recipe.design.budget.seconds)),
        ("stop_requested".into(), J::Bool(stop_requested(ctx, &run))),
        ("in_flight_cancel_limit".into(), J::s("stop file is checked after each bounded request; it cannot interrupt an active loopback socket call")),
        ("run_log_error".into(), chain_error.as_deref().map_or(J::Null, J::s)),
        ("outputs".into(), J::Arr(outputs.iter().map(output_json).collect())),
    ]);
    let report_path = out_dir.join("task.json");
    write_new(&report_path, &report.compact())
        .map_err(|e| format!("could not preserve task report: {e}"))?;
    let receipt_path = ctx
        .agent_data
        .join("foundry")
        .join("receipts")
        .join(format!("{run}.json"));
    write_new(&receipt_path, &report.compact())
        .map_err(|e| format!("could not preserve task receipt: {e}"))?;
    let reason = if let Some(error) = chain_error {
        format!("receipt chain not verified: {error}")
    } else if stop_requested(ctx, &run) {
        "stop was requested; any in-flight request completed or timed out before the check".into()
    } else if provider_ok {
        "candidate output saved under the kept recipe".into()
    } else {
        "one or more model calls or artifact writes failed; raw outcomes are preserved".into()
    };
    Ok((run, status, reason, receipt_path))
}

fn one(
    log: &RunLog,
    out_dir: &Path,
    recipe: &Recipe,
    meter: &Meter,
    arm: &'static str,
    system: &str,
    task: &str,
) -> Result<Output, String> {
    let plan = log.emit(
        unix_secs(),
        Ev {
            parent: None,
            depth: 0,
            member: arm,
            engine: ENGINE,
            model: &recipe.model,
            kind: "plan",
            fields: vec![("system", J::s(system)), ("task", J::s(task))],
        },
    );
    let started = Instant::now();
    let reply = infer(
        recipe.port,
        &recipe.model,
        system,
        task,
        recipe.design.budget.request_tokens,
        false,
        meter,
    );
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let error = reply.error.clone();
    let rel = format!("{arm}/answer.txt");
    let (artifact, artifact_error) = match write_new(&out_dir.join(&rel), &reply.content) {
        Ok(()) => (Some(rel), None),
        Err(e) => (
            None,
            Some(format!("could not preserve {arm} task output: {e}")),
        ),
    };
    let result = log.emit(
        unix_secs(),
        Ev {
            parent: None,
            depth: 0,
            member: arm,
            engine: ENGINE,
            model: &recipe.model,
            kind: "result",
            fields: vec![
                (
                    "status",
                    J::s(if error.is_some() { "error" } else { "complete" }),
                ),
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
                ("elapsed_ms", j_u64(elapsed_ms)),
                (
                    "artifact_error",
                    artifact_error.as_deref().map_or(J::Null, J::s),
                ),
            ],
        },
    );
    Ok(Output {
        arm,
        answer: reply.content,
        body: reply.body,
        error,
        requested_model: reply.requested_model,
        response_model: reply.response_model,
        prompt_tokens: reply.usage.prompt,
        completion_tokens: reply.usage.completion,
        usage_source: reply.usage.source,
        elapsed_ms,
        prompt_ref: log.reference(plan),
        answer_ref: log.reference(result),
        artifact,
        artifact_error,
    })
}

pub(super) fn find_qualification(ctx: &Ctx, recipe: &Recipe) -> Result<Qualification, String> {
    runner::verify_current_recipe(ctx, recipe)?;
    let root = ctx.agent_data.join("foundry").join("exams");
    let mut dirs = fs::read_dir(&root)
        .map_err(|e| format!("exam history is unavailable: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    for dir in dirs.into_iter().rev() {
        let run = dir
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_string();
        if recipe.role == Role::Code {
            let mut proofs = fs::read_dir(&dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|x| x.to_str())
                        .is_some_and(|n| n.starts_with("code-check-") && n.ends_with(".json"))
                })
                .collect::<Vec<_>>();
            proofs.sort();
            for proof_path in proofs.into_iter().rev() {
                let Some(proof) = read_json(&proof_path) else {
                    continue;
                };
                if proof.str("status") != Some("kept")
                    || proof.get("keep") != Some(&J::Bool(true))
                    || proof.str("recipe_id") != Some(recipe.id.as_str())
                    || proof.str("run_id") != Some(run.as_str())
                    || proof.str("source_sha256") != Some(exam::CODE_SOURCE_SHA256)
                {
                    continue;
                }
                let (Some(base_hash), Some(candidate_hash)) =
                    (proof.str("baseline_sha256"), proof.str("candidate_sha256"))
                else {
                    continue;
                };
                if file_hash_matches(&dir.join("baseline_budget.rs"), base_hash)
                    && file_hash_matches(&dir.join("candidate_budget.rs"), candidate_hash)
                {
                    let exam_report = read_json(&dir.join("exam.json"));
                    if exam_report.as_ref().is_some_and(|r| {
                        r.str("exam_sha256") == Some(exam::exam_hash(Role::Code).as_str())
                            && r.str("source_sha256") == Some(exam::CODE_SOURCE_SHA256)
                            && runner::receipt_recipe_matches(
                                r.str("recipe_sha256"),
                                &recipe.recipe_sha256,
                            )
                    }) && verify_report_log(ctx, &run, exam_report.as_ref().unwrap()).is_ok()
                    {
                        return Ok(Qualification {
                            run,
                            total_tokens: exam_report
                                .as_ref()
                                .and_then(|r| integer_field(r, "total_prompt_completion_tokens"))
                                .unwrap_or(0),
                            total_ms: exam_report
                                .as_ref()
                                .and_then(|r| integer_field(r, "total_elapsed_ms"))
                                .unwrap_or(0),
                            receipt: proof_path.to_string_lossy().into_owned(),
                        });
                    }
                }
            }
        } else {
            let Some(report) = read_json(&dir.join("exam.json")) else {
                continue;
            };
            if report.str("status") != Some("kept")
                || report.str("recipe_id") != Some(recipe.id.as_str())
                || report.str("role") != Some(recipe.role.name())
                || !runner::receipt_recipe_matches(
                    report.str("recipe_sha256"),
                    &recipe.recipe_sha256,
                )
                || report.str("exam_sha256") != Some(exam::exam_hash(recipe.role).as_str())
                || report.str("source_sha256") != Some(exam::source_hash(recipe.role).as_str())
                || report.get("take_write_error") != Some(&J::Null)
                || report.get("run_log_error") != Some(&J::Null)
                || !matches!(
                    report.get("decision").and_then(|d| d.get("keep")),
                    Some(J::Bool(true))
                )
            {
                continue;
            }
            let cases = exam::cases(recipe.role);
            let Some(J::Arr(attempts)) = report.get("attempts") else {
                continue;
            };
            let good = cases.iter().all(|case| {
                attempts.iter().any(|a| {
                    a.str("arm") == Some("candidate")
                        && a.str("case") == Some(case.id)
                        && a.get("correct") == Some(&J::Bool(true))
                        && a.str("status") == Some("complete")
                })
            });
            if good && verify_report_log(ctx, &run, &report).is_ok() {
                return Ok(Qualification {
                    run,
                    total_tokens: integer_field(&report, "total_prompt_completion_tokens")
                        .unwrap_or(0),
                    total_ms: integer_field(&report, "total_elapsed_ms").unwrap_or(0),
                    receipt: dir.join("exam.json").to_string_lossy().into_owned(),
                });
            }
        }
    }
    Err("no current kept exam receipt exists for this recipe and frozen rubric".into())
}

fn verify_report_log(ctx: &Ctx, run: &str, report: &J) -> Result<(), String> {
    let path = runlog::find(ctx, run).ok_or("qualified exam log is missing")?;
    let count = runlog::verify(&path)?;
    let expected = report
        .get("attempts")
        .and_then(|v| match v {
            J::Arr(a) => Some(
                a.iter()
                    .filter_map(|a| a.str("answer_ref").and_then(line_number))
                    .max()
                    .unwrap_or(0),
            ),
            _ => None,
        })
        .ok_or("qualified report references are missing")?;
    if count < expected {
        Err("qualified log is shorter than its answer references".into())
    } else {
        Ok(())
    }
}

fn output_json(output: &Output) -> J {
    J::Obj(vec![
        ("arm".into(), J::s(output.arm)),
        (
            "status".into(),
            J::s(if output.error.is_some() {
                "error"
            } else {
                "complete"
            }),
        ),
        ("answer".into(), J::s(&output.answer)),
        ("provider_body".into(), J::s(&output.body)),
        (
            "error".into(),
            output.error.as_deref().map_or(J::Null, J::s),
        ),
        ("requested_model".into(), J::s(&output.requested_model)),
        (
            "response_model".into(),
            output.response_model.as_deref().map_or(J::Null, J::s),
        ),
        ("prompt_tokens".into(), j_u64(output.prompt_tokens)),
        ("completion_tokens".into(), j_u64(output.completion_tokens)),
        ("usage_source".into(), J::s(output.usage_source)),
        ("elapsed_ms".into(), j_u64(output.elapsed_ms)),
        ("prompt_ref".into(), J::s(&output.prompt_ref)),
        ("answer_ref".into(), J::s(&output.answer_ref)),
        (
            "artifact".into(),
            output.artifact.as_deref().map_or(J::Null, J::s),
        ),
        (
            "artifact_error".into(),
            output.artifact_error.as_deref().map_or(J::Null, J::s),
        ),
    ])
}

pub fn stop(ctx: &Ctx, args: &[String]) -> i32 {
    if args.len() != 1 || !safe_id(&args[0]) {
        eprintln!("Use: tempo-foundry stop RUN_ID");
        return 2;
    }
    let run = &args[0];
    let task_dir = ctx.agent_data.join("foundry").join("tasks").join(run);
    let team_dir = ctx.agent_data.join("foundry").join("teams").join(run);
    if !task_dir.is_dir() && !team_dir.is_dir() {
        eprintln!("no active foundry task or team has this run id");
        return 1;
    }
    let path = stop_path(ctx, run);
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("could not create stop folder: {e}");
            return 1;
        }
    }
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            if let Err(e) = writeln!(file, "stop requested at {}", unix_secs()) {
                eprintln!("could not save stop flag: {e}");
                return 1;
            }
            println!(
                "Stop requested for {run}; in-flight calls finish or time out before observing it."
            );
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            println!("Stop was already requested for {run}.");
            0
        }
        Err(e) => {
            eprintln!("could not write stop flag: {e}");
            1
        }
    }
}

pub(super) fn stop_path(ctx: &Ctx, run: &str) -> PathBuf {
    ctx.agent_data
        .join("foundry")
        .join("stops")
        .join(format!("{run}.stop"))
}

pub(super) fn stop_requested(ctx: &Ctx, run: &str) -> bool {
    stop_path(ctx, run).is_file()
}

fn read_json(path: &Path) -> Option<J> {
    parse(&fs::read_to_string(path).ok()?).ok()
}
fn file_hash_matches(path: &Path, expected: &str) -> bool {
    fs::read(path)
        .ok()
        .is_some_and(|b| sha256_hex(&b) == expected)
}
fn integer_field(value: &J, name: &str) -> Option<u64> {
    value.get(name).and_then(|x| match x {
        J::Num(n) => n.parse().ok(),
        _ => None,
    })
}
fn j_u64(n: u64) -> J {
    J::Num(n.to_string())
}
fn line_number(reference: &str) -> Option<usize> {
    reference.rsplit_once('#')?.1.parse().ok()
}
fn safe_id(s: &str) -> bool {
    (2..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}
