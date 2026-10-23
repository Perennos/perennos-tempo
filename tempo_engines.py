"""Perennos Tempo engine registry: attach any model with one entry, keyed by its real revision.

  python tempo_engines.py sync      # rebuild engines.json: Ollama models (digest = revision) + attached.json + image/video engines on disk
  python tempo_engines.py list

Attaching your own model: add an object to attached.json (same folder). Fields:
  id, provider (ollama | openai-compatible | anthropic | openai | google | comfyui), model, revision, locality (local | hosted),
  roles (thinking | research | inference | writing-code | imagery | video), notes.
No keys live here: hosted credentials stay where the provider's own tool keeps them.
A changed revision is a new engine cell in tempo_ratings (non-stationarity), so updates never inherit an old record.
Privacy: classes listed in tenant.json "local_only" may only route to locality "local" (policy enforces; receipts flag).
Paths come from tenant.json in the data folder (TEMPO_HOME), so each owner or client has their own registry.
"""
import json, sys, time, urllib.request
from pathlib import Path

HOME = Path(__file__).resolve().parent
sys.path.insert(0, str(HOME))
from tempo_home import T, DATA

OUT, ATTACHED = DATA / "engines.json", DATA / "attached.json"
OLLAMA = T["ollama_url"].rstrip("/") + "/api/tags"
IMAGE_ROOT = Path(T["image_models_dir"]) if T["image_models_dir"] else None
VIDEO_ROOT = Path(T["video_models_dir"]) if T["video_models_dir"] else None


def _ollama_roles(name):
    n = name.lower()
    if "embed" in n: return ["embedding"]
    # One qualified general-purpose default; role availability is not a quality rating.
    if n.removesuffix(":latest") == "qwen3:4b":
        return ["thinking", "research", "inference", "writing-code"]
    if "coder" in n: return ["writing-code"]
    if "gpt-oss" in n: return ["thinking", "research", "inference"]
    return ["research", "inference"]


def ollama():
    try:
        tags = json.load(urllib.request.urlopen(OLLAMA, timeout=5))["models"]
    except Exception as e:
        return [], f"ollama unreachable: {type(e).__name__}"
    from tempo_runtime import policy
    return [{"id": m["name"].removesuffix(":latest"), "provider": "ollama", "model": m["name"],
             "revision": m["digest"], "revision_kind": "ollama-manifest-digest", "locality": "local",
             "roles": _ollama_roles(m["name"]), "size": m.get("details", {}).get("parameter_size"),
             "quantization": m.get("details", {}).get("quantization_level"),
             "runtime": "ollama",'family':m.get('details',{}).get('family'),
             'runtime_policy':policy(m['name'],m.get('details',{}).get('family'))} for m in tags], None


def _sampled_hash(f, chunk=16 << 20):
    """SHA-256 over size + first and last 16 MB: cheap on 20 GB files, changes when the file is replaced."""
    import hashlib
    h = hashlib.sha256(str(f.stat().st_size).encode())
    with open(f, "rb") as fh:
        h.update(fh.read(chunk))
        fh.seek(max(0, f.stat().st_size - chunk))
        h.update(fh.read(chunk))
    return h.hexdigest()


def on_disk(root, role):
    if not root or not root.exists():
        return []
    out = []
    for f in sorted(root.rglob("*")):
        if f.suffix.lower() in (".gguf", ".safetensors") and f.stat().st_size > 1_000_000_000 \
                and not any(p in f.parts for p in ("text_encoders", "vae", "clip", "loras", "upscale_models")):
            out.append({"id": f.stem, "provider": "comfyui", "model": str(f), "revision": _sampled_hash(f),
                        "revision_kind": "sha256(size+first16MB+last16MB)", "locality": "local", "roles": [role]})
    return out


def sync():
    engines, note = ollama()
    engines += on_disk(IMAGE_ROOT, "imagery") + on_disk(VIDEO_ROOT, "video")
    if ATTACHED.exists():
        engines += json.loads(ATTACHED.read_text(encoding="utf-8"))
    doc = {"tenant": T["tenant"], "synced_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "note": note, "engines": engines}
    OUT.write_text(json.dumps(doc, indent=1), encoding="utf-8")
    return {"engines": len(engines), "by_role": {r: sum(r in e["roles"] for e in engines)
            for r in ("thinking", "research", "inference", "writing-code", "imagery", "video")}, "note": note}


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    if cmd == "sync":
        print(json.dumps(sync(), indent=1))
    else:
        for e in json.loads(OUT.read_text(encoding="utf-8"))["engines"]:
            print(f'{e["id"][:48]:48} {e["provider"]:18} {e["locality"]:7} {e["revision"]:16} {",".join(e["roles"])}')
