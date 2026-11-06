# The Rust port of the local agent

The Rust port of this folder's harness (`tempo_agent.py`, `mcphost.py`) is the standalone crate
[`perennos-tempo-local-agent/`](../perennos-tempo-local-agent/README.md) at the repository root: library
`tempo_local_agent`, binary `tempo-local-agent`. The same crate lives in the private Perennos core, and the two
copies are kept byte-identical. The Python harness still serves the GUI chat surface until the Rust side has an
HTTP API (see "What is left").

An earlier, smaller port lived in this folder (`tempo-agent/src`). It was retired so there is one port, not two.
Its history is in commits `b5f02b1` to `30e6d83`; for example `git show 30e6d83:tempo-agent/src/agent.rs`.

## Build, test, run

```bash
cargo build --release --manifest-path perennos-tempo-local-agent/Cargo.toml
```

```bash
cargo test --manifest-path perennos-tempo-local-agent/Cargo.toml
```

```bash
cargo clippy --all-targets --manifest-path perennos-tempo-local-agent/Cargo.toml -- -D warnings
```

The tests need no GPU, no Ollama and no network beyond loopback: they use an in-process mock Ollama and a mock
stdio MCP server. CI runs both commands on every push.

Run it through the repo CLI, which looks for the binary in `TEMPO_AGENT_BIN`, then
`perennos-tempo-local-agent/target/{release,debug}`, then `PATH`:

```bash
python tempo.py agent "list the files in ~/notes"
```

`tempo.sh agent ...` and `tempo.cmd agent ...` do the same. With no prompt it reads one turn per line from stdin.

## Configuration

Nothing machine-specific is in the code.

| Setting | Where |
|---|---|
| Engine URL | `OLLAMA_URL` (or `TEMPO_OLLAMA`), else `ollama_url` in the config file, else `http://127.0.0.1:11434` |
| Config file | `--config PATH` or `TEMPO_AGENT_CONFIG`; see `perennos-tempo-local-agent/config.example.json` |
| Run-time data | `data_dir`, or `TEMPO_AGENT_DATA`, else `./tempo-agent-data` (git-ignored) |
| MCP servers | Only from a file you name: `--mcp-config`, `TEMPO_MCP_CONFIG` or `mcp_config`. It may be a `{"mcpServers": {...}}` file or your `~/.claude.json`; with the latter, the per-project servers and `.mcp.json` of the folders in `TEMPO_MCP_PROJECT_DIRS` load too. Nothing loads implicitly, because those files can hold tokens in `env` blocks. |

The Python harness reads `agent-config.json` in this folder (see `agent-config.example.json`) and `TEMPO_CLAUDE_JSON`
for its MCP servers. The two config files have different keys; keep both local, never committed.

## What is ported

Everything the harness does in its agent loop, plus a few things the Python version lacks:

| Python | Rust crate | Notes |
|---|---|---|
| `agent_turn`, `MAX_STEPS`, `trim` | `agent.rs` | Step cap with a tool-free summary request, repeat-call guard, history budget |
| `pick_lane`, fast lane `NEEDS_TOOLS` hand-off | `agent.rs` | Fast model without tools, heavy model with tools |
| `chat_once`, engine fallback | `ollama.rs`, `agent.rs` | Streams `/api/chat`; retries a model (configurable, default 3) before falling back; timings to `logs/timing.jsonl` |
| (new) prompt reuse | `prefix_stable`, default on | System message and offered tools stay fixed within a conversation and per-turn context goes in the user message, so each request extends the previous one and Ollama only reads the new tokens |
| `mcphost.py`, `Tools.select`, `Tools.shrink` | `mcp.rs` | `initialize`, `tools/list`, `tools/call`, declining server requests, schema shrinking, keyword selection of up to 8 MCP tools, blocked and confirm lists |
| RAG (`rag_search`, `build_index`) | `rag.rs` | `embeddinggemma` on the CPU, per-file index cache, keyword fallback, pinned "current work" files |
| `Conv`, workflow file | `conv.rs` | Conversations saved atomically; `workflows/<conv>.md` with task list, notes and log |
| Built-in tools | `tools.rs`, `fstools.rs` | `list_dir`, `read_file`, `write_file`, `trash`, `run_command`, `fetch_url`, `search_memory`, `workflow_update`, `delegate`, `comfy_image`, plus new `edit_file`, `grep`, `glob` |
| `redact`, `cap`, offline probe | `util.rs` | |

## What is left

1. **HTTP API for the GUI.** The chat surface talks to `/agent/new`, `send`, `events`, `confirm`, `workflow`,
   `health`, `convs` and the second-brain views `memory`, `memory/file` and `memory/graph`, and the Python server also serves `/chat/` and proxies the Tempo GUI. Port these to the
   Rust binary, ideally with server-sent events instead of polling, then switch `Start-TempoOffline.ps1` to it.
   Confirmations then come from the GUI instead of the terminal.
2. **`claude_code`**, the Python tool that hands a task to Claude Code on the local engine, has no Rust
   equivalent yet.
3. **Measure on the real machine.** The prompt-reuse saving on the 27B model, and real MCP servers, have only been
   tested against mocks.
4. **Windows in CI.** The crate is written for Windows too, but CI runs the Rust tests on Linux only.

## Keeping private data out

The repository is public. Never commit config files with real paths or hosts, conversations, workflows, logs,
`rag-index.json`, memory files or tokens; `.gitignore` covers the defaults. The CI leak scan rejects token shapes
and home-network addresses, so tests build token-shaped strings at run time.
