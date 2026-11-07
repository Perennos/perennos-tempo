# Perennos Tempo local agent

A small offline agent for a local Ollama. It runs a tool loop (builtin file and shell tools, stdio MCP servers, markdown RAG, delegation to other Ollama hosts, ComfyUI images), keeps a persistent workflow file per conversation, and is built to keep working with no internet.

Rust, three dependencies (`serde_json`, `ureq`, `regex`), no async runtime. It uses Ollama's `/api/chat` with streaming and tool calls.

## Run

```
cargo build --release
./target/release/tempo-local-agent --config config.json "list the files in ./notes"   # one turn
./target/release/tempo-local-agent --config config.json                                # line REPL
```

Options: `--config PATH` (or env `TEMPO_AGENT_CONFIG`), `--mcp-config PATH` (or env `TEMPO_MCP_CONFIG`), `--conv ID` to continue a conversation, `-c`/`--continue` for the most recent one, `--reindex` to rebuild the RAG index, `--help`. A new conversation started from the terminal works in the folder you started it in.

MCP servers load only from a file you name. That can be a plain `{"mcpServers": {...}}` file or your Claude Code `~/.claude.json` (`--mcp-config ~/.claude.json`); with the latter, the per-project servers and `.mcp.json` of the folders in env `TEMPO_MCP_PROJECT_DIRS` (path-list separated) load too.

Streamed text goes to stdout, status lines to stderr. Settings come from `--config`, else `TEMPO_AGENT_CONFIG`, else the agent's own file in its data folder: an older `agent-config.json`, else the `config.json` that `tempo-launch setup` writes (the launcher uses the same order). A settings file that is there but cannot be read stops the agent with a message, rather than being skipped. With no file at all it uses safe defaults: Ollama at `http://127.0.0.1:11434`, data in env `TEMPO_AGENT_DATA` or `<Tempo home>/agent`. Env `OLLAMA_URL` (or `TEMPO_OLLAMA`) overrides the Ollama URL.

The Tempo home is found by the one rule every Perennos Tempo product shares (`src/tempo_home.rs`, the same file byte for byte in each product, with the same conformance tests): `TEMPO_HOME` (or the older `PERENNOS_TEMPO_DATA`), else a folder an earlier version used if it exists (`C:\LocalAI\apps\decision-head` on Windows, `$XDG_STATE_HOME/perennos-tempo` or `~/.local/state/perennos-tempo` elsewhere), else `%LOCALAPPDATA%\Perennos Tempo` on Windows and `~/.tempo` elsewhere. Nothing is moved between them. The launcher passes `TEMPO_HOME` to every app it starts; `tempo-launch home` shows which folder was chosen and why.

On Windows, `build.rs` gives `tempo-local-agent.exe` its icon (`assets/tempo-local-agent.ico`) and version details (Perennos Tempo Agent, by Perennos) when a resource compiler (`rc.exe` or `windres`) is there; without one it is built the same, minus those.

Through the repository CLI, `python tempo.py agent "list the files in ~/notes"` (or `tempo.sh` / `tempo.cmd`) runs this binary; it looks in `TEMPO_AGENT_BIN`, then `target/{release,debug}` here, then `PATH`. With no prompt it reads one turn per line from stdin.

## Relation to the Python harness

This crate is the Rust port of `tempo-agent/tempo_agent.py` and `mcphost.py`: the agent loop with its step cap and repeat-call guard, the fast and heavy lanes, engine retry and fallback, MCP hosting, RAG, conversations and the workflow file, and the built-in tools. It adds `edit_file`, `grep`, `glob`, `apply_patch`, prompt reuse (`prefix_stable`), guest conversations for rooms, and automatic product tools. The Python harness stays in `tempo-agent/` and reads its own `agent-config.json` (see `agent-config.example.json` there); the two config files have different keys, and both stay local and uncommitted. An earlier, smaller Rust port in `tempo-agent/src` was retired so there is one port, not two; `git show 30e6d83:tempo-agent/src/agent.rs` shows it.

Still to do:

1. Run `serve` on a machine with the real GUI, real Ollama and real MCP servers, then start the binary instead of `tempo_agent.py` from the launcher scripts.
2. `claude_code`, the Python tool that hands a task to Claude Code on the local engine, has no Rust equivalent yet.
3. Measure the prompt-reuse saving on a large model and with real MCP servers; so far only mocks have been used.
4. CI runs the Rust tests on Linux, macOS and Windows runners for the products; this crate's own tests have only been tried on Linux.

