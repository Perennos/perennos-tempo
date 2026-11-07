//! Changes to the agent's settings that a person approves: `tempo-launch link URL` (add another
//! computer's Ollama), and a proposal an AI wrote through `tempo-launch mcp` (`setup_propose`),
//! which only the person applies, with `tempo-launch setup --apply-proposal`, after a diff and a
//! y/N. Nothing here is ever applied by the AI.

use super::apply::{agent_settings_path, pinned_agent_settings};
use super::json::{parse, J};
use crate::{file_stamp, parse_remote_url, stamp, write_atomic, Ctx};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where an AI's proposal waits, inside the Tempo home.
pub const PROPOSAL_FILE: &str = "proposed-settings.json";
/// The command a person runs to apply it.
pub const APPLY_COMMAND: &str = "tempo-launch setup --apply-proposal";
/// The port Ollama listens on unless told otherwise.
pub const OLLAMA_DEFAULT_PORT: u16 = 11434;
/// The settings an AI may propose.
pub const ALLOWED_KEYS: [&str; 5] = [
    "models",
    "fast_models",
    "embed_model",
    "ollama_url",
    "ollama_fallbacks",
];

/// An Ollama address as the agent stores it: `http(s)://host:port` on a private, tailnet or local
/// address (the same check as remote.json), the port 11434 when none is given.
pub fn engine_url(url: &str) -> Result<String, String> {
    let t = url.trim().trim_end_matches('/');
    parse_remote_url(t)?;
    let rest = t.split_once("://").map_or(t, |x| x.1);
    let has_port = if rest.starts_with('[') {
        rest.contains("]:")
    } else {
        rest.contains(':')
    };
    let full = if has_port {
        t.to_string()
    } else {
        format!("{t}:{OLLAMA_DEFAULT_PORT}")
    };
    parse_remote_url(&full)?;
    Ok(full)
}

/// A model name as Ollama writes it: letters, digits and `: . - _ /`, 1 to 100 characters.
pub fn model_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b":.-_/".contains(&b))
}

fn names(v: &J, key: &str, max: usize) -> Result<J, String> {
    let J::Arr(a) = v else {
        return Err(format!("{key} must be a list of model names"));
    };
    if a.is_empty() || a.len() > max {
        return Err(format!("{key} must hold 1 to {max} names"));
    }
    let mut out = Vec::new();
    for x in a {
        match x {
            J::Str(s) if model_name_ok(s) => out.push(J::s(s)),
            _ => {
                return Err(format!(
                    "{key}: not a model name (letters, digits and : . - _ / only, at most 100)"
                ))
            }
        }
    }
    Ok(J::Arr(out))
}

/// The proposed settings, checked and in their stored form. Only the keys in `ALLOWED_KEYS`;
/// every address must be private, tailnet or local.
pub fn check_settings(v: &J) -> Result<Vec<(String, J)>, String> {
    let J::Obj(o) = v else {
        return Err("settings must be an object".into());
    };
    if o.is_empty() {
        return Err("settings is empty: name at least one setting".into());
    }
    let mut out = Vec::new();
    for (k, val) in o {
        let checked = match k.as_str() {
            "models" => names(val, k, 8)?,
            "fast_models" => names(val, k, 8)?,
            "embed_model" => match val {
                J::Str(s) if model_name_ok(s) => J::s(s),
                _ => return Err("embed_model must be one model name".into()),
            },
            "ollama_url" => match val {
                J::Str(s) => J::s(&engine_url(s)?),
                _ => return Err("ollama_url must be an address".into()),
            },
            "ollama_fallbacks" => match val {
                J::Arr(a) if a.len() <= 8 => {
                    let mut urls: Vec<J> = Vec::new();
                    for x in a {
                        match x {
                            J::Str(s) => {
                                let u = J::s(&engine_url(s)?);
                                if !urls.contains(&u) {
                                    urls.push(u);
                                }
                            }
                            _ => return Err("ollama_fallbacks must be a list of addresses".into()),
                        }
                    }
                    J::Arr(urls)
                }
                _ => return Err("ollama_fallbacks must be a list of at most 8 addresses".into()),
            },
            other => {
                return Err(format!(
                    "{other} cannot be proposed. Allowed settings: {}",
                    ALLOWED_KEYS.join(", ")
                ))
            }
        };
        out.push((k.clone(), checked));
    }
    Ok(out)
}

