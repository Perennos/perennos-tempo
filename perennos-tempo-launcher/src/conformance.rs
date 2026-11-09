use crate::setup::json::{self, J};
use crate::Ctx;
use std::path::{Path, PathBuf};

const INPUTS: &[&str] = &[
    "use-case.json",
    "data-flow.json",
    "models.json",
    "signoffs.json",
    "incidents-overrides.json",
    "evaluation-canary.json",
    "signed-receipts.json",
];
const MAX_BYTES: u64 = 1 << 20;
const MAX_ROWS: usize = 1000;

#[derive(Debug, PartialEq)]
pub struct Opts {
    pub case_id: String,
    pub out: PathBuf,
    pub edition: String,
}

pub fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut case_id = None;
    let mut out = None;
    let mut edition = "cayman".to_string();
    let mut edition_set = false;
    let mut i = 0;
    while i < args.len() {
        let value = args
            .get(i + 1)
            .ok_or("conformance needs a value after each option")?;
        match args[i].as_str() {
            "--case" if case_id.is_none() => case_id = Some(value.clone()),
            "--out" if out.is_none() => out = Some(PathBuf::from(value)),
            "--edition" if !edition_set => {
                edition = value.clone();
                edition_set = true;
            }
            _ => {
                return Err(
                    "conformance takes --case ID --out NEW_DIRECTORY [--edition cayman|general]"
                        .into(),
                )
            }
        }
        i += 2;
    }
    let case_id = case_id.ok_or("conformance needs --case ID")?;
    if !valid_id(&case_id) {
        return Err(
            "case id must use up to 64 ASCII letters, numbers, underscores or hyphens".into(),
        );
    }
    let out = out.ok_or("conformance needs --out NEW_DIRECTORY")?;
    if !matches!(edition.as_str(), "cayman" | "general") {
        return Err("edition must be cayman or general".into());
    }
    Ok(Opts {
        case_id,
        out,
        edition,
    })
}

