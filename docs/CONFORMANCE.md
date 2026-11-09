# Perennos Tempo conformance report

Run one local evidence export:

```text
tempo-launch conformance --case CASE_ID --out NEW_DIRECTORY [--edition cayman|general] [--trusted-key HEX] [--sign-receipts]
```

The default edition is Caymanian Integrated Systems. `CASE_ID` uses up to 64 ASCII letters, digits, underscores, or hyphens. The output folder must be new, its parent must already exist, and no path component may be a symlink or Windows reparse point. The command reads files under `<Perennos Tempo home>/conformance/CASE_ID/`. It makes no network or model calls and does not start a service or change live settings.

Use this command only on a stable, owner-controlled filesystem. Static link and reparse-point checks do not establish isolation against concurrent path replacement. The export is not an atomic filesystem boundary or a security certification.

## Input files

Each file is optional. When present, it must be a JSON object with `schema_version: 1`. Duplicate keys anywhere in a file are rejected. Each file is capped at 1 MiB and arrays at 1,000 entries. The exporter opens only these names; it does not crawl the folder or copy other files:

- `use-case.json`: top-level fields `case_id`, `tier`, `purpose`, `owner_role`, `deputy_role`, `approval_reference`, `risk_assessment_reference`, `review_due`, `lawful_basis_reference`, `contract_reference`, `retention_reference`, `stop_test_reference`, `notification_plan_reference`, `tier_reason_reversibility`, `tier_reason_challenge`, `owner_reference`, `deputy_reference`, `approval_date`, `authority_reference`, `change_reference`, `data_subjects_reference`, `confidentiality_reference`, `prohibited_data_reference`, `supplier_terms_reference`, `outsourcing_reference`, `fairness_reference`, `cyber_register_reference`, `output_storage_reference`, `run_volume_reference`, `review_standard_reference`, `human_decisions_reference`, `integrity_review_reference`, `override_review_reference`, `reversal_reference`, `explanation_reference`, `appeal_reference`, and `rehearsal_reference`. `case_id` must match the requested case and `tier` must be 0 through 3.
- `data-flow.json`: `records` with `stage`, `country`, `operator_role`, `data_categories`, `retention`, `access_reference`, and `transfer_basis_reference`.
- `models.json`: `records` with `model_id`, `version`, `digest`, `runtime`, `runtime_version`, `observed_at`, and `change_reference`.
- `signoffs.json`: `records` with `decision_reference`, `reviewer_role`, `decision`, `at`, and `approval_reference`.
- `incidents-overrides.json`: `records` with `event_id`, `kind`, `discovered_at`, `reasonable_awareness_at`, `action_reference`, `owner_role`, `reason_reference`, and `notification_reference`.
- `evaluation-canary.json`: `records` with `evaluation_id`, `model_digest`, `dataset_digest`, `baseline_reference`, `candidate_reference`, `canary_reference`, `rollback_reference`, `reviewer_role`, `result`, and `at`.
- `signed-receipts.json`: either raw `records` with `receipt_id` and `artifact_digest`, or a signed batch with `schema_version: 1`, `case_id`, those `records`, `node_public_key`, and `signature`. Caller supplied signature booleans are ignored.

Use compact role labels and reference IDs with no whitespace, not names, addresses, paths, URLs, keys, secrets, prompts, responses, or other raw evidence. Unknown fields are ignored. Text with whitespace, path separators, contact markers, markup, likely secret markers, or unsupported characters is withheld and the section stays In progress. Character checks cannot identify every personal name or place, so keep personal and confidential details out of every field.

The model-register adapter reads the local agent configuration selected by the launcher, when it is inside the fixed agent data folder. It extracts only the `models`, `fast_models`, and `embed_model` names, rejecting whitespace and endpoint-shaped IDs. It does not export configured endpoints or other settings. This is a configuration snapshot, not runtime identity; it does not establish a live runtime, digest, runtime version, or observed time. If an external configuration path is selected, that adapter remains Untested.

## Output and status

A successful export creates `report.md` and `evidence.json`, then writes `manifest.json` last with SHA-256 hashes and `complete: true`. The output contains the normalized sections, record counts, source byte hashes, product version, edition, creation time, missing evidence, next actions, and boundaries. Unix output files use mode 0600 and the folder uses mode 0700. Existing output is never overwritten. Only a valid manifest whose hashes match both output files establishes a complete export; this marker says the export finished, not that its contents are approved. If a write fails after the folder is created, the incomplete folder remains without a valid manifest; use a different new folder on the next attempt.

Present human records stay In progress until an independent reviewer checks them. Missing inputs are Untested and show their exact relative path. Malformed or unsafe inputs stay In progress with a short reason. Without `--trusted-key`, signing and verification are not attempted and the signature status stays Untested; the command does not read or create a signing identity. `--trusted-key` is a pinned 64-character Ed25519 public key. Use `--sign-receipts` with that key to seal complete, normalized receipt records, or provide an existing signed batch with the key to verify it. Invalid signatures, wrong keys, wrong cases, and incomplete records stay In progress, with no batch proof exported. A verified batch means only that the pinned key sealed those exact receipt references; it does not establish who controlled the key, historical job authorship, truth, completeness, human approval, or legal compliance. The signed envelope appears as `signature_verification.batch` in `evidence.json` and in the report only after verification succeeds.

The exporter returns exit code 2 whenever it writes a report, because human review remains open even when a receipt batch seal verifies. Exit code 1 means the report could not be safely written. Input findings such as missing, malformed, or unsafe files are recorded in the report and still return 2.
