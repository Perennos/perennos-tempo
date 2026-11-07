//! `GET /agent/catalog`: everything the chat's slash menu lists, in one answer. The commands come
//! straight from `HELP_TEXT` and the tools from `builtin_schemas`, so a new one shows up without
//! anyone remembering to add it here.

use crate::agent::HELP_TEXT;
use crate::config::Config;
use crate::project;
use crate::tools::builtin_schemas;
use serde_json::{json, Value};
use std::path::Path;

/// Tools that ask first (new websites, overwrites, the trash, commands that change things).
const ASKS: &[&str] = &[
    "write_file",
    "trash",
    "run_command",
    "fetch_url",
    "edit_file",
    "apply_patch",
];

/// The house voice for each builtin; the schema's own words are the fallback.
fn say(name: &str) -> Option<&'static str> {
    Some(match name {
        "list_dir" => "See what is in a folder.",
        "read_file" => "Read a text file.",
        "write_file" => "Write a file. Overwriting asks first.",
        "edit_file" => "Change one exact piece of a file, and keep a backup.",
        "apply_patch" => {
            "Change several files in one go. All of it lands, or none. /undo puts it back."
        }
        "grep" => "Search inside files for a word or pattern.",
        "glob" => "Find files by name, newest first.",
        "run_command" => "Run a command in the folder. Anything that changes things asks first.",
        "command_output" => "Look in on a command running in the background.",
        "command_stop" => "End a background command.",
        "trash" => "Move a file or folder to the trash. Nothing is ever deleted.",
        "fetch_url" => "Read a web page. Asks before a new website.",
        "search_memory" => "Look through your notes and memory, with sources.",
        "workflow_update" => "Keep the task list for a job with several steps.",
        "delegate" => "Hand a small read-only job to another machine.",
        "comfy_image" => "Make an image with ComfyUI on this computer.",
        _ => return None,
    })
}

/// "/cd <folder>       work in..." becomes name, args hint and help.
fn parse_help(text: &str) -> Vec<Value> {
    let mut out = vec![];
    for line in text.lines() {
        let Some(first) = line.chars().next() else {
            continue;
        };
        if !matches!(first, '/' | '!' | '@') {
            continue;
        }
        // name, then an args hint (<folder>, [name]), then the help
        let mut words = line.split_whitespace().peekable();
        let name = words.next().unwrap_or("");
        let mut args = vec![];
        while let Some(w) = words.next_if(|w| w.starts_with(['<', '['])) {
            args.push(w);
        }
        let help = words.collect::<Vec<_>>().join(" ");
        let args = args.join(" ");
        // `!<command>` is typed into a message, not picked as a command
        let (name, args) = match name.find('<') {
            Some(i) if first == '!' => (&name[..i], name[i..].to_string()),
            _ => (name, args),
        };
        let kind = if first == '/' { "command" } else { "syntax" };
        out.push(json!({"name": name, "args": args, "help": help, "kind": kind}));
    }
    // /help answers on its own and /review is /soundcheck by another name; neither has a line
    out.push(json!({"name": "/help", "args": "", "help": "list the commands, and your own", "kind": "command"}));
    out.push(json!({"name": "/review", "args": "[what]", "help": "the same as /soundcheck", "kind": "command"}));
    out
}

fn first_sentence(s: &str) -> String {
    let end = s.find(". ").map(|i| i + 1).unwrap_or(s.len());
    s[..end].to_string()
}