/// A note an AI wrote, as plain short text: control characters become spaces.
pub fn clean_note(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    t.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

/// Write `<home>/proposed-settings.json` and return the command that applies it.
pub fn write_proposal(
    ctx: &Ctx,
    settings: &J,
    note: &str,
    now: u64,
) -> Result<&'static str, String> {
    let checked = check_settings(settings)?;
    let doc = J::Obj(vec![
        (
            "proposed_by".into(),
            J::s("an AI, through tempo-launch mcp"),
        ),
        ("proposed_at".into(), J::s(&stamp(now))),
        ("note".into(), J::s(&clean_note(note))),
        ("settings".into(), J::Obj(checked)),
    ]);
    write_atomic(&ctx.home.join(PROPOSAL_FILE), doc.pretty().as_bytes())
        .map_err(|e| format!("could not write the proposal: {e}"))?;
    Ok(APPLY_COMMAND)
}

/// Whether a proposal is waiting.
pub fn proposal_waiting(ctx: &Ctx) -> bool {
    ctx.home.join(PROPOSAL_FILE).is_file()
}

/// Copy a file into `<home>/trash/<date>/` (under its own name), so nothing is lost.
fn keep_old(home: &Path, path: &Path, text: &str, now: u64) -> Result<PathBuf, String> {
    let fs = file_stamp(now);
    let dir = home.join("trash").join(&fs[..8]);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not make the trash folder: {e}"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.json".into());
    let mut dest = dir.join(&name);
    if dest.exists() {
        let stem = name.strip_suffix(".json").unwrap_or(&name);
        dest = dir.join(format!("{stem}-{}.json", &fs[9..]));
    }
    std::fs::write(&dest, text).map_err(|e| format!("could not save the old settings: {e}"))?;
    Ok(dest)
}

/// The settings file as an object (empty when there is none), and its text.
fn load_settings(path: &Path) -> Result<(J, Option<String>), String> {
    let old = std::fs::read_to_string(path).ok();
    match &old {
        Some(t) => match parse(t) {
            Ok(j @ J::Obj(_)) => Ok((j, old)),
            _ => Err(format!(
                "{} is not a JSON object, so it was left alone. Fix or move it, then try again.",
                path.display()
            )),
        },
        None => Ok((J::Obj(Vec::new()), None)),
    }
}

/// Put `values` into the settings file; the old file goes to the trash first.
fn store(
    ctx: &Ctx,
    path: &Path,
    mut cfg: J,
    old: Option<String>,
    values: &[(String, J)],
    now: u64,
) -> Result<Option<PathBuf>, String> {
    for (k, v) in values {
        cfg.set(k, v.clone());
    }
    let trashed = match &old {
        Some(t) => Some(keep_old(&ctx.home, path, t, now)?),
        None => None,
    };
    write_atomic(path, cfg.pretty().as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(trashed)
}

fn short(j: &J) -> String {
    let t = j.compact();
    if t.chars().count() > 120 {
        format!("{}...", t.chars().take(117).collect::<String>())
    } else {
        t
    }
}

/// `tempo-launch link URL`: add another computer's Ollama to `ollama_fallbacks`, after the same
/// private-address check remote.json uses. `pinned` is the file TEMPO_AGENT_CONFIG names, if any:
/// then nothing is written and the line to add is printed.
pub fn link(ctx: &Ctx, pinned: Option<PathBuf>, url: &str, now: u64, out: &mut dyn Write) -> i32 {
    let url = match engine_url(url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("tempo-launch: link: {e}");
            return 2;
        }
    };
    let path = pinned
        .clone()
        .unwrap_or_else(|| agent_settings_path(&ctx.agent_data));
    let (cfg, old) = match load_settings(&path) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("tempo-launch: link: {e}");
            return 1;
        }
    };
    let mut list = cfg.strs("ollama_fallbacks");
    if list.contains(&url) {
        let _ = writeln!(
            out,
            "{url} is already in ollama_fallbacks. Nothing changed."
        );
        return 0;
    }
    list.push(url.clone());
    let value = J::strings(&list);
    if pinned.is_some() {
        let _ = writeln!(
            out,
            "TEMPO_AGENT_CONFIG names {}, which tempo-launch leaves alone. Add this line to it:\n  \"ollama_fallbacks\": {}",
            path.display(),
            value.compact()
        );
        return 0;
    }
    match store(
        ctx,
        &path,
        cfg,
        old,
        &[("ollama_fallbacks".into(), value)],
        now,
    ) {
        Ok(trashed) => {
            let _ = writeln!(
                out,
                "Linked {url}: it is now in ollama_fallbacks in {}.",
                path.display()
            );
            if let Some(t) = trashed {
                let _ = writeln!(out, "The old settings were kept at {}.", t.display());
            }
            let _ = writeln!(
                out,
                "Tempo uses this computer's own Ollama first, and moves to the next address when one cannot be reached. Nothing was contacted. Restart the agent (tempo-launch stop, then tempo-launch start) to use it."
            );
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: link: {e}");
            1
        }
    }
}

