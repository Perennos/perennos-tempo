"""Privacy-minimal implicit feedback transitions for a local Tempo conversation."""
from __future__ import annotations

import hashlib
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

from tempo_store import append_jsonl, atomic_json, file_lock

EDIT_MARKERS = ("fix", "edit", "rewrite", "change", "correct", "revise", "wrong", "redo", "improve",
                "you said", "your answer", "the draft", "the output")
FEEDBACK_EVIDENCE = {"accept": "owner_accept", "edit": "owner_edit", "reject": "owner_reject"}


def _iso(value=None):
    value = value or datetime.now(timezone.utc)
    return value.astimezone(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def _load_rows(path):
    if not Path(path).exists():
        return []
    result = []
    for line_no, line in enumerate(Path(path).read_text(encoding="utf-8").splitlines(), 1):
        if line.strip():
            try:
                result.append(json.loads(line))
            except ValueError as exc:
                raise ValueError(f"outcomes ledger is invalid at line {line_no}") from exc
    return result


def infer_transition(previous, messages, request_sha256, now=None, feedback_sha256=None, implicit_action=None):
    if not isinstance(previous, dict) or not previous.get("attempt_id"):
        return None
    messages = messages if isinstance(messages, list) else []
    latest_user = next((m.get("content", "") for m in reversed(messages) if m.get("role") == "user"), "")
    if not isinstance(latest_user, str) or not latest_user.strip():
        return None
    has_assistant = any(m.get("role") == "assistant" for m in messages)
    base = {k: previous.get(k) for k in ("attempt_id", "engine_id", "revision", "role", "task_class",
                                         "policy_tier", "deterministic_rank")}
    base.update({"verdict_by": "implicit", "judged_at": _iso(now)})
    if implicit_action == 'regenerate':
        return {**base, 'verdict': 'reject', 'evidence': 'workspace_regenerate_action'}
    if (feedback_sha256 and feedback_sha256 == previous.get('feedback_sha256')) or (
            not has_assistant and request_sha256 == previous.get("request_sha256")):
        return {**base, "verdict": "reject", "evidence": "same_conversation_input_regenerated" if feedback_sha256 else "same_request_regenerated_without_assistant"}
    lowered = latest_user.casefold()
    if any(marker in lowered for marker in EDIT_MARKERS):
        return {**base, "verdict": "edit", "evidence": "followup_requests_revision"}
    due = (now or datetime.now(timezone.utc)) + timedelta(hours=24)
    return {**base, "verdict": "unjudged", "evidence": "conversation_moved_on_pending_24h",
            "finalize_after": _iso(due)}


def explicit_feedback(attempt, verdict, evidence=None, now=None):
    if verdict not in FEEDBACK_EVIDENCE:
        raise ValueError("verdict must be accept, edit or reject")
    if not isinstance(attempt, dict) or not attempt.get("attempt_id"):
        raise ValueError("attempt not found")
    attempt = dict(attempt)
    if attempt.get('revision') and attempt.get('engine_revision') and attempt['revision'] != attempt['engine_revision']:
        raise ValueError('attempt revision fields disagree')
    attempt['revision'] = attempt.get('revision') or attempt.get('engine_revision')
    required = ("engine_id", "revision", "role", "task_class")
    if any(not attempt.get(key) for key in required):
        raise ValueError("attempt is missing rating fields")
    expected = FEEDBACK_EVIDENCE[verdict]
    if evidence is not None and evidence != expected:
        raise ValueError("evidence must match the selected owner verdict")
    return {k: attempt.get(k) for k in ("attempt_id", "engine_id", "revision", "role", "task_class",
                                        "policy_tier", "deterministic_rank")} | {
        "verdict": verdict, "verdict_by": "owner", "evidence": expected, "judged_at": _iso(now)}


def finalize_due(outcomes_path, now=None):
    path = Path(outcomes_path)
    with file_lock(path.with_suffix(path.suffix + ".finalize.lock")):
        return _finalize_due_locked(path, now)


def _finalize_due_locked(outcomes_path, now=None):
    now = now or datetime.now(timezone.utc)
    latest = {}
    for row in _load_rows(outcomes_path):
        if row.get("attempt_id"):
            latest[row["attempt_id"]] = row
    created = []
    for attempt_id, row in latest.items():
        if row.get("verdict") != "unjudged" or row.get("verdict_by") != "implicit":
            continue
        try:
            due = datetime.fromisoformat(str(row["finalize_after"]).replace("Z", "+00:00"))
        except (KeyError, ValueError):
            continue
        if due > now:
            continue
        accepted = {k: v for k, v in row.items() if k not in {"finalize_after"}}
        accepted.update({"verdict": "accept", "evidence": "conversation_moved_on_for_24h", "judged_at": _iso(now)})
        append_jsonl(outcomes_path, accepted)
        created.append(accepted)
    return created


class ConversationTracker:
    def __init__(self, directory, conversation_hash):
        if not isinstance(conversation_hash, str) or len(conversation_hash) != 64 or any(c not in "0123456789abcdef" for c in conversation_hash):
            raise ValueError("conversation key must be a SHA-256 hex digest")
        self.directory = Path(directory)
        self.path = self.directory / f"{conversation_hash}.json"
        self.lock_path = self.directory / f"{conversation_hash}.lock"
        self._lock = None

    def acquire(self):
        if self._lock is not None:
            raise RuntimeError("conversation is already locked")
        self._lock = file_lock(self.lock_path)
        self._lock.__enter__()

    def release(self):
        if self._lock is not None:
            self._lock.__exit__(None, None, None)
            self._lock = None

    def load(self):
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return None
        if not isinstance(data, dict):
            raise ValueError("conversation state must be an object")
        return data

    def save(self, state):
        if not isinstance(state, dict) or not state.get("attempt_id"):
            raise ValueError("conversation state requires an attempt id")
        allowed = {"attempt_id", "request_sha256", "response_sha256", "completed_at", "task_class", "role",
                   "privacy_class", "engine_id", "revision", "policy_tier", "deterministic_rank", "feedback_sha256"}
        if set(state) - allowed:
            raise ValueError("conversation state contains an unsupported field")
        atomic_json(self.path, state)
