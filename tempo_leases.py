"""Fail-closed wrapper for the shared local GPU lease."""
from __future__ import annotations

import contextlib
import subprocess
import sys
from pathlib import Path


class LeaseError(RuntimeError):
    pass


@contextlib.contextmanager
def gpu_lease(script, holder, need_mib, ttl_min=20, wait_min=0, python=None):
    """Hold the shared lease across exactly one local inference."""
    if isinstance(need_mib, bool) or not isinstance(need_mib, int) or need_mib < 1:
        raise LeaseError("a positive GPU memory estimate is required")
    path = Path(script).expanduser().resolve() if script else None
    if not path or not path.is_file():
        raise LeaseError("GPU lease script is missing; refusing inference")
    interpreter = python or sys.executable
    acquire = [interpreter, str(path), "acquire", "--holder", str(holder),
               "--portfolio", "perennos", "--need-mib", str(need_mib),
               "--ttl-min", str(max(1, int(ttl_min))), "--wait-min", str(max(0, int(wait_min))),
               "--max-util", "10"]
    try:
        result = subprocess.run(acquire, capture_output=True, text=True, timeout=35)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise LeaseError(f"GPU lease request failed: {type(exc).__name__}") from exc
    if result.returncode != 0:
        raise LeaseError("GPU lease was not granted; refusing inference")
    try:
        yield
    finally:
        release = [interpreter, str(path), "release", "--holder", str(holder)]
        try:
            released = subprocess.run(release, capture_output=True, text=True, timeout=35)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise LeaseError(f"GPU lease release failed: {type(exc).__name__}") from exc
        if released.returncode != 0:
            raise LeaseError("GPU lease release was not confirmed")
