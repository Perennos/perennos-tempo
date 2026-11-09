"""Did Tempo actually help? Every use leaves a small row (tool, who, seconds, ok); anyone can add the outcome afterwards
(helped, partly or no, with a short note); `summary` shows the answer per tool. Cloud work that Tempo does not run itself
(a cloud coding session, a hosted model) is tracked the same way with `external_start` / `external_done`: it needs no
memory, GPU or window from this computer, only an honest start, finish and outcome.

  python tempo_outcomes.py outcome REF helped|partly|no [--seconds N] [--note TEXT]
  python tempo_outcomes.py summary
Rows are metadata only (no prompts, no replies) in one append-only file under TEMPO_OUTCOMES_HOME (default ~/.tempo/outcomes).
"""
import argparse
import json
import os
import statistics
import sys
import time
import uuid
from pathlib import Path

HOME = Path(os.environ.get("TEMPO_OUTCOMES_HOME") or Path.home() / ".tempo" / "outcomes")
OUTCOMES = ("helped", "partly", "no")
NOTE_MAX = 300


def _append(row, path=None):
    path = Path(path or HOME / "outcomes.jsonl")
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("ab") as stream:   # one write per row keeps concurrent appends from interleaving
        stream.write((json.dumps(row, separators=(",", ":")) + "\n").encode("utf-8"))
        stream.flush()
        os.fsync(stream.fileno())
    return row


def rows(path=None):
    try:
        lines = Path(path or HOME / "outcomes.jsonl").read_text(encoding="utf-8").splitlines()
    except OSError:
        return []
    out = []
    for line in lines:
        try:
            out.append(json.loads(line))
        except ValueError:
            continue
    return out


def _now(now):
    return time.time() if now is None else now


def _outcome(value):
    if value is not None and value not in OUTCOMES:
        raise ValueError("outcome must be one of %s" % ", ".join(OUTCOMES))
    return value


def record_use(tool, who, seconds, ok, ref=None, path=None, now=None):
    """One call to a Tempo tool. Returns the row; its id is what an outcome refers to."""
    return _append({"kind": "use", "id": "u" + uuid.uuid4().hex[:10], "at": _now(now), "tool": str(tool)[:80],
                    "who": str(who)[:60], "seconds": round(float(seconds), 2), "ok": bool(ok), "ref": ref}, path)


def record_outcome(ref, outcome, seconds=None, note="", who="", path=None, now=None):
    """Say whether the thing `ref` (a use id, a receipt id, an external id) helped. `seconds` is the time it cost you, if known."""
    return _append({"kind": "outcome", "ref": str(ref)[:80], "at": _now(now), "outcome": _outcome(outcome),
                    "seconds": None if seconds is None else round(float(seconds), 2), "note": str(note)[:NOTE_MAX],
                    "who": str(who)[:60]}, path)


def external_start(who, unit, engine, path=None, now=None):
    """A cloud or hosted job began. Needs nothing from this computer: it is recorded, not admitted against local resources."""
    return _append({"kind": "external_start", "id": "x" + uuid.uuid4().hex[:10], "at": _now(now), "who": str(who)[:60],
                    "unit": str(unit)[:200], "engine": str(engine)[:80]}, path)


def external_done(ext_id, ok=True, outcome=None, note="", path=None, receipts_path=None, now=None):
    """The external job finished. Optionally also appends a metadata-only row to a Tempo receipt chain (route 'external')."""
    now = _now(now)
    start = next((r for r in rows(path) if r.get("kind") == "external_start" and r.get("id") == ext_id), None)
    if start is None:
        raise KeyError("no external job %s" % ext_id)
    done = _append({"kind": "external_done", "ref": ext_id, "at": now, "seconds": round(now - start["at"], 2), "ok": bool(ok),
                    "outcome": _outcome(outcome), "note": str(note)[:NOTE_MAX], "engine": start["engine"], "who": start["who"]}, path)
    if receipts_path:
        from tempo_receipts import append_receipt
        append_receipt(receipts_path, {"job_id": ext_id, "status": "completed" if ok else "failed", "route": "external",
                                       "engine_id": start["engine"], "elapsed_ms": round(done["seconds"] * 1000, 2)})
    return done


def summary(all_rows=None):
    """Per tool (or external engine): calls, how many were rated, helped/partly/no, share that helped, time spent."""
    all_rows = rows() if all_rows is None else all_rows
    rated = {}
    for r in all_rows:
        if r.get("kind") == "outcome" or (r.get("kind") == "external_done" and r.get("outcome")):
            rated[r["ref"]] = r["outcome"]            # the latest word on a ref wins
    table, open_external = {}, 0
    done = {r["ref"] for r in all_rows if r.get("kind") == "external_done"}
    for r in all_rows:
        if r.get("kind") == "use":
            name, ref, secs = r["tool"], r["id"], r["seconds"]
        elif r.get("kind") == "external_done":
            name, ref, secs = "external:" + r["engine"], r["ref"], r["seconds"]
        else:
            open_external += r.get("kind") == "external_start" and r["id"] not in done
            continue
        row = table.setdefault(name, {"calls": 0, "helped": 0, "partly": 0, "no": 0, "unrated": 0, "seconds": []})
        row["calls"] += 1
        row["seconds"].append(secs)
        if ref in rated:
            row[rated[ref]] += 1
        else:
            row["unrated"] += 1
    counted = {r.get("id") for r in all_rows if r.get("kind") == "use"} | done
    for r in all_rows:      # a rating for a receipt id or any ref we hold no use row for still counts, under its own line
        if r.get("kind") == "outcome" and r["ref"] not in counted and rated.get(r["ref"]) == r["outcome"]:
            counted.add(r["ref"])
            row = table.setdefault("(receipts and other refs)", {"calls": 0, "helped": 0, "partly": 0, "no": 0, "unrated": 0, "seconds": []})
            row["calls"] += 1
            row[r["outcome"]] += 1
            row["seconds"].append(r.get("seconds") or 0)
    for row in table.values():
        n = row["helped"] + row["partly"] + row["no"]
        row["helped_pct"] = round(100 * row["helped"] / n) if n else None
        row["total_seconds"] = round(sum(row["seconds"]), 1)
        row["median_seconds"] = round(statistics.median(row.pop("seconds")), 1)
    return {"tools": table, "external_still_open": open_external}


def text(report):
    lines = ["%-28s %5s %6s %6s %3s %7s %8s %8s" % ("tool", "calls", "helped", "partly", "no", "unrated", "helped%", "seconds")]
    for name, r in sorted(report["tools"].items(), key=lambda kv: -kv[1]["calls"]):
        pct = "-" if r["helped_pct"] is None else "%d%%" % r["helped_pct"]
        lines.append("%-28s %5d %6d %6d %3d %7d %8s %8.0f" % (name[:28], r["calls"], r["helped"], r["partly"], r["no"], r["unrated"], pct, r["total_seconds"]))
    if report["external_still_open"]:
        lines.append("%d external job(s) started and not finished" % report["external_still_open"])
    return "\n".join(lines)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    o = sub.add_parser("outcome")
    o.add_argument("ref"); o.add_argument("outcome", choices=OUTCOMES)
    o.add_argument("--seconds", type=float); o.add_argument("--note", default=""); o.add_argument("--who", default="")
    sub.add_parser("summary")
    a = ap.parse_args(argv)
    if a.cmd == "outcome":
        print(json.dumps(record_outcome(a.ref, a.outcome, a.seconds, a.note, a.who)))
    else:
        print(text(summary()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
