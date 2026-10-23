"""Calibration evaluation for typed decisions: raw & post-temperature ECE,
Brier, NLL, accuracy.

The per-pass work lives in `collect_rows` (one forward pass, unpadded per
row) and `raw_metrics`/`apply_temperature` (metrics on those rows).
`evaluate` is the public entry point: it fits per-(type, K-bucket)
temperature on a calibration loader (never the test split — Laya's own
issue #186 warns fitting temperature on training items inflates it),
applies it to a test loader, and optionally
persists both raw and post-temperature results. `eval_per_epoch` is the
periodic-validation hook used by `src.pipelines.train` — it only reports
raw metrics, since fitting temperature every epoch is unnecessary and the
monitored metric (`raw_ece`) is deliberately the *pre-calibration* one.
"""

from collections.abc import Sequence
from pathlib import Path

import torch

from src.config import QTYPES
from src.modules.model import DecisionModel
from src.utils.io_utils import load_json, save_json


def k_bucket(k: int, boundaries: tuple[int, ...] = (2, 5, 10)) -> int:
    """Option-count bucket index: 2 / 3..boundaries[1] / boundaries[1]+1..
    boundaries[2] / 11+. Matches Laya's own `temp_bucket` boundaries
    (laya/common.py) and `DecisionModelConfig.k_buckets`'s default.
    """
    for i, b in enumerate(boundaries):
        if k <= b:
            return i
    return len(boundaries)


def expected_calibration_error(
    confidences: torch.Tensor, correct: torch.Tensor, n_bins: int = 15
) -> float:
    """Standard equal-width-bin ECE on `answer_confidence = max(p)` — not
    the entropy-based `confidence` Laya also reports (see
    `laya/common.py::answer_confidence` vs. `confidence_from_probs`); only
    the former is what temperature scaling is fit against."""
    edges = torch.linspace(0, 1, n_bins + 1)
    ece = 0.0
    n = confidences.shape[0]
    if n == 0:
        return float("nan")
    for i in range(n_bins):
        lo, hi = edges[i], edges[i + 1]
        in_bin = (confidences > lo) & (confidences <= hi)
        if i == 0:
            in_bin = in_bin | (confidences == lo)
        count = int(in_bin.sum())
        if count == 0:
            continue
        acc = correct[in_bin].float().mean()
        conf = confidences[in_bin].mean()
        ece += (count / n) * float((acc - conf).abs())
    return ece


@torch.no_grad()
def collect_rows(model: DecisionModel, loader, device: torch.device) -> list[dict]:
    """One forward pass over `loader`; returns a flat list of per-question
    rows `{"logits", "target", "qtype", "k"}`, trimmed to each row's true
    (unpadded) option count so rows with different K can be grouped and
    re-padded independently downstream (`fit_temperatures`).
    """
    model.eval()
    rows: list[dict] = []
    for batch in loader:
        ids = batch["ids"].to(device)
        attention_mask = batch["attention_mask"].to(device)
        marker_pos = batch["marker_pos"].to(device)
        marker_mask = batch["marker_mask"].to(device)
        qtype = batch["qtype"].to(device)

        logits = model(ids, attention_mask, marker_pos, marker_mask, qtype).cpu()
        marker_mask_cpu = marker_mask.cpu()
        qtype_cpu = qtype.cpu()
        target = batch["target"]
        label = batch.get("label")

        for i in range(logits.shape[0]):
            k = int(marker_mask_cpu[i].sum())
            rows.append(
                {
                    "logits": logits[i, :k],
                    "target": target[i, :k],
                    "qtype": int(qtype_cpu[i]),
                    "k": k,
                    "label": int(label[i]) if label is not None else -1,
                }
            )
    return rows


def _gold_index(row: dict) -> int:
    """Gold option index: the hard `label` when the row has one (what Laya's
    own evaluation scores against), else the soft target's argmax."""
    label = row.get("label", -1)
    return label if 0 <= label < row["k"] else int(row["target"].argmax())


