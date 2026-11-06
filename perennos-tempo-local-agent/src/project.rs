//! What the agent knows about the folder it works in, as Claude Code (`CLAUDE.md`) and Codex
//! (`AGENTS.md`) know it:
//!
//! - **Instructions**: `AGENTS.md`, `TEMPO.md` and `CLAUDE.md` from the working folder up to the
//!   top of the project (the folder holding `.git`, or the home folder), nearer files last so they
//!   win; plus the owner's own `<home>/AGENTS.md`. A line `@other.md` brings that file in (one
//!   level, inside the project). Redacted and capped.
//! - **Mentions**: `@path/to/file` in a message attaches that file (capped), as `@` does there.
//! - **Commands**: `/name args` runs `<project>/.tempo/commands/name.md` or
//!   `<home>/commands/name.md`, with `$ARGUMENTS` and `$1`..`$9` filled in.
use crate::util::{self, head_chars, is_secret_path};
use std::path::{Path, PathBuf};

pub const INSTRUCTION_FILES: [&str; 3] = ["AGENTS.md", "TEMPO.md", "CLAUDE.md"];
/// All instructions together are cut to this many characters (nearest files are kept whole
/// first): about 2,000 tokens, room a local model can spare every turn.
pub const MAX_INSTRUCTIONS: usize = 8_000;
pub const MAX_MENTION: usize = 10_000;
const MAX_MENTIONS: usize = 6;

/// The top of the project that holds `dir`: the nearest folder with `.git`, else `dir` itself.
pub fn project_root(dir: &Path) -> PathBuf {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        cur = d.parent();
    }
    dir.to_path_buf()
}

fn read_capped(p: &Path, cap: usize) -> Option<String> {
    if is_secret_path(&p.to_string_lossy()) {
        return None;
    }
    let md = std::fs::metadata(p).ok()?;
    if !md.is_file() || md.len() > 4 << 20 {
        return None;
    }
    let b = std::fs::read(p).ok()?;
    Some(util::redact(&head_chars(&String::from_utf8_lossy(&b), cap)))
}

