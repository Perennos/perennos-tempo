# Retune (weight edits)

A retune is adjusting the instrument itself. Your frontier model (Claude through Claude Code or Claude Desktop over MCP, or your own cloud engine in Tempo) reads a local model's weights and edits them directly. Tempo then checks the result on prompts the editor never saw, and keeps it only if it wins.

Nothing is trained. The editor looks at numbers, reasons about what they do, and changes a few. That is the whole trick, and it is why a retune is small, readable and undoable.

## The paper

Rikiya Takehi, Jayden Teoh, A. Zhang and Omar Khattab (MIT CSAIL), *Sparks of Weight-Space Programming: LLMs can natively read & write neural network weights*, 2 November 2026. <https://rikiyat.github.io/blog/weight-space-programming/>. The authors' logs are at <https://github.com/RikiyaT/lm_understand-logs>, and their edited model is `RikiyaT/Qwen3.5-0.8B-weight-space-programming` on Hugging Face.

The experiment Retune is built from: a frontier model got a Python kernel over Qwen3.5-0.8B's weights (CPU only, no GPU), practice prompts from the GSM8K and MATH training sets, and a budget of 10, 25 or 50 forward passes of up to 512 prompts each. It never saw the evaluation sets. It traced the model's thinking loops to one direction written by the MLPs of layers 15 and 19, and subtracted that direction from 64 output columns in each: 131,072 weights, 0.016% of the model. Reported results:

| | before | after |
| --- | --- | --- |
| MATH-500 | 7.9% | 48.1% |
| GSM8K | 13.8% | 67.1% |
| IFEval | 21.0% | 62.3% |

These are the paper's numbers, not ours. We have not reproduced them.

## What works, and what does not

**Works: removing an obstacle.** A small model that loops (it keeps thinking and never answers, or repeats a stretch of words until the token limit) is not short of ability. Something in its weights keeps it from using the ability. Find the cause, take it out, and the ability shows.

**Does not work: adding a skill.** On Gemma 3 270M, which does not loop, the same method could not add ability. Editing removes what is in the way; it does not teach. If a model is bad at something because it never learned it, use the Woodshed ([WOODSHED.md](WOODSHED.md)), or a bigger model.

So the first question is whether your model loops. Rehearsal now counts it. An answer loops when it runs into the token limit, or when one run of eight words shows up three times in it. When a model does it on one answer in ten or more, Rehearsal says so: "This model loops on 41% of answers. A retune might fix that." You see it in `tempo-launch woodshed rehearse` and in `retune check`.

## The steps

```
tempo-launch retune plan qwen3.5:0.8b       what it needs, and how to connect your AI
tempo-launch retune bench qwen3.5:0.8b      the bench (your AI starts this itself)
tempo-launch retune show SESSION            the edits so far
tempo-launch retune check SESSION --held-out FILE
tempo-launch retune undo qwen3.5:0.8b       go back a version
tempo-launch retune undo SESSION --edit     take the last edit off
tempo-launch retune versions
```

