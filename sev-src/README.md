# Sev

**Sev** is a reproduction and clean-room analysis of RLCD (Reinforcement
Learning for Calibrated Decisions), the training method behind typed-decision
models such as TypeSafe's Jev and its open reproduction, Laya.

**Atomic answer:** Laya's RL term is an evolution-strategies estimate of the
gradient of a noise-smoothed proper scoring rule. As the noise vanishes it
equals the cross-entropy gradient the model already computes; at real noise
levels it makes inference over-confident. Across 30 runs, plain cross-entropy
matched or beat it on every proper score, and the only thing that raised
accuracy was the input token budget.

**TL;DR**

- The RL term is a smoothed cross-entropy gradient; as σ → 0 it *is* the CE gradient.
- Its smoothed optimum is provably over-sharp at inference; the fitted temperature rises 1.14 → 2.42 as the noise scale goes 0.25 → 4.
- 30-run ablation: CE-only is at least as good as RL+CE on Brier, NLL and accuracy.
- The only accuracy lever is the input option-token budget (typed decisions 0.782 → 0.789; Banking77 15.8% → 31.2%).

Start here:

- **[`docs/paper/main.pdf`](docs/paper/main.pdf)** — the IEEE-format technical
  report (*Dissecting RLCD*), built from `docs/paper/main.tex`; every table and
  figure regenerates from the JSON result files.
- **[`docs/EXPERIMENTS.md`](docs/EXPERIMENTS.md)** — the runbook/log for each
  experiment (hypothesis, config, command, result, decision).
- **[`docs/reports/`](docs/reports/)** — dated run/compute reports
  (`docs/reports/YYYY-MM-DD/<topic>.md`), e.g. the E2 results.
- **[`docs/analysis/`](docs/analysis/)** — dated analytical notes that feed the
  report (e.g. the noise-smoothing over-confidence proof sketch).
- **[`experiments/`](experiments/)** — self-contained numerical scripts (E1's
  toy bias check) that don't need the full `src/` training pipeline.
- **[`scripts/e2/`](scripts/e2/)** — the rented-GPU runbook plus the analysis
  tooling: HF export/fetch, the σ sweep, the reward-weight ratio, E4, E5, the
  noise-averaging probe, and paired-bootstrap statistics.

## Results: what does the RL term do?

The report's central result: Laya's RL term is a score-function
(evolution-strategies) estimator of the gradient of a noise-smoothed proper
scoring rule. As the noise vanishes it equals the cross-entropy gradient the
model already computes; at non-zero noise its optimum is provably over-sharp at
inference. Across the full ablation — CE-only / RL+CE / RL-only, a σ sweep to 4,
a reward-weight sweep, and a reward-composition sweep — CE-only is at least as
good on every proper score. The only change that raised accuracy was matching
the checkpoint's documented 1024-token sequence budget:

| model | accuracy | Brier | NLL |
|---|---|---|---|
| CE-only (512/192) | 0.782 ± 0.004 | 0.052 | 0.861 |
| RL+CE (Laya recipe) | 0.773 ± 0.002 | 0.054 | 0.866 |
| RL-only | 0.769 | 0.054 | 0.866 |
| **CE-only, 1024/256** | **0.789** | **0.0495** | **0.858** |
| CE-only, 1024/256, typed-decisions init | 0.789 | 0.051 | 0.859 |
| Laya `typed-decisions` (reference) | 0.766 | 0.062 | — |

