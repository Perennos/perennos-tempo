# Perennos Tempo local agent (offline harness), work in progress

The harness that lets a local model (Ollama) work like a frontier coding agent: an agent loop with tool calls, an MCP host that loads the same MCP servers a Claude Code install uses, retrieval over local memory files, delegation to other local engines, a persistent workflow file, repeat-call detection and a step-cap summary.

- `tempo_agent.py`: agent loop and HTTP API used by the Tempo GUI chat surface
- `mcphost.py`: MCP host (stdio servers from the local Claude Code config)
- `ask.py`: CLI client
- `Start-TempoOffline.ps1`: Windows launcher (Ollama + Tempo GUI + agent)
- `gui-chat/`: the CHAT surface module for the Tempo GUI (chats, workflow, and the Memory pane: the second brain, with excerpts and a graph)

The Memory pane reads `GET /agent/memory`, `/agent/memory/file?src=` (indexed files only, redacted) and `/agent/memory/graph` (files as nodes, shared distinctive terms as edges). The graph view, `gui-chat/memgraph.mjs`, is imported only when opened and draws with the vendored three.js when WebGL works, else a flat SVG. `gui-chat/vendor/three.module.min.js` is three.js 0.170.0 unmodified from npm (MIT, `vendor/three.LICENSE`; sha256 08fd7545d13d2c7fb65ab691530a802dafefd638596501854f267d0fb13c39e7). To update it, replace the file and the licence from the `three` npm package and change the version and hash here. Served by the Rust `tempo-local-agent serve`, the surface streams events over `/agent/stream`; with `tempo_agent.py` it polls.

Private data (memory files, the retrieval index, conversations, workflows) is never committed; it is built locally at run time.
Machine settings (folders, hosts, memory files, owner) come from a local, git-ignored `agent-config.json`; see `agent-config.example.json`.

The Rust port is the crate `perennos-tempo-local-agent/` at the repository root; `tempo agent` runs it. [RUST-PORT.md](RUST-PORT.md) says what is ported, how to build and test it, and what is left.