fn builtins() -> Vec<Value> {
    builtin_schemas()
        .iter()
        .filter_map(|t| {
            let f = &t["function"];
            let name = f["name"].as_str()?;
            let desc = say(name)
                .map(str::to_string)
                .unwrap_or_else(|| first_sentence(f["description"].as_str().unwrap_or("")));
            let args: Vec<&str> = f["parameters"]["properties"]
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let need: Vec<&str> = f["parameters"]["required"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            Some(json!({"name": name, "description": desc, "approval": ASKS.contains(&name), "args": args, "required": need}))
        })
        .collect()
}

/// The bands the Conductor saved: `<home>/bands/<name>/band.json`. Read loosely, since the
/// Conductor already checked the file when it wrote it.
fn bands(home: &Path) -> Vec<Value> {
    let Ok(rd) = std::fs::read_dir(home.join("bands")) else {
        return vec![];
    };
    let mut out: Vec<Value> = rd
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("band.json")).ok()?;
            let j: Value = serde_json::from_str(&text).ok()?;
            let pick = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
            let flows: Vec<Value> = j["flows"]
                .as_array()?
                .iter()
                .map(|f| json!({"name": pick(f, "name"), "goal": pick(f, "goal"), "every": pick(f, "every")}))
                .collect();
            let members: Vec<Value> = j["members"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|m| json!({"handle": pick(m, "handle"), "name": pick(m, "name"), "role": pick(m, "role")}))
                        .collect()
                })
                .unwrap_or_default();
            Some(json!({"name": pick(&j, "name"), "about": pick(&j, "about"), "flows": flows, "members": members}))
        })
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

/// Is ComfyUI answering? A quick knock on its port, not a request.
fn comfy_up(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let port = uri.port_u16().unwrap_or(80);
    (host, port).to_socket_addrs().is_ok_and(|mut a| {
        a.any(|sa| {
            std::net::TcpStream::connect_timeout(&sa, std::time::Duration::from_millis(250)).is_ok()
        })
    })
}

/// `mcp`: the tools the connected servers offer, as `{server, tool, name, description, approval}`.
pub fn build(cfg: &Config, home: &Path, mcp: &[Value]) -> Value {
    let own: Vec<Value> = project::list_commands(&cfg.command_cwd, Some(home))
        .into_iter()
        .map(|(n, d)| json!({"name": format!("/{n}"), "description": d}))
        .collect();
    let fleet = cfg.all_hosts().len() > 1
        || mcp
            .iter()
            .any(|t| t["server"].as_str().is_some_and(|s| s.contains("fleet")));
    json!({
        "commands": parse_help(HELP_TEXT),
        "tools": builtins(),
        "mcp": mcp,
        "custom": own,
        "bands": bands(home),
        "comfy": comfy_up(&cfg.comfy_url),
        "fleet": fleet,
        "folder": cfg.command_cwd.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_help_line_and_every_builtin_is_listed() {
        let d = std::env::temp_dir().join(format!("tempo-catalog-{}", std::process::id()));
        std::fs::create_dir_all(d.join("bands/folk")).unwrap();
        std::fs::write(
            d.join("bands/folk/band.json"),
            r#"{"name":"folk","about":"a","members":[{"handle":"ann","name":"Ann","role":"r"}],"flows":[{"name":"weekly-plan","goal":"g","every":"manual"}]}"#,
        )
        .unwrap();
        let cfg = Config::defaults(Some(d.to_string_lossy().into_owned()));
        let v = build(
            &cfg,
            &d,
            &[
                json!({"server": "s", "tool": "t", "name": "s__t", "description": "d", "approval": true}),
            ],
        );
        let names: Vec<&str> = v["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        for line in HELP_TEXT.lines().filter(|l| l.starts_with(['/', '!', '@'])) {
            let want = line.split_whitespace().next().unwrap();
            let want = if want.starts_with('!') { "!" } else { want };
            assert!(
                names.contains(&want),
                "{want} is in HELP_TEXT but not the catalog: {names:?}"
            );
        }
        for c in v["commands"].as_array().unwrap() {
            assert!(!c["help"].as_str().unwrap().is_empty(), "{c}");
        }
        let tools: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for t in builtin_schemas() {
            let n = t["function"]["name"].as_str().unwrap();
            assert!(tools.contains(&n), "{n} missing");
        }
        let ask = |n: &str| {
            v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == n)
                .unwrap()["approval"]
                .clone()
        };
        assert_eq!(ask("trash"), true);
        assert_eq!(ask("grep"), false);
        assert_eq!(v["mcp"][0]["name"], "s__t");
        assert_eq!(v["bands"][0]["flows"][0]["name"], "weekly-plan");
        assert_eq!(v["bands"][0]["members"][0]["handle"], "ann");
        let _ = std::fs::remove_dir_all(d);
    }
}
