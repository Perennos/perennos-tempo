# Woodshed (training)

Woodshed is the music word for practising a part until you own it. Here it means your local models getting better at their parts from the takes (judged attempts) your engines and bands produce, with your own frontier model helping if you have one connected.

Changing a model's weights in real time is not something anyone can do well today. What works runs at three speeds, and Tempo has all three:

| Speed | What it is | When it is heard |
| --- | --- | --- |
| **Lessons** | Short notes per band, put in the prompt. No training. | The next answer |
| **Woodshed** | A LoRA (a small add-on to the model) trained at night on your winning takes, gated, versioned. | The next day, once it passes |
| **Mix** | llama-server with several adapters loaded and a fader per band or part. | The next answer, no reload |

Everything is off until you turn it on for a band: `tempo-launch woodshed on <band>` (`default` is the usual chat). With nothing trained, `tempo-launch woodshed status` says: "Nothing in the woodshed yet. Turn it on for a band with `tempo-launch woodshed on <band>`."

## What leaves your computer

Nothing, unless it is your own Fleet or your own cloud model. Training runs on this computer or on a machine of your Fleet that your own ssh key reaches (`tempo-launch fleet check NAME`), and the scrubbed data goes there over that ssh and nowhere else. The fleet messenger's `job` topic only plays chat steps through a guest conversation; it never runs a program, so training goes over ssh instead. A computer that only paired to chat is never a candidate, and a machine is only used when it is quiet (one-minute load under half a processor each). Your own cloud model can write lessons by handing over a small JSON file (`woodshed reflect <band> --from FILE`); Tempo never calls a cloud service by itself.

## The terms note

Anthropic's consumer terms (section 3) and commercial terms (D.4(a)) bar using its models' outputs to train competing models. So a cloud model may judge takes and write Lessons, but text it wrote is left out of the LoRA data by default. `woodshed.allow_cloud_text` (off) is for owners whose own contract allows it; `tempo-launch woodshed allow-cloud-text on` says so in one sentence and asks y/N. A take whose `author` is not `local` or `owner` is never trained on unless that setting is on, and an author nobody named is treated as cloud. Tempo ships no general training text for the same reason: general examples come from your own other bands' winning takes and an optional `<home>/woodshed/general.jsonl` of your own. This is not legal advice; read your own terms.

## 1. Lessons

`<agent data>/lessons/<band-or-default>.jsonl`, one short bullet per line:
`{"id","text","helped","hurt","source":"local"|"cloud"|"owner","created","last_used"}`.

- The file is only ever **appended to**. Later lines are small ops on a bullet: `bump` (helped or hurt up), `retire`, `restore`, `edit`. Reading folds them in order.
- A bullet whose `hurt` passes its `helped` by 2 or more is **retired** by itself. It stays in the file, in the "retired" section, and `--restore ID` brings it back with the count started again. Nothing is deleted.
- The top bullets (helped minus hurt, your own words first on a tie, then the most recently used) that fit under a token cap (400 by default, `lesson_tokens`) are written to `<band>.prompt.txt`. The agent puts that file in the system message of the band's parts. A band with no lessons gets no extra text.
- **The Reflector** reads new judged takes with the lessons that were in play and answers `{"add":[...],"helped":[ids],"hurt":[ids]}`: up to 3 new bullets, and which old ones showed in the winners or the losers. It can only touch ids it was shown. A local model plays it through the agent (a guest conversation, no tools): `woodshed reflect [band]`, and `woodshed run` does it first each night. Your own cloud model plays it by writing the same JSON to a file: `woodshed reflect <band> --from FILE`; those bullets are marked `cloud`.
- You see and edit them with `tempo-launch woodshed lessons [band]` (`--add TEXT`, `--edit ID TEXT`, `--retire ID`, `--restore ID`).

## 2. The Woodshed (nightly LoRA)

`tempo-launch woodshed` takes `status`, `plan`, `run [--yes]`, `promote`, `rollback`, `versions`, `lessons`, and also `on`, `off`, `set`, `allow-cloud-text`, `reflect`, `rehearse` and `mix`. Run `woodshed run --yes` from your scheduler for the nightly run; it still refuses a busy machine and a band without enough takes.