def _rows_to_metrics(rows: list[dict], n_bins: int = 15) -> dict[str, object]:
    confidences, corrects, briers, nlls, soft_accs, score_abs_errs, target_maxes = (
        [],
        [],
        [],
        [],
        [],
        [],
        [],
    )
    for row in rows:
        p = torch.softmax(row["logits"], dim=-1)
        t = row["target"]
        confidences.append(float(p.max()))
        corrects.append(int(int(p.argmax()) == _gold_index(row)))
        briers.append(float(((p - t) ** 2).sum()))
        nlls.append(float(-(t * p.clamp_min(1e-12).log()).sum()))
        soft_accs.append(float((p * t).sum()))
        target_maxes.append(float(t.max()))
        if row["qtype"] == QTYPES["score"]:
            levels = torch.arange(row["k"], dtype=p.dtype)
            score_abs_errs.append(float(((p - t) * levels).sum().abs()))

    n = len(rows)
    return {
        "ece": expected_calibration_error(
            torch.tensor(confidences), torch.tensor(corrects), n_bins
        ),
        "brier": sum(briers) / n if n else float("nan"),
        "nll": sum(nlls) / n if n else float("nan"),
        "accuracy": sum(corrects) / n if n else float("nan"),
        "soft_accuracy": sum(soft_accs) / n if n else float("nan"),
        "mean_confidence": sum(confidences) / n if n else float("nan"),
        # Sharpness relative to the soft targets (the §V claim's reference
        # point): > 0 means the model's max-prob exceeds the target's.
        "mean_target_max": sum(target_maxes) / n if n else float("nan"),
        "score_mae": (
            sum(score_abs_errs) / len(score_abs_errs) if score_abs_errs else None
        ),
        "n": n,
    }


def _metrics_with_breakdown(rows: list[dict], n_bins: int = 15) -> dict[str, object]:
    """Overall metrics plus the same dict per question type under `by_type`."""
    result = _rows_to_metrics(rows, n_bins)
    names = {v: k for k, v in QTYPES.items()}
    result["by_type"] = {
        names[qt]: _rows_to_metrics([r for r in rows if r["qtype"] == qt], n_bins)
        for qt in sorted({r["qtype"] for r in rows})
    }
    return result


def raw_metrics(rows: list[dict], n_bins: int = 15) -> dict[str, object]:
    return _rows_to_metrics(rows, n_bins)


def fit_temperature(
    logits: torch.Tensor, target: torch.Tensor, mask: torch.Tensor, max_iter: int = 100
) -> float:
    """LBFGS-fit a scalar temperature minimizing NLL of `target` against
    `softmax(logits / T)` (matches `laya/common.py`'s fitting approach).
    `logits`/`target`/`mask`: (N, K), already padded to a common K.
    """
    mask_bool = mask.bool()
    log_t = torch.zeros(1, requires_grad=True)
    opt = torch.optim.LBFGS([log_t], lr=0.1, max_iter=max_iter)

    def closure():
        opt.zero_grad()
        z = logits.masked_fill(~mask_bool, -1e4) / log_t.exp()
        loss = -(target * torch.log_softmax(z, dim=-1)).sum(-1).mean()
        loss.backward()
        return loss

    opt.step(closure)
    return float(log_t.detach().exp().clamp(0.5, 5.0))


def fit_temperatures(
    rows: list[dict], boundaries: tuple[int, ...] = (2, 5, 10)
) -> dict[tuple[int, int], float]:
    """Fit one temperature per (qtype, K-bucket) group present in `rows`.

    `boundaries` must match `DecisionModelConfig.k_buckets` when the result is
    written back into the model's temperature buffer.
    """
    groups: dict[tuple[int, int], list[dict]] = {}
    for row in rows:
        key = (row["qtype"], k_bucket(row["k"], boundaries))
        groups.setdefault(key, []).append(row)

    temperatures = {}
    for key, group_rows in groups.items():
        max_k = max(row["k"] for row in group_rows)
        logits = torch.stack(
            [_pad_1d(row["logits"], max_k, -1e4) for row in group_rows]
        )
        target = torch.stack([_pad_1d(row["target"], max_k, 0.0) for row in group_rows])
        mask = torch.stack(
            [_pad_1d(torch.ones(row["k"]), max_k, 0.0).bool() for row in group_rows]
        )
        temperatures[key] = fit_temperature(logits, target, mask)
    return temperatures


