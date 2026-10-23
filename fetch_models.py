"""Fetch (or verify) the pinned files Perennos Tempo needs, into the tenant's models_dir and data folder.

  python fetch_models.py --verify      # check what is on disk against the pins, download nothing
  python fetch_models.py               # download whatever is missing or wrong, then verify

Every file is pinned to a repository revision and a SHA-256; a mismatch is an error, never a silent replacement.
About 1.7 GB in total (the Sev weights). ModernBERT-large weights are not needed: only its config and tokenizer.
"""
import argparse, hashlib, json, sys, tempfile, time, urllib.request
from pathlib import Path

HOME = Path(__file__).resolve().parent
sys.path.insert(0, str(HOME))
from tempo_home import DATA, MODELS

PINS = [
    # (target, url, sha256)
    ("sev/model.safetensors", "https://huggingface.co/LakoreAI/sev/resolve/08c6ae0ba7ee58871418a4c439a1889289e7df6b/model.safetensors",
     "be8aae6b150491cfe3d8049d69fce35dc06dfe4aac1db1d71a321b19432a9f9f"),
    ("sev/config.json", "https://huggingface.co/LakoreAI/sev/resolve/08c6ae0ba7ee58871418a4c439a1889289e7df6b/config.json",
     "0426a5b354a9e0a0b7508cbc67cf2c63e7fdc2ac021aee85f0f90630a3237138"),
    ("sev/model_config.json", "https://huggingface.co/LakoreAI/sev/resolve/08c6ae0ba7ee58871418a4c439a1889289e7df6b/model_config.json",
     "7bf870cf7dd5a5fb6d02f17e1b89d17ecec5d06ff3f2ab3fc0f80577e42392fc"),
    ("sev/temperatures.json", "https://huggingface.co/LakoreAI/sev/resolve/08c6ae0ba7ee58871418a4c439a1889289e7df6b/temperatures.json",
     "9605824724443ce7a0f4f24371fb4e8edfd083307e90cfb94008d0e6c6396ab3"),
    ("modernbert-large/config.json", "https://huggingface.co/answerdotai/ModernBERT-large/resolve/45bb4654a4d5aaff24dd11d4781fa46d39bf8c13/config.json",
     "55dca68059e98f0debf8eb63ab355c451c65b1f5d90fdca4754113fdaadd2cca"),
    ("modernbert-large/tokenizer.json", "https://huggingface.co/answerdotai/ModernBERT-large/resolve/45bb4654a4d5aaff24dd11d4781fa46d39bf8c13/tokenizer.json",
     "9fd55248d51d33976b324fc11592e28071da7d41e0e9401dfb7082e30574b7b1"),
    ("modernbert-large/tokenizer_config.json", "https://huggingface.co/answerdotai/ModernBERT-large/resolve/45bb4654a4d5aaff24dd11d4781fa46d39bf8c13/tokenizer_config.json",
     "3cd2017ff46d0a527e5d39cae39272eccfa1f19bb9f89b05d166aab2e38354e2"),
    ("modernbert-large/special_tokens_map.json", "https://huggingface.co/answerdotai/ModernBERT-large/resolve/45bb4654a4d5aaff24dd11d4781fa46d39bf8c13/special_tokens_map.json",
     "ea97ecdbcc73713039d8d64dbb05e3689495c96657fbd9a18f5bed381be81049"),
]


def sha256(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        while b := f.read(1 << 20):
            h.update(b)
    return h.hexdigest()


def get(url, dest, want, *, max_bytes=2 * 1024 ** 3, timeout=1800):
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = tempfile.NamedTemporaryFile(prefix=dest.name + ".", suffix=".part", dir=dest.parent, delete=False)
    tmp = Path(part.name)
    deadline = time.monotonic() + timeout
    digest, total = hashlib.sha256(), 0
    try:
        with part as f, urllib.request.urlopen(url, timeout=min(60, timeout)) as r:
            # read1 avoids a server indefinitely trickling inside a large read().
            read = getattr(r, "read1", r.read)
            while True:
                if time.monotonic() >= deadline:
                    raise TimeoutError("model download deadline exceeded")
                b = read(min(1 << 20, max_bytes + 1 - total))
                if not b:
                    break
                total += len(b)
                if total > max_bytes:
                    raise ValueError("model download size limit exceeded")
                digest.update(b)
                f.write(b)
        if time.monotonic() >= deadline:
            raise TimeoutError("model download deadline exceeded")
        if digest.hexdigest() != want:
            raise ValueError("model download SHA-256 mismatch")
        tmp.replace(dest)
    finally:
        tmp.unlink(missing_ok=True)


def run(verify_only=False, only=None):
    if only and only.startswith("benchmarks"):
        raise ValueError("benchmark data is not part of the public core")
    out = []
    groups = [(MODELS, PINS)]
    for base, items in groups:
        for rel, url, want in items:
            if only and not rel.startswith(only):
                continue
            p = base / rel
            ok = p.exists() and sha256(p) == want
            action = "present"
            if not ok and not verify_only:
                get(url, p, want)
                ok, action = sha256(p) == want, "downloaded"
            out.append({"file": str(p), "ok": ok, "action": action if ok or not verify_only else "missing or wrong"})
    return {"all_ok": all(o["ok"] for o in out), "files": out}


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--verify", action="store_true")
    ap.add_argument("--only", help="limit to pinned files whose path starts with this (e.g. sev/config)")
    a = ap.parse_args()
    r = run(a.verify, a.only)
    print(json.dumps(r, indent=1))
    sys.exit(0 if r["all_ok"] else 1)
