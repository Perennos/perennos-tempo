"""Bounded, read-only install diagnostics. No inference, repair, or external HTTP."""
from __future__ import annotations

import json
import os
from pathlib import Path
import stat

MAX_FILE_BYTES = 1 << 20
SETUP_GATES = (
    "selftest", "decision_head_files", "decision_head_loads", "engines_synced",
    "priors", "qualified_local_engine",
)


def _safe_path(path: Path, *, directory=False):
    # Do not resolve first: that would hide linked folders and Windows junctions.
    for part in (*reversed(path.parents), path):
        info = part.lstat()
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError("linked diagnostic input")
    expected = stat.S_ISDIR if directory else stat.S_ISREG
    if not expected(info.st_mode):
        raise ValueError("unsupported diagnostic input")


def _read(path: Path, limit=MAX_FILE_BYTES):
    _safe_path(path)
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise ValueError("diagnostic input exceeds limit")
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("diagnostic input exceeds limit")
    return raw.decode("utf-8-sig")


def _object(path: Path):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON member")
            result[key] = value
        return result
    obj = json.loads(_read(path), object_pairs_hook=unique)
    if not isinstance(obj, dict):
        raise ValueError("expected object")
    return obj


def diagnose(home: Path, port: int = 8765, connect: bool = False) -> dict:
    """Return fixed diagnostic codes and hints; never copy private input into output.

    A passing connected report verifies configuration shape, recorded setup gates,
    and authenticated gateway access. It does not qualify a model or run an answer.
    """
    from tempo_client import _check_target
    _check_target("127.0.0.1", port)
    home = Path(home).expanduser().absolute()
    checks = []

    def add(code, ok, hint):
        checks.append({"code": code, "state": "Verified complete" if ok else "In progress", "hint": hint})

    def finish():
        failures = sum(c["state"] == "In progress" for c in checks)
        return {"schema_version": 1, "product": "Perennos Tempo",
                "state": "In progress" if failures else "Verified complete" if connect else "Untested",
                "checks": checks, "inference_tested": False,
                "scope": "Configuration and gateway access only; no answer was generated."}

    try:
        _safe_path(home, directory=True)
        add("home_available", True, "Data folder is accessible.")
    except (OSError, ValueError):
        add("home_unavailable", False, "Select an existing, unlinked data folder or complete setup.")
        return finish()

    for stem in ("tenant", "config"):
        try:
            _object(home / (stem + ".json"))
            add(stem + "_readable", True, "Configuration is readable JSON; policy contents were not exported.")
        except (OSError, ValueError, RecursionError):
            add(stem + "_invalid", False, "Configuration is missing, linked, oversized or invalid. Review setup locally.")

    try:
        receipt = _object(home / "setup-receipt.json")
        gates = receipt.get("gates")
        ready = (receipt.get("ready") is True and isinstance(gates, dict)
                 and all(gates.get(name) is True for name in SETUP_GATES))
        add("setup_recorded" if ready else "setup_incomplete", ready,
            "All required setup gates are recorded." if ready else "Setup gates are incomplete. Re-run reviewed setup; keep existing data.")
    except (OSError, ValueError, RecursionError):
        add("setup_invalid", False, "Setup receipt is unavailable or invalid. Complete setup before inference.")

    try:
        engines = _object(home / "engines.json").get("engines")
        found = (isinstance(engines, list) and any(isinstance(e, dict) and e.get("locality") == "local" for e in engines))
        add("local_engine_recorded" if found else "local_engine_missing", found,
            "A local engine is registered; live inference was not tested." if found else "Start the approved local engine and run engine sync.")
    except (OSError, ValueError, RecursionError):
        add("engines_invalid", False, "Engine registry is missing or invalid. Run setup or engine sync.")

    token = None
    try:
        token = _read(home / "ledger" / "gateway-token", 4096).strip()
        if len(token) < 32:
            raise ValueError("invalid token")
        add("token_available", True, "Existing gateway credential is readable; its value is never reported.")
    except (OSError, ValueError):
        token = None
        add("token_unavailable", False, "Gateway credential is unavailable. Complete setup; do not paste credentials into an issue.")

    if connect and token:
        previous_home = os.environ.get("TEMPO_HOME")
        try:
            from tempo_client import list_models
            # The installed Windows client additionally checks the selected
            # home's process receipt. Restore the caller's environment afterward.
            os.environ["TEMPO_HOME"] = str(home)
            payload, _ = list_models("127.0.0.1", port, token, timeout=5)
            if not isinstance(payload.get("data"), list) or not payload["data"]:
                raise ValueError("invalid model listing")
            add("gateway_authenticated", True, "Local gateway identity and authenticated model listing verified. No inference was run.")
        except Exception:
            add("gateway_unavailable", False, "Open Tempo, confirm its selected port, then retry. Listener trust or gateway access failed.")
        finally:
            if previous_home is None:
                os.environ.pop("TEMPO_HOME", None)
            else:
                os.environ["TEMPO_HOME"] = previous_home
    elif connect:
        add("gateway_unavailable", False, "Gateway probe requires the existing local credential. Complete setup first.")
    else:
        checks.append({"code": "gateway_not_probed", "state": "Untested",
                       "hint": "Use --connect for a bounded authenticated loopback probe; it sends no chat content."})
    return finish()


def exit_code(report: dict) -> int:
    return int(any(c["state"] == "In progress" for c in report["checks"]))