def _pad_1d(x: torch.Tensor, length: int, value: float) -> torch.Tensor:
    if x.shape[0] == length:
        return x
    pad = torch.full((length - x.shape[0],), value, dtype=x.dtype)
    return torch.cat([x, pad])


def _scale_rows(
    rows: list[dict],
    temperatures: dict[tuple[int, int], float],
    boundaries: tuple[int, ...] = (2, 5, 10),
) -> list[dict]:
    scaled_rows = []
    for row in rows:
        key = (row["qtype"], k_bucket(row["k"], boundaries))
        t = temperatures.get(key, 1.0)
        scaled_rows.append({**row, "logits": row["logits"] / t})
    return scaled_rows


def fill_model_temperature(
    model: DecisionModel,
    temperatures: dict[tuple[int, int], float],
    boundaries: tuple[int, ...] = (2, 5, 10),
) -> None:
    """Write fitted temperatures into `model.temperature` (a
    `(num_qtypes, num_buckets)` buffer) so the calibration travels with the
    checkpoint's state_dict. Groups absent from `temperatures` stay at 1.0.
    """
    with torch.no_grad():
        model.temperature.fill_(1.0)
        for (qtype, bucket), value in temperatures.items():
            if (
                0 <= qtype < model.temperature.shape[0]
                and 0 <= bucket < model.temperature.shape[1]
            ):
                model.temperature[qtype, bucket] = value


def temperature_for(
    model: DecisionModel,
    qtype: int,
    k: int,
    boundaries: tuple[int, ...] = (2, 5, 10),
) -> float:
    """Fitted temperature for one row's (qtype, option count), defaulting to
    1.0 for an unknown group."""
    bucket = k_bucket(k, boundaries)
    if (
        0 <= qtype < model.temperature.shape[0]
        and 0 <= bucket < model.temperature.shape[1]
    ):
        return float(model.temperature[qtype, bucket])
    return 1.0


def save_temperatures(
    temperatures: dict[tuple[int, int], float],
    boundaries: tuple[int, ...],
    path: str | Path,
) -> None:
    """Persist fitted temperatures for `src.pipelines.infer` — the JSON
    sidecar written next to a checkpoint.
    """
    save_json(
        {
            "k_buckets": list(boundaries),
            "temperatures": [
                {"qtype": int(q), "bucket": int(b), "T": float(t)}
                for (q, b), t in sorted(temperatures.items())
            ],
        },
        path,
    )


def load_temperatures(
    path: str | Path,
) -> tuple[dict[tuple[int, int], float], tuple[int, ...]]:
    """Inverse of `save_temperatures`: returns
    `(temperatures, boundaries)`.
    """
    data = load_json(path)
    boundaries = tuple(int(b) for b in data.get("k_buckets", (2, 5, 10)))
    temperatures = {
        (int(e["qtype"]), int(e["bucket"])): float(e["T"]) for e in data["temperatures"]
    }
    return temperatures, boundaries


def apply_temperature(
    rows: list[dict],
    temperatures: dict[tuple[int, int], float],
    n_bins: int = 15,
    boundaries: tuple[int, ...] = (2, 5, 10),
) -> dict[str, object]:
    """Re-score `rows` with `logits / T[qtype, k_bucket]` and report the
    same metric dict as `raw_metrics`.
    """
    return _rows_to_metrics(_scale_rows(rows, temperatures, boundaries), n_bins)


