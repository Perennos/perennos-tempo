"""Perennos Tempo compliance pack: one command turns a tenant's receipts into an audit bundle a reviewer can read.

  python tempo_audit.py pack [--since 2026-10-01] [--out <folder>]
  python tempo_audit.py verify <pack folder>        # recompute every hash and the receipt chain

The pack holds:
 - AUDIT-SUMMARY.md: the reviewer's page, covering the setup, what the concierge sent and where, every routing
   decision by privacy class, any non-local engine offered for a local-only job and what was done about it,
   engines by content digest, and who judged results.
 - decisions.jsonl, outcomes.jsonl and attempts.jsonl: metadata snapshots, hashed at pack time, not chained on append.
 - setup-receipt.json and concierge-receipt.json, as written at the time.
 - CHAIN.json: a pack-time decision/outcome hash chain and file hashes; append-chained receipts retain their window anchor.
It is a record of what the router decided and who judged it. It is not legal advice or a certification.
"""
import argparse, hashlib, json, shutil, stat, sys, time
from pathlib import Path

HOME = Path(__file__).resolve().parent
sys.path.insert(0, str(HOME))


def _jsonl(p):
    return [json.loads(l) for l in p.read_text(encoding="utf-8").splitlines() if l.strip()] if p.exists() else []


def _sha(b):
    return hashlib.sha256(b).hexdigest()


def chain(rows):
    h, out = "0" * 64, []
    for r in rows:
        h = _sha((h + json.dumps(r, sort_keys=True)).encode())
        out.append(h)
    return out


