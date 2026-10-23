"""Perennos Tempo ratings: per-engine Beta-Binomial scorer + Sev job-risk head, merged per Sol and Argon reviews (17 Oct 2026).

  python tempo_ratings.py plan --request JSON      # ratings (or abstain) + receipt, appends a shadow record
  python tempo_ratings.py record --outcome JSON    # append one judged outcome to the ledger
  python tempo_ratings.py board                    # living tier board from the ledger (S/A/B/C/unranked)
  python tempo_ratings.py selftest                 # synthetic, offline checks of the rules below

How it decides (all inference until the ledger proves it):
1. Engine score = posterior mean of Beta(a0 + accepts, b0 + rejects) for the cell (engine REVISION, role, task_class).
   Prior from a dated public benchmark score s0 for that role (priors.json): a0 = N0*s0, b0 = N0*(1-s0), N0 = 8.
   The prior's weight fades as N0/(N0+n). A new model revision starts a new cell (non-stationarity: Argon).
2. Ratings reorder engines only inside a policy tier (toolbox/chain.py). Tier precedence is never crossed (Sol, change 2).
3. Sev runs ONCE per job, engine-agnostic: P(accept/edit/reject) for the job itself. It is shown and recorded,
   and later becomes a feature; it never selects an engine and never grants permission (Sol change 1, Argon section 1).
4. Exploration: only low-stakes classes, only between the top two eligible engines of the same tier when their
   90% intervals overlap, epsilon 0.10, and the sampling probability is written to the receipt (Argon section 2).
5. MODE (config.json): "shadow" (default) computes everything and writes the receipt but returns no ratings, so the
   planner's deterministic order stands; "live" returns ratings only for cells with >= 20 judged jobs and a passed,
   reviewed calibration (Sol change 4). Auto is never enabled from here.
"""
import argparse, json, math, os, random, sys, time, uuid
from pathlib import Path

HOME = Path(__file__).resolve().parent                # code
sys.path.insert(0, str(HOME))
from tempo_home import T, DATA, LEDGER_DIR             # data: one folder per owner or client (TEMPO_HOME)

LEDGER = LEDGER_DIR / "outcomes.jsonl"                # append-only judged outcomes (V2-1 will own the real ledger)
SHADOW = LEDGER_DIR / "shadow-decisions.jsonl"        # what the scorer would have done, with receipts
CONFIG = DATA / "config.json"
PRIORS = DATA / "priors.json"
ENGINES = DATA / "engines.json"
CALIBRATION = DATA / "ledger-calibration.json"
DEFAULT_CONFIG = {"mode": "shadow", "N0": 8, "min_judged_live": 20, "epsilon": 0.10,
                  "low_stakes_classes": ["docs", "summary", "lint", "draft"], "use_sev_job_risk": True,
                  "implicit_weight": 0.5}
NEVER_EXPLORE = set(T["never_explore"])               # per tenant (tenant.json)
LOCAL_ONLY = set(T["local_only"])


def _cfg():
    c = dict(DEFAULT_CONFIG)
    if CONFIG.exists():
        c.update(json.loads(CONFIG.read_text(encoding="utf-8")))
    return c


def _priors():
    return json.loads(PRIORS.read_text(encoding="utf-8")) if PRIORS.exists() else {"priors": {}}


def _sha_file(p):
    import hashlib
    return hashlib.sha256(p.read_bytes()).hexdigest() if p.exists() else None


def calibration_ok():
    """A calibration counts only if it parses, passed, was reviewed, is for this tenant and config, and has not expired."""
    if not CALIBRATION.exists():
        return False, "no calibration"
    try:
        c = json.loads(CALIBRATION.read_text(encoding="utf-8"))
    except ValueError:
        return False, "calibration does not parse"
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    checks = {"passed": c.get("passed") is True, "reviewed": bool(c.get("reviewed_by")),
              "tenant": c.get("tenant") == T["tenant"], "config": c.get("config_sha256") == _sha_file(CONFIG),
              "ledger_snapshot": bool(c.get("ledger_sha256")) and c.get("ledger_sha256") == _sha_file(LEDGER),
              "held_out_split": bool(c.get("held_out_split")),
              "fresh": bool(c.get("expires_at")) and c["expires_at"] > now}
    bad = [k for k, ok in checks.items() if not ok]
    return (not bad), ("calibrated" if not bad else "calibration fails: " + ", ".join(bad))


