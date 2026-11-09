//! A signed snapshot of receipt references supplied by a conformance case.

use crate::fleet::crypto::{hex, unhex};
use crate::setup::json::{parse, J};
use crate::Ctx;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const DOMAIN: &[u8] = b"perennos-tempo.receipt-batch.v1\0";
const MAX_FILE_BYTES: u64 = 1_048_576;
const MAX_RECORDS: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Receipt {
    receipt_id: String,
    artifact_digest: String,
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !looks_secret_shaped(value)
}

fn looks_secret_shaped(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "sk-proj-",
        "sk-ant-",
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "hf_",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "xoxs-",
        "glpat-",
        "npm_",
        "pypi-",
        "akia",
        "asia",
    ];
    value.char_indices().any(|(index, _)| {
        (index == 0 || matches!(value.as_bytes()[index - 1], b'_' | b'-'))
            && PREFIXES
                .iter()
                .any(|prefix| value[index..].starts_with(prefix))
    })
}

fn lower_hex<const N: usize>(value: &str, label: &str) -> Result<[u8; N], String> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!(
            "{label} must be {} lowercase hex characters",
            N * 2
        ));
    }
    let bytes = unhex(value).ok_or_else(|| format!("{label} is malformed"))?;
    bytes
        .try_into()
        .map_err(|_| format!("{label} has the wrong length"))
}

fn exact_fields(value: &J, expected: &[&str], label: &str) -> Result<(), String> {
    let J::Obj(fields) = value else {
        return Err(format!("{label} must be an object"));
    };
    for (i, (key, _)) in fields.iter().enumerate() {
        if !expected.contains(&key.as_str()) {
            return Err(format!("{label} has an unknown field"));
        }
        if fields[..i].iter().any(|(previous, _)| previous == key) {
            return Err(format!("{label} has a duplicate field"));
        }
    }
    if expected
        .iter()
        .any(|name| !fields.iter().any(|(key, _)| key == name))
    {
        return Err(format!("{label} is missing a field"));
    }
    Ok(())
}

fn schema_v1(value: &J, label: &str) -> Result<(), String> {
    match value.get("schema_version") {
        Some(J::Num(version)) if version == "1" => Ok(()),
        _ => Err(format!("{label} must use schema_version 1")),
    }
}

fn normalize_records(records: &J) -> Result<Vec<Receipt>, String> {
    let J::Arr(rows) = records else {
        return Err("records must be an array".into());
    };
    if rows.is_empty() || rows.len() > MAX_RECORDS {
        return Err("records must contain between 1 and 1000 rows".into());
    }
    let mut normalized = Vec::with_capacity(rows.len());
    for row in rows {
        exact_fields(row, &["receipt_id", "artifact_digest"], "receipt row")?;
        let receipt_id = row
            .str("receipt_id")
            .ok_or_else(|| "receipt_id must be a string".to_string())?;
        if !valid_identifier(receipt_id) {
            return Err(
                "receipt_id must be 1-64 ASCII letters, digits, underscores or hyphens".into(),
            );
        }
        let artifact_digest = row
            .str("artifact_digest")
            .ok_or_else(|| "artifact_digest must be a string".to_string())?;
        lower_hex::<32>(artifact_digest, "artifact_digest")?;
        normalized.push(Receipt {
            receipt_id: receipt_id.into(),
            artifact_digest: artifact_digest.into(),
        });
    }
    normalized.sort_by(|a, b| a.receipt_id.cmp(&b.receipt_id));
    if normalized
        .windows(2)
        .any(|pair| pair[0].receipt_id == pair[1].receipt_id)
    {
        return Err("records contain a duplicate receipt_id".into());
    }
    Ok(normalized)
}

fn records_json(records: &[Receipt]) -> J {
    J::Arr(
        records
            .iter()
            .map(|row| {
                J::Obj(vec![
                    ("receipt_id".into(), J::s(&row.receipt_id)),
                    ("artifact_digest".into(), J::s(&row.artifact_digest)),
                ])
            })
            .collect(),
    )
}

