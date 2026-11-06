//! Perennos Tempo's own products as tools, with no setup. `tempo-vault`, `tempo-studio` and
//! `tempo-room` are found next to this program (then in `TEMPO_BIN`, then on PATH) and connected
//! as MCP servers with fixed arguments. Connector files add more: one MCP server per
//! `<tempo home>/connectors/*.json`, and the same inside each pack (`<tempo home>/packs/<name>/`).
//!
//! Order of precedence: products, then connector files (they may replace a product), then the
//! explicit `mcpServers` config (it always wins). `TEMPO_AUTO_TOOLS=0` or `"auto_tools": false`
//! turns all of this off. The tools that change or send something ask first: overwriting,
//! renaming or trashing a note, and posting into a chat room.
use crate::config::{expand_home, home_dir, Config};
use crate::mcp::ServerCfg;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Found {
    pub servers: BTreeMap<String, ServerCfg>,
    /// (server, tool) pairs that need the owner's approval.
    pub confirm: Vec<(String, String)>,
    /// One line per product or connector: what was connected, or why not.
    pub notes: Vec<String>,
}

/// Where to look for product binaries: this program's folder, `TEMPO_BIN`, then PATH.
pub fn bin_dirs(getenv: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut d = vec![];
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        d.push(p);
    }
    for var in ["TEMPO_BIN", "PATH"] {
        if let Some(v) = getenv(var) {
            d.extend(std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()));
        }
    }
    d
}

/// `name` (or `name.exe` on Windows) in the first folder that has it.
pub fn find_bin(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

/// `TEMPO_HOME`, then the config's `tempo_home`, then `~/.tempo`.
pub fn tempo_home(cfg: &Config, getenv: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    getenv("TEMPO_HOME")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| cfg.tempo_home.clone())
        .map(|h| expand_home(&h, home_dir()))
        .or_else(|| home_dir().map(|h| h.join(".tempo")))
}

fn off(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 40
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut f: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
        .collect();
    f.sort();
    f
}

/// One connector file: `{"name"?, "command", "args"?, "env"?, "confirm"?: [tool], "enabled"?}`.
fn connector(path: &Path) -> Result<Option<(String, ServerCfg, Vec<String>)>, String> {
    let txt = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&txt).map_err(|e| format!("not JSON ({e})"))?;
    if v["enabled"] == false {
        return Ok(None);
    }
    let name = v["name"]
        .as_str()
        .map(str::to_string)
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if !valid_name(&name) {
        return Err(format!("the name {name:?} must be letters, digits, - or _"));
    }
    let command = v["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .ok_or("it has no \"command\"")?
        .to_string();
    let strs = |k: &str| -> Vec<String> {
        v[k].as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let env = v["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(Some((
        name,
        ServerCfg {
            command: expand_home(&command, home_dir())
                .to_string_lossy()
                .into_owned(),
            args: strs("args"),
            env,
        },
        strs("confirm"),
    )))
}

