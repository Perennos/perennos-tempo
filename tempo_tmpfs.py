"""Keep RAM-disk (tmpfs) leftovers from piling up. A job registers the tmpfs folders it makes; when the job ends the
folders are MOVED to a trash folder on a real disk (never deleted); `sweep` flags tmpfs folders nobody registered that are
old and have no process holding them open. Nothing is ever deleted, and nothing is moved unless it was registered.

  python tempo_tmpfs.py register JOB PATH      # PATH must be on tmpfs
  python tempo_tmpfs.py release JOB            # job ended: registered folders go to the trash folder
  python tempo_tmpfs.py sweep [--hours 24]     # read-only: list old tmpfs folders with no open handles
Registry and trash live under TEMPO_TMPFS_HOME (default ~/.tempo/tmpfs), which must not be tmpfs itself.
"""
import argparse
import json
import os
import shutil
import sys
import time
from pathlib import Path

if "__file__" in globals():   # absent when the source is piped to `python -` on another computer
    sys.path.insert(0, str(Path(__file__).resolve().parent))
import tempo_pressure as P  # noqa: E402

HOME = Path(os.environ.get("TEMPO_TMPFS_HOME") or Path.home() / ".tempo" / "tmpfs")
KEEP_FREE = 10 * P.GIB   # a move that would leave less than this free on the trash disk is refused


def _load(registry):
    try:
        return json.loads(Path(registry).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []


def _save(registry, rows):
    registry = Path(registry)
    registry.parent.mkdir(parents=True, exist_ok=True)
    tmp = registry.with_suffix(".tmp")
    tmp.write_text(json.dumps(rows, indent=1), encoding="utf-8")
    os.replace(tmp, registry)


def _under(path, root):
    path, root = Path(path), Path(root)
    return path == root or root in path.parents


def on_tmpfs(path, mounts="/proc/mounts"):
    path = os.path.realpath(path)
    return any(_under(path, m) for m in P.tmpfs_mounts(mounts))


def register(job, path, registry=None, mounts="/proc/mounts"):
    registry = registry or HOME / "registry.json"
    path = os.path.realpath(path)
    if not on_tmpfs(path, mounts):
        return {"ok": False, "reason": "%s is not on tmpfs, so it is not tracked" % path}
    rows = _load(registry)
    if not any(r["job"] == job and r["path"] == path and r["state"] == "open" for r in rows):
        rows.append({"job": job, "path": path, "registered_at": time.time(), "state": "open"})
        _save(registry, rows)
    return {"ok": True, "job": job, "path": path}


def release(job, registry=None, trash=None, keep_free=KEEP_FREE, now=None):
    """Move this job's registered folders to the trash disk. A folder that would not fit with `keep_free` to spare stays
    where it is and is reported, so a full disk is never made fuller."""
    registry, trash = registry or HOME / "registry.json", Path(trash or HOME / "trash")
    now, rows, moved, kept = time.time() if now is None else now, _load(registry), [], []
    for r in rows:
        if r["job"] != job or r["state"] != "open":
            continue
        if not os.path.exists(r["path"]):
            r["state"] = "gone"
            continue
        size, _, _ = P._walk(r["path"], time.monotonic() + 60)
        trash.mkdir(parents=True, exist_ok=True)
        if shutil.disk_usage(trash).free - size < keep_free:
            r["state"] = "kept"
            kept.append({"path": r["path"], "bytes": size, "reason": "moving it would leave under %.0f GiB free on the trash disk" % (keep_free / P.GIB)})
            continue
        dest = trash / time.strftime("%Y%m%d", time.gmtime(now)) / ("%s-%s" % (job, Path(r["path"]).name))
        dest.parent.mkdir(parents=True, exist_ok=True)
        if dest.exists():
            dest = dest.with_name(dest.name + "-%d" % now)
        shutil.move(r["path"], dest)
        r.update(state="trashed", trashed_to=str(dest), released_at=now)
        moved.append({"path": r["path"], "bytes": size, "trashed_to": str(dest)})
    _save(registry, rows)
    return {"job": job, "moved": moved, "kept": kept}


def open_handles(path, proc="/proc"):
    """(processes holding `path` open as a file, cwd or root, processes we could not read)."""
    path, held, unreadable = os.path.realpath(path), 0, 0
    try:
        pids = [n for n in os.listdir(proc) if n.isdigit()]
    except OSError:
        return 0, 0
    for pid in pids:
        try:
            links = [os.path.join(proc, pid, n) for n in ("cwd", "root")]
            fd_dir = os.path.join(proc, pid, "fd")
            links += [os.path.join(fd_dir, n) for n in os.listdir(fd_dir)]
        except OSError:
            unreadable += 1
            continue
        for link in links:
            try:
                target = os.readlink(link)
            except OSError:
                continue
            if _under(target, path):
                held += 1
                break
    return held, unreadable


def sweep(registry=None, hours=24.0, proc="/proc", mounts="/proc/mounts", now=None):
    """Old tmpfs folders with no open handle, biggest first. Read-only."""
    registry, now = registry or HOME / "registry.json", time.time() if now is None else now
    known = {r["path"]: r for r in _load(registry) if r["state"] == "open"}
    report = P.collect(proc=proc, mounts=mounts, now=now, limit=50)
    flagged = []
    for top in report["tmpfs_top"]:
        if top["idle_hours"] < hours:
            continue
        held, unreadable = open_handles(top["path"], proc)
        if held:
            continue
        flagged.append({**top, "registered": top["path"] in known, "job": (known.get(top["path"]) or {}).get("job"),
                        "unreadable_processes": unreadable})
    return {"hours": hours, "flagged": flagged, "total_bytes": sum(f["bytes"] for f in flagged)}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("register", "release"):
        p = sub.add_parser(name)
        p.add_argument("job")
        if name == "register":
            p.add_argument("path")
    sub.add_parser("sweep").add_argument("--hours", type=float, default=24.0)
    a = ap.parse_args(argv)
    out = (register(a.job, a.path) if a.cmd == "register" else release(a.job) if a.cmd == "release" else sweep(hours=a.hours))
    print(json.dumps(out, indent=1))
    return 0 if out.get("ok", True) else 1


if __name__ == "__main__":
    sys.exit(main())