def _engines():
    return json.loads(ENGINES.read_text(encoding="utf-8")).get("engines", []) if ENGINES.exists() else []


def _ledger(path=None):
    path = path or LEDGER
    if not path.exists():
        return []
    rows = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    # corrections are append-only: the latest verdict for an attempt_id wins
    latest = {}
    for r in rows:
        latest[r["attempt_id"]] = r
    return [r for r in latest.values() if r.get("verdict") in ("accept", "edit", "reject")]


def _beta_ppf(q, a, b):
    """Normal approximation to a Beta quantile (enough for interval-overlap checks)."""
    m = a / (a + b); v = a * b / ((a + b) ** 2 * (a + b + 1))
    z = {0.05: -1.6449, 0.95: 1.6449}[q]
    return min(1.0, max(0.0, m + z * math.sqrt(v)))


def cell_stats(engine_id, revision, role, task_class, rows, cfg, priors):
    prior = priors.get("priors", {}).get(role, {}).get(engine_id, {})
    prior = prior.get("by_task_class", {}).get(task_class) or prior     # fleet cell for this task class wins
    s0 = prior.get("score")
    N0 = prior.get("N0", cfg["N0"])                          # match strength: exact 8, probable 6, family 3
    a0, b0 = (N0 * s0, N0 * (1 - s0)) if s0 is not None else (1.0, 1.0)
    n = k = 0
    w_edit = cfg.get("edit_weight", 0.5)                     # accepted after edits: half a success, not a rejection
    w_implicit = cfg.get("implicit_weight", 0.5)              # implicit outcomes are half their explicit equivalent
    for r in rows:
        if (r["engine_id"], r.get("revision"), r["role"], r["task_class"]) == (engine_id, revision, role, task_class):
            factor = w_implicit if r.get("verdict_by") == "implicit" else 1.0
            success = 1.0 if r["verdict"] == "accept" else (w_edit if r["verdict"] == "edit" else 0.0)
            n += 1; k += factor * success
    a, b = a0 + k, b0 + n - k
    pinned = bool(revision) and revision != "as-served" and not str(revision).startswith("<")
    return {"engine_id": engine_id, "revision": revision, "identity": "pinned" if pinned else "unpinned",
            "n_judged": n, "accepts": round(k, 2),
            "prior_score": s0, "prior_source": prior.get("source") or prior.get("benchmark"),
            "mean": round(a / (a + b), 4), "lo90": round(_beta_ppf(0.05, a, b), 4), "hi90": round(_beta_ppf(0.95, a, b), 4)}


def job_risk(request):
    """Sev once per job: P(accept/edit/reject) of the job itself. Advisory only."""
    try:
        from tempo_decision_head import score
        state = json.dumps({"task_class": request.get("task_class"), "role": request.get("role"),
                            "summary": request.get("summary", "")[:1500], "history": request.get("history", "")[:1500]})
        q = {"type": "choice", "instructions": "How will the owner judge the result of this job?",
             "criteria": {"accept": "accepted as is", "edit": "accepted after edits", "reject": "rejected"}}
        s = score(state, q)
        return {"status": "ok", "advisory_only": True, **s}
    except Exception as e:                                   # missing head never blocks planning
        return {"status": "unavailable", "reason": type(e).__name__}


def profile_eligible(request, engine, tenant, profile):
    """Scorer gate. The gateway rechecks independently immediately before HTTP."""
    if not profile.get('uncensored') and (engine.get('uncensored') or any(s in engine.get('model','').lower() for s in ('uncensored','abliterat','heretic','dolphin'))):
        return False
    if engine.get("locality") == "local":
        return True
    from tempo_profiles import hosted_allowed
    return engine.get("locality") == "hosted" and hosted_allowed(request, profile, tenant)


