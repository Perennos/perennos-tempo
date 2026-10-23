"""Append-only SHA-256 chain for metadata-only Perennos Tempo job receipts."""
from __future__ import annotations

import hashlib
import hmac
import json
import os
import threading
import time
import uuid
from pathlib import Path

ZERO_HASH = "0" * 64
_LOCAL_LOCK = threading.RLock()


class ReceiptIntegrityError(RuntimeError):
    pass


def _canonical(value: dict) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def _has_content_key(value, where="receipt"):
    forbidden = {"prompt", "messages", "content", "response", "output", "input_text", "output_text",
                 "authorization", "api_key", "secret", "request_body", "response_body"}
    if isinstance(value, dict):
        for key, child in value.items():
            if str(key).lower() in forbidden:
                raise ValueError(f"{where} contains content-bearing field {key!r}; receipts store metadata only")
            _has_content_key(child, where + "." + str(key))
    elif isinstance(value, (list, tuple)):
        for index, child in enumerate(value):
            _has_content_key(child, f"{where}[{index}]")


def _locked(path: Path):
    """Cross-process append lock. The lock file is retained; OS locks release on process exit."""
    lock = path.with_suffix(path.suffix + ".lock")
    lock.parent.mkdir(parents=True, exist_ok=True)
    handle = lock.open("a+b")
    try:
        if os.name == "nt":
            import msvcrt
            handle.seek(0, os.SEEK_END)
            if handle.tell() == 0:
                handle.write(b"0")
                handle.flush()
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
        else:
            import fcntl
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        return handle
    except Exception:
        handle.close()
        raise


def _unlock(handle):
    try:
        if os.name == "nt":
            import msvcrt
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
    finally:
        handle.close()


def verify_chain(path, initial_hash=ZERO_HASH):
    path = Path(path)
    try:
        lines = path.read_text(encoding='utf-8').splitlines() if path.exists() else []
    except (OSError, UnicodeError) as exc:
        return {'ok':False, 'length':0, 'head':None, 'errors':[type(exc).__name__]}
    return _verify_lines(lines, initial_hash)


def _verify_lines(lines, initial_hash=ZERO_HASH):
    if not isinstance(initial_hash, str) or len(initial_hash) != 64 or any(c not in '0123456789abcdef' for c in initial_hash):
        return {'ok':False, 'length':0, 'head':None, 'errors':['invalid receipt anchor']}
    previous = initial_hash
    count = 0
    errors = []
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            errors.append(f"line {line_number}: blank lines are not permitted")
            continue
        try:
            row = json.loads(line, object_pairs_hook=_reject_duplicate_keys)
            if not isinstance(row, dict):
                raise ValueError("receipt row is not an object")
            claimed = row.pop("receipt_hash", None)
            if row.get("previous_hash") != previous:
                errors.append(f"line {line_number}: previous_hash mismatch")
            actual = hashlib.sha256(_canonical(row)).hexdigest()
            if not isinstance(claimed, str) or not hmac.compare_digest(claimed, actual):
                errors.append(f"line {line_number}: receipt_hash mismatch")
            previous = claimed if isinstance(claimed, str) else actual
            count += 1
        except Exception as exc:
            errors.append(f"line {line_number}: {type(exc).__name__}")
    return {"ok": not errors, "length": count, "head": previous if count else None, "errors": errors}


def verified_snapshot(path):
    """One immutable, verified receipt snapshot under the same lock as append."""
    path = Path(path)
    if not path.exists():
        return []
    with _LOCAL_LOCK:
        lock = _locked(path)
        try:
            lines = path.read_bytes().decode('utf-8').splitlines()
            checked = _verify_lines(lines)
            if not checked['ok']:
                raise ReceiptIntegrityError('source receipt chain failed verification; refusing pack')
            return [json.loads(line, object_pairs_hook=_reject_duplicate_keys) for line in lines]
        finally:
            _unlock(lock)


def append_receipt(path, payload, now=None):
    if not isinstance(payload, dict):
        raise TypeError("receipt payload must be an object")
    reserved = {"kind", "receipt_id", "recorded_at", "previous_hash", "receipt_hash"}
    conflict = reserved.intersection(payload)
    if conflict:
        raise ValueError(f"receipt payload uses reserved fields: {sorted(conflict)}")
    _has_content_key(payload)
    path = Path(path)
    with _LOCAL_LOCK:
        lock = _locked(path)
        try:
            checked = verify_chain(path)
            if not checked["ok"]:
                raise ReceiptIntegrityError("existing receipt chain failed verification; refusing append")
            row = {"kind": "tempo.receipt.v1", "receipt_id": uuid.uuid4().hex,
                   "recorded_at": now or time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                   "previous_hash": checked["head"] or ZERO_HASH, **payload}
            row["receipt_hash"] = hashlib.sha256(_canonical(row)).hexdigest()
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("ab") as stream:
                stream.write(_canonical(row) + b"\n")
                stream.flush()
                os.fsync(stream.fileno())
            return row
        finally:
            _unlock(lock)

