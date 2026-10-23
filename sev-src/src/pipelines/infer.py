"""Single-checkpoint inference: a state blob + one typed question ->
option probabilities.

Loads a checkpoint, rebuilds the model from its saved architecture config,
and scores one question against a state string.

Usage:
    uv run python -m src.pipelines.infer \\
        --ckpt checkpoints/<run>/best.pt \\
        --state '{"task": "...", "trace_summary": {...}}' \\
        --question '{"type": "choice", "instructions": "What should happen?", \\
                      "criteria": {"continue": "...", "stop": "..."}}'
"""

import argparse
import json
from dataclasses import fields
from pathlib import Path

import torch
from transformers import AutoTokenizer

from src.config import QTYPES, DecisionModelConfig
from src.data import build_sequence, option_keys
from src.modules.loss import predict, probabilities
from src.modules.model import DecisionModel
from src.pipelines.eval import (
    fill_model_temperature,
    load_temperatures,
    temperature_for,
)
from src.utils.model_utils import detect_device


def config_from_checkpoint(ckpt: dict) -> DecisionModelConfig:
    """Rebuild DecisionModelConfig from a checkpoint's saved `extra["cfg"]`."""
    saved = ckpt.get("extra", {}).get("cfg", {})
    known = {f.name for f in fields(DecisionModelConfig)}
    return DecisionModelConfig(**{k: v for k, v in saved.items() if k in known})


def config_from_safetensors(weights_path: Path) -> DecisionModelConfig:
    """Rebuild DecisionModelConfig from the `model_config.json` that
    `scripts/e2/export_to_hf.py` writes next to a weights-only
    `model.safetensors`, so the public HF artifacts can be used for
    inference without the original `.pt` checkpoint."""
    meta_path = Path(weights_path).parent / "model_config.json"
    if not meta_path.exists():
        raise FileNotFoundError(
            f"{meta_path.name} is required next to {Path(weights_path).name}"
        )
    saved = json.loads(meta_path.read_text())
    known = {f.name for f in fields(DecisionModelConfig)}
    return DecisionModelConfig(**{k: v for k, v in saved.items() if k in known})


def load_model(ckpt_path: Path, device: torch.device):
    ckpt_path = Path(ckpt_path)
    if not ckpt_path.exists():
        raise FileNotFoundError(f"checkpoint not found: {ckpt_path}")
    if ckpt_path.suffix == ".safetensors":
        from safetensors.torch import load_file

        raw = {"model": load_file(str(ckpt_path))}
        cfg = config_from_safetensors(ckpt_path)
    else:
        raw = torch.load(ckpt_path, map_location=str(device))
        cfg = config_from_checkpoint(raw)
    model = DecisionModel(cfg).to(device)
    model.load_state_dict(raw["model"])

    # A fitted-temperature sidecar (written by train.py's final evaluate) uses
    # the same K-bucket boundaries as the model; fall back to the config.
    boundaries = tuple(cfg.k_buckets)
    sidecar = ckpt_path.parent / "temperatures.json"
    if sidecar.exists():
        temperatures, boundaries = load_temperatures(sidecar)
        fill_model_temperature(model, temperatures, boundaries)
        print(f"applied fitted temperatures from {sidecar}")
    model._temperature_boundaries = boundaries

    model.eval()
    tokenizer = AutoTokenizer.from_pretrained(cfg.encoder_name)
    return model, cfg, tokenizer


@torch.no_grad()
def infer(
    ckpt_path: Path,
    state: str,
    question: dict,
    out_path: Path | None = None,
    apply_temperature: bool = True,
) -> dict:
    """Score `question` against `state` with the model at `ckpt_path`.

    Applies the checkpoint's fitted `softmax(z / T)` unless
    `apply_temperature=False` (`--raw` on the CLI), which is what the paper
    describes for inference.
    """
    device = detect_device()
    model, cfg, tokenizer = load_model(ckpt_path, device)

    ids, markers = build_sequence(
        tokenizer, state, question, cfg.max_len, cfg.head_max_len
    )
    ids_t = torch.tensor([ids], dtype=torch.long, device=device)
    attention_mask = torch.ones_like(ids_t)
    marker_pos = torch.tensor([markers], dtype=torch.long, device=device)
    marker_mask = torch.ones_like(marker_pos, dtype=torch.bool)
    qtype = torch.tensor([QTYPES[question["type"]]], dtype=torch.long, device=device)

    logits = model(ids_t, attention_mask, marker_pos, marker_mask, qtype)
    temperature = 1.0
    if apply_temperature:
        temperature = temperature_for(
            model,
            int(qtype.item()),
            len(markers),
            getattr(model, "_temperature_boundaries", tuple(cfg.k_buckets)),
        )
        logits = logits / temperature
    probs = probabilities(logits)
    pred_idx = int(predict(logits)[0])
    keys = option_keys(question)

    print(f"checkpoint: {ckpt_path}")
    print(f"question type: {question['type']}   options: {keys}")
    print(
        f"temperature: {temperature:.3f} ({'raw' if not apply_temperature else 'fitted'})"
    )
    print(
        f"predicted: {keys[pred_idx]}   "
        f"probabilities: {dict(zip(keys, (round(p, 4) for p in probs[0].tolist())))}"
    )

    result = {
        "logits": logits.cpu(),
        "probabilities": probs.cpu(),
        "predicted_key": keys[pred_idx],
        "option_keys": keys,
        "temperature": temperature,
    }
    if out_path is not None:
        torch.save(result, out_path)
        print(f"saved: {out_path}")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--ckpt", type=Path, required=True)
    parser.add_argument("--state", type=str, required=True, help="JSON state blob")
    parser.add_argument(
        "--question", type=str, required=True, help="JSON question dict"
    )
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument(
        "--raw",
        action="store_true",
        help="skip the fitted temperature (report softmax(z) instead of softmax(z/T))",
    )
    args = parser.parse_args()
    infer(
        args.ckpt,
        args.state,
        json.loads(args.question),
        args.out,
        apply_temperature=not args.raw,
    )
