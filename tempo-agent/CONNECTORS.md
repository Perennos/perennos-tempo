# Tools that connect themselves

Perennos Tempo calls its own products, and any connector you add, as tools. You don't have to
write MCP config. Both the Rust agent (`perennos-tempo-local-agent`, `src/products.rs`) and the
Python harness (`tempo-agent/mcphost.py`, `discover`) follow the same rules.

## Perennos Tempo's own products

Put the product next to the agent binary (or in a folder named by `TEMPO_BIN`, or on PATH), and
the agent finds it at startup. A line per product on stderr says what was connected, or what is
missing.

| Product | Connected as | Arguments | Needs | Asks first |
| --- | --- | --- | --- | --- |
| `tempo-vault` (notes, Obsidian-compatible) | `vault` | `mcp --write --vault <dir>` | `TEMPO_VAULT` or `"vault"` in the config | `vault_write` (a whole-note overwrite), `vault_rename`, `vault_trash` |
| `tempo-studio` (REAPER, mixing, mastering) | `studio` | `mcp --allow-root <dir>...` | `TEMPO_STUDIO_ROOTS` (a path list), `"studio_roots"`, or an existing `~/Music` | nothing (it never overwrites; REAPER opens only when asked for) |
| `tempo-room` (chat rooms) | `room` | `mcp` | nothing | `room_post` (speaking in a room) |

- `"vault_write": false` connects the vault read-only.
- Explicit `mcpServers` entries with the same name always win.
- `TEMPO_AUTO_TOOLS=0`, or `"auto_tools": false`, turns all of this off.

## Plug-in connectors

A connector is one JSON file in `<tempo home>/connectors/`. `<tempo home>` is `TEMPO_HOME`, else
`"tempo_home"` in the config, else `~/.tempo`.

```json
{
  "name": "comfy",
  "command": "~/tools/comfy-mcp",
  "args": ["--port", "8188"],
  "env": {"MODE": "local"},
  "confirm": ["render"],
  "enabled": true
}
```

- `name` is optional (the file name is used); letters, digits, `-` and `_` only.
- `command` is required; a leading `~` is the home folder.
- `args` and `env` are passed as they are, and `confirm` lists the tools that ask before they run.
- `"enabled": false` keeps the file but turns the connector off.
- A connector with the name of a product replaces it.
- A file that is not JSON, or has no command, is skipped with a note; it never stops the agent.

Only add connectors you trust: a connector is a program the agent will run. Keep tokens out of
these files where you can; the program can read its own config instead.

## Packs: setups for a business or a workflow

A pack is a folder in `<tempo home>/packs/<name>/` with its own `connectors/`. For example,
`packs/law-firm/connectors/clio.json`. Every pack is on unless its `pack.json` says
`{"enabled": false}`. This is how a business setup ships its tools without touching anyone else's
config: drop the folder in, or take it out.

## Guest conversations (chat rooms)

`POST /agent/new {"mode": "guest"}` makes a conversation for other people, for example a family
or team chat room through `tempo-room`. `GET /agent/health` says `"guest": true` when the agent
supports it. In a guest conversation:

- no tools are offered, and a tool call the model makes anyway is refused;
- the memory is not searched;
- the system prompt has no owner context, no pinned core file and no workflow file.

The room also denies every approval request it sees, so room messages can never approve anything.