**plan** says one honest thing the first time and asks y/N once (and remembers): Anthropic's consumer terms bar using the service to develop or train competing AI models. A retune edits your own local model for your own use, but it's your call; check your plan's terms. `--yes` does not answer this one for you. Then it shows the model's licence (Qwen is Apache-2.0, Gemma follows Gemma's terms, Llama follows its community licence; a model it does not know asks whether you have read the card), picks a computer, and sets up what is missing, each after a y/N that shows its size:

- the bench's own Python folder (numpy, torch, transformers; about 1 GB on a CPU-only machine, about 5 GB with a card), made in `<home>/retune/venv` and touching nothing else;
- the full-precision safetensors weights, about 1.6 GB for a 0.8B model in bf16. Ollama's own copies are quantised GGUF files and an edit needs the original numbers. Tempo looks in `<home>/retune/weights/` and the Hugging Face cache first, and `--weights FOLDER` points at your own. Gemma and Llama ask you to accept their terms on Hugging Face first.

Last it prints the exact lines to connect the bench:

```
claude mcp add tempo-retune -- /path/to/tempo-launch retune bench qwen3.5:0.8b
```

or, for Claude Desktop, a `mcpServers` snippet for `claude_desktop_config.json`.

**Which computer.** A computer with a graphics card that holds the model wins (most graphics memory first). Failing that, the one with the most memory, on its processors. The bench and its forward passes run on the same computer. If that is another computer in your Fleet, plan says so and gives the ssh line: the bench is a stdio server, so Claude can reach it over `ssh host tempo-launch retune bench ...`.

**check** takes a session's edit list and:

1. applies it to a copy of the weights (a new folder; never the original);
2. converts the copy to GGUF with llama.cpp's `convert_hf_to_gguf.py` (`--outtype q8_0` by default; `f16` and `bf16` too);
3. makes an Ollama model from it that keeps the base model's chat template and settings;
4. runs the Woodshed's Rehearsal against the base: the same scorer on your held-out set, plus the canary of twelve general tasks, plus the loop count;
5. promotes only if it wins by the margin (`margin` in `woodshed.json`, 5 points by default) and the canary does not slip. Then `ollama create tempo-<model>-retuned-<yyyymmdd>`.

Every check keeps a version folder in `<home>/retune/versions/<model>/`. Its `manifest.json` records the SHA-256 of the edit list, how many weights changed (count and share of the model), and every score. `retune versions` lists them. `retune undo <model>` steps back to the version before, or to the plain model. Nothing is deleted: the Ollama models and the folders stay.

Your held-out set is a file of your own, lines like `{"prompt": "...", "answer": "..."}`, passed as `--held-out` to `bench` or `check`. It is scored with the Woodshed's token-F1, which measures how close an answer's wording is to yours, so write references the way you want answers to sound. Anything the bench handed out as practice is left out of it.

One thing is not level: your Ollama copy of the base is quantised (usually Q4) and the retuned model is q8_0 or f16. A win might be a little precision. check says so.

## The bench

`retune bench` is an MCP server on stdio, the same plumbing as `crate mcp`. Behind it is a small Python helper (`retune_bench.py`, written next to the venv so you can read it first). The tools are fixed.

| Tool | What it does | Costs |
| --- | --- | --- |
| `model_info` | architecture, layers, every tensor's name, shape and number format | free |
| `tensor_stats` | norms, mean, std, top singular values; with a `vector`, the columns that write that direction most | free |
| `read_slice` | up to 20,000 numbers from a tensor | free |
| `top_directions` | the strongest singular directions of a matrix | free |
| `forward` | up to 512 prompts through the model; texts, loop and length stats | 1 pass |
| `activation_diff` | mean residual difference between two prompt sets at a layer, and principal directions | 2 passes |
| `ablate_direction` | subtract a direction from chosen columns of a matrix | free |
| `scale` | multiply chosen rows or columns | free |
| `add_rank1` | add alpha times the outer product of two vectors | free |
| `set_slice` | write numbers into a slice | free |
| `practice_set` | practice prompts you supplied, or a band's winning takes (`woodshed:<band>`) | free |
| `budget` | passes used and left | free |

The budget is 50 passes, set with `--passes`. It is kept on disk and written before a pass runs, so restarting the bench never gives passes back. A pass is one `forward` call, whatever its batch size; fewer prompts take less time for the same pass. Reads and edits keep working at zero.

Edits use the weights' own number format (bf16 values are rounded to bf16 after each edit, so what the bench shows is what check writes). `ablate_direction(tensor, vector, columns)` normalises the vector and does `W[:, j] -= v (v . W[:, j])` for each listed column, so those columns no longer write that direction.

**Practice prompts** go in `<home>/retune/practice/NAME.txt` (one prompt per line) or `NAME.jsonl` (`{"prompt": ...}`).

## Safety

- **The tools are fixed, and none of them runs code.** A cloud model must never get a shell on your machine. No tool takes a path, a command or a script. The bench rebuilds every argument from the keys the tool knows and drops the rest; the helper checks again and has no door that executes anything. Tensor names are letters, digits, dots, dashes and underscores. A test fails if a tool's schema ever asks for a path, file, command, script, code or url.
- **Originals are never touched.** The helper opens the weights read-only. An edit is written to the session's edit list (`edits.json`, with each edit's numbers as `.npy` files in `deltas/`) and replayed on the untouched original whenever a tensor is read. `check` writes a new folder and refuses one that already has files.
- **Every edit is reversible.** `retune show` lists them; `retune undo SESSION --edit` moves the last one to `undone.jsonl`, and the weights are exactly as before it (replayed from the original, not subtracted back). Edit numbers are never reused.
- **The editor never sees the held-out set.** No tool names a file; `practice_set` opens only your `practice/` folder and Woodshed takes, leaves out any prompt that is in the held-out set, and writes down every prompt it hands out so check can leave out a held-out prompt that slipped through.
- **Nothing leaves your computer unless you set it up.** The bench talks to whatever you connected to it, and only what its tools return crosses over: numbers, answers to the prompts it was given, and practice prompts. If you connect a cloud model, those go to it. That is the point of connecting one, and your call.
- **Risky steps ask first.** The terms, an unknown licence, the Python folder, the download, and each check are y/N. Nothing is deleted anywhere. A guest never gets these tools: they exist only as `tempo-launch retune`, on your computer, for the AI you connected.

## CPU-only computers

The paper's run was CPU-only, and it works. It is slow. Plan prints a guess from the model's size: about 8 tokens a second per core across a batch, so a full pass (512 prompts of 512 tokens) on a 0.8B model takes about an hour on 8 cores, and 50 of them about two days. The guess has not been measured. Tell your AI to use small batches (64 prompts is about 8 times quicker for the same pass) and short answers while it is looking, and a full batch when it checks a fix. A computer that cannot hold the model (about 4 bytes a parameter plus room, from memory, on processors) is not offered.

## What was not tested

**Retune was never run against a real model.** The machine it was built on has no GPU, and Hugging Face downloads are blocked there. The tests use a tiny fake: a few numpy tensors written as safetensors when the test runs, and a fake forward function. They prove the rules: `ablate_direction` matches the math (also in bf16), edits never touch the original file (checked by hash), the budget is enforced and survives a restart, the held-out set is not reachable and is filtered, undo restores exactly, loop detection, the y/N gates, the order of the check steps, and the manifest. The helper's file format was cross-checked against the `safetensors` library once, by hand, and the test for that skips itself when the library is not there.

Not tested: the torch and transformers forward passes (`TorchRunner`), whether Hugging Face tensor names match the loaded model's parameter names for each architecture (multimodal models may differ), llama.cpp's conversion of an edited model, `ollama create` from the result, the Fleet pick on real machines, and the time guesses. Try it on the smallest model first, read the helper first, and expect to fix something.
