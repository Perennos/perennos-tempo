//! JSON config with safe generic defaults. Nothing machine-specific lives in code.
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct HostCfg {
    pub urls: Vec<String>,
    /// House models: when set, only these may be requested on that host.
    pub models: Vec<String>,
    /// The owner's own machine: its model may read here (list_dir, read_file, search_memory)
    /// during a delegate without asking each time. Off unless the owner sets it.
    pub trust_reads: bool,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub ollama_url: String,
    /// More Ollama addresses (your other machines), used in this order when `ollama_url` cannot
    /// be reached; a conversation stays on the engine that served it last.
    pub ollama_fallbacks: Vec<String>,
    pub data_dir: PathBuf,
    pub models: Vec<String>,
    pub fast_models: Vec<String>,
    pub num_ctx: BTreeMap<String, u64>,
    pub default_num_ctx: u64,
    pub keep_alive: String,
    pub temperature: f64,
    pub exclusive_models: Vec<String>,
    pub embed_model: String,
    pub rag_k: usize,
    pub rag_chars: usize,
    pub hist_chars: usize,
    /// Keep the system message and offered tools fixed within a conversation, put per-turn
    /// context in the user message and trim with headroom, so the engine reuses the cached
    /// prompt prefix every turn (only new tokens are processed). Default on.
    pub prefix_stable: bool,
    pub max_steps: usize,
    pub tool_cap: usize,
    pub retries: usize,
    pub retry_delay_ms: u64,
    pub chat_timeout_s: u64,
    pub owner_context: String,
    pub core_file: Option<String>,
    pub rag_files: Vec<String>,
    pub rag_globs: Vec<(String, usize)>,
    pub pinned_current_files: Vec<(String, usize)>,
    pub write_roots: Vec<String>,
    pub trash_dir: PathBuf,
    pub protect_regex: Option<String>,
    pub command_cwd: PathBuf,
    pub command_timeout_s: f64,
    pub offline_probe: String,
    /// Host names `fetch_url` may reach even though they are on this computer or its network
    /// (everything private, loopback or link-local, such as the cloud metadata address, is refused).
    pub fetch_allow_private: Vec<String>,
    /// Command prefixes that run without asking (`cargo test`, `npm run lint`), each part of a
    /// command checked on its own, with no redirection or substitution.
    pub command_allow: Vec<String>,
    /// Command prefixes that are refused outright (`git push`, `docker`).
    pub command_deny: Vec<String>,
    /// The mode a new conversation starts in (`default`, `accept-edits`, `plan`).
    pub permission: crate::conv::Permission,
    /// Hosts `fetch_url` may read without asking (`example.com`, or `*.example.com` for its
    /// subdomains). Any other host asks the owner once per conversation.
    pub fetch_allow_hosts: Vec<String>,
    /// Pages `fetch_url` keeps for later reads (0: none). A full cache takes no new pages; it
    /// never deletes to make room.
    pub fetch_cache_max: usize,
    /// Packs (`<tempo home>/packs/<name>/`) whose connectors may be started. A pack comes from
    /// someone else and its connectors are programs it supplies, so none runs until listed here.
    pub packs_allow: Vec<String>,
    pub hosts: BTreeMap<String, HostCfg>,
    pub mcp_config: Option<String>,
    pub mcp_skip: Vec<String>,
    pub mcp_blocked: BTreeMap<String, Vec<String>>,
    pub mcp_confirm: Vec<(String, String)>,
    /// Tools of pack connectors the owner lets run without asking (packs ask for every tool).
    pub mcp_trust: Vec<(String, String)>,
    pub mcp_hints: BTreeMap<String, String>,
    pub mcp_start_timeout_s: f64,
    pub mcp_call_timeout_s: f64,
    pub comfy_url: String,
    pub comfy_output_dir: Option<String>,
    pub comfy_start_cmd: Option<String>,
    pub comfy_stop_cmd: Option<String>,
    pub lease_status_cmd: Option<String>,
    pub lease_acquire_cmd: Option<String>,
    pub lease_release_cmd: Option<String>,
    /// Connect Perennos Tempo's own products and connector files by themselves (products.rs).
    pub auto_tools: bool,
    /// The Tempo Vault folder (env TEMPO_VAULT wins).
    pub vault: Option<String>,
    /// Let Tempo write to the vault (history is kept; overwrite, rename and trash ask first).
    pub vault_write: bool,
    /// Folders Tempo Studio may read and write (env TEMPO_STUDIO_ROOTS wins; default ~/Music).
    pub studio_roots: Vec<String>,
    /// The settings files this agent was started with (its config, its MCP file): never written
    /// by its own tools, wherever they are. Set by whoever loads them, never from JSON.
    pub own_files: Vec<PathBuf>,
}

