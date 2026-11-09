# Perennos Tempo sets itself up: `tempo-launch setup`

Plain English. Perennos Tempo configures itself for each computer and each person's goals, and recommends models instead of shipping them. The code is in `perennos-tempo-launcher/src/setup/`.

## What it does, in one minute

1. It looks at the computer: how much memory, whether there is a graphics card and how much memory it has, how much disk is free, whether Ollama is running and what it already holds.
2. It works out what you want Tempo for (your goals).
3. It checks a list of candidate models live against the official Ollama registry, then chooses a model for each job from the ones that checked out (and the ones already installed), and says why in one line each, with the real size and licence.
4. It shows what would have to be downloaded, with sizes and your free disk. **Nothing is downloaded** unless you add `--apply` and say yes (or add `--yes`).
5. With your yes it downloads only what is missing, through Ollama, then writes the agent's settings so the agent uses those models.

Run it as often as you like. A second run changes nothing that is already right.

```
tempo-launch welcome                        write and open the first-run page (scan, one or several computers, AI helper)
tempo-launch link URL                       add another computer's Ollama (private or tailnet address) to ollama_fallbacks
tempo-launch mcp [--scan]                   the setup helper as a stdio MCP server for your AI; --scan prints the scan facts
tempo-launch setup --apply-proposal         show what your AI's proposal would change, ask y/N, then write it
tempo-launch setup                          show the plan, change nothing
tempo-launch setup --apply                  show the plan, ask y/N, fetch what is missing, set up the agent
tempo-launch setup --apply --yes            the same without the question
tempo-launch setup --with-d1                include d1 (see below)
tempo-launch setup --preferred-prefix P    save P as this computer's preferred model prefix ("" clears it)
tempo-launch setup --prefer mainstream      ignore your preferred models (or --prefer preferred)
tempo-launch setup --prefer quality         on a computer without a GPU, allow bigger, slower thinking models
tempo-launch setup --goal chat,code         add goals (chat, code, notes, music, rooms, private)
tempo-launch setup --json                   the plan as JSON for other programs
tempo-launch setup --page                   only write the plan page (<Tempo home>\setup.html)
tempo-launch setup --ollama [--yes]         only fetch Tempo's own Ollama (checked against its pinned SHA-256), no models
```

## What it checks

| What | How |
| --- | --- |
| System and CPUs | the operating system, the architecture, the number of logical CPUs |
| Memory (total and free) | Linux: `/proc/meminfo`. Mac: `sysctl hw.memsize` and `vm_stat`. Windows: the system call `GlobalMemoryStatusEx` |
| Graphics card | `nvidia-smi` on any system (name and memory). A Mac with Apple Silicon shares its memory with the graphics chip. Anything else: "no dedicated GPU", and Tempo plans for the CPU, because Tempo works without a big GPU |
| Free disk | in the Tempo home: `df` on Linux and Mac, `GetDiskFreeSpaceExW` on Windows |
| Ollama | is it answering on this computer (`/api/version`), which models it holds (`/api/tags`), is the program installed |
| Perennos's own models | an Ollama model with `umbra` or `d1` in its name, or a `.gguf` file under `<Tempo home>/models/` |

Every program it runs is hidden on Windows (no windows pop up) and has a time limit. If a check fails, that fact is shown as "unknown": nothing is guessed.

## What it wants Tempo for (goals)

Goals come from what is installed next to `tempo-launch` (the agent means chat, the vault means notes, Room means rooms, Studio means music) plus anything you name with `--goal`. With nothing at all, the goals are chat and notes.

- chat, code, notes, private: a thinking model. With `code`, a model that is good at code is preferred.
- music, rooms: only the quick lane and memory.
- Always: a quick-answer model and a memory model.

## How it chooses