Never commit config files with real paths or hosts, conversations, workflows, logs, `rag-index.json`, memory files or tokens. The CI leak scan rejects token shapes and home-network addresses, so tests build token-shaped strings at run time.

## Serve the Tempo GUI

```
./target/release/tempo-local-agent serve --config config.json --port 8765 \
    --chat-dir ../tempo-agent/gui-chat --gui-port 18795
```

`serve` runs the HTTP API the Tempo GUI chat surface uses (`/agent/new`, `send`, `events`, `confirm`, `workflow`, `health`, `convs`, and the second-brain views `memory`, `memory/file`, `memory/graph`), plus a server-sent-events stream at `/agent/stream?conv=ID` that the chat surface uses instead of polling. `--chat-dir` serves the chat surface files at `/chat/`; `--gui-port` passes every other path through to the Tempo GUI server on loopback, so the GUI and the agent share one origin.

- **A key.** Each start writes a fresh random key to `<data>/control.key` (owner-only on Unix). Every request except `GET /agent/health`, the page and its files must send it as `X-Tempo-Key`, or gets 401 with a sign-in hint. Without the key, health says only `auth`, `stream` and `guest`; with it, the engines too, probed at most every 15 seconds. Listening beyond this computer (`--allow-remote`), the GUI behind the agent needs the key as well. The chat page takes it from `#key=...` when Perennos Tempo opens it (`tempo-launch open agent`), keeps it for that tab only and clears it from the address bar; Tempo Room, the Vault's Tempo tab and the launcher send it too. Another program on this computer cannot drive the agent without it.
- Listens on 127.0.0.1 only; another address needs `--bind ADDR --allow-remote` (and the key still applies). The Host header must be `127.0.0.1`, `localhost` or `[::1]` with the port, the address it is bound to, or a name given with `--allow-host` (`NAME:443` also accepts the bare `NAME` that `tailscale serve` sends), and a present Origin (`http://` or `https://`) must be ours, so neither DNS rebinding nor a web page in the browser can drive the agent.
- Only GET and POST. POST bodies must be `application/json` and at most 1 MB.
- Approvals appear in the GUI. `/agent/confirm` resolves only an id this server issued for that conversation, and only an explicit `true` approves; an unanswered approval is denied after `--confirm-timeout` seconds (default 900).
- **Teammates for Tempo Room.** `POST /agent/new` takes two optional fields:
  - `model`: letters, digits and `: . - _ /`, at most 100 characters. It is tried first, with the configured models as fallbacks; a model the engine does not have falls back at once.
  - `role`: written by the owner, at most 2,000 characters, redacted. It goes into the system message after the rules, never into what anyone types, and is kept with the conversation.
  With `"mode": "guest"`, the conversation is still a guest one: no tools, memory or owner context. Health says `"roles": true`.
- One worker runs turns in order; reading memory or events never waits for a turn and never creates a conversation. Event text and file excerpts are redacted.

## Working in a project

The habits that make Claude Code and Codex good to work with, built in and running on your own machines:

- **Project instructions.** `AGENTS.md`, `TEMPO.md` and `CLAUDE.md` from the top of the project (the folder with `.git`) down to the working folder go into the system prompt, nearer files last, after your own `<Tempo home>/AGENTS.md`. A line `@docs/style.md` brings that file in (one level, inside the project). They say how to work here; they never override the safety rules or your approvals. Capped at 8,000 characters, redacted.
- **`@file`** in a message attaches that file (up to six, 10,000 characters each; secret files never).
- **Commands** (typed in the chat; the model never sees them):

  | Command | What it does |
  | --- | --- |
  | `/cd <folder>` | Work in another folder: commands run there, relative paths start there, its instructions apply |
  | `/mode default`, `accept-edits`, `plan` | How much Tempo may do without asking (below) |
  | `/plan [task]` | Plan mode; with a task, plan it |
  | `/undo` | Put back every file the last changing turn changed; their newer versions go to the trash |
  | `/diff` | What the last changing turn changed |
  | `/think [on\|off]` | Think before answering, with models that can (DeepSeek-R1, Qwen 3 and others); a model that cannot is asked again without it |
  | `/model [name]` | The model for this conversation; the config's stay as fallbacks |
  | `/compact [focus]` | Ask the model for a summary and put it in place of the history |
  | `/clear` | A fresh start; the history so far is kept as its own conversation |
  | `/init` | Look at the folder and write an `AGENTS.md` for it |
  | `/review [what]` | Review the changes in the folder (`git diff`), change nothing |
  | `/usage`, `/status` | Tokens and engine time; folder, mode, model, instructions, background commands |
  | `/stop [bg-N]` | End this conversation's background commands, all of them or one |
  | `/name args` | Your own: `<project>/.tempo/commands/name.md` or `<Tempo home>/commands/name.md`, with `$ARGUMENTS` and `$1`..`$9` |
  | `!command` | Run a command yourself in the folder (the deny list and the no-delete rule still hold); its output joins the conversation |

  Rooms' guests have none of these: what they type is only text.
- **Modes.** `default` asks before every change. `accept-edits` writes and edits files inside `write_roots` without asking (commands, the trash and new websites still ask). `plan` offers only the tools that read, refuses the rest, and asks the model for a numbered plan instead of changes. `permission` in the config sets the mode new conversations start in.
- **Checkpoints.** Before a turn first changes a file, what it held is kept, byte for byte, in `<data>/checkpoints/<conversation>/<turn>/`. After a turn that changed files, a status line lists them (`a.rs +3 -1, notes.md (new) +12`); `/diff` and `/undo` work from there. Nothing is deleted on the way: an undone version and a file the turn created both go to the trash.
- **`apply_patch`** changes several files in one step in the patch format Codex uses (`*** Begin Patch`, `*** Add File:`, `*** Update File:` with `*** Move to:`, `*** Delete File:`, `@@` hunks). Each hunk is found by its lines (exactly, then ignoring trailing spaces, then indentation); the patch applies whole or not at all; line endings are kept; a deleted file goes to the trash and asks first. Paths may not contain `..` and are resolved through links before they are checked, and the checked path is the one written; a patch never writes over a file that is not text (at most 200 files a patch).
- **Command lists.** `command_allow` lists commands that run without asking (`cargo test`, `npm run lint`), matched word for word on every part of a command with no redirection or substitution, followed only by arguments that are not options (`cargo test parser`, not `cargo test --config ...`, which could swap the compiler); an entry ending in ` *` allows any arguments. Once Tempo has changed a file in the conversation, allowed commands ask again: a changed build script or `package.json` can make `cargo test` or `npm test` run anything. `command_deny` lists ones that never run (`git push`, `docker`).
- **Background commands.** `run_command` with `background: true` starts a command and returns at once (a dev server, a long build or test run), after the same approvals and refusals as any command. `command_output` shows the end of what it printed (the last 64 KB are kept) and whether it still runs, and can wait up to a minute for it to finish; `command_stop` ends it and everything it started. At most four run at once, a conversation sees only its own, and all of them end when the agent stops. What a background command starts ends with it: on Linux and macOS it runs as its own process group, on Windows in a job, which Windows ends even if the agent crashes. A program it leaves running (`prog &`) keeps it counted as running while that program holds its output, so it stays visible and stoppable.
- **Hooks.** Commands of yours that run around Tempo's steps, set in the agent's settings file only (never by a project's files or a pack):

  ```json
  "hooks": {
    "before_tool": [{"match": "run_command|apply_patch", "command": "python3 ~/tempo-hooks/check.py"}],
    "after_tool": [{"match": "write_file|edit_file|apply_patch", "command": "cargo fmt"}],
    "after_turn": [{"command": "git status --short"}]
  }
  ```

  `match` is a regex the whole tool name must match (connector tools are named `server__tool`); without one a hook runs for every tool. A hook runs in the conversation's folder with `TEMPO_HOOK_EVENT` set to JSON (the event, the conversation, the folder, the mode; the tool and its arguments; the result after a tool; the files changed and the reply after a turn) and `TEMPO_CONVERSATION`. A `before_tool` hook that exits with 2 stops the call, and what it printed is what the model reads instead; a hook that fails another way is noted and the call goes on, as in Claude Code. What an `after_tool` hook prints joins the result the model reads (a formatter's or a linter's findings), and what an `after_turn` hook prints is a status line. `before_tool` hooks also gate the reads a `delegate` model asks for. Plan mode runs only `before_tool` hooks; guest rooms run none. The deny list and the no-delete rule hold for hooks too, and each may run for `timeout_s` seconds (`hook_timeout_s`, 30 by default, at most 600).
- **Instructions are the owner's.** Writing `AGENTS.md`, `TEMPO.md`, `CLAUDE.md`, anything in `.tempo/commands/`, or a file an instruction file imports always asks, in every mode and even for a new file, so one conversation cannot quietly give orders to the next. The Tempo home's `AGENTS.md`, `commands/`, `connectors/` and `packs/` are never written by Tempo's tools, nor is anything inside a `.git` folder (git's config and hooks run programs). Instruction, import and command files are read only when the file behind any link is inside the project and has no secret's name; an instruction file follows at most 10 imports.
- **Tool calls written as text.** Local models often write a call as text instead of making it: Hermes and Qwen `<tool_call>{...}</tool_call>`, DeepSeek's `<｜tool▁sep｜>name` with a JSON block, or a reply that is only `{"name": ..., "arguments": {...}}`. Tempo runs those as the calls they are, for offered tools only.
- **Trims leave notes.** When the history is trimmed to its budget, a short note of what was dropped (each request, each tool call, the start of each answer) takes its place, so the thread of the work survives without an extra model call; `/compact` asks for a real summary.
- **Fresh reads.** The repeat guard (the same call is not run twice in a turn) forgets earlier reads after anything that may change files, so a check after an edit reads the file as it is now.

