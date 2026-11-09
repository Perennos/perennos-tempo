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
    pub trusted_key: Option<String>,
    pub sign_receipts: bool,
}

pub fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut case_id = None;
    let mut out = None;
    let mut edition = "cayman".to_string();
    let mut edition_set = false;
    let mut trusted_key = None;
    let mut sign_receipts = false;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--sign-receipts" {
            if sign_receipts {
                return Err("conformance takes --sign-receipts at most once".into());
            }
            sign_receipts = true;
            i += 1;
            continue;
        }
        let value = args
            .get(i + 1)
            .ok_or("conformance needs a value after each option")?;
        match args[i].as_str() {
            "--case" if case_id.is_none() => case_id = Some(value.clone()),
            "--out" if out.is_none() => out = Some(PathBuf::from(value)),
            "--trusted-key" if trusted_key.is_none() => {
                if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("trusted key must be 64 hexadecimal characters".into());
                }
                trusted_key = Some(value.to_ascii_lowercase());
            }
            "--edition" if !edition_set => {
                edition = value.clone();
                edition_set = true;
            }
            _ => {
                return Err(
                    "conformance takes --case ID --out NEW_DIRECTORY [--edition cayman|general] [--trusted-key HEX] [--sign-receipts]"
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
    if sign_receipts && trusted_key.is_none() {
        return Err("--sign-receipts needs --trusted-key".into());
    }
    Ok(Opts {
        case_id,
        out,
        edition,
        trusted_key,
        sign_receipts,
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
    signed_batch: Option<J>,
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
        || value.chars().any(char::is_whitespace)
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-:,()".contains(c))
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
        || looks_endpoint_shaped(value)
        || [".com", ".net", ".org", ".local", ".internal", "localhost"]
            .iter()
            .any(|needle| lower.contains(needle))
        || value.contains('/')
        || value.contains('\\')
        || value.contains('@')
        || value.contains("..")
        || value.chars().any(char::is_whitespace)
        || (value.contains('.')
            && !value.contains(':')
            && !value.chars().all(|c| c.is_ascii_digit() || c == '.'))
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-:,()".contains(c))
    {
        return Err("model metadata contains a value this report cannot export".into());
    }
    Ok(value.to_string())
}

fn looks_endpoint_shaped(value: &str) -> bool {
    let dotted_numeric_host = value.split('.').count() == 4
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    dotted_numeric_host
        || value.parse::<std::net::IpAddr>().is_ok()
        || value.parse::<std::net::SocketAddr>().is_ok()
        || value
            .rsplit_once(':')
            .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
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
                signed_batch: None,
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
                signed_batch: None,
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
        signed_batch: None,
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
    if filename == "signed-receipts.json"
        && (value.get("node_public_key").is_some() || value.get("signature").is_some())
    {
        section.signed_batch = Some(value.clone());
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

fn complete_receipts(section: &Section) -> bool {
    section.hash.is_some()
        && section.count > 0
        && section.rows.len() == section.count
        && section.missing.is_empty()
}

fn canonical_receipt_records(rows: &[J]) -> Vec<J> {
    let mut sorted = rows.to_vec();
    sorted.sort_by(|a, b| {
        a.get("receipt_id")
            .and_then(json_is_string)
            .cmp(&b.get("receipt_id").and_then(json_is_string))
    });
    sorted
}

fn signature_verification(ctx: &Ctx, opts: &Opts, section: &Section) -> J {
    let no_batch = |status: &str, reason: &str| {
        object(vec![
            ("status", J::s(status)),
            ("reason", J::s(reason)),
            ("batch", J::Null),
        ])
    };
    let Some(trusted_key) = opts.trusted_key.as_deref() else {
        return no_batch(
            "Untested",
            "No trusted public key was supplied, so signing and verification were not attempted.",
        );
    };
    if section.hash.is_none() {
        return no_batch(
            if opts.sign_receipts {
                "In progress"
            } else {
                "Untested"
            },
            "The fixed signed-receipts input is not available.",
        );
    }

    let verify = |batch: &J| match crate::receipt_batches::verify_batch(
        batch,
        trusted_key,
        &opts.case_id,
    ) {
        Ok(canonical) if complete_receipts(section) => {
            let expected_records = J::Arr(canonical_receipt_records(&section.rows));
            if canonical.get("records") == Some(&expected_records) {
                object(vec![
                    ("status", J::s("Verified complete")),
                    (
                        "reason",
                        J::s("The pinned key verifies this batch seal only; identity, history, truth, completeness and human approval remain unverified."),
                    ),
                    ("batch", canonical),
                ])
            } else {
                no_batch(
                    "In progress",
                    "The signed batch does not match the complete, normalized receipt records.",
                )
            }
        }
        Ok(_) => no_batch(
            "In progress",
            "The receipt records are incomplete or contain values this report cannot export.",
        ),
        Err(_) => no_batch(
            "In progress",
            "The signed batch did not verify for this case and trusted public key.",
        ),
    };

    if let Some(batch) = section.signed_batch.as_ref() {
        return verify(batch);
    }
    if !opts.sign_receipts {
        return no_batch(
            "In progress",
            "No signed envelope was supplied; use --sign-receipts to seal complete normalized receipt records.",
        );
    }
    if !complete_receipts(section) {
        return no_batch(
            "In progress",
            "The receipt input is missing, incomplete, or contains values this report cannot export.",
        );
    }
    let records = J::Arr(section.rows.clone());
    match crate::receipt_batches::sign_batch(ctx, &opts.case_id, &records) {
        Ok(batch) => verify(&batch),
        Err(_) => no_batch(
            "In progress",
            "The local signing identity could not produce a receipt batch.",
        ),
    }
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
    if opts.sign_receipts && opts.trusted_key.is_none() {
        return Err("--sign-receipts needs --trusted-key".into());
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
        let section = section_input(&root, &opts.case_id, file, names[i], fields[i], i == 0)?;
        sections.push(section);
    }
    let signature = signature_verification(ctx, opts, &sections[6]);
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
        ("signature_verification", signature.clone()),
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
        ("next_actions", J::Arr(vec![J::s("Complete missing fixed files and fields."), J::s("Have each human assertion checked independently."), J::s("Supply the trusted public key to verify or seal complete receipt records.")])),
    ]);
    let report = render_markdown(
        opts,
        &created_at,
        crate::VERSION,
        &sections,
        &model_status,
        &signature,
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
    let signature_status = signature
        .get("status")
        .and_then(json_is_string)
        .unwrap_or("In progress");
    Ok((
        2,
        format!(
            "Perennos Tempo wrote the report to {}. Evidence remains In progress; signature verification is {signature_status}.",
            out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "new folder".into())
        ),
    ))
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
    opts: &Opts,
    created: &str,
    version: &str,
    sections: &[Section],
    model_status: &str,
    signature: &J,
    model_details: ModelRenderDetails<'_>,
) -> String {
    let case_id = opts.case_id.as_str();
    let edition = opts.edition.as_str();
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
    let signature_status = signature
        .get("status")
        .and_then(json_is_string)
        .unwrap_or("In progress");
    out.push_str(&format!(
        "## Signature verification\n\nStatus: {signature_status}\n"
    ));
    if let Some(reason) = signature.get("reason").and_then(json_is_string) {
        out.push_str(&format!("{reason}\n"));
    }
    if let Some(batch) = signature.get("batch") {
        if !matches!(batch, J::Null) {
            out.push_str("\nCanonical signed batch:\n\n```json\n");
            out.push_str(&batch.pretty());
            out.push_str("\n```\n");
        }
    }
    out.push('\n');
    out.push_str("## Missing evidence and next actions\n\n");
    if missing.is_empty() {
        out.push_str("No fixed-file input was missing. Human validation remains open; signature status appears above.\n");
    }
    for item in missing {
        out.push_str(&format!("- {}\n", item.compact()));
    }
    out.push_str("- Complete missing fixed files and fields.\n- Have each human assertion checked independently.\n- Supply the trusted public key to verify or seal complete receipt records.\n");
    out.push_str("\nThis export selects allowlisted metadata fields. Review it for sensitive content; human validation remains open. No legal deadline or notification advice is calculated.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn private_dir(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path).unwrap();
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(path).unwrap();
    }

    fn temp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = temp_dir.join(format!("tempo-conformance-{}-{n}", std::process::id()));
        private_dir(&path);
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

    fn install_identity(context: &Ctx) -> String {
        use std::io::Write;

        private_dir(&context.home);
        let seed: [u8; 32] = crate::fleet::crypto::random(32).try_into().unwrap();
        let public_key = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = crate::fleet::crypto::hex(public_key.as_bytes());
        let path = context.home.join("fleet-identity");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).unwrap();
        file.write_all(&seed).unwrap();
        file.sync_all().unwrap();
        trusted_key
    }

    fn opts(root: &Path) -> Opts {
        Opts {
            case_id: "case-1".into(),
            out: root.join("fresh-output"),
            edition: "cayman".into(),
            trusted_key: None,
            sign_receipts: false,
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
        let key = "a".repeat(64);
        let signed = parse_args(&[
            "--case".into(),
            "ok".into(),
            "--out".into(),
            "fresh".into(),
            "--trusted-key".into(),
            key.clone(),
            "--sign-receipts".into(),
        ])
        .unwrap();
        assert_eq!(signed.trusted_key.as_deref(), Some(key.as_str()));
        assert!(signed.sign_receipts);
        assert!(parse_args(&[
            "--case".into(),
            "ok".into(),
            "--out".into(),
            "fresh".into(),
            "--sign-receipts".into(),
        ])
        .is_err());
        assert!(parse_args(&[
            "--case".into(),
            "ok".into(),
            "--out".into(),
            "fresh".into(),
            "--trusted-key".into(),
            "not-a-key".into(),
        ])
        .is_err());
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
    fn exported_labels_must_be_compact_and_model_endpoints_are_rejected() {
        for value in ["Person Name", "Street address", "owner role"] {
            assert!(safe_text(value).is_err());
        }
        assert!(safe_text("owner_role").is_ok());
        for value in ["Person Name", "Street address"] {
            let row = object(vec![
                ("case_id", J::s("case-1")),
                ("owner_role", J::s(value)),
            ]);
            assert!(normalized_row(&row, USE_CASE, Some("case-1")).is_err());
        }
        let local_host = ["127", "0", "0", "1"].join(".");
        assert!(safe_model_text(&format!("{local_host}:11434")).is_err());
        let dotted_number = ["192", "168", "0", "193"].join(".");
        assert!(safe_model_text(&dotted_number).is_err());
        assert!(safe_model_text("qwen3.6:35b").is_ok());
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
        assert!(evidence.contains("\"batch\": null"));
        assert!(!context.home.join("fleet-identity").exists());
        let existing = out.join("keep.txt");
        std::fs::write(&existing, "keep").unwrap();
        assert!(export(&context, &opts(&root)).is_err());
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "keep");
    }

    #[test]
    fn signing_and_verifying_batch_exports_sorted_proof_and_keeps_human_status_open() {
        let root = temp();
        let context = ctx(&root);
        let trusted_key = install_identity(&context);
        let input_dir = context.home.join("conformance/case-1");
        std::fs::create_dir_all(&input_dir).unwrap();
        std::fs::write(
            input_dir.join("signed-receipts.json"),
            format!(
                r#"{{"schema_version":1,"records":[{{"receipt_id":"zeta","artifact_digest":"{}","signed":true,"verified":true,"private_notes":"RAW-RECEIPT-SENTINEL"}},{{"receipt_id":"alpha","artifact_digest":"{}"}}]}}"#,
                "b".repeat(64),
                "a".repeat(64)
            ),
        )
        .unwrap();
        let mut options = opts(&root);
        options.trusted_key = Some(trusted_key.clone());
        options.sign_receipts = true;

        assert_eq!(export(&context, &options).unwrap().0, 2);
        let out = root.join("fresh-output");
        let evidence =
            json::parse(&std::fs::read_to_string(out.join("evidence.json")).unwrap()).unwrap();
        let signature = evidence.get("signature_verification").unwrap();
        assert_eq!(
            signature.get("status").and_then(json_is_string),
            Some("Verified complete")
        );
        assert_eq!(
            evidence.get("status").and_then(json_is_string),
            Some("In progress")
        );
        let batch = signature.get("batch").unwrap();
        let J::Arr(records) = batch.get("records").unwrap() else {
            panic!("canonical batch records are missing");
        };
        let ids = records
            .iter()
            .map(|row| row.get("receipt_id").and_then(json_is_string).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["alpha", "zeta"]);
        assert_eq!(
            &crate::receipt_batches::verify_batch(batch, &trusted_key, "case-1").unwrap(),
            batch
        );
        let J::Arr(sections) = evidence.get("sections").unwrap() else {
            panic!("report sections are missing");
        };
        let receipts = sections
            .iter()
            .find(|section| section.get("name").and_then(json_is_string) == Some("signed receipts"))
            .unwrap();
        assert_eq!(
            receipts.get("status").and_then(json_is_string),
            Some("In progress")
        );
        let report = std::fs::read_to_string(out.join("report.md")).unwrap();
        assert!(report.contains("Status: Verified complete"));
        assert!(report.contains(&batch.pretty()));
        assert!(!report.contains("RAW-RECEIPT-SENTINEL"));
        assert!(!report.contains("\"signed\":true") && !report.contains("\"verified\":true"));

        let supplied_root = temp();
        let supplied = ctx(&supplied_root);
        let supplied_dir = supplied.home.join("conformance/case-1");
        std::fs::create_dir_all(&supplied_dir).unwrap();
        std::fs::write(supplied_dir.join("signed-receipts.json"), batch.pretty()).unwrap();
        let mut supplied_options = opts(&supplied_root);
        supplied_options.trusted_key = Some(trusted_key.clone());
        assert_eq!(export(&supplied, &supplied_options).unwrap().0, 2);
        let supplied_evidence = json::parse(
            &std::fs::read_to_string(supplied_root.join("fresh-output/evidence.json")).unwrap(),
        )
        .unwrap();
        let supplied_signature = supplied_evidence.get("signature_verification").unwrap();
        assert_eq!(
            supplied_signature.get("status").and_then(json_is_string),
            Some("Verified complete")
        );
        assert_eq!(supplied_signature.get("batch"), Some(batch));
        assert!(!supplied.home.join("fleet-identity").exists());
    }

    #[test]
    fn bad_pins_and_redacted_receipts_never_emit_verified_batch_proof() {
        let root = temp();
        let context = ctx(&root);
        let trusted_key = install_identity(&context);
        let records = J::Arr(vec![object(vec![
            ("receipt_id", J::s("receipt-1")),
            ("artifact_digest", J::s(&"a".repeat(64))),
        ])]);
        let batch = crate::receipt_batches::sign_batch(&context, "case-1", &records).unwrap();
        let tampered_text = batch.pretty().replace(
            &format!("\"{}\"", "a".repeat(64)),
            &format!("\"{}\"", "b".repeat(64)),
        );
        assert_ne!(tampered_text, batch.pretty());
        let tampered = json::parse(&tampered_text).unwrap();
        assert!(crate::receipt_batches::verify_batch(&tampered, &trusted_key, "case-1").is_err());
        let wrong_key = format!(
            "{}{}",
            if trusted_key.starts_with('0') {
                '1'
            } else {
                '0'
            },
            &trusted_key[1..]
        );
        assert!(crate::receipt_batches::verify_batch(&batch, &wrong_key, "case-1").is_err());
        assert!(crate::receipt_batches::verify_batch(&batch, &trusted_key, "other-case").is_err());

        let input_dir = context.home.join("conformance/case-1");
        std::fs::create_dir_all(&input_dir).unwrap();
        std::fs::write(input_dir.join("signed-receipts.json"), batch.pretty()).unwrap();
        let mut options = opts(&root);
        options.trusted_key = Some(wrong_key);
        assert_eq!(export(&context, &options).unwrap().0, 2);
        let failed =
            json::parse(&std::fs::read_to_string(root.join("fresh-output/evidence.json")).unwrap())
                .unwrap();
        let signature = failed.get("signature_verification").unwrap();
        assert_eq!(
            signature.get("status").and_then(json_is_string),
            Some("In progress")
        );
        assert!(matches!(signature.get("batch"), Some(J::Null)));
        assert!(
            !std::fs::read_to_string(root.join("fresh-output/report.md"))
                .unwrap()
                .contains("Canonical signed batch")
        );

        let bad_root = temp();
        let bad_context = ctx(&bad_root);
        let bad_key = install_identity(&bad_context);
        let bad_dir = bad_context.home.join("conformance/case-1");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("signed-receipts.json"),
            format!(
                r#"{{"schema_version":1,"records":[{{"receipt_id":"ok","artifact_digest":"{}"}},{{"receipt_id":"private value","artifact_digest":"{}"}}]}}"#,
                "c".repeat(64),
                "d".repeat(64)
            ),
        )
        .unwrap();
        let mut bad_options = opts(&bad_root);
        bad_options.trusted_key = Some(bad_key);
        bad_options.sign_receipts = true;
        assert_eq!(export(&bad_context, &bad_options).unwrap().0, 2);
        let bad_evidence =
            std::fs::read_to_string(bad_root.join("fresh-output/evidence.json")).unwrap();
        assert!(bad_evidence.contains("\"status\": \"In progress\""));
        assert!(bad_evidence.contains("\"batch\": null"));
        assert!(!bad_evidence.contains("private value"));
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
