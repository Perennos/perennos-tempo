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

- **Windows installer:** a box, ticked by default: "Set up models for this computer (downloads what fits, shown before anything is fetched)". If Ollama is already there, it runs `tempo-launch setup --apply --yes` hidden after the install. If not, it only runs `tempo-launch setup --page`, which leaves the plan in `<Tempo home>\setup.html`.
- **Linux and Mac installer:** `install.sh --setup` runs `tempo-launch setup`, then asks y/N before `--apply`; `--yes` skips the question.
- **First start:** when there are no agent settings yet, `tempo-launch` (or `tempo-launch start`) writes `<Tempo home>\setup.html` if it is not there yet, and opens it once. It shows the plan in plain words, the exact command to apply it and the d1 licence note. Nothing is fetched. A marker file `setup.page-opened` keeps it from opening again.

## Re-run it

After a hardware change (more RAM, a new card), after adding models to the catalog, or after a new goal: `tempo-launch setup` shows what changed, `tempo-launch setup --apply` applies it. On each PC of a group, run it on that PC.

## What it does not do

- It does not install Ollama on your computer the way Ollama's own installer does. The only thing it ever fetches is Tempo's own pinned copy, after your yes, kept inside the Tempo home. It does not touch Ollama on another machine. If Ollama listens only on a tailnet address, setup (which looks at `127.0.0.1`) reports it as not running.
- It does not choose the router/judge in the agent's settings: the agent has no key for it yet. d1 is fetched and recorded, nothing more.
- The Windows installer text has been checked by reading only; there is no Inno Setup compiler in the cloud box.
