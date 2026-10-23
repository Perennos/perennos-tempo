"""Copy ONLY the MANIFEST.json-listed public core (+ MANIFEST.json) into a fresh, closed payload folder.

build_desktop_installer.py refuses extra files, so the checkout (.git, .github, acceptance/, docs not in the
manifest, ...) must never reach the payload. Nothing in the repo is modified.
"""
import argparse
import hashlib
import json
import os
import stat
import shutil
import sys
from pathlib import Path

HEX64 = set("0123456789abcdef")
MUST_LIST = {"MANIFEST.json"}  # never listed by the product manifest itself
REQUIRED = {"tempo.py", "tempo_client.py", "tempo_desktop.py", "tempo_bootstrap.py", "tempo_setup.py",
            "tempo_gateway.py", "tempo_mcp.py", "fetch_models.py", "model_catalog.json",
            "desktop/Bootstrap.cs", "build_desktop_installer.py", "tempo_local_http.py"}


def fail(message):
    raise SystemExit("prepare_payload: " + message)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def is_link(path):
    st = os.lstat(path)
    return stat.S_ISLNK(st.st_mode) or bool(getattr(st, "st_file_attributes", 0) & 0x400)  # reparse point


def clean_name(name):
    parts = name.split("/") if isinstance(name, str) else []
    if (not parts or "\\" in name or ":" in name or name.startswith("/") or name.endswith((".", " "))
            or any(p in ("", ".", "..") for p in parts)):
        fail("unsafe manifest path: %r" % (name,))
    return name


def confined_source(repo, name):
    """Return the regular file for `name`, rejecting any symlink/reparse point on the way down from repo."""
    current = repo
    for part in name.split("/"):
        current = current / part
        if not os.path.lexists(current):
            fail("manifest file is missing from the checkout: " + name)
        if is_link(current):
            fail("symlink/reparse point refused: " + name)
    resolved = current.resolve()
    if repo not in resolved.parents or not stat.S_ISREG(os.lstat(current).st_mode):
        fail("not a regular file inside the checkout: " + name)
    return current


def prepare(repo, dest, expect_files):
    repo, dest = Path(repo).resolve(), Path(dest)
    if os.path.lexists(dest):
        fail("destination already exists; a fresh folder is required: " + str(dest))
    manifest_path = confined_source(repo, "MANIFEST.json")
    raw = manifest_path.read_bytes()
    manifest = json.loads(raw.decode("utf-8"))
    files = manifest.get("files")
    if manifest.get("product") != "Perennos Tempo" or not isinstance(files, dict):
        fail("MANIFEST.json is not the Perennos Tempo public core manifest")
    if len(files) != expect_files:
        fail("manifest lists %d files, expected %d" % (len(files), expect_files))
    folded = set()
    for name, digest in files.items():
        clean_name(name)
        if name in MUST_LIST or name.lower() in folded:
            fail("manifest lists MANIFEST.json or a case-insensitive duplicate: " + name)
        folded.add(name.lower())
        if not (isinstance(digest, str) and len(digest) == 64 and set(digest) <= HEX64):
            fail("manifest digest is not lowercase sha256 hex: " + name)
    if not REQUIRED <= set(files):
        fail("manifest is missing required files: " + ", ".join(sorted(REQUIRED - set(files))))

    sources = {}
    for name, digest in files.items():  # verify every source BEFORE creating anything
        sources[name] = confined_source(repo, name)
        if sha256(sources[name]) != digest:
            fail("checkout file does not match its manifest hash: " + name)

    dest.mkdir(parents=True)
    for name, source in sources.items():
        target = dest / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    (dest / "MANIFEST.json").write_bytes(raw)

    # Re-verify the copy: closed set, no links, hashes still match.
    actual = set()
    for path in dest.rglob("*"):
        if is_link(path):
            fail("link in payload: " + str(path))
        if path.is_file():
            actual.add(path.relative_to(dest).as_posix())
    if actual != set(files) | {"MANIFEST.json"}:
        fail("payload is not exactly the manifest set")
    for name, digest in files.items():
        if sha256(dest / name) != digest:
            fail("payload copy hash mismatch: " + name)

    outside = []  # honest evidence of what was deliberately NOT copied
    for dirpath, dirnames, filenames in os.walk(repo):
        dirnames[:] = [d for d in dirnames if d != ".git"]
        for filename in filenames:
            rel = (Path(dirpath) / filename).relative_to(repo).as_posix()
            if rel not in files and rel != "MANIFEST.json":
                outside.append(rel)
    return {"passed": True, "payload": str(dest), "files_in_manifest": len(files),
            "files_in_payload": len(actual), "manifest_sha256": hashlib.sha256(raw).hexdigest(),
            "checkout_files_not_copied": len(outside), "checkout_files_not_copied_sample": sorted(outside)[:40]}


def self_test():
    for good in ("tempo.py", "desktop/Bootstrap.cs"):
        assert clean_name(good) == good
    for bad in ("../x", "/x", "a\\b", "C:x", "a//b", "a/./b", ""):
        try:
            clean_name(bad)
        except SystemExit:
            continue
        raise AssertionError("accepted " + repr(bad))
    print("prepare_payload self-test ok")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--dest", default="C:/TempoCI/payload")
    parser.add_argument("--expect-files", type=int, default=61)
    parser.add_argument("--result", default=None)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        sys.exit(0)
    outcome = prepare(args.repo, args.dest, args.expect_files)
    text = json.dumps(outcome, indent=2, sort_keys=True)
    if args.result:
        Path(args.result).parent.mkdir(parents=True, exist_ok=True)
        Path(args.result).write_text(text + "\n", encoding="utf-8")
    print(text)
