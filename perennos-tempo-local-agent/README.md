# Perennos Tempo local agent

A small offline agent for a local Ollama. It runs a tool loop (builtin file and shell tools, stdio MCP servers, markdown RAG, delegation to other Ollama hosts, ComfyUI images), keeps a persistent workflow file per conversation, and is built to keep working with no internet.

Rust, three dependencies (`serde_json`, `ureq`, `regex`), no async runtime. It uses Ollama's `/api/chat` with streaming and tool calls.

## Run

```
cargo build --release
./target/release/tempo-local-agent --config config.json "list the files in ./notes"   # one turn
./target/release/tempo-local-agent --config config.json                                # line REPL
```

Options: `--config PATH` (or env `TEMPO_AGENT_CONFIG`), `--mcp-config PATH` (or env `TEMPO_MCP_CONFIG`), `--conv ID` to continue a conversation, `--reindex` to rebuild the RAG index, `--help`.

Streamed text goes to stdout, status lines to stderr. With no config file it uses safe defaults: Ollama at `http://127.0.0.1:11434`, data in env `TEMPO_AGENT_DATA` or `./tempo-agent-data`. Env `TEMPO_OLLAMA` overrides the Ollama URL.

## What it does

- Agent loop: up to 40 steps, history trimmed to a character budget, repeat-call guard (the same call is not re-run), and at the step cap one last request with no tools that asks what is done and what is next.
- Lanes: short plain questions go to a fast model with no tools (it answers `NEEDS_TOOLS` to hand the turn to the heavy lane); anything tool-like, hard or long goes straight to the heavy lane.
- Engine retry: an HTTP error or an in-band `{"error":...}` line (for example a CUDA crash on the first request after a model load) retries the same model up to 3 times, then falls back to the next model in the lane. If all fail the turn ends with "all engines failed".
- Timing of every request (first token, total, load, prompt tokens) is appended to `logs/timing.jsonl` in the data dir.
- Builtin tools: `list_dir`, `read_file`, `write_file`, `trash`, `run_command`, `fetch_url`, `search_memory`, `workflow_update`, `delegate`, `comfy_image`, `edit_file`, `grep`, `glob`.
- MCP: stdio servers from a `{"mcpServers": {"name": {"command", "args", "env"}}}` file. The agent offers all builtins plus up to 8 MCP tools picked by keyword fit to the prompt, named `server__tool`.
- RAG: markdown files and globs from the config, chunked on blank lines, embedded with `embeddinggemma` on the CPU only (`num_gpu: 0`), cached in `rag-index.json` per file (reused while the file mtime is unchanged). If embedding fails, keyword search still works.
- Workflow: `workflows/<conv>.md` has `## Task list`, `## Notes` and `## Log`; the last 2500 characters go into the system prompt. Conversations are saved atomically under `conversations/`.

## Safety

- Nothing is ever deleted. `trash` only moves into `<trash_dir>/<YYYYMMDD>/` under a unique name; `edit_file` copies the original into the trash dir before writing; `run_command` refuses `rm`, `del`, `Remove-Item` and similar.
- `write_file`, `trash` and `edit_file` only work inside `write_roots`. Paths that look like secrets are refused, and tool output is redacted and capped at 6000 characters.
- Overwrites, trash moves and commands that write, install or use the network ask `y/N` on the terminal. When stdin is not a terminal they are denied. There is no approve-everything switch.
- `protect_regex` (optional) refuses stop/kill commands that mention protected services. `delegate` only allows a host's configured house models, and `comfy_image` refuses to run while the GPU lease status says it is held.

## Config keys

All keys are optional. See `config.example.json` for placeholders.

| Key | Meaning |
| --- | --- |
| `ollama_url`, `data_dir` | Ollama address and where conversations, workflows, logs and the RAG index live |
| `models`, `fast_models` | Heavy-lane and fast-lane model lists (fallback order) |
| `num_ctx`, `default_num_ctx`, `keep_alive`, `temperature` | Per-model context size and request options (`think:false` is sent for model names containing `qwen`) |
| `exclusive_models` | Name fragments; before such a model loads, our other models are unloaded |
| `retries`, `retry_delay_ms`, `chat_timeout_s` | Engine retry behaviour |
| `max_steps`, `tool_cap`, `hist_chars`, `rag_k`, `rag_chars` | Loop and prompt limits |
| `embed_model`, `rag_files`, `rag_globs`, `pinned_current_files` | RAG sources; `rag_globs` entries are `{"pattern", "newest"}`; pinned files lead the results for "what am I working on" style questions |
| `core_file`, `owner_context` | Text pinned into every system prompt |
| `write_roots`, `trash_dir`, `protect_regex`, `command_cwd`, `command_timeout_s` | Safety and shell settings |
| `offline_probe` | `host:port` for the quick offline check (env `TEMPO_OFFLINE` forces offline) |
| `hosts` | Name to `{"urls": [...], "models": [...]}` for `delegate`; an implicit `local` host is the main Ollama |
| `mcp_config`, `mcp_skip`, `mcp_blocked`, `mcp_confirm`, `mcp_hints`, `mcp_*_timeout_s` | MCP settings |
| `comfy_url`, `comfy_output_dir`, `comfy_start_cmd`, `comfy_stop_cmd`, `lease_status_cmd`, `lease_acquire_cmd`, `lease_release_cmd` | ComfyUI and optional GPU lease commands |

## Test

```
cargo test
cargo clippy --all-targets -- -D warnings
```

Tests use an in-process mock Ollama on `127.0.0.1` and a mock stdio MCP server; they never touch the network beyond loopback.