Jobs: **quick answers** (the agent's fast lane), **thinking and code** (the heavy lane), **memory** (notes search; always on the CPU), and with `--with-d1`, **routing and judging**.

1. **Budget.** With an NVIDIA card, its memory minus 1 GB for the screen. Without one, your RAM minus what Windows and your apps need (a quarter of RAM, never less than 3 GB). Apple Silicon uses its one shared pool, counted the same way as RAM.
2. **Fit.** A model fits when its download size plus 1.5 GB (room to think) is inside the budget. The catalog shows that as `min_ram_gb` and `min_vram_gb`.
3. **Card first.** With a card, models that fit its memory come first. If none does, the plan falls back to the CPU and says so.
4. **Yours first.** UMBRA-2 on the computer is used for quick answers; UMBRA-3 for thinking, if it fits. Then one of your preferred installed models that fits (see below). Then a model that is already installed, unless a model that would have to be downloaded is much bigger (more than 1.5 times the parameters). Then the biggest that fits; when two are equally big, the earlier one in the candidate list. The quick lane only takes downloads of 3 GB or less. A code-only model is used for thinking only when you asked for the `code` goal.
5. **Unknown size is never chosen by size.** If a model's size or the computer's memory is unknown, that model is not recommended on size grounds.

Every choice is one line with the size and licence and where they came from, for example: `16 GB RAM, no GPU: qwen3:14b (9.3 GB) for thinking and code, runs on CPU; will be fetched. Licence: Apache License 2.0. Verified with the Ollama registry today.` (the numbers in that example are test values). `--json` carries the same: `licence`, `verified`, `gb` for each pick and each fetch, and a `registry` block with the counts.

### Candidates, verified live with the Ollama registry

Tempo ships a list of **candidate names** (`perennos-tempo-launcher/catalog/candidates.json`, built in): about 47 popular Ollama library models across sizes and jobs (qwen3, qwen3-coder, qwen2.5-coder, gemma3, gemma3n, llama3.2, llama3.1, phi4, phi4-mini, mistral, mistral-small, deepseek-r1, gpt-oss, granite, smollm2, and the memory models embeddinggemma, nomic-embed-text, mxbai-embed-large, qwen3-embedding). The list holds a name, the jobs, the parameter count from the tag and a one-line description. **It never holds a size or a licence.** Those come from the registry, on your computer, when you run setup:

- For each candidate `family:tag` setup fetches `https://registry.ollama.ai/v2/library/<family>/manifests/<tag>`. The size is the sum of the layer sizes (model plus any picture-reading layer). The licence is read from the first lines of the licence layer (`/blobs/<digest>`), for example "Apache License 2.0", "MIT", "Gemma Terms of Use", "Llama 3.2 Community License". If it cannot tell, it says "see the licence at https://ollama.com/library/<family>".
- A candidate the registry does not know, or that does not answer, is **skipped**. Nothing unverified is ever recommended or fetched, so a wrong or missing tag is harmless.
- std has no TLS, so it runs `curl` (on Windows `C:\Windows\System32\curl.exe`, hidden, no window), with a time limit per request, only `https`, a size cap, and no redirects for manifests. The licence text is the one exception: the registry hands it to its storage host, so that one request follows up to 3 redirects, https only. If that is blocked, the licence reads "see the licence".
- The lookups run 8 at a time and stop after 20 seconds in all. What did not answer in time is skipped, and setup says how many.
- Results are cached for 7 days in `<Tempo home>/models/registry-cache.json`, so the next run is instant and works offline. A recommendation then says "From the cache, N days old". A failed lookup (no network) is never cached. The first-start page never uses the network: it uses the cache only.
- **No curl, or offline:** setup still works. It plans only from the models already installed here (for those it uses Ollama's own size) and the verified entries that ship with Tempo, and says so in plain words.

The built-in verified catalog (`catalog/models.json`) still holds the entries copied from the verified public catalog (`qwen3:4b`), plus `embeddinggemma`, d1-2B, UMBRA-2 and UMBRA-3. When the registry answers for one of them, the registry's size and licence replace the shipped ones. To add a model on a machine, put a file in the same shape at `<Tempo home>/models/catalog.json`: its entries replace entries of the same name and add new ones. **The live lookups have not been run against the real registry from the build machine** (its network blocks it); they run on your computer when you use setup.

### Speed on a computer without a GPU

Without a usable graphics card the thinking lane is ranked by speed first. Each candidate has `active_params_b`: the parameters used per answer. A dense model uses all of them; a mixture-of-experts model uses far fewer (qwen3:30b about 3.3B, qwen3-coder:30b 3.3B, gpt-oss:20b 3.6B, gpt-oss:120b 5.1B). By default only models with **8B active parameters or fewer** are eligible; among those the one with the most total parameters that fits the memory budget wins. So a 16 GB computer gets an 8B dense model, and a 32 GB computer gets qwen3:30b (fast on a CPU) instead of a dense 14B. The plan line says why, for example "mixture of experts, about 3B active, so it runs fast on CPU".

- With a graphics card (or Apple Silicon) the rule is unchanged: the biggest that fits the card's memory. If the card is too small and the plan falls back to the CPU, the CPU rule applies.
- `--prefer quality` lifts the 8B limit on a CPU: the biggest model that fits, which answers more slowly. The plan says so.
- Your own UMBRA and your preferred installed models are not limited by this.

### Your preferred models

You can tell setup which of your installed models you prefer, by the start of their names: `tempo-launch setup --preferred-prefix mine-` saves `mine-` for this computer in `<Tempo home>/setup.json` (`TEMPO_PREFERRED_PREFIX` overrides it; `--preferred-prefix ""` clears it). Out of the box no prefix is set and nothing is preferred.

With a prefix set, an installed Ollama model whose name starts with it is preferred for the quick lane (the smallest such model that fits) and the thinking lane (the biggest that fits), after UMBRA and before anything else. Setup **never fetches** these from anywhere; it only uses what you installed. Their size is Ollama's own and their licence is shown as unknown.

- `--prefer preferred` (the default when such models are installed): as above. With none installed, or no prefix set, it says so and recommends the usual models.
- `--prefer mainstream`: your preferred models are left out for this plan, and setup says it did that.
- With no prefix set, or none installed, the default changes nothing.

## UMBRA stays on your computers

UMBRA-2 and UMBRA-3 are Perennos's own. Setup only **detects and uses** them where they already are. It never downloads them, never uploads them and never copies them anywhere. They are marked "local-only" in the catalog and cannot be put on a fetch list (a test checks this). An UMBRA `.gguf` file found under `<Tempo home>/models/` is reported, but Ollama cannot load LFM3 models, so it is not put into the agent's settings: it runs on its own llama.cpp service.

## d1 is optional, and has a licence

d1-2B is Liquid AI's public model. Perennos uses it locally to route and to judge work.

- It is **only** included with `--with-d1`.
- Its licence is the **LFM Open License v1.0**: free use only while your yearly revenue is under **USD 10 million**. Setup prints that, and the link to the licence text, before it asks about fetching. Read it before you say yes.
- It is fetched with `ollama pull hf.co/LiquidAI/d1-2B-GGUF:Q4_K_M` (about 1.10 GB; pinned to revision `4a181094d1b6addfaa3a0956f78e7ab2aa406a0c`, SHA-256 `adca5789ad30ae6027e8e936749bb9f19673097492ab26fa1c5b69643ca50b4b`). The digest Ollama reports is printed and saved in `<Tempo home>/setup/applied.json`. Whether Ollama can run this model has not been tried on a real machine; if the pull fails the message says so and nothing else is affected.

## Consent and safety

- No download without `--apply` and a yes (the `y/N` question, or `--yes`).
- Before asking it shows the total size and your free disk. If the downloads plus 1 GB do not fit, it stops: "Not enough free disk".
- If Ollama is not installed, Perennos Tempo can fetch its own copy, but only after you say yes (see "When there is no Ollama" below). It never installs anything system-wide. If Ollama is installed but not running, setup starts it hidden and waits up to 20 seconds; if that does not work it tells you to open it.
- It only talks to Ollama on this computer (`127.0.0.1`, port 11434, or the port in `OLLAMA_URL`). The outside addresses it contacts are `registry.ollama.ai` (through curl, read-only, to check sizes and licences) and, only if you say yes to Tempo's own copy of Ollama, `github.com` (the official Ollama release file, below); nothing about you is sent.

## When there is no Ollama

If no Ollama answers on this computer and no `ollama` program is installed, setup used to stop there. Now Perennos Tempo can fetch its own copy of Ollama, one pinned release (version 0.40.6) listed in `perennos-tempo-launcher/catalog/runtime.json`. This happens only with `--apply` and your yes (the `y/N` question, or `--yes`). Plain `tempo-launch setup`, `setup --json` and `setup --page` only show the plan and fetch nothing.

The plan says exactly what would happen: the file name, the Ollama version, its size, the address it comes from, the SHA-256 it must match, the free disk, and the folder it would go to. The free disk must cover about 2.5 times the download, because unpacking needs room. If this computer has nothing Tempo can fetch (an operating system or chip Ollama does not publish for), the plan says so plainly and points to ollama.com.

| Computer | File | Unpacked with |
| --- | --- | --- |
| Windows, Intel or AMD | `ollama-windows-amd64.zip` | `%SystemRoot%\System32\tar.exe -xf` |
| Windows, Arm | `ollama-windows-arm64.zip` | `%SystemRoot%\System32\tar.exe -xf` |
| Mac | `ollama-darwin.tgz` | `/usr/bin/tar -xzf` |
| Linux, Intel or AMD | `ollama-linux-amd64.tar.zst` | `tar --zstd -xf` |
| Linux, Arm | `ollama-linux-arm64.tar.zst` | `tar --zstd -xf` |

What happens after your yes, in this order:

1. **Fetch.** curl downloads the file into `<Tempo home>/runtime/downloads/<file>.part`, over https only, with a long time limit and resume: if the link drops, run setup again and it carries on from the part. No window opens on Windows.
2. **Check.** Perennos Tempo computes the SHA-256 of the file itself (no other program is trusted for this) and compares it with the pinned one **before anything else touches the file**. If it does not match, the file is moved to `<Tempo home>/trash/<date>/` (never deleted), setup refuses, and says so. Nothing is unpacked or run.
3. **Unpack.** With the computer's own `tar`, into `<Tempo home>/runtime/ollama-0.40.6.partial`. Tempo finds the `ollama` (or `ollama.exe`) program inside, then renames the folder to `<Tempo home>/runtime/ollama-0.40.6`. An existing folder is never overwritten, and a leftover `.partial` folder is moved to the trash first. On Linux this needs the `zstd` program; if it is missing, setup says: install zstd (for example sudo apt install zstd), or install Ollama from ollama.com, then run setup again. Perennos Tempo never installs system packages itself.
4. **Record.** It writes `<Tempo home>/runtime/ollama.json` (product "Perennos Tempo", purpose "owned-ollama-runtime", version, sha256, file, the program's path and when it was installed), written whole or not at all.
5. **Run.** Setup starts it right away, so the model downloads that follow use it. The verified download stays in `runtime/downloads/`; it is yours to move to the trash if you want the space back.

From then on Perennos Tempo runs its own Ollama like its other apps. It is called "Ollama (Tempo's own copy)" in `tempo-launch status`.

- `tempo-launch start` starts it before the agent, but only when the record exists and no Ollama answers. It runs `ollama serve` with `OLLAMA_HOST=127.0.0.1:<port>` (11434 unless you set another port in `OLLAMA_URL`), hidden, with its output in `logs/ollama.log`. `OLLAMA_MODELS` is not touched, so models are kept where a normal Ollama keeps them.
- **It runs lean.** Tempo's own Ollama starts with `OLLAMA_FLASH_ATTENTION=1` and `OLLAMA_KV_CACHE_TYPE=q8_0` (flash attention is what lets the cache be quantised, and q8 halves the memory a chat's history takes for very little loss), `OLLAMA_NUM_PARALLEL=1` (on a personal computer memory grows with parallel requests times context) and `OLLAMA_MAX_LOADED_MODELS=2` (the heavy model and the quick one). On a Mac only flash attention is set and the cache stays f16: some Apple machines ran slower with a quantised cache. Change any of them with an `ollama_env` object in the agent's settings (for example `{"ollama_env": {"OLLAMA_NUM_PARALLEL": 2}}`); a variable already set in your environment beats both. None of this touches an Ollama you run yourself: for that one, `tempo-launch setup` and the welcome page print a single line with the same variables, and it is yours to set.
- `tempo-launch stop` ends it only by the process id it wrote down when it started it, never by name.
- An Ollama that Perennos Tempo did not start is never stopped, replaced or taken over. If one answers on the port, Tempo uses it as it is, and `status` says "another Ollama answers" instead of calling it Tempo's own.
- If you install Ollama yourself later, remove nothing: the installed one is used as usual and Tempo's own copy stays where it is.

## The agent's settings

After the downloads, setup writes the agent's settings file in `<agent data>` (`TEMPO_AGENT_DATA`, else `<Tempo home>/agent`). That is `config.json`, or an older `agent-config.json` when there is one, because the agent reads that one first:

| Key | Set to |
| --- | --- |
| `models` | the thinking model |
| `fast_models` | the quick model (left out when it is the same model) |
| `embed_model` | the memory model |
| `ollama_url` | `http://127.0.0.1:11434` (or your port) |
| `configured_by`, `configured_at`, `configured_values` | a record of what setup wrote |

- It only **fills keys you have not set**. A key you set yourself is kept, and setup says which.
- Keys setup wrote earlier are refreshed on a later run, but only if you have not edited them since.
- Only models that are installed (or just fetched) go in. A model that failed to download is left out.
- The old file is **copied to `<Tempo home>/trash/<date>/`** under its own name before it is replaced. Nothing is deleted. A file that is not valid JSON is left exactly as it is.
- The agent finds its settings by itself: `--config`, else `TEMPO_AGENT_CONFIG`, else `agent-config.json`, else `config.json`. `tempo-launch` follows the same order when it starts the agent.
- When `TEMPO_AGENT_CONFIG` names a file, that file always wins, so setup leaves it alone. Setup then prints the lines to add to it, ready to paste.

## At install and at first start

- **Windows installer:** an Ollama box ("Download Ollama, the engine Tempo runs models with"), ticked by default only when no Ollama is on the computer; ticking it is the consent, and it runs `tempo-launch setup --ollama --yes` hidden (fetches and checks only Ollama, never a model). A separate, unticked models box ("Download models that fit this computer now") runs `tempo-launch setup --apply --yes` hidden when an Ollama is there (installed, or just set up by the Ollama box). Otherwise Setup only runs `tempo-launch setup --page`, which leaves the plan in `<Tempo home>\setup.html`.
- **Linux and Mac installer:** `install.sh --setup` runs `tempo-launch setup`, then asks y/N before `--apply`; `--yes` skips the question.
- **First start:** when there are no agent settings yet, `tempo-launch` (or `tempo-launch start`) writes `<Tempo home>\setup.html` if it is not there yet (the plan in plain words, the exact command to apply it and the d1 licence note) and opens the welcome page once (see First run). Nothing is fetched. A marker file `setup.page-opened` keeps it from opening again.

## First run

The first time Tempo starts (no agent settings yet), it scans this computer and opens `<Tempo home>/welcome.html` once. Open it again with `tempo-launch welcome`. The page is static, works offline, loads nothing and runs no script. It has three parts:

1. **What this computer can run**, from the same scan and plan as `setup` (memory, graphics card, free disk, Ollama), in plain words, with the models that fit.
2. **One computer or several?** The page cannot take a click, so each answer shows its exact command. *Just this computer*: `tempo-launch setup --apply`. *Several computers*: run that on each; on the engine (a home server or GPU box) use Ollama from ollama.com and let it listen on its tailnet or home-network address (`OLLAMA_HOST`); on the computer you sit at run `tempo-launch link http://<engine address>:11434`. `link` adds the address to `ollama_fallbacks` in the agent's settings after the same private-address check as `remote.json` (10/8, 172.16/12, 192.168/16, 100.64/10, loopback, `.ts.net`, `.local`, `.lan` and similar; never a public address), uses port 11434 when none is given, keeps the old file in `<Tempo home>/trash/<date>/` and contacts nothing. Tempo then uses its own Ollama first and moves to the next address when one cannot be reached. A file named by `TEMPO_AGENT_CONFIG` is left alone: the line to add is printed.
3. **Let your AI help you set up.** Copy-paste entries for Claude Desktop (`claude_desktop_config.json`), Claude Code (`claude mcp add tempo-setup -- <path>/tempo-launch mcp`) and other clients that start a local program. ChatGPT cannot start a local program, so for it the page offers `tempo-launch mcp --scan`, whose output you paste yourself.

### The setup helper: `tempo-launch mcp`

A stdio MCP server (JSON-RPC 2.0, one message per line). Tools:

| Tool | Does |
| --- | --- |
| `setup_scan` (read-only) | machine facts only: system, CPU and graphics card class, memory, free disk, whether Ollama is present |
| `setup_plan` (read-only) | the recommended models and runtime, with size, licence and why; nothing is fetched |
| `tempo_status` (read-only) | which Tempo apps are installed and running, whether settings exist, how many other computers are linked, whether a proposal is waiting |
| `setup_propose` | writes a proposed settings change to `<Tempo home>/proposed-settings.json` and returns the command for you to run |
| `conductor_templates` (read-only) | the ready-made bands and the rules a band follows (see "Tempo Conductor" below) |
| `conductor_propose` | writes a proposed band to `<Tempo home>/bands/proposals/<name>.json` and returns the command for you to run; `conductor apply-proposal` shows it and asks y/N |

**What is shared.** Only the scan facts and the plan. Never files, notes or chats, and no user name, folder, computer name or address (a test checks this). What the tools answer becomes part of your conversation with that AI, so it reaches that AI's provider. It is opt-in: nothing connects unless you add the entry to your AI, and Tempo never calls out to any AI. Remove the entry to stop.

**The AI never applies anything.** `setup_propose` accepts only `models`, `fast_models`, `embed_model`, `ollama_url` and `ollama_fallbacks` (private, tailnet or local addresses only). You apply it with `tempo-launch setup --apply-proposal`: it shows each change as `-` and `+` lines, shows the AI's note labelled as the AI's, and asks y/N (there is no `--yes`). On yes it writes the settings (old file to the trash) and moves the proposal to `<Tempo home>/setup/`. The agent reads settings when it starts, so run `tempo-launch stop` and `tempo-launch start` afterwards.

## Tempo Conductor: your workflows, your engines

Setup chooses the models; the Conductor puts them to work. The Conductor ties you to your workflows and to your engines: it sets up your bands, raises the baton when one runs, and cues each member on the engine that suits the part. (`tempo-launch band ...` still works and means the same; so do the older tool names `band_templates` and `band_propose`.) A *band* is the name for the team of members: a small team of AI members set up for your own workflow, all on your own computer. For a social media account it is a researcher, a writer, an editor and a scheduler. You can talk to each member in Room like a person (`@writer`), and a *flow* walks them one after another and files what they make as linked notes in your Vault, so the second-brain graph shows the chain.

**The rule.** No member ever posts, sends, buys or deletes anything outside the computer. Members chat as guests with the local agent, with no tools and no access to your files. The most they do is write drafts: the launcher files each answer as a note, only under the band's own folder in the Vault. A member whose `may` is `talk` only answers in Room; one whose `may` is `draft` can be a step in a flow.

### Start one

```
tempo-launch conductor templates
tempo-launch conductor new --template social-media
tempo-launch conductor new --describe "I run a bakery's Instagram and want posts planned weekly" --name bakery-posts
```

The templates are `social-media`, `music-release`, `small-business` (inbox, invoices and follow-ups, all as drafts), `dev-team` (planner, builder, Soundcheck reviewer, release notes), `research`, `content-creator` and `study`, and four for Stage (video and image workflows): `product-ad`, `short-film`, `recast-clip` and `music-video`; each has 3 to 5 members and 1 or 2 flows. The Stage ones write the shot list and the exact `stage run` lines for you to run; their members get no tools. With `--describe`, the model on this computer picks the closest template by the words in your sentence and rewrites the roles to fit your work. Its answer is checked like any other spec; if it is not usable, you get the template as it is, and Tempo says so.

`conductor new` always shows the band in plain words first: who is in it, what each member does, what the flows are, when they run and where the notes go. Then it asks y/N (`--yes` skips the question). On yes it:

1. saves `<Tempo home>/bands/<name>/band.json` (an earlier one goes to `<Tempo home>/trash/<date>/`);
2. if Room is running, makes a room named after the band and adds each member as an `agent` teammate (through Room's owner calls, with the control key Room writes in its data folder; only `127.0.0.1` is ever called). If Room is not running it prints `tempo-launch conductor room <name>` to run later. A handle that is already a teammate with a different role (another band's, say) is left alone and reported: rename that member in `band.json` and run `conductor room` again;
3. registers each `daily` or `weekly` flow with the system scheduler, as a crontab line on Linux or a scheduled task on Windows, each running `tempo-launch conductor run <band> <flow>` with no window. On a Mac, or when the scheduler refuses, it prints the line to add yourself.

### The spec: `<Tempo home>/bands/<name>/band.json`

```json
{
  "version": 1,
  "name": "bakery-posts",
  "about": "Plan, write, edit and schedule a week of social posts, as drafts you publish yourself.",
  "members": [
    {"handle": "writer", "name": "Quill", "instrument": "Piccolo", "role": "You are the writer of a small social media team...", "model": "qwen3:8b", "may": "draft"}
  ],
  "flows": [
    {"name": "weekly-plan", "goal": "Plan next week's posts.", "steps": ["writer"], "every": "weekly mon 09:00", "notes_to": "Bands/bakery-posts/"}
  ]
}
```

A band has 2 to 8 members and 1 to 4 flows. `handle` is 2 to 24 lowercase letters, digits or hyphens; `instrument` is one word; `role` is at most 2000 characters (Room and the agent take the same, and Tempo adds a short house rule to it when it fits); `model` comes from the plan above (empty means the agent's usual model); `engine` is optional and is the Conductor's pick for the member (see "Engines" below). `every` is `manual`, `daily HH:MM` or `weekly <mon..sun> HH:MM`. `notes_to` must be `Bands/<name>/` or a folder inside it. The spec is checked strictly when it is written and again whenever it is read, so a hand-edited file is held to the same rules: unknown keys, a step that is not a member (or only talks), a folder outside the band's own, an overlong role and a schedule that is not one are each refused, and every problem is listed in words.

### Run a flow

```
tempo-launch conductor run bakery-posts weekly-plan
tempo-launch conductor run bakery-posts one-post --note "the new rye loaf"
```

Each step is a guest conversation with that member's role and model. It is told the flow's goal, your `--note`, and what the step before it wrote. Each answer becomes a note, `Bands/<band>/<date> <flow>/<n> <member>.md` (dates are UTC), with front matter (`band`, `member`, `flow`, `step`) and `[[wiki links]]` to the step before and to the flow's `Index` note. Without a Vault, the notes go to `<Tempo home>/bands/<band>/runs/`. If a member cannot answer, the flow stops there and the `Index` note says so; what was written stays.

Material for a flow goes in `Bands/<band>/Inbox/` (or `<Tempo home>/bands/<band>/Inbox/` without a Vault): text and Markdown files there are read by the first step, so it is the place to drop emails, sources or a request. Members cannot open a mailbox or the internet; they work from what you give them.

### The dashboard

`tempo-launch conductor page` writes and opens `<Tempo home>/conductor.html`, one place to see everything: the **Engines** (up or down, with their models), the **Bands** (members with their instrument, role and engine, the flows and their schedules), the **Last runs** with links to their notes in the Vault, and the **Apps** (whether the agent, Vault, Room and Studio are running). `conductor page <band>` writes `<Tempo home>/bands/<band>/band.html` with just that band. Like the welcome page it is static, with a strict Content-Security-Policy and no script; it loads nothing, and it fits a phone.

### Engines: who plays each part

The Conductor knows every engine you have: this computer's Ollama (or Tempo's own), every address you added with `tempo-launch link` (the agent's `ollama_fallbacks`), and every address of a `hosts` entry in the agent's settings. `tempo-launch conductor engines` asks each one for its models (a quick `GET /api/tags`, two seconds at most, only at private, tailnet or loopback addresses, exactly as `link` allows; any other address is refused without a connection) and shows whether it is up, its models with their sizes, and a rough class from the biggest one: strong (20B and up), capable (7B and up) or light.

When a band is set up, and again whenever a flow runs, the Conductor casts the members. The heavy ones (writer, planner, builder and their kind) go to the strongest reachable engine that has their model; the light ones (scheduler, sorter) go to the quickest one that has theirs. The choice is saved per member as an optional `engine` field in `band.json` (an id from `conductor engines`, checked against your engines whenever the file is read). If the engine chosen is down when a run starts, or has lost the model, the next best one plays the part, and the run's `Index` note says so ("engine box is down, so local played it").

What the agent allows: when its health says `"engines": true`, a conversation can be pinned to one of the agent's own engines. The Conductor sends the engine's address with the member's model and role; the agent asks that engine first, and moves on to its usual order only when the engine cannot be reached or does not have the model. After each step the agent says which engine answered, and the run's `Index` note says "played on <engine>" for that member. With an older agent (no `engines` in its health) the Conductor sends only the model, as before: the agent tries `ollama_url` first and then `ollama_fallbacks`, the note says the agent decided, and the engine named for the member is the one the Conductor expects to answer. Engines are never started or changed, and nothing about them leaves your network.

### Let your AI design the band with you

`tempo-launch mcp` has two more tools for the AI you already use: `conductor_templates` (read-only: the ready-made bands and the rules) and `conductor_propose`, which writes `<Tempo home>/bands/proposals/<name>.json` and returns the command for you. Nothing is set up until you run `tempo-launch conductor apply-proposal [NAME]`: it shows the band in plain words, shows the AI's note labelled as the AI's, and asks y/N (there is no `--yes`). A proposal may say `fast` or `heavy` for a member's model, or leave it out; Tempo fills in this computer's models when you apply it, and the Conductor picks each member's engine then (an `engine` written in a proposal is dropped). The proposal is checked strictly again when it is read.

## Tempo Fleet: your own computers, playing together

Tempo knows the computers you own and talks between them. The heading on the fleet page and in the Conductor's Fleet section is never a stock line. It counts the machines you really have, in words: "One machine, one field.", "Three machines, one field." (this computer plus every machine you left switched on).

One strong computer needs none of this. `tempo-launch setup` tells you plainly when yours can play on its own (a big enough graphics card and enough memory for the thinking model it picked), and then the advice is: stay a one-machine band. A fleet is optional, and one command away.

### Add a computer in a minute

One computer is the main one: it runs the Tempo app. The others join it.

1. On the main computer: `tempo-launch fleet invite`. It shows a short code like `4-tiger-violet`. The code works once, for 10 minutes, and only on your own network.
2. On the new computer, with Tempo installed: `tempo-launch fleet join`. It finds the main computer by mDNS (`_tempo._tcp`) and a gentle look around your network, says "Join <name>? [y/N]", then asks for the code. (`--controller ADDRESS` skips the search; `--code` and `--yes` make it unattended.) The Windows installer and the welcome page ask the same thing: "Is this your main computer, or joining another?". A silent install joins nothing unless you give it `/JOIN=code`.
3. That is all. The new computer sets itself up: it starts Ollama on its private address (when it is the engine), runs `tempo-launch setup --apply --yes`, and starts the fleet messenger (kept running by `tempo-launch start` and `tempo-launch schedule`). It shows up on the main computer with the part its hardware suits: a computer with a graphics card of 8 GB or more (or an Apple chip with 16 GB or more) is the **engine**, with its tools; a laptop is a **client**; a small box (under 8 GB of memory, or 4 threads or fewer) is a **runner**. The Conductor sees an engine at once. The agent reads its settings when it starts, so restart Tempo (`tempo-launch stop`, then `tempo-launch start`) to let the agent use its models too.

How the code stays safe. The code is the password of a SPAKE2 exchange (the Ed25519 group; `spake2` from RustCrypto), so a short code cannot be guessed offline: anyone listening learns nothing they can test guesses against. Each try is one live attempt, and three wrong tries burn the invitation. SPAKE2 is elliptic-curve maths, which a quantum computer would break, so pairing is hybrid: the SPAKE2 key seals a second exchange, ML-KEM-768 (FIPS 203; the `ml-kem` crate from RustCrypto, pure Rust), and the key that carries everything after is HKDF-SHA-256 over both secrets, so recording a pairing today and attacking it later gets nothing. There is no pairing without the ML-KEM half. That key seals what follows (ChaCha20-Poly1305): the new computer's identity and a look at its hardware go in; the fleet key and the main computer's identity come out. [docs/FLEET-SECURITY.md](FLEET-SECURITY.md) has every piece in one table, what a quantum computer does to each, and what is not encrypted. Each side then pins the other's identity, an Ed25519 key per computer in `<home>/fleet-identity` (readable only by you), and every frame between them is signed by it.

What is shared, and what is not. Shared with the main computer, once, at joining: this computer's name, its Ed25519 public key, and a hardware look (system, CPU threads, memory, graphics card class and memory, free disk, whether Ollama is there and which models, whether ComfyUI answers, the names of the MCP servers). Never shared: files, notes, chats, passwords, MCP server settings or keys. Shared with this computer by the main one: the fleet key and its name and public key. After that, each computer only sends what the fleet's topics carry (heartbeats, notes, steps, tool calls) and offers nothing until its owner allows it (see "Tools anywhere").

If you would rather use your own ssh, `fleet setup NAME` (below) does the same job over your keys.

### Finding them: `tempo-launch fleet scan`

What a scan does. It looks at the addresses on this computer's own home or office networks (the /24 around each of its 10/8, 172.16/12 and 192.168/16 addresses), at the computers `tailscale status --json` lists when Tailscale is installed (the 100.64/10 range is never swept: only the peers it lists), and at mDNS (`_tempo._tcp`, `_ollama._tcp`). At each address it tries to open a connection, for 300 milliseconds, to Ollama's port (11434), the Tempo agent's (8765) and the fleet messenger's (8770), at most 64 at a time. Where something answers it reads one public page (`/api/tags`, the agent's unkeyed health, or the messenger's `/hello`). It prints each computer with its address, its name (mDNS, tailscale, the messenger's own, or reverse DNS), what it runs (Ollama with its models and a rough class, a Tempo agent and its version) and whether it is already in the fleet.

What it does not do. It never scans a public range, link-local addresses or the whole of 100.64/10. It sends no password and no file, and changes nothing. The first time it explains this and asks y/N (`--yes` skips the question). Finding a computer does not add it.

### Choosing them: `fleet`, `add`, `remove`, `enable`, `disable`

`tempo-launch fleet` lists the fleet. `fleet add ADDRESS|NAME [--role engine|agent|both] [--name NAME]` puts a computer in; an engine goes into the agent's `ollama_fallbacks` and `hosts` (so `delegate` can name it and the Conductor can cast on it), through the same private-address check as `tempo-launch link`, with the old settings copied to `<home>/trash/<date>/` first. A public address is refused and nothing is written. `fleet disable NAME` takes a computer out of the settings and out of the heading but keeps it listed; `fleet enable NAME` puts it back; `fleet remove NAME` forgets it. When `TEMPO_AGENT_CONFIG` names the settings file, Tempo leaves it alone and prints the lines to add. `<home>/fleet.json` holds each machine's name, address, role, enabled flag, pinned identity, last heard, last probe, last ssh check, last heartbeat and what it offers, and the list of things this computer lets the others run (`allow`).

The AI you already use can see the fleet and propose changes through `tempo-launch mcp`: `fleet_status` (read-only: the count, and for each other computer a label such as `machine-2`, its role, whether it answers, its load and free memory; no names, no addresses) and `fleet_propose` (writes `<home>/fleet-proposal.json`). You apply a proposal with `tempo-launch fleet apply-proposal`, which shows the real names and asks y/N.

### Reaching them over ssh, your way

`fleet ssh NAME -- COMMAND` and `fleet check NAME` use the system `ssh` with `-o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new`. They use your keys and your ssh agent. Tempo never asks for, types, reads or stores a password or a key; with `BatchMode` a missing key is an error message, not a prompt. A first host key is remembered, a changed one is refused. `fleet hosts` lists the `Host` names in `~/.ssh/config`, which are accepted as machine names.

`fleet check NAME` reports the remote system, CPU threads, memory, graphics card (`nvidia-smi`, or `system_profiler` on a Mac; best effort), free disk, and whether `tempo-launch` and `ollama` are installed, with versions. The answer is kept in `fleet.json` and shown on the fleet page.

`fleet setup NAME` shows the exact commands first and runs them only after a y/N (`--yes` skips the question): the fleet key and the two computers' identities go over ssh's input (never on a command line), Ollama is made to listen on the machine's private address (`OLLAMA_HOST=<address>:11434`, as a systemd user service where there is one; otherwise it prints the steps), `tempo-launch setup --apply --yes` runs there (or it prints how to install Tempo), the messenger starts there, and the machine is linked here.

### The messenger: `fleet serve`, `send`, `inbox`, `ping`

`tempo-launch fleet init` makes the fleet key: 32 random bytes in `<home>/fleet.key`, readable only by you (mode 0600; on Windows an ACL that grants only you). `fleet invite` and `fleet setup` hand it to a new computer; `fleet rotate` makes a new one (it is 256 bits, so rotate when a computer is lost, sold or reset, or you think the key was copied, and once a year as a habit; [FLEET-SECURITY.md](FLEET-SECURITY.md) says more), sends it to each computer over your ssh, then switches (a computer it cannot reach keeps the old key and drops out until you run `fleet setup NAME` for it; the old key is kept as `fleet.key.old`; a running messenger picks the new key up within seconds).

`fleet serve` listens on this computer's private and tailnet addresses and on 127.0.0.1, never on 0.0.0.0. It answers "who is `_tempo._tcp`?" on mDNS (UDP 5353, on every interface, which makes a firewall ask) only when started with `--announce`: `fleet invite` does that while it waits for the new computer, and otherwise computers find this one by the address sweep. On Windows the first time the messenger listens, Windows Defender Firewall asks about it; it only listens on your private and tailnet addresses, so choose Allow for private networks. `tempo-launch start` starts it when the fleet has more than one machine, and `tempo-launch schedule` keeps it running (a crontab line or a Windows task that starts it every five minutes; it ends at once when one already runs; on a Mac `start` does it).

Frames are length-prefixed over one persistent connection per peer: `length | HMAC-SHA256 | Ed25519 signature | one JSON header line | sealed body`. The body is sealed with ChaCha20-Poly1305 (a key from the fleet key, a fresh random nonce per frame, the header as associated data; see FLEET-SECURITY.md), so a note or a picture cannot be read on the wire. The HMAC (a second key from the fleet key) covers the header and the sealed body, the signature is the sender's own pinned identity, and the header holds a millisecond time and a nonce. A frame is dropped, with no answer, when its HMAC is wrong, when its sender is not pinned or the signature is not that identity's (so someone who copied the fleet key still cannot speak as a computer), when its clock is more than two minutes off, when its nonce was already seen (a replay), or when it is addressed to someone else. Many frames go out in one write and their answers are read after, so a batch costs one round trip.

Every message is written to a SQLite outbox (`<home>/fleet.db`, WAL) before anything is sent: `id, peer, topic, body, created, tries, delivered`. It is sent at once when the peer is up and retried with backoff (2, 4, 8 ... up to 300 seconds) when it is not, so nothing is lost when a laptop sleeps or this program restarts. The receiver keeps each id once (and answers a duplicate, so the sender stops), so a retry is never a second message. Topics: `heartbeat` (every 10 seconds, the click track every computer keeps time with: load, free memory, free graphics memory, the models loaded, how many requests are running, the queue, the measured speed, version, and every sixth beat what the computer offers; sent and forgotten, never queued), `note` (text from one machine to another), `job` (a member's step, played by another computer's own agent; the answer comes back as `result`), `call` and `callres` and `blob:...` (a tool call, its answer and the pieces of a file) and `status`. A computer is shown as away after three missed heartbeats (35 seconds).

`fleet send NAME "text"`, `fleet inbox` and `fleet ping NAME` (a round trip in microseconds on the open connection) are the command line. `GET /fleet.json` on the messenger's port answers this computer only (the Host header must name 127.0.0.1 or localhost, so a web page cannot read it); `GET /hello` answers any private address with the computer's name and whether a code is waiting, nothing else.

### Tools anywhere

Each computer offers its tools to the band, and only the ones its own owner allowed. With nothing allowed, a computer offers nothing. On the computer that owns the tool: `tempo-launch fleet allow comfy_image`, `fleet allow stage` (this computer plays Stage's workflows for the fleet, [STAGE.md](STAGE.md)), `fleet allow mcp:SERVER/TOOL` or `fleet allow mcp:SERVER/*` (and `fleet deny ...` takes one back). `tempo-launch fleet cap GIB` sets the biggest file that may cross between computers (4 GiB unless you say so). What is offered rides on the heartbeat: ComfyUI (when it answers on 127.0.0.1:8188 or at the agent's `comfy_url`, with its checkpoints and the memory of its card), and the allowed MCP tools (names, descriptions and parameters).

The agent on the main computer gets them with nothing to wire. `comfy_image` runs on this computer's own ComfyUI when there is one; otherwise on the computer with the biggest graphics card that offers it. The call travels as a signed `call` message, the other computer runs it through its own agent (so its GPU lease and "unload the language models first" rules still apply), and the picture comes back in 256 KiB pieces, checked against its SHA-256, into the output folder (`comfy_output_dir`, else `<agent data>/fleet-images`). The model sees a normal result and a line like "Made on studio-box, 6.2 s." Remote MCP tools show up as `<node>__<server>__<tool>`.

The main computer's rules still apply to every remote call: plan mode offers and allows only what reads; the deny list (`mcp_blocked`, by the server's name or the node's and server's; `fleet` / `comfy_image` for remote pictures) is honoured; anything that is not read-only asks first (and so does anything named in `mcp_confirm`, unless `mcp_trust` names it); results are redacted like local ones. The other computer checks its allow list again before running anything, limits arguments to 32 KiB, and writes a line to `logs/fleet-calls.log`, as the main computer does (tool and sizes, never the content). A call is never retried on its own, because a tool may write. Honest limit: the tool-calling path was tested end to end with stand-ins for ComfyUI and an MCP server, and against Ollama and a real ComfyUI only by reading.

### Which computer plays a request

Whole requests go to one computer; a model is never split across machines (that decodes about twice as slowly, even over a fast cable). Candidates are ordered by the model already loaded there, then free graphics memory (or memory), then the fewest requests running, then the measured speed (one fixed short prompt, timed once when a model is loaded and the computer is idle, kept until the model or Ollama's version changes). A step that fails on the wire (no answer, a dropped connection) goes once to the next computer under the same request id, and the first computer is not left holding it; a step that was answered, even with a no, is final.

### Honest limits

Everyone holding the fleet key can talk to the messenger, but only a pinned identity can speak as a given computer. A replayed frame is refused for the two minutes the clock allows, and the nonce list lives in memory, so right after a restart a replay inside that window passes the transport; a message is still stored once by its id. The fleet page is static; a switch is the command printed under a machine. The Inno Setup page was written by reading: there is no Inno Setup compiler here. mDNS announcing needs port 5353, which the system's own mDNS service may hold; then this computer is found by the address sweep instead.
## The Crate: free tools Tempo can set up and play

The Crate is a bank of free third-party tools, like a record crate you dig through: the best free one in each class, set up for you and played by Tempo. (Plain meaning: tools you can configure. The command is `tempo-launch crate`.) The catalog is `catalog/crate.json`, with 23 tools, each with its licence named: Blender (GPL-3.0-or-later), ComfyUI (GPL-3.0-only), FFmpeg (LGPL-2.1-or-later, GPL in fuller builds), ImageMagick (the ImageMagick License), Inkscape (GPL-3.0-or-later), GIMP (GPL-3.0-or-later), SoX (GPL-2.0-or-later), whisper.cpp (MIT), Piper (GPL-3.0-or-later), Pandoc (GPL-2.0-or-later), LibreOffice (MPL-2.0), Tesseract (Apache-2.0), yt-dlp (Unlicense), OBS Studio (GPL-2.0-or-later), Godot (MIT), FreeCAD (LGPL-2.0-or-later), OpenSCAD (GPL-2.0-or-later), KiCad (GPL-3.0-or-later), Ollama (MIT), ripgrep (Unlicense or MIT), fd (MIT or Apache-2.0), ExifTool (Artistic or GPL) and Audacity (GPL-2.0-or-later, listed as an app only). GIMP is the pick over Krita because `gimp -i -b` batch scripting is the better headless story.

```
tempo-launch crate                  what is here and what is not, with versions
tempo-launch crate show ID          one tool in full: licence, size, the install command, what Tempo can do
tempo-launch crate check            look for every tool and remember it in <home>/crate.json
tempo-launch crate add ID           show the exact command, ask y/N, run it, then ask whether Tempo may play it
tempo-launch crate remove ID        show the uninstall command, ask y/N, run it
tempo-launch crate enable ID        let Tempo play a tool (disable turns it off)
tempo-launch crate folders          the folders the tools may touch (add DIR, remove DIR)
tempo-launch crate review SCRIPT    read a Blender script and approve exactly that text
tempo-launch crate mcp              the MCP server the agent starts
```

**Your own package manager.** `add` picks winget on Windows, Homebrew on Mac, apt on Linux, or pipx (Piper), whichever you have, and prints the command before it asks. apt needs admin rights, so Tempo prints that command and you run it; Tempo never runs sudo. A pinned download (the catalog supports one, with its SHA-256) is checked before anything is unpacked or run, and a file that does not match goes to `<home>/trash`. No pinned download ships yet: I could not fetch release files to hash them from here, so every tool uses a package manager, or a by-hand note where none has it (whisper.cpp on Windows and Linux, Godot on Linux, ComfyUI). `remove` for something Tempo fetched itself moves it to the trash; nothing is deleted.

**What was checked.** winget ids and Homebrew names were checked against the winget-pkgs and Homebrew trees on 2026-11-07, and Piper's licence against PyPI. apt names and install folders were not tried on a real machine. A tool with a guess in it says `"verified": false` and why: ComfyUI, GIMP, whisper.cpp, Piper, Godot, FreeCAD and KiCad.

**How Tempo plays them.** `crate mcp` serves only tools that are installed and turned on: `blender_render`, `blender_script`, `ffmpeg_convert`, `ffmpeg_trim`, `image_resize`, `image_convert`, `svg_export`, `ocr`, `doc_convert` (Pandoc or LibreOffice), `transcribe`, `speak`, `audio_normalize`, `exif_read`, `text_search`, `openscad_export`. Each runs the real program with a fixed argument list: no shell, no free-form command. Paths must sit inside the workbench (`<home>/crate/work`), a folder you added, `TEMPO_CRATE_ROOTS`, or the agent's own write folders; `..` and odd characters are refused; inputs have size limits; an output never replaces a file; a run that hangs is stopped. Reading tools (`ocr`, `transcribe`, `exif_read`, `text_search`) carry `readOnlyHint`; writing tools carry `destructiveHint: false`. The exception is `blender_script`, which can do anything a script can, so it is marked destructive, runs only a file in `<home>/crate/scripts` whose SHA-256 you approved with `crate review`, and refuses it again if the file changes. `whisper.cpp` and Piper need a model or voice file in `<home>/crate/models`.

The agent connects the Crate by itself, the way it connects Vault, Studio and Room: when `<home>/crate.json` has a tool turned on and `tempo-launch` is next to Tempo, it adds a `crate` server whose every tool asks first unless you trust it (`mcp_trust`). The setup helper (`tempo-launch mcp`) also has `crate_list` (read-only) and `crate_propose`, which only writes a suggestion and gives you the command to run. The welcome page lists the best picks for this computer. Stage (video and image workflows, [STAGE.md](STAGE.md)) connects the same way, as a `stage` server, when `<home>/stage.json` has a workflow turned on.

## Re-run it

After a hardware change (more RAM, a new card), after adding models to the catalog, or after a new goal: `tempo-launch setup` shows what changed, `tempo-launch setup --apply` applies it. On each PC of a group, run it on that PC.

## What it does not do

- It does not install Ollama on your computer the way Ollama's own installer does. The only thing it ever fetches is Tempo's own pinned copy, after your yes, kept inside the Tempo home. It does not touch Ollama on another machine. If Ollama listens only on a tailnet address, setup (which looks at `127.0.0.1`) reports it as not running.
- It does not choose the router/judge in the agent's settings: the agent has no key for it yet. d1 is fetched and recorded, nothing more.
- The Windows installer text has been checked by reading only; there is no Inno Setup compiler in the cloud box.