fn payload(case_id: &str, records: &[Receipt]) -> J {
    J::Obj(vec![
        ("schema_version".into(), J::Num("1".into())),
        ("case_id".into(), J::s(case_id)),
        ("records".into(), records_json(records)),
    ])
}

fn signed_envelope(case_id: &str, records: &[Receipt], public_key: &str, signature: &str) -> J {
    J::Obj(vec![
        ("schema_version".into(), J::Num("1".into())),
        ("case_id".into(), J::s(case_id)),
        ("records".into(), records_json(records)),
        ("node_public_key".into(), J::s(public_key)),
        ("signature".into(), J::s(signature)),
    ])
}

fn signing_bytes(case_id: &str, records: &[Receipt]) -> Vec<u8> {
    let compact = payload(case_id, records).compact();
    let mut bytes = Vec::with_capacity(DOMAIN.len() + compact.len());
    bytes.extend_from_slice(DOMAIN);
    bytes.extend_from_slice(compact.as_bytes());
    bytes
}

/// Sign this canonical snapshot with the existing Fleet identity in `ctx.home`.
/// A missing or unsafe identity is an error; this path never creates or rotates one.
pub fn sign_batch(ctx: &Ctx, case_id: &str, records: &J) -> Result<J, String> {
    if !valid_identifier(case_id) {
        return Err("case_id must be 1-64 ASCII letters, digits, underscores or hyphens".into());
    }
    let records = normalize_records(records)?;
    let key = read_identity(ctx)?;
    let signature = key.sign(&signing_bytes(case_id, &records));
    Ok(signed_envelope(
        case_id,
        &records,
        &hex(key.verifying_key().as_bytes()),
        &hex(&signature.to_bytes()),
    ))
}

/// Verify a normalized batch against a separately supplied Fleet public-key pin.
pub fn verify_batch(batch: &J, trusted_key: &str, expected_case: &str) -> Result<J, String> {
    exact_fields(
        batch,
        &[
            "schema_version",
            "case_id",
            "records",
            "node_public_key",
            "signature",
        ],
        "signed batch",
    )?;
    schema_v1(batch, "signed batch")?;
    if !valid_identifier(expected_case) {
        return Err("expected case_id is invalid".into());
    }
    let case_id = batch
        .str("case_id")
        .ok_or_else(|| "case_id must be a string".to_string())?;
    if !valid_identifier(case_id) || case_id != expected_case {
        return Err("signed batch case_id does not match the expected case".into());
    }
    let records = normalize_records(
        batch
            .get("records")
            .ok_or_else(|| "signed batch is missing records".to_string())?,
    )?;
    let public_text = batch
        .str("node_public_key")
        .ok_or_else(|| "node_public_key must be a string".to_string())?;
    let public_bytes = lower_hex::<32>(public_text, "node_public_key")?;
    let trusted_bytes = lower_hex::<32>(trusted_key, "trusted public key")?;
    if public_bytes != trusted_bytes {
        return Err("signed batch key does not match the trusted Fleet key".into());
    }
    let signature_text = batch
        .str("signature")
        .ok_or_else(|| "signature must be a string".to_string())?;
    let signature_bytes = lower_hex::<64>(signature_text, "signature")?;
    let verifying_key = VerifyingKey::from_bytes(&public_bytes)
        .map_err(|_| "node_public_key is not a valid Ed25519 key".to_string())?;
    let signature = Signature::from_bytes(&signature_bytes);
    verifying_key
        .verify_strict(&signing_bytes(case_id, &records), &signature)
        .map_err(|_| "signature does not match the canonical receipt batch".to_string())?;
    Ok(signed_envelope(
        case_id,
        &records,
        public_text,
        signature_text,
    ))
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "could not resolve the requested path".to_string())?
            .join(path)
    };
    if absolute
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("paths containing a parent-directory component are refused".into());
    }
    Ok(absolute)
}