pub fn run(ctx: &Ctx, opts: &Opts) -> i32 {
    match export(ctx, opts) {
        Ok((code, summary)) => {
            println!("{summary}");
            code
        }
        Err(message) => {
            eprintln!("Perennos Tempo could not export the report: {message}");
            1
        }
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn object(fields: Vec<(&str, J)>) -> J {
    J::Obj(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn unique_keys(value: &J) -> Result<(), String> {
    match value {
        J::Obj(fields) => {
            let mut seen = std::collections::HashSet::new();
            for (key, child) in fields {
                if !seen.insert(key) {
                    return Err("duplicate JSON object key".into());
                }
                unique_keys(child)?;
            }
        }
        J::Arr(items) => {
            if items.len() > MAX_ROWS {
                return Err("array exceeds the 1000 entry limit".into());
            }
            for child in items {
                unique_keys(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err("input is not a regular file".into());
    }
    if meta.len() > MAX_BYTES {
        return Err("input exceeds the 1 MiB limit".into());
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut limited = file.take(MAX_BYTES + 1);
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    limited.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("input exceeds the 1 MiB limit".into());
    }
    Ok(bytes)
}

#[derive(Clone)]
struct Section {
    name: &'static str,
    status: &'static str,
    source: String,
    hash: Option<String>,
    count: usize,
    rows: Vec<J>,
    reason: Option<String>,
    missing: Vec<String>,
}

const USE_CASE: &[&str] = &[
    "case_id",
    "tier",
    "purpose",
    "owner_role",
    "deputy_role",
    "approval_reference",
    "risk_assessment_reference",
    "review_due",
    "lawful_basis_reference",
    "contract_reference",
    "retention_reference",
    "stop_test_reference",
    "notification_plan_reference",
    "tier_reason_reversibility",
    "tier_reason_challenge",
    "owner_reference",
    "deputy_reference",
    "approval_date",
    "authority_reference",
    "change_reference",
    "data_subjects_reference",
    "confidentiality_reference",
    "prohibited_data_reference",
    "supplier_terms_reference",
    "outsourcing_reference",
    "fairness_reference",
    "cyber_register_reference",
    "output_storage_reference",
    "run_volume_reference",
    "review_standard_reference",
    "human_decisions_reference",
    "integrity_review_reference",
    "override_review_reference",
    "reversal_reference",
    "explanation_reference",
    "appeal_reference",
    "rehearsal_reference",
];
const DATA_FLOW: &[&str] = &[
    "stage",
    "country",
    "operator_role",
    "data_categories",
    "retention",
    "access_reference",
    "transfer_basis_reference",
];
const MODELS: &[&str] = &[
    "model_id",
    "version",
    "digest",
    "runtime",
    "runtime_version",
    "observed_at",
    "change_reference",
];
const SIGNOFFS: &[&str] = &[
    "decision_reference",
    "reviewer_role",
    "decision",
    "at",
    "approval_reference",
];
const INCIDENTS: &[&str] = &[
    "event_id",
    "kind",
    "discovered_at",
    "reasonable_awareness_at",
    "action_reference",
    "owner_role",
    "reason_reference",
    "notification_reference",
];
const EVALUATIONS: &[&str] = &[
    "evaluation_id",
    "model_digest",
    "dataset_digest",
    "baseline_reference",
    "candidate_reference",
    "canary_reference",
    "rollback_reference",
    "reviewer_role",
    "result",
    "at",
];

fn looks_secret_shaped(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "xoxs-",
        "akia",
        "aiza",
        "ya29.",
        "secret",
        "password",
        "bearer",
        "token",
        "api_key",
        "private_key",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn safe_text(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 256 || !value.is_ascii() {
        return Err("field is empty, too long, or not ASCII".into());
    }
    if looks_secret_shaped(value)
        || value.contains('/')
        || value.contains('\\')
        || value.contains('@')
        || value.contains("..")
        || value.contains("<")
        || value.contains(">")
        || value.chars().any(|c| c.is_control())
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " _-:,()".contains(c))
    {
        return Err("field contains a value this report cannot export".into());
    }
    Ok(value.to_string())
}

fn safe_model_text(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 256 || !value.is_ascii() {
        return Err("model metadata is empty, too long, or not ASCII".into());
    }
    let lower = value.to_ascii_lowercase();
    if looks_secret_shaped(value)
        || [".com", ".net", ".org", ".local", ".internal", "localhost"]
            .iter()
            .any(|needle| lower.contains(needle))
        || value.contains('/')
        || value.contains('\\')
        || value.contains('@')
        || value.contains("..")
        || value.chars().any(|c| c.is_control())
        || (value.contains('.')
            && !value.contains(':')
            && !value.chars().all(|c| c.is_ascii_digit() || c == '.'))
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._-:,()".contains(c))
    {
        return Err("model metadata contains a value this report cannot export".into());
    }
    Ok(value.to_string())
}
fn normalized_row(
    value: &J,
    allowed: &[&str],
    case_id: Option<&str>,
) -> Result<(J, Vec<String>), String> {
    let J::Obj(fields) = value else {
        return Err("record must be an object".into());
    };
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for key in allowed {
        let Some((_, value)) = fields.iter().find(|(name, _)| name == key) else {
            missing.push((*key).to_string());
            continue;
        };
        let safe = if *key == "tier" {
            match value {
                J::Num(n) if matches!(n.as_str(), "0" | "1" | "2" | "3") => value.clone(),
                _ => return Err("tier must be a whole number from 0 to 3".into()),
            }
        } else if *key == "data_categories" {
            match value {
                J::Str(s) => J::s(&safe_text(s)?),
                J::Arr(items) if items.len() <= 32 => J::Arr(
                    items
                        .iter()
                        .map(|v| match v {
                            J::Str(s) => safe_text(s).map(|x| J::s(&x)),
                            _ => Err("data categories must be short text".into()),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                _ => return Err("data categories must be text or a short list".into()),
            }
        } else if *key == "digest" || *key == "model_digest" || *key == "dataset_digest" {
            match value {
                J::Str(s) if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
                    J::s(&s.to_ascii_lowercase())
                }
                _ => return Err("digest must be a 64 character SHA-256 value".into()),
            }
        } else {
            match value {
                J::Str(s) if ["model_id", "version", "runtime_version"].contains(key) => {
                    J::s(&safe_model_text(s)?)
                }
                J::Str(s) => J::s(&safe_text(s)?),
                _ => return Err("allowlisted fields must be text".into()),
            }
        };
        if *key == "case_id"
            && case_id.is_some_and(|want| !matches!(&safe, J::Str(value) if value == want))
        {
            return Err("case id does not match the requested case".into());
        }
        out.push(((*key).to_string(), safe));
    }
    Ok((J::Obj(out), missing))
}

fn json_is_string(value: &J) -> Option<&str> {
    if let J::Str(s) = value {
        Some(s)
    } else {
        None
    }
}

fn inspect_path(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "source reference is outside its fixed folder")?;
    let mut current = PathBuf::new();
    for part in root.components().chain(relative.components()) {
        current.push(part.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => return Err("source contains a symbolic link".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn read_input(root: &Path, path: &Path) -> Result<Option<Vec<u8>>, String> {
    inspect_path(root, path)?;
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
        Ok(meta) if is_link(&meta) || !meta.is_file() => Err("input is not a regular file".into()),
        Ok(_) => read_bounded(path).map(Some),
    }
}

fn section_input(
    root: &Path,
    case_id: &str,
    filename: &'static str,
    name: &'static str,
    fields: &[&str],
    use_case: bool,
) -> Result<Section, String> {
    let path = root.join(filename);
    let shown = format!("conformance/{case_id}/{filename}");
    let bytes = match read_input(root, &path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Ok(Section {
                name,
                status: "Untested",
                source: shown,
                hash: None,
                count: 0,
                rows: Vec::new(),
                reason: Some("No source file was found.".into()),
                missing: vec![filename.into()],
            })
        }
        Err(reason) => {
            return Ok(Section {
                name,
                status: "In progress",
                source: shown,
                hash: None,
                count: 0,
                rows: Vec::new(),
                reason: Some(reason),
                missing: Vec::new(),
            })
        }
    };
    let hash = crate::setup::sha256::sha256_hex(&bytes);
    let parsed = std::str::from_utf8(&bytes)
        .map_err(|_| "source is not UTF-8".to_string())
        .and_then(json::parse)
        .and_then(|v| {
            unique_keys(&v)?;
            Ok(v)
        });
    let source = shown;
    let mut section = Section {
        name,
        status: "In progress",
        source,
        hash: Some(hash),
        count: 0,
        rows: Vec::new(),
        reason: None,
        missing: Vec::new(),
    };
    let value = match parsed {
        Ok(v) => v,
        Err(_) => {
            section.reason = Some("Source JSON is malformed or has duplicate keys.".into());
            return Ok(section);
        }
    };
    let J::Obj(_) = value else {
        section.reason = Some("Source must be a JSON object.".into());
        return Ok(section);
    };
    if !matches!(value.get("schema_version"), Some(J::Num(n)) if n == "1") {
        section.reason = Some("schema_version must be 1.".into());
        return Ok(section);
    }
    if use_case {
        match normalized_row(&value, fields, Some(case_id)) {
            Ok((row, missing)) => {
                section.count = 1;
                section.rows.push(row);
                section.missing.extend(missing);
                if section.missing.is_empty() {
                    section.reason = Some("Human assertions need independent review.".into());
                }
            }
            Err(e) => section.reason = Some(e),
        }
        return Ok(section);
    }
    let Some(J::Arr(rows)) = value.get("records") else {
        section.reason = Some("records must be an array.".into());
        return Ok(section);
    };
    if rows.len() > MAX_ROWS {
        section.reason = Some("records exceeds the 1000 entry limit.".into());
        return Ok(section);
    }
    for row in rows {
        match normalized_row(row, fields, None) {
            Ok((record, missing)) => {
                section.rows.push(record);
                section.missing.extend(missing);
            }
            Err(e) => {
                section.rows.clear();
                section.missing.clear();
                section.reason = Some(e);
                return Ok(section);
            }
        }
    }
    section.count = section.rows.len();
    if section.count == 0 {
        section.reason = Some("No human records were supplied.".into());
    } else if !section.missing.is_empty() {
        section.reason = Some("Some record fields are missing.".into());
    } else {
        section.reason = Some("Human assertions need independent review.".into());
    }
    Ok(section)
}

type ModelConfigSnapshot = (String, Option<String>, Vec<J>, Vec<String>);
type ModelRenderDetails<'a> = (Option<&'a str>, &'a [J], &'a [J]);

fn config_models(ctx: &Ctx, external_selected: bool) -> Result<ModelConfigSnapshot, String> {
    let selected = if external_selected {
        None
    } else {
        ctx.agent_config_file().or_else(|| {
            let legacy = ctx.agent_data.join("agent-config.json");
            legacy.is_file().then_some(legacy)
        })
    };
    let Some(path) = selected else {
        return Ok((
            "Untested".into(),
            None,
            Vec::new(),
            vec!["agent/config.json or an in-folder legacy config".into()],
        ));
    };
    let root = &ctx.agent_data;
    let Some(bytes) = read_input(root, &path)? else {
        return Ok((
            "Untested".into(),
            None,
            Vec::new(),
            vec!["agent/config.json".into()],
        ));
    };
    let hash = crate::setup::sha256::sha256_hex(&bytes);
    let text = std::str::from_utf8(&bytes).map_err(|_| "agent config is not UTF-8".to_string())?;
    let value = json::parse(text)?;
    unique_keys(&value)?;
    let J::Obj(_) = value else {
        return Err("agent config is not an object".into());
    };
    let mut rows = Vec::new();
    for key in ["models", "fast_models"] {
        if let Some(v) = value.get(key) {
            let J::Arr(items) = v else {
                return Err("model setting must be a list".into());
            };
            if items.len() > MAX_ROWS {
                return Err("model setting exceeds the 1000 entry limit".into());
            }
            for item in items {
                let id = safe_model_text(json_is_string(item).ok_or("model id must be text")?)?;
                rows.push(object(vec![
                    ("model_id", J::s(&id)),
                    ("version", J::Null),
                    ("digest", J::Null),
                    ("runtime", J::s("Ollama configured")),
                    ("runtime_version", J::Null),
                    ("observed_at", J::Null),
                    ("change_reference", J::s("agent config")),
                ]));
            }
        }
    }
    if let Some(value) = value.get("embed_model") {
        let id = safe_model_text(json_is_string(value).ok_or("embedding model id must be text")?)?;
        rows.push(object(vec![
            ("model_id", J::s(&id)),
            ("version", J::Null),
            ("digest", J::Null),
            ("runtime", J::s("Ollama configured")),
            ("runtime_version", J::Null),
            ("observed_at", J::Null),
            ("change_reference", J::s("agent config")),
        ]));
    }
    Ok(("In progress".into(), Some(hash), rows, Vec::new()))
}

fn section_json(section: &Section) -> J {
    object(vec![
        ("name", J::s(section.name)),
        ("status", J::s(section.status)),
        ("source", J::s(&section.source)),
        ("sha256", section.hash.as_deref().map_or(J::Null, J::s)),
        ("record_count", J::n(section.count as f64)),
        ("records", J::Arr(section.rows.clone())),
        ("reason", section.reason.as_deref().map_or(J::Null, J::s)),
        ("missing_fields", J::strings(&section.missing)),
    ])
}

fn export(ctx: &Ctx, opts: &Opts) -> Result<(i32, String), String> {
    if !valid_id(&opts.case_id) {
        return Err("case id is invalid".into());
    }
    inspect_path(&ctx.home, &ctx.home)?;
    let home = std::fs::canonicalize(&ctx.home).map_err(|e| format!("home is unavailable: {e}"))?;
    let root = home.join("conformance").join(&opts.case_id);
    let names = [
        "use case",
        "data flow",
        "model register",
        "signoffs",
        "incidents and overrides",
        "evaluation and canary",
        "signed receipts",
    ];
    let fields: [&[&str]; 7] = [
        USE_CASE,
        DATA_FLOW,
        MODELS,
        SIGNOFFS,
        INCIDENTS,
        EVALUATIONS,
        &["receipt_id", "artifact_digest"],
    ];
    let mut sections = Vec::new();
    for (i, file) in INPUTS.iter().enumerate() {
        let mut section = section_input(&root, &opts.case_id, file, names[i], fields[i], i == 0)?;
        if i == 6 {
            section.status = "Untested";
            section.reason = Some("Upstream signed verification is not available; source hash does not prove signer or truth.".into());
        }
        sections.push(section);
    }
    let (model_status, model_hash, model_rows, model_missing) =
        match config_models(ctx, std::env::var_os("TEMPO_AGENT_CONFIG").is_some()) {
            Ok(v) => v,
            Err(_) => (
                "In progress".into(),
                None,
                Vec::new(),
                vec!["agent/config.json could not be safely read".into()],
            ),
        };
    let adapter = object(vec![
        ("name", J::s("Perennos Tempo agent model register")),
        ("status", J::s(&model_status)),
        ("source", J::s("agent configuration")),
        ("sha256", model_hash.as_deref().map_or(J::Null, J::s)),
        ("record_count", J::n(model_rows.len() as f64)),
        ("records", J::Arr(model_rows.clone())),
        ("missing_evidence", J::strings(&model_missing)),
    ]);
    let created_at = crate::unix_now().to_string();
    let section_values = sections.iter().map(section_json).collect::<Vec<_>>();
    let mut missing = Vec::new();
    for section in &sections {
        if section.status != "Verified complete" {
            missing.push(object(vec![
                ("artifact", J::s(section.name)),
                ("path", J::s(&section.source)),
                ("status", J::s(section.status)),
                ("fields", J::strings(&section.missing)),
                ("reason", section.reason.as_deref().map_or(J::Null, J::s)),
            ]));
        }
    }
    if !model_missing.is_empty() {
        missing.push(object(vec![
            ("artifact", J::s("agent model register")),
            ("path", J::s("agent/config.json")),
            ("status", J::s("Untested")),
            ("fields", J::strings(&model_missing)),
        ]));
    }
    let evidence = object(vec![
        ("schema_version", J::n(1.0)), ("product", J::s("Perennos Tempo")), ("product_version", J::s(crate::VERSION)),
        ("case_id", J::s(&opts.case_id)), ("edition", J::s(if opts.edition == "cayman" { "Caymanian Integrated Systems" } else { "General" })),
        ("created_at_unix", J::s(&created_at)), ("status", J::s("In progress")),
        ("sections", J::Arr(section_values.clone())), ("model_register", adapter),
        ("signature_verification", object(vec![("status", J::s("Untested")), ("reason", J::s("The upstream signed verifier is not available."))])),
        (
            "boundaries",
            J::Arr(vec![
                J::s("This report organizes supplied records; it does not certify legal compliance."),
                J::s("Human supplied assertions remain In progress pending independent validation."),
                J::s("This export selects allowlisted metadata fields. Review it for sensitive content; human validation remains open."),
                J::s("No legal deadline or notification advice is calculated."),
                J::s("Identity, role, and Cayman residence have not been validated."),
            ]),
        ),
        ("missing_evidence", J::Arr(missing.clone())),
        ("next_actions", J::Arr(vec![J::s("Complete missing fixed files and fields."), J::s("Have each human assertion checked independently."), J::s("Use the upstream signed verifier when its export interface is available.")])),
    ]);
    let report = render_markdown(
        &opts.case_id,
        &opts.edition,
        &created_at,
        crate::VERSION,
        &sections,
        &model_status,
        (model_hash.as_deref(), &model_rows, &missing),
    );
    let out = output_path(&opts.out)?;
    create_private_dir(&out)?;
    let report_bytes = report.as_bytes();
    let evidence_bytes = evidence.pretty().into_bytes();
    write_new(&out.join("report.md"), report_bytes)?;
    write_new(&out.join("evidence.json"), &evidence_bytes)?;
    let manifest = object(vec![
        ("complete", J::Bool(true)),
        (
            "report_sha256",
            J::s(&crate::setup::sha256::sha256_hex(report_bytes)),
        ),
        (
            "evidence_sha256",
            J::s(&crate::setup::sha256::sha256_hex(&evidence_bytes)),
        ),
    ])
    .pretty();
    write_new(&out.join("manifest.json"), manifest.as_bytes())?;
    Ok((2, format!("Perennos Tempo wrote the report to {}. Evidence remains In progress; signed verification is Untested.", out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "new folder".into()))))
}

fn output_path(path: &Path) -> Result<PathBuf, String> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("output path cannot contain parent references".into());
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut current = PathBuf::new();
    let parts = absolute.components().collect::<Vec<_>>();
    for (i, part) in parts.iter().enumerate() {
        current.push(part.as_os_str());
        if i + 1 < parts.len() {
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if is_link(&meta) => {
                    return Err("output path crosses a symbolic link".into())
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err("output parent folder does not exist".into())
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    match std::fs::symlink_metadata(&absolute) {
        Ok(_) => Err("output folder already exists; choose a new folder".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(absolute),
        Err(e) => Err(e.to_string()),
    }
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("could not create output folder: {e}"))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("could not write output file: {e}"))?;
    use std::io::Write;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("could not finish output file: {e}"))
}

fn render_markdown(
    case_id: &str,
    edition: &str,
    created: &str,
    version: &str,
    sections: &[Section],
    model_status: &str,
    model_details: ModelRenderDetails<'_>,
) -> String {
    let (model_hash, model_rows, missing) = model_details;
    let mut out = format!("# Perennos Tempo conformance report\n\nCase: {case_id}\nProduct version: {version}\nEdition: {}\nCreated at (Unix): {created}\nStatus: In progress\n\n", if edition == "cayman" { "Caymanian Integrated Systems" } else { "General" });
    out.push_str("This report organizes supplied records. It does not certify legal compliance. Human assertions need independent validation. Identity, role, and Cayman residence have not been validated.\n\n");
    for section in sections {
        out.push_str(&format!(
            "## {}\n\nStatus: {}\nSource: {}\nRecords: {}\nSHA-256: {}\n",
            section.name,
            section.status,
            section.source,
            section.count,
            section.hash.as_deref().unwrap_or("not available")
        ));
        if let Some(reason) = &section.reason {
            out.push_str(&format!("Note: {reason}\n"));
        }
        for row in &section.rows {
            out.push_str(&format!("\n{}\n", row.pretty().trim()));
        }
        if !section.missing.is_empty() {
            out.push_str(&format!("Missing fields: {}\n", section.missing.join(", ")));
        }
        out.push('\n');
    }
    out.push_str(&format!("## Perennos Tempo agent model register\n\nStatus: {model_status}\nRecords: {}\nSource: agent configuration\nSHA-256: {}\n", model_rows.len(), model_hash.unwrap_or("not available")));
    for row in model_rows {
        out.push_str(&format!("{}\n", row.pretty().trim()));
    }
    out.push('\n');
    out.push_str("## Signature verification\n\nStatus: Untested\nThe upstream signed verifier is not available. A file hash does not prove who signed it or whether its contents are true.\n\n");
    out.push_str("## Missing evidence and next actions\n\n");
    if missing.is_empty() {
        out.push_str("No fixed-file input was missing. Human validation and signed verification remain open.\n");
    }
    for item in missing {
        out.push_str(&format!("- {}\n", item.compact()));
    }
    out.push_str("- Complete missing fixed files and fields.\n- Have each human assertion checked independently.\n- Use the upstream signed verifier when its export interface is available.\n");
    out.push_str("\nThis export selects allowlisted metadata fields. Review it for sensitive content; human validation remains open. No legal deadline or notification advice is calculated.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("tempo-conformance-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn ctx(root: &Path) -> Ctx {
        Ctx {
            home: root.join("home"),
            agent_data: root.join("agent"),
            vault: root.join("vault"),
            bin_dirs: Vec::new(),
            browser: false,
            opened: RefCell::new(Vec::new()),
        }
    }

    fn opts(root: &Path) -> Opts {
        Opts {
            case_id: "case-1".into(),
            out: root.join("fresh-output"),
            edition: "cayman".into(),
        }
    }

    #[test]
    fn command_options_reject_paths_and_duplicates() {
        assert!(parse_args(&[
            "--case".into(),
            "../escape".into(),
            "--out".into(),
            "new".into()
        ])
        .is_err());
        assert!(parse_args(&[
            "--case".into(),
            "ok".into(),
            "--case".into(),
            "again".into(),
            "--out".into(),
            "new".into()
        ])
        .is_err());
        assert_eq!(
            parse_args(&[
                "--case".into(),
                "ok_1".into(),
                "--out".into(),
                "fresh".into()
            ])
            .unwrap()
            .edition,
            "cayman"
        );
    }

    #[test]
    fn duplicate_keys_are_checked_inside_objects_and_arrays() {
        let nested = json::parse(r#"{"records":[{"field":1,"field":2}]}"#).unwrap();
        assert!(unique_keys(&nested).is_err());
        let clean = json::parse(r#"{"records":[{"field":1}]}"#).unwrap();
        assert!(unique_keys(&clean).is_ok());
    }

    #[test]
    fn allowlist_drops_unknown_values_and_withholds_unsafe_fields() {
        let raw = json::parse(r#"{"case_id":"case-1","tier":1,"purpose":"internal_review","private_notes":"UNIQUE_SENTINEL"}"#).unwrap();
        let (clean, _) = normalized_row(&raw, USE_CASE, Some("case-1")).unwrap();
        assert!(!clean.pretty().contains("UNIQUE_SENTINEL"));
        assert!(!clean.pretty().contains("private_notes"));
        let unsafe_row =
            json::parse(r#"{"case_id":"case-1","purpose":"C:\\private\\notes"}"#).unwrap();
        assert!(normalized_row(&unsafe_row, USE_CASE, Some("case-1")).is_err());
        assert!(safe_text("multi\nline").is_err());
    }

    #[test]
    fn credential_shaped_metadata_is_rejected_without_echoing_it() {
        let samples = [
            ["s", "k", "-", "runtime-only-dummy"].concat(),
            ["g", "h", "p", "_", "runtime-only-dummy"].concat(),
        ];
        for sample in samples {
            let error = safe_model_text(&sample).unwrap_err();
            assert!(!error.contains(sample.as_str()));
            assert!(safe_text(&sample).is_err());
        }
    }

    #[test]
    fn config_json_wins_when_both_agent_files_exist() {
        let root = temp();
        let context = ctx(&root);
        std::fs::create_dir_all(&context.agent_data).unwrap();
        std::fs::write(
            context.agent_data.join("config.json"),
            r#"{"models":["canonical-model"]}"#,
        )
        .unwrap();
        std::fs::write(
            context.agent_data.join("agent-config.json"),
            r#"{"models":["legacy-model"]}"#,
        )
        .unwrap();
        let (_, _, rows, _) = config_models(&context, false).unwrap();
        assert!(rows.iter().any(|row| {
            row.get("model_id").and_then(json_is_string) == Some("canonical-model")
        }));
        assert!(!rows
            .iter()
            .any(|row| { row.get("model_id").and_then(json_is_string) == Some("legacy-model") }));
    }

    #[test]
    fn export_is_create_only_and_does_not_trust_signature_booleans() {
        let root = temp();
        let context = ctx(&root);
        std::fs::create_dir_all(context.home.join("conformance/case-1")).unwrap();
        std::fs::create_dir_all(&context.agent_data).unwrap();
        std::fs::write(context.agent_data.join("config.json"), r#"{"models":["qwen3.6:35b"],"ollama_url":"http://127.0.0.1:11434","unlisted":"UNIQUE_CONFIG_SENTINEL"}"#).unwrap();
        std::fs::write(context.home.join("conformance/case-1/use-case.json"), r#"{"schema_version":1,"case_id":"case-1","tier":2,"purpose":"internal_review","unlisted":"UNIQUE_SOURCE_SENTINEL"}"#).unwrap();
        std::fs::write(context.home.join("conformance/case-1/signed-receipts.json"), format!(r#"{{"schema_version":1,"records":[{{"receipt_id":"r1","artifact_digest":"{}","signed":true,"verified":true}}]}}"#, "a".repeat(64))).unwrap();
        assert_eq!(export(&context, &opts(&root)).unwrap().0, 2);
        let out = root.join("fresh-output");
        let evidence = std::fs::read_to_string(out.join("evidence.json")).unwrap();
        let report = std::fs::read_to_string(out.join("report.md")).unwrap();
        assert!(out.join("manifest.json").is_file());
        assert!(evidence.contains("qwen3.6:35b"));
        assert!(!evidence.contains("UNIQUE_CONFIG_SENTINEL"));
        assert!(!evidence.contains("UNIQUE_SOURCE_SENTINEL"));
        assert!(!evidence.contains("\"signed\":true") && !evidence.contains("\"verified\":true"));
        assert!(evidence.contains("\"signature_verification\": {\n    \"status\": \"Untested\""));
        assert!(evidence.contains("Identity, role, and Cayman residence have not been validated."));
        assert!(report.contains("Product version:") && report.contains("SHA-256:"));
        let existing = out.join("keep.txt");
        std::fs::write(&existing, "keep").unwrap();
        assert!(export(&context, &opts(&root)).is_err());
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "keep");
    }

    #[test]
    fn missing_and_malformed_inputs_keep_their_statuses_distinct() {
        let root = temp();
        let context = ctx(&root);
        std::fs::create_dir_all(context.home.join("conformance/case-1")).unwrap();
        std::fs::write(
            context.home.join("conformance/case-1/use-case.json"),
            r#"{"schema_version":2,"case_id":"case-1"}"#,
        )
        .unwrap();
        let section = section_input(
            &context.home.join("conformance/case-1"),
            "case-1",
            "use-case.json",
            "use case",
            USE_CASE,
            true,
        )
        .unwrap();
        assert_eq!(section.status, "In progress");
        assert!(section.reason.unwrap().contains("schema_version"));
        let absent = section_input(
            &context.home.join("conformance/case-1"),
            "case-1",
            "data-flow.json",
            "data flow",
            DATA_FLOW,
            false,
        )
        .unwrap();
        assert_eq!(absent.status, "Untested");
        assert!(absent.source.ends_with("data-flow.json"));
    }

    #[test]
    fn bounded_reader_stops_oversized_sources() {
        let root = temp();
        let path = root.join("large.json");
        std::fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(read_bounded(&path).unwrap_err().contains("1 MiB"));
    }

    #[cfg(unix)]
    #[test]
    fn export_refuses_a_symlinked_home_before_canonicalization() {
        use std::os::unix::fs::symlink;
        let root = temp();
        let context = ctx(&root);
        let actual_home = root.join("actual-home");
        std::fs::create_dir_all(actual_home.join("conformance/case-1")).unwrap();
        symlink(&actual_home, &context.home).unwrap();
        let error = export(&context, &opts(&root)).unwrap_err();
        assert!(error.contains("symbolic link"));
        assert!(!root.join("fresh-output").exists());
    }

    #[cfg(unix)]
    #[test]
    fn case_directory_symlink_is_not_read() {
        use std::os::unix::fs::symlink;
        let root = temp();
        let context = ctx(&root);
        std::fs::create_dir_all(context.home.join("conformance")).unwrap();
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join("use-case.json"),
            r#"{"schema_version":1,"case_id":"case-1","tier":1}"#,
        )
        .unwrap();
        symlink(&outside, context.home.join("conformance/case-1")).unwrap();
        let section = section_input(
            &context.home.join("conformance/case-1"),
            "case-1",
            "use-case.json",
            "use case",
            USE_CASE,
            true,
        )
        .unwrap();
        assert_eq!(section.status, "In progress");
        assert!(section.reason.unwrap().contains("symbolic link"));
    }
}
