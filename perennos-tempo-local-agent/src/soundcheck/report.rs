//! How a finished soundcheck is written down: Markdown for people, JSON for scripts, SARIF
//! for code scanning. Only the Markdown carries the little flourishes; JSON and SARIF stay
//! plain so other tools can rely on them.

use super::{Finding, Origin, Report, Severity, Status};
use serde_json::{json, Value};

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Scanner => "scanner",
        Status::Confirmed => "confirmed",
        Status::Unchecked => "not checked",
        Status::CouldNotCheck => "could not be checked",
        Status::NotConfirmed => "not confirmed",
    }
}

fn found_by(f: &Finding) -> String {
    match f.origin {
        Origin::Scanner => format!("a scanner (`{}`), no model involved", f.rule),
        Origin::Model => {
            let lens = super::Lens::from_key(&f.rule).map_or(f.rule.clone(), |l| l.label());
            let second = match f.status {
                Status::Confirmed => "; it survived a second listen that tried to refute it",
                Status::NotConfirmed => "; it did not survive a second listen",
                Status::CouldNotCheck => "; the second listen could not run, so it is unchecked",
                _ => "; low severity, so it was not sent to a second listen",
            };
            format!("the model, through {lens}{second}")
        }
    }
}

fn place(f: &Finding) -> String {
    if f.line > 0 {
        format!("{}:{}", f.file, f.line)
    } else {
        f.file.clone()
    }
}

fn write_finding(out: &mut String, n: usize, f: &Finding) {
    out.push_str(&format!(
        "### {n}. {} - {} - `{}`\n\n{}\n\n",
        f.severity.name().to_uppercase(),
        f.category,
        place(f),
        f.problem
    ));
    if !f.evidence.is_empty() {
        out.push_str(&format!("```\n{}\n```\n\n", f.evidence));
    }
    out.push_str(&format!(
        "- What goes wrong: {}\n- Fix: {}\n",
        f.scenario, f.fix
    ));
    out.push_str(&format!("- Found by: {}\n", found_by(f)));
    if !f.check_note.is_empty() {
        out.push_str(&format!("- Second listen said: {}\n", f.check_note));
    }
    out.push('\n');
}

/// The report for a person. `verbose` adds what the second listen knocked out.
pub fn markdown(r: &Report, verbose: bool) -> String {
    let mut out = format!(
        "# Tempo Soundcheck\n\n**{}** ({})\n\nListened to: {} ({} file{}). Nothing left this computer.\n\n",
        r.verdict.headline(),
        r.verdict.plain(),
        r.source,
        r.files,
        if r.files == 1 { "" } else { "s" },
    );
    if r.findings.is_empty() {
        out.push_str("No findings.\n\n");
    } else {
        out.push_str(&counts_line(&r.findings));
        for (i, f) in r.findings.iter().enumerate() {
            write_finding(&mut out, i + 1, f);
        }
    }
    if !r.dropped.is_empty() {
        if verbose {
            out.push_str("## Didn't hold up on a second listen\n\n");
            for (i, f) in r.dropped.iter().enumerate() {
                write_finding(&mut out, i + 1, f);
            }
        } else {
            out.push_str(&format!(
                "{} finding(s) didn't hold up on a second listen and were left out (--verbose shows them).\n\n",
                r.dropped.len()
            ));
        }
    }
    if !r.notes.is_empty() {
        out.push_str("## Notes\n\n");
        for n in &r.notes {
            out.push_str(&format!("- {n}\n"));
        }
        out.push('\n');
    }
    for line in &r.flourishes {
        out.push_str(&format!("_{line}_\n\n"));
    }
    out.trim_end().to_string() + "\n"
}

fn counts_line(findings: &[Finding]) -> String {
    let count = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
    format!(
        "{} critical, {} high, {} medium, {} low.\n\n",
        count(Severity::Critical),
        count(Severity::High),
        count(Severity::Medium),
        count(Severity::Low)
    )
}

fn finding_json(f: &Finding) -> Value {
    json!({
        "file": f.file,
        "line": f.line,
        "severity": f.severity.name(),
        "category": f.category,
        "problem": f.problem,
        "scenario": f.scenario,
        "fix": f.fix,
        "evidence": f.evidence,
        "origin": match f.origin { Origin::Scanner => "scanner", Origin::Model => "model" },
        "rule": f.rule,
        "status": status_word(f.status),
        "second_listen": f.check_note,
    })
}

/// The report for scripts.
pub fn json(r: &Report) -> Value {
    json!({
        "tool": "tempo-soundcheck",
        "version": env!("CARGO_PKG_VERSION"),
        "verdict": r.verdict.key(),
        "verdict_text": r.verdict.headline(),
        "source": r.source,
        "files": r.files,
        "chunks": r.chunks,
        "model_calls": r.model_calls,
        "findings": r.findings.iter().map(finding_json).collect::<Vec<_>>(),
        "not_confirmed": r.dropped.iter().map(finding_json).collect::<Vec<_>>(),
        "notes": r.notes,
    })
}

fn sarif_level(s: Severity) -> &'static str {
    match s {
        Severity::Critical | Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// SARIF 2.1.0, the format GitHub code scanning reads. Only findings that stood are included.
pub fn sarif(r: &Report) -> Value {
    let rule_id = |f: &Finding| match f.origin {
        Origin::Scanner => format!("tempo.scanner.{}", f.rule),
        Origin::Model => format!("tempo.model.{}", f.rule),
    };
    let mut rule_ids: Vec<String> = r.findings.iter().map(rule_id).collect();
    rule_ids.sort();
    rule_ids.dedup();
    let rules: Vec<Value> = rule_ids
        .iter()
        .map(|id| json!({"id": id, "name": id, "shortDescription": {"text": format!("Tempo Soundcheck rule {id}")}}))
        .collect();
    let results: Vec<Value> = r
        .findings
        .iter()
        .map(|f| {
            json!({
                "ruleId": rule_id(f),
                "level": sarif_level(f.severity),
                "message": {"text": format!("{} What goes wrong: {} Fix: {}", f.problem, f.scenario, f.fix)},
                "locations": [{"physicalLocation": {
                    "artifactLocation": {"uri": f.file},
                    "region": {"startLine": f.line.max(1), "snippet": {"text": f.evidence}}
                }}],
                "properties": {
                    "severity": f.severity.name(),
                    "category": f.category,
                    "origin": match f.origin { Origin::Scanner => "scanner", Origin::Model => "model" },
                    "status": status_word(f.status)
                }
            })
        })
        .collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {
                "name": "Tempo Soundcheck",
                "version": env!("CARGO_PKG_VERSION"),
                "rules": rules
            }},
            "results": results
        }]
    })
}
