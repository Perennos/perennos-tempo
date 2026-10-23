"""Production effect gate for the Perennos Tempo office gateway.

Why this exists: the accepted H3 gate (tempo_hands.policy) only recognises browser-shaped, screenshot-bound actions and
has no production trust source, so every desk tool would be denied. This gate is the production trust source for desk
tools (observe / act-reversible). It trusts exactly three things, all owned by the machine owner and none of them the
model, the client or the request body:

  1. the owner policy file, pinned by SHA-256 in a separate pin file (re-checked on EVERY call, so an edit after start
     closes the gate);
  2. the authenticated caller identity the gateway already derived from the bearer digest (carried on GateCall);
  3. a durable single-use ledger: the call id is consumed (committed, fsync'd) BEFORE the runner is allowed to start. A
     crash after that point leaves the row `consumed`, which is final: an unknown outcome is never replayed.

It refuses, with the gateway's gate vocabulary: act-risky and any unrecognised risk class (needs a human), tools that the
caller's pinned profile does not list, a risk claim that differs from the pinned policy, unknown or mismatched callers,
egress tools on a private route, and every call when the pin, the policy or the ledger cannot be trusted.
"""

import hashlib
import hmac
import json
import re
import sqlite3
import threading
import time
from pathlib import Path

import tool_policy

PIN_RE = re.compile(r"[0-9a-f]{64}")
MAX_POLICY_BYTES = 262_144
GATE_RISKS = ("observe", "act-reversible")


class _Unavailable(Exception):
    pass


class ProductionEffectGate:
    def __init__(self, policy_path, pin_path, ledger_path, clock=time.time):
        self.policy_path = Path(policy_path)
        self.pin_path = Path(pin_path)
        self.ledger_path = Path(ledger_path)
        self._clock = clock
        self._lock = threading.Lock()
        try:
            self._connect().close()                       # fail the ledger early and visibly; every call re-checks anyway
        except Exception:
            pass

    # ---- trust source
    def _trusted_policy(self):
        """The parsed policy, only if its bytes hash to the owner's pin right now."""
        try:
            raw = self.policy_path.read_bytes()
            if len(raw) > MAX_POLICY_BYTES:
                raise _Unavailable
            pin = self.pin_path.read_text(encoding="ascii").split()[0].lower()
        except (OSError, IndexError, UnicodeError):
            raise _Unavailable from None
        if not PIN_RE.fullmatch(pin):
            raise _Unavailable
        digest = hashlib.sha256(raw).hexdigest()
        if not hmac.compare_digest(digest, pin):
            raise _Unavailable
        try:
            text = raw.decode("utf-8-sig")
            if str(self.policy_path).lower().endswith(".json"):
                mapping = json.loads(text)
            else:
                import yaml
                mapping = yaml.safe_load(text)
            return tool_policy.load_policy(mapping), digest
        except Exception:
            raise _Unavailable from None

    # ---- durable single-use ledger
    def _connect(self):
        self.ledger_path.parent.mkdir(parents=True, exist_ok=True)
        db = sqlite3.connect(str(self.ledger_path), timeout=2.0, isolation_level=None)
        try:
            db.execute("PRAGMA journal_mode=DELETE")
            db.execute("PRAGMA synchronous=FULL")
            db.execute("CREATE TABLE IF NOT EXISTS consumed (call_id TEXT PRIMARY KEY, caller TEXT NOT NULL, "
                       "profile TEXT NOT NULL, tool TEXT NOT NULL, risk TEXT NOT NULL, args_sha256 TEXT NOT NULL, "
                       "policy_sha TEXT NOT NULL, at REAL NOT NULL, state TEXT NOT NULL)")
        except Exception:
            db.close()
            raise
        return db

    def _consume(self, call, spec_risk, policy_sha):
        """True exactly once per call id; commits before returning. Raises _Unavailable when the ledger cannot be trusted."""
        args_sha = hashlib.sha256(tool_policy._canon(call.args).encode("utf-8")).hexdigest()
        try:
            with self._lock:
                db = self._connect()
                try:
                    db.execute("BEGIN IMMEDIATE")
                    try:
                        db.execute("INSERT INTO consumed VALUES (?,?,?,?,?,?,?,?,?)",
                                   (call.call_id, call.caller, call.profile, call.tool, spec_risk, args_sha,
                                    policy_sha, self._clock(), "consumed"))
                    except sqlite3.IntegrityError:
                        db.execute("ROLLBACK")
                        return False
                    db.execute("COMMIT")
                    return True
                finally:
                    db.close()
        except sqlite3.Error:
            raise _Unavailable from None
        except OSError:
            raise _Unavailable from None

    def _mark(self, call_id, state):
        try:
            with self._lock:
                db = self._connect()
                try:
                    db.execute("UPDATE consumed SET state=? WHERE call_id=? AND state='consumed'", (state, call_id))
                finally:
                    db.close()
        except Exception:
            pass          # the row stays `consumed`, which is the conservative (never replay) state

    # ---- the gate
    def execute(self, call, runner):
        blocked = lambda why: tool_policy.GateResult("blocked", why)
        try:
            policy, policy_sha = self._trusted_policy()
        except _Unavailable:
            return blocked(tool_policy.R_GATE_UNAVAILABLE)
        caller, profile_name, tool = getattr(call, "caller", None), getattr(call, "profile", None), getattr(call, "tool", None)
        if type(call.call_id) is not str or not call.call_id or len(call.call_id) > 80:
            return blocked(tool_policy.R_GATE_BLOCKED)
        entry = policy._callers.get(caller) if type(caller) is str else None
        if entry is None or entry[1] != profile_name:
            return blocked(tool_policy.R_GATE_BLOCKED)
        profile = policy._profiles.get(entry[1])
        spec = profile.tools.get(tool) if profile is not None and type(tool) is str else None
        if spec is None or spec.risk != call.risk or spec.risk not in GATE_RISKS:
            return blocked(tool_policy.R_GATE_BLOCKED)
        if spec.locality == "egress" and getattr(call, "private", True) is not False:
            return blocked(tool_policy.R_GATE_BLOCKED)
        try:
            if not self._consume(call, spec.risk, policy_sha):
                return blocked(tool_policy.R_GATE_BLOCKED)
        except _Unavailable:
            return blocked(tool_policy.R_GATE_UNAVAILABLE)
        try:
            payload = runner(call.args)
        except tool_policy.GateAbandoned:
            self._mark(call.call_id, "abandoned")
            return blocked(tool_policy.R_GATE_BLOCKED)
        except BaseException:
            self._mark(call.call_id, "unknown")
            return tool_policy.GateResult("unknown", "effect may have occurred; no replay")
        self._mark(call.call_id, "executed")
        return tool_policy.GateResult("executed", payload)
