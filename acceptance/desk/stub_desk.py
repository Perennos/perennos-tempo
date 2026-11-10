#!/usr/bin/env python3
"""A fake Desk, used only to prove the harness can tell good from bad. It is not a Desk.

It reads the gold files, so it is an oracle. STUB_MODE picks how it misbehaves:
  good   answers, declines, marks drafts and survives restarts exactly as required
  leaky  answers wall probes (leaks)
  liar   invents a citation, answers unanswerable questions, skips the DRAFT mark, duplicates rows on restart
Env: STUB_EVAL=<eval dir>  STUB_STATE=<state file>  STUB_BYTES=<fake egress bytes for the egress end command>
Usage as adapter: python stub_desk.py            (request JSON on stdin)
                  python stub_desk.py egress-end (prints the capture JSON)
"""
import json, os, sys
from pathlib import Path

ev = Path(os.environ.get("STUB_EVAL", "."))
mode = os.environ.get("STUB_MODE", "good")
state = Path(os.environ.get("STUB_STATE", "stub-state.json"))


def rows(n):
    return [json.loads(l) for l in (ev / n).read_text(encoding="utf-8").splitlines() if l.strip()]


def receipt(req, scope="firm"):
    return dict(user=req.get("user"), matter_scope=scope, model="stub", model_version="0", source_hashes=[], egress_bytes=0, reviewer=None, blocked_urls=["http://collect.invalid/x"])


def ask(req):
    q = req["question"]
    rc = receipt(req)
    for c in rows("citation.jsonl"):
        if c["question"] == q:
            cit = [dict(paragraph_id="HR-M99-001#p9" if mode == "liar" else c["gold_ids"][0])]
            return dict(answer=c["answers"][0], citations=cit, receipt=rc)
    for p in rows("wall_probes.jsonl"):
        if p["question"] == q and mode == "leaky" and p["kind"] in ("direct", "social"):
            return dict(answer=f"The answer is {p['markers'][0]}.", citations=[], receipt=rc)
    for u in rows("unanswerable.jsonl"):
        if u["question"] == q and mode == "liar":
            return dict(answer="It was 42.", citations=[], receipt=rc)
    return dict(declined=True, answer="I can't support that from your documents.", citations=[], receipt=rc)


def main():
    if sys.argv[1:] == ["egress-end"]:
        print(json.dumps(dict(packets=int(os.environ.get("STUB_BYTES", 0)) > 0 and 3 or 0, bytes=int(os.environ.get("STUB_BYTES", 0)))))
        return
    req = json.loads(sys.stdin.read() or "{}")
    op = req.get("op")
    if op == "version":
        out = dict(name="stub", mode=mode)
    elif op == "ask":
        out = ask(req)
    elif op == "draft":
        out = dict(draft_text=("" if mode == "liar" else "DRAFT - not reviewed\n\n") + "Dear sirs, ...", author_of_record=None, receipt=receipt(req))
    elif op == "reset":
        state.write_text(json.dumps(dict(n=0)))
        out = dict(ok=True)
    elif op == "ingest":
        cfg = json.loads((ev / "restart.json").read_text(encoding="utf-8"))
        state.write_text(json.dumps(dict(n=cfg["expected_unique_documents"] + (3 if mode == "liar" and "corpus" in req.get("path", "") else 0))))
        out = dict(status="complete", receipt=receipt(req))
    elif op == "state":
        cfg = json.loads((ev / "restart.json").read_text(encoding="utf-8"))
        n = json.loads(state.read_text())["n"] if state.exists() else 0
        out = dict(documents=cfg["expected_unique_documents"], duplicate_rows=n - cfg["expected_unique_documents"], corrupt=0, integrity_ok=True, hashes_sha256=cfg["expected_hashes_sha256"])
    else:
        out = dict(error="unknown op")
    print(json.dumps(out))


main()
