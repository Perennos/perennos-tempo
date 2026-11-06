# tempo-agent: the Rust port

The Rust crate in this folder (`tempo-agent`, library `tempo_agent`, binary `tempo-agent`) ports the core of
`tempo_agent.py` and `mcphost.py`: the agent loop over Ollama tool calling and the MCP stdio host. The Python
harness still runs the GUI chat surface; the crate is the tested core that will replace it piece by piece.

Dependencies: `serde` and `serde_json` only. HTTP to Ollama is a small `std::net` client, because engines serve
plain HTTP on loopback or the LAN.

## Build, test, run

From the repository root (a Cargo workspace) or from this folder:

```bash
cargo build --release -p tempo-agent
```

```bash
cargo test
```

```bash
cargo clippy --all-targets -- -D warnings
```

The tests need no GPU, no Ollama and no network: they run against a scripted mock Ollama HTTP server and a mock
MCP server (over in-process pipes, and as a real child process). CI runs both commands on every push.

Run it:

```bash
target/release/tempo-agent "what is in ~/notes?"
```

```bash
python tempo.py agent --lane heavy --model qwen3:14b "list the files in ~/notes"
```

`tempo agent ...` (the repo CLI, `tempo.sh` / `tempo.cmd`) forwards to the binary. It looks in `TEMPO_AGENT_BIN`,
then `target/release`, `target/debug`, then `PATH`. With no prompt the binary reads one turn per line from stdin.
`tempo-agent --help` lists every option.

## Configuration

Nothing machine-specific is in the code. Precedence: command-line flag, then environment, then the config file,
then the defaults (which mirror the Python harness).

| Setting | Where |
|---|---|
| Engine URL | `--url`, `OLLAMA_URL` (or `TEMPO_OLLAMA`), `ollama_url`, default `http://127.0.0.1:11434` |
| Run-time data (workflow files, default write root) | `TEMPO_AGENT_HOME`, default `~/.perennos-tempo-agent` |
| Config file | `--config`, `TEMPO_AGENT_CONFIG`, else `$TEMPO_AGENT_HOME/agent-config.json` if present |
| MCP servers | `TEMPO_CLAUDE_JSON` or `claude_json`, default `~/.claude.json`; plus `.mcp.json` in each of `TEMPO_MCP_PROJECT_DIRS` / `mcp_project_dirs` |
| Models | `--model` / `--fast-model` (repeat for fallbacks), `heavy_models` / `fast_models` |

`agent-config.example.json` shows every key. The Python harness reads the same file (its `TEMPO_AGENT_HOME` defaults
to this folder) and each side ignores the keys it does not use. Copy it to `agent-config.json`, which git ignores.

## What is ported

| Python | Rust | Notes |
|---|---|---|
| Ollama `/api/chat` messages, tool calls, tool definitions, options | `ollama.rs` | Streams NDJSON (chunked, Content-Length or read-to-close); tolerates string or null arguments and the newer `id`/`index` fields |
| `chat_once` | `Ollama::chat` | Content deltas, tool calls and timings (first token, total, load, prompt and output tokens) |
| `agent_turn` loop, `MAX_STEPS` | `agent.rs` `Agent::run_turn` | Events: user, status, delta, partial, assistant, tool call, tool result, error |
| Engine retry and fallback | `send_with_retry` | One retry after an HTTP error (wait `retry_delay_secs`), then the next model in the lane |
| Repeat-call guard | `run_call` | Same name and arguments (keys sorted) answered from the first result, with the `(xN)` count |
| Step-cap summary | end of `turn` | One call without tools asking for done/left/next, then the "say continue" note |
| `pick_lane`, `TOOLISH`, `HARDISH`, fast lane `NEEDS_TOOLS` hand-off | `lanes.rs`, `agent.rs` | Word-boundary matching without a regex crate; sticky heavy lane when only the big model is loaded |
| `trim` | `trim_history` | See differences below |
| `Tools.select`, `HINTS`, `BLOCKED_MCP`, `CONFIRM_MCP` | `select_tools`, config | Same scoring (name words x3, description words, hint +1, 3 to qualify, at most 8) |
| `Tools.shrink` | `mcp::shrink_schema` | Drops `title`/`$schema`/`examples`/`default` metadata, cuts descriptions to 120 chars, keeps properties with those names |
| `mcphost.load_server_configs` | `mcp::load_server_configs` | Global, per-project and `.mcp.json` servers; stdio entries only |
| `McpServer` start, `initialize`, `notifications/initialized`, `tools/list`, `tools/call`, decline server requests, `stop` | `mcp::McpServer` | Follows `nextCursor` paging; fails fast when a server exits; kills only its own child |
| Lazy server start | `mcp::McpHost` | All servers start in parallel the first time the catalog is needed |
| Workflow file (task list, notes, log) | `workflow.rs` | Same Markdown layout, so both harnesses can continue one file |
| `list_dir`, `read_file`, `write_file`, `workflow_update` | `tools.rs` | Secret-path refusal, write roots, approval for overwrites |
| `redact`, `cap` | `text.rs` | Also redacts the token after `Bearer` and a few more token shapes |
| CLI client (`ask.py`) | `main.rs` | Terminal client for the loop itself rather than the HTTP API |

