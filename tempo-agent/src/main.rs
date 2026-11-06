//! `tempo-agent`: the agent loop in a terminal. The repo CLI's `tempo agent ...`
//! forwards here. Answers go to stdout; status, tool and error lines to stderr.
use std::cell::RefCell;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;
use tempo_agent::config::agent_home;
use tempo_agent::mcp::{McpHost, NoTools, ToolHost};
use tempo_agent::ollama::Ollama;
use tempo_agent::text::{now, truncate_chars};
use tempo_agent::workflow::Workflow;
use tempo_agent::{Agent, AgentConfig, Event, Lane};

const USAGE: &str = "usage: tempo-agent [options] [prompt...]

Runs the Perennos Tempo agent loop against a local Ollama. With no prompt it reads one
turn per line from stdin ('exit' or end of input stops).

options:
  --url URL          Ollama base URL (default: $OLLAMA_URL, else http://127.0.0.1:11434)
  --model NAME       heavy-lane model; repeat for fallbacks (default: from config)
  --fast-model NAME  fast-lane model; repeat for fallbacks
  --lane LANE        auto (default), fast or heavy
  --max-steps N      model calls per turn before the step-cap summary (default 40)
  --workflow PATH    workflow file to continue (default: a new file in
                     $TEMPO_AGENT_HOME/workflows)
  --config PATH      agent-config.json (default: $TEMPO_AGENT_CONFIG, else
                     $TEMPO_AGENT_HOME/agent-config.json when present)
  --no-mcp           do not start MCP servers
  --yes              approve every confirmation (overwrites, flagged MCP tools)
  -q, --quiet        print answers only, no status or tool lines
  -h, --help         show this text
  -V, --version      show the version

environment:
  OLLAMA_URL           engine URL
  TEMPO_AGENT_HOME     run-time data folder (default ~/.perennos-tempo-agent)
  TEMPO_AGENT_CONFIG   config file path
  TEMPO_CLAUDE_JSON    MCP servers file (default ~/.claude.json)
  TEMPO_MCP_PROJECT_DIRS  project folders whose MCP servers are loaded too";

#[derive(Default)]
struct Cli {
    prompt: Vec<String>,
    url: Option<String>,
    models: Vec<String>,
    fast_models: Vec<String>,
    lane: Option<Option<Lane>>,
    max_steps: Option<usize>,
    workflow: Option<PathBuf>,
    config: Option<PathBuf>,
    no_mcp: bool,
    yes: bool,
    quiet: bool,
}

enum Parsed {
    Run(Cli),
    Help,
    Version,
}

fn value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse(args: Vec<String>) -> Result<Parsed, String> {
    let mut cli = Cli::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--url" => cli.url = Some(value(&mut it, &a)?),
            "--model" => cli.models.push(value(&mut it, &a)?),
            "--fast-model" => cli.fast_models.push(value(&mut it, &a)?),
            "--lane" => {
                cli.lane = Some(match value(&mut it, &a)?.as_str() {
                    "auto" => None,
                    "fast" => Some(Lane::Fast),
                    "heavy" => Some(Lane::Heavy),
                    other => return Err(format!("--lane must be auto, fast or heavy, not '{other}'")),
                })
            }
            "--max-steps" => {
                let v = value(&mut it, &a)?;
                cli.max_steps = Some(v.parse().ok().filter(|n| *n > 0).ok_or(format!("--max-steps needs a positive number, not '{v}'"))?);
            }
            "--workflow" => cli.workflow = Some(value(&mut it, &a)?.into()),
            "--config" => cli.config = Some(value(&mut it, &a)?.into()),
            "--no-mcp" => cli.no_mcp = true,
            "--yes" => cli.yes = true,
            "-q" | "--quiet" => cli.quiet = true,
            "--" => {
                cli.prompt.extend(it);
                break;
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
            _ => cli.prompt.push(a),
        }
    }
    Ok(Parsed::Run(cli))
}

/// Streams answers to stdout. Holds back a reply that may turn out to be the fast
/// lane's `NEEDS_TOOLS` hand-off, and does not print a final answer twice.
struct Printer {
    quiet: bool,
    live: String,
    printed: usize,
    segment: String,
}

const HANDOFF: &str = "NEEDS_TOOLS";

impl Printer {
    fn flush_live(&mut self) {
        print!("{}", &self.live[self.printed..]);
        let _ = io::stdout().flush();
        self.printed = self.live.len();
    }

    fn end_stream(&mut self) {
        if self.live.is_empty() {
            return;
        }
        if self.printed == 0 && HANDOFF.starts_with(self.live.trim()) {
            self.live.clear(); // the hand-off word itself: never shown
            return;
        }
        self.flush_live();
        println!();
        self.segment = std::mem::take(&mut self.live);
        self.printed = 0;
    }