def pack(since=None, out=None):
    from tempo_home import DATA, T, LEDGER_DIR
    from tempo_receipts import verify_chain, ZERO_HASH, verified_snapshot, ReceiptIntegrityError
    try:
        all_receipts = verified_snapshot(LEDGER_DIR / 'receipts.jsonl')
    except ReceiptIntegrityError as exc:
        raise ValueError(str(exc)) from None
    dec = [r for r in _jsonl(LEDGER_DIR / "shadow-decisions.jsonl") if not since or r.get("at", "") >= since]
    outc = [r for r in _jsonl(LEDGER_DIR / "outcomes.jsonl") if not since or r.get("judged_at", "") >= since]
    attempts = [r for r in _jsonl(LEDGER_DIR / "attempts.jsonl") if not since or r.get("at", "") >= since]
    # Keep a contiguous suffix. Backdated rows after the first in-window row remain
    # included so clock corrections cannot create holes in the append chain.
    start = next((i for i,r in enumerate(all_receipts) if not since or r.get('recorded_at','') >= since), len(all_receipts))
    receipt_anchor = all_receipts[start-1]['receipt_hash'] if start else ZERO_HASH
    receipts = all_receipts[start:]
    out = Path(out) if out else DATA / "audit" / f"pack-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}"
    out.mkdir(parents=True, exist_ok=False)
    (out / "decisions.jsonl").write_text("".join(json.dumps(r) + "\n" for r in dec), encoding="utf-8")
    (out / "outcomes.jsonl").write_text("".join(json.dumps(r) + "\n" for r in outc), encoding="utf-8")
    (out / "attempts.jsonl").write_text("".join(json.dumps(r) + "\n" for r in attempts), encoding="utf-8")
    (out / "receipts.jsonl").write_text("".join(json.dumps(r) + "\n" for r in receipts), encoding="utf-8")
    for name, dst in (("setup-receipt.json", "setup-receipt.json"), ("concierge-proposal.json", "concierge-receipt.json")):
        if (DATA / name).exists():
            shutil.copy2(DATA / name, out / dst)
    for name in ("egress-policy.json", "profiles.json"):
        if (DATA / name).exists():
            shutil.copy2(DATA / name, out / name)
    if not (out / "egress-policy.json").exists():
        (out / "egress-policy.json").write_text(json.dumps({
            "schema": "perennos-tempo.egress-preview.v1", "enforcement": "not-applied",
            "firewall_changed": False, "reason": "No verified OS egress adapter is active."
        }, indent=1), encoding="utf-8")
    if T.get("vertical") == "fiduciary":
        vertical_spec = HOME / "verticals" / "fiduciary" / "vertical.json"
        if vertical_spec.exists():
            shutil.copy2(vertical_spec, out / "fiduciary-dpa-map.json")
    profile_jobs = {}
    for row in receipts:
        if row.get('profile') and row.get('job_id'):
            profile_jobs[row['job_id']] = {**profile_jobs.get(row['job_id'], {}), **row}
    profile_summary = {}
    screen_hits = []
    for job_id, row in profile_jobs.items():
        cell = profile_summary.setdefault(row['profile'], {'jobs':0,'hosted_calls':0,'actual_usd':0.0,'reserved_unknown_usd':0.0})
        cell['jobs'] += 1
        cell['hosted_calls'] += int(row.get('hosted') is True)
        if row.get('actual_usd') is not None: cell['actual_usd'] += float(row['actual_usd'])
        elif row.get('hosted'): cell['reserved_unknown_usd'] += float(row.get('reserved_usd', 0))
        screen = row.get('screen_result') or {}
        if screen.get('hit'): screen_hits.append({'job_id':job_id,'profile':row['profile'],'reasons':screen.get('reasons',[])})
    (out / 'PROFILE-SUMMARY.json').write_text(json.dumps({'profiles':profile_summary,'screen_hits':screen_hits},indent=2),encoding='utf-8')
    by_class, offered, engines = {}, [], {}
    for r in dec:
        pc = (r.get("request") or {}).get("privacy_class") or "unlabelled"
        by_class[pc] = by_class.get(pc, 0) + 1
        pv = r.get("privacy") or {}
        if pv.get("non_local_candidates"):
            offered.append({"decision": r.get("decision_id"), "at": r.get("at"), "class": pv.get("class"),
                            "task_class": pv.get("task_class"), "excluded": pv["non_local_candidates"]})
        for eid, c in (r.get("cells") or {}).items():
            engines[eid] = c.get("revision")
    judges = {}
    for o in outc:
        judges[o.get("verdict_by", "?")] = judges.get(o.get("verdict_by", "?"), 0) + 1
    setup = json.loads((out / "setup-receipt.json").read_text(encoding="utf-8")) if (out / "setup-receipt.json").exists() else {}
    conc = json.loads((out / "concierge-receipt.json").read_text(encoding="utf-8")) if (out / "concierge-receipt.json").exists() else {}
    lines = [f"# Audit summary: {T['tenant']}", "",
             f"Made {time.strftime('%Y-%m-%d %H:%M UTC', time.gmtime())}. Period: {since or 'all records'}. "
             f"Mode: {T.get('deployment', 'unknown')}. Router mode: shadow unless stated in the decisions.", "",
             "## Setup", f"- Ready: {setup.get('ready')}. Gates: {json.dumps(setup.get('gates'))}",
             f"- Qualified engine: {(setup.get('steps', {}).get('qualification') or {}).get('qualified')}", "",
             "## What left this machine during onboarding",
             (f"- Concierge: {conc.get('provider')} / {conc.get('model')}; left the machine: {conc.get('left_machine')}; "
              f"payload {conc.get('payload_bytes')} bytes, SHA-256 {conc.get('payload_sha256')}") if conc else "- No concierge record.", "",
             "## Private classes (never routed to a non-local engine)", "- " + ", ".join(T["local_only"]), "",
             "## Profiles", "PROFILE-SUMMARY.json counts executed jobs and hosted calls by profile; unknown provider outcomes retain their reserved cost.",
             "", "## Routing decisions by privacy class"] + [f"- {k}: {v}" for k, v in sorted(by_class.items())] + [
             "", "## Non-local engines offered for local-only jobs"] + (
             [f"- {x['at']} decision {x['decision']} (task {x['task_class']}, label {x['class']}): excluded {', '.join(x['excluded'])}"
              for x in offered] or ["- None offered."]) + [
             "", "## Engines that appeared in decisions (content digest)"] + [f"- {k}: {v}" for k, v in sorted(engines.items())] + [
             "", "## Who judged results"] + ([f"- {k}: {v}" for k, v in sorted(judges.items())] or ["- No judged results yet."]) + [
             "", "## Executed jobs", f"- Attempts recorded: {len(attempts)}. Receipt entries recorded: {len(receipts)}.",
             "", "## Integrity", "Decisions, outcomes and attempts are hashed at pack time, not chained on append. CHAIN.json verifies the pack-time decision/outcome chain and file hashes.",
             "receipts.jsonl is chained on append. A since-window retains a contiguous suffix anchored to the last excluded receipt hash; any later backdated rows remain included.",
             "The anchor and hashes provide internal consistency, not an external signature. Preserve a trusted chain head separately to detect a rewritten whole pack.",
             "The egress policy is included; not-applied means the host firewall was not changed.",
             "Check it with: python tempo_audit.py verify <this folder>", "",
             "This is a record of what the router decided and who judged it. It is not legal advice or a certification."]
    (out / "AUDIT-SUMMARY.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    files = {p.name: _sha(p.read_bytes()) for p in sorted(out.iterdir()) if p.is_file()}
    c = chain(dec + outc)
    receipt_chain = verify_chain(out / "receipts.jsonl", initial_hash=receipt_anchor)
    (out / "CHAIN.json").write_text(json.dumps({"head": c[-1] if c else None, "length": len(c), "files": files,
                                                "receipt_chain": receipt_chain, "receipt_anchor":receipt_anchor,
                                                "since":since}, indent=1), encoding="utf-8")
    return {"pack": str(out), "decisions": len(dec), "outcomes": len(outc), "attempts": len(attempts),
            "receipts": len(receipts), "receipt_chain_ok": receipt_chain["ok"],
            "non_local_offered_and_excluded": len(offered), "chain_head": c[-1] if c else None}



def verify(folder):
    f = Path(folder)
    required = {"AUDIT-SUMMARY.md", "PROFILE-SUMMARY.json", "decisions.jsonl", "outcomes.jsonl",
                "attempts.jsonl", "receipts.jsonl", "egress-policy.json"}
    allowed = required | {"setup-receipt.json", "concierge-receipt.json", "profiles.json", "fiduciary-dpa-map.json"}

    def regular(name):
        p = f / name
        info = p.lstat()
        if not stat.S_ISREG(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError("audit pack member must be a regular file")
        return p

    regular("CHAIN.json")
    c = json.loads((f / "CHAIN.json").read_text(encoding="utf-8"))
    files = c.get("files") if isinstance(c, dict) else None
    # Validate raw names before ANY path lookup (including UNC and drive paths).
    if not isinstance(files, dict) or not required <= set(files) or any(n not in allowed for n in files):
        raise ValueError("audit pack contains unknown or missing members")
    bad = [n for n, h in files.items() if _sha(regular(n).read_bytes()) != h]
    rows = _jsonl(f / "decisions.jsonl") + _jsonl(f / "outcomes.jsonl")
    ch = chain(rows)
    ok_chain = (ch[-1] if ch else None) == c["head"] and len(ch) == c["length"]
    from tempo_receipts import verify_chain, ZERO_HASH
    receipt_chain = verify_chain(f / "receipts.jsonl", initial_hash=c.get('receipt_anchor', ZERO_HASH))
    return {"files_ok": not bad, "changed_files": bad, "chain_ok": ok_chain,
            "receipt_chain_ok": receipt_chain["ok"], "receipt_chain_errors": receipt_chain["errors"]}


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("pack"); p.add_argument("--since"); p.add_argument("--out")
    v = sub.add_parser("verify"); v.add_argument("folder")
    a = ap.parse_args()
    result = pack(a.since, a.out) if a.cmd == "pack" else verify(a.folder)
    print(json.dumps(result, indent=1))
    if a.cmd == "verify" and not all(result[k] for k in ("files_ok", "chain_ok", "receipt_chain_ok")):
        sys.exit(1)
