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
tempo-launch home [--json]   the Tempo folder in use, why, and other Tempo folders with files
tempo-launch home --merge DIR  move DIR's files into the home; never overwrites or deletes
tempo-launch start           everything installed, nothing opened (what sign-in runs)
tempo-launch stop            ends what tempo-launch started
tempo-launch status [--json] what is installed and what runs (and the apps in remote.json)
tempo-launch open ID         opens an app on another machine listed in remote.json
tempo-launch setup [--apply [--yes]] [--with-d1] [--goal chat,code,...] [--json | --page]
                             recommends models for this computer and your goals; with --apply and your yes it
                             fetches what is missing and writes the agent's settings (docs/TEMPO-SETUP.md)
tempo-launch learn           one learning pass over the vault (what the hourly task runs)
tempo-launch schedule [--daily | --remove | --print]
tempo-launch logs            opens the logs folder
```

| App | Program | Port | Health |
| --- | --- | --- | --- |
| Tempo Agent | `tempo-local-agent serve` | 8765 | `/agent/health` |
| Tempo Vault | `tempo-vault serve --vault FOLDER` | 8766 (other folders 8780 to 8799) | `/vault/health` |
| Tempo Room | `tempo-room serve` | 8767 | `/room/health` |
| Tempo Field | `tempo-dashboard` | 7878 | `/healthz` |

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
resource compiler (`windres` or `rc.exe`) is there. `cargo test` runs 143 tests, among them a start, wait, open
and stop of a stand-in vault, and a crash shown with its last words.