The budget lever is not specific to typed decisions. On a high-cardinality
recast task (Banking77, 77 options) raising the option-token budget from 256 to
512 roughly doubles zero-shot accuracy — 15.8% → 31.2% (base Laya) and 15.8% →
32.0% (the CE-only model) — while it changes nothing on 4–6-option tasks
(AG News 94.5%, Emotion 59.8%, matching Laya's published 95.0 / 59.5).

The best checkpoint is published as a Hugging Face model:
**[minhleduc/laya-typed-decisions-ce-1024](https://huggingface.co/minhleduc/laya-typed-decisions-ce-1024)**
(model card, weights, fitted temperatures) and mirrored under
**[LakoreAI/sev](https://huggingface.co/LakoreAI/sev)**.
All ablation checkpoints and per-run metrics are in
[minhleduc/rlcd-e2-checkpoints](https://huggingface.co/minhleduc/rlcd-e2-checkpoints).

## Scaffold status

This repository started from a generic MLP-over-fixed-features research
template (config-driven training loop, callbacks, evaluation, tests). Its own
"Extending the template" contract — replace the model/dataset/loss/metrics,
keep the checkpoint schema, CLI, and callbacks working — is exactly what has
been done: `src/` now implements the typed-decision (RLCD/Laya-style) model
needed for the E2 loss ablation directly in place, not as
a parallel package.

## Contents

- [Quickstart](#quickstart)
- [Data format](#data-format)
- [Training](#training)
- [Evaluation and inference](#evaluation-and-inference)
- [Configuration](#configuration)
- [Extending the template](#extending-the-template)
- [License](#license)

## Quickstart

Requires Python ≥ 3.12 and [`uv`](https://docs.astral.sh/uv/).

```bash
git clone <your-repo-url> sev
cd sev

uv sync                # core: torch, transformers, datasets, numpy, scikit-learn
uv sync --extra rich   # optional: pretty training summary (panels / tables)
uv sync --extra wandb  # optional: Weights & Biases logging
uv run pytest          # test suite (offline — uses fake encoder/tokenizer fixtures)

# end-to-end against the real dataset with a tiny real encoder, no GPU required
uv run python scripts/training/smoke_test.py

# E1's standalone toy-bias experiment (no src/ pipeline needed)
uv run python experiments/e1_toy_bias.py
```

Platform-aware torch builds resolve from `pyproject.toml`: Linux + NVIDIA uses
the CUDA wheels, macOS resolves CPU/MPS wheels.

## Data format

Training reads directly from a Hugging Face dataset shaped like
`LocalLLaMA/typed-decisions`: each row is one *case*
with three JSON-string columns —

| Column | Contents |
|---|---|
| `state` | free-text/JSON context the questions are asked about |
| `questions` | `{qid: {"type": "choice"\|"score"\|"noul", "instructions": str, "criteria": ...}}` |
| `gold` | `{qid: {"label": str, "probabilities": {option_key: float, ...}, ...}}` |

`src.data.TypedDecisionDataset` flattens each case into one row per question
(state re-encoded per question, matching Laya's own sequence builder) and
`src.data.build_sequence` renders that row as
`[CLS] <type> question: <instr> [SEP] [MASK] opt0 [MASK] opt1 ... [SEP] <state> [SEP]`.
A calibration slice is carved out of the train split at load time
(`calib_fraction` in `TrainingConfig`) for raw-ECE validation and
post-training temperature fitting — never from the test split
(Laya issue #186).

## Training

```bash
uv run python scripts/training/train.py --config configs/train.yaml

# individual overrides win over the YAML
uv run python scripts/training/train.py \
    --config configs/train.yaml --epochs 5 --lr 1e-5 --w_rl 0 --w_ce 1

# fast local smoke run: tiny real encoder, small batch, one epoch
uv run python scripts/training/train.py --config configs/rlcd_smoke.yaml
```

The run writes `checkpoints/<run_name>/best.pt`, periodic `epoch_*.pt`, and a
`train_log.json`. Checkpoints carry the model's `DecisionModelConfig` so the
evaluator/inference script can rebuild it without the original YAML. `w_rl`
/ `w_ce` / `sigma_start` / `sigma_end` / `anneal_sigma` in `TrainingConfig`
are exactly the E2 ablation knobs (CE-only:
`w_rl: 0`; RL-only: `w_ce: 0`; fixed-σ: `anneal_sigma: false`).

## Evaluation and inference

```bash
# fit temperature on a calibration slice, report raw + post-T metrics on test
uv run python scripts/training/evaluate.py \
    --ckpt checkpoints/<run>/best.pt --json results/eval.json

# score one typed question against one state blob
uv run python -m src.pipelines.infer \
    --ckpt checkpoints/<run>/best.pt \
    --state '{"task": "..."}' \
    --question '{"type": "choice", "instructions": "...", "criteria": {"a": "...", "b": "..."}}'
```

## Configuration

Configuration is split in two, mirroring the reference project:

- **`src/config.py` → `DecisionModelConfig`** holds the *architecture*
  (encoder name, head depth, sequence budgets, K-bucket boundaries). It is
  saved into every checkpoint.
- **`src/pipelines/config.py` → `TrainingConfig`** holds the *training loop*
  (dataset, optimizer, RLCD loss weights, callbacks). It is loaded from
  `configs/train.yaml` and overridable from the CLI.

Callbacks are wired from the same YAML, unchanged from the inherited
scaffold:

```yaml
lr_scheduler: {type: cosine, t_max: 10, eta_min: 1.0e-7}
early_stopping: {enabled: true, monitor: raw_ece, mode: min, patience: 3}
save_best: true
best_metric: raw_ece
best_mode: min
wandb: {enabled: false, project: sev}
```

## Extending the template

1. Replace `DecisionModel` in `src/modules/model.py` (keep the
   `forward(ids, attention_mask, marker_pos, marker_mask, qtype) -> logits`
   contract).
2. Replace `TypedDecisionDataset` / `build_sequence` in `src/data.py` for a
   different typed-decision source.
3. Replace the metrics in `src/pipelines/eval.py`.
4. Wire any new callback into `src/callbacks/` and register it in
   `build_callbacks` (`src/pipelines/train.py`).

Everything else — checkpoint schema, CLI, W&B/early-stopping/best-checkpoint
callbacks, test structure — keeps working.

## FAQ

**Is RL needed to calibrate typed decisions?** No. Soft cross-entropy against
good target distributions is already a proper-score optimum; the RL term is a
noisy estimate of the same gradient and does not beat CE on calibration or
accuracy.

**What actually improves accuracy?** The input option-token budget. Matching the
checkpoint's 1024-token context and 256-token option budget raised
typed-decisions accuracy from 0.782 to 0.789, and roughly doubled zero-shot
accuracy on the 77-option Banking77 task.

**Why does the fitted temperature matter?** A value above 1 means the raw
outputs are over-sharp. It rises with the training noise scale, so temperature
scaling is undoing the RL objective rather than just cleaning up after it.

## Citation

```bibtex
@misc{leduc2026sev,
  title  = {Dissecting RLCD: What Reinforcement Learning Does (and Doesn't) Do for Calibrated Typed Decisions},
  author = {Le Duc Minh},
  year   = {2026},
  url    = {https://github.com/LakoreAI/sev}
}
```

## License

Released under the repository [LICENSE](LICENSE).
