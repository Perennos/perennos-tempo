use super::runner::{self, Recipe};
use super::task;
use super::{infer_for_sections, unix_secs, write_new, Role};
use crate::conductor::budget::{Budget, Meter};
use crate::conductor::checks::NoChecks;
use crate::conductor::engines::{self, Cast, Engine, Model};
use crate::conductor::runlog::{self, Ev, RunLog};
use crate::conductor::sections::Tree;
use crate::conductor::settings::{self, Settings};
use crate::conductor::spec::{Band, Every, Flow, May, Member};
use crate::conductor::takes_file;
use crate::conductor::{Chat, Said};
use crate::setup::json::J;
use crate::setup::sha256::sha256_hex;
use crate::Ctx;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ENGINE: &str = "openai-compatible-loopback";

struct Route {
    recipe: Recipe,
    role_prompt: String,
    engine: Engine,
}

struct CallRecord {
    role: String,
    recipe_role: String,
    role_source: String,
    recipe_id: String,
    engine_id: String,
    url: String,
    requested_model: String,
    response_model: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
    max_output_tokens: u64,
    usage_source: &'static str,
    elapsed_ms: u64,
    answer: String,
    provider_body: String,
    error: Option<String>,
    prompt_ref: String,
    answer_ref: String,
}

struct FoundryChat<'a> {
    agent_data: PathBuf,
    routes: Vec<Route>,
    log: &'a RunLog,
    provider_gate: Mutex<()>,
    calls: Mutex<Vec<CallRecord>>,
}

pub fn run(ctx: &Ctx, args: &[String]) -> i32 {
    let (ids, task_path) = match parse_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut recipes = Vec::new();
    let mut qualifications = Vec::new();
    for (id, role) in ids
        .into_iter()
        .zip([Role::Research, Role::Code, Role::Content])
    {
        let recipe = match runner::load_recipe(ctx, id) {
            Ok(v) if v.role == role => v,
            Ok(_) => {
                eprintln!(
                    "recipe {id} does not have the required {} role",
                    role.name()
                );
                return 2;
            }
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        };
        let qualification = match task::find_qualification(ctx, &recipe) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("team refused: {e}");
                return 1;
            }
        };
        recipes.push(recipe);
        qualifications.push(qualification);
    }
    for recipe in &recipes {
        if let Err(e) = runner::verify_current_recipe(ctx, recipe) {
            eprintln!("team refused: {e}");
            return 1;
        }
    }
    let task_bytes = match fs::read(task_path) {
        Ok(v) if !v.is_empty() && v.len() <= 64 * 1024 => v,
        Ok(_) => {
            eprintln!("task file must contain 1 to 65536 bytes");
            return 2;
        }
        Err(e) => {
            eprintln!("could not read task file: {e}");
            return 1;
        }
    };
    let task_text = match String::from_utf8(task_bytes) {
        Ok(v)
            if !v.trim().is_empty()
                && v.chars()
                    .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')) =>
        {
            v
        }
        _ => {
            eprintln!(
                "team task must be nonempty UTF-8 text without disallowed control characters"
            );
            return 2;
        }
    };
    match execute(ctx, recipes, qualifications, task_path, &task_text) {
        Ok((run, status, reason, receipt)) => {
            println!(
                "Team {status}: {reason}; run {run}; receipt {}",
                receipt.display()
            );
            if status == "Verified complete" {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("Team failed: {e}");
            1
        }
    }
}

fn parse_args(args: &[String]) -> Result<([&str; 3], &str), String> {
    if args.len() != 8 {
        return Err("Use: tempo-foundry team --research RECIPE_ID --code RECIPE_ID --content RECIPE_ID --task-file FILE".into());
    }
    let expected = ["--research", "--code", "--content", "--task-file"];
    let mut values = Vec::new();
    for (pair, flag) in args.chunks_exact(2).zip(expected) {
        if pair[0] != flag
            || pair[1].is_empty()
            || (flag != "--task-file" && !safe_id(&pair[1]))
            || (flag == "--task-file" && pair[1].starts_with("--"))
        {
            return Err(
                "team options must appear exactly once in the documented order with valid values"
                    .into(),
            );
        }
        values.push(pair[1].as_str());
    }
    Ok(([values[0], values[1], values[2]], values[3]))
}