    fn answer(&mut self, text: &str) {
        let open = !self.live.is_empty();
        if open {
            self.flush_live();
        }
        let shown = if open { std::mem::take(&mut self.live) } else { std::mem::take(&mut self.segment) };
        match text.strip_prefix(shown.as_str()).filter(|_| !shown.is_empty()) {
            Some(rest) => {
                print!("{rest}");
                if open || !rest.is_empty() {
                    println!();
                }
            }
            None => {
                if open {
                    println!();
                }
                println!("{text}");
            }
        }
        self.segment.clear();
        self.printed = 0;
        let _ = io::stdout().flush();
    }

    fn event(&mut self, e: &Event) {
        match e {
            Event::Delta(t) => {
                self.live.push_str(t);
                if !HANDOFF.starts_with(self.live.trim()) {
                    self.flush_live();
                }
            }
            Event::Assistant { text, .. } | Event::AssistantPartial(text) => self.answer(text),
            Event::User(_) => {}
            other => {
                self.end_stream();
                if !matches!(other, Event::Status(_)) {
                    self.segment.clear();
                }
                let one_line = |s: &str| truncate_chars(&s.replace('\n', " | "), 300).to_string();
                let line = match other {
                    Event::Error(s) => Some(format!("[error] {s}")),
                    _ if self.quiet => None,
                    Event::Status(s) => Some(format!("[status] {s}")),
                    Event::ToolCall { name, args } => Some(format!("[tool] {name} {}", one_line(args))),
                    Event::ToolResult { name, text } => Some(format!("[result] {name}: {}", one_line(text))),
                    _ => None,
                };
                if let Some(l) = line {
                    eprintln!("{l}");
                }
            }
        }
    }
}

fn confirm(yes: bool, what: &str, detail: &str) -> bool {
    let detail = truncate_chars(detail, 300);
    if yes {
        eprintln!("[confirm] {what}: {detail} -> approved (--yes)");
        return true;
    }
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        eprintln!("[confirm] {what}: {detail} -> denied (no terminal; pass --yes to approve)");
        return false;
    }
    eprint!("[confirm] {what}: {detail}\napprove? [y/N] ");
    let _ = io::stderr().flush();
    let mut s = String::new();
    stdin.lock().read_line(&mut s).is_ok() && matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let mut cfg = AgentConfig::load(cli.config.as_deref()).map_err(|e| e.to_string())?;
    if !cli.models.is_empty() {
        cfg.heavy_models = cli.models.clone();
    }
    if !cli.fast_models.is_empty() {
        cfg.fast_models = cli.fast_models.clone();
    }
    if let Some(lane) = cli.lane {
        cfg.lane = lane;
    }
    if let Some(n) = cli.max_steps {
        cfg.max_steps = n;
    }
    let url = cfg.resolve_ollama_url(cli.url.as_deref());
    let mut engine = Ollama::new(&url);
    engine.timeout = Duration::from_secs(cfg.request_timeout_secs);
    let host: Box<dyn ToolHost> = if cli.no_mcp { Box::new(NoTools) } else { Box::new(McpHost::from_config(&cfg)) };

    let stamp = now().replace(['-', ':', 'Z'], "").replace(' ', "-");
    let wf_path = cli.workflow.clone().unwrap_or_else(|| agent_home().join("workflows").join(format!("cli-{stamp}.md")));
    let id = wf_path.file_stem().map_or_else(|| "cli".to_string(), |s| s.to_string_lossy().into_owned());
    let workflow = Workflow::open(&wf_path, &id).map_err(|e| format!("workflow file {}: {e}", wf_path.display()))?;
    if !cli.quiet {
        eprintln!("[status] engine {url}; workflow {}", wf_path.display());
    }

    let printer = Rc::new(RefCell::new(Printer { quiet: cli.quiet, live: String::new(), printed: 0, segment: String::new() }));
    let p = printer.clone();
    let yes = cli.yes;
    let mut agent = Agent::new(cfg, Box::new(engine), host, workflow)
        .with_events(move |e| p.borrow_mut().event(e))
        .with_confirm(move |what, detail| confirm(yes, what, detail));

    if !cli.prompt.is_empty() {
        let ok = agent.run_turn(&cli.prompt.join(" ")).is_ok();
        return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
    }
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    loop {
        if interactive {
            eprint!("you> ");
            let _ = io::stderr().flush();
        }
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        match line.trim() {
            "" => continue,
            "exit" | "quit" => break,
            text => {
                let _ = agent.run_turn(text); // a failure was already printed as [error]
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match parse(std::env::args().skip(1).collect()) {
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Parsed::Version) => {
            println!("tempo-agent {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Parsed::Run(cli)) => run(cli).unwrap_or_else(|e| {
            eprintln!("tempo-agent: {e}");
            ExitCode::FAILURE
        }),
        Err(e) => {
            eprintln!("tempo-agent: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
