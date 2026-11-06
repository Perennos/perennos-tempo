# Perennos Tempo local agent (offline harness), work in progress

The harness that lets a local model (Ollama) work like a frontier coding agent: an agent loop with tool calls, an MCP host that loads the same MCP servers a Claude Code install uses, retrieval over local memory files, delegation to other local engines, a persistent workflow file, repeat-call detection and a step-cap summary.

- `tempo_agent.py`: agent loop and HTTP API used by the Tempo GUI chat surface
- `mcphost.py`: MCP host (stdio servers from the local Claude Code config)
- `ask.py`: CLI client
- `Start-TempoOffline.ps1`: Windows launcher (Ollama + Tempo GUI + agent)
- `gui-chat/`: the CHAT surface module for the Tempo GUI

Private data (memory files, the retrieval index, conversations, workflows) is never committed; it is built locally at run time.
Machine settings (folders, hosts, memory files, owner) come from a local, git-ignored `agent-config.json`; see `agent-config.example.json`.

The Rust port is the crate `perennos-tempo-local-agent/` at the repository root; `tempo agent` runs it. [RUST-PORT.md](RUST-PORT.md) says what is ported, how to build and test it, and what is left.