fn execute(
    ctx: &Ctx,
    recipes: Vec<Recipe>,
    qualifications: Vec<task::Qualification>,
    task_path: &str,
    task_text: &str,
) -> Result<(String, &'static str, String, PathBuf), String> {
    for recipe in &recipes {
        runner::verify_current_recipe(ctx, recipe)?;
    }
    let run = format!(
        "foundry-team-{}-{}",
        super::unix_millis(),
        std::process::id()
    );
    let out_dir = ctx.agent_data.join("foundry").join("teams").join(&run);
    fs::create_dir_all(&out_dir).map_err(|e| format!("could not create team folder: {e}"))?;
    let stop_path = task::stop_path(ctx, &run);
    if stop_path.exists() {
        return Err("a stop flag already exists for this run id".into());
    }
    let log = RunLog::create(ctx, &run, unix_secs())
        .map_err(|e| format!("could not open team log: {e}"))?;

    let budget_tokens = recipes
        .iter()
        .map(|r| r.design.budget.tokens)
        .min()
        .ok_or("no recipes")?;
    let budget_seconds = recipes
        .iter()
        .map(|r| r.design.budget.seconds)
        .min()
        .ok_or("no recipes")?;
    let request_tokens = recipes
        .iter()
        .map(|r| r.design.budget.request_tokens)
        .min()
        .ok_or("no request budgets")?;
    if budget_tokens == 0 || budget_seconds == 0 || request_tokens == 0 {
        return Err("team recipes must each provide finite positive budgets".into());
    }
    let settings = Settings {
        takes: 1,
        budget: Budget {
            tokens: budget_tokens,
            wall_time: Duration::from_secs(budget_seconds),
            tool_calls: 0,
            cloud_cents: 0,
        },
        ..Settings::default()
    };

    let mut members = Vec::new();
    let mut engines_list = Vec::new();
    let mut routes = Vec::new();
    for recipe in recipes {
        let handle = recipe.role.name().to_string();
        let engine_id = canonical_engine_id(recipe.port, &recipe.model);
        let url = format!("http://127.0.0.1:{}", recipe.port);
        let role_prompt = recipe.design.runtime_system_prompt();
        members.push(Member {
            handle: handle.clone(),
            name: recipe.design.name.clone(),
            instrument: handle.clone(),
            role: role_prompt.clone(),
            model: recipe.model.clone(),
            engine: engine_id.clone(),
            may: May::Draft,
        });
        if !engines_list
            .iter()
            .any(|engine: &Engine| engine.id == engine_id)
        {
            engines_list.push(Engine {
                id: engine_id.clone(),
                url: url.clone(),
                up: true,
                ms: 0,
                models: vec![Model {
                    name: recipe.model.clone(),
                    billions: 0.0,
                }],
                why: String::new(),
            });
        }
        routes.push(Route {
            recipe,
            role_prompt: crate::conductor::with_guard(&role_prompt),
            engine: Engine {
                id: engine_id,
                url,
                up: true,
                ms: 0,
                models: Vec::new(),
                why: String::new(),
            },
        });
    }
    let handles = members.iter().map(|m| m.handle.clone()).collect::<Vec<_>>();
    let band = Band {
        name: "foundry-team".into(),
        about: "A temporary team of kept local foundry roles.".into(),
        members,
        flows: Vec::new(),
    };
    let flow = Flow {
        name: "foundry-task".into(),
        goal: "Complete the owner-supplied task as a local draft.".into(),
        steps: handles,
        every: Every::Manual,
        notes_to: "foundry".into(),
    };
    let cast: Vec<Cast> = engines::cast(&band, &engines_list);
    let chat = FoundryChat {
        agent_data: ctx.agent_data.clone(),
        routes,
        log: &log,
        provider_gate: Mutex::new(()),
        calls: Mutex::new(Vec::new()),
    };
    let checks = NoChecks;
    let takes_path = out_dir.join("isolated-takes.jsonl");
    let mut tree = Tree::new(
        &chat,
        &checks,
        &log,
        takes_path.clone(),
        &settings,
        &band,
        &flow,
        &cast,
        &engines_list,
    );
    tree.tools.clear();
    tree.note =
        "Foundry roles have no callable tools; all source checks remain outside model authority."
            .into();
    tree.material = String::new();
    println!("Team run {run} started; stop with `tempo-foundry stop {run}` (in-flight requests run until bounded timeout).");
    let started = Instant::now();
    let goal = task_text.to_string();
    let joined = std::thread::scope(|scope| {
        let handle = scope.spawn(|| tree.run(&goal));
        loop {
            for line in tree.drain_lines() {
                println!("  {line}");
            }
            if task::stop_requested(ctx, &run) {
                tree.meter.stop();
            }
            if handle.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        for line in tree.drain_lines() {
            println!("  {line}");
        }
        handle.join()
    });
    let outcome = joined.map_err(|_| {
        format!(
            "Sections run panicked; source outputs remain in {}",
            log.path.display()
        )
    })?;
    let used = tree.meter.used();
    let calls = chat
        .calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(call_json)
        .collect::<Vec<_>>();
    let refs_max = chat
        .calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter_map(|c| line_number(&c.answer_ref))
        .max()
        .unwrap_or(0);
    let log_lines =
        runlog::verify(&log.path).map_err(|e| format!("team run log did not verify: {e}"))?;
    let log_ok = log_lines >= refs_max && log_lines > 0;
    let stored_takes = takes_file::read_all(&takes_path)
        .into_iter()
        .filter(|t| t.run == run)
        .count();
    let takes_ok = stored_takes == outcome.takes;
    let stopped = task::stop_requested(ctx, &run) || outcome.halt.is_some();
    let all_sections_ok = !outcome.reports.is_empty() && outcome.reports.iter().all(|r| r.ok);
    let status = if !log_ok || !takes_ok {
        "Untested"
    } else if !stopped && outcome.halt.is_none() && outcome.final_text.is_some() && all_sections_ok
    {
        "Verified complete"
    } else {
        "In progress"
    };
    let (actual_provider_tokens, estimated_provider_tokens) = {
        let call_rows = chat.calls.lock().unwrap_or_else(|p| p.into_inner());
        let actual = call_rows
            .iter()
            .filter(|c| c.usage_source == "actual")
            .fold(0u64, |sum, c| {
                sum.saturating_add(c.prompt_tokens.saturating_add(c.completion_tokens))
            });
        let estimated = call_rows
            .iter()
            .filter(|c| c.usage_source != "actual")
            .fold(0u64, |sum, c| {
                sum.saturating_add(c.prompt_tokens.saturating_add(c.completion_tokens))
            });
        (actual, estimated)
    };
    let qualification_tokens: u64 = qualifications.iter().map(|q| q.total_tokens).sum();
    let qualification_ms: u64 = qualifications.iter().map(|q| q.total_ms).sum();
    let role_receipts = recipes_summary(&chat, &qualifications);
    let report = J::Obj(vec![
        ("v".into(), J::n(1.0)), ("kind".into(), J::s("foundry_team")), ("run".into(), J::s(&run)),
        ("status".into(), J::s(status)), ("quality_status".into(), J::s("Untested")),
        ("task_file".into(), J::s(task_path)), ("task_sha256".into(), J::s(&sha256_hex(task_text.as_bytes()))), ("goal".into(), J::s(task_text)),
        ("engine".into(), J::s(ENGINE)), ("roles".into(), J::Arr(role_receipts)),
        ("provider_calls".into(), J::Arr(calls)),
        ("tree_meter_tokens_conservative".into(), J::Num(used.tokens.to_string())),
        ("tree_meter_contract".into(), J::s("Existing Sections adds its prompt/role/answer chars-over-4 estimate after each success. This adapter charges only actual provider usage above that estimate before returning, so the shared root counts max(actual, estimate) without double charging; failed calls charge the larger observed actual count or conservative text estimate.")),
        ("provider_concurrency".into(), J::Num("1".into())),
        ("team_call_budget_note".into(), J::s("Provider calls are serialized. Each call receives at most its role request cap and current shared Tree headroom less the settings chars-over-4 estimates for role and prompt plus 64 tokens of fixed overhead. The existing Sections tree remains the only scheduler.")),
        ("producer_role_policy".into(), J::s("The exact host-owned Conductor Sections producer role may judge on an approved local endpoint. It is recorded as role=producer with its route recipe role and host-owned role source; it is not designed or qualified as a recipe.")),
        ("route_identity_policy".into(), J::s("Conductor engine IDs are derived from loopback port and requested model, and duplicate IDs share one engine entry so identical endpoints trigger Sections same-engine producer fallback.")),
        ("provider_actual_prompt_completion_tokens".into(), J::Num(actual_provider_tokens.to_string())),
        ("provider_estimated_prompt_completion_tokens".into(), J::Num(estimated_provider_tokens.to_string())),
        ("provider_usage_note".into(), J::s("Provider counts are split by source and preserved per call. Actual usage above the Sections chars/4 estimate is included in the same root meter; provider actual fields are not added a second time.")),
        ("qualification_total_tokens".into(), J::Num(qualification_tokens.to_string())),
        ("qualification_total_usage_note".into(), J::s("Qualification receipts retain their own per-attempt actual or estimated usage labels.")),
        ("qualification_elapsed_ms".into(), J::Num(qualification_ms.to_string())),
        ("team_elapsed_ms".into(), J::Num(started.elapsed().as_millis().min(u128::from(u64::MAX)).to_string())),
        ("total_foundry_elapsed_ms".into(), J::Num(qualification_ms.saturating_add(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64).to_string())),
        ("root_budget_tokens".into(), J::Num(budget_tokens.to_string())), ("root_budget_seconds".into(), J::Num(budget_seconds.to_string())),
        ("stop_requested".into(), J::Bool(stopped)), ("in_flight_cancel_limit".into(), J::s("Stop is polled between calls; an in-flight loopback request ends at its bounded timeout.")),
        ("final_by".into(), J::s(&outcome.final_by)), ("final_text".into(), outcome.final_text.as_deref().map_or(J::Null, J::s)),
        ("sections_halt".into(), outcome.halt.as_ref().map_or(J::Null, |(_, line)| J::s(line))),
        ("takes_path".into(), J::s(&takes_path.to_string_lossy())), ("takes".into(), J::Num((stored_takes as u64).to_string())),
        ("training_eligible".into(), J::Bool(false)),
        ("log_path".into(), J::s(&log.path.to_string_lossy())), ("log_verified_lines".into(), J::Num((log_lines as u64).to_string())),
    ]);
    let report_path = out_dir.join("team.json");
    write_new(&report_path, &report.compact())
        .map_err(|e| format!("could not preserve team report: {e}"))?;
    let receipt_path = ctx
        .agent_data
        .join("foundry")
        .join("receipts")
        .join(format!("{run}.json"));
    write_new(&receipt_path, &report.compact())
        .map_err(|e| format!("could not preserve team receipt: {e}"))?;
    let reason = if !log_ok {
        "team run log verification failed".to_string()
    } else if !takes_ok {
        "isolated team take count does not match Sections outcome".to_string()
    } else if stopped {
        "stop or finite budget ended the Sections run".to_string()
    } else {
        "existing Conductor Sections completed; output quality remains untested".to_string()
    };
    Ok((run, status, reason, receipt_path))
}

impl Chat for FoundryChat<'_> {
    fn say(&self, _model: &str, _role: &str, _text: &str) -> Result<String, String> {
        Err("foundry team calls must carry the shared Sections meter".into())
    }

    fn say_run(
        &self,
        meter: &Meter,
        engine: &str,
        model: &str,
        role: &str,
        text: &str,
        schema: &str,
    ) -> Result<Said, String> {
        let _provider_guard = match self.provider_gate.lock() {
            Ok(guard) => guard,
            Err(_) => {
                meter.stop();
                return Err(
                    "foundry provider gate was poisoned; stopping the shared team meter".into(),
                );
            }
        };
        if meter.halt().is_some() {
            return Err("Sections root meter is stopped".into());
        }
        let route = self
            .routes
            .iter()
            .find(|r| {
                route_matches(
                    &r.engine.url,
                    &r.recipe.model,
                    &r.role_prompt,
                    engine,
                    model,
                    role,
                )
            })
            .ok_or_else(|| {
                "Sections requested a model/role/loopback route outside the kept team roles and fixed producer role"
                    .to_string()
            })?;
        if let Err(e) = runner::verify_recipe_in_dir(&self.agent_data, &route.recipe) {
            meter.stop();
            return Err(e);
        }
        let is_producer = role == crate::conductor::sections::PRODUCER_ROLE;
        let runtime_role = if is_producer {
            "producer"
        } else {
            route.recipe.role.name()
        };
        let role_source = if is_producer {
            "host-owned Conductor Sections producer prompt"
        } else {
            "kept recipe prompt"
        };
        let prompt = if schema.is_empty() {
            text.to_string()
        } else {
            format!("Return output matching this schema:\n{schema}\n\n{text}")
        };
        let remaining = meter.budget().tokens.saturating_sub(meter.used().tokens);
        let input_reserve = settings::tokens_of(role)
            .saturating_add(settings::tokens_of(&prompt))
            .saturating_add(64);
        let max_tokens = route
            .recipe
            .design
            .budget
            .request_tokens
            .min(remaining.saturating_sub(input_reserve));
        if max_tokens == 0 {
            meter.stop();
            return Err("shared Sections token budget cannot cover this role prompt and a positive output cap".into());
        }
        let started = Instant::now();
        let plan = self.log.emit(
            unix_secs(),
            Ev {
                parent: None,
                depth: 0,
                member: runtime_role,
                engine: ENGINE,
                model,
                kind: "foundry_provider_plan",
                fields: vec![
                    ("recipe", J::s(&route.recipe.id)),
                    ("role", J::s(runtime_role)),
                    ("recipe_role", J::s(route.recipe.role.name())),
                    ("role_source", J::s(role_source)),
                    ("schema", J::s(schema)),
                    ("prompt", J::s(&prompt)),
                ],
            },
        );
        let reply = infer_for_sections(
            route.recipe.port,
            model,
            role,
            &prompt,
            max_tokens,
            !schema.is_empty(),
            meter,
        );
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let mut error = reply.error.clone();
        if error.is_none() && reply.finish_reason != "stop" {
            error = Some(
                "Sections provider response was truncated or had an invalid finish reason".into(),
            );
        }
        let tree_estimate = tree_tokens(role, text, &reply.content);
        let actual_tokens = if reply.usage.source == "actual" {
            reply.usage.prompt.saturating_add(reply.usage.completion)
        } else {
            0
        };
        if error.is_none() && actual_tokens > 0 {
            if let Err(e) = charge_actual_over_tree_estimate(meter, actual_tokens, tree_estimate) {
                error = Some(e);
            }
        }
        let result = self.log.emit(
            unix_secs(),
            Ev {
                parent: None,
                depth: 0,
                member: runtime_role,
                engine: ENGINE,
                model,
                kind: "foundry_provider_result",
                fields: vec![
                    ("recipe", J::s(&route.recipe.id)),
                    ("role", J::s(runtime_role)),
                    ("recipe_role", J::s(route.recipe.role.name())),
                    ("role_source", J::s(role_source)),
                    (
                        "status",
                        J::s(if error.is_some() { "error" } else { "complete" }),
                    ),
                    ("answer", J::s(&reply.content)),
                    ("provider_body", J::s(&reply.body)),
                    ("error", error.as_deref().map_or(J::Null, J::s)),
                    ("prompt_tokens", J::Num(reply.usage.prompt.to_string())),
                    ("max_output_tokens", J::Num(max_tokens.to_string())),
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
        let record = CallRecord {
            role: runtime_role.into(),
            recipe_role: route.recipe.role.name().into(),
            role_source: role_source.into(),
            recipe_id: route.recipe.id.clone(),
            engine_id: route.engine.id.clone(),
            url: route.engine.url.clone(),
            requested_model: model.into(),
            response_model: reply.response_model.clone(),
            prompt_tokens: reply.usage.prompt,
            completion_tokens: reply.usage.completion,
            max_output_tokens: max_tokens,
            usage_source: reply.usage.source,
            elapsed_ms,
            answer: reply.content.clone(),
            provider_body: reply.body.clone(),
            error: error.clone(),
            prompt_ref: self.log.reference(plan),
            answer_ref: self.log.reference(result),
        };
        self.calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(record);
        if let Some(error) = error {
            charge_failed_provider_call(meter, actual_tokens, role, &prompt, &reply.content);
            meter.stop();
            return Err(error);
        }
        Ok(Said {
            text: reply.content,
            engine: Some(route.engine.url.clone()),
        })
    }
}

fn tree_tokens(role: &str, ask: &str, answer: &str) -> u64 {
    settings::tokens_of(ask)
        .saturating_add(settings::tokens_of(role))
        .saturating_add(settings::tokens_of(&crate::redact(answer.trim())))
}

fn canonical_engine_id(port: u16, model: &str) -> String {
    let identity = format!("127.0.0.1:{port}\n{model}");
    format!("foundry-local-{}", &sha256_hex(identity.as_bytes())[..16])
}

fn route_matches(
    route_engine: &str,
    route_model: &str,
    route_role_prompt: &str,
    engine: &str,
    model: &str,
    role: &str,
) -> bool {
    route_engine == engine
        && route_model == model
        && (route_role_prompt == role || role == crate::conductor::sections::PRODUCER_ROLE)
}

fn charge_actual_over_tree_estimate(
    meter: &Meter,
    actual_tokens: u64,
    tree_estimate: u64,
) -> Result<(), String> {
    let remaining = meter.budget().tokens.saturating_sub(meter.used().tokens);
    if actual_tokens > remaining {
        meter.charge_tokens(remaining);
        meter.stop();
        return Err("actual provider usage exceeded the shared Sections token budget".into());
    }
    // Tree will add its existing chars/4 estimate after say_run returns. This difference makes
    // the shared meter count max(actual provider usage, that estimate), without double charging.
    meter.charge_tokens(actual_tokens.saturating_sub(tree_estimate));
    Ok(())
}

fn charge_failed_provider_call(
    meter: &Meter,
    actual_tokens: u64,
    role: &str,
    prompt: &str,
    answer: &str,
) {
    let estimate = settings::tokens_of(role)
        .saturating_add(settings::tokens_of(prompt))
        .saturating_add(settings::tokens_of(&crate::redact(answer.trim())));
    let remaining = meter.budget().tokens.saturating_sub(meter.used().tokens);
    meter.charge_tokens(actual_tokens.max(estimate).min(remaining));
}

fn recipes_summary(chat: &FoundryChat<'_>, qualifications: &[task::Qualification]) -> Vec<J> {
    let calls = chat.calls.lock().unwrap_or_else(|p| p.into_inner());
    chat.routes
        .iter()
        .zip(qualifications)
        .map(|(r, q)| {
            J::Obj(vec![
                ("role".into(), J::s(r.recipe.role.name())),
                ("recipe_id".into(), J::s(&r.recipe.id)),
                ("recipe_sha256".into(), J::s(&r.recipe.recipe_sha256)),
                ("agent_name".into(), J::s(&r.recipe.design.name)),
                ("engine_id".into(), J::s(&r.engine.id)),
                ("engine_url".into(), J::s(&r.engine.url)),
                ("requested_model".into(), J::s(&r.recipe.model)),
                ("qualification_run".into(), J::s(&q.run)),
                ("qualification_receipt".into(), J::s(&q.receipt)),
                (
                    "calls".into(),
                    J::Num(
                        calls
                            .iter()
                            .filter(|c| c.recipe_id == r.recipe.id)
                            .count()
                            .to_string(),
                    ),
                ),
            ])
        })
        .collect()
}

fn call_json(c: &CallRecord) -> J {
    J::Obj(vec![
        ("role".into(), J::s(&c.role)),
        ("recipe_role".into(), J::s(&c.recipe_role)),
        ("role_source".into(), J::s(&c.role_source)),
        ("recipe_id".into(), J::s(&c.recipe_id)),
        ("engine_id".into(), J::s(&c.engine_id)),
        ("url".into(), J::s(&c.url)),
        ("requested_model".into(), J::s(&c.requested_model)),
        (
            "response_model".into(),
            c.response_model.as_deref().map_or(J::Null, J::s),
        ),
        ("prompt_tokens".into(), J::Num(c.prompt_tokens.to_string())),
        (
            "completion_tokens".into(),
            J::Num(c.completion_tokens.to_string()),
        ),
        (
            "max_output_tokens".into(),
            J::Num(c.max_output_tokens.to_string()),
        ),
        ("usage_source".into(), J::s(c.usage_source)),
        ("elapsed_ms".into(), J::Num(c.elapsed_ms.to_string())),
        ("answer".into(), J::s(&c.answer)),
        ("provider_body".into(), J::s(&c.provider_body)),
        ("error".into(), c.error.as_deref().map_or(J::Null, J::s)),
        ("prompt_ref".into(), J::s(&c.prompt_ref)),
        ("answer_ref".into(), J::s(&c.answer_ref)),
    ])
}

fn safe_id(s: &str) -> bool {
    (2..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}
fn line_number(reference: &str) -> Option<usize> {
    reference.rsplit_once('#')?.1.parse().ok()
}

#[cfg(test)]
mod meter_accounting_tests {
    use super::{canonical_engine_id, charge_actual_over_tree_estimate, route_matches};
    use crate::conductor::budget::{Budget, Meter};
    use std::time::Duration;

    #[test]
    fn two_actual_heavy_replies_stop_the_shared_tree_meter() {
        let root = Meter::root(Budget {
            tokens: 100,
            wall_time: Duration::from_secs(30),
            tool_calls: 0,
            cloud_cents: 0,
        });

        charge_actual_over_tree_estimate(&root, 40, 10).unwrap();
        root.charge_tokens(10); // Existing Sections Tree estimate after the first reply.
        assert_eq!(root.used().tokens, 40);

        let second = charge_actual_over_tree_estimate(&root, 70, 10);
        assert!(second.is_err());
        assert_eq!(root.used().tokens, 100);
        assert!(root.halt().is_some());
    }

    #[test]
    fn producer_is_the_only_extra_route_role_and_engine_ids_bind_endpoint_and_model() {
        let endpoint = "http://127.0.0.1:8101";
        let producer = crate::conductor::sections::PRODUCER_ROLE;
        assert!(route_matches(
            endpoint,
            "qwen-local",
            "kept role",
            endpoint,
            "qwen-local",
            "kept role"
        ));
        assert!(route_matches(
            endpoint,
            "qwen-local",
            "kept role",
            endpoint,
            "qwen-local",
            producer
        ));
        assert!(!route_matches(
            endpoint,
            "qwen-local",
            "kept role",
            endpoint,
            "qwen-local",
            "model supplied role"
        ));
        assert!(!route_matches(
            endpoint,
            "qwen-local",
            "kept role",
            "http://127.0.0.1:8102",
            "qwen-local",
            producer
        ));
        assert_eq!(
            canonical_engine_id(8101, "qwen-local"),
            canonical_engine_id(8101, "qwen-local")
        );
        assert_ne!(
            canonical_engine_id(8101, "qwen-local"),
            canonical_engine_id(8102, "qwen-local")
        );
        assert_ne!(
            canonical_engine_id(8101, "qwen-local"),
            canonical_engine_id(8101, "other-model")
        );
    }
}
