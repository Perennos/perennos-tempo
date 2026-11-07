#!/usr/bin/env python3
"""THIRD-PARTY-NOTICES.txt for the programs in the installer: every third-party crate built into
them (Perennos's own crates aside), its license, and the license text the crate ships.

usage: python3 notices.py OUT_FILE TARGET CRATE_DIR...
"""
import hashlib
import json
import os
import subprocess
import sys

PREFIXES = ("license", "licence", "copying", "notice", "unlicense", "copyright")


def build_graph(crate_dir, target):
    """The packages a release build of the crate at `crate_dir` compiles in, for `target`."""
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target,
         "--manifest-path", os.path.join(crate_dir, "Cargo.toml")],
        capture_output=True, text=True, check=True).stdout
    meta = json.loads(out)
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    seen, todo = set(), [meta["resolve"]["root"]]
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            # normal dependencies only: dev and build dependencies are not in the program
            if any(k.get("kind") is None for k in dep["dep_kinds"]):
                todo.append(dep["pkg"])
    return [pkgs[p] for p in seen]


def license_texts(pkg):
    root = os.path.dirname(pkg["manifest_path"])
    names = sorted(f for f in os.listdir(root) if f.lower().startswith(PREFIXES) and os.path.isfile(os.path.join(root, f)))
    if pkg.get("license_file"):
        lf = os.path.join(root, pkg["license_file"])
        if os.path.isfile(lf) and os.path.relpath(lf, root) not in names:
            names.append(os.path.relpath(lf, root))
    texts = []
    for n in names:
        with open(os.path.join(root, n), encoding="utf-8", errors="replace") as f:
            texts.append((n, f.read().strip()))
    return texts


def main(out_file, target, crate_dirs):
    found = {}
    for d in crate_dirs:
        if not os.path.isfile(os.path.join(d, "Cargo.toml")):
            continue
        for p in build_graph(d, target):
            if p.get("source") is None:
                continue  # a path dependency: Perennos's own code
            found[(p["name"], p["version"])] = p
    rows = sorted(found.values(), key=lambda p: (p["name"].lower(), p["version"]))
    groups = {}
    missing = []
    for p in rows:
        texts = license_texts(p)
        if not texts:
            missing.append(p)
            continue
        body = "\n\n".join(f"--- {n}\n\n{t}" if len(texts) > 1 else t for n, t in texts)
        key = hashlib.sha256(body.encode()).hexdigest()
        groups.setdefault(key, (body, []))[1].append(f"{p['name']} {p['version']}")
    lines = [
        "Perennos Tempo: third-party notices",
        "",
        "Perennos Tempo's programs are built with the Rust crates below, each under its own license. Each crate's",
        "license follows, as the crate ships it. The installer is made with Inno Setup",
        "(Copyright (C) 1997-2025 Jordan Russell and Martijn Laan).",
        "",
        f"{len(rows)} crates:",
        "",
    ]
    lines += [f"  {p['name']} {p['version']}: {p.get('license') or 'see its license file'}" for p in rows]
    for body, names in groups.values():
        lines += ["", "=" * 78, ", ".join(names), "-" * 78, "", body]
    for p in missing:
        lines += ["", "=" * 78, f"{p['name']} {p['version']}", "-" * 78, "",
                  f"License: {p.get('license') or 'unknown'} (the crate ships no license file; see {p.get('repository') or 'crates.io'})"]
    os.makedirs(os.path.dirname(os.path.abspath(out_file)), exist_ok=True)
    with open(out_file, "w", encoding="utf-8", newline="\r\n") as f:
        f.write("\n".join(lines) + "\n")
    print(f"{len(rows)} crates, {len(groups)} license texts, {len(missing)} without a file -> {out_file}")


if __name__ == "__main__":
    if len(sys.argv) < 4:
        sys.exit(__doc__.strip())
    main(sys.argv[1], sys.argv[2], sys.argv[3:])
