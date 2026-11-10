use super::exam;
use super::runner::Recipe;
use super::{exact_fields, write_new, Role};
use crate::setup::json::{parse, J};
use crate::setup::sha256::sha256_hex;
use crate::Ctx;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PINNED_CHECKER: &[u8] = include_bytes!("fixtures/code_exam_evaluate.py");
const PINNED_HARNESS: &[u8] = include_bytes!("fixtures/code_exam_overflow_tests.rs");
const PINNED_CHECKER_SHA256: &str =
    "e2fa575bca05baee2b753cca8f7c60ab7cd834853ba080e97c02f5abb03e79b5";
const PINNED_HARNESS_SHA256: &str =
    "90db05ba3878f00d256a8504ebbfd97b56c9882a2464a44d7f1c5c9d1738df13";

pub(super) fn import(ctx: &Ctx, recipe: &Recipe, proof_path: &str) -> i32 {
    match import_code_check_inner(ctx, recipe, proof_path) {
        Ok((run, keep, status, reason, receipt)) => {
            println!(
                "Code exam {status}: {reason}; run {run}; receipt {}",
                receipt.display()
            );
            if keep {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("Code proof refused: {e}");
            2
        }
    }
}

fn import_code_check_inner(
    ctx: &Ctx,
    recipe: &Recipe,
    proof_path: &str,
) -> Result<(String, bool, &'static str, String, PathBuf), String> {
    let proof_bytes =
        fs::read(proof_path).map_err(|e| format!("could not read owner proof: {e}"))?;
    if proof_bytes.len() > 2_000_000 {
        return Err("owner proof exceeds the 2 MB receipt limit".into());
    }
    let proof_text = std::str::from_utf8(&proof_bytes).map_err(|_| "owner proof is not UTF-8")?;
    let proof = parse(proof_text).map_err(|e| format!("owner proof JSON is invalid: {e}"))?;
    let required = [
        "v",
        "kind",
        "run_id",
        "recipe_id",
        "recipe_sha256",
        "baseline_sha256",
        "candidate_sha256",
    ];
    if exact_fields(&proof, &required).is_err() {
        let all = [
            "v",
            "kind",
            "run_id",
            "recipe_id",
            "recipe_sha256",
            "baseline_sha256",
            "candidate_sha256",
            "source_sha256",
            "harness_sha256",
        ];
        exact_fields(&proof, &all).map_err(|e| format!("proof bundle fields are invalid: {e}"))?;
    }
    if proof.f64("v") != Some(1.0)
        || proof.str("kind") != Some("foundry_code_check_bundle")
        || proof.str("recipe_id") != Some(recipe.id.as_str())
        || proof.str("recipe_sha256") != Some(recipe.recipe_sha256.as_str())
    {
        return Err("proof bundle version, kind, or saved recipe identity does not match".into());
    }
    let run = checked_text(
        proof.str("run_id").ok_or("proof run_id is missing")?,
        64,
        "run id",
    )?;
    if !safe_id(&run) {
        return Err("proof run id is not safe".into());
    }
    let run_dir = ctx.agent_data.join("foundry").join("exams").join(&run);
    let report_path = run_dir.join("exam.json");
    let report_text = fs::read_to_string(&report_path)
        .map_err(|e| format!("saved exam report is missing: {e}"))?;
    let report = parse(&report_text).map_err(|e| format!("saved exam report is invalid: {e}"))?;
    if report.str("kind") != Some("foundry_exam")
        || report.str("run") != Some(run.as_str())
        || report.str("recipe_id") != Some(recipe.id.as_str())
        || report.str("recipe_sha256") != Some(recipe.recipe_sha256.as_str())
        || report.str("role") != Some("code")
        || report.str("status") != Some("Untested")
    {
        return Err("proof does not point to an unqualified code exam".into());
    }
    let expected_exam_hash = exam::exam_hash(Role::Code);
    if report.str("exam_sha256") != Some(expected_exam_hash.as_str())
        || report.str("source_sha256") != Some(exam::CODE_SOURCE_SHA256)
        || report.str("source_base") != Some(exam::CODE_SOURCE_BASE)
    {
        return Err("saved exam or frozen code source identity changed".into());
    }
    if !saved_code_attempts_match(&report, &recipe.model) {
        return Err(
            "saved baseline and candidate provider attempts are incomplete or mismatched".into(),
        );
    }
    let baseline_file = run_dir.join("baseline_budget.rs");
    let candidate_file = run_dir.join("candidate_budget.rs");
    let baseline_hash = file_sha256(&baseline_file)?;
    let candidate_hash = file_sha256(&candidate_file)?;
    if proof.str("baseline_sha256") != Some(baseline_hash.as_str())
        || proof.str("candidate_sha256") != Some(candidate_hash.as_str())
    {
        return Err("proof bundle does not bind both saved model artifacts".into());
    }
    verify_report_artifact(&report, "baseline", &baseline_hash)?;
    verify_report_artifact(&report, "candidate", &candidate_hash)?;

    let runlog = crate::conductor::runlog::find(ctx, &run).ok_or("exam run log is missing")?;
    let verified_lines = crate::conductor::runlog::verify(&runlog)
        .map_err(|e| format!("exam run log did not verify: {e}"))?;
    let expected_lines = report
        .get("attempts")
        .and_then(|v| match v {
            J::Arr(items) => Some(items),
            _ => None,
        })
        .ok_or("saved exam attempts are missing")?
        .iter()
        .filter_map(|attempt| attempt.str("answer_ref").and_then(line_number))
        .max()
        .unwrap_or(0);
    if verified_lines < expected_lines {
        return Err(format!("exam log is shorter than its answer refs: verified {verified_lines}, expected at least {expected_lines}"));
    }

    let expected_source = exam::CODE_SOURCE_SHA256;
    let (baseline, candidate, native_run, harness_hash) =
        run_pinned_check(&run_dir, &baseline_file, &candidate_file)?;
    let (base_state, base_harness) =
        validate_checker_result(&baseline, &baseline_hash, expected_source, false)?;
    let (candidate_state, candidate_harness) =
        validate_checker_result(&candidate, &candidate_hash, expected_source, true)?;
    if base_harness != harness_hash || candidate_harness != harness_hash {
        return Err("native checker harness identity does not match the pinned fixture".into());
    }
    if proof
        .str("source_sha256")
        .is_some_and(|h| h != expected_source)
        || proof
            .str("harness_sha256")
            .is_some_and(|h| h != harness_hash)
    {
        return Err(
            "bundle-level source or harness identity does not match checker results".into(),
        );
    }

    let (status, keep, reason) = match candidate_state {
        CheckerState::Pass if base_state != CheckerState::Untested => ("kept", true, "candidate passed all original budget tests and five independent overflow regressions; baseline comparison is bound to the same source and harness".to_string()),
        CheckerState::Untested => ("Untested", false, "candidate compile or test evidence is incomplete; no recipe keep granted".to_string()),
        CheckerState::Fail => ("rejected", false, "candidate did not pass the required original tests and five overflow regressions".to_string()),
        CheckerState::Pass => ("Untested", false, "baseline proof is incomplete; candidate output is preserved without a keep".to_string()),
    };
    let proof_hash = sha256_hex(&proof_bytes);
    let receipt = J::Obj(vec![
        ("v".into(), J::n(1.0)),
        ("kind".into(), J::s("foundry_code_check")),
        ("run_id".into(), J::s(&run)),
        ("recipe_id".into(), J::s(&recipe.id)),
        ("recipe_sha256".into(), J::s(&recipe.recipe_sha256)),
        ("status".into(), J::s(status)),
        ("keep".into(), J::Bool(keep)),
        ("reason".into(), J::s(&reason)),
        ("proof_sha256".into(), J::s(&proof_hash)),
        ("baseline_sha256".into(), J::s(&baseline_hash)),
        ("candidate_sha256".into(), J::s(&candidate_hash)),
        ("source_sha256".into(), J::s(expected_source)),
        ("harness_sha256".into(), J::s(&harness_hash)),
        ("baseline_checker".into(), baseline.clone()),
        ("candidate_checker".into(), candidate.clone()),
        ("native_checker".into(), native_run),
    ]);
    let receipt_path = run_dir.join(format!("code-check-{proof_hash}.json"));
    write_new(&receipt_path, &receipt.compact())
        .map_err(|e| format!("could not preserve code proof receipt: {e}"))?;
    Ok((run, keep, status, reason, receipt_path))
}

fn saved_code_attempts_match(report: &J, model: &str) -> bool {
    if report.str("requested_model") != Some(model) {
        return false;
    }
    let Some(J::Arr(attempts)) = report.get("attempts") else {
        return false;
    };
    let mut baseline = false;
    let mut candidate = false;
    for attempt in attempts
        .iter()
        .filter(|attempt| attempt.str("case") == Some("budget-counter-fix"))
    {
        if attempt.str("status") != Some("complete")
            || !matches!(attempt.get("error"), Some(J::Null))
            || attempt.str("response_model") != Some(model)
        {
            return false;
        }
        match attempt.str("arm") {
            Some("baseline") if !baseline => baseline = true,
            Some("candidate") if !candidate => candidate = true,
            _ => return false,
        }
    }
    baseline && candidate
}

fn run_pinned_check(
    run_dir: &Path,
    baseline_file: &Path,
    candidate_file: &Path,
) -> Result<(J, J, J, String), String> {
    if !cfg!(target_os = "linux") {
        return Err("native code checks require Linux and the pinned sandbox tools".into());
    }
    let checker_hash = sha256_hex(PINNED_CHECKER);
    let harness_hash = sha256_hex(PINNED_HARNESS);
    if checker_hash != PINNED_CHECKER_SHA256 || harness_hash != PINNED_HARNESS_SHA256 {
        return Err("pinned code checker or overflow harness identity changed".into());
    }
    let source_hash = sha256_hex(exam::BUDGET_SOURCE.as_bytes());
    if source_hash != exam::CODE_SOURCE_SHA256 {
        return Err("frozen budget source identity changed".into());
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("system clock is invalid for native checker run: {e}"))?
        .as_nanos();
    let root = run_dir.join(format!("native-check-{}-{nonce}", std::process::id()));
    fs::create_dir(&root).map_err(|e| format!("could not preserve native checker run: {e}"))?;
    let fixtures = root.join("fixtures");
    fs::create_dir(&fixtures).map_err(|e| format!("could not create checker fixtures: {e}"))?;
    let checker_path = fixtures.join("evaluate.py");
    let harness_path = fixtures.join("overflow_tests.rs");
    let source_path = fixtures.join("budget_before.rs");
    fs::write(&checker_path, PINNED_CHECKER)
        .map_err(|e| format!("could not materialize pinned evaluator: {e}"))?;
    fs::write(&harness_path, PINNED_HARNESS)
        .map_err(|e| format!("could not materialize pinned harness: {e}"))?;
    fs::write(&source_path, exam::BUDGET_SOURCE.as_bytes())
        .map_err(|e| format!("could not materialize frozen budget source: {e}"))?;
    let (baseline, baseline_run) = run_checker_arm(
        run_dir,
        &root,
        &checker_path,
        &source_path,
        baseline_file,
        "baseline",
    )?;
    let (candidate, candidate_run) = run_checker_arm(
        run_dir,
        &root,
        &checker_path,
        &source_path,
        candidate_file,
        "candidate",
    )?;
    let python_hash = file_sha256(Path::new("/usr/bin/python3"))?;
    let native = J::Obj(vec![
        ("python".into(), J::s("/usr/bin/python3")),
        ("python_sha256".into(), J::s(&python_hash)),
        ("checker_sha256".into(), J::s(&checker_hash)),
        ("harness_sha256".into(), J::s(&harness_hash)),
        ("source_sha256".into(), J::s(&source_hash)),
        ("baseline_run".into(), baseline_run),
        ("candidate_run".into(), candidate_run),
    ]);
    Ok((baseline, candidate, native, harness_hash))
}

fn run_checker_arm(
    run_dir: &Path,
    root: &Path,
    checker: &Path,
    source: &Path,
    artifact: &Path,
    arm: &str,
) -> Result<(J, J), String> {
    let out = root.join(arm);
    let stdout_path = root.join(format!("{arm}-python.stdout.log"));
    let stderr_path = root.join(format!("{arm}-python.stderr.log"));
    let argv = vec![
        "/usr/bin/python3".to_string(),
        "-I".into(),
        "-S".into(),
        checker.to_string_lossy().into_owned(),
        "--source".into(),
        source.to_string_lossy().into_owned(),
        "--candidate".into(),
        artifact.to_string_lossy().into_owned(),
        "--out".into(),
        out.to_string_lossy().into_owned(),
    ];
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME")
        .env_remove("PYTHONSTARTUP")
        .output()
        .map_err(|e| format!("pinned /usr/bin/python3 checker could not start: {e}"))?;
    fs::write(&stdout_path, &output.stdout)
        .map_err(|e| format!("could not preserve checker stdout: {e}"))?;
    fs::write(&stderr_path, &output.stderr)
        .map_err(|e| format!("could not preserve checker stderr: {e}"))?;
    let result_path = out.join("result.json");
    let result_text = fs::read_to_string(&result_path).map_err(|e| {
        format!(
            "pinned checker produced no {} result.json (stdout {}, stderr {}): {e}",
            arm,
            stdout_path.display(),
            stderr_path.display()
        )
    })?;
    let result =
        parse(&result_text).map_err(|e| format!("native {arm} checker result is invalid: {e}"))?;
    let artifact_hash = file_sha256(artifact)?;
    if result.str("source_sha256") != Some(exam::CODE_SOURCE_SHA256)
        || result.str("candidate_sha256") != Some(artifact_hash.as_str())
        || result.str("harness_sha256") != Some(PINNED_HARNESS_SHA256)
    {
        return Err(format!(
            "native {arm} checker did not bind the frozen inputs"
        ));
    }
    let status = output.status.code();
    if (status == Some(0)) != (result.str("status") == Some("Verified complete")) {
        return Err(format!(
            "native {arm} checker exit status disagrees with its result"
        ));
    }
    let rel = |path: &Path| {
        path.strip_prefix(run_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let execution = J::Obj(vec![
        ("argv".into(), J::strings(&argv)),
        (
            "returncode".into(),
            status.map_or(J::Null, |code| J::n(code as f64)),
        ),
        ("stdout_file".into(), J::s(&rel(&stdout_path))),
        ("stdout_sha256".into(), J::s(&sha256_hex(&output.stdout))),
        ("stderr_file".into(), J::s(&rel(&stderr_path))),
        ("stderr_sha256".into(), J::s(&sha256_hex(&output.stderr))),
        ("result_file".into(), J::s(&rel(&result_path))),
        (
            "result_sha256".into(),
            J::s(&sha256_hex(result_text.as_bytes())),
        ),
    ]);
    Ok((result, execution))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckerState {
    Pass,
    Fail,
    Untested,
}

fn validate_checker_result(
    value: &J,
    artifact_hash: &str,
    source_hash: &str,
    candidate_arm: bool,
) -> Result<(CheckerState, String), String> {
    exact_fields(
        value,
        &[
            "status",
            "reason",
            "commands",
            "tests",
            "source",
            "source_sha256",
            "candidate",
            "candidate_sha256",
            "harness",
            "harness_sha256",
            "compiled_source",
            "compiled_source_sha256",
            "compiler",
            "compiler_sha256",
            "toolchain",
            "sandbox",
            "expected_tests",
            "compiled_binary",
            "compiled_binary_sha256",
            "resource_limits",
        ],
    )
    .map_err(|e| format!("checker result fields are invalid: {e}"))?;
    let status = value.str("status").ok_or("checker status is missing")?;
    if !matches!(status, "Verified complete" | "In progress" | "Untested") {
        return Err("checker status is not recognized".into());
    }
    validate_resource_limits(
        value
            .get("resource_limits")
            .ok_or("checker resource limits are missing")?,
        false,
    )?;
    if value.str("source_sha256") != Some(source_hash)
        || value.str("candidate_sha256") != Some(artifact_hash)
    {
        return Err("checker source or artifact hash does not match".into());
    }
    if status == "Untested" {
        let harness = value
            .str("harness_sha256")
            .filter(|h| is_sha256(h))
            .unwrap_or("unavailable");
        return Ok((CheckerState::Untested, harness.to_string()));
    }
    let harness = value
        .str("harness_sha256")
        .ok_or("checker harness hash is missing")?;
    if !is_sha256(harness) {
        return Err("checker harness hash is invalid".into());
    }
    for field in [
        "compiled_source_sha256",
        "compiler_sha256",
        "compiled_binary_sha256",
    ] {
        if !value.str(field).is_some_and(is_sha256) {
            return Err(format!("checker {field} is missing or invalid"));
        }
    }
    let original_names = [
        "tests::a_limit_ends_the_run_and_says_which",
        "tests::what_a_child_uses_counts_above_and_a_parent_limit_stops_the_child",
        "tests::a_limit_from_above_is_told_with_the_figures_of_the_member_that_set_it",
        "tests::one_stop_at_the_top_ends_every_member_and_a_child_stop_stays_below",
        "tests::time_and_money_are_limits_too_and_zero_means_none",
        "tests::members_on_other_threads_share_the_counters",
    ];
    let fixed_names = exam::CODE_TESTS.map(|name| format!("code_exam_tests::{name}"));
    validate_expected_group(
        value
            .get("expected_tests")
            .ok_or("expected test names are missing")?,
        "original_budget_tests",
        &original_names,
    )?;
    let fixed_expected: Vec<&str> = fixed_names.iter().map(String::as_str).collect();
    validate_expected_group(
        value
            .get("expected_tests")
            .ok_or("expected test names are missing")?,
        "fixed_overflow_exam",
        &fixed_expected,
    )?;
    let tests = value.get("tests").ok_or("test outcomes are missing")?;
    exact_fields(
        tests,
        &["original_budget_tests", "fixed_overflow_exam", "other"],
    )
    .map_err(|e| format!("test group fields are invalid: {e}"))?;
    if !matches!(tests.get("other"), Some(J::Arr(items)) if items.is_empty()) {
        return Err(
            "checker reported tests outside the frozen original and overflow suites".into(),
        );
    }
    let original = validate_outcomes(
        tests
            .get("original_budget_tests")
            .ok_or("original test outcomes are missing")?,
        &original_names,
    )?;
    let fixed = validate_outcomes(
        tests
            .get("fixed_overflow_exam")
            .ok_or("overflow test outcomes are missing")?,
        &fixed_expected,
    )?;
    let commands = value
        .get("commands")
        .ok_or("checker command receipts are missing")?;
    exact_fields(commands, &["rustup_which", "rustc", "tests"])
        .map_err(|e| format!("checker command fields are invalid: {e}"))?;
    validate_command_record(
        commands
            .get("rustup_which")
            .ok_or("rustup command receipt is missing")?,
    )?;
    let compile = command_result(commands.get("rustc").ok_or("compiler receipt is missing")?)?;
    let test_run = command_result(
        commands
            .get("tests")
            .ok_or("test command receipt is missing")?,
    )?;
    if status == "Untested"
        || compile.1
        || test_run.1
        || compile.0.is_none()
        || test_run.0.is_none()
    {
        return Ok((CheckerState::Untested, harness.to_string()));
    }
    if compile.0 != Some(0) {
        return Ok((
            if candidate_arm && status == "In progress" {
                CheckerState::Fail
            } else {
                CheckerState::Untested
            },
            harness.to_string(),
        ));
    }
    if !candidate_arm && original.iter().any(|o| o != "ok") {
        return Ok((CheckerState::Untested, harness.to_string()));
    }
    if candidate_arm {
        let pass = status == "Verified complete"
            && test_run.0 == Some(0)
            && original.iter().all(|o| o == "ok")
            && fixed.iter().all(|o| o == "ok");
        Ok((
            if pass {
                CheckerState::Pass
            } else {
                CheckerState::Fail
            },
            harness.to_string(),
        ))
    } else {
        let status_consistent = if fixed.iter().all(|o| o == "ok") {
            status == "Verified complete" && test_run.0 == Some(0)
        } else {
            status == "In progress" && test_run.0 != Some(0)
        };
        Ok((
            if status_consistent {
                CheckerState::Pass
            } else {
                CheckerState::Untested
            },
            harness.to_string(),
        ))
    }
}

fn validate_expected_group(value: &J, group: &str, expected: &[&str]) -> Result<(), String> {
    exact_fields(value, &["original_budget_tests", "fixed_overflow_exam"])
        .map_err(|e| format!("expected test groups are invalid: {e}"))?;
    let arr = match value.get(group) {
        Some(J::Arr(a)) => a,
        _ => return Err(format!("expected test names for {group} are missing")),
    };
    let mut names: Vec<&str> = arr
        .iter()
        .map(|v| match v {
            J::Str(name) => Ok(name.as_str()),
            _ => Err("expected test name is not a string"),
        })
        .collect::<Result<_, _>>()?;
    let mut wanted = expected.to_vec();
    names.sort_unstable();
    wanted.sort_unstable();
    if names != wanted {
        return Err(format!(
            "expected test list for {group} differs from the frozen suite"
        ));
    }
    Ok(())
}

fn validate_outcomes(value: &J, expected: &[&str]) -> Result<Vec<String>, String> {
    exact_fields(value, &["count", "outcomes"])
        .map_err(|e| format!("test outcome group is invalid: {e}"))?;
    if integer(value.get("count").ok_or("test outcome count is missing")?)
        != Some(expected.len() as u64)
    {
        return Err("test outcome count does not match the frozen suite".into());
    }
    let rows = match value.get("outcomes") {
        Some(J::Arr(a)) if a.len() == expected.len() => a,
        _ => return Err("test outcomes do not match the frozen suite length".into()),
    };
    let mut outcomes = Vec::with_capacity(rows.len());
    let mut names = Vec::with_capacity(rows.len());
    for row in rows {
        exact_fields(row, &["name", "outcome"])
            .map_err(|e| format!("individual test result is invalid: {e}"))?;
        names.push(row.str("name").ok_or("test outcome name is missing")?);
        let outcome = row.str("outcome").ok_or("test outcome is missing")?;
        if !matches!(outcome, "ok" | "FAILED") {
            return Err("test outcome is not recognized".into());
        }
        outcomes.push(outcome.to_string());
    }
    let mut wanted = expected.to_vec();
    names.sort_unstable();
    wanted.sort_unstable();
    if names != wanted {
        return Err("test outcome names differ from the frozen suite".into());
    }
    Ok(outcomes)
}

fn command_result(value: &J) -> Result<(Option<i64>, bool), String> {
    validate_command_record(value)?;
    let code = match value.get("returncode") {
        Some(J::Null) => None,
        Some(J::Num(n)) => Some(
            i64::try_from(
                n.parse::<u64>()
                    .map_err(|_| "checker command return code is invalid")?,
            )
            .map_err(|_| "checker return code is out of range")?,
        ),
        _ => return Err("checker command return code is missing".into()),
    };
    let timed_out = match value.get("timed_out") {
        Some(J::Bool(b)) => *b,
        _ => return Err("checker command timeout flag is missing".into()),
    };
    Ok((code, timed_out))
}

fn validate_resource_limits(value: &J, command: bool) -> Result<bool, String> {
    if command {
        exact_fields(
            value,
            &[
                "address_space_bytes",
                "cpu_seconds",
                "processes",
                "output_file_bytes",
                "wall_timeout_seconds",
                "launcher",
                "launcher_available",
                "launch_attempted",
            ],
        )
        .map_err(|e| format!("command resource limits are invalid: {e}"))?;
    } else {
        exact_fields(
            value,
            &[
                "address_space_bytes",
                "cpu_seconds",
                "processes",
                "output_file_bytes",
                "wall_timeout_seconds",
                "launcher",
                "required",
            ],
        )
        .map_err(|e| format!("checker resource limits are invalid: {e}"))?;
    }
    if integer(
        value
            .get("address_space_bytes")
            .ok_or("address-space limit is missing")?,
    ) != Some(2_147_483_648)
        || integer(value.get("cpu_seconds").ok_or("CPU limit is missing")?) != Some(90)
        || integer(value.get("processes").ok_or("process limit is missing")?) != Some(512)
        || integer(
            value
                .get("output_file_bytes")
                .ok_or("output-file limit is missing")?,
        ) != Some(67_108_864)
        || integer(
            value
                .get("wall_timeout_seconds")
                .ok_or("wall-time limit is missing")?,
        ) != Some(90)
        || value.str("launcher") != Some("/usr/bin/prlimit")
    {
        return Err("checker resource limits do not match the required bounds".into());
    }
    if command {
        let available = match value.get("launcher_available") {
            Some(J::Bool(value)) => *value,
            _ => return Err("resource-limiter availability is missing".into()),
        };
        let attempted = match value.get("launch_attempted") {
            Some(J::Bool(value)) => *value,
            _ => return Err("resource-limited launch state is missing".into()),
        };
        if attempted && !available {
            return Err("resource-limited command claims launch without prlimit".into());
        }
        Ok(attempted)
    } else if matches!(value.get("required"), Some(J::Bool(true))) {
        Ok(true)
    } else {
        Err("checker resource limits are not marked required".into())
    }
}

fn validate_command_record(value: &J) -> Result<(), String> {
    exact_fields(
        value,
        &[
            "argv",
            "resource_limits",
            "returncode",
            "timed_out",
            "timeout_seconds",
            "elapsed_seconds",
            "stdout",
            "stderr",
        ],
    )
    .map_err(|e| format!("checker command receipt fields are invalid: {e}"))?;
    if !validate_resource_limits(
        value
            .get("resource_limits")
            .ok_or("command resource limits are missing")?,
        true,
    )? {
        return Err("checker command was not launched with prlimit".into());
    }
    if integer(
        value
            .get("timeout_seconds")
            .ok_or("command wall timeout is missing")?,
    )
    .is_none_or(|seconds| seconds == 0 || seconds > 90)
    {
        return Err("checker command wall timeout exceeds 90 seconds".into());
    }
    let argv = match value.get("argv") {
        Some(J::Arr(items)) => items,
        _ => return Err("checker command argv is missing".into()),
    };
    let required = [
        "/usr/bin/prlimit",
        "--as=2147483648",
        "--cpu=90",
        "--nproc=512",
        "--fsize=67108864",
        "--",
    ];
    if argv.len() <= required.len()
        || !argv
            .iter()
            .zip(required)
            .all(|(actual, expected)| matches!(actual, J::Str(value) if value == expected))
    {
        return Err("checker command argv lacks the required prlimit bounds".into());
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("saved model artifact is missing: {e}"))?;
    Ok(sha256_hex(&bytes))
}

fn verify_report_artifact(report: &J, arm: &str, hash: &str) -> Result<(), String> {
    let list = match report.get("code_artifacts") {
        Some(J::Arr(a)) => a,
        _ => return Err("saved code artifacts are missing".into()),
    };
    let artifact = list
        .iter()
        .find(|a| a.str("arm") == Some(arm))
        .ok_or_else(|| format!("saved {arm} artifact receipt is missing"))?;
    let expected_file = format!("{arm}_budget.rs");
    if artifact.str("file") != Some(expected_file.as_str()) || artifact.str("sha256") != Some(hash)
    {
        return Err(format!(
            "saved {arm} artifact receipt does not match the file"
        ));
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn line_number(reference: &str) -> Option<usize> {
    reference.rsplit_once('#')?.1.parse().ok()
}

fn integer(value: &J) -> Option<u64> {
    if let J::Num(n) = value {
        n.parse().ok()
    } else {
        None
    }
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

pub(super) fn prompt() -> Result<String, String> {
    let source = exam::BUDGET_SOURCE;
    let methods = ["charge_tokens", "charge_tool", "charge_cents"]
        .iter()
        .map(|name| {
            let range = method_range(source, name)
                .ok_or_else(|| format!("frozen target method {name} is missing"))?;
            Ok(source[range].trim().to_string())
        })
        .collect::<Result<Vec<_>, String>>()?
        .join("\n\n");
    Ok(format!("Fix the actual wrapping atomic additions in `charge_tokens`, `charge_tool`, and `charge_cents` in this frozen `budget.rs`. Each counter must saturate at u64::MAX instead of wrapping, including concurrent charges; reaching a configured counter limit must still stop that member, and a child charge must preserve child-to-parent-to-root propagation and halt visibility. Preserve ordinary increments and SeqCst ordering. Return only three complete methods in this exact marker format, with each method body indented as Rust source:\n=== method:charge_tokens ===\n    pub fn charge_tokens(...) {{ ... }}\n=== end ===\n=== method:charge_tool ===\n    pub fn charge_tool(...) {{ ... }}\n=== end ===\n=== method:charge_cents ===\n    pub fn charge_cents(...) {{ ... }}\n=== end ===\nThe independent host harness checks overflow, child/root propagation, and concurrency. It is not part of this prompt. Qualified paths may use only `Ordering::SeqCst` and `u64::MAX` (spaces around `::` are allowed); do not add imports or refer to I/O or process APIs. Replacement method bodies may contain no comments, string/character literals, macro invocations, attributes, or unsafe code; use none of //, /*, ', \", !, or # tokens. Include no prose.\n\nOnly these target methods are shown; original tests are withheld:\n<target_methods>\n{methods}\n</target_methods>"))
}

pub(super) fn save_artifact(
    out_dir: &Path,
    arm: &str,
    answer: &str,
    source: &str,
) -> Result<Option<String>, String> {
    let Some(candidate) = assemble_budget(source, answer) else {
        return Ok(None);
    };
    let path = out_dir.join(format!("{arm}_budget.rs"));
    write_new(&path, &candidate)
        .map_err(|e| format!("could not save paired {arm} code artifact: {e}"))?;
    Ok(Some(format!("{arm}_budget.rs")))
}

pub(super) fn artifact_receipt(arm: &str, out_dir: &Path, relative: &str) -> Result<J, String> {
    let path = out_dir.join(relative);
    let bytes =
        fs::read(&path).map_err(|e| format!("could not read saved {arm} code artifact: {e}"))?;
    Ok(J::Obj(vec![
        ("arm".into(), J::s(arm)),
        ("file".into(), J::s(relative)),
        ("sha256".into(), J::s(&sha256_hex(&bytes))),
    ]))
}

fn assemble_budget(source: &str, answer: &str) -> Option<String> {
    let bodies = method_blocks(answer)?;
    if bodies.values().any(|body| !replacement_body_is_safe(body)) {
        return None;
    }
    let mut ranges = ["charge_tokens", "charge_tool", "charge_cents"]
        .map(|name| method_range(source, name))
        .into_iter()
        .collect::<Option<Vec<_>>>()?;
    if ranges.len() != 3 {
        return None;
    }
    let mut out = source.to_string();
    for (name, range) in ["charge_tokens", "charge_tool", "charge_cents"]
        .iter()
        .zip(ranges.drain(..))
        .rev()
    {
        let replacement = bodies.get(*name)?.clone();
        out.replace_range(range, &replacement);
    }
    Some(out)
}

fn replacement_body_is_safe(body: &str) -> bool {
    if body.contains("//")
        || body.contains("/*")
        || body.chars().any(|c| matches!(c, '\'' | '"' | '!' | '#'))
        || ["unsafe", "use", "extern"].iter().any(|word| {
            body.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|token| token == *word)
        })
    {
        return false;
    }

    let bytes = body.as_bytes();
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index] != b':' || bytes[index + 1] != b':' {
            continue;
        }

        let mut left_end = index;
        while left_end > 0 && bytes[left_end - 1].is_ascii_whitespace() {
            left_end -= 1;
        }
        let mut left_start = left_end;
        while left_start > 0 && path_identifier_byte(bytes[left_start - 1]) {
            left_start -= 1;
        }

        let mut right_start = index + 2;
        while right_start < bytes.len() && bytes[right_start].is_ascii_whitespace() {
            right_start += 1;
        }
        let mut right_end = right_start;
        while right_end < bytes.len() && path_identifier_byte(bytes[right_end]) {
            right_end += 1;
        }

        if !matches!(
            (&body[left_start..left_end], &body[right_start..right_end]),
            ("Ordering", "SeqCst") | ("u64", "MAX")
        ) {
            return false;
        }
    }
    true
}

fn path_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

fn method_blocks(answer: &str) -> Option<std::collections::HashMap<&'static str, String>> {
    let mut text = answer.trim();
    if text.starts_with("```rust") && text.ends_with("```") {
        text = text
            .strip_prefix("```rust")?
            .trim_start()
            .strip_suffix("```")?
            .trim_end();
    }
    let mut out = std::collections::HashMap::new();
    for name in ["charge_tokens", "charge_tool", "charge_cents"] {
        let open = format!("=== method:{name} ===");
        let start = text.find(&open)? + open.len();
        let tail = &text[start..];
        let end = tail.find("=== end ===")?;
        if !text[..start - open.len()].trim().is_empty()
            || tail[end + "=== end ===".len()..].contains(&open)
        {
            return None;
        }
        let body = tail[..end].trim_matches(['\r', '\n']);
        if !body.contains(&format!("pub fn {name}(")) {
            return None;
        }
        out.insert(name, body.to_string());
        text = tail[end + "=== end ===".len()..].trim_start();
    }
    if !text.trim().is_empty() {
        return None;
    }
    Some(out)
}

fn method_range(source: &str, name: &str) -> Option<std::ops::Range<usize>> {
    let needle = format!("pub fn {name}(");
    let start = source.find(&needle)?;
    if source[start + needle.len()..].contains(&needle) {
        return None;
    }
    let line_start = source[..start].rfind('\n').map_or(0, |n| n + 1);
    let open = source[start..].find('{')? + start;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in source.as_bytes()[open..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(line_start..open + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod replacement_safety_tests {
    use super::replacement_body_is_safe;

    #[test]
    fn rejects_unapproved_qualified_paths_and_imports() {
        for body in [
            "let _ = std::process::id;",
            "let _ = std::io::stdout;",
            "let _ = ::Ordering::SeqCst;",
            "let _ = std :: io::stdout;",
            "let _ = Ordering::SeqCst::MAX;",
            "use std::io::stdout;",
            "extern crate std;",
        ] {
            assert!(!replacement_body_is_safe(body), "accepted {body}");
        }
    }

    #[test]
    fn accepts_saturating_fetch_update_with_approved_paths() {
        assert!(replacement_body_is_safe(
            "pub fn charge_tokens(&self, n: u64) { self.tokens.fetch_update(Ordering :: SeqCst, Ordering::SeqCst, |current| Some(current.saturating_add(n).min(u64 :: MAX))); }"
        ));
    }
}

#[cfg(test)]
mod saved_attempt_tests {
    use super::{saved_code_attempts_match, J};

    fn attempt(arm: &str, status: &str, error: Option<&str>, model: &str) -> J {
        J::Obj(vec![
            ("arm".into(), J::s(arm)),
            ("case".into(), J::s("budget-counter-fix")),
            ("status".into(), J::s(status)),
            ("error".into(), error.map_or(J::Null, J::s)),
            ("response_model".into(), J::s(model)),
        ])
    }

    fn report(model: &str, attempts: Vec<J>) -> J {
        J::Obj(vec![
            ("requested_model".into(), J::s(model)),
            ("attempts".into(), J::Arr(attempts)),
        ])
    }

    fn complete_pair() -> Vec<J> {
        vec![
            attempt("baseline", "complete", None, "model-a"),
            attempt("candidate", "complete", None, "model-a"),
        ]
    }

    #[test]
    fn accepts_one_complete_model_matched_baseline_candidate_pair() {
        assert!(saved_code_attempts_match(
            &report("model-a", complete_pair()),
            "model-a"
        ));
    }

    #[test]
    fn rejects_provider_errors_wrong_models_and_truncation() {
        assert!(!saved_code_attempts_match(
            &report(
                "model-a",
                vec![
                    attempt("baseline", "complete", None, "model-a"),
                    attempt("candidate", "error", Some("provider failed"), "model-a"),
                ]
            ),
            "model-a"
        ));
        assert!(!saved_code_attempts_match(
            &report(
                "model-a",
                vec![
                    attempt("baseline", "complete", None, "model-a"),
                    attempt("candidate", "complete", Some("provider failed"), "model-a"),
                ]
            ),
            "model-a"
        ));
        assert!(!saved_code_attempts_match(
            &report(
                "model-a",
                vec![
                    attempt("baseline", "complete", None, "model-a"),
                    attempt(
                        "candidate",
                        "truncated",
                        Some("answer truncated"),
                        "model-a"
                    ),
                ]
            ),
            "model-a"
        ));
        assert!(!saved_code_attempts_match(
            &report(
                "model-a",
                vec![
                    attempt("baseline", "complete", None, "model-a"),
                    attempt("candidate", "complete", None, "model-b"),
                ]
            ),
            "model-a"
        ));
        assert!(!saved_code_attempts_match(
            &report("model-b", complete_pair()),
            "model-a"
        ));
    }

    #[test]
    fn rejects_missing_or_duplicate_code_attempts() {
        assert!(!saved_code_attempts_match(
            &report(
                "model-a",
                vec![attempt("baseline", "complete", None, "model-a")]
            ),
            "model-a"
        ));
        let mut duplicate = complete_pair();
        duplicate.push(attempt("candidate", "complete", None, "model-a"));
        assert!(!saved_code_attempts_match(
            &report("model-a", duplicate),
            "model-a"
        ));
    }
}
