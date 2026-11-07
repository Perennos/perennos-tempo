# Perennos Tempo Vault

Our own Obsidian, in Rust: a folder of plain Markdown notes with links, backlinks, a 3D graph,
search, daily notes, templates, tasks, history for every change and an app that looks like the
rest of Perennos Tempo. Plus what Obsidian does not do: Perennos Tempo learns into it every day,
reads it through MCP, and every change by anyone (you, Obsidian, the agent) is kept.

One binary, `tempo-vault`, about 4 MB (5 MB on Windows), one dependency (`serde_json`). Nothing leaves
the machine.

## Start

```bash
cargo build --release --manifest-path perennos-tempo-vault/Cargo.toml
tempo-vault init "<vault folder>"            # new vault, or adds .tempo/ settings to an Obsidian vault
tempo-vault serve --vault "<vault folder>"   # then open http://127.0.0.1:8766/
```

`--vault` can be left out when `TEMPO_VAULT` is set. An existing Obsidian vault works as it is:
the notes, `.obsidian/daily-notes.json` and `.obsidian/templates.json` are used, and nothing in
`.obsidian/` is changed.

## Same as Obsidian

| Feature | Notes |
| --- | --- |
| `[[wikilinks]]`, `[[Note#Heading]]`, `[[Note#^block]]`, `[[Note\|alias]]`, `![[embeds]]` | Resolved like Obsidian: same folder first, then path, then shortest path, then aliases, then attachments |
| Markdown links `[text](Folder/Note.md)` | Relative paths, `%20` names |
| Properties (YAML frontmatter), `aliases`, `tags` | Text and list values; set from the CLI, MCP or by editing |
| `#tags`, nested `#a/b` | A parent tag counts its children |
| Tasks `- [ ]`, `- [x]`, other statuses, due dates `📅 2026-11-07` or `due:: 2026-11-07` | Click to check them off anywhere |
| Callouts `> [!tip]`, foldable `> [!note]-` | All Obsidian types |
| Tables, footnotes (`[^1]` and `^[inline]`), `==highlight==`, `~~strike~~`, `%%comments%%`, code blocks | Raw HTML is shown as text (only `<br>` is kept) |
| Math: `$e^{i\pi}+1=0$` and `$$...$$` | Typeset by the browser itself (MathML, no script): fractions, roots, scripts, Greek, operators and arrows, sums and integrals with limits, `\left ... \right`, accents, `\text`, `\mathbb{R}` and the other alphabets, matrices, cases, aligned lines. The TeX is kept for copying |
| Diagrams: ```` ```mermaid ```` | Flowcharts (every node shape, edge style and label, subgraphs), state diagrams, sequence diagrams (actors, notes, loops, alternatives, numbering) and pie charts, drawn on the server as SVG: no script, so they also work on published pages |
| Embedded search: a ```` ```query ```` block | Live results |
| Bases: `.base` files and ```` ```base ```` blocks | Table, cards and list views; `filters` (`and`, `or`, `not`), `formulas`, `properties` with `displayName`, `order`, `sort`, `groupBy`, `limit`, `image`. `![[Projects.base#Board]]` embeds one view |
| Canvas: `.canvas` files (JSON Canvas 1.0) | Text, note, file, link and group cards; arrows with sides, ends, colours and labels; pan, zoom, snap, resize, connect, colours, groups, undo. `![[Board.canvas]]` draws it in a note. Fields the editor does not know are kept |
| Dataview: ```` ```dataview ```` blocks | `LIST`, `TABLE` (with `AS` and `WITHOUT ID`) and `TASK`; `FROM #tag`, `"folder"`, `[[note]]` with `and`, `or`, `-`; `WHERE`, `SORT`, `GROUP BY`, `LIMIT` |
| Search operators | `"phrase"`, `-not`, `a OR b`, `word*`, `tag:#x`, `path:x`, `file:x`, `task:x`, `task-todo:`, `task-done:`, `[prop]`, `[prop:value]` |
| Backlinks with context, outgoing links, unlinked mentions, outline, local graph | Right-hand panel |
| Graph view | three.js on the GPU (SVG without WebGL); tags, attachments, wanted notes, orphans, filter |
| Daily notes, templates with `{{date}}`, `{{time}}`, `{{title}}`, `{{date:dddd D MMMM YYYY}}` | Obsidian's settings are read when Tempo has none |
| Quick switcher and commands | `Ctrl+K` (also `Ctrl+P`, `Ctrl+O`) |
| Rename or move | Every link to the note is rewritten, in every note |

## More than Obsidian

- **Tempo learns every day.** `tempo-vault learn` turns each Perennos Tempo conversation into a
  note (summary, decisions, to-dos, links to the notes it talks about), adds a line to that day's
  daily note, and keeps `Tempo/Tempo Learned.md` up to date: recent conversations, recurring
  topics, open to-dos. A local model summarizes when you name one (only on this machine or the home
  network); otherwise an extractive summary is used. It only redoes conversations that changed,
  never overwrites a note you edited (new points go under "Updates"), and redacts secrets first.
  Reviewed teach rows from workflows (`<data>/workflows/teach/`, as `tempo workflow teach`
  writes them) become "Taught" day notes, once each. `tempo-vault schedule --install` runs it
  every hour (a hidden scheduled task on Windows, cron elsewhere), followed by Tempo Learn's daily
  cycle (`tempo-learn run --dry-run`, found next to `tempo-vault` or on PATH; if it fails, that is
  a warning in the run log, never a failure of the vault's learning).
- **The agent reads and writes it.** `tempo-vault mcp` gives Perennos Tempo (or any MCP client)
  search, read, backlinks, neighbours, tags, tasks and daily-note tools; `--write` adds create,
  append, set property, check task, rename and trash. Every write is redacted and kept in history.
- **Ask Tempo, beside the note.** The Tempo tab talks to the Perennos Tempo agent on this
  computer: ask about the open note, or tap Summarize, Links, Tasks or Related. Answers stream in
  and then read like a note (their `[[links]]` open notes), tool calls show as they happen, and
  when Tempo wants to change something it asks first, showing the whole request with Approve and
  Deny.
- **Boards you can move.** A base view with `type: board` and `groupBy: status` lays the notes
  out in lanes (`lanes: [Todo, Doing, Done]` fixes their order and keeps empty ones). Drag a card
  to another lane, or focus it and press Alt+← or Alt+→, and the note's property changes.
  Double-click a cell of a table to change that property in the note (links in it are kept), and
  click a ✓ to flip a yes/no property.
- **Canvases that know your notes.** A note on a canvas (as a card or a `[[link]]` in one) lists
  the canvas among its backlinks, so it is never called an orphan. "Map this note on a canvas"
  lays out a note with its links and backlinks around it. Drop a picture on the board and it
  becomes an attachment card; drop a `[[link]]` and it becomes a note card. Tempo reads a canvas
  as an outline (groups, cards, arrows) and can write one; a canvas that would not open is never
  saved.
- **Dates that count.** `date(due) - today()` is a number of days; `due + "2w"`, `today + 7` and
  `date(x) + "1 month"` are dates.
- **Tempo queries it like a database.** The `vault_query` MCP tool runs a Dataview query, Bases
  YAML or a `.base` file and answers in Markdown tables; `tempo-vault query "TABLE status FROM
  #project SORT due"` does the same in a terminal (`--json` for data).
- **Nothing is lost.** Every change is snapshotted first (`.tempo/history/`), written atomically,
  and logged (`.tempo/changes.jsonl`). A note changed on disk while you edit it is never
  overwritten silently: you choose. Trash moves to `.trash/<date>/`; nothing is deleted.
- **Live.** Changes made by Obsidian, an editor or the agent show up in the app within two seconds.
- **Big vaults stay fast.** The graph is laid out in Rust (Barnes-Hut), not in the browser.
- **`tempo-vault check`** lists wanted notes, broken embeds, orphans, names that clash, and names
  Windows cannot store.
- **`tempo-vault publish --out DIR`** writes a static site of the notes marked `publish: true`.
  A published page is rendered from the published notes alone, so an embed, a query, a task list
  or a base on it never shows text from a note that is not published.

## The app

Charcoal field, ivory ink, italic serif titles, mono labels, red-orange for what is active, as in
the Field GUI. Keys when not typing: `F` files, `S` or `/` search, `G` graph, `T` tasks, `H` tags,
`D` today, `M` calendar, `L` learned, `C` check, `N` new note, `E` edit, `P` present, `I` side
panel, `,` theme
(charcoal, ink, paper). `Ctrl+E` read or edit, `Ctrl+S` save now, `F2` rename, `Esc` back to
reading. Type `[[` while editing to link a note.

## Small things

- **Paste or drop** an image or any file into a note while editing: it is saved as an attachment
  and embedded where the cursor is. It goes where Obsidian would put it (`attachmentFolderPath`
  in `.obsidian/app.json`), else in `Attachments/`, and never over an existing file. A pasted
  screenshot is named `Pasted image <date and time>.png`, as in Obsidian.
- **Lists that keep going**: Enter continues `- `, `- [ ] ` and `1. ` (counting up). Enter on an
  empty item ends the list, and Tab or Shift+Tab indents or outdents the lines.
- **Hover a link** to a note and the note shows beside it; Esc or moving away hides it.
- **`P` presents** the open note as slides, split at its `---` lines (or at each heading when
  there are none): arrows or space to move, `F` for full screen, Esc to leave.
- **`M` is a calendar** of daily notes: a dot where one exists, a line on days you edited notes;
  a day without one starts it (with your daily template).
- **Check (`C`)** opens with your half year in notes, a square a day, with your longest run.
- **"Open this note in Obsidian"** in the command palette, for when you want Obsidian's plugins.
- **`?`** shows every key. **`R`** opens a random note. **`Z`** is focus mode: just the page.
- **On this day**: a daily note shows the same day in earlier years. The status bar keeps your
  daily-note streak, and long notes show their reading time.
- A greeting by time of day, and a quiet signature in the browser console.
- Tap **`B`** to a beat and Tempo tells you the tempo (stay near 120 bpm and it says you are
  right in tempo). And there is the code everyone tries first (↑ ↑ ↓ ↓ ← → ← → B A).

## Command line

```
tempo-vault init [DIR] | index | check | version
tempo-vault search QUERY [--limit N] [--json]
tempo-vault read PATH [--html] | new PATH [--text T | --template NAME] | append PATH TEXT [--heading H]
tempo-vault daily [--date YYYY-MM-DD] [--append TEXT] [--heading H]
tempo-vault rename FROM TO | trash PATH | history PATH | restore PATH ID
tempo-vault links PATH | backlinks PATH | mentions PATH | tags | tasks [--rules R] | orphans | unresolved
tempo-vault query "TABLE status, due FROM \"Projects\" WHERE status != \"Done\" SORT due" | query --base FILE [--json]
tempo-vault graph [--center PATH] [--depth N] [--tags] [--unresolved] [--no-attachments] [--filter Q] [--out FILE]
tempo-vault publish --out DIR [--all]
tempo-vault serve [--port 8766] [--bind 127.0.0.1] [--read-only] [--allow-host H] [--watch SECS]
tempo-vault mcp [--write]
tempo-vault learn [--agent-data DIR] [--model NAME] [--ollama URL] [--dry-run] [--since DAYS] [--max N] [--with-model-cycle]
tempo-vault schedule [--every hourly|daily] [--print | --install | --remove]
```

Task rules (`tasks --rules`, the ```` ```tasks ```` block, `vault_tasks`), one per line:
`not done` (default), `done`, `all`, `due before|after|on DATE` (or `today`, `tomorrow`,
`yesterday`), `due today`, `has due date`, `no due date`, `path includes x`, `tag includes x`,
`text includes x`, `limit n`.

## Connect it to Perennos Tempo

Add the vault to the agent's MCP servers (read-only; add `--write` to let Tempo write):

```json
{"mcpServers": {"vault": {"command": "<path to>/tempo-vault", "args": ["mcp", "--vault", "<vault folder>"]}}}
```

`learn` finds the agent's data in the Tempo home (`<home>/agent`), found by the one rule every
Perennos Tempo product shares (`src/tempo_home.rs`, the same file byte for byte in each product):
`TEMPO_HOME` (or the older `PERENNOS_TEMPO_DATA`), then a folder an earlier version used if it
exists, else `%LOCALAPPDATA%\Perennos Tempo` on Windows and `~/.tempo` elsewhere. Reviewed teach rows come from
`<agent data>/workflows/teach/` and `<home>/workflows/teach/`. To point it elsewhere, set it once
in `<vault>/.tempo/config.json`:

```json
{"daily_folder": "Daily", "templates_folder": "Templates", "agent_data": "<tempo agent data folder>", "learn_model": "<fast lane model, optional>"}
```

Then `tempo-vault schedule --install --vault "<vault folder>"`. The environment variables
`TEMPO_VAULT`, `TEMPO_AGENT_DATA`, `TEMPO_LEARN_MODEL`, `OLLAMA_URL`, `TEMPO_HOME` and
`TEMPO_UTC_OFFSET` win over the config file; flags win over both.

On Windows, `build.rs` gives `tempo-vault.exe` its icon (`assets/tempo-vault.ico`) and version
details (Perennos Tempo Vault, by Perennos) when a resource compiler (`rc.exe` or `windres`) is
there; without one it is built the same, minus those.

Behind `tailscale serve` (HTTPS on 443), pass `--allow-host <name>:443`: the bare name the browser
sends then counts as ours, and so does its `https://` origin. A vault bound to one address (a
tailnet IP) also accepts that address as its Host.

## Security

- `serve` binds 127.0.0.1. The Host header must be ours (no DNS rebinding), a cross-origin page
  cannot read or write (Origin and `Sec-Fetch-Site` checks; JSON-only POST bodies; OPTIONS
  refused), bodies are capped at 1 MiB, and `--read-only` refuses every change. Served on any
  address other than loopback, the vault is read-only unless you also pass `--allow-writes`.
- The app runs under a strict Content Security Policy (no inline script or style). Note text is
  always HTML-escaped; links work only for http(s) and mailto. Attachments are served under a
  sandboxing policy, and an `.html` attachment is served as text.
- Paths are always vault-relative and cleaned: no `..`, no absolute paths, no hidden folders, no
  Windows device names (`CON`, `NUL`, `COM1`...). Reads and writes never follow a symlink or
  junction out of the vault. `publish` leaves out HTML and scripted SVG attachments.
- The Tempo tab reaches the agent only through this server, server to server, and only five
  calls (health, new, send, events, confirm). Every call but health carries Tempo's key, which the
  page gets from Perennos Tempo (`#key=...` when the launcher opens the vault, kept for that tab
  only); this server passes it on and never reads Tempo's key file itself, so a program on this
  computer cannot reach the agent through the vault. It works only when the vault is served on loopback
  and not read-only, for a request from loopback under a loopback Host name with no proxy
  headers: a vault shared on the network or behind `tailscale serve` cannot reach the agent.
  `TEMPO_AGENT_URL` moves the agent, but only to another port on this computer.
- `schedule --install` quotes the vault and program paths for cron and `schtasks`, and refuses a
  path that holds a line break or a quote.
- `learn` sends text only to a model on this machine or the home network (loopback, private or
  tailnet addresses), after redaction.

## Files Tempo adds to a vault

`.tempo/config.json` (settings), `.tempo/history/<path>/<time>.md` (versions),
`.tempo/changes.jsonl` (who changed what, when), `.tempo/learn.json` and `learn-runs.jsonl`
(learning state), `.trash/<date>/` (trashed notes). All hidden from the vault's own notes; all
safe to keep in a backup.

## Numbers

Release build, a synthetic vault of 5,000 notes (865,000 words, 25,000 links), cloud Linux CPU:
cold open and index 0.39 s; search 2 to 10 ms per query through the server; open a note with its
backlinks 0.9 ms; render 0.7 ms; first graph with layout 2.3 s, then 57 ms (an edit that changes
no link keeps its layout); peak memory 109 MB. With Bases, canvases and diagrams added (November 2026): open,
index and a Dataview `TABLE ... WHERE ... SORT ... LIMIT` from the command line 0.4 s, with 20
canvases of 50 cards each linked into the index.

## Not done (yet)

Plugins, editing inside the reading view, and Dataview's `FLATTEN` and DataviewJS are not
supported. Mermaid's gantt, class, entity, journey, git and mind-map diagrams show as code. A
link card on a canvas shows the address, not the page (pages from other sites are never framed).