/// The agent's data folder when nothing names one: `TEMPO_AGENT_DATA`, else `<Tempo home>/agent`,
/// the home found by the rule every Perennos Tempo product shares (`tempo_home`).
pub fn default_agent_data(data_dir_env: Option<String>) -> PathBuf {
    data_dir_env
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::tempo_home::tempo_home().dir.join("agent"))
}

/// The settings file the agent reads when neither `--config` nor `TEMPO_AGENT_CONFIG` names one,
/// in the launcher's order: an older `<agent data>/agent-config.json`, else the `config.json`
/// that `tempo-launch setup` writes there.
pub fn default_config_file(agent_data: &Path) -> Option<PathBuf> {
    ["agent-config.json", "config.json"]
        .iter()
        .map(|n| agent_data.join(n))
        .find(|p| p.is_file())
}

fn strs(v: &Value, k: &str) -> Option<Vec<String>> {
    v.get(k)?.as_array().map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect()
    })
}
fn string(v: &Value, k: &str) -> Option<String> {
    v.get(k)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}
fn num(v: &Value, k: &str) -> Option<f64> {
    v.get(k)?.as_f64()
}
fn path_list(v: &Value, k: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    if let Some(a) = v.get(k).and_then(|x| x.as_array()) {
        for e in a {
            if let Some(s) = e.as_str() {
                out.push((s.to_string(), 0));
            } else if let Some(p) = e
                .get("pattern")
                .or_else(|| e.get("path"))
                .and_then(|x| x.as_str())
            {
                let n = e
                    .get("newest")
                    .or_else(|| e.get("tail"))
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                out.push((p.to_string(), n as usize));
            }
        }
    }
    out
}

impl Config {
    /// Defaults only. `data_dir_env` is the value of TEMPO_AGENT_DATA, if any; without it the data
    /// lives in `<tempo home>/agent` (see `tempo_home`).
    pub fn defaults(data_dir_env: Option<String>) -> Config {
        let data_dir = default_agent_data(data_dir_env);
        Config {
            ollama_url: "http://127.0.0.1:11434".into(),
            ollama_fallbacks: vec![],
            models: vec!["local-heavy".into()],
            fast_models: vec!["local-fast".into()],
            num_ctx: BTreeMap::new(),
            default_num_ctx: 8192,
            keep_alive: "60m".into(),
            temperature: 0.4,
            exclusive_models: vec![],
            embed_model: "embeddinggemma".into(),
            rag_k: 4,
            rag_chars: 700,
            hist_chars: 11000,
            prefix_stable: true,
            max_steps: 40,
            tool_cap: 40,
            retries: 3,
            retry_delay_ms: 10_000,
            chat_timeout_s: 1200,
            owner_context: String::new(),
            core_file: None,
            rag_files: vec![],
            rag_globs: vec![],
            pinned_current_files: vec![],
            write_roots: vec![data_dir.to_string_lossy().into_owned()],
            trash_dir: data_dir.join("trash"),
            protect_regex: None,
            command_cwd: data_dir.clone(),
            command_timeout_s: 60.0,
            offline_probe: "1.1.1.1:443".into(),
            fetch_allow_private: vec![],
            fetch_allow_hosts: vec![],
            fetch_cache_max: 500,
            command_allow: vec![],
            command_deny: vec![],
            permission: crate::conv::Permission::Default,
            packs_allow: vec![],
            hosts: BTreeMap::new(),
            mcp_config: None,
            auto_tools: true,
            vault: None,
            vault_write: true,
            studio_roots: vec![],
            own_files: vec![],
            mcp_skip: vec![],
            mcp_blocked: BTreeMap::new(),
            mcp_confirm: vec![],
            mcp_trust: vec![],
            mcp_hints: BTreeMap::new(),
            mcp_start_timeout_s: 90.0,
            mcp_call_timeout_s: 600.0,
            comfy_url: "http://127.0.0.1:8188".into(),
            comfy_output_dir: None,
            comfy_start_cmd: None,
            comfy_stop_cmd: None,
            lease_status_cmd: None,
            lease_acquire_cmd: None,
            lease_release_cmd: None,
            data_dir,
        }
    }