1. **Data.** The band's winning takes (`winner: true`), scrubbed: anything that holds a key or token drops the whole example (a half-hidden key teaches the model to write `[hidden]`); emails, phone numbers and home folders (`/home/ana`, `C:\Users\Ana`) are replaced. About 20% general examples are mixed in (a quarter as many as the band's own), then 85/15 into `train.jsonl` and `valid.jsonl` (chat JSONL, split by a hash of the take id so it is stable). A **scrub report** (`scrub-report.txt` and `.json`) sits beside them. At least `min_examples` (40) usable winners are needed. The lessons block the agent adds to a prompt is cut out of the training prompts.
2. **Node.** The Fleet machine with the most graphics memory on an NVIDIA card, or an Apple Silicon Mac with the most memory (two thirds of a Mac's memory counts), or this computer; `set_top node` in woodshed.json pins one by name. Only when idle. The machine also decides the model, in words the plan prints:
   - **A small card (6 to 8 GB):** QLoRA on the largest model of the base's family that fits (about 0.65 GB per billion parameters plus 2 GB, an estimate), and the plan says which: "qwen3:8b needs about 7 GB and big-box has 6, so this run trains qwen3:4b". The adapter fits that model, not the one you asked for.
   - **No card at all:** a processor with the memory trains models of 3B and under with plain 16-bit LoRA (no 4-bit without a card), slowly. The plan shows a time as a range, labelled an estimate, from the threads (for example "about 5–10 hours for 2,000 examples (1 pass) on 16 threads"; it is worked out from the model's size and a guess at a thread's speed, and was never timed on a real run). Over 24 hours it is said in days. Memory counted is three quarters of the RAM.
   - **Above 3B with no card:** no CPU job starts. It takes days, so the plan says plainly that a retune or Lessons is the better tool (neither needs a card), or that a computer with an NVIDIA card or Apple Silicon would do it.
3. **Script.** Written into the version folder for you to read: TRL QLoRA on CUDA (4-bit base, rank 8 to 16, 1 or 2 passes, attention and MLP projections only: `q k v o gate up down`), or `mlx_lm.lora` with `lora.yaml` on a Mac plus `mlx_to_peft.py` to turn its adapter into a PEFT one.
4. **Install.** The trainer gets its own venv (`<home>/woodshed/venv`, or `~/.tempo-woodshed/venv` on another machine) and a shallow clone of llama.cpp, only after a y/N that shows the commands and the size (about 5 GB on CUDA, about 3 GB on a Mac: estimates, never measured).
5. **Run and convert.** Training, then llama.cpp's `convert_lora_to_gguf.py --base-model-id <hf> --outtype f16 --outfile adapter.gguf <adapter>` (flags checked against the script's own help on 7 Nov 2026), then the `.gguf` is brought home.
6. **Rehearsal.** An Ollama model `tempo-<band>-try-<version>` is made from `FROM <base>` and `ADAPTER <file>`. Base and adapter answer the held-out takes (up to 60) and 12 canary tasks of general knowledge, same scorer. The scorer is token F1 against the winning take (how much of its wording the answer shares). That is a proxy for "sounds like the takes you kept", not a judge of whether an answer is good.
7. **Gate.** Promote only if the adapter wins by the margin (5 points, `margin`) and the canary score does not drop (`canary_slack`, 0). Fewer than 5 held-out examples is "too few to tell".
8. **Promote.** `woodshed promote [band]` asks y/N, writes a Modelfile (`FROM <base>`, `ADAPTER <file>`), runs `ollama create tempo-<band>-<yyyymmdd>` (`-2`, `-3` when the name was used) and adds a line to `active.jsonl`. The Conductor then plays the members that played the band's base model on that model (`conductor run` says so).
9. **Versions.** `<home>/woodshed/<band>/versions/<yyyymmdd>-<n>/` keeps the data, scrub report, scripts, adapter, `Modelfile` and `manifest.json`: base, data hash (SHA-256 of train then valid), scores, gate, parent version (the one playing when it was trained), node, rank, epochs. Nothing is deleted. `woodshed rollback` is one command: it goes to the parent's model or the plain base by adding a line to `active.jsonl`. `versions` lists them all.

## 3. Mix (the faders)

