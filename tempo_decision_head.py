"""Perennos Tempo decision head: a calibrated, local scorer for typed decisions (Sev on ModernBERT-large).

  python tempo_decision_head.py selftest            # loads Sev on CPU, scores three fixed questions, prints timings
  python tempo_decision_head.py score --state S --question JSON
  python tempo_decision_head.py ratings --request JSON   # what Perennos Tempo's planner would receive

Configuration follows the independent design review of 17 Oct 2026:
- Prediction never grants permission. This module returns numbers and a receipt; it never dispatches, retries or approves.
- Ratings reorder engines only inside a policy tier (toolbox/chain.py). Each candidate gets its own binary
  P(accepted outcome), never a share of one "best engine" vote.
- Abstain by default: until a ledger calibration file exists and passes its kill test, `ratings` returns
  status "abstain" with no ratings, so the planner keeps its deterministic order.
- Offline and CPU-first: HF_HUB_OFFLINE is forced, weights and tokenizer load from the tenant's models_dir
  (tenant.json; fetch them once with fetch_models.py).
  The GPU is used only if TEMPO_HEAD_DEVICE=cuda is set, and then only under the shared GPU lease (caller's job).
Weights: LakoreAI/sev rev 08c6ae0b (Apache-2.0); code: github.com/LakoreAI/sev fefcfc33 (MIT);
encoder config/tokenizer: answerdotai/ModernBERT-large (pinned snapshot).
"""
import argparse, hashlib, json, os, sys, time
from pathlib import Path

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
from tempo_home import DATA, MODELS

SEV_SRC = ROOT / "sev-src"
WEIGHTS = MODELS / "sev" / "model.safetensors"
ENCODER = MODELS / "modernbert-large"
CALIBRATION = DATA / "ledger-calibration.json"      # written only by a reviewed ledger evaluation (V2 kill test)
PINS = {"sev_rev": "08c6ae0ba7ee58871418a4c439a1889289e7df6b",
        "sev_sha256": "be8aae6b150491cfe3d8049d69fce35dc06dfe4aac1db1d71a321b19432a9f9f",
        "sev_code": "fefcfc33d01c08104dc3de0374ad4e5daf07f923"}

_cache = {}


def _sha256(path, chunk=1 << 20):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while b := f.read(chunk):
            h.update(b)
    return h.hexdigest()


