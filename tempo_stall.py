"""Flag a job or unit whose heartbeat or report file has not changed in N hours, and say so ONCE per stall.

A watched item is an id plus a file or folder that should keep changing while the work is alive (a report, a log, a
heartbeat). `scan` compares the newest change time with now; the first scan that finds it stale alerts, later scans stay
quiet, and a fresh change (or the work finishing) clears it so a later stall can alert again. The alert file is one per
item and is rewritten in place, never one new file per scan.

  python tempo_stall.py watch ID PATH [--hours 6]     # start watching
  python tempo_stall.py unwatch ID                    # work finished
  python tempo_stall.py scan                          # print and write alerts for new stalls
Files live under TEMPO_STALL_HOME (default ~/.tempo/stall).
"""
import argparse
import json
import os
import sys
import time
from pathlib import Path

HOME = Path(os.environ.get("TEMPO_STALL_HOME") or Path.home() / ".tempo" / "stall")
DEFAULT_HOURS = 6.0


def newest_change(path, depth=2, cap=500):
    """Newest modification time of a file, or of a folder and what is in it (to `depth`, at most `cap` entries); None if missing."""
    try:
        newest = os.stat(path).st_mtime
    except OSError:
        return None
    seen, level = 0, [path] if os.path.isdir(path) else []
    for _ in range(depth):
        nxt = []
        for folder in level:
            try:
                with os.scandir(folder) as it:
                    for entry in it:
                        seen += 1
                        if seen > cap:
                            return newest
                        try:
                            newest = max(newest, entry.stat().st_mtime)
                        except OSError:
                            continue
                        if entry.is_dir(follow_symlinks=False):
                            nxt.append(entry.path)
            except OSError:
                continue
        level = nxt
    return newest


def check(items, state, now, default_hours=DEFAULT_HOURS):
    """items: [{id, label, last_beat (epoch or None), done (bool), hours (optional)}]. Returns (new_alerts, state).
    An item with no readable beat is treated as 'unknown', never as stalled."""
    alerts, live = [], {}
    for item in items:
        if item.get("done") or item.get("last_beat") is None:
            continue
        age_h = (now - item["last_beat"]) / 3600
        if age_h < item.get("hours", default_hours):
            continue
        prev = state.get(item["id"])
        live[item["id"]] = prev or {"first_flagged": now}
        if not prev:
            alerts.append({"id": item["id"], "label": item.get("label") or item["id"], "age_hours": round(age_h, 1),
                           "last_beat": item["last_beat"], "hours": item.get("hours", default_hours)})
    return alerts, live


def alert_markdown(alert, source=""):
    when = time.strftime("%Y-%m-%d %H:%MZ", time.gmtime(alert["last_beat"]))
    return "\n".join([
        "# STALLED: %s" % alert["label"], "",
        "- **Status:** In progress. Nothing has been stopped, changed or deleted.",
        "- **What is stale:** %s%s" % (alert["id"], " (%s)" % source if source else ""),
        "- **Last change seen:** %s, %.1f h ago (the limit for this item is %.1f h)" % (when, alert["age_hours"], alert["hours"]),
        "- **What would clear it:** the work writes again, or `tempo_stall.py unwatch %s` once it has finished." % alert["id"],
        "",
        "This is written once per stall and is not repeated each scan.", ""])


def write_alert(alert, alert_dir, source=""):
    """One file per item, rewritten in place."""
    alert_dir = Path(alert_dir)
    alert_dir.mkdir(parents=True, exist_ok=True)
    safe = "".join(c if c.isalnum() or c in "-_." else "_" for c in alert["id"])[:100]
    path = alert_dir / ("STUCK-stall-%s.md" % safe)
    path.write_text(alert_markdown(alert, source), encoding="utf-8", newline="\n")
    return path


def _read(path, default):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def _write(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(data, indent=1), encoding="utf-8")
    os.replace(tmp, path)


def watch(item_id, path, hours=DEFAULT_HOURS, watchfile=None):
    watchfile = watchfile or HOME / "watch.json"
    rows = [r for r in _read(watchfile, []) if r["id"] != item_id]
    rows.append({"id": item_id, "path": str(path), "hours": float(hours)})
    _write(watchfile, rows)
    return rows[-1]


def unwatch(item_id, watchfile=None, statefile=None):
    watchfile, statefile = watchfile or HOME / "watch.json", statefile or HOME / "state.json"
    _write(watchfile, [r for r in _read(watchfile, []) if r["id"] != item_id])
    state = _read(statefile, {})
    state.pop(item_id, None)
    _write(statefile, state)


def scan(watchfile=None, statefile=None, alert_dir=None, now=None):
    """Read the watch list, alert on new stalls, remember who is already alerted. Returns the new alerts."""
    watchfile, statefile = watchfile or HOME / "watch.json", statefile or HOME / "state.json"
    now = time.time() if now is None else now
    rows = _read(watchfile, [])
    items = [{"id": r["id"], "label": r["id"], "hours": r.get("hours", DEFAULT_HOURS), "last_beat": newest_change(r["path"])} for r in rows]
    alerts, state = check(items, _read(statefile, {}), now)
    paths = {r["id"]: r["path"] for r in rows}
    for alert in alerts:
        alert["file"] = str(write_alert(alert, alert_dir or HOME / "alerts", paths[alert["id"]]))
    _write(statefile, state)
    return alerts


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    w = sub.add_parser("watch")
    w.add_argument("id"); w.add_argument("path"); w.add_argument("--hours", type=float, default=DEFAULT_HOURS)
    sub.add_parser("unwatch").add_argument("id")
    sub.add_parser("scan")
    a = ap.parse_args(argv)
    out = watch(a.id, a.path, a.hours) if a.cmd == "watch" else (unwatch(a.id) or {"ok": True}) if a.cmd == "unwatch" else scan()
    print(json.dumps(out, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