## What it does

- Agent loop: up to 40 steps, history trimmed to a character budget, repeat-call guard (the same call is not re-run), and at the step cap one last request with no tools that asks what is done and what is next.
- Lanes: short plain questions go to a fast model with no tools (it answers `NEEDS_TOOLS` to hand the turn to the heavy lane); anything tool-like, hard or long goes straight to the heavy lane.
- Engine retry: an HTTP error or an in-band `{"error":...}` line (for example a CUDA crash on the first request after a model load) retries the same model up to 3 times, waiting about twice as long each time (with a little jitter), then falls back to the next model in the lane. If all fail the turn ends with "all engines failed".
- Engine failover, as a load balancer does it: `ollama_fallbacks` lists your other machines' Ollama addresses. When an engine cannot be reached at all (refused, no route, no answer within 8 seconds) the request moves to the next one and a status line says so; an engine that failed twice in a row is passed over for 30 seconds, then tried again. A conversation stays on the engine that last answered it (its prompt is cached there), and the timing line names the engine when it is not the main one. An engine's own error (a model it does not have, a crash) is not a reason to move.
- Web pages, as a CDN keeps them: pages `fetch_url` read are kept in `cache/fetch/` (addresses without their queries; a cached page from a redirect is served only to a conversation that may read the redirect's target too) (up to `fetch_cache_max`, 500 by default; a full cache takes no new pages rather than delete any). A page is fresh for the site's own `max-age` (five minutes when it says nothing, a day at most); after that the next read asks the site with its `ETag` or `Last-Modified`, and a `304` costs nothing. A site that says `no-store` is never kept. While the internet or the site is down, the copy read before is served with its age said plainly ("this is the copy read 2 hours ago").
- Every `fetch_url` read is logged, as a proxy logs its traffic: `logs/egress.jsonl` has the time, the conversation, the address without its query, its user-info or path parts that look like tokens, and what happened (read, cached, refused, denied, failed).
- Timing of every request (first token, total, load, prompt tokens) is appended to `logs/timing.jsonl` in the data dir.
- Builtin tools: `list_dir`, `read_file`, `write_file`, `trash`, `run_command`, `fetch_url`, `search_memory`, `workflow_update`, `delegate`, `comfy_image`, `edit_file`, `grep`, `glob`, `apply_patch`, `command_output`, `command_stop`.
- MCP: stdio servers from a `{"mcpServers": {"name": {"command", "args", "env"}}}` file. The agent offers all builtins plus up to 8 MCP tools picked by keyword fit to the prompt, named `server__tool`. A server that fails to start or stops later (`ssh tower tempo-vault mcp` while Tower is off) is started again at the next turn, at most once a minute, and its tools come back; what it writes to stderr is kept in `logs/mcp-<name>.log` and its last line is in the error.
- RAG: markdown files and globs from the config, chunked on blank lines, embedded with `embeddinggemma` on the CPU only (`num_gpu: 0`), cached in `rag-index.json` per file (reused while the file mtime is unchanged). If embedding fails, keyword search still works.
- Workflow: `workflows/<conv>.md` has `## Task list`, `## Notes` and `## Log`; the last 2500 characters go into the system prompt. Conversations are saved atomically under `conversations/`.

## Safety

- Nothing is ever deleted. `trash` only moves into `<trash_dir>/<YYYYMMDD>/` under a unique name; `edit_file` copies the original into the trash dir before writing; `run_command` refuses `rm`, `del`, `Remove-Item` and similar.
- `write_file`, `trash` and `edit_file` only work inside `write_roots`. Paths that look like secrets are refused, and tool output is redacted and capped at 6000 characters.
- Overwrites, trash moves and every command that is not plainly read-only (`ls`, `cat`, `grep`, `git status` and the like, with no redirection or substitution; `tree`, `date`, `hostname` only bare) ask `y/N` on the terminal. An approval always shows the whole request, never a cut-off one; a request over 20,000 characters is refused. When stdin is not a terminal they are denied. There is no approve-everything switch.
- Text from other people is information, not instructions: the automatic memory never takes in a room's notes (`<vault>/Rooms/`), and the vault marks them untrusted when Tempo reads them.
- `fetch_url` reads only the internet (not this computer, the LAN, the tailnet or the cloud metadata address, unless `fetch_allow_private` names a host), and the first read of each host in a conversation asks the owner ("Tempo wants to read example.com", with the whole address); yes covers that host for the rest of the conversation. `fetch_allow_hosts` lists hosts that never ask (`*.example.com` covers its subdomains). Every redirect is checked the same way.
- The address is checked again at the moment of connecting, by the resolver the connection itself uses, so a name that answers with a public address for the first look and a private one for the connection (DNS rebinding) gets nowhere. The special-use ranges (documentation, benchmarking, reserved, NAT64, 6to4 and Teredo wrappers of private addresses) count as not the internet. Through a proxy (`HTTPS_PROXY`), the proxy looks names up; an address written out is still checked, and the owner still approves each host. A name lookup gives up after 5 seconds.
- Commands that print what files hold (`cat`, `grep`, `Get-Content` and the like) ask when they name a secret (`control.key`, `~/.ssh/...`), and when a wildcard or a search through folders reaches the agent's own data folder.
- Plain `http://` addresses are tried over `https://` first (a page read over plain http can be changed on the way, and could carry instructions); the plain one is read only when the site does not answer on https, and the result says so.
- TLS uses the built-in list of authorities, or the bundle in `SSL_CERT_FILE` when it names one (as for curl, Python and OpenSSL), so a company's own authority or a TLS-inspecting proxy works.
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
| `write_roots`, `trash_dir`, `protect_regex`, `command_cwd`, `command_timeout_s` | Safety and shell settings (`command_cwd` is where a conversation works until `/cd`) |
| `command_allow`, `command_deny` | Command prefixes that run without asking, and ones that never run |
| `permission` | The mode new conversations start in: `default`, `accept-edits` or `plan` |
| `hooks`, `hook_timeout_s` | Your commands before and after tools and after turns (see Hooks); a hook that cannot be understood stops the agent from starting rather than be skipped |
| `offline_probe` | `host:port` for the quick offline check (env `TEMPO_OFFLINE` forces offline) |
| `hosts` | Name to `{"urls": [...], "models": [...], "trust_reads": false}` for `delegate`; an implicit `local` host is the main Ollama. A remote host's model asks before each file it reads here, unless `trust_reads` marks the machine as your own |
| `fetch_allow_hosts` | Hosts `fetch_url` reads without asking (`example.com`, `*.example.com`); others ask once per conversation |
| `fetch_cache_max` | Pages kept for later reads (default 500; 0 keeps none) |
| `ollama_fallbacks` | Your other machines' Ollama addresses, tried in order when `ollama_url` cannot be reached |
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
