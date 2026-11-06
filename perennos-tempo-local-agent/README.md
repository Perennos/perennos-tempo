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

MCP servers load only from a file you name. That can be a plain `{"mcpServers": {...}}` file or your Claude Code `~/.claude.json` (`--mcp-config ~/.claude.json`); with the latter, the per-project servers and `.mcp.json` of the folders in env `TEMPO_MCP_PROJECT_DIRS` (path-list separated) load too.

Streamed text goes to stdout, status lines to stderr. With no config file it uses safe defaults: Ollama at `http://127.0.0.1:11434`, data in env `TEMPO_AGENT_DATA` or `<Tempo home>/agent`. The Tempo home is found as every Perennos Tempo product finds it: `TEMPO_HOME` (or the older `PERENNOS_TEMPO_DATA`), else a folder an earlier version used if it exists (`C:\LocalAI\apps\decision-head` on Windows, `$XDG_STATE_HOME/perennos-tempo` or `~/.local/state/perennos-tempo` elsewhere), else `%LOCALAPPDATA%\Perennos Tempo` on Windows and `~/.tempo` elsewhere. Nothing is moved between them. Env `OLLAMA_URL` (or `TEMPO_OLLAMA`) overrides the Ollama URL.

## Serve the Tempo GUI

```
./target/release/tempo-local-agent serve --config config.json --port 8765 \
    --chat-dir ../tempo-agent/gui-chat --gui-port 18795
```

`serve` runs the HTTP API the Tempo GUI chat surface uses (`/agent/new`, `send`, `events`, `confirm`, `workflow`, `health`, `convs`, and the second-brain views `memory`, `memory/file`, `memory/graph`), plus a server-sent-events stream at `/agent/stream?conv=ID` that the chat surface uses instead of polling. `--chat-dir` serves the chat surface files at `/chat/`; `--gui-port` passes every other path through to the Tempo GUI server on loopback, so the GUI and the agent share one origin.

- **A key.** Each start writes a fresh random key to `<data>/control.key` (owner-only on Unix). Every request except `GET /agent/health` (which says `"auth":"key"`), the page and its files must send it as `X-Tempo-Key`, or gets 401 with a sign-in hint. The chat page takes it from `#key=...` when Perennos Tempo opens it (`tempo-launch open agent`), keeps it for that tab only and clears it from the address bar; Tempo Room, the Vault's Tempo tab and the launcher send it too. Another program on this computer cannot drive the agent without it.
- Listens on 127.0.0.1 only; another address needs `--bind ADDR --allow-remote` (and the key still applies). The Host header must be `127.0.0.1`, `localhost` or `[::1]` with the port, the address it is bound to, or a name given with `--allow-host` (`NAME:443` also accepts the bare `NAME` that `tailscale serve` sends), and a present Origin (`http://` or `https://`) must be ours, so neither DNS rebinding nor a web page in the browser can drive the agent.
- Only GET and POST. POST bodies must be `application/json` and at most 1 MB.
- Approvals appear in the GUI. `/agent/confirm` resolves only an id this server issued for that conversation, and only an explicit `true` approves; an unanswered approval is denied after `--confirm-timeout` seconds (default 900).
- One worker runs turns in order; reading memory or events never waits for a turn and never creates a conversation. Event text and file excerpts are redacted.

## What it does

- Agent loop: up to 40 steps, history trimmed to a character budget, repeat-call guard (the same call is not re-run), and at the step cap one last request with no tools that asks what is done and what is next.
- Lanes: short plain questions go to a fast model with no tools (it answers `NEEDS_TOOLS` to hand the turn to the heavy lane); anything tool-like, hard or long goes straight to the heavy lane.
- Engine retry: an HTTP error or an in-band `{"error":...}` line (for example a CUDA crash on the first request after a model load) retries the same model up to 3 times, then falls back to the next model in the lane. If all fail the turn ends with "all engines failed".
- Timing of every request (first token, total, load, prompt tokens) is appended to `logs/timing.jsonl` in the data dir.
- Builtin tools: `list_dir`, `read_file`, `write_file`, `trash`, `run_command`, `fetch_url`, `search_memory`, `workflow_update`, `delegate`, `comfy_image`, `edit_file`, `grep`, `glob`.
- MCP: stdio servers from a `{"mcpServers": {"name": {"command", "args", "env"}}}` file. The agent offers all builtins plus up to 8 MCP tools picked by keyword fit to the prompt, named `server__tool`. A server that fails to start or stops later (`ssh zeus tempo-vault mcp` while Zeus is off) is started again at the next turn, at most once a minute, and its tools come back; what it writes to stderr is kept in `logs/mcp-<name>.log` and its last line is in the error.
- RAG: markdown files and globs from the config, chunked on blank lines, embedded with `embeddinggemma` on the CPU only (`num_gpu: 0`), cached in `rag-index.json` per file (reused while the file mtime is unchanged). If embedding fails, keyword search still works.
- Workflow: `workflows/<conv>.md` has `## Task list`, `## Notes` and `## Log`; the last 2500 characters go into the system prompt. Conversations are saved atomically under `conversations/`.

