//! Settings. Defaults mirror `tempo_agent.py`; machine-specific values (folders, hosts,
//! owner details) come only from a local `agent-config.json` or environment variables,
//! never from the source. Unknown keys in the file are ignored, so the Python harness
//! and this crate can share one file.
use crate::error::{Error, Result};
use crate::ollama::{base_name, DEFAULT_OLLAMA_URL};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Small model, no tools; it can hand the turn up by answering `NEEDS_TOOLS`.
    Fast,
    /// Big model with tools.
    Heavy,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Fast => "fast",
            Lane::Heavy => "heavy",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub ollama_url: Option<String>,
    /// Heavy lane, in fallback order.
    pub heavy_models: Vec<String>,
    /// Fast lane, in fallback order.
    pub fast_models: Vec<String>,
    pub num_ctx: BTreeMap<String, u32>,
    pub default_num_ctx: u32,
    pub temperature: f64,
    pub keep_alive: String,
    /// Models whose name starts with one of these get `think: false`.
    pub no_think_prefixes: Vec<String>,
    pub max_steps: usize,
    /// Most tools offered in one turn, built-ins included.
    pub tool_cap: usize,
    /// Most MCP tools offered in one turn.
    pub mcp_tools_per_turn: usize,
    /// Characters kept from one tool result.
    pub result_cap: usize,
    /// History budget in serialized characters (about 4K tokens for 16000).
    pub hist_chars: usize,
    /// Wait before the one retry after an engine HTTP error (model swap, load).
    pub retry_delay_secs: f64,
    pub request_timeout_secs: u64,
    pub mcp_start_timeout_secs: u64,
    pub mcp_call_timeout_secs: u64,
    /// How the system prompt names the person the agent works for.
    pub owner: String,
    /// One extra line about the owner for the system prompt.
    pub owner_note: String,
    /// Folders `write_file` may write into. Empty means the agent home only.
    pub write_roots: Vec<PathBuf>,
    /// Extra words that send a turn to the heavy lane (machine names, project names).
    pub heavy_words: Vec<String>,
    /// MCP tools never offered: server -> tool names.
    pub blocked_tools: BTreeMap<String, Vec<String>>,
    /// MCP tools that need approval, as `server:tool`.
    pub confirm_tools: Vec<String>,
    /// Words in a prompt that favour a server's tools: server -> words.
    pub mcp_hints: BTreeMap<String, Vec<String>>,
    /// Servers left out of the tool catalog (still callable by name).
    pub mcp_skip: Vec<String>,
    pub claude_json: Option<PathBuf>,
    /// Project folders whose MCP servers are loaded too (from `.claude.json` and `.mcp.json`).
    pub mcp_project_dirs: Vec<PathBuf>,
    /// Force a lane instead of routing (CLI `--lane`).
    #[serde(skip)]
    pub lane: Option<Lane>,
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

impl Default for AgentConfig {
    fn default() -> Self {
        let blocked = [
            ("perennos_tempo", &[
                "terminal", "execute_code", "process", "write_file", "patch", "read_file", "search_files", "cronjob",
                "browser_cdp", "ha_call_service", "delegate_task", "tempo_run", "tempo_submit", "tempo_improve_run",
                "tempo_waitlist_cancel", "tempo_hold", "tempo_gpu_lease", "tempo_book", "tempo_finance_execution_contract",
            ][..]),
            ("comfy-media", &[
                "launch_comfyui", "stop_comfyui", "restart_comfyui", "update_comfyui", "switch_comfyui_version",
                "install_node", "download_model", "partner_generate", "emit_partner_workflow", "auth_login",
                "generate_image", "run_template",
            ][..]),
            ("playwright-isolated", &["browser_run_code_unsafe", "browser_evaluate", "browser_file_upload"][..]),
        ];
        let hints = [
            ("comfy-media", &["image", "picture", "photo", "comfy", "render", "draw", "video", "workflow", "generate"][..]),
            ("playwright-isolated", &["browse", "browser", "web", "page", "site", "url", "http", "click", "screenshot"][..]),
            ("perennos_tempo", &[
                "tempo", "kanban", "memory", "research", "status", "job", "route", "lint", "todo", "house", "engine",
                "home assistant", "homeassistant", "search",
            ][..]),
        ];
        AgentConfig {
            ollama_url: None,
            heavy_models: strings(&["uncensored-qwen3.8-27b-8k", "local-qwen3", "local-gpt-oss-32k"]),
            fast_models: strings(&["local-qwen3", "uncensored-qwen3.8-27b-8k"]),
            num_ctx: [
                ("uncensored-qwen3.8-27b-8k", 8192),
                ("uncensored-qwen3.8-27b", 8192),
                ("local-gpt-oss-32k", 16384),
                ("local-qwen3", 8192),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect(),
            default_num_ctx: 8192,
            temperature: 0.4,
            keep_alive: "60m".to_string(),
            no_think_prefixes: strings(&["uncensored-qwen", "local-qwen"]),
            max_steps: 40,
            tool_cap: 40,
            mcp_tools_per_turn: 8,
            result_cap: 6000,
            hist_chars: 16000,
            retry_delay_secs: 8.0,
            request_timeout_secs: 1200,
            mcp_start_timeout_secs: 90,
            mcp_call_timeout_secs: 600,
            owner: "the owner".to_string(),
            owner_note: String::new(),
            write_roots: Vec::new(),
            heavy_words: Vec::new(),
            blocked_tools: blocked.iter().map(|(s, ts)| (s.to_string(), strings(ts))).collect(),
            confirm_tools: strings(&["comfy-media:run_workflow", "perennos_tempo:memory", "perennos_tempo:web_extract"]),
            mcp_hints: hints.iter().map(|(s, ws)| (s.to_string(), strings(ws))).collect(),
            mcp_skip: strings(&["perennos_knowledge"]),
            claude_json: None,
            mcp_project_dirs: Vec::new(),
            lane: None,
        }
    }
}

impl AgentConfig {
    pub fn from_json(text: &str) -> Result<Self> {
        let mut c: AgentConfig = serde_json::from_str(text)?;
        c.expand_paths();
        Ok(c)
    }