/// Lines `@file.md` in an instruction file, inside `root` (one level; no secrets).
fn with_imports(text: &str, base: &Path, root: &Path) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rel) = t
            .strip_prefix('@')
            .filter(|r| !r.is_empty() && !r.contains(char::is_whitespace))
        {
            let p = util::resolve(&base.join(rel));
            if p.starts_with(util::resolve(root)) {
                if let Some(inner) = read_capped(&p, 6000) {
                    out.push_str(&format!("(from {rel})\n{inner}\n"));
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The instructions for work in `dir`, with where each came from, nearest last. Empty when none.
pub fn instructions(dir: &Path, home: Option<&Path>) -> String {
    gather(dir, home)
        .into_iter()
        .map(|(p, t)| {
            format!(
                "--- {} ---\n{}",
                p.display(),
                head_chars(t.trim(), MAX_INSTRUCTIONS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The instruction files that apply to `dir` (for `/cd` and `/status`).
pub fn instruction_files(dir: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    gather(dir, home).into_iter().map(|(p, _)| p).collect()
}

fn gather(dir: &Path, home: Option<&Path>) -> Vec<(PathBuf, String)> {
    let dir = util::resolve(dir);
    let root = project_root(&dir);
    let mut found: Vec<(PathBuf, String)> = vec![];
    if let Some(h) = home {
        if let Some(t) = read_capped(&h.join("AGENTS.md"), MAX_INSTRUCTIONS) {
            found.push((h.join("AGENTS.md"), with_imports(&t, h, h)));
        }
    }
    // from the top of the project down to the working folder
    let mut chain: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|a| a.starts_with(&root))
        .map(Path::to_path_buf)
        .collect();
    chain.reverse();
    for d in chain {
        for name in INSTRUCTION_FILES {
            let p = d.join(name);
            if found.iter().any(|(q, _)| *q == p) {
                continue;
            }
            if let Some(t) = read_capped(&p, MAX_INSTRUCTIONS) {
                found.push((p, with_imports(&t, &d, &root)));
            }
        }
    }
    // keep the nearest whole when the total is too long: drop from the far end
    while found.iter().map(|(_, t)| t.len()).sum::<usize>() > MAX_INSTRUCTIONS && found.len() > 1 {
        found.remove(0);
    }
    found
}

/// `@path` mentions in a message: the files they name (inside `dir` or absolute), attached after
/// the message. Mentions that name no file are left as they are (an email address, a handle).
pub fn attach_mentions(text: &str, dir: &Path) -> (String, Vec<String>) {
    let mut seen: Vec<String> = vec![];
    let mut out = String::new();
    for word in text.split_whitespace() {
        let Some(rel) = word.strip_prefix('@') else {
            continue;
        };
        let rel = rel.trim_end_matches([',', '.', ';', ':', ')', '!', '?']);
        if rel.is_empty() || seen.iter().any(|s| s == rel) || seen.len() >= MAX_MENTIONS {
            continue;
        }
        let p = if Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            dir.join(rel)
        };
        if let Some(body) = read_capped(&p, MAX_MENTION) {
            out.push_str(&format!("\n\n[file {rel}]\n```\n{body}\n```"));
            seen.push(rel.to_string());
        }
    }
    (format!("{text}{out}"), seen)
}

/// A custom command: its prompt with the arguments filled in, or None when there is no such file.
pub fn custom_command(name: &str, args: &str, dir: &Path, home: Option<&Path>) -> Option<String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let root = project_root(&util::resolve(dir));
    let mut places = vec![root.join(".tempo").join("commands")];
    if let Some(h) = home {
        places.push(h.join("commands"));
    }
    let body = places
        .iter()
        .find_map(|d| read_capped(&d.join(format!("{name}.md")), 20_000))?;
    // a front-matter block (description: ...) is for the list, not the prompt
    let body = match body
        .strip_prefix("---\n")
        .and_then(|r| r.split_once("\n---\n"))
    {
        Some((_, rest)) => rest.to_string(),
        None => body,
    };
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut p = body.replace("$ARGUMENTS", args.trim());
    for i in (1..=9).rev() {
        p = p.replace(&format!("${i}"), words.get(i - 1).copied().unwrap_or(""));
    }
    Some(p.trim().to_string())
}

/// Every custom command with its description, for `/help`.
pub fn list_commands(dir: &Path, home: Option<&Path>) -> Vec<(String, String)> {
    let root = project_root(&util::resolve(dir));
    let mut out: Vec<(String, String)> = vec![];
    let mut places = vec![root.join(".tempo").join("commands")];
    if let Some(h) = home {
        places.push(h.join("commands"));
    }
    for d in places {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            if out.iter().any(|(n, _)| n == stem) {
                continue;
            }
            let text = read_capped(&e.path(), 4000).unwrap_or_default();
            let desc = text
                .lines()
                .find_map(|l| l.strip_prefix("description:"))
                .map(|d| d.trim().to_string())
                .unwrap_or_else(|| {
                    head_chars(
                        text.lines()
                            .find(|l| !l.trim().is_empty() && *l != "---")
                            .unwrap_or(""),
                        80,
                    )
                });
            out.push((stem.to_string(), desc));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::tmpdir;

    #[test]
    fn instructions_come_from_the_project_down_to_the_folder() {
        let d = tmpdir("proj-instr");
        let repo = d.join("repo");
        let sub = repo.join("crates").join("app");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(d.join("AGENTS.md"), "outside the project: never read").unwrap();
        std::fs::write(
            repo.join("AGENTS.md"),
            "Use cargo test.\n@docs/style.md\n@../../secret.md",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/style.md"), "Short sentences.").unwrap();
        // the fake key is put together at run time, so no leak scanner ever matches this file
        let fake_key = format!("sk-{}", "abcdefghijklmnopqrstuvwxyz123456");
        std::fs::write(
            sub.join("CLAUDE.md"),
            format!("This crate is the app. api_key={fake_key}"),
        )
        .unwrap();
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("AGENTS.md"), "Alex prefers plain words.").unwrap();
        let t = instructions(&sub, Some(&home));
        let (a, b, c) = (
            t.find("plain words").unwrap(),
            t.find("cargo test").unwrap(),
            t.find("This crate").unwrap(),
        );
        assert!(
            a < b && b < c,
            "owner first, then the top of the project, then nearer: {t}"
        );
        assert!(t.contains("(from docs/style.md)\nShort sentences."), "{t}");
        assert!(
            t.contains("@../../secret.md"),
            "an import outside the project stays a plain line: {t}"
        );
        assert!(
            !t.contains("never read"),
            "above the project is not read: {t}"
        );
        assert!(!t.contains(&fake_key), "redacted: {t}");
        assert!(instructions(&tmpdir("proj-none"), None).is_empty());
    }

    #[test]
    fn mentions_attach_files_and_leave_the_rest() {
        let d = tmpdir("proj-mention");
        std::fs::write(d.join("notes.md"), "line one\nline two").unwrap();
        std::fs::write(d.join(".env"), "SECRET=1").unwrap();
        let (t, files) = attach_mentions("Fix @notes.md, mail @alex and look at @.env", &d);
        assert_eq!(files, ["notes.md"]);
        assert!(t.starts_with("Fix @notes.md, mail @alex and look at @.env\n\n[file notes.md]\n```\nline one\nline two\n```"), "{t}");
        assert!(!t.contains("SECRET"), "secret files are never attached");
    }

    #[test]
    fn custom_commands_fill_in_their_arguments() {
        let d = tmpdir("proj-cmd");
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::create_dir_all(d.join(".tempo/commands")).unwrap();
        std::fs::write(
            d.join(".tempo/commands/review.md"),
            "---\ndescription: review a file\n---\nReview $1 for $ARGUMENTS.",
        )
        .unwrap();
        let home = d.join("h");
        std::fs::create_dir_all(home.join("commands")).unwrap();
        std::fs::write(
            home.join("commands/standup.md"),
            "Write my standup from today's notes.",
        )
        .unwrap();
        assert_eq!(
            custom_command("review", "main.rs bugs", &d, Some(&home)).unwrap(),
            "Review main.rs for main.rs bugs."
        );
        assert_eq!(
            custom_command("standup", "", &d, Some(&home)).unwrap(),
            "Write my standup from today's notes."
        );
        assert!(custom_command("../etc", "", &d, Some(&home)).is_none());
        assert!(custom_command("nope", "", &d, Some(&home)).is_none());
        let l = list_commands(&d, Some(&home));
        assert_eq!(
            l,
            [
                ("review".to_string(), "review a file".to_string()),
                (
                    "standup".to_string(),
                    "Write my standup from today's notes.".to_string()
                )
            ]
        );
    }
}