/// `tempo-launch setup --apply-proposal`: show what the proposal would change, ask, and only on
/// a yes write it.
pub fn apply_proposal(
    ctx: &Ctx,
    pinned: Option<PathBuf>,
    now: u64,
    out: &mut dyn Write,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> i32 {
    let file = ctx.home.join(PROPOSAL_FILE);
    let Ok(text) = std::fs::read_to_string(&file) else {
        let _ = writeln!(out, "No proposal is waiting. (Your AI writes one with the setup_propose tool of tempo-launch mcp.)");
        return 1;
    };
    let doc = match parse(&text) {
        Ok(d @ J::Obj(_)) => d,
        _ => {
            eprintln!("tempo-launch: the proposal is not valid JSON; it was left alone");
            return 1;
        }
    };
    let proposed = match doc.get("settings").map(check_settings) {
        Some(Ok(p)) => p,
        Some(Err(e)) => {
            eprintln!("tempo-launch: the proposal was refused: {e}");
            return 1;
        }
        None => {
            eprintln!("tempo-launch: the proposal holds no settings");
            return 1;
        }
    };
    let path = pinned
        .clone()
        .unwrap_or_else(|| agent_settings_path(&ctx.agent_data));
    let (cfg, old) = match load_settings(&path) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            return 1;
        }
    };
    let note = doc.str("note").map(clean_note).unwrap_or_default();
    let _ = writeln!(
        out,
        "Your AI proposed changes to {}.{}\n",
        path.display(),
        if note.is_empty() {
            String::new()
        } else {
            format!("\nIts note (written by the AI, not by Tempo): {note}")
        }
    );
    let mut changes: Vec<(String, J)> = Vec::new();
    for (k, v) in &proposed {
        match cfg.get(k) {
            Some(cur) if cur == v => {
                let _ = writeln!(out, "  {k}: unchanged ({})", short(v));
            }
            Some(cur) => {
                let _ = writeln!(out, "  - {k}: {}", short(cur));
                let _ = writeln!(out, "  + {k}: {}", short(v));
                changes.push((k.clone(), v.clone()));
            }
            None => {
                let _ = writeln!(out, "  + {k}: {}", short(v));
                changes.push((k.clone(), v.clone()));
            }
        }
    }
    if changes.is_empty() {
        let _ = writeln!(out, "\nNothing would change.");
        return 0;
    }
    if pinned.is_some() {
        let _ = writeln!(
            out,
            "\nTEMPO_AGENT_CONFIG names that file, which tempo-launch leaves alone. Add the + lines to it yourself."
        );
        return 0;
    }
    if !confirm("\nApply these changes? [y/N] ") {
        let _ = writeln!(
            out,
            "Nothing changed. The proposal stays at {} until you apply or remove it.",
            file.display()
        );
        return 0;
    }
    match store(ctx, &path, cfg, old, &changes, now) {
        Ok(trashed) => {
            let _ = writeln!(out, "Applied to {}.", path.display());
            if let Some(t) = trashed {
                let _ = writeln!(out, "The old settings were kept at {}.", t.display());
            }
            // the proposal is done: it moves aside, never deleted
            let done = ctx
                .home
                .join("setup")
                .join(format!("proposal-applied-{}.json", file_stamp(now)));
            if let Some(d) = done.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::rename(&file, &done);
            let _ = writeln!(
                out,
                "Restart the agent to use them: tempo-launch stop, then tempo-launch start."
            );
            0
        }
        Err(e) => {
            eprintln!("tempo-launch: {e}");
            1
        }
    }
}

/// The real file TEMPO_AGENT_CONFIG names, if any.
pub fn real_pinned() -> Option<PathBuf> {
    pinned_agent_settings()
}
