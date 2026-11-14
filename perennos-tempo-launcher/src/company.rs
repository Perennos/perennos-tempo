//! First-run company membership. Discoveries stay pending until the owner approves a node.

use crate::setup::json::{parse, J};
use crate::{write_atomic, Ctx};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FILE: &str = "company.json";
const LIMIT: u64 = 1 << 20;
const CLOUD_STATUS_TIMEOUT: Duration = Duration::from_secs(3);
const CLOUD_OUTPUT_LIMIT: usize = 4096;

fn regular_file(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err("company.json must be a regular file; it was kept unchanged".into())
        }
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("company.json metadata: {e}")),
    }
}

fn person(id: &str, name: &str, kind: &str, role: &str) -> J {
    J::Obj(vec![
        ("id".into(), J::s(id)),
        ("name".into(), J::s(name)),
        ("kind".into(), J::s(kind)),
        ("role".into(), J::s(role)),
        ("status".into(), J::s("active")),
    ])
}

pub fn initial() -> J {
    let ceo = person("ceo", "You", "person", "CEO");
    let manager_name = format!("{} - Conductor", human_name("manager:conductor"));
    let manager = person("manager:conductor", &manager_name, "agent", "Manager desk");
    J::Obj(vec![
        ("schema_version".into(), J::Num("1".into())),
        (
            "org".into(),
            J::Obj(vec![
                ("ceo".into(), ceo.clone()),
                ("manager".into(), manager.clone()),
                ("peers".into(), J::Arr(vec![])),
            ]),
        ),
        ("employees".into(), J::Arr(vec![ceo, manager])),
        ("departments".into(), J::Arr(vec![])),
        ("nodes".into(), J::Arr(vec![])),
    ])
}

pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

pub fn ensure(home: &Path) -> Result<J, String> {
    let path = path(home);
    let Some(meta) = regular_file(&path)? else {
        let doc = initial();
        write_atomic(&path, doc.pretty().as_bytes()).map_err(|e| e.to_string())?;
        return Ok(doc);
    };
    if meta.len() > LIMIT {
        return Err("company.json exceeds the 1 MiB limit; it was kept unchanged".into());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("company.json read: {e}"))?;
    let mut doc =
        parse(&text).map_err(|_| "company.json is malformed; it was kept unchanged".to_string())?;
    if doc.get("nodes").is_none() {
        doc.set("nodes", J::Arr(vec![]));
        save(home, &doc)?;
    }
    validate(&doc)?;
    Ok(doc)
}

/// Read the company view without creating or migrating profile files.
pub fn read_only(home: &Path) -> Result<J, String> {
    let path = path(home);
    let Some(meta) = regular_file(&path)? else {
        return Ok(initial());
    };
    if meta.len() > LIMIT {
        return Err("company.json exceeds the 1 MiB limit; it was kept unchanged".into());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("company.json read: {e}"))?;
    let mut doc =
        parse(&text).map_err(|_| "company.json is malformed; it was kept unchanged".to_string())?;
    if doc.get("nodes").is_none() {
        doc.set("nodes", J::Arr(vec![]));
    }
    validate(&doc)?;
    Ok(doc)
}

pub fn save(home: &Path, doc: &J) -> Result<(), String> {
    validate(doc)?;
    let path = path(home);
    regular_file(&path)?;
    let bytes = doc.pretty();
    if bytes.len() as u64 > LIMIT {
        return Err("company.json exceeds the 1 MiB limit".into());
    }
    write_atomic(&path, bytes.as_bytes()).map_err(|e| format!("company.json write: {e}"))
}

fn safe_tree(value: &J, depth: usize) -> bool {
    if depth > 20 {
        return false;
    }
    match value {
        J::Obj(fields) => fields.iter().all(|(k, v)| {
            let key = k.to_ascii_lowercase();
            ![
                "token",
                "secret",
                "password",
                "credential",
                "api_key",
                "api-key",
                "authorization",
            ]
            .iter()
            .any(|part| key.contains(part))
                && safe_tree(v, depth + 1)
        }),
        J::Arr(items) => items.iter().all(|v| safe_tree(v, depth + 1)),
        J::Str(s) => {
            let lower = s.to_ascii_lowercase();
            s.len() <= 256
                && !s.chars().any(char::is_control)
                && ![
                    "sk-", "sk_", "ghp_", "gho_", "aiza", "eyj", "bearer ", "api_key=", "token=",
                    "secret=",
                ]
                .iter()
                .any(|needle| lower.contains(needle))
        }
        _ => true,
    }
}

pub fn validate(doc: &J) -> Result<(), String> {
    let org = doc.get("org").ok_or("company.json needs org")?;
    if !matches!(org.get("ceo"), Some(J::Obj(_)))
        || !matches!(org.get("manager"), Some(J::Obj(_)))
        || !matches!(org.get("peers"), Some(J::Arr(_)))
        || !matches!(doc.get("employees"), Some(J::Arr(_)))
        || !matches!(doc.get("departments"), Some(J::Arr(_)))
        || !matches!(doc.get("nodes"), Some(J::Arr(_)))
        || !safe_tree(doc, 0)
    {
        return Err(
            "company.json has an invalid org, roster, department list, or sensitive field".into(),
        );
    }
    Ok(())
}

