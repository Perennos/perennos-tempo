"""Build a chronological held-out calibration candidate; activation requires an interactive owner approval."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import statistics
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path


def _sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def _latest(rows):
    by_attempt = {}
    for row in rows:
        if isinstance(row, dict) and row.get("attempt_id"):
            by_attempt[row["attempt_id"]] = row
    return [r for r in by_attempt.values() if r.get("verdict") in {"accept", "edit", "reject"}]


def _outcome(row, edit_weight, implicit_weight):
    score = {"accept": 1.0, "edit": edit_weight, "reject": 0.0}[row["verdict"]]
    return score * (implicit_weight if row.get("verdict_by") == "implicit" else 1.0)


def _pair_score(a, b):
    # Ties in prediction or held-out outcome count as half a correct ordering.
    if a == 0 or b == 0:
        return 0.5
    return 1.0 if (a > 0) == (b > 0) else 0.0


def generate(rows, tenant, config_sha256, ledger_sha256, now=None, ledger_path=None, config_path=None,
             edit_weight=0.5, implicit_weight=0.5):
    if ledger_path is not None and _sha(ledger_path) != ledger_sha256:
        raise ValueError("ledger hash does not match the requested calibration snapshot")
    if config_path is not None and _sha(config_path) != config_sha256:
        raise ValueError("config hash does not match the requested calibration snapshot")
    judged = _latest(rows)
    judged.sort(key=lambda r: (str(r.get("judged_at", "")), str(r.get("attempt_id", ""))))
    split_at = int(len(judged) * 0.8)
    training, held_out = judged[:split_at], judged[split_at:]
    split_cutoff = judged[split_at].get("judged_at") if split_at < len(judged) else None

    def key(row):
        return (row.get("role"), row.get("task_class"), row.get("policy_tier"), row.get("engine_id"), row.get("revision"))

    total, train, test = {}, {}, {}
    for row in judged:
        total.setdefault(key(row), []).append(row)
    for row in training:
        train.setdefault(key(row), []).append(row)
    for row in held_out:
        test.setdefault(key(row), []).append(row)

    eligible = []
    excluded = []
    for cell, all_rows in total.items():
        tr, ho = train.get(cell, []), test.get(cell, [])
        if len(all_rows) < 20 or len(tr) < 16 or len(ho) < 4 or not cell[2]:
            excluded.append({"cell": list(cell), "total": len(all_rows), "train": len(tr), "held_out": len(ho),
                             "reason": "needs 20 judgements, 16 chronological training rows, 4 held-out rows and a policy tier"})
            continue
        first = all_rows[0]
        if not all(isinstance(r.get("deterministic_rank"), int) and r["deterministic_rank"] >= 0 for r in all_rows):
            excluded.append({"cell": list(cell), "total": len(all_rows), "reason": "deterministic_rank missing"})
            continue
        tr_score = sum(_outcome(r, edit_weight, implicit_weight) for r in tr)
        pred = (tr_score + 1.0) / (len(tr) + 2.0)
        observed = sum(_outcome(r, edit_weight, implicit_weight) for r in ho) / len(ho)
        rank = statistics.median(r["deterministic_rank"] for r in all_rows)
        eligible.append({"cell": cell, "predicted": pred, "observed": observed, "deterministic_rank": rank,
                         "holdout_count": len(ho), "policy_tier": first.get("policy_tier")})

    pairs = []
    groups = {}
    for item in eligible:
        group = item["cell"][:3]
        groups.setdefault(group, []).append(item)
    rating_correct = baseline_correct = 0.0
    for group, cells in groups.items():
        for i, left in enumerate(cells):
            for right in cells[i + 1:]:
                observed = left["observed"] - right["observed"]
                rating = left["predicted"] - right["predicted"]
                baseline = right["deterministic_rank"] - left["deterministic_rank"]
                rating_correct += _pair_score(rating, observed)
                baseline_correct += _pair_score(baseline, observed)
                pairs.append({"group": list(group), "left": left["cell"][3], "right": right["cell"][3],
                              "rating_correct": _pair_score(rating, observed),
                              "deterministic_correct": _pair_score(baseline, observed)})
    n = len(pairs)
    rating_accuracy = rating_correct / n if n else None
    baseline_accuracy = baseline_correct / n if n else None
    passed = bool(n) and rating_accuracy >= 0.5 and rating_accuracy >= baseline_accuracy
    clock = now or datetime.now(timezone.utc)
    expires = (clock + timedelta(days=30)).replace(microsecond=0).isoformat().replace("+00:00", "Z")
    return {"kind": "tempo.calibration.v1", "tenant": tenant, "config_sha256": config_sha256,
            "ledger_sha256": ledger_sha256, "held_out_split": "chronological-80/20",
            "split_cutoff": split_cutoff, "created_at": clock.replace(microsecond=0).isoformat().replace("+00:00", "Z"),
            "expires_at": expires, "passed": passed,
            "metrics": {"judged_rows": len(judged), "eligible_cells": len(eligible), "excluded_cells": excluded,
                        "rankable_pairs": n, "rating_pairwise_accuracy": rating_accuracy,
                        "deterministic_pairwise_accuracy": baseline_accuracy, "pair_results": pairs},
            "method": "Predicted per-engine acceptance is learned on the first 80% of tenant judgements; held-out pairwise order is compared with the recorded deterministic policy rank within the same role, task class and policy tier. This is observational and does not establish counterfactual engine quality."}


def _read_rows(path):
    rows = []
    if not path.exists():
        return rows
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if line.strip():
            try:
                rows.append(json.loads(line))
            except ValueError as exc:
                raise ValueError(f"invalid ledger JSON on line {line_number}") from exc
    return rows


def _atomic_write(path, obj):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(obj, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def main():
    ap = argparse.ArgumentParser(description="Build a held-out Perennos Tempo ratings calibration")
    ap.add_argument("--approve", action="store_true", help="interactive owner approval; never approved by generation alone")
    ap.add_argument("--output", help="write a candidate file; without --approve it remains unreviewed")
    ap.add_argument("--edit-weight", type=float, default=0.5)
    ap.add_argument("--implicit-weight", type=float, default=0.5)
    args = ap.parse_args()
    from tempo_home import DATA, LEDGER_DIR, T
    ledger = LEDGER_DIR / "outcomes.jsonl"
    config = DATA / "config.json"
    candidate = generate(_read_rows(ledger), T["tenant"], _sha(config) if config.exists() else "",
                         _sha(ledger) if ledger.exists() else hashlib.sha256(b"").hexdigest(),
                         ledger_path=ledger if ledger.exists() else None, config_path=config if config.exists() else None,
                         edit_weight=args.edit_weight, implicit_weight=args.implicit_weight)
    if args.approve:
        if not candidate["passed"]:
            raise SystemExit("refused: held-out calibration did not pass; no calibration file was changed")
        if not sys.stdin.isatty():
            raise SystemExit("refused: --approve requires an interactive owner terminal")
        typed = input(f"Type APPROVE {T['tenant']} to activate this calibration: ").strip()
        if typed != f"APPROVE {T['tenant']}":
            raise SystemExit("refused: tenant confirmation did not match; no calibration file was changed")
        candidate["reviewed_by"] = f"owner:{T['tenant']}"
    output = Path(args.output) if args.output else (DATA / "ledger-calibration.json" if args.approve else None)
    if output:
        _atomic_write(output, candidate)
        candidate["written_to"] = str(output)
    print(json.dumps(candidate, indent=1))


if __name__ == "__main__":
    main()