@torch.no_grad()
def paired_bootstrap_diff(
    a: Sequence[float] | torch.Tensor,
    b: Sequence[float] | torch.Tensor,
    n_boot: int = 2000,
    seed: int = 0,
    alpha: float = 0.05,
) -> dict[str, float]:
    """Paired bootstrap CI for the mean difference ``a - b`` over shared items.

    Use it on per-row scores (e.g. NLL) of two configurations evaluated on the
    same test rows, so the item-to-item spread cancels and only the systematic
    gap is tested. Returns the observed ``mean``, the ``[lo, hi]`` percentile
    interval, and a two-sided bootstrap ``p`` (twice the fraction of resampled
    means on the opposite side of zero, capped at 1).
    """
    a_t = torch.as_tensor(a, dtype=torch.float64)
    b_t = torch.as_tensor(b, dtype=torch.float64)
    if a_t.shape != b_t.shape:
        raise ValueError(f"paired inputs must match: {a_t.shape} vs {b_t.shape}")
    diff = a_t - b_t
    n = diff.shape[0]
    if n == 0:
        nan = float("nan")
        return {"mean": nan, "lo": nan, "hi": nan, "p": nan, "n": 0}
    generator = torch.Generator().manual_seed(seed)
    idx = torch.randint(0, n, (n_boot, n), generator=generator)
    boot = diff[idx].mean(dim=1)
    p = 2 * min(float((boot <= 0).double().mean()), float((boot >= 0).double().mean()))
    return {
        "mean": float(diff.mean()),
        "lo": float(torch.quantile(boot, alpha / 2)),
        "hi": float(torch.quantile(boot, 1 - alpha / 2)),
        "p": min(p, 1.0),
        "n": n,
    }


@torch.no_grad()
def eval_per_epoch(
    model: DecisionModel, loader, device: torch.device
) -> dict[str, object]:
    """Periodic-validation hook for `src.pipelines.train`. Reports only raw
    (pre-temperature) metrics under the `raw_*` keys `TrainerState.extra`
    expects (`best_metric: raw_ece` in `TrainingConfig`).
    """
    if loader is None:
        return {}
    rows = collect_rows(model, loader, device)
    result = raw_metrics(rows)
    return {f"raw_{k}": v for k, v in result.items()}


def evaluate(
    model: DecisionModel,
    calib_loader,
    test_loader,
    device: torch.device,
    save_json_path: str | Path | None = None,
    save_temperatures_path: str | Path | None = None,
) -> dict[str, object]:
    """Full evaluation: fit temperature on `calib_loader`, report raw and
    post-temperature metrics on `test_loader`, plus the fitted temperatures
    themselves. Fitted values are also written into `model.temperature` so a
    checkpoint saved afterwards carries its calibration, and optionally to a
    JSON sidecar for `src.pipelines.infer`.
    """
    boundaries = tuple(model.cfg.k_buckets)
    calib_rows = collect_rows(model, calib_loader, device)
    test_rows = collect_rows(model, test_loader, device)

    temperatures = fit_temperatures(calib_rows, boundaries)
    fill_model_temperature(model, temperatures, boundaries)
    result = {
        "raw": _metrics_with_breakdown(test_rows),
        "post_temperature": _metrics_with_breakdown(
            _scale_rows(test_rows, temperatures, boundaries)
        ),
        "fitted_temperature": {
            f"type{k[0]}_bucket{k[1]}": v for k, v in temperatures.items()
        },
    }
    if save_json_path is not None:
        save_json(result, save_json_path)
    if save_temperatures_path is not None:
        save_temperatures(temperatures, boundaries, save_temperatures_path)
    return result


def format_report(result: dict[str, object]) -> str:
    raw, post = result["raw"], result["post_temperature"]
    lines = [
        (
            f"raw:   ECE={raw['ece'] * 100:6.2f}  Brier={raw['brier']:.4f}  "
            f"NLL={raw['nll']:.4f}  acc={raw['accuracy'] * 100:6.2f}  n={raw['n']}"
        ),
        (
            f"postT: ECE={post['ece'] * 100:6.2f}  Brier={post['brier']:.4f}  "
            f"NLL={post['nll']:.4f}  acc={post['accuracy'] * 100:6.2f}  n={post['n']}"
        ),
    ]
    for key, t in sorted(result["fitted_temperature"].items()):
        lines.append(f"  T[{key}] = {t:.3f}")
    return "\n".join(lines)