pub fn human_name(id: &str) -> String {
    const NAMES: &[&str] = &["Ari", "Mira", "Nico", "Sage", "Tavi", "Vale", "Rae", "Kian"];
    let hash = id.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    });
    format!(
        "{} {:04}",
        NAMES[(hash as usize) % NAMES.len()],
        hash % 10_000
    )
}

fn add_peer(doc: &mut J, peer: J) {
    if let J::Obj(root) = doc {
        let employees = root
            .iter_mut()
            .find(|(k, _)| k == "employees")
            .map(|(_, v)| v);
        if let Some(J::Arr(items)) = employees {
            let id = peer.str("id").unwrap_or("").to_string();
            if let Some(old) = items.iter_mut().find(|e| e.str("id") == Some(&id)) {
                *old = peer.clone();
            } else {
                items.push(peer.clone());
            }
        }
        if let Some(J::Obj(org)) = root.iter_mut().find(|(k, _)| k == "org").map(|(_, v)| v) {
            if let Some(J::Arr(peers)) = org.iter_mut().find(|(k, _)| k == "peers").map(|(_, v)| v)
            {
                let id = peer.str("id").unwrap_or("").to_string();
                if let Some(old) = peers.iter_mut().find(|e| e.str("id") == Some(&id)) {
                    *old = peer;
                } else {
                    peers.push(peer);
                }
            }
        }
    }
}

fn add_node(doc: &mut J, node: J) {
    let id = node.str("id").unwrap_or("").to_string();
    if let J::Obj(root) = doc {
        if let Some(J::Arr(nodes)) = root
            .iter_mut()
            .find(|(key, _)| key == "nodes")
            .map(|(_, value)| value)
        {
            if let Some(old) = nodes.iter_mut().find(|old| old.str("id") == Some(&id)) {
                *old = node;
            } else {
                nodes.push(node);
            }
        }
    }
}

/// One owner action enrolls one node and its read-only findings; no finding is active before this call.
pub fn approve_node(
    home: &Path,
    node_id: &str,
    node_name: &str,
    engines: &[J],
) -> Result<J, String> {
    if node_id.is_empty() || node_id.len() > 128 || !safe_label(node_name) {
        return Err("invalid node identity".into());
    }
    let mut doc = ensure(home)?;
    let mut node = person(&format!("node:{node_id}"), node_name, "node", "Node");
    node.set("node_id", J::s(node_id));
    add_peer(&mut doc, node.clone());
    add_node(&mut doc, node);
    for e in engines {
        let id = e.str("id").ok_or("engine has no stable id")?;
        let name = e.str("name").ok_or("engine has no display name")?;
        let kind = e.str("kind").ok_or("engine has no kind")?;
        if !id.starts_with(&format!("{node_id}:"))
            || !safe_label(id)
            || !safe_label(name)
            || !safe_label(kind)
        {
            return Err("engine identity does not belong to the approved node".into());
        }
        let lower_kind = kind.to_ascii_lowercase();
        let is_agent = lower_kind.contains("agent") || lower_kind.contains("lane");
        let member_kind = if is_agent { "agent" } else { "engine" };
        let member_id = format!("{member_kind}:{id}");
        let mut item = person(&member_id, &human_name(&member_id), member_kind, kind);
        item.set("source_name", J::s(name));
        item.set("node_id", J::s(node_id));
        if let Some(status) = e.str("status") {
            item.set("status", J::s(status));
        }
        add_peer(&mut doc, item);
    }
    save(home, &doc)?;
    Ok(doc)
}

pub fn safe_label(s: &str) -> bool {
    let lower = s.trim().to_ascii_lowercase();
    !lower.is_empty()
        && s.len() <= 64
        && ![
            "sk-", "sk_", "ghp_", "gho_", "aiza", "eyj", "bearer ", "api_key", "api-key", "token=",
        ]
        .iter()
        .any(|p| lower.contains(p))
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " ._-'/:()".contains(c))
}

pub fn rename_member(home: &Path, id: &str, name: &str) -> Result<J, String> {
    if !safe_label(name) {
        return Err("Use a short display name without keys or control characters.".into());
    }
    let mut doc = ensure(home)?;
    let mut found = false;
    if let J::Obj(root) = &mut doc {
        if let Some(J::Arr(items)) = root
            .iter_mut()
            .find(|(k, _)| k == "employees")
            .map(|(_, v)| v)
        {
            for item in items {
                if item.str("id") == Some(id) {
                    item.set("name", J::s(name));
                    found = true;
                }
            }
        }
        if let Some(J::Obj(org)) = root.iter_mut().find(|(k, _)| k == "org").map(|(_, v)| v) {
            for (_, value) in org {
                if value.str("id") == Some(id) {
                    value.set("name", J::s(name));
                }
                if let J::Arr(peers) = value {
                    for peer in peers {
                        if peer.str("id") == Some(id) {
                            peer.set("name", J::s(name));
                        }
                    }
                }
            }
        }
        if let Some(J::Arr(nodes)) = root.iter_mut().find(|(k, _)| k == "nodes").map(|(_, v)| v) {
            for node in nodes {
                if node.str("id") == Some(id) {
                    node.set("name", J::s(name));
                }
            }
        }
    }
    if !found {
        return Err("That employee is no longer in the org chart.".into());
    }
    save(home, &doc)?;
    Ok(doc)
}

