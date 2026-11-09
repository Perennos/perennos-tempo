"""Why is this host short of memory? A read-only report: swap, tmpfs (RAM disk) use per mount, the largest tmpfs
folders with their age, and the biggest processes by memory. Linux /proc only; anything it cannot read is left out.

  python tempo_pressure.py            # JSON on stdout; also fine piped to `python -` on another computer over ssh
Nothing is changed, moved or deleted.
"""
import json
import os
import sys
import time
from pathlib import Path

GIB = 1 << 30
TMPFS_TYPES = {"tmpfs", "ramfs"}
SKIP_MOUNTS = ("/sys", "/proc", "/run/lock", "/run/snapd", "/run/credentials")


def meminfo(proc="/proc"):
    """Bytes for every `Key: N kB` line of /proc/meminfo."""
    out = {}
    try:
        for line in Path(proc, "meminfo").read_text().splitlines():
            key, _, rest = line.partition(":")
            parts = rest.split()
            if parts and parts[0].isdigit():
                out[key] = int(parts[0]) * (1024 if len(parts) > 1 else 1)
    except OSError:
        pass
    return out


def tmpfs_mounts(mounts="/proc/mounts"):
    found = []
    try:
        for line in Path(mounts).read_text().splitlines():
            parts = line.split()
            if len(parts) >= 3 and parts[2] in TMPFS_TYPES and not parts[1].startswith(SKIP_MOUNTS):
                found.append(parts[1])
    except OSError:
        pass
    return found


def _alloc(st):
    """Bytes really allocated (a sparse or deduplicated file counts for what it holds)."""
    return st.st_blocks * 512 if hasattr(st, "st_blocks") else st.st_size


def _walk(path, deadline):
    """(allocated bytes, newest mtime, complete) of a file or folder; symlinks are not followed."""
    try:
        st = os.lstat(path)
    except OSError:
        return 0, 0.0, True
    size, newest = _alloc(st), st.st_mtime
    if not os.path.isdir(path) or os.path.islink(path):
        return size, newest, True
    stack = [path]
    while stack:
        if time.monotonic() > deadline:
            return size, newest, False
        try:
            with os.scandir(stack.pop()) as it:
                for entry in it:
                    try:
                        st = entry.stat(follow_symlinks=False)
                    except OSError:
                        continue
                    size += _alloc(st)
                    newest = max(newest, st.st_mtime)
                    if entry.is_dir(follow_symlinks=False):
                        stack.append(entry.path)
        except OSError:
            continue
    return size, newest, True


def processes(proc="/proc"):
    rows = []
    try:
        names = [n for n in os.listdir(proc) if n.isdigit()]
    except OSError:
        return rows
    for pid in names:
        try:
            fields = {}
            for line in Path(proc, pid, "status").read_text().splitlines():
                key, _, rest = line.partition(":")
                if key in ("Name", "VmRSS", "VmSwap", "Uid"):
                    fields[key] = rest.split()
            rows.append({"pid": int(pid), "name": fields["Name"][0], "uid": int(fields["Uid"][0]),
                         "rss_bytes": int(fields.get("VmRSS", ["0"])[0]) * 1024,
                         "swap_bytes": int(fields.get("VmSwap", ["0"])[0]) * 1024})
        except (OSError, KeyError, IndexError, ValueError):
            continue
    return rows


def collect(proc="/proc", mounts="/proc/mounts", now=None, budget_s=8.0, limit=5):
    now = time.time() if now is None else now
    deadline = time.monotonic() + budget_s
    mem = meminfo(proc)
    report = {"collected_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now)),
              "mem": {"total": mem.get("MemTotal", 0), "available": mem.get("MemAvailable", 0),
                      "swap_total": mem.get("SwapTotal", 0),
                      "swap_used": mem.get("SwapTotal", 0) - mem.get("SwapFree", 0)},
              "tmpfs": [], "tmpfs_top": [], "top_rss": [], "top_swap": [], "truncated": False}
    for mount in tmpfs_mounts(mounts):
        try:
            vfs = os.statvfs(mount)
        except OSError:
            continue
        size, free = vfs.f_blocks * vfs.f_frsize, vfs.f_bavail * vfs.f_frsize
        row = {"mount": mount, "size": size, "used": size - free, "used_pct": round(100 * (size - free) / size) if size else 0}
        report["tmpfs"].append(row)
        if size - free < (64 << 20):
            continue
        try:
            children = [os.path.join(mount, n) for n in os.listdir(mount)]
        except OSError:
            continue
        scanned = 0
        for child in children:
            used, newest, complete = _walk(child, deadline)
            scanned += used
            report["truncated"] |= not complete
            report["tmpfs_top"].append({"path": child, "bytes": used,
                                        "idle_hours": round(max(0.0, now - newest) / 3600, 1)})
        row["unseen"] = max(0, row["used"] - scanned)   # not visible as files: unreadable folders, or deleted files still held open
    report["tmpfs_top"] = sorted(report["tmpfs_top"], key=lambda r: -r["bytes"])[:limit]
    procs = processes(proc)
    report["top_rss"] = sorted(procs, key=lambda r: -r["rss_bytes"])[:limit]
    report["top_swap"] = [r for r in sorted(procs, key=lambda r: -r["swap_bytes"])[:3] if r["swap_bytes"]]
    return report


def summarise(report):
    """One plain line saying where the memory went; empty when nothing stands out."""
    mem, parts = report.get("mem") or {}, []
    held = sum(m["used"] for m in report.get("tmpfs", []))
    if held >= GIB:
        where = ", ".join("%s %.1f GiB" % (m["mount"], m["used"] / GIB) for m in report["tmpfs"] if m["used"] >= GIB)
        parts.append("tmpfs (RAM disk) holds %.1f GiB (%s)" % (held / GIB, where))
        if report.get("tmpfs_top"):
            top = report["tmpfs_top"][0]
            parts.append("largest is %s at %.1f GiB, untouched for %.0f h" % (top["path"], top["bytes"] / GIB, top["idle_hours"]))
        unseen = sum(m.get("unseen", 0) for m in report["tmpfs"])
        if unseen >= GIB:
            parts.append("%.1f GiB of it is not visible as files (unreadable folders, or deleted files still held open)" % (unseen / GIB))
    if mem.get("swap_total") and mem["swap_used"] >= 0.5 * mem["swap_total"]:
        parts.append("swap is %d%% used (%.1f of %.1f GiB)" % (100 * mem["swap_used"] / mem["swap_total"], mem["swap_used"] / GIB, mem["swap_total"] / GIB))
    if mem.get("total"):
        parts.append("%.1f of %.1f GiB available" % (mem["available"] / GIB, mem["total"] / GIB))
    if report.get("top_rss"):
        parts.append("biggest processes: " + ", ".join("%s (pid %d) %.1f GiB" % (p["name"], p["pid"], p["rss_bytes"] / GIB) for p in report["top_rss"][:3]))
    return "; ".join(parts)


if __name__ == "__main__":
    json.dump(collect(), sys.stdout)
    sys.stdout.write("\n")