fn is_link_or_reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn check_directory(metadata: &Metadata, _home: bool) -> Result<(), String> {
    if is_link_or_reparse(metadata) || !metadata.is_dir() {
        return Err("path has a link or a non-directory ancestor".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        let sticky_shared = mode & 0o1000 != 0;
        if mode & 0o022 != 0 && !sticky_shared {
            return Err("path has an ancestor writable by other accounts".into());
        }
        if _home && (mode & 0o022 != 0 || mode & 0o200 == 0) {
            return Err("Fleet identity home is not owner-controlled".into());
        }
    }
    Ok(())
}

fn checked_path(path: &Path, missing_leaf: bool, directory_leaf: bool) -> Result<PathBuf, String> {
    let absolute = absolute_path(path)?;
    let components: Vec<_> = absolute.components().collect();
    if components.is_empty() || (!directory_leaf && absolute.file_name().is_none()) {
        return Err("requested path has no file name".into());
    }
    let mut current = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::Prefix(_) => {
                current.push(component.as_os_str());
                continue;
            }
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => continue,
            Component::Normal(_) => current.push(component.as_os_str()),
            Component::ParentDir => return Err("parent-directory paths are refused".into()),
        }
        let leaf = index + 1 == components.len();
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if leaf && missing_leaf && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(absolute);
            }
            Err(_) => return Err("requested path does not exist or cannot be inspected".into()),
        };
        if leaf && !directory_leaf {
            if is_link_or_reparse(&metadata) || !metadata.is_file() {
                return Err("requested file is not a regular, non-link file".into());
            }
        } else {
            check_directory(&metadata, leaf && directory_leaf)?;
        }
    }
    Ok(absolute)
}

fn open_read(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|_| "could not open the requested file".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "could not inspect the requested file".to_string())?;
    if is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err("requested file is not a regular, non-link file".into());
    }
    Ok(file)
}

fn set_no_follow(options: &mut OpenOptions) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000); // O_NOFOLLOW
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100); // O_NOFOLLOW
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let path = checked_path(path, false, false)?;
    let file = open_read(&path)?;
    let size = file
        .metadata()
        .map_err(|_| "could not inspect the requested file".to_string())?
        .len();
    if size > MAX_FILE_BYTES {
        return Err("input file exceeds the 1 MiB limit".into());
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read the requested file".to_string())?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("input file exceeds the 1 MiB limit".into());
    }
    Ok(bytes)
}

fn read_identity(ctx: &Ctx) -> Result<SigningKey, String> {
    let requested = ctx.home.join("fleet-identity");
    if matches!(fs::symlink_metadata(&requested), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(
            "Fleet identity is missing; pair this computer before signing receipt batches".into(),
        );
    }
    let home = checked_path(&ctx.home, false, true)?;
    let identity = checked_path(&requested, false, false)?;
    let home_metadata = fs::symlink_metadata(&home)
        .map_err(|_| "could not inspect the Fleet identity home".to_string())?;
    let before = fs::symlink_metadata(&identity)
        .map_err(|_| "could not inspect the Fleet identity".to_string())?;
    check_directory(&home_metadata, true)?;
    if is_link_or_reparse(&before) || !before.is_file() {
        return Err("Fleet identity must be a regular, non-link file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if before.permissions().mode() & 0o077 != 0 || before.uid() != home_metadata.uid() {
            return Err(
                "Fleet identity must be owned by its private home and readable only by its owner"
                    .into(),
            );
        }
    }
    let mut file = open_read(&identity)?;
    let opened = file
        .metadata()
        .map_err(|_| "could not inspect the Fleet identity".to_string())?;
    if opened.len() != 32 || is_link_or_reparse(&opened) || !opened.is_file() {
        return Err("Fleet identity must be exactly 32 bytes".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if opened.permissions().mode() & 0o077 != 0 || opened.uid() != home_metadata.uid() {
            return Err("Fleet identity permissions or owner changed while opening it".into());
        }
    }
    let mut seed = Vec::with_capacity(32);
    Read::by_ref(&mut file)
        .take(33)
        .read_to_end(&mut seed)
        .map_err(|_| "could not read the Fleet identity".to_string())?;
    let seed: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| "Fleet identity must be exactly 32 bytes".to_string())?;
    Ok(SigningKey::from_bytes(&seed))
}