llama-server loads the base once and several adapters on top; each request says how loud each one is. Checked against llama.cpp's `tools/server/README.md` (master, 7 Nov 2026): `--lora FNAME` (repeat it), `--lora-init-without-apply`, `GET /lora-adapters` (`[{"id","path","scale"}]`), `POST /lora-adapters` (`[{"id","scale"}]`, the global scales) and the per-request `"lora": [{"id":0,"scale":0.5}]`, which overrides the global ones; an adapter left out runs at 0. The README documents `lora` under `/completion`; it says `/v1/chat/completions` also takes `/completion`'s own options, so the agent sends it there too, **but this was not run against a real llama-server**.

- `woodshed mix base PATH.gguf`, `woodshed mix add <band> [PATH]` (a promoted adapter; the id is the order, so adapters are only added at the end), `woodshed mix start` (shows the command, asks, starts llama-server on 127.0.0.1 and adds it to the agent's `llama_servers`), `woodshed mix check` (compares what the server loaded with `mix.json`).
- `woodshed mix set <band>[/<part>|*] name=scale ...` writes `<agent data>/mix/faders.json` (and one line to `faders.log`). The agent reads it at every request, so a fader moved while Tempo runs is heard on the next answer, with no reload. The Conductor, or your own frontier model acting as Conductor, moves them with this command. `mix global` sets the server's default scales.
- In the agent, a guest conversation for a band with faders plays on the first `llama_servers` engine (OpenAI-compatible chat, answered in one piece, no tools: guests never get tools) and falls back to Ollama if it cannot be reached.

## The data contract

You may read or write these; Woodshed owns the files and this schema.

**`<agent data>/takes/takes.jsonl`**, append only. A reader keeps the last line for each take id.

```json
{"v":1,"take":"<id>","run":"<run-id>","band":"<band or null>","part":"<part or null>","task":"<short task text>",
 "engine":"<engine id>","model":"<model>","author":"local|cloud|owner","prompt_ref":"runs/<date>/<run>.jsonl#<n>",
 "answer_ref":"...","checks":{"tests":true,"soundcheck":{"high":0,"med":0,"low":0},"judge":0.0,"owner":"accept|reject|null"},
 "winner":true,"pair":"<take id of the loser or winner of the same task, or null>","ts":"RFC3339"}
```

- `author` is who wrote the answer's words. `cloud` only for engines listed in `cloud_engines` in conductor.json; `owner` for text you wrote by hand. Your thumb on a model's answer is `checks.owner`, and `author` stays `local`.
- References: `runs/<date>/<run>.jsonl#<n>` is line `n` of the run log counting from 0; `conversations/<id>.json#<n>` is event `n` of an agent conversation. Anything else, or a path that leaves the agent's folder, is not read.
- The reader is forgiving: only `take` is required; a line it cannot read is counted and skipped; a run-log line of a kind it has never seen still gives up its prompt and answer when it has `messages`, `prompt`/`input`/`task`/`text`, or `answer`/`output`/`response`/`text`. A text cut short (`...[truncated`) is not trained on.
- **Thumbs in agent chat** (`POST /agent/verdict {"conv","n","verdict":"up"|"down"}`) append a take with `winner` true or false. They are kept only when Woodshed is on for the conversation's band; otherwise the chat says so and keeps nothing.
- **`<agent data>/woodshed.json`**: `allow_cloud_text` (false), `margin` (5), `min_examples` (40), `canary_slack` (0), `rank` (8 to 16), `epochs` (1 or 2), `lesson_tokens` (400), `node`, `llama_cpp`, `reflector_model`, and `bands.<band>.on|base|hf`. The old file goes to the trash folder before a new one is written.
- **`/agent/new`** takes `band` and `part`; the Conductor sends them.

## What was never run for real

The machine this was built on has no GPU and cannot download from GitHub or Hugging Face except the llama.cpp texts above. So these were only ever run against **fakes** (a fake trainer, a fake Ollama and a fake llama-server): the training script, the install, the conversion, the Rehearsal against a real model, `ollama create` with an adapter, the Mac path including `mlx_to_peft.py`, the processor-only path and its time estimate (a formula with guessed constants, never checked against a real run), and per-request `lora` on a real llama-server. The tests prove the rules: the scrub, the cloud-text filter, the split, the gate, the version files, rollback, the order of commands, the ids of the faders. They do not prove that the generated Python runs on your versions of TRL, mlx-lm or llama.cpp, or that a real adapter beats its base. Try a small band first and read the script before you say yes.