/// Everything Tempo can connect by itself, from this machine's state.
pub fn discover(cfg: &Config, getenv: &dyn Fn(&str) -> Option<String>, dirs: &[PathBuf]) -> Found {
    let mut f = Found::default();
    if !cfg.auto_tools || getenv("TEMPO_AUTO_TOOLS").is_some_and(|v| off(&v)) {
        f.notes.push("automatic tools are off".into());
        return f;
    }
    let s = |p: &Path| p.to_string_lossy().into_owned();
    // Vault: the notes, read and (by default) written through history snapshots.
    let vault = getenv("TEMPO_VAULT")
        .filter(|v| !v.trim().is_empty())
        .or_else(|| cfg.vault.clone())
        .map(|v| expand_home(&v, home_dir()));
    match (find_bin("tempo-vault", dirs), vault) {
        (Some(b), Some(v)) if v.is_dir() => {
            let mut args = vec!["mcp".to_string()];
            if cfg.vault_write {
                args.push("--write".into());
                for t in ["vault_write", "vault_rename", "vault_trash"] {
                    f.confirm.push(("vault".into(), t.into()));
                }
            }
            args.extend(["--vault".to_string(), s(&v)]);
            f.notes.push(format!("vault: {} ({}){}", s(&v), s(&b), if cfg.vault_write { "" } else { ", read-only" }));
            f.servers.insert("vault".into(), ServerCfg { command: s(&b), args, env: BTreeMap::new() });
        }
        (Some(_), Some(v)) => f.notes.push(format!("vault: not connected, {} is not a folder", s(&v))),
        (Some(_), None) => f.notes.push("vault: tempo-vault is here; set TEMPO_VAULT (or \"vault\" in the config) to connect it".into()),
        (None, Some(_)) => f.notes.push("vault: a vault is set but tempo-vault was not found next to Tempo, in TEMPO_BIN or on PATH".into()),
        (None, None) => {}
    }
    // Studio: REAPER mixing and mastering, limited to the music folders.
    if let Some(b) = find_bin("tempo-studio", dirs) {
        let mut roots: Vec<PathBuf> =
            match getenv("TEMPO_STUDIO_ROOTS").filter(|v| !v.trim().is_empty()) {
                Some(v) => std::env::split_paths(&v).collect(),
                None if !cfg.studio_roots.is_empty() => cfg
                    .studio_roots
                    .iter()
                    .map(|r| expand_home(r, home_dir()))
                    .collect(),
                None => home_dir().map(|h| h.join("Music")).into_iter().collect(),
            };
        roots.retain(|r| r.is_dir());
        if roots.is_empty() {
            f.notes.push("studio: tempo-studio is here; set TEMPO_STUDIO_ROOTS (or \"studio_roots\") to the folders it may use".into());
        } else {
            let mut args = vec!["mcp".to_string()];
            for r in &roots {
                args.extend(["--allow-root".to_string(), s(r)]);
            }
            f.notes.push(format!(
                "studio: {} ({})",
                roots.iter().map(|r| s(r)).collect::<Vec<_>>().join(", "),
                s(&b)
            ));
            f.servers.insert(
                "studio".into(),
                ServerCfg {
                    command: s(&b),
                    args,
                    env: BTreeMap::new(),
                },
            );
        }
    }
    // Room: the chat rooms; posting into one asks first.
    if let Some(b) = find_bin("tempo-room", dirs) {
        f.confirm.push(("room".into(), "room_post".into()));
        f.notes.push(format!("room: {}", s(&b)));
        f.servers.insert(
            "room".into(),
            ServerCfg {
                command: s(&b),
                args: vec!["mcp".into()],
                env: BTreeMap::new(),
            },
        );
    }
    // Plug-in connectors, then the connectors of every enabled pack.
    if let Some(home) = tempo_home(cfg, getenv) {
        let mut files = json_files(&home.join("connectors"));
        let mut packs: Vec<PathBuf> = std::fs::read_dir(home.join("packs"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        packs.sort();
        for p in packs {
            let manifest: Value = std::fs::read_to_string(p.join("pack.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(Value::Null);
            if manifest["enabled"] == false {
                f.notes.push(format!(
                    "pack {}: off",
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ));
                continue;
            }
            files.extend(json_files(&p.join("connectors")));
        }
        for file in files {
            match connector(&file) {
                Ok(Some((name, sc, confirm))) => {
                    f.confirm.retain(|(srv, _)| *srv != name);
                    f.confirm
                        .extend(confirm.into_iter().map(|t| (name.clone(), t)));
                    f.notes.push(format!("connector {name}: {}", s(&file)));
                    f.servers.insert(name, sc);
                }
                Ok(None) => {}
                Err(e) => f.notes.push(format!("connector {} skipped: {e}", s(&file))),
            }
        }
    }
    f
}

/// Add what was found to the explicit servers; an explicit entry of the same name wins.
pub fn merge(explicit: &mut BTreeMap<String, ServerCfg>, cfg: &mut Config, found: Found) {
    for (name, sc) in found.servers {
        explicit.entry(name).or_insert(sc);
    }
    for c in found.confirm {
        if !cfg.mcp_confirm.contains(&c) {
            cfg.mcp_confirm.push(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::testutil::tmpdir;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    fn exe(name: &str) -> String {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        }
    }

    #[test]
    fn products_connect_themselves() {
        let d = tmpdir("products");
        let bin = d.join("bin");
        for n in ["tempo-vault", "tempo-studio", "tempo-room"] {
            touch(&bin.join(exe(n)));
        }
        let vault = d.join("Vault");
        let music = d.join("Music");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::create_dir_all(&music).unwrap();
        let cfg = Config::defaults(Some(d.to_string_lossy().into_owned()));
        let g = env(&[
            ("TEMPO_VAULT", vault.to_string_lossy().into_owned()),
            ("TEMPO_STUDIO_ROOTS", music.to_string_lossy().into_owned()),
            ("TEMPO_HOME", d.join("home").to_string_lossy().into_owned()),
        ]);
        let f = discover(&cfg, &g, std::slice::from_ref(&bin));
        assert_eq!(
            f.servers.keys().collect::<Vec<_>>(),
            ["room", "studio", "vault"]
        );
        assert_eq!(
            f.servers["vault"].args,
            ["mcp", "--write", "--vault", &vault.to_string_lossy()]
        );
        assert_eq!(
            f.servers["studio"].args,
            ["mcp", "--allow-root", &music.to_string_lossy()]
        );
        assert_eq!(f.servers["room"].args, ["mcp"]);
        assert!(f.servers["vault"].command.ends_with(&exe("tempo-vault")));
        for c in [
            ("vault", "vault_trash"),
            ("vault", "vault_rename"),
            ("vault", "vault_write"),
            ("room", "room_post"),
        ] {
            assert!(
                f.confirm.contains(&(c.0.to_string(), c.1.to_string())),
                "{c:?} asks first"
            );
        }
        // read-only when the config says so
        let mut ro = cfg.clone();
        ro.vault_write = false;
        let f2 = discover(&ro, &g, std::slice::from_ref(&bin));
        assert_eq!(
            f2.servers["vault"].args,
            ["mcp", "--vault", &vault.to_string_lossy()]
        );
        assert!(!f2.confirm.iter().any(|(s, _)| s == "vault"));
        // nothing set: nothing connected, and the notes say how
        let f3 = discover(
            &cfg,
            &env(&[("TEMPO_HOME", d.join("none").to_string_lossy().into_owned())]),
            std::slice::from_ref(&bin),
        );
        assert_eq!(
            f3.servers.keys().collect::<Vec<_>>(),
            ["room"],
            "studio needs a folder that exists, vault needs a vault"
        );
        assert!(
            f3.notes.iter().any(|n| n.contains("set TEMPO_VAULT")),
            "{:?}",
            f3.notes
        );
        // switched off
        let f4 = discover(
            &cfg,
            &env(&[("TEMPO_AUTO_TOOLS", "0".into())]),
            std::slice::from_ref(&bin),
        );
        assert!(f4.servers.is_empty() && f4.notes == ["automatic tools are off"]);
        // no binaries anywhere: no servers, no noise
        let f5 = discover(
            &cfg,
            &env(&[("TEMPO_HOME", d.join("none").to_string_lossy().into_owned())]),
            &[d.join("empty")],
        );
        assert!(
            f5.servers.is_empty() && f5.notes.is_empty(),
            "{:?}",
            f5.notes
        );
    }

    #[test]
    fn connectors_and_packs_plug_in() {
        let d = tmpdir("connectors");
        let home = d.join("home");
        std::fs::create_dir_all(home.join("connectors")).unwrap();
        std::fs::write(home.join("connectors/comfy.json"), r#"{"command": "~/tools/comfy-mcp", "args": ["--port", "8188"], "env": {"MODE": "local"}, "confirm": ["render"]}"#).unwrap();
        std::fs::write(
            home.join("connectors/off.json"),
            r#"{"command": "x", "enabled": false}"#,
        )
        .unwrap();
        std::fs::write(home.join("connectors/broken.json"), "{nope").unwrap();
        std::fs::write(home.join("connectors/bad name.json"), r#"{"command": "x"}"#).unwrap();
        std::fs::create_dir_all(home.join("packs/law-firm/connectors")).unwrap();
        std::fs::write(
            home.join("packs/law-firm/pack.json"),
            r#"{"name": "law-firm"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("packs/law-firm/connectors/clio.json"),
            r#"{"name": "clio", "command": "clio-mcp"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(home.join("packs/retired/connectors")).unwrap();
        std::fs::write(
            home.join("packs/retired/pack.json"),
            r#"{"enabled": false}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("packs/retired/connectors/old.json"),
            r#"{"command": "old"}"#,
        )
        .unwrap();
        // a connector may replace a product of the same name
        let bin = d.join("bin");
        touch(&bin.join(exe("tempo-room")));
        std::fs::write(
            home.join("connectors/room.json"),
            r#"{"command": "my-room", "args": ["mcp", "--data", "x"]}"#,
        )
        .unwrap();
        let mut cfg = Config::defaults(Some(d.to_string_lossy().into_owned()));
        let f = discover(
            &cfg,
            &env(&[("TEMPO_HOME", home.to_string_lossy().into_owned())]),
            &[bin],
        );
        assert_eq!(
            f.servers.keys().collect::<Vec<_>>(),
            ["clio", "comfy", "room"]
        );
        assert_eq!(f.servers["comfy"].args, ["--port", "8188"]);
        assert_eq!(f.servers["comfy"].env["MODE"], "local");
        assert!(
            !f.servers["comfy"].command.starts_with('~'),
            "a leading ~ is the home folder"
        );
        assert_eq!(f.servers["room"].command, "my-room");
        assert!(f.confirm.contains(&("comfy".into(), "render".into())));
        assert!(
            !f.confirm.iter().any(|(s, _)| s == "room"),
            "the replaced product's own rules go with it"
        );
        assert!(
            f.notes
                .iter()
                .any(|n| n.contains("broken.json skipped: not JSON")),
            "{:?}",
            f.notes
        );
        assert!(
            f.notes.iter().any(|n| n.contains("bad name.json skipped")),
            "{:?}",
            f.notes
        );
        assert!(f.notes.iter().any(|n| n == "pack retired: off"));
        // explicit config always wins over anything found
        let mut explicit = BTreeMap::from([(
            "comfy".to_string(),
            ServerCfg {
                command: "mine".into(),
                args: vec![],
                env: BTreeMap::new(),
            },
        )]);
        merge(&mut explicit, &mut cfg, f);
        assert_eq!(explicit["comfy"].command, "mine");
        assert!(
            explicit.contains_key("clio")
                && cfg.mcp_confirm.contains(&("comfy".into(), "render".into()))
        );
    }
}