def load(verify=True):
    if "model" in _cache:
        return _cache["model"]
    if verify and _sha256(WEIGHTS) != PINS["sev_sha256"]:
        raise RuntimeError("sev weights do not match the pinned sha256")
    sys.path.insert(0, str(SEV_SRC))
    import torch
    from safetensors.torch import load_file
    from transformers import AutoConfig, AutoModel, AutoTokenizer
    from src.modules.model import DecisionModel
    from src.pipelines.eval import fill_model_temperature, load_temperatures
    from src.pipelines.infer import config_from_safetensors

    device = torch.device(os.environ.get("TEMPO_HEAD_DEVICE", "cpu"))
    torch.set_num_threads(max(1, min(os.cpu_count() or 4, int(os.environ.get("TEMPO_HEAD_THREADS", max(1, (os.cpu_count() or 4) // 2))))))
    cfg = config_from_safetensors(WEIGHTS)
    cfg.encoder_name = str(ENCODER)                      # local pinned snapshot, never the hub
    encoder = AutoModel.from_config(AutoConfig.from_pretrained(str(ENCODER)))
    model = DecisionModel(cfg, encoder=encoder)
    model.load_state_dict(load_file(str(WEIGHTS)))
    temps, bounds = load_temperatures(WEIGHTS.parent / "temperatures.json")
    fill_model_temperature(model, temps, bounds)
    model._temperature_boundaries = bounds
    model.to(device).eval()
    tok = AutoTokenizer.from_pretrained(str(ENCODER))
    _cache["model"] = (model, cfg, tok, device)
    return _cache["model"]


def score(state, question):
    """One typed question -> calibrated probabilities over its option keys."""
    load()                                               # puts sev-src on sys.path before its imports
    import torch
    from src.config import QTYPES
    from src.data import build_sequence, option_keys
    from src.modules.loss import probabilities
    from src.pipelines.eval import temperature_for
    model, cfg, tok, device = load()
    t0 = time.perf_counter()
    ids, markers = build_sequence(tok, state, question, cfg.max_len, cfg.head_max_len)
    ids_t = torch.tensor([ids], dtype=torch.long, device=device)
    with torch.no_grad():
        logits = model(ids_t, torch.ones_like(ids_t), torch.tensor([markers], device=device),
                       torch.ones((1, len(markers)), dtype=torch.bool, device=device),
                       torch.tensor([QTYPES[question["type"]]], device=device))
        T = temperature_for(model, QTYPES[question["type"]], len(markers), model._temperature_boundaries)
        probs = probabilities(logits / T)[0].tolist()
    keys = option_keys(question)
    return {"probabilities": dict(zip(keys, (round(p, 4) for p in probs))),
            "predicted_key": keys[max(range(len(probs)), key=probs.__getitem__)],
            "temperature": round(float(T), 4), "ms": round((time.perf_counter() - t0) * 1000, 1)}


def calibration_status():
    """Ratings are allowed only after a reviewed ledger evaluation beat its baselines (Sol, required change 4)."""
    from tempo_ratings import calibration_ok                 # one gate for both paths (Argon review, 17 Oct)
    ok, reason = calibration_ok()
    if ok:
        try:
            c = json.loads(CALIBRATION.read_text(encoding="utf-8"))
        except ValueError:
            return {"usable": False, "reason": "calibration does not parse"}
        if c.get("weights_sha256") != PINS["sev_sha256"]:
            return {"usable": False, "reason": "calibration is for other weights"}
    return {"usable": ok, "reason": reason}


def ratings(request):
    """Planner input: {task_class, role, privacy_class, candidates:[{id, ...}], history:"..."} ->
    ratings {engine_id: P(accepted)} plus a receipt, or an abstention that leaves chain order untouched."""
    status = calibration_status()
    receipt = {"kind": "tempo.decision_head.v1", "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "pins": PINS, "calibration": status["reason"],
               "candidates": [c.get("id") for c in request.get("candidates", [])]}
    from tempo_ratings import LOCAL_ONLY, _engines
    cands = request.get("candidates", [])
    if {request.get("privacy_class"), request.get("task_class")} & LOCAL_ONLY:
        loc = {e["id"]: e.get("locality") for e in _engines()}
        receipt["excluded_non_local"] = [c.get("id") for c in cands if loc.get(c.get("id"), "unknown") != "local"]
        cands = [c for c in cands if c.get("id") not in receipt["excluded_non_local"]]
    if not status["usable"]:
        return {"status": "abstain", "ratings": None, "reason": status["reason"], "receipt": receipt}
    if not cands:
        return {"status": "abstain", "ratings": None, "reason": "no eligible candidate", "receipt": receipt}
    out = {}
    for c in cands:
        state = json.dumps({"task_class": request.get("task_class"), "role": request.get("role"),
                            "privacy_class": request.get("privacy_class"), "engine": c,
                            "history": request.get("history", "")[:2000]})
        q = {"type": "noul", "instructions": "Will this engine's result for this job be accepted?",
             "criteria": {"yes": "accepted", "no": "edited or rejected"}}
        s = score(state, q)
        out[c["id"]] = s["probabilities"].get("yes", 0.0)
        receipt.setdefault("scores", {})[c["id"]] = s
    return {"status": "rated", "ratings": out, "receipt": receipt}


SELFTEST = [
    ("Job: rewrite a README for a local GPU tool. Draft passed lint and the owner approved two earlier drafts in this voice.",
     {"type": "noul", "instructions": "Should this draft be sent for owner review now?",
      "criteria": {"yes": "send for review", "no": "keep working"}}),
    ("Job: renew a TLS certificate on the home server. Last attempt failed with a permission error; nothing changed since.",
     {"type": "choice", "instructions": "What should happen next?",
      "criteria": {"retry": "run the same step again", "escalate": "ask the owner", "investigate": "read the logs first"}}),
    ("Job: summarise a 40-page PDF for a private family matter.",
     {"type": "choice", "instructions": "Which kind of engine fits this job?",
      "criteria": {"local": "a private model on the home machine", "hosted": "a cloud model", "none": "do not run"}}),
]


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("selftest")
    s = sub.add_parser("score"); s.add_argument("--state", required=True); s.add_argument("--question", required=True)
    r = sub.add_parser("ratings"); r.add_argument("--request", required=True)
    a = ap.parse_args()
    if a.cmd == "selftest":
        t0 = time.perf_counter(); load(); load_s = time.perf_counter() - t0
        res = [dict(state=st[:60], **score(st, q)) for st, q in SELFTEST]
        res2 = [score(st, q)["ms"] for st, q in SELFTEST]          # warm timings
        abst = ratings({"task_class": "docs", "role": "writing-code", "candidates": [{"id": "a"}, {"id": "b"}]})
        print(json.dumps({"load_s": round(load_s, 1), "results": res, "warm_ms": res2,
                          "ratings_without_calibration": abst["status"], "reason": abst["reason"]}, indent=1))
    elif a.cmd == "score":
        print(json.dumps(score(a.state, json.loads(a.question)), indent=1))
    else:
        print(json.dumps(ratings(json.loads(a.request)), indent=1))


if __name__ == "__main__":
    main()
