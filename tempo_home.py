"""Where one Perennos Tempo install keeps its data, models and rules. One install = one owner (a person or a client).

Resolution: the TEMPO_HOME environment variable names the data folder. Without it, the code folder is used only if it
holds its own tenant.json (a single-owner install); otherwise this refuses, so a client never writes into the code.
The data folder holds tenant.json, config.json, attached.json, priors.json, prior_map.json, engines.json and ledger/.
Relative paths in tenant.json must stay inside the data folder; values are type-checked.
This is a namespace for separate runs, not an OS security boundary: give each client its own OS account or machine.

  python tempo_home.py          # print the resolved tenant
"""
import json, os
from pathlib import Path

CODE = Path(__file__).resolve().parent


class TenantError(RuntimeError):
    pass


def _data_dir():
    if os.environ.get("TEMPO_HOME"):
        return Path(os.environ["TEMPO_HOME"]).resolve()
    if (CODE / "tenant.json").exists():
        return CODE
    raise TenantError("set TEMPO_HOME to this owner's data folder (run tempo_setup.py first)")


DATA = _data_dir()

DEFAULTS = {
    "tenant": "default",
    "models_dir": "models/decision",            # relative paths resolve inside the data folder
    "ollama_url": "http://127.0.0.1:11434",
    "image_models_dir": None,                   # folders of local image / video weights to register (optional)
    "video_models_dir": None,
    # task or privacy classes never explored (no epsilon tries): mistakes there cost more than learning gains
    "never_explore": ["money", "publishing", "family", "private", "legal", "medical"],
    # task or privacy classes that must stay on locality "local" engines; others are excluded from ratings
    "local_only": ["private", "family", "legal", "medical"],
}
TYPES = {"tenant": str, "models_dir": str, "ollama_url": str, "image_models_dir": (str, type(None)),
         "video_models_dir": (str, type(None)), "never_explore": list, "local_only": list}


def _resolve(p):
    if not p:
        return None
    raw = Path(os.path.expandvars(str(p)))
    if raw.is_absolute():
        return raw                               # the owner chose an absolute path (e.g. shared weights on C:)
    full = (DATA / raw).resolve()
    if DATA not in full.parents and full != DATA:
        raise TenantError(f"relative path escapes the data folder: {p}")
    return full


def load():
    t = dict(DEFAULTS)
    f = DATA / "tenant.json"
    if f.exists():
        t.update(json.loads(f.read_text(encoding="utf-8-sig")))
    for k, ty in TYPES.items():
        if not isinstance(t.get(k), ty):
            raise TenantError(f"tenant.json: {k} has the wrong type")
    if not all(isinstance(x, str) for x in t["never_explore"] + t["local_only"]):
        raise TenantError("tenant.json: class lists must hold strings")
    t["data_dir"] = str(DATA)
    for k in ("models_dir", "image_models_dir", "video_models_dir"):
        r = _resolve(t.get(k))
        t[k] = str(r) if r else None
    return t


T = load()
MODELS = Path(T["models_dir"])
LEDGER_DIR = DATA / "ledger"

if __name__ == "__main__":
    print(json.dumps(T, indent=1))
