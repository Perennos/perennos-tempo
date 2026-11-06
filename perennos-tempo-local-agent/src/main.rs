use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use tempo_local_agent::agent::Agent;
use tempo_local_agent::config::{self, Config};
use tempo_local_agent::conv::{Conv, StdSink, TtyConfirm};
use tempo_local_agent::mcp::{self, McpHost};
use tempo_local_agent::tools::Env;

const HELP: &str = "Perennos Tempo local agent: an offline agent loop over a local Ollama.

USAGE:
    tempo-local-agent [OPTIONS] [PROMPT]

With a PROMPT it runs one turn and exits. With no PROMPT it starts a line REPL
(an empty line or `exit` ends it).

OPTIONS:
    --config PATH       JSON config file (env TEMPO_AGENT_CONFIG)
    --mcp-config PATH   MCP servers file, {\"mcpServers\": {...}} (env TEMPO_MCP_CONFIG).
                        ~/.claude.json works too; its per-project servers load for
                        the folders in env TEMPO_MCP_PROJECT_DIRS. Nothing loads by default.
    --conv ID           continue the conversation ID (workflow and history are kept)
    --reindex           rebuild the RAG index from scratch first
    -h, --help          show this help

Risky actions (overwrite, trash, shell commands that write) ask y/N on the
terminal. When stdin is not a terminal they are denied.

Env OLLAMA_URL (or TEMPO_OLLAMA) overrides the Ollama URL.
";

struct Args {
    config: Option<String>,
    mcp: Option<String>,
    conv: Option<String>,
    reindex: bool,
    prompt: Vec<String>,
}

fn parse(args: Vec<String>) -> Result<Option<Args>, String> {
    let mut a = Args {
        config: None,
        mcp: None,
        conv: None,
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
            "--reindex" => a.reindex = true,
            s if s.starts_with("--") && s.len() > 2 => {
                return Err(format!("unknown option {s} (try --help)"))
            }
            _ => a.prompt.push(x),
        }
    }
    Ok(Some(a))
}

fn real_main() -> Result<(), String> {
    let Some(args) = parse(std::env::args().skip(1).collect())? else {
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
    let servers = match config::mcp_path(args.mcp.as_deref(), getenv("TEMPO_MCP_CONFIG"), &cfg) {
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
    let host = McpHost::new(&cfg, servers);
    let mut agent = Agent::new(Env::new(cfg.clone(), Box::new(TtyConfirm), host));
    if args.reindex {
        let n = agent.env.rag.borrow_mut().build_index(&cfg, true);
        eprintln!("[reindexed {n} files]");
    }
    let mut conv = Conv::open(&cfg, args.conv.as_deref())?;
    eprintln!("[conversation {}]", conv.id);
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