fn read_json(path: &Path, label: &str) -> Result<J, String> {
    let bytes = read_bounded(path)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{label} must be UTF-8 JSON"))?;
    parse(&text).map_err(|_| format!("{label} is not valid JSON"))
}

fn source_records(document: &J) -> Result<&J, String> {
    exact_fields(document, &["schema_version", "records"], "reference batch")?;
    schema_v1(document, "reference batch")?;
    document
        .get("records")
        .ok_or_else(|| "reference batch is missing records".to_string())
}

fn write_new(path: &Path, bytes: &[u8], _home: &Path) -> Result<(), String> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("signed batch exceeds the 1 MiB limit".into());
    }
    let path = checked_path(path, true, false)?;
    let parent = path
        .parent()
        .ok_or_else(|| "output path has no parent folder".to_string())?;
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|_| "output parent folder must already exist".to_string())?;
    check_directory(&parent_metadata, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let home = checked_path(_home, false, true)?;
        let home_metadata = fs::symlink_metadata(home)
            .map_err(|_| "could not inspect the Perennos Tempo home".to_string())?;
        let sticky_shared = parent_metadata.permissions().mode() & 0o1000 != 0;
        if !sticky_shared && parent_metadata.uid() != home_metadata.uid() {
            return Err("output folder is not owned with the Tempo home".into());
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    set_no_follow(&mut options);
    let mut file = options
        .open(&path)
        .map_err(|_| "output must be a new file in an existing safe folder".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "could not inspect the new output file".to_string())?;
    if is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err("output is not a regular file".into());
    }
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "could not finish writing the signed batch".to_string())
}

fn source_path(ctx: &Ctx, case_id: &str) -> PathBuf {
    ctx.home
        .join("conformance")
        .join(case_id)
        .join("signed-receipts.json")
}

/// Read the fixed conformance export, sign it, then create the requested output without replacing it.
pub fn export(ctx: &Ctx, case_id: &str, output: &Path) -> Result<usize, String> {
    if !valid_identifier(case_id) {
        return Err("case_id must be 1-64 ASCII letters, digits, underscores or hyphens".into());
    }
    let document = read_json(&source_path(ctx, case_id), "reference receipt file")?;
    let records = source_records(&document)?;
    let batch = sign_batch(ctx, case_id, records)?;
    let count = batch.arr("records").len();
    let mut bytes = batch.compact().into_bytes();
    bytes.push(b'\n');
    write_new(output, &bytes, &ctx.home)?;
    Ok(count)
}

/// Verify an untrusted file using a caller-supplied Fleet key; return only its case and row count.
pub fn verify_file(path: &Path, trusted_key: &str) -> Result<(String, usize), String> {
    let document = read_json(path, "signed batch")?;
    let expected_case = document
        .str("case_id")
        .ok_or_else(|| "signed batch has no case_id".to_string())?
        .to_string();
    let checked = verify_batch(&document, trusted_key, &expected_case)?;
    Ok((expected_case, checked.arr("records").len()))
}

pub fn parse_export_args(args: &[&str]) -> Result<(String, PathBuf), String> {
    let mut case_id = None;
    let mut output = None;
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i] {
            "--case" => &mut case_id,
            "--out" => &mut output,
            _ => return Err("receipts export: unknown option".into()),
        };
        if slot.is_some() {
            return Err(format!("receipts export: duplicate option {}", args[i]));
        }
        let value = args
            .get(i + 1)
            .copied()
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| format!("receipts export: {} needs a value", args[i]))?;
        *slot = Some(value.to_string());
        i += 2;
    }
    let case_id = case_id.ok_or_else(|| "receipts export needs --case ID".to_string())?;
    if !valid_identifier(&case_id) {
        return Err("receipts export: invalid case ID".into());
    }
    let output = output.ok_or_else(|| "receipts export needs --out FILE".to_string())?;
    Ok((case_id, PathBuf::from(output)))
}