    /// Load `explicit` (must exist), else `$TEMPO_AGENT_CONFIG`, else
    /// `<agent home>/agent-config.json` if present, else defaults.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let home = agent_home();
        let named = explicit.map(Path::to_path_buf).or_else(|| std::env::var_os("TEMPO_AGENT_CONFIG").map(PathBuf::from));
        let mut cfg = match named {
            Some(p) => {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| Error::Config(format!("cannot read config {}: {e}", p.display())))?;
                Self::from_json(&text).map_err(|e| Error::Config(format!("{}: {e}", p.display())))?
            }
            None => match std::fs::read_to_string(home.join("agent-config.json")) {
                Ok(text) => Self::from_json(&text).map_err(|e| Error::Config(format!("agent-config.json: {e}")))?,
                Err(_) => AgentConfig::default(),
            },
        };
        if cfg.write_roots.is_empty() {
            cfg.write_roots.push(home);
        }
        Ok(cfg)
    }

    fn expand_paths(&mut self) {
        for p in self.write_roots.iter_mut().chain(self.mcp_project_dirs.iter_mut()).chain(self.claude_json.iter_mut()) {
            *p = expand_tilde(p);
        }
    }

    pub fn models_for(&self, lane: Lane) -> &[String] {
        match lane {
            Lane::Fast => &self.fast_models,
            Lane::Heavy => &self.heavy_models,
        }
    }

    pub fn num_ctx_for(&self, model: &str) -> u32 {
        self.num_ctx.get(model).or_else(|| self.num_ctx.get(base_name(model))).copied().unwrap_or(self.default_num_ctx)
    }

    pub fn no_think(&self, model: &str) -> bool {
        self.no_think_prefixes.iter().any(|p| model.starts_with(p.as_str()))
    }

    pub fn is_blocked(&self, server: &str, tool: &str) -> bool {
        self.blocked_tools.get(server).is_some_and(|ts| ts.iter().any(|t| t == tool))
    }

    pub fn needs_confirm(&self, server: &str, tool: &str) -> bool {
        self.confirm_tools.iter().any(|c| c.split_once(':') == Some((server, tool)))
    }

    pub fn retry_delay(&self) -> Duration {
        Duration::from_secs_f64(self.retry_delay_secs.max(0.0))
    }

    /// CLI value, then `OLLAMA_URL`/`TEMPO_OLLAMA`, then the file, then loopback.
    pub fn resolve_ollama_url(&self, cli: Option<&str>) -> String {
        cli.map(str::to_string)
            .or_else(crate::ollama::ollama_url_from_env)
            .or_else(|| self.ollama_url.clone())
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
    }

    /// `TEMPO_CLAUDE_JSON`, then `claude_json` in the file, then `~/.claude.json`.
    pub fn claude_json_path(&self) -> PathBuf {
        claude_json_from(std::env::var_os("TEMPO_CLAUDE_JSON"), self.claude_json.as_deref(), home_dir())
    }

    /// `TEMPO_MCP_PROJECT_DIRS` (path-list separated), then `mcp_project_dirs` in the file.
    pub fn project_dirs(&self) -> Vec<PathBuf> {
        match std::env::var_os("TEMPO_MCP_PROJECT_DIRS") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v).map(|p| expand_tilde(&p)).collect(),
            _ => self.mcp_project_dirs.clone(),
        }
    }
}