pub fn add_department(home: &Path, name: &str) -> Result<J, String> {
    if !safe_label(name) {
        return Err("Use a short department name.".into());
    }
    let mut doc = ensure(home)?;
    let id = format!("dept:{}", name.to_ascii_lowercase().replace(' ', "-"));
    if doc
        .arr("departments")
        .iter()
        .any(|d| d.str("id") == Some(id.as_str()))
    {
        return Ok(doc);
    }
    if let J::Obj(root) = &mut doc {
        if let Some(J::Arr(items)) = root
            .iter_mut()
            .find(|(k, _)| k == "departments")
            .map(|(_, v)| v)
        {
            items.push(J::Obj(vec![
                ("id".into(), J::s(&id)),
                ("name".into(), J::s(name)),
            ]));
        }
    }
    save(home, &doc)?;
    Ok(doc)
}

pub fn reorder_peer(home: &Path, id: &str, direction: &str) -> Result<J, String> {
    let mut doc = ensure(home)?;
    let peers = doc
        .get("org")
        .and_then(|o| o.get("peers"))
        .and_then(|p| match p {
            J::Arr(_) => Some(()),
            _ => None,
        });
    if peers.is_none() {
        return Err("The org chart has no peer list.".into());
    }
    if let J::Obj(root) = &mut doc {
        if let Some(J::Obj(org)) = root.iter_mut().find(|(k, _)| k == "org").map(|(_, v)| v) {
            if let Some(J::Arr(items)) = org.iter_mut().find(|(k, _)| k == "peers").map(|(_, v)| v)
            {
                let index = items
                    .iter()
                    .position(|p| p.str("id") == Some(id))
                    .ok_or("That peer is no longer in the org chart.")?;
                let target = match direction {
                    "up" => index.checked_sub(1),
                    "down" => (index + 1 < items.len()).then_some(index + 1),
                    _ => return Err("Choose up or down.".into()),
                };
                if let Some(target) = target {
                    items.swap(index, target);
                }
            }
        }
    }
    save(home, &doc)?;
    Ok(doc)
}

/// Remove an approved employee from membership; a later scan can enroll it again.
pub fn remove_member(home: &Path, id: &str) -> Result<J, String> {
    let mut doc = ensure(home)?;
    let member = doc
        .arr("employees")
        .iter()
        .find(|p| p.str("id") == Some(id))
        .ok_or("That employee is no longer in the org chart.")?;
    let kind = member.str("kind").unwrap_or("");
    if !matches!(kind, "node" | "engine" | "agent") {
        return Err("Only approved nodes and lanes can be removed.".into());
    }
    let node_id = (kind == "node").then(|| member.str("node_id").unwrap_or("").to_string());
    let mut removed = std::collections::HashSet::from([id.to_string()]);
    if let Some(node_id) = node_id.as_deref() {
        for item in doc.arr("employees") {
            if item.str("node_id") == Some(node_id) {
                if let Some(member_id) = item.str("id") {
                    removed.insert(member_id.to_string());
                }
            }
        }
    }
    if let J::Obj(root) = &mut doc {
        if let Some(J::Arr(items)) = root
            .iter_mut()
            .find(|(k, _)| k == "employees")
            .map(|(_, v)| v)
        {
            items.retain(|p| !removed.contains(p.str("id").unwrap_or("")));
        }
        if let Some(J::Obj(org)) = root.iter_mut().find(|(k, _)| k == "org").map(|(_, v)| v) {
            if let Some(J::Arr(peers)) = org.iter_mut().find(|(k, _)| k == "peers").map(|(_, v)| v)
            {
                peers.retain(|p| !removed.contains(p.str("id").unwrap_or("")));
            }
        }
        if let Some(J::Arr(nodes)) = root.iter_mut().find(|(k, _)| k == "nodes").map(|(_, v)| v) {
            nodes.retain(|node| !removed.contains(node.str("id").unwrap_or("")));
        }
    }
    save(home, &doc)?;
    Ok(doc)
}

fn node_candidate(id: &str, name: &str) -> J {
    J::Obj(vec![
        ("node_id".into(), J::s(id)),
        ("name".into(), J::s(name)),
        ("engines".into(), J::Arr(vec![])),
    ])
}

fn add_engine(nodes: &mut [J], node_id: &str, engine: J) {
    if let Some(J::Obj(fields)) = nodes.iter_mut().find(|n| n.str("node_id") == Some(node_id)) {
        if let Some(J::Arr(items)) = fields
            .iter_mut()
            .find(|(k, _)| k == "engines")
            .map(|(_, v)| v)
        {
            items.push(engine);
        }
    }
}