    fn set_data_dir(&mut self, d: PathBuf) {
        self.data_dir = d;
        self.write_roots = vec![self.data_dir.to_string_lossy().into_owned()];
        self.trash_dir = self.data_dir.join("trash");
        self.command_cwd = self.data_dir.clone();
    }

    pub fn from_json(v: &Value, data_dir_env: Option<String>) -> Result<Config, String> {
        if !v.is_object() {
            return Err("config must be a JSON object".into());
        }
        let mut c = Config::defaults(data_dir_env.clone());
        if let Some(d) = string(v, "data_dir") {
            c.set_data_dir(PathBuf::from(d));
        }
        if let Some(x) = string(v, "ollama_url") {
            c.ollama_url = x.trim_end_matches('/').to_string();
        }
        if let Some(x) = strs(v, "ollama_fallbacks") {
            c.ollama_fallbacks = x
                .into_iter()
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                .collect();
        }
        if let Some(x) = strs(v, "models").filter(|x| !x.is_empty()) {
            c.models = x;
        }
        if let Some(x) = strs(v, "fast_models") {
            c.fast_models = x;
        }
        if let Some(m) = v.get("num_ctx").and_then(|x| x.as_object()) {
            for (k, n) in m {
                if let Some(n) = n.as_u64() {
                    c.num_ctx.insert(k.clone(), n);
                }
            }
        }
        if let Some(n) = num(v, "default_num_ctx") {
            c.default_num_ctx = n as u64;
        }
        if let Some(x) = string(v, "keep_alive") {
            c.keep_alive = x;
        }
        if let Some(n) = num(v, "temperature") {
            c.temperature = n;
        }
        if let Some(x) = strs(v, "exclusive_models") {
            c.exclusive_models = x;
        }
        if let Some(x) = string(v, "embed_model") {
            c.embed_model = x;
        }
        for (k, slot) in [
            ("rag_k", &mut c.rag_k),
            ("rag_chars", &mut c.rag_chars),
            ("hist_chars", &mut c.hist_chars),
            ("max_steps", &mut c.max_steps),
            ("tool_cap", &mut c.tool_cap),
            ("retries", &mut c.retries),
            ("fetch_cache_max", &mut c.fetch_cache_max),
        ] {
            if let Some(n) = num(v, k) {
                *slot = n as usize;
            }
        }
        if let Some(b) = v.get("prefix_stable").and_then(Value::as_bool) {
            c.prefix_stable = b;
        }
        if let Some(n) = num(v, "retry_delay_ms") {
            c.retry_delay_ms = n as u64;
        }
        if let Some(n) = num(v, "chat_timeout_s") {
            c.chat_timeout_s = n as u64;
        }
        if let Some(x) = string(v, "owner_context") {
            c.owner_context = x;
        }
        c.core_file = string(v, "core_file");
        if let Some(x) = strs(v, "rag_files") {
            c.rag_files = x;
        }
        c.rag_globs = path_list(v, "rag_globs");
        c.pinned_current_files = path_list(v, "pinned_current_files");
        if let Some(x) = strs(v, "packs_allow") {
            c.packs_allow = x;
        }
        if let Some(x) = strs(v, "command_allow") {
            c.command_allow = x;
        }
        if let Some(x) = strs(v, "command_deny") {
            c.command_deny = x;
        }
        if let Some(p) = string(v, "permission").and_then(|p| crate::conv::Permission::parse(&p)) {
            c.permission = p;
        }
        if let Some(x) = strs(v, "fetch_allow_hosts") {
            c.fetch_allow_hosts = x
                .into_iter()
                .map(|h| h.trim().to_ascii_lowercase())
                .collect();
        }
        if let Some(x) = strs(v, "fetch_allow_private") {
            c.fetch_allow_private = x;
        }
        if let Some(x) = strs(v, "write_roots") {
            c.write_roots = x;
        }
        if let Some(x) = string(v, "trash_dir") {
            c.trash_dir = PathBuf::from(x);
        }
        c.protect_regex = string(v, "protect_regex");
        if let Some(x) = string(v, "command_cwd") {
            c.command_cwd = PathBuf::from(x);
        }
        if let Some(n) = num(v, "command_timeout_s") {
            c.command_timeout_s = n;
        }
        if let Some(x) = string(v, "offline_probe") {
            c.offline_probe = x;
        }
        if let Some(m) = v.get("hosts").and_then(|x| x.as_object()) {
            for (name, h) in m {
                let urls = strs(h, "urls").unwrap_or_default();
                if !urls.is_empty() {
                    c.hosts.insert(
                        name.clone(),
                        HostCfg {
                            urls,
                            models: strs(h, "models").unwrap_or_default(),
                            trust_reads: h
                                .get("trust_reads")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        },
                    );
                }
            }
        }
        c.mcp_config = string(v, "mcp_config");
        if let Some(b) = v.get("auto_tools").and_then(|x| x.as_bool()) {
            c.auto_tools = b;
        }
        c.vault = string(v, "vault");
        if let Some(b) = v.get("vault_write").and_then(|x| x.as_bool()) {
            c.vault_write = b;
        }
        if let Some(x) = strs(v, "studio_roots") {
            c.studio_roots = x;
        }
        if let Some(x) = strs(v, "mcp_skip") {
            c.mcp_skip = x;
        }
        if let Some(m) = v.get("mcp_blocked").and_then(|x| x.as_object()) {
            for (k, l) in m {
                c.mcp_blocked.insert(
                    k.clone(),
                    l.as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
        }
        // [["server", "tool"], ...]
        let pairs = |key: &str| -> Vec<(String, String)> {
            v.get(key)
                .and_then(|x| x.as_array())
                .into_iter()
                .flatten()
                .filter_map(|e| {
                    let p = e.as_array()?;
                    Some((
                        p.first()?.as_str()?.to_string(),
                        p.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        };
        c.mcp_confirm.extend(pairs("mcp_confirm"));
        c.mcp_trust.extend(pairs("mcp_trust"));
        if let Some(m) = v.get("mcp_hints").and_then(|x| x.as_object()) {
            for (k, r) in m {
                if let Some(r) = r.as_str() {
                    c.mcp_hints.insert(k.clone(), r.into());
                }
            }
        }
        if let Some(n) = num(v, "mcp_start_timeout_s") {
            c.mcp_start_timeout_s = n;
        }
        if let Some(n) = num(v, "mcp_call_timeout_s") {
            c.mcp_call_timeout_s = n;
        }
        if let Some(x) = string(v, "comfy_url") {
            c.comfy_url = x.trim_end_matches('/').to_string();
        }
        c.comfy_output_dir = string(v, "comfy_output_dir");
        c.comfy_start_cmd = string(v, "comfy_start_cmd");
        c.comfy_stop_cmd = string(v, "comfy_stop_cmd");
        c.lease_status_cmd = string(v, "lease_status_cmd");
        c.lease_acquire_cmd = string(v, "lease_acquire_cmd");
        c.lease_release_cmd = string(v, "lease_release_cmd");
        Ok(c)
    }

    /// Load from `--config` or the env value; with neither, from the agent's own settings file if
    /// there is one (`default_config_file`), else defaults. A file that is there but cannot be read
    /// is an error, wherever it was named: settings such as `command_deny` must never be dropped
    /// quietly.
    pub fn load(
        cli_path: Option<&str>,
        env_path: Option<String>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Config, String> {
        let data_env = env("TEMPO_AGENT_DATA");
        let path = resolve_path(cli_path, env_path).or_else(|| {
            default_config_file(&default_agent_data(data_env.clone()))
                .map(|p| p.to_string_lossy().into_owned())
        });
        let mut c = match path {
            None => Config::defaults(data_env),
            Some(p) => {
                let txt = std::fs::read_to_string(&p)
                    .map_err(|e| format!("cannot read config {p}: {e}"))?;
                let v: Value = serde_json::from_str(&txt)
                    .map_err(|e| format!("bad config JSON in {p}: {e}"))?;
                if v.get("tempo_home").is_some() {
                    eprintln!("[config: {p} names tempo_home, which is not read: the Tempo home comes from TEMPO_HOME, as for every Perennos Tempo product (tempo-launch home shows it)]");
                }
                let mut c = Config::from_json(&v, data_env)?;
                c.own_files.push(PathBuf::from(&p));
                c
            }
        };
        let url = ["OLLAMA_URL", "TEMPO_OLLAMA"].iter().find_map(|k| {
            env(k)
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
        });
        if let Some(u) = url {
            c.ollama_url = u.trim_end_matches('/').to_string();
        }
        Ok(c)
    }

    pub fn num_ctx_for(&self, model: &str) -> u64 {
        self.num_ctx
            .get(model)
            .copied()
            .unwrap_or(self.default_num_ctx)
    }

    pub fn is_qwen(model: &str) -> bool {
        let m = model.to_lowercase();
        m.contains("qwen")
    }

    /// Every host usable by delegate. An implicit `local` host is the main Ollama URL.
    pub fn all_hosts(&self) -> BTreeMap<String, HostCfg> {
        let mut h = self.hosts.clone();
        h.entry("local".into()).or_insert(HostCfg {
            urls: vec![self.ollama_url.clone()],
            models: vec![],
            trust_reads: true,
        });
        h
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
}

/// Config path: CLI flag wins, then env TEMPO_AGENT_CONFIG.
pub fn resolve_path(cli: Option<&str>, env: Option<String>) -> Option<String> {
    cli.map(String::from).or(env).filter(|s| !s.is_empty())
}

/// MCP config path: CLI flag, then env TEMPO_MCP_CONFIG, then the main config's `mcp_config`.
/// Explicit only: nothing is loaded unless one of these names a file. A leading `~` is the home folder.
pub fn mcp_path(cli: Option<&str>, env: Option<String>, cfg: &Config) -> Option<PathBuf> {
    resolve_path(cli, env)
        .or_else(|| cfg.mcp_config.clone())
        .map(|p| expand_home(&p, home_dir()))
}

/// The user's home folder: `HOME`, else `USERPROFILE` (Windows).
pub fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `~`, `~/x` or `~\x` under `home`; anything else unchanged.
pub fn expand_home(p: &str, home: Option<PathBuf>) -> PathBuf {
    let rest = if p == "~" {
        Some("")
    } else {
        p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\"))
    };
    match (rest, home) {
        (Some(r), Some(h)) => h.join(r),
        _ => PathBuf::from(p),
    }
}

pub fn ensure_dirs(c: &Config) -> Result<(), String> {
    for d in [
        c.data_dir.join("conversations"),
        c.data_dir.join("workflows"),
        c.logs_dir(),
    ] {
        std::fs::create_dir_all(&d)
            .map_err(|e| format!("cannot create {}: {e}", Path::new(&d).display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_agents_own_settings_file_is_read_when_nothing_names_one() {
        let d = std::env::temp_dir().join(format!("tla-cfgfile-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let ds = d.to_string_lossy().into_owned();
        let env = |k: &str| (k == "TEMPO_AGENT_DATA").then(|| ds.clone());
        let models = |c: Config| c.models;
        assert_eq!(
            models(Config::load(None, None, &env).unwrap()),
            Config::defaults(None).models
        );
        // the config.json that tempo-launch setup writes
        std::fs::write(d.join("config.json"), r#"{"models": ["from-setup"]}"#).unwrap();
        assert_eq!(
            models(Config::load(None, None, &env).unwrap()),
            ["from-setup"]
        );
        // an older agent-config.json comes first, as in the launcher
        std::fs::write(d.join("agent-config.json"), r#"{"models": ["older"]}"#).unwrap();
        assert_eq!(models(Config::load(None, None, &env).unwrap()), ["older"]);
        // a file that is named wins over both
        let named = d.join("named.json");
        std::fs::write(&named, r#"{"models": ["named"]}"#).unwrap();
        assert_eq!(
            models(Config::load(named.to_str(), None, &env).unwrap()),
            ["named"]
        );
        // a broken file is an error, never quietly skipped (it may hold command_deny)
        std::fs::write(d.join("agent-config.json"), "{not json").unwrap();
        assert!(Config::load(None, None, &env)
            .unwrap_err()
            .contains("bad config JSON"));
    }

    #[test]
    fn defaults_are_generic() {
        let c = Config::defaults(None);
        assert_eq!(c.ollama_url, "http://127.0.0.1:11434");
        // without TEMPO_AGENT_DATA the data lives in <tempo home>/agent
        assert_eq!(
            c.data_dir,
            crate::tempo_home::tempo_home().dir.join("agent")
        );
        assert_eq!(c.embed_model, "embeddinggemma");
        assert_eq!(c.max_steps, 40);
        let c = Config::defaults(Some("/x/data".into()));
        assert_eq!(c.data_dir, PathBuf::from("/x/data"));
    }

    #[test]
    fn parses_json() {
        let v = json!({"ollama_url":"http://h:1/","models":["a","b"],"fast_models":["f"],"num_ctx":{"a":4096},
            "retry_delay_ms":0,"pinned_current_files":[{"path":"/p.md","tail":40}],"rag_globs":[{"pattern":"/n/*.md","newest":5}],
            "hosts":{"box":{"urls":["http://b:11434"],"models":["house"]}},"mcp_confirm":[["s","t"]],"mcp_blocked":{"s":["x"]}});
        let c = Config::from_json(&v, None).unwrap();
        assert_eq!(c.ollama_url, "http://h:1");
        assert_eq!(c.models, ["a", "b"]);
        assert_eq!(c.num_ctx_for("a"), 4096);
        assert_eq!(c.num_ctx_for("zz"), 8192);
        assert_eq!(c.retry_delay_ms, 0);
        assert_eq!(c.pinned_current_files, [("/p.md".to_string(), 40)]);
        assert_eq!(c.rag_globs, [("/n/*.md".to_string(), 5)]);
        assert_eq!(c.hosts["box"].models, ["house"]);
        assert!(c.all_hosts().contains_key("local"));
        assert_eq!(c.mcp_confirm, [("s".to_string(), "t".to_string())]);
        assert!(Config::from_json(&json!([1]), None).is_err());
    }

    #[test]
    fn example_config_parses() {
        let v: Value = serde_json::from_str(include_str!("../config.example.json")).unwrap();
        let c = Config::from_json(&v, None).unwrap();
        assert_eq!(c.models.len(), 2);
        assert_eq!(c.rag_globs[0].1, 400);
        assert!(c.lease_status_cmd.is_none());
        assert_eq!(c.hosts["other-machine"].models, ["house-model-name"]);
    }

    #[test]
    fn paths_from_cli_and_env() {
        assert_eq!(resolve_path(Some("a"), Some("b".into())), Some("a".into()));
        assert_eq!(resolve_path(None, Some("b".into())), Some("b".into()));
        assert_eq!(resolve_path(None, None), None);
        let c = Config::defaults(None);
        // TEMPO_MCP_CONFIG value
        assert_eq!(
            mcp_path(None, Some("/m.json".into()), &c),
            Some(PathBuf::from("/m.json"))
        );
        assert_eq!(
            mcp_path(Some("/cli.json"), Some("/m.json".into()), &c),
            Some(PathBuf::from("/cli.json"))
        );
        assert_eq!(mcp_path(None, None, &c), None);
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            expand_home("~/.claude.json", home.clone()),
            PathBuf::from("/home/u/.claude.json")
        );
        assert_eq!(expand_home("~", home.clone()), PathBuf::from("/home/u"));
        assert_eq!(
            expand_home("~other/x", home.clone()),
            PathBuf::from("~other/x")
        );
        assert_eq!(
            expand_home("/abs/m.json", home),
            PathBuf::from("/abs/m.json")
        );
    }

    #[test]
    fn ollama_url_wins_over_tempo_ollama() {
        let both = |k: &str| match k {
            "OLLAMA_URL" => Some("http://gpu-box.local:11434/".to_string()),
            "TEMPO_OLLAMA" => Some("http://127.0.0.1:9".to_string()),
            _ => None,
        };
        assert_eq!(
            Config::load(None, None, &both).unwrap().ollama_url,
            "http://gpu-box.local:11434"
        );
        let blank = |k: &str| match k {
            "OLLAMA_URL" => Some("  ".to_string()),
            "TEMPO_OLLAMA" => Some("http://127.0.0.1:9".to_string()),
            _ => None,
        };
        assert_eq!(
            Config::load(None, None, &blank).unwrap().ollama_url,
            "http://127.0.0.1:9"
        );
        let none = |_: &str| None;
        assert_eq!(
            Config::load(None, None, &none).unwrap().ollama_url,
            "http://127.0.0.1:11434"
        );
    }

    #[test]
    fn load_reads_file_and_env_override() {
        let d = crate::util::testutil::tmpdir("cfg");
        let f = d.join("c.json");
        std::fs::write(&f, r#"{"models":["m1"]}"#).unwrap();
        let env = |k: &str| match k {
            "TEMPO_OLLAMA" => Some("http://127.0.0.1:9/".to_string()),
            "TEMPO_AGENT_DATA" => Some("/dd".to_string()),
            _ => None,
        };
        let c = Config::load(Some(f.to_str().unwrap()), None, &env).unwrap();
        assert_eq!(c.models, ["m1"]);
        assert_eq!(c.ollama_url, "http://127.0.0.1:9");
        assert_eq!(c.data_dir, PathBuf::from("/dd"));
        assert!(Config::load(Some("/no/such/file.json"), None, &env).is_err());
    }
}