pub fn parse_verify_args(args: &[&str]) -> Result<(PathBuf, String), String> {
    let mut signed = None;
    let mut trusted_key = None;
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i] {
            "--signed" => &mut signed,
            "--trusted-key" => &mut trusted_key,
            _ => return Err("verify: unknown option".into()),
        };
        if slot.is_some() {
            return Err(format!("verify: duplicate option {}", args[i]));
        }
        let value = args
            .get(i + 1)
            .copied()
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| format!("verify: {} needs a value", args[i]))?;
        *slot = Some(value.to_string());
        i += 2;
    }
    let signed = signed.ok_or_else(|| "verify needs --signed FILE".to_string())?;
    let trusted_key =
        trusted_key.ok_or_else(|| "verify needs --trusted-key PUBLIC_HEX".to_string())?;
    lower_hex::<32>(&trusted_key, "trusted public key")?;
    Ok((PathBuf::from(signed), trusted_key))
}

pub fn run_export(ctx: &Ctx, case_id: &str, output: &Path) -> i32 {
    match export(ctx, case_id, output) {
        Ok(count) => {
            println!("Signed {count} receipt(s) for case {case_id}.");
            0
        }
        Err(error) => {
            eprintln!("receipts export: {error}");
            1
        }
    }
}

pub fn run_verify(path: &Path, trusted_key: &str) -> i32 {
    match verify_file(path, trusted_key) {
        Ok((case_id, count)) => {
            println!("Verified {count} signed receipt(s) for case {case_id}.");
            0
        }
        Err(error) => {
            eprintln!("verify: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::crypto::{random, write_private};
    use ed25519_dalek::SigningKey;

    fn temp_home() -> PathBuf {
        let suffix = hex(&random(12));
        let path = std::env::temp_dir().join(format!("tempo-receipt-test-{suffix}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        }
        #[cfg(not(unix))]
        fs::create_dir(&path).unwrap();
        path
    }

    fn ctx(home: PathBuf) -> Ctx {
        Ctx {
            agent_data: home.join("agent"),
            vault: home.join("vault"),
            home,
            bin_dirs: vec![],
            browser: false,
            opened: Default::default(),
        }
    }

    fn identity(home: &Path) -> String {
        let seed: [u8; 32] = random(32).try_into().unwrap();
        write_private(&home.join("fleet-identity"), &seed).unwrap();
        hex(SigningKey::from_bytes(&seed).verifying_key().as_bytes())
    }

    fn records() -> J {
        let digest = "a".repeat(64);
        J::Arr(vec![
            J::Obj(vec![
                ("receipt_id".into(), J::s("receipt_b")),
                ("artifact_digest".into(), J::s(&digest)),
            ]),
            J::Obj(vec![
                ("receipt_id".into(), J::s("receipt_a")),
                ("artifact_digest".into(), J::s(&digest)),
            ]),
        ])
    }

    fn set_digest(batch: &mut J, value: &str) {
        if let J::Obj(fields) = batch {
            if let Some((_, J::Arr(rows))) = fields.iter_mut().find(|(key, _)| key == "records") {
                if let Some(J::Obj(row)) = rows.first_mut() {
                    if let Some((_, digest)) =
                        row.iter_mut().find(|(key, _)| key == "artifact_digest")
                    {
                        *digest = J::s(value);
                    }
                }
            }
        }
    }

    #[test]
    fn signs_and_verifies_sorted_rows_with_existing_fleet_identity() {
        let home = temp_home();
        let trusted = identity(&home);
        let batch = sign_batch(&ctx(home), "case_1", &records()).unwrap();
        let checked = verify_batch(&batch, &trusted, "case_1").unwrap();
        assert_eq!(checked, batch);
        let rows = checked.arr("records");
        assert_eq!(rows[0].str("receipt_id"), Some("receipt_a"));
        assert_eq!(rows[1].str("receipt_id"), Some("receipt_b"));
    }

    #[test]
    fn rejects_forged_digest_wrong_key_and_wrong_case() {
        let home = temp_home();
        let trusted = identity(&home);
        let other_seed: [u8; 32] = random(32).try_into().unwrap();
        let other_key = hex(SigningKey::from_bytes(&other_seed)
            .verifying_key()
            .as_bytes());
        let batch = sign_batch(&ctx(home), "case_1", &records()).unwrap();
        assert!(verify_batch(&batch, &other_key, "case_1").is_err());
        assert!(verify_batch(&batch, &trusted, "case_2").is_err());
        let mut forged = batch;
        set_digest(&mut forged, &"b".repeat(64));
        assert!(verify_batch(&forged, &trusted, "case_1").is_err());
    }

    #[test]
    fn rejects_unknown_duplicate_fields_and_duplicate_receipt_ids() {
        let home = temp_home();
        let trusted = identity(&home);
        let batch = sign_batch(&ctx(home), "case_1", &records()).unwrap();

        let mut unknown = batch.clone();
        if let J::Obj(fields) = &mut unknown {
            fields.push(("head".into(), J::s("not part of this schema")));
        }
        assert!(verify_batch(&unknown, &trusted, "case_1").is_err());

        let mut duplicate = batch.clone();
        if let J::Obj(fields) = &mut duplicate {
            fields.push(("case_id".into(), J::s("case_1")));
        }
        assert!(verify_batch(&duplicate, &trusted, "case_1").is_err());

        let mut row_unknown = records();
        if let J::Arr(rows) = &mut row_unknown {
            if let J::Obj(fields) = &mut rows[0] {
                fields.push(("extra".into(), J::Null));
            }
        }
        assert!(sign_batch(&ctx(temp_home()), "case_1", &row_unknown).is_err());

        let mut row_duplicate = records();
        if let J::Arr(rows) = &mut row_duplicate {
            if let J::Obj(fields) = &mut rows[0] {
                fields.push(("receipt_id".into(), J::s("receipt_b")));
            }
        }
        assert!(sign_batch(&ctx(temp_home()), "case_1", &row_duplicate).is_err());

        let mut repeated = records();
        if let J::Arr(rows) = &mut repeated {
            rows[1].set("receipt_id", J::s("receipt_b"));
        }
        assert!(sign_batch(&ctx(temp_home()), "case_1", &repeated).is_err());

        assert!(sign_batch(&ctx(temp_home()), "sk-proj-secret-shaped", &records()).is_err());
        let mut credential_shaped = records();
        if let J::Arr(rows) = &mut credential_shaped {
            rows[0].set("receipt_id", J::s("case-ghp_secret-shaped"));
        }
        assert!(sign_batch(&ctx(temp_home()), "case_1", &credential_shaped).is_err());
    }

    #[test]
    fn missing_identity_does_not_create_identity_or_output() {
        let home = temp_home();
        let source_dir = home.join("conformance").join("case_1");
        fs::create_dir_all(&source_dir).unwrap();
        let source = J::Obj(vec![
            ("schema_version".into(), J::Num("1".into())),
            ("records".into(), records()),
        ]);
        fs::write(source_dir.join("signed-receipts.json"), source.compact()).unwrap();
        let output = home.join("signed.json");
        assert!(export(&ctx(home.clone()), "case_1", &output).is_err());
        assert!(!home.join("fleet-identity").exists());
        assert!(!output.exists());
    }

    #[test]
    fn output_creation_is_exclusive_and_owner_only_on_unix() {
        let home = temp_home();
        let output = home.join("signed.json");
        write_new(&output, b"first", &home).unwrap();
        assert!(write_new(&output, b"second", &home).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cli_options_reject_unknown_and_duplicate_flags() {
        assert!(
            parse_export_args(&["--case", "case_1", "--case", "case_2", "--out", "x"]).is_err()
        );
        assert!(parse_export_args(&["--case", "case_1", "--out", "x", "--unknown", "y"]).is_err());
        assert!(parse_verify_args(&[
            "--signed",
            "x",
            "--trusted-key",
            &"a".repeat(64),
            "--signed",
            "y"
        ])
        .is_err());
    }
}
