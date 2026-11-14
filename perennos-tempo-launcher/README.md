# tempo-launch

The program behind every Perennos Tempo shortcut. A shortcut names an app; if the app's server is not
answering yet, tempo-launch starts it with no window, waits until it answers, and opens it in the browser.
On Windows it is a windowless program, and the servers it starts get a console nobody sees, which the
programs they start (the agent's tools) share, so nothing ever opens a console window.

```
tempo-launch                 the agent, then the Tempo Field (or Tempo Vault without the Field)
tempo-launch vault [FOLDER]  Tempo Vault on FOLDER (default: TEMPO_VAULT, else Documents\Tempo Vault)
tempo-launch room            Tempo Room, signed in as the owner
tempo-launch field           the Tempo Field
tempo-launch agent           the agent, then its chat page (`#key=` from `<agent data>/control.key`, never in a query, log or error)
tempo-launch studio          Tempo Studio (mixing and mastering): the agent, then its chat page, and a line on how to use Studio
tempo-launch home [--json]   the Tempo folder in use, why, and other Tempo folders with files
tempo-launch home --merge DIR  move DIR's files into the home; never overwrites or deletes
tempo-launch start           everything installed, nothing opened (what sign-in runs)
tempo-launch stop            ends what tempo-launch started
tempo-launch status [--json] what is installed and what runs (and the apps in remote.json)
tempo-launch open ID         opens an app on another machine listed in remote.json
tempo-launch setup [--apply [--yes]] [--with-d1] [--goal chat,code,...] [--json | --page]
                             recommends models for this computer and your goals; with --apply and your yes it
                             fetches what is missing and writes the agent's settings (docs/TEMPO-SETUP.md)
tempo-launch welcome         the first-run page: scan, one or several computers, how to let your AI help (opened once by itself)
tempo-launch link URL        adds another computer's Ollama (private or tailnet address) to ollama_fallbacks
tempo-launch mcp [--scan]    stdio MCP server for your AI (scan, plan, status, propose); --scan prints the scan facts
tempo-launch setup --apply-proposal  shows what your AI's proposal would change; applies it only after y/N
tempo-launch conductor       Tempo Conductor: sets up bands (teams of AI members), runs them and picks the engine for each part; `band` works too
tempo-launch conductor templates  the ready-made bands (social-media, music-release, small-business, dev-team, research, content-creator, study, product-ad, short-film, recast-clip, music-video)
tempo-launch conductor new (--template NAME | --describe "TEXT") [--name BAND] [--yes]
                             sets a band up: shows it in plain words, asks y/N, saves it, adds the members to Room, schedules the flows
tempo-launch conductor run BAND [FLOW] [--note TEXT]
                             walks a flow through its members; each step becomes a linked note in the Vault
tempo-launch conductor sections BAND [FLOW] [--note TEXT] [--takes 1-3] [--resume RUN]
                             plays a big job as sections (sub-teams) and takes (attempts), under a budget (../docs/SECTIONS.md)
tempo-launch conductor runs  the newest sectioned runs: budget used, takes, winners
tempo-launch conductor stop [RUN]  ends a run at its next step; `conductor takes accept|reject TAKE` is your say on a take
tempo-launch conductor room BAND  adds the band's members to Room as teammates (when Room was not running)
tempo-launch conductor engines    every engine you have: up or down, its models, a rough class
tempo-launch conductor page [BAND]
                             writes and opens the Conductor page: engines, bands, last runs, apps
tempo-launch conductor apply-proposal [NAME]
                             shows the band your AI proposed; sets it up only after y/N
tempo-launch learn           one learning pass over the vault (what the hourly task runs)
tempo-launch schedule [--daily | --remove | --print]
tempo-launch fleet           your own computers: list, invite, join, scan, add, remove, ssh, check, setup, allow, rotate, serve, send, inbox, ping, page
tempo-launch logs            opens the logs folder
```

| App | Program | Port | Health |
| --- | --- | --- | --- |
| Tempo Agent | `tempo-local-agent serve` | 8765 | `/agent/health` |
| Tempo Vault | `tempo-vault serve --vault FOLDER` | 8766 (other folders 8780 to 8799) | `/vault/health` |
| Tempo Room | `tempo-room serve` | 8767 | `/room/health` |
| Tempo Field | `tempo-dashboard` | 7878 | `/healthz` |

- Tempo Studio is not in this table because it has no window, server or port of its own: it is a command-line
  program and a tool the agent uses, connected automatically. `tempo-launch studio` therefore does what helps:
  it starts the agent if it is not running (nothing else is started), opens the agent's page exactly as
  `tempo-launch agent` does, and says plainly: "Tempo Studio works through Tempo: ask Tempo to analyse or master
  a file in your Music folder." The line is printed when run in a terminal and also written to `launch.log`
  (a shortcut shows no console). If the agent cannot start, the usual problem page says why, as for `agent`.
  `start`, `stop` and `status` do not list Studio, as there is nothing of it to start, wait for or end.
- An app counts as running only when its own health answer comes back (the vault's must name the folder
  asked for), so another program on the port is never taken for it.
- The programs are looked for next to tempo-launch, then in the folders of `TEMPO_BIN`. `PATH` is never searched, and a relative folder is ignored, so nothing planted elsewhere (or in the current folder) can run as Tempo.
- The Tempo home (`tempo-launch home` shows it and why; `docs/TEMPO-HOME.md` has the rule), `TEMPO_AGENT_DATA`
  (default `<home>/agent`) and `TEMPO_VAULT` are passed on (`TEMPO_HOME` to every app), so the agent connects the vault as a tool. A missing vault folder is made with
  `tempo-vault init`; an existing one, an Obsidian vault included, is opened as it is.
- Logs: `<home>/logs/<app>.log`, plus `launch.log` for
  what tempo-launch did. A log over 8 MB is renamed aside, never deleted.
- `stop` ends only processes tempo-launch started, and only while the process still runs the same program
  (process ids are reused). Its records in `run/` are marked stopped, not removed.
- When an app cannot start after a click, a page says why and shows the last lines of its log, with
  anything that looks like a key hidden (`token=x`, `"token":"x"`, `Authorization: Bearer x`, cookies, long keys). At sign-in and in the scheduled task, problems go to `launch.log`
  instead: nothing pops up.
- `TEMPO_NO_BROWSER=1` prints addresses instead of opening them.

## Setup (`tempo-launch setup`)

Looks at the computer (memory, graphics card, free disk, Ollama, Perennos models already there) and your goals, chooses a model for each job, and prints the plan with the real size and licence of each. The models come from a built-in list of candidate names (`catalog/candidates.json`, never holding sizes or licences) that setup verifies live against the Ollama registry with curl (cached 7 days in `<home>/models/registry-cache.json`, skipped when unverified, offline-safe), the shipped verified entries (`catalog/models.json`, override at `<home>/models/catalog.json`) and what is installed. Installed models whose names start with your preferred prefix are preferred (`--preferred-prefix P` saves it per computer; none by default; `--prefer mainstream` overrides). Without a GPU the thinking lane keeps to 8B active parameters or fewer (a mixture-of-experts model counts its active parameters) unless `--prefer quality`. Nothing is fetched without `--apply` and a yes (or `--yes`); it refuses when the disk is too small, installs nothing system-wide, and fetches through Ollama's own API, hidden, with progress lines. It then fills the keys the user has not set in `<agent data>/config.json` (`models`, `fast_models`, `embed_model`, `ollama_url`), copying the old file to `<home>/trash/<date>/` first. UMBRA models are only detected and used, never fetched; d1 only with `--with-d1`, after its licence (LFM Open License v1.0, free under USD 10 million yearly revenue) and link are shown. When `<agent data>/config.json` exists the launcher starts the agent with `--config` on it (unless `TEMPO_AGENT_CONFIG` is set). The first `start` or bare launch with no settings writes `<home>/setup.html` and opens it once. When no Ollama is on the computer, setup can also fetch Tempo's own pinned copy (`catalog/runtime.json`, SHA-256 checked before it is unpacked with the system's own `tar`, kept in `<home>/runtime`), only after a yes; `start` runs it before the agent and `stop` ends it only by its own pid record. Details: `docs/TEMPO-SETUP.md`.

## Tempo Band (`tempo-launch conductor`)

The Conductor ties the owner to their workflows and to their engines. A *band* is the name for the team it sets up: a small team of AI members for the owner's own workflow (a social media team of researcher, writer, editor and scheduler, say), all on this computer. Each member is a teammate in Room to `@mention` like a colleague, and a flow walks the members one after another and files what they make as linked notes in the Vault. Code: `src/conductor/`. The long version, with the template list and the spec, is in `docs/TEMPO-SETUP.md` under "Tempo Conductor".

- **The spec** is `<home>/bands/<name>/band.json`: `members` (`handle`, `name`, `instrument`, `role` of at most 2000 characters, `model`, optionally `engine`, `may` as `talk` or `draft`) and `flows` (`name`, `goal`, `steps` as handles, `every` as `manual`, `daily HH:MM` or `weekly <day> HH:MM`, `notes_to` as `Bands/<name>/` or a folder inside it). It is checked strictly, on write and again on read: unknown keys, overlong roles, steps that are not members (or only talk), schedules that are not schedules and folders outside the band's own are all refused, with every problem listed in words.
- **Never outside the computer.** Members chat as guests with the agent (no tools, no memory of your files). They never post, send, buy or delete anything; the most they do is the launcher writing their drafts into the Vault, only under the band's notes folder. `may: "talk"` members only answer in Room and cannot be steps.
- **`conductor new`** takes `--template` (eleven are built in, four of them for Stage: Product ad, Short film, Recast a clip, Music video) or `--describe "..."` (the model on this computer fits the closest template to the sentence; its answer is validated and, if it is not usable, the template is used as it is). It always shows the band in plain words and asks y/N (`--yes` skips the question), saves it (an earlier band.json goes to `<home>/trash/<date>/`), adds each member to Room as an `agent` teammate in a room named after the band when Room is running (it prints `tempo-launch conductor room NAME` when it is not), and registers the flows that run by themselves with the system scheduler (a crontab line, or a Windows scheduled task; on a Mac, or when that fails, it prints the line to add).
- **`conductor run`** sends each step the flow goal, the owner's `--note`, and the step before it, and files each answer as `<notes folder>/<date> <flow>/<n> <member>.md` with front matter (`band`, `member`, `flow`, `step`) and `[[links]]` to the step before and to the flow's `Index` note. Without a Vault the notes go to `<home>/bands/<name>/runs/`. Text files in an `Inbox` folder next to the runs (`Bands/<name>/Inbox/` in the Vault) are read by the first step, so there is a place to drop emails, sources or a request.
- **`conductor page`** writes `<home>/conductor.html` (or `<home>/bands/<name>/band.html` for one band): the engines (up or down, with models), the bands (members with instrument, role and engine, flows, schedules), the last runs with links to their notes, and the apps (agent, Vault, Room, Studio) running or not. Static, with the same strict Content-Security-Policy as the welcome page, no script, and a layout that fits a phone.
- **`conductor engines`** reads `ollama_url`, `ollama_fallbacks` and `hosts` from the agent's settings and asks each engine for `/api/tags` (two seconds, private, tailnet and loopback addresses only; any other is refused unread). The Conductor casts heavy members on the strongest reachable engine that has their model and light ones on the quickest, keeps the pick as `engine` in band.json, re-checks at every run, and notes a fallback in the run's `Index` note. When the agent's health says `"engines": true` the Conductor sends the pick as the conversation's `engine`, and the run's `Index` note says "played on <engine>" with the engine the agent reports; the agent moves a part off that engine only when it cannot be reached or lacks the model. An older agent cannot pin (it takes a model, then fails over `ollama_url` then `ollama_fallbacks`), so there the pick is only the engine the Conductor expects to answer, and the note keeps saying the agent decided.
- **The AI's two tools** in `tempo-launch mcp`: `conductor_templates` (read-only) and `conductor_propose`, which writes `<home>/bands/proposals/<name>.json`. The owner applies it with `conductor apply-proposal`, which shows the band in plain words, labels the AI's note as the AI's, and asks y/N (there is no `--yes`).

## Tempo Fleet (`tempo-launch fleet`)

Tempo knows your own computers and talks between them. Code: `src/fleet/`; the long version, including "Add a computer in a minute", is in `docs/TEMPO-SETUP.md` under "Tempo Fleet".

- **Heading.** The fleet page (`fleet page`) and the Conductor page show "One machine, one field." / "Three machines, one field.": the real count in words, never a stock line. Each machine shows live health from its heartbeats (CPU load, memory, models loaded, last heard) and its last ssh check; a switch is the exact command under it (the pages are static). `GET /fleet.json` on the messenger's loopback side gives the GUI the same facts.
- **Add a computer in a minute.** On the main computer `fleet invite` shows a code like `4-tiger-violet` (10 minutes, once, your own network only). On the new one `fleet join` finds the main computer (mDNS and a gentle look), asks "Join <name>?", asks for the code, and sets itself up: Ollama on its private address, `setup --apply --yes`, the messenger. The code drives a SPAKE2 exchange (Ed25519 group), so a short code cannot be guessed offline, and an ML-KEM-768 exchange inside it (hybrid, for quantum computers; see docs/FLEET-SECURITY.md) means a recorded pairing stays closed later; a ChaCha20-Poly1305 key from both carries the fleet key, the two Ed25519 identities (pinned on both sides) and a hardware look. The part follows the hardware: a big card is the engine, a laptop a client, a small box a runner. The Windows installer asks "Is this your main computer, or joining another?"; `/JOIN=code` joins on a silent install, which otherwise joins nothing. A strong single computer is told plainly that it can play on its own.
- **`fleet scan`** finds them: the /24 around each of this computer's own 10/8, 172.16/12 and 192.168/16 addresses, the peers `tailscale status --json` lists (the 100.64/10 range is never swept), and mDNS. A 300 ms connect to Ollama (11434), the agent (8765) and the messenger (8770), at most 64 at once, then one plain GET (no password) to that same private address. Never a public range. It explains itself and asks y/N the first time (`--yes`); it changes nothing.
- **`fleet add|remove|enable|disable`** choose the fleet and write the agent's `ollama_fallbacks` and `hosts` with the `link` private-address check and a backup in `<home>/trash/`. `tempo-launch mcp` gains `fleet_status` (read-only; labels, no names or addresses) and `fleet_propose` (applied by you with `fleet apply-proposal`).
- **ssh, your way.** `fleet ssh|check|setup NAME` use the system `ssh` with `BatchMode=yes`, `ConnectTimeout=5` and `StrictHostKeyChecking=accept-new`: your keys and agent, and Tempo never asks for, types or stores a password or key. `fleet setup` shows the exact remote commands and asks y/N. `fleet hosts` lists your `~/.ssh/config` Host names.
- **The messenger.** `fleet init` makes `<home>/fleet.key` (32 random bytes, 0600); `fleet rotate` replaces it. `fleet serve` listens on private and tailnet addresses and 127.0.0.1 only (never 0.0.0.0); `start` and `schedule` keep it running when the fleet has more than one machine. Frames are length-prefixed with an HMAC-SHA256 from the fleet key and an Ed25519 signature from the sender's pinned identity, a time and a nonce (replays, strangers and impostors are dropped), one persistent connection per peer, batched writes. Messages go through a SQLite (WAL) outbox and inbox, are retried with backoff and kept once by id, so nothing is lost when a laptop sleeps. Topics: `heartbeat`, `note`, `job`, `result`, `call`, `callres`, `blob`, `status`. `fleet send|inbox|ping` are the commands.
- **Tools anywhere.** Each computer offers only what its owner allowed (`fleet allow comfy_image`, `fleet allow mcp:SERVER/TOOL`): ComfyUI and MCP tools ride on the heartbeat, and the agent on the main computer gets them as its own: `comfy_image` runs on this computer's ComfyUI, else on the biggest card in the fleet (the picture returns in checked pieces: "Made on studio-box, 6.2 s."), and remote MCP tools appear as `<node>__<server>__<tool>`. The main computer's plan mode, deny list and questions apply to every remote call, the other computer checks its allow list again, and both write a line to `logs/fleet-calls.log`.
- **Routing.** Whole requests, never a model split across machines: the model already loaded wins, then free graphics memory, then the shortest line, then the measured speed. A wire failure goes once to the next computer; tool calls are never retried.
## The Crate (`tempo-launch crate`)

A bank of 23 free tools (Blender, FFmpeg, ImageMagick, Pandoc, Tesseract, whisper.cpp, Piper and more), each with its licence named, that Tempo can set up with your own package manager and then play. `crate` lists them, `crate add ID` shows the exact command and asks y/N (apt is printed for you to run, never run with sudo), `crate enable ID` lets Tempo use one, and `crate mcp` is the MCP server the agent starts, with fixed arguments and only the folders you allow. Details, the tool list and what was checked: [docs/TEMPO-SETUP.md](../docs/TEMPO-SETUP.md#the-crate-free-tools-tempo-can-set-up-and-play).

## Stage (`tempo-launch stage`)

Stage (video and image workflows) plays ComfyUI workflows on your own machines: re-angle, character swap, effect clips, camera moves, relight, lip sync, with Pedals (modifiers) like camera, lens and light. `catalog/stage.json` pins each workflow to a commit and a SHA-256, `stage add` shows the sizes and asks y/N, looks at each custom node's code first (a high finding needs you to type yes; a paid or cloud node is refused), and `stage mcp` serves each workflow as a tool the agent can call. ComfyUI itself comes from the Crate. See [docs/STAGE.md](../docs/STAGE.md) and [docs/STAGE-SOURCES.md](../docs/STAGE-SOURCES.md).

## Apps on other machines (`remote.json`)

Optional file `remote.json` in the Tempo home. No file means no remote apps. A broken file gives one warning line, never a crash. Nothing is ever started or stopped on another machine.

```json
{"remote": [
  {"id": "vault@tower", "title": "Tempo Vault on Tower", "url": "http://tower:8766",
   "health": "/vault/health", "sign": "\"app\":\"tempo-vault\""},
  {"id": "room@tower", "title": "Tempo Room on Tower", "url": "https://tower.your-tailnet.ts.net",
   "health": "/room/health", "sign": "\"app\":\"tempo-room\""}
]}
```

- `id` is `[a-z0-9@._-]`, at most 64 characters. `url` is `http://` or `https://` with host and optional port only: no user name, no path, no query. `health` is a path starting with `/`. `sign` is text the health answer must hold.
- Allowed places: IPv4 10/8, 172.16/12, 192.168/16, 100.64/10 (the tailnet) and 127/8; IPv6 `fd00::/8` and `::1`; a one-word name (`tower`); names ending in `.ts.net`, `.local`, `.lan`, `.internal`, `.home.arpa`. Anything else is refused with a message, so a mistaken entry never probes or opens the public internet. A name is also checked after it resolves: only a private address is connected to.
- `status` lists the remote apps after the local ones: `answering`, `not answering`, or `not checked (https)`. Plain http only (std has no TLS), 1.5 s, the sign must be in a 200 answer, no cookies or credentials are sent, redirects are not followed. `status --json` adds `remote` and `remote_warnings`.
- `open ID` opens the address with the system opener. The opener still refuses everything except this computer's own pages and files, and now exactly the address of an entry that passed these checks.

std only; `build.rs` gives the Windows program its icon (`TEMPO_LAUNCH_ICON`) and version details when a
resource compiler (`windres` or `rc.exe`) is there. `cargo test` runs 203 tests, among them a start, wait, open
and stop of a stand-in vault, and a crash shown with its last words.