/// Probes only the configured, private Ollama endpoints and returns pending findings.
pub fn discover_specs(specs: &[crate::conductor::engines::Spec]) -> Vec<J> {
    let mut nodes = vec![node_candidate(
        "local",
        &format!("{} - This computer", human_name("local")),
    )];
    for engine in crate::conductor::engines::probe_all(specs)
        .into_iter()
        .filter(|e| e.up)
    {
        let node_id = if engine.id == crate::conductor::engines::LOCAL {
            "local".to_string()
        } else {
            engine.id.clone()
        };
        let node_name = if node_id == "local" {
            format!("{} - This computer", human_name(&node_id))
        } else {
            format!("{} - {}", human_name(&node_id), node_id)
        };
        if !nodes.iter().any(|n| n.str("node_id") == Some(&node_id)) {
            nodes.push(node_candidate(&node_id, &node_name));
        }
        for model in engine.models {
            add_engine(
                &mut nodes,
                &node_id,
                J::Obj(vec![
                    (
                        "id".into(),
                        J::s(&format!("{node_id}:ollama:{}", model.name)),
                    ),
                    ("name".into(), J::s(&model.name)),
                    ("kind".into(), J::s("Ollama model")),
                    ("status".into(), J::s("Found")),
                ]),
            );
        }
    }
    nodes
}

pub fn discover(ctx: &Ctx) -> Result<Vec<J>, String> {
    let mut specs = crate::conductor::engines::configured(ctx);
    let fleet = crate::fleet::store::load(&crate::fleet::store::Paths::of(ctx));
    for machine in fleet.machines.iter().filter(|m| m.enabled && m.engine()) {
        let url = machine.engine_url();
        if !specs.iter().any(|s| s.url == url) {
            specs.push(crate::conductor::engines::Spec {
                id: machine.name.clone(),
                url,
            });
        }
    }
    let mut nodes = discover_specs(&specs);
    for machine in fleet.machines.iter().filter(|m| m.enabled) {
        if !nodes
            .iter()
            .any(|n| n.str("node_id") == Some(&machine.name))
        {
            nodes.push(node_candidate(
                &machine.name,
                &format!("{} - {}", human_name(&machine.name), machine.name),
            ));
        }
    }
    let cfg_path = crate::setup::apply::pinned_agent_settings()
        .unwrap_or_else(|| crate::setup::apply::agent_settings_path(&ctx.agent_data));
    let cfg = std::fs::read_to_string(cfg_path)
        .ok()
        .and_then(|s| parse(&s).ok());
    if let Some(cfg) = cfg.as_ref() {
        for model in cfg
            .strs("models")
            .into_iter()
            .chain(cfg.strs("fast_models"))
        {
            add_engine(
                &mut nodes,
                "local",
                J::Obj(vec![
                    ("id".into(), J::s(&format!("local:agent:{model}"))),
                    ("name".into(), J::s(&model)),
                    ("kind".into(), J::s("Agent lane")),
                    ("status".into(), J::s("Configured")),
                ]),
            );
        }
        if let Some(model) = cfg.str("embed_model").filter(|s| !s.is_empty()) {
            add_engine(
                &mut nodes,
                "local",
                J::Obj(vec![
                    ("id".into(), J::s(&format!("local:embedding:{model}"))),
                    ("name".into(), J::s(model)),
                    ("kind".into(), J::s("Embedding model")),
                    ("status".into(), J::s("Configured")),
                ]),
            );
        }
        for (i, _server) in cfg.strs("llama_servers").iter().enumerate() {
            add_engine(
                &mut nodes,
                "local",
                J::Obj(vec![
                    ("id".into(), J::s(&format!("local:llama-server:{i}"))),
                    ("name".into(), J::s(&format!("llama-server {}", i + 1))),
                    ("kind".into(), J::s("llama-server")),
                    ("status".into(), J::s("Configured")),
                ]),
            );
        }
    }
    let bands_dir = crate::conductor::bands_dir(ctx);
    if std::fs::symlink_metadata(&bands_dir)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir())
    {
        for entry in std::fs::read_dir(&bands_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(band) = crate::conductor::load(ctx, &name) else {
                continue;
            };
            for member in band.members {
                let id = format!("local:conductor:{}:{}", band.name, member.handle);
                if safe_label(&id) && safe_label(&member.name) {
                    add_engine(
                        &mut nodes,
                        "local",
                        J::Obj(vec![
                            ("id".into(), J::s(&id)),
                            ("name".into(), J::s(&member.name)),
                            ("kind".into(), J::s("Conductor agent lane")),
                            ("status".into(), J::s("Configured")),
                        ]),
                    );
                }
            }
        }
    }
    let voice = crate::setup::voice::read_record(&ctx.home);
    if !voice.model_id.is_empty() {
        add_engine(
            &mut nodes,
            "local",
            J::Obj(vec![
                (
                    "id".into(),
                    J::s(&format!("local:voice:{}", voice.model_id)),
                ),
                ("name".into(), J::s(&voice.model_id)),
                ("kind".into(), J::s("Voice model")),
                ("status".into(), J::s("Found")),
            ]),
        );
    }
    Ok(nodes)
}