### Deliberate differences

- On a fast-to-heavy hand-off the "FAST LANE: no tools" note is removed from the system prompt (the Python loop
  left it in, telling the heavy model it had no tools while offering them).
- The repeat guard is per turn. The Python guard lived on the conversation, so asking to list a folder again in a
  later turn returned the old listing.
- The step-cap summary is kept in history as an assistant message.
- History trimming drops earlier turns first, then the oldest steps of the current turn, and never the current
  question. The Python `trim` could drop the question when several large tool results landed in one step.
- The fast-lane hand-off word is never shown in the CLI.
- Timestamps are UTC (`2026-11-06 12:00:00Z`): the standard library has no local time zone.
- The system prompt does not cite memory excerpts yet, because retrieval is not ported (below).

## What is left

In rough order of value:

1. **HTTP API for the GUI** (`/agent/new`, `send`, `events`, `confirm`, `workflow`, `health`, `convs`), the static
   `/chat/` files and the proxy to the Tempo GUI server, ideally with a server-sent-events stream instead of
   polling, then switch `Start-TempoOffline.ps1` to the binary.
2. **Conversation persistence**: `conversations/<id>.json`, one workflow file per conversation, confirmations that
   wait for the GUI (with the 15-minute timeout).
3. **Retrieval** ("second brain"): chunking, `embeddinggemma` through `/api/embed`, the index file, and the hybrid
   score (keywords, title and description lines, recency, definitional boost, "current work" files first).
4. **Remaining built-ins**: `search_memory`, `delegate` (host probing, house-model protection, the sub-loop),
   `run_command` (delete refusal, confirmation and protection rules), `trash`, `fetch_url` (with the offline probe),
   `claude_code`, `comfy_image` (GPU lease, unloading Tempo's own models).
5. Timing log (`logs/timing.jsonl`) and delta batching for the GUI.
6. Windows checks in CI. The code builds for Windows (child processes start with `CREATE_NO_WINDOW`, paths compare
   case-insensitively), but CI runs the Rust tests on Linux only.

## Known limits

- Engine URLs must be `http://`. No TLS and no proxy support, on purpose.
- Write roots are checked lexically after resolving `.` and `..`; a symlink inside a root that points elsewhere is
  not detected (same as the Python harness).
- Stopping an MCP server closes its stdin, waits up to 300 ms, then kills that child process. Grandchildren (for
  example `npx` starting `node`) can outlive it, as with the Python host.

## Keeping private data out

The repository is public. Never commit `agent-config.json`, conversations, workflows, logs, `rag-index.json`,
memory files or tokens; `.gitignore` covers the defaults. The CI leak scan rejects token shapes and home-network
addresses, and the redaction tests build their token-shaped strings at run time for that reason.
