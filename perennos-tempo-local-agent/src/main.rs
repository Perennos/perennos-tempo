use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use tempo_local_agent::agent::Agent;
use tempo_local_agent::config::{self, Config};
use tempo_local_agent::conv::{Conv, StdSink, TtyConfirm};
use tempo_local_agent::mcp::{self, McpHost};
use tempo_local_agent::server::{self, ServeOpts};
use tempo_local_agent::tools::Env;

const HELP: &str = "Perennos Tempo local agent: an offline agent loop over a local Ollama.

USAGE:
    tempo-local-agent [OPTIONS] [PROMPT]
    tempo-local-agent serve [OPTIONS] [SERVE OPTIONS]

With a PROMPT it runs one turn and exits. With no PROMPT it starts a line REPL
(an empty line or `exit` ends it). A new conversation works in the folder you start
it in. In the chat, /help lists the commands (/cd, /mode, /plan, /undo, /diff,
/compact, /init, /review, /think, /model, /usage, /status, your own commands);
!cmd runs a command yourself and @file attaches a file.

OPTIONS:
    --config PATH       JSON config file (env TEMPO_AGENT_CONFIG; else agent-config.json or
                        config.json in the agent's data folder, if there is one)
    --mcp-config PATH   MCP servers file, {\"mcpServers\": {...}} (env TEMPO_MCP_CONFIG).
                        ~/.claude.json works too; its per-project servers load for
                        the folders in env TEMPO_MCP_PROJECT_DIRS. Nothing loads by default.
    --conv ID           continue the conversation ID (workflow and history are kept)
    -c, --continue      continue the most recent conversation
    --reindex           rebuild the RAG index from scratch first
    -h, --help          show this help

Risky actions (overwrite, trash, shell commands that write) ask y/N on the
terminal. When stdin is not a terminal they are denied.

SERVE: the HTTP API the Tempo GUI chat surface uses (/agent/*, plus an SSE
stream at /agent/stream?conv=ID). Loopback only unless --bind and --allow-remote say
otherwise. Each start writes a fresh key to <data>/control.key; every call but
GET /agent/health must send it as X-Tempo-Key (the page takes it from #key=...).
Approvals come from the GUI and are denied after --confirm-timeout.
    --port N            listen port (default 8765)
    --bind ADDR         listen address (default 127.0.0.1)
    --allow-host NAME   also accept this Host header (repeatable; NAME:443 also accepts the
                        bare NAME that `tailscale serve` sends)
    --allow-remote      allow --bind to an address other than loopback (the key still applies)
    --chat-dir DIR      serve the chat surface files at /chat/
    --gui-port N        pass every other path to the Tempo GUI server on 127.0.0.1:N
    --confirm-timeout S seconds an approval waits before it is denied (default 900)

Env OLLAMA_URL (or TEMPO_OLLAMA) overrides the Ollama URL.
";

struct Args {
    config: Option<String>,
    mcp: Option<String>,
    conv: Option<String>,
    cont: bool,
    reindex: bool,
    prompt: Vec<String>,
}

fn parse(args: Vec<String>) -> Result<Option<Args>, String> {
    let mut a = Args {
        config: None,
        mcp: None,
        conv: None,
        cont: false,
        reindex: false,
        prompt: vec![],
    };
    let mut it = args.into_iter();
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match x.as_str() {
            "-h" | "--help" => return Ok(None),
            "--config" => a.config = Some(val("--config")?),
            "--mcp-config" => a.mcp = Some(val("--mcp-config")?),
            "--conv" => a.conv = Some(val("--conv")?),
            "-c" | "--continue" => a.cont = true,
            "--reindex" => a.reindex = true,
            s if s.starts_with("--") && s.len() > 2 => {
                return Err(format!("unknown option {s} (try --help)"))
            }
            _ => a.prompt.push(x),
        }
    }
    Ok(Some(a))
}

/// Pull the serve-only options out of the argument list.
fn parse_serve(args: &mut Vec<String>) -> Result<ServeOpts, String> {
    let mut o = ServeOpts::default();
    let mut rest = Vec::new();
    let mut it = std::mem::take(args).into_iter();
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match x.as_str() {
            "--port" => {
                o.port = val("--port")?
                    .parse()
                    .map_err(|_| "--port needs a number")?
            }
            "--bind" => {
                o.bind = val("--bind")?
                    .parse()
                    .map_err(|_| "--bind needs an IP address")?
            }
            "--allow-host" => o.allow_hosts.push(val("--allow-host")?),
            "--allow-remote" => o.allow_remote = true,
            "--chat-dir" => o.chat_dir = Some(val("--chat-dir")?.into()),
            "--gui-port" => {
                o.gui_port = Some(
                    val("--gui-port")?
                        .parse()
                        .map_err(|_| "--gui-port needs a number")?,
                )
            }
            "--confirm-timeout" => {
                let secs: u64 = val("--confirm-timeout")?
                    .parse()
                    .map_err(|_| "--confirm-timeout needs seconds")?;
                o.confirm_timeout = std::time::Duration::from_secs(secs);
            }
            _ => rest.push(x),
        }
    }
    *args = rest;
    Ok(o)
}

fn real_main() -> Result<(), String> {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let serve = argv.first().is_some_and(|a| a == "serve");
    let serve_opts = if serve {
        argv.remove(0);
        Some(parse_serve(&mut argv)?)
    } else {
        None
    };
    let Some(args) = parse(argv)? else {
        print!("{HELP}");
        return Ok(());
    };
    let getenv = |k: &str| std::env::var(k).ok();
    let cfg = Config::load(
        args.config.as_deref(),
        getenv("TEMPO_AGENT_CONFIG"),
        &getenv,
    )?;
    config::ensure_dirs(&cfg)?;
    let mut cfg = cfg;
    let mut servers = match config::mcp_path(args.mcp.as_deref(), getenv("TEMPO_MCP_CONFIG"), &cfg)
    {
        Some(p) => {
            let project_dirs: Vec<std::path::PathBuf> = std::env::var_os("TEMPO_MCP_PROJECT_DIRS")
                .map(|v| {
                    std::env::split_paths(&v)
                        .filter(|d| !d.as_os_str().is_empty())
                        .map(|d| config::expand_home(&d.to_string_lossy(), config::home_dir()))
                        .collect()
                })
                .unwrap_or_default();
            mcp::load_server_configs(&p, &project_dirs)?
        }
        None => Default::default(),
    };
    // Perennos Tempo's own products and plug-in connectors, connected by themselves.
    let found = tempo_local_agent::products::discover(
        &cfg,
        &getenv,
        &tempo_local_agent::products::bin_dirs(&getenv),
    );
    for n in &found.notes {
        eprintln!("[tools: {n}]");
    }
    tempo_local_agent::products::merge(&mut servers, &mut cfg, found);
    if let Some(opts) = serve_opts {
        if !args.prompt.is_empty() || args.conv.is_some() || args.cont || args.reindex {
            return Err("serve takes no prompt, --conv, --continue or --reindex".into());
        }
        if !opts.bind.is_loopback() && opts.allow_remote {
            eprintln!("[warning: listening on {}, not loopback; anyone who can reach it and has the key can drive this agent]", opts.bind);
        }
        return server::serve(cfg, servers, opts);
    }
    let host = McpHost::new(&cfg, servers);
    let mut agent = Agent::new(Env::new(cfg.clone(), Box::new(TtyConfirm), host));
    if args.reindex {
        let n = agent.env.rag.borrow_mut().build_index(&cfg, true);
        eprintln!("[reindexed {n} files]");
    }
    let id = match (&args.conv, args.cont) {
        (Some(_), true) => return Err("give --conv or --continue, not both".into()),
        (None, true) => {
            Some(newest_conversation(&cfg).ok_or("there is no conversation to continue yet")?)
        }
        (id, false) => id.clone(),
    };
    let mut conv = Conv::open(&cfg, id.as_deref())?;
    // like Claude Code and Codex: a new conversation works where it was started
    if conv.cwd.is_none() && conv.messages.is_empty() {
        conv.cwd = std::env::current_dir().ok();
    }
    eprintln!(
        "[conversation {} in {}; /help lists the commands]",
        conv.id,
        agent.env.cwd(&conv).display()
    );
    let mut sink = StdSink::default();
    let result = if !args.prompt.is_empty() {
        agent
            .turn(&mut conv, &args.prompt.join(" "), &mut sink)
            .map(|_| println!())
    } else {
        repl(&mut agent, &mut conv, &mut sink)
    };
    agent.env.mcp.borrow_mut().stop_all();
    result.map(|_| ())
}

/// The owner's most recently used conversation (rooms' guest conversations are not the owner's).
fn newest_conversation(cfg: &Config) -> Option<String> {
    let mut all: Vec<(std::time::SystemTime, std::path::PathBuf)> =
        std::fs::read_dir(cfg.data_dir.join("conversations"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
            .collect();
    all.sort();
    all.into_iter().rev().find_map(|(_, p)| {
        let text = std::fs::read_to_string(&p).ok()?;
        (!text.contains("\"mode\":\"guest\""))
            .then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .flatten()
    })
}

fn repl(agent: &mut Agent, conv: &mut Conv, sink: &mut StdSink) -> Result<(), String> {
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal();
    loop {
        if tty {
            eprint!("> ");
            let _ = std::io::stderr().flush();
        }
        let mut line = String::new();
        if stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            return Ok(());
        }
        let t = line.trim();
        if t.is_empty() || t == "exit" || t == "quit" {
            return Ok(());
        }
        match agent.turn(conv, t, sink) {
            Ok(_) => println!(),
            Err(e) => eprintln!("error: {e}"), // the REPL keeps going; the conversation is saved
        }
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