def plan(request, rng=random.random, *, lease_held=False, risk_evaluator=None):
    """Plan one job. Sev inference is allowed only under an already-held GPU lease."""
    cfg, pri, rows = _cfg(), _priors(), _ledger()
    if cfg["use_sev_job_risk"] and not lease_held and risk_evaluator is None:
        raise RuntimeError("Sev job-risk inference requires the shared GPU lease")
    original_tiers = request.get("tiers", [])
    tiers = original_tiers
    from tempo_profiles import load as load_profiles, resolve as resolve_profile
    selected_profile, _ = resolve_profile(load_profiles(DATA), request.get("model", "tempo/auto"), request.get("workflow"), request.get("privacy_class"))
    registry = {e["id"]: e for e in _engines()}
    profile_excluded = [e['id'] for tier in tiers for e in tier if not profile_eligible(request,registry.get(e['id'],{}),T,selected_profile)]
    tiers = [[e for e in tier if profile_eligible(request, registry.get(e["id"], {}), T, selected_profile)] for tier in tiers]
    tiers = [tier for tier in tiers if tier]
    privacy = None
    if {request.get("privacy_class"), request.get("task_class")} & LOCAL_ONLY:   # either label makes the job local-only
        loc = {e["id"]: e.get("locality") for e in _engines()}
        hosted = [e["id"] for t in original_tiers for e in t if loc.get(e["id"], "unknown") != "local"]
        tiers = [[e for e in t if e["id"] not in hosted] for t in tiers]
        tiers = [t for t in tiers if t]
        privacy = {"class": request.get("privacy_class"), "task_class": request.get("task_class"), "local_only": True,
                   "non_local_candidates": hosted,
                   "action": "excluded from ratings and exploration"}
    stats, explore, tier_ready = {}, None, []
    for tier in tiers:
        cells = [cell_stats(e["id"], e.get("revision"), request["role"], request["task_class"], rows, cfg, pri) for e in tier]
        for c in cells:
            stats[c["engine_id"]] = c
        # a tier is rated on its own once every member is pinned and judged enough; one new engine elsewhere
        # no longer silences every tier (Argon review, 17 Oct)
        tier_ready.append(all(c["identity"] == "pinned" and c["n_judged"] >= cfg["min_judged_live"] for c in cells))
        top = sorted(cells, key=lambda c: -c["mean"])[:2]
        low = selected_profile['exploration'] and request["task_class"] in cfg["low_stakes_classes"] and not (set([request["task_class"], request.get("privacy_class")]) & NEVER_EXPLORE)
        if explore is None and low and len(top) == 2 and top[1]["hi90"] >= top[0]["lo90"]:
            pick_second = rng() < cfg["epsilon"]
            explore = {"tier_engines": [c["engine_id"] for c in top], "epsilon": cfg["epsilon"],
                       "chosen": top[1]["engine_id"] if pick_second else top[0]["engine_id"],
                       "propensity": cfg["epsilon"] if pick_second else 1 - cfg["epsilon"]}
    ratings = {k: v["mean"] for k, v in stats.items()}
    if explore:                                              # lift the explored engine to the top of its own tier only
        ratings[explore["chosen"]] = max(ratings[e] for e in explore["tier_engines"]) + 1e-6
    cal_ok, cal_reason = calibration_ok() if cfg["mode"] == "live" else (False, "shadow mode")
    rated_ids = {e["id"] for t, ok in zip(tiers, tier_ready) if ok for e in t}
    live_ok = cfg["mode"] == "live" and bool(stats) and cal_ok and bool(rated_ids)
    ratings = {k: v for k, v in ratings.items() if k in rated_ids}   # unrated tiers keep the planner's order
    receipt = {"kind": "tempo.ratings.v1", "decision_id": uuid.uuid4().hex[:12],
               "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "mode": cfg["mode"],
               "evidence_cutoff": max((r.get("judged_at", "") for r in rows), default=None),
               "cells": stats, "exploration": explore, "privacy": privacy, "tenant": T["tenant"], "priors_as_of": pri.get("as_of"),
               "job_risk": (risk_evaluator or job_risk)(request) if cfg["use_sev_job_risk"] else {"status": "off"},
               "calibration": cal_reason, "eligible": len(stats), "tiers_ready": tier_ready,
               "returned": "ratings" if live_ok else "abstain", "profile": selected_profile["name"],
               "profile_excluded":profile_excluded}
    SHADOW.parent.mkdir(parents=True, exist_ok=True)
    with SHADOW.open("a", encoding="utf-8") as f:
        f.write(json.dumps({"request": {k: request.get(k) for k in ("task_class", "role", "privacy_class")}, **receipt}) + "\n")
    return {"status": "rated" if live_ok else "abstain", "ratings": ratings if live_ok else None,
            "reason": None if live_ok else f"mode={cfg['mode']}; rankings need >= {cfg['min_judged_live']} judged jobs per cell and a reviewed calibration",
            "receipt": receipt}