## Safety

- Nothing is ever deleted. `trash` only moves into `<trash_dir>/<YYYYMMDD>/` under a unique name; `edit_file` copies the original into the trash dir before writing; `run_command` refuses `rm`, `del`, `Remove-Item` and similar.
- `write_file`, `trash` and `edit_file` only work inside `write_roots`. Paths that look like secrets are refused, and tool output is redacted and capped at 6000 characters.
- Overwrites, trash moves and every command that is not plainly read-only (`ls`, `cat`, `grep`, `git status` and the like, with no redirection or substitution; `tree`, `date`, `hostname` only bare) ask `y/N` on the terminal. An approval always shows the whole request, never a cut-off one; a request over 20,000 characters is refused. When stdin is not a terminal they are denied. There is no approve-everything switch.
- Text from other people is information, not instructions: the automatic memory never takes in a room's notes (`<vault>/Rooms/`), and the vault marks them untrusted when Tempo reads them.
- `fetch_url` reads only the internet (not this computer, the LAN, the tailnet or the cloud metadata address, unless `fetch_allow_private` names a host), and the first read of each host in a conversation asks the owner ("Tempo wants to read example.com", with the whole address); yes covers that host for the rest of the conversation. `fetch_allow_hosts` lists hosts that never ask (`*.example.com` covers its subdomains). Every redirect is checked the same way.
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
| `prefix_stable` (default `true`) | Keep the system message and the offered tools fixed within a conversation, put per-turn context (date, workflow tail, memory) in that turn's user message, and trim history with headroom. Every request then starts with the previous one, so Ollama reuses its cached prompt and only processes new tokens. `false` restores the original layout. |
| `embed_model`, `rag_files`, `rag_globs`, `pinned_current_files` | RAG sources; `rag_globs` entries are `{"pattern", "newest"}`; pinned files lead the results for "what am I working on" style questions |
| `core_file`, `owner_context` | Text pinned into every system prompt |
| `write_roots`, `trash_dir`, `protect_regex`, `command_cwd`, `command_timeout_s` | Safety and shell settings |
| `offline_probe` | `host:port` for the quick offline check (env `TEMPO_OFFLINE` forces offline) |
| `hosts` | Name to `{"urls": [...], "models": [...], "trust_reads": false}` for `delegate`; an implicit `local` host is the main Ollama. A remote host's model asks before each file it reads here, unless `trust_reads` marks the machine as your own |
| `fetch_allow_hosts` | Hosts `fetch_url` reads without asking (`example.com`, `*.example.com`); others ask once per conversation |
| `fetch_allow_private` | Host names `fetch_url` may reach although they are on this computer or a private network (all of those, and the cloud metadata address, are refused otherwise) |
| `packs_allow` | Names of packs whose connectors may start (a pack supplies programs, so none runs until you list it) |
| `mcp_config`, `mcp_skip`, `mcp_blocked`, `mcp_confirm`, `mcp_trust`, `mcp_hints`, `mcp_*_timeout_s` | MCP settings (`mcp_trust`: pack tools you let run without asking; every pack tool asks first otherwise) |
| `comfy_url`, `comfy_output_dir`, `comfy_start_cmd`, `comfy_stop_cmd`, `lease_status_cmd`, `lease_acquire_cmd`, `lease_release_cmd` | ComfyUI and optional GPU lease commands |

## Test

```
cargo test
cargo clippy --all-targets -- -D warnings
```

Tests use an in-process mock Ollama on `127.0.0.1` and a mock stdio MCP server; they never touch the network beyond loopback.
