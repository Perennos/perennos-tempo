# Perennos Tempo local agent (offline harness), work in progress

The harness that lets a local model (Ollama) work like a frontier coding agent: an agent loop with tool calls, an MCP host that loads the same MCP servers a Claude Code install uses, retrieval over local memory files, delegation to other local engines, a persistent workflow file, repeat-call detection and a step-cap summary.

- `tempo_agent.py`: agent loop and HTTP API used by the Tempo GUI chat surface
- `mcphost.py`: MCP host (stdio servers from the local Claude Code config)
- `ask.py`: CLI client
- `Start-TempoOffline.ps1`: Windows launcher (Ollama + Tempo GUI + agent)
- `gui-chat/`: the CHAT surface module for the Tempo GUI (chats, workflow, and the Memory pane: the second brain, with excerpts and a graph). One Perennos look (`tempo-theme.css`, the same bytes in every Tempo crate; the official wordmark and insignia, the site's fonts), Light / Dark / System, and a Brain / Dashboard / Both switch (keys B, D, Shift+B; remembered). Where to swap the artwork: the `--wm-src` and `--ins-src` lines and the `@font-face` rules at the top of `chat.css`; the insignia is mounted in the one element marked `data-insignia` in `chat.mjs` (the `brand` span), which a live version can replace, with `root.dataset.busy` as the thinking signal and `graph.burst()` / `graph.ripple()` for finishing a run or a reply

The Memory pane reads `GET /agent/memory`, `/agent/memory/file?src=` (indexed files only, redacted) and `/agent/memory/graph` (files as nodes, shared distinctive terms as edges). The graph view, `gui-chat/memgraph.mjs`, is imported only when opened and draws with the vendored three.js when WebGL works, else a flat SVG. `gui-chat/vendor/three.module.min.js` is three.js 0.170.0 unmodified from npm (MIT, `vendor/three.LICENSE`; sha256 08fd7545d13d2c7fb65ab691530a802dafefd638596501854f267d0fb13c39e7). To update it, replace the file and the licence from the `three` npm package and change the version and hash here. Served by the Rust `tempo-local-agent serve`, the surface streams events over `/agent/stream`; with `tempo_agent.py` it polls.

Private data (memory files, the retrieval index, conversations, workflows) is never committed; it is built locally at run time.
Machine settings (folders, hosts, memory files, owner) come from a local, git-ignored `agent-config.json`; see `agent-config.example.json`.

The Rust port is the crate `perennos-tempo-local-agent/` at the repository root; `tempo agent` runs it. Its [README](../perennos-tempo-local-agent/README.md) says how to build, test and run it, and how it relates to the Python harness in this folder (`tempo_agent.py`, `mcphost.py`).