fn status_command(executable: &Path, args: &[&str]) -> Option<(i32, String)> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::spawn_hidden(&mut command).ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let reader = std::thread::spawn(move || {
        let mut captured = Vec::with_capacity(CLOUD_OUTPUT_LIMIT);
        let mut buffer = [0; 512];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    let remaining = CLOUD_OUTPUT_LIMIT.saturating_sub(captured.len());
                    captured.extend_from_slice(&buffer[..size.min(remaining)]);
                }
            }
        }
        String::from_utf8_lossy(&captured).into_owned()
    });
    let deadline = Instant::now() + CLOUD_STATUS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().ok()?;
    Some((status?.code().unwrap_or(-1), output))
}

fn codex_logged_in(output: &str) -> Option<bool> {
    let text = output.to_ascii_lowercase();
    if text.contains("not logged in") || text.contains("no active login") {
        Some(false)
    } else if text.contains("logged in") {
        Some(true)
    } else {
        None
    }
}

fn claude_logged_in(output: &str) -> Option<bool> {
    parse(output).ok()?.bool("loggedIn")
}

fn cloud_account(id: &str, name: &str, status: &str, source: &str, connect_url: &str) -> J {
    J::Obj(vec![
        ("id".into(), J::s(id)),
        ("name".into(), J::s(name)),
        ("kind".into(), J::s("Cloud lane")),
        ("status".into(), J::s(status)),
        ("source".into(), J::s(source)),
        ("connect_url".into(), J::s(connect_url)),
    ])
}

/// Check only the official, local status commands found beside the launcher or in TEMPO_BIN.
/// Output is bounded, discarded after parsing, and never written to the profile or a log.
pub fn discover_cloud(ctx: &Ctx) -> (Vec<J>, Option<J>) {
    let codex = ctx.find("codex").map_or_else(
        || ("Not detected", "No configured Codex CLI"),
        |exe| match status_command(&exe, &["login", "status"])
            .and_then(|(_, out)| codex_logged_in(&out))
        {
            Some(true) => ("Connected", "Codex CLI status"),
            Some(false) => ("Not connected", "Codex CLI status"),
            None => ("Status unavailable", "Codex CLI status"),
        },
    );
    let claude = ctx.find("claude").map_or_else(
        || ("Not detected", "No configured Claude Code CLI"),
        |exe| match status_command(&exe, &["auth", "status"]) {
            Some((0, output)) if claude_logged_in(&output) == Some(true) => {
                ("Connected", "Claude Code auth status")
            }
            Some((1, output)) if claude_logged_in(&output) == Some(false) => {
                ("Not connected", "Claude Code auth status")
            }
            _ => ("Status unavailable", "Claude Code auth status"),
        },
    );
    let accounts = vec![
        cloud_account(
            "codex",
            "OpenAI Codex",
            codex.0,
            codex.1,
            "https://developers.openai.com/codex/auth",
        ),
        cloud_account(
            "claude",
            "Anthropic Claude Code",
            claude.0,
            claude.1,
            "https://code.claude.com/docs/en/cli-usage",
        ),
        cloud_account(
            "gemini",
            "Google Gemini CLI",
            "Status unavailable",
            "No supported tokenless status command",
            "https://geminicli.com/docs/get-started/authentication/",
        ),
    ];
    let connected: Vec<J> = accounts
        .iter()
        .filter(|account| account.str("status") == Some("Connected"))
        .map(|account| {
            J::Obj(vec![
                (
                    "id".into(),
                    J::s(&format!(
                        "cloud:{}",
                        account.str("id").unwrap_or("provider")
                    )),
                ),
                (
                    "name".into(),
                    J::s(account.str("name").unwrap_or("Cloud lane")),
                ),
                ("kind".into(), J::s("Cloud lane")),
                (
                    "status".into(),
                    J::s(&format!(
                        "Connected · {}",
                        account.str("source").unwrap_or("")
                    )),
                ),
            ])
        })
        .collect();
    let node = (!connected.is_empty()).then(|| {
        J::Obj(vec![
            ("node_id".into(), J::s("cloud")),
            ("name".into(), J::s("Cloud services")),
            ("engines".into(), J::Arr(connected)),
        ])
    });
    (accounts, node)
}