fn claude_json_from(env: Option<OsString>, file: Option<&Path>, home: Option<PathBuf>) -> PathBuf {
    if let Some(v) = env.filter(|v| !v.is_empty()) {
        return expand_tilde(Path::new(&v));
    }
    if let Some(p) = file {
        return p.to_path_buf();
    }
    home.unwrap_or_default().join(".claude.json")
}

/// The user's home folder: `HOME`, else `USERPROFILE` (Windows).
pub fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"].iter().filter_map(std::env::var_os).find(|v| !v.is_empty()).map(PathBuf::from)
}

/// `~` or `~/...` to the home folder; anything else unchanged.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let rest = if s == "~" {
        Some("")
    } else {
        s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\"))
    };
    match (rest, home_dir()) {
        (Some(r), Some(h)) => h.join(r),
        _ => p.to_path_buf(),
    }
}

/// Where run-time data lives (workflows, default write root): `TEMPO_AGENT_HOME`, else
/// `~/.perennos-tempo-agent`. Never inside the repository unless you point it there.
pub fn agent_home() -> PathBuf {
    match std::env::var_os("TEMPO_AGENT_HOME").filter(|v| !v.is_empty()) {
        Some(v) => expand_tilde(Path::new(&v)),
        None => home_dir().unwrap_or_else(std::env::temp_dir).join(".perennos-tempo-agent"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_python_harness() {
        let c = AgentConfig::default();
        assert_eq!((c.max_steps, c.tool_cap, c.result_cap, c.hist_chars), (40, 40, 6000, 16000));
        assert_eq!(c.heavy_models[0], "uncensored-qwen3.8-27b-8k");
        assert_eq!(c.fast_models[0], "local-qwen3");
        assert_eq!(c.num_ctx_for("local-gpt-oss-32k"), 16384);
        assert_eq!(c.num_ctx_for("local-gpt-oss-32k:latest"), 16384);
        assert_eq!(c.num_ctx_for("something-else"), 8192);
        assert!(c.no_think("local-qwen3") && c.no_think("uncensored-qwen3.8-27b-8k") && !c.no_think("local-gpt-oss-32k"));
        assert!(c.is_blocked("comfy-media", "launch_comfyui") && !c.is_blocked("comfy-media", "server_info"));
        assert!(c.needs_confirm("perennos_tempo", "memory") && !c.needs_confirm("perennos_tempo", "status"));
        assert!(c.write_roots.is_empty(), "no folder is hard-coded");
    }

    #[test]
    fn file_overrides_and_ignores_unknown_keys() {
        let c = AgentConfig::from_json(
            r#"{"heavy_models": ["big"], "fast_models": ["small"], "max_steps": 3, "owner": "Sam",
                "write_roots": ["~/work"], "hosts": {"ignored": "by rust"}, "num_ctx": {"big": 4096}}"#,
        )
        .unwrap();
        assert_eq!((c.models_for(Lane::Heavy), c.models_for(Lane::Fast)), (&["big".to_string()][..], &["small".to_string()][..]));
        assert_eq!((c.max_steps, c.owner.as_str(), c.num_ctx_for("big")), (3, "Sam", 4096));
        assert_eq!(c.tool_cap, 40, "unset keys keep their defaults");
        if let Some(h) = home_dir() {
            assert_eq!(c.write_roots, vec![h.join("work")]);
        }
        assert!(AgentConfig::from_json("{not json").is_err());
    }

    #[test]
    fn claude_json_precedence() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(claude_json_from(None, None, home.clone()), PathBuf::from("/home/u/.claude.json"));
        assert_eq!(claude_json_from(None, Some(Path::new("/cfg/c.json")), home.clone()), PathBuf::from("/cfg/c.json"));
        assert_eq!(
            claude_json_from(Some("/env/c.json".into()), Some(Path::new("/cfg/c.json")), home.clone()),
            PathBuf::from("/env/c.json")
        );
        assert_eq!(claude_json_from(Some("".into()), None, home), PathBuf::from("/home/u/.claude.json"));
    }

    #[test]
    fn tilde_expansion() {
        assert_eq!(expand_tilde(Path::new("/abs/x")), PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde(Path::new("~user/x")), PathBuf::from("~user/x"));
        if let Some(h) = home_dir() {
            assert_eq!(expand_tilde(Path::new("~")), h);
            assert_eq!(expand_tilde(Path::new("~/a/b")), h.join("a/b"));
        }
    }
}