def record(outcome):
    need = {"attempt_id", "engine_id", "revision", "role", "task_class", "verdict", "verdict_by", "evidence"}
    missing = need - set(outcome)
    if missing:
        raise SystemExit(f"missing fields: {sorted(missing)}")
    if outcome["verdict"] not in ("accept", "edit", "reject", "unjudged"):
        raise SystemExit("verdict must be accept, edit, reject or unjudged")
    outcome.setdefault("judged_at", time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
    from tempo_store import append_jsonl
    append_jsonl(LEDGER, outcome)
    return {"ok": True}


def board():
    rows, cfg, pri = _ledger(), _cfg(), _priors()
    cells = {}
    for r in rows:
        cells.setdefault((r["role"], r["task_class"], r["engine_id"], r.get("revision")), None)
    out = []
    for (role, tc, eid, rev) in cells:
        s = cell_stats(eid, rev, role, tc, rows, cfg, pri)
        tier = "unranked" if s["n_judged"] < cfg["min_judged_live"] else \
            "S" if s["lo90"] >= 0.85 else "A" if s["lo90"] >= 0.70 else "B" if s["lo90"] >= 0.50 else "C"
        out.append({"role": role, "task_class": tc, "tier": tier, **s})
    return sorted(out, key=lambda x: (x["role"], x["task_class"], "SABCu".index(x["tier"][0]), -x["mean"]))


def selftest():
    global LEDGER, SHADOW, ENGINES, CALIBRATION
    import tempfile
    tmp = Path(tempfile.mkdtemp(prefix="tempo-ratings-"))
    LEDGER, SHADOW, ENGINES = tmp / "outcomes.jsonl", tmp / "shadow.jsonl", tmp / "engines.json"
    ENGINES.write_text(json.dumps({"engines": [{"id": "a", "locality": "local"}, {"id": "b", "locality": "local"},
                                               {"id": "c", "locality": "hosted"}]}), encoding="utf-8")
    checks = []
    req = {"task_class": "docs", "role": "writing-code", "privacy_class": "public",
           "tiers": [[{"id": "a", "revision": "r1"}, {"id": "b", "revision": "r1"}], [{"id": "c", "revision": "r1"}]]}
    p = plan(req, rng=lambda: 0.99, lease_held=True, risk_evaluator=lambda _: {"status": "synthetic-test"})
    checks.append(("shadow mode abstains", p["status"] == "abstain" and p["ratings"] is None))
    checks.append(("default Private profile records only local engine cells", set(p["receipt"]["cells"]) == {"a", "b"} and p['receipt']['profile']=='private'))
    for i in range(30):
        record({"attempt_id": f"x{i}", "engine_id": "a", "revision": "r1", "role": "writing-code", "task_class": "docs",
                "verdict": "accept" if i % 10 else "reject", "verdict_by": "test", "evidence": "synthetic"})
        record({"attempt_id": f"y{i}", "engine_id": "b", "revision": "r1", "role": "writing-code", "task_class": "docs",
                "verdict": "accept" if i % 2 else "reject", "verdict_by": "test", "evidence": "synthetic"})
    s = plan(req, rng=lambda: 0.99, lease_held=True, risk_evaluator=lambda _: {"status": "synthetic-test"})["receipt"]["cells"]
    checks.append(("evidence ranks a above b", s["a"]["mean"] > s["b"]["mean"]))
    s2 = cell_stats("a", "r2", "writing-code", "docs", _ledger(), _cfg(), {"priors": {}})
    checks.append(("new revision starts a fresh cell", s2["n_judged"] == 0))
    ed = cell_stats("e", "r1", "writing-code", "docs", [{"attempt_id": "e1", "engine_id": "e", "revision": "r1",
                    "role": "writing-code", "task_class": "docs", "verdict": "edit"}], _cfg(), {"priors": {}})
    checks.append(("an edit counts as half a success", ed["accepts"] == 0.5))
    checks.append(("an unpinned hosted revision is never rated", cell_stats("h", "as-served", "writing-code", "docs", [],
                   _cfg(), {"priors": {}})["identity"] == "unpinned"))
    record({"attempt_id": "x1", "engine_id": "a", "revision": "r1", "role": "writing-code", "task_class": "docs",
            "verdict": "reject", "verdict_by": "owner", "evidence": "correction"})
    checks.append(("corrections replace, not double count", cell_stats("a", "r1", "writing-code", "docs", _ledger(), _cfg(), {"priors": {}})["n_judged"] == 30))
    hi = dict(req, task_class="money")
    checks.append(("never explore money", plan(hi, rng=lambda: 0.0, lease_held=True, risk_evaluator=lambda _: {"status": "synthetic-test"})["receipt"]["exploration"] is None))
    CALIBRATION = tmp / "cal.json"
    CALIBRATION.write_text("{}", encoding="utf-8")
    checks.append(("a stub calibration file is not enough", calibration_ok()[0] is False))
    pt = plan(dict(req, task_class="private", privacy_class="public"), rng=lambda: 0.99, lease_held=True, risk_evaluator=lambda _: {"status": "synthetic-test"})["receipt"]
    checks.append(("a local-only task class excludes hosted engines even when labelled public", "c" not in pt["cells"]))
    pr = plan(dict(req, privacy_class="private"), rng=lambda: 0.99, lease_held=True, risk_evaluator=lambda _: {"status": "synthetic-test"})["receipt"]
    checks.append(("hosted engine excluded from a local-only job",
                   bool(pr["privacy"]) and pr["privacy"]["non_local_candidates"] == ["c"] and "c" not in pr["cells"]))
    b = board()
    checks.append(("board ranks a and b with 30 judged", {x["engine_id"] for x in b if x["tier"] != "unranked"} == {"a", "b"}))
    passed = sum(ok for _, ok in checks)
    return {"passed": passed, "failed": len(checks) - passed, "checks": checks, "tmp": str(tmp)}


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("plan"); p.add_argument("--request", required=True)
    p.add_argument("--lease-script", required=True); p.add_argument("--lease-need-mib", type=int, default=4096)
    r = sub.add_parser("record"); r.add_argument("--outcome", required=True)
    sub.add_parser("board"); sub.add_parser("selftest")
    a = ap.parse_args()
    def leased_plan():
        from tempo_leases import gpu_lease
        with gpu_lease(a.lease_script, "tempo-ratings-plan", a.lease_need_mib):
            return plan(json.loads(a.request), lease_held=True)
    out = {"plan": leased_plan, "record": lambda: record(json.loads(a.outcome)),
           "board": board, "selftest": selftest}[a.cmd]()
    print(json.dumps(out, indent=1, default=str))


if __name__ == "__main__":
    main()