/// Recent completed Conductor steps for the signed-in profile; note contents and paths stay local.
pub fn activity(ctx: &Ctx, doc: &J) -> (J, J) {
    let month = crate::stamp(crate::unix_now());
    let month = month.get(..7).unwrap_or("");
    let employees: Vec<(String, String)> = doc
        .arr("employees")
        .iter()
        .filter(|employee| employee.str("kind") == Some("agent"))
        .filter_map(|employee| {
            Some((
                employee.str("id")?.to_string(),
                employee.str("name")?.to_string(),
            ))
        })
        .collect();
    let mut scores = std::collections::HashMap::<String, usize>::new();
    let mut events = Vec::<(String, J)>::new();
    let bands_dir = crate::conductor::bands_dir(ctx);
    let Ok(meta) = std::fs::symlink_metadata(&bands_dir) else {
        return (J::Arr(vec![]), J::Arr(vec![]));
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return (J::Arr(vec![]), J::Arr(vec![]));
    }
    for entry in std::fs::read_dir(bands_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(band) = crate::conductor::load(ctx, &name) else {
            continue;
        };
        for run in crate::conductor::run::recent_runs(ctx, &band) {
            if !run.folder.starts_with(month) {
                continue;
            }
            let date = run.folder.get(..10).unwrap_or("");
            for (label, file) in run.notes {
                let Ok(meta) = std::fs::symlink_metadata(&file) else {
                    continue;
                };
                if meta.file_type().is_symlink() || !meta.is_file() {
                    continue;
                }
                let Some((step, handle)) = label.split_once(' ') else {
                    continue;
                };
                if step.parse::<u32>().is_err() {
                    continue;
                }
                let Some(member) = band.member(handle) else {
                    continue;
                };
                let id = format!("agent:local:conductor:{}:{}", band.name, member.handle);
                let Some((_, employee_name)) =
                    employees.iter().find(|(employee_id, _)| employee_id == &id)
                else {
                    continue;
                };
                *scores.entry(id.clone()).or_default() += 1;
                events.push((
                    date.to_string(),
                    J::Obj(vec![
                        ("employee_id".into(), J::s(&id)),
                        ("employee".into(), J::s(employee_name)),
                        ("band".into(), J::s(&band.name)),
                        ("step".into(), J::s(step)),
                        ("date".into(), J::s(date)),
                    ]),
                ));
            }
        }
    }
    let mut standings: Vec<(String, String, usize)> = scores
        .into_iter()
        .filter_map(|(id, score)| {
            employees
                .iter()
                .find(|(employee_id, _)| employee_id == &id)
                .map(|(_, name)| (id, name.clone(), score))
        })
        .collect();
    standings.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    let standings = J::Arr(
        standings
            .into_iter()
            .enumerate()
            .map(|(index, (id, name, score))| {
                J::Obj(vec![
                    ("rank".into(), J::Num((index + 1).to_string())),
                    ("employee_id".into(), J::s(&id)),
                    ("name".into(), J::s(&name)),
                    ("completed_steps".into(), J::Num(score.to_string())),
                ])
            })
            .collect(),
    );
    events.sort_by(|a, b| b.0.cmp(&a.0));
    let traffic = J::Arr(
        events
            .into_iter()
            .take(50)
            .map(|(_, event)| event)
            .collect(),
    );
    (standings, traffic)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// First-open Conductor tour: current org roster, every known node, and truthful empty standings.
pub fn tour_markup(ctx: &Ctx, doc: &J) -> String {
    let org = doc.get("org");
    let mut people = Vec::new();
    if let Some(org) = org {
        for key in ["ceo", "manager"] {
            if let Some(person) = org.get(key) {
                people.push(person);
            }
        }
        people.extend(org.arr("peers").iter());
    }
    let mut seen = std::collections::HashSet::new();
    let roster = people
        .into_iter()
        .filter_map(|p| {
            let id = p.str("id")?;
            if !seen.insert(id.to_string()) {
                return None;
            }
            let role = p.str("role").or_else(|| p.str("kind")).unwrap_or("Peer");
            let role = p
                .str("source_name")
                .map_or_else(|| role.to_string(), |source| format!("{role} · {source}"));
            Some(format!(
                "<li><strong>{}</strong><span>{}</span></li>",
                html_escape(p.str("name").unwrap_or("Employee")),
                html_escape(&role)
            ))
        })
        .collect::<String>();
    let fleet = crate::fleet::store::load(&crate::fleet::store::Paths::of(ctx));
    let mut nodes = vec![format!(
        "<li><strong>{}</strong><span>Local node · This computer</span></li>",
        html_escape(&human_name("local"))
    )];
    for machine in fleet.machines.iter().filter(|m| m.enabled) {
        nodes.push(format!(
            "<li><strong>{}</strong><span>{} node · {}</span></li>",
            html_escape(&human_name(&machine.name)),
            html_escape(&machine.role),
            html_escape(&machine.name)
        ));
    }
    let nodes = nodes.join("");
    format!(
        r#"<style>
html,body{{overflow-x:clip}}
.tempo-tour{{display:none}}
.tempo-tour[open]{{box-sizing:border-box;position:fixed;inset:0;z-index:var(--z-modal,400);width:100%;height:100%;max-width:none;max-height:none;margin:0;border:0;padding:16px;overflow:auto;background:transparent;display:grid;place-items:center;color:var(--ink)}}
.tempo-tour::backdrop{{background:rgba(8,12,16,.72)}}
.tempo-tour-card{{width:min(720px,100%);max-height:min(90vh,900px);overflow:auto;background:var(--bg);border:1px solid var(--silver);border-radius:8px;padding:clamp(20px,4vw,36px);box-shadow:0 24px 80px rgba(0,0,0,.28)}}
.tempo-tour-card h2,.tempo-tour-card h3{{font-style:normal;overflow-wrap:anywhere}}
.tempo-tour-card h2:focus,.tempo-tour-card h3:focus{{outline:3px solid var(--warm);outline-offset:4px}}
.tempo-tour-card ul{{display:grid;gap:8px;padding-left:20px}}
.tempo-tour-card li{{display:flex;justify-content:space-between;gap:16px;border-bottom:1px solid var(--silver);padding:8px 0}}
.tempo-tour-card .actions{{display:flex;justify-content:space-between;gap:8px;margin-top:20px}}
.tempo-tour-step[hidden]{{display:none}}
.tempo-tour-progress{{font-variant-numeric:tabular-nums;color:var(--silver)}}
@media(max-width:480px){{.tempo-tour-card li{{display:grid;gap:3px}}.tempo-tour-card .actions{{display:grid;grid-template-columns:1fr 1fr}}.tempo-tour-card button{{white-space:nowrap}}}}
</style><dialog class="tempo-tour" id="conductor-first-run" aria-modal="true" aria-labelledby="conductor-tour-title" aria-describedby="conductor-tour-org" tabindex="-1">
<div class="tempo-tour-card"><p class="tempo-tour-progress" id="conductor-tour-progress" aria-live="polite">1 of 3</p><h2 id="conductor-tour-title" tabindex="-1">Welcome to the Conductor</h2>
<section class="tempo-tour-step" id="conductor-tour-org" data-tour-step="0"><h3 class="tempo-section-heading">Your org chart</h3><p id="conductor-tour-copy">You lead. Your manager desk and approved colleagues appear on the org chart.</p><ul>{roster}</ul></section>
<section class="tempo-tour-step" id="conductor-tour-map" data-tour-step="1" hidden><h3 class="tempo-section-heading">Operations map</h3><p>Every known node is listed here. Pending nodes stay outside the company until you approve them.</p><ul>{nodes}</ul></section>
<section class="tempo-tour-step" id="conductor-tour-standings" data-tour-step="2" hidden><h3 class="tempo-section-heading">Employee of the Month</h3><p>No standings yet. The Conductor will show real results after the roster completes work.</p></section>
<div class="actions"><button type="button" id="conductor-tour-dismiss">Dismiss tour</button><div><button type="button" id="conductor-tour-back" disabled>Back</button> <button type="button" id="conductor-tour-next">Continue</button></div></div></div></dialog>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::woodshed::takes::TestDir;

    #[test]
    fn org_chart_round_trips_and_keeps_ceo_manager_peers_shape() {
        let dir = TestDir::new("company-roundtrip");
        let home = &*dir;
        let original = ensure(home).unwrap();
        let reloaded = ensure(home).unwrap();
        assert_eq!(original, reloaded);
        assert!(matches!(
            reloaded.get("org").and_then(|v| v.get("ceo")),
            Some(J::Obj(_))
        ));
        assert!(matches!(
            reloaded.get("org").and_then(|v| v.get("manager")),
            Some(J::Obj(_))
        ));
        assert!(matches!(
            reloaded.get("org").and_then(|v| v.get("peers")),
            Some(J::Arr(_))
        ));
        assert!(reloaded.get("org").unwrap().arr("peers").is_empty());
        assert!(matches!(reloaded.get("nodes"), Some(J::Arr(_))));
    }

    #[test]
    fn read_only_snapshot_never_creates_or_migrates_profile_files() {
        let dir = TestDir::new("company-read-only");
        let home = &*dir;
        let original = read_only(home).unwrap();
        assert_eq!(
            original.get("org").unwrap().get("ceo").unwrap().str("id"),
            Some("ceo")
        );
        assert!(!path(home).exists());

        let legacy = r#"{"schema_version":1,"org":{"ceo":{"id":"ceo","name":"You","kind":"person","role":"CEO","status":"active"},"manager":{"id":"manager:conductor","name":"Conductor","kind":"agent","role":"Manager desk","status":"active"},"peers":[]},"employees":[{"id":"ceo","name":"You","kind":"person","role":"CEO","status":"active"},{"id":"manager:conductor","name":"Conductor","kind":"agent","role":"Manager desk","status":"active"}],"departments":[]}"#;
        std::fs::write(path(home), legacy).unwrap();
        let loaded = read_only(home).unwrap();
        assert!(matches!(loaded.get("nodes"), Some(J::Arr(_))));
        assert_eq!(std::fs::read_to_string(path(home)).unwrap(), legacy);
    }

    #[test]
    fn node_membership_migrates_tracks_approval_rename_and_removal() {
        let dir = TestDir::new("company-node-shape");
        let home = &*dir;
        let legacy = r#"{"schema_version":1,"org":{"ceo":{"id":"ceo","name":"You","kind":"person","role":"CEO","status":"active"},"manager":{"id":"manager:conductor","name":"Conductor","kind":"agent","role":"Manager desk","status":"active"},"peers":[]},"employees":[{"id":"ceo","name":"You","kind":"person","role":"CEO","status":"active"},{"id":"manager:conductor","name":"Conductor","kind":"agent","role":"Manager desk","status":"active"}],"departments":[]}"#;
        std::fs::write(path(home), legacy).unwrap();
        let migrated = ensure(home).unwrap();
        assert!(migrated.arr("nodes").is_empty());
        assert!(std::fs::read_to_string(path(home))
            .unwrap()
            .contains("\"nodes\""));

        let approved = approve_node(home, "local", "This computer", &[]).unwrap();
        assert_eq!(approved.arr("nodes").len(), 1);
        assert_eq!(approved.arr("nodes")[0].str("node_id"), Some("local"));
        let renamed = rename_member(home, "node:local", "Studio machine").unwrap();
        assert_eq!(renamed.arr("nodes")[0].str("name"), Some("Studio machine"));
        let removed = remove_member(home, "node:local").unwrap();
        assert!(removed.arr("nodes").is_empty());
    }

    #[test]
    fn discovered_engines_are_absent_until_the_node_is_approved() {
        let dir = TestDir::new("company-approval");
        let home = &*dir;
        let candidate = J::Obj(vec![
            ("id".into(), J::s("local:ollama:qwen")),
            ("name".into(), J::s("qwen")),
            ("kind".into(), J::s("Ollama model")),
        ]);
        let before = ensure(home).unwrap();
        assert!(!before
            .arr("employees")
            .iter()
            .any(|e| e.str("id") == Some("engine:local:ollama:qwen")));
        let after = approve_node(home, "local", "This computer", &[candidate]).unwrap();
        assert!(after
            .arr("employees")
            .iter()
            .any(|e| e.str("id") == Some("engine:local:ollama:qwen")));
    }

    #[test]
    fn stable_names_are_repeatable_and_credentials_are_rejected() {
        assert_eq!(human_name("node:zeus"), human_name("node:zeus"));
        assert_eq!(codex_logged_in("Logged in using ChatGPT"), Some(true));
        assert_eq!(codex_logged_in("Not logged in"), Some(false));
        assert_eq!(codex_logged_in("Status check failed"), None);
        assert_eq!(claude_logged_in(r#"{"loggedIn":true}"#), Some(true));
        assert_eq!(claude_logged_in(r#"{"loggedIn":false}"#), Some(false));
        assert_eq!(claude_logged_in(r#"{"unexpected":true}"#), None);
        let mut bad = initial();
        bad.set("note", J::s("prefix sk-test-only"));
        assert!(validate(&bad).is_err());
    }

    #[test]
    fn loopback_discovery_creates_pending_engine_findings_without_enrolling_them() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let dir = TestDir::new("company-discovery");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).unwrap();
            let body = r#"{"models":[{"name":"qwen3:4b","details":{"parameter_size":"4B"}}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let spec = crate::conductor::engines::Spec {
            id: "local".into(),
            url: format!("http://{address}"),
        };
        let nodes = discover_specs(&[spec]);
        server.join().unwrap();
        let local = nodes
            .iter()
            .find(|n| n.str("node_id") == Some("local"))
            .unwrap();
        assert!(local
            .arr("engines")
            .iter()
            .any(|e| e.str("name") == Some("qwen3:4b")));
        let profile = ensure(&dir).unwrap();
        assert!(!profile
            .arr("employees")
            .iter()
            .any(|e| e.str("id") == Some("engine:local:ollama:qwen3:4b")));
        let local = nodes
            .iter()
            .find(|n| n.str("node_id") == Some("local"))
            .unwrap();
        let enrolled = approve_node(
            &dir,
            "local",
            local.str("name").unwrap(),
            local.arr("engines"),
        )
        .unwrap();
        assert!(enrolled
            .arr("employees")
            .iter()
            .any(|e| e.str("id") == Some("engine:local:ollama:qwen3:4b")));
        assert!(enrolled
            .get("org")
            .and_then(|o| o.get("peers"))
            .and_then(|p| match p {
                J::Arr(items) => Some(items),
                _ => None,
            })
            .is_some_and(|peers| peers.iter().any(|e| e.str("kind") == Some("engine"))));
        let removed = remove_member(&dir, "node:local").unwrap();
        let reloaded = ensure(&dir).unwrap();
        assert_eq!(removed, reloaded);
        assert!(!reloaded
            .arr("employees")
            .iter()
            .any(|e| e.str("node_id") == Some("local")));
        assert!(reloaded
            .get("org")
            .and_then(|o| o.get("peers"))
            .and_then(|p| match p {
                J::Arr(items) => Some(items),
                _ => None,
            })
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn agent_lanes_are_distinct_stable_employees() {
        let dir = TestDir::new("company-agent-lane");
        let lane = J::Obj(vec![
            ("id".into(), J::s("local:agent:planner")),
            ("name".into(), J::s("planner")),
            ("kind".into(), J::s("Agent lane")),
        ]);
        let first =
            approve_node(&dir, "local", "This computer", std::slice::from_ref(&lane)).unwrap();
        let id = "agent:local:agent:planner";
        let member = first
            .arr("employees")
            .iter()
            .find(|e| e.str("id") == Some(id))
            .unwrap();
        assert_eq!(member.str("kind"), Some("agent"));
        assert_eq!(member.str("name"), Some(human_name(id).as_str()));
        assert_eq!(member.str("source_name"), Some("planner"));
    }

    #[cfg(unix)]
    #[test]
    fn company_profile_symlinks_are_rejected_without_touching_the_target() {
        let dir = TestDir::new("company-symlink");
        let home = &*dir;
        let target = home.join("target.json");
        let original = initial().pretty();
        std::fs::write(&target, original.as_bytes()).unwrap();
        std::os::unix::fs::symlink(&target, path(home)).unwrap();
        assert!(ensure(home).is_err());
        assert!(save(home, &initial()).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), original);
    }
}
