"""Small, bounded stdlib client for the local Perennos Tempo gateway."""
from __future__ import annotations

import http.client
import hashlib
import hmac
import json
import math
import os
import re
import secrets
import socket
import stat
import threading
import time
from pathlib import Path

LOOPBACK_HOSTS = {"127.0.0.1"}
MAX_REQUEST_BYTES = 1 << 20
MAX_RESPONSE_BYTES = 1 << 20
MAX_PROMPT_CHARS = 32_000
MAX_TIMEOUT_SECONDS = 660
ALLOWED_ENDPOINTS = {
    ("GET", "/health"),
    ("GET", "/v1/models"),
    ("POST", "/v1/chat/completions"),
}
SAFE_RESPONSE_HEADERS = {
    "x-tempo-receipt-id",
    "x-tempo-completion-receipt-id",
    "x-tempo-job-id",
    "x-tempo-profile",
    "x-tempo-review-required",
}
AMBIGUOUS_POST_MESSAGE = (
    "Perennos Tempo request outcome is unknown after send. The backend may continue; "
    "check the receipt before retrying; no automatic replay was performed."
)
CHALLENGE_HEADER = "X-Tempo-Gateway-Nonce"
PROOF_HEADER = "X-Tempo-Gateway-Proof"
_CHALLENGE_RE = re.compile(r"^[A-Za-z0-9_-]{43}$")


class TempoAPIError(RuntimeError):
    def __init__(self, message="Perennos Tempo gateway request failed", *, status=None, code=None,
                 request_may_continue=False):
        super().__init__(message)
        self.status = status
        self.code = code
        self.request_may_continue = request_may_continue


class TempoCancelled(TempoAPIError):
    def __init__(self, *, request_may_continue=False):
        message = "Perennos Tempo request was cancelled before send; it was not replayed."
        if request_may_continue:
            message = ("Perennos Tempo request was cancelled after send; the backend may continue. "
                       "Check receipts before retrying; no automatic replay was performed.")
        super().__init__(message, request_may_continue=request_may_continue)


class _ActiveRequest:
    def __init__(self, connection, retained_socket, cancel_event):
        self.connection = connection
        self.retained_socket = retained_socket
        self.cancel_event = cancel_event

    def close(self):
        self.cancel_event.set()
        try:
            self.retained_socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.connection.close()


def _check_target(host, port):
    if host not in LOOPBACK_HOSTS:
        raise ValueError("Perennos Tempo gateway must use a numeric loopback address")
    if isinstance(port, bool) or not isinstance(port, int) or not 1024 <= port <= 65535:
        raise ValueError("Perennos Tempo gateway port must be from 1024 to 65535")


def _check_timeout(timeout):
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not math.isfinite(timeout):
        raise ValueError("timeout must be finite")
    if not 0 < timeout <= MAX_TIMEOUT_SECONDS:
        raise ValueError("timeout is outside the supported limit")


def gateway_token(home):
    """Read the existing token at this install's ledger path without creating one."""
    home = Path(home).expanduser().resolve()
    if not home.is_dir():
        raise ValueError("Perennos Tempo data folder must be an existing directory")
    os.environ["TEMPO_HOME"] = str(home)
    ledger_path = home / "ledger"
    token_path = ledger_path / "gateway-token"
    reparse_point = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    for path in (ledger_path, token_path):
        try:
            file_stat = path.lstat()
        except OSError:
            continue
        if path.is_symlink() or getattr(file_stat, "st_file_attributes", 0) & reparse_point:
            raise TempoAPIError("Perennos Tempo gateway token path is invalid")
    if not token_path.is_file():
        raise TempoAPIError("Perennos Tempo gateway token is not initialized")
    from tempo_gateway import _restrict_token_file

    _restrict_token_file(token_path)
    try:
        with token_path.open("r", encoding="utf-8") as stream:
            token = stream.read(4097).strip()
    except (OSError, UnicodeError):
        raise TempoAPIError("Perennos Tempo gateway token is unavailable") from None
    if len(token) < 32 or len(token) > 4096:
        raise TempoAPIError("Perennos Tempo gateway token is invalid")
    return token


def _cancelled(cancel_event):
    return cancel_event is not None and cancel_event.is_set()


def _remaining(conn, deadline, cancel_event):
    if _cancelled(cancel_event):
        raise TempoCancelled()
    left = deadline - time.monotonic()
    if left <= 0:
        raise TempoAPIError("Perennos Tempo gateway request timed out")
    if conn.sock is not None:
        conn.sock.settimeout(left)
    return left


def _read_bounded(response, conn, deadline, cancel_event):
    length = response.getheader("Content-Length")
    if length is not None:
        try:
            if int(length) < 0 or int(length) > MAX_RESPONSE_BYTES:
                raise TempoAPIError("Perennos Tempo gateway response exceeded its size limit")
        except ValueError:
            raise TempoAPIError("Perennos Tempo gateway returned an invalid response length") from None
    chunks = []
    total = 0
    while True:
        _remaining(conn, deadline, cancel_event)
        chunk = response.read1(min(64 * 1024, MAX_RESPONSE_BYTES + 1 - total))
        if not chunk:
            break
        total += len(chunk)
        if total > MAX_RESPONSE_BYTES:
            raise TempoAPIError("Perennos Tempo gateway response exceeded its size limit")
        chunks.append(chunk)
    return b"".join(chunks)


def _gateway_proof(token, nonce, port):
    message = f"PerennosTempoGateway-v1\n{port}\n{nonce}".encode("ascii")
    return hmac.new(token.encode("utf-8"), message, hashlib.sha256).hexdigest()


class _OwnershipRequest:
    """Narrow request view for the bootstrap guard; guard failure closes without masking its error as cancel."""
    def __init__(self, connection):
        self.connection = connection

    def close(self):
        self.connection.close()


def _is_bootstrap_owner_guard(callback):
    if (getattr(callback, "__module__", None) not in {"tempo_bootstrap", "__main__"}
            or not getattr(callback, "__qualname__", "").endswith(
                "_connection_owner_guard.<locals>.verify_before_request")):
        return False
    try:
        from tempo_bootstrap import _connection_owner_guard
        expected = _connection_owner_guard({}, Path("."), 1024)
    except Exception:
        return False
    # Native setup executes the same file as __main__; its separately compiled
    # code object is equal, but cannot have the imported object's identity.
    return getattr(callback, "__code__", None) == getattr(expected, "__code__", None)


def _installed_windows_owner_guard(port):
    """Use the finished app receipt when present; source/headless installs use the nonce proof."""
    if os.name != "nt" or not os.environ.get("TEMPO_HOME"):
        return None
    home = Path(os.environ["TEMPO_HOME"]).expanduser().resolve()
    receipt_path = home / "desktop-install-receipt.json"
    try:
        receipt_stat = receipt_path.lstat()
    except FileNotFoundError:
        return None
    except OSError:
        raise TempoAPIError("Perennos Tempo listener ownership could not be verified") from None
    reparse_point = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    if receipt_path.is_symlink() or getattr(receipt_stat, "st_file_attributes", 0) & reparse_point:
        raise TempoAPIError("Perennos Tempo listener ownership could not be verified")
    try:
        with receipt_path.open("r", encoding="utf-8-sig") as stream:
            raw = stream.read(1 << 20 | 1)
        if len(raw) > 1 << 20:
            raise ValueError("receipt exceeds limit")
        receipt = json.loads(raw)
        record = receipt["gateway_process"]
        home_record = Path(receipt["data_home"]).expanduser().resolve()
        bootstrap_python = Path(receipt["bootstrap_python_path"]).expanduser().resolve()
        expected_python = (bootstrap_python.parents[2] / "runtime" / "python" / "python.exe").resolve()
        app_root = bootstrap_python.parents[2]
        executable = (app_root / "runtime" / "venv" / "Scripts" / "python.exe").resolve()
        if (not isinstance(receipt, dict) or receipt.get("product") != "Perennos Tempo"
                or receipt.get("state") != "Verified complete"
                or receipt.get("first_private_answer") is not True
                or not isinstance(receipt.get("first_completion_receipt_id"), str)
                or receipt.get("first_completion_receipt_id") == ""
                or home_record != home or not isinstance(record, dict)
                or type(record.get("port")) is not int or record["port"] != port
                or record.get("home") != str(home)
                or bootstrap_python != expected_python
                or Path(record.get("launcher_path", "")).expanduser().resolve() != executable
                or not isinstance(receipt.get("bootstrap_python_sha256"), str)):
            raise ValueError("receipt does not bind this listener")
        from tempo_bootstrap import _connection_owner_guard
        return _connection_owner_guard(record, executable, port, python=bootstrap_python,
                                       python_sha256=receipt["bootstrap_python_sha256"])
    except Exception:
        raise TempoAPIError("Perennos Tempo listener ownership could not be verified") from None


def request_json(host, port, token, method, path, body=None, *, timeout=15, cancel_event=None,
                 on_connection=None):
    """Call one fixed API endpoint directly; never follows redirects or uses a proxy."""
    _check_target(host, port)
    _check_timeout(timeout)
    method = method.upper() if isinstance(method, str) else ""
    if (method, path) not in ALLOWED_ENDPOINTS:
        raise ValueError("unsupported Perennos Tempo gateway endpoint")
    if (method == "POST") != (body is not None):
        raise ValueError("the POST endpoint requires a JSON request body")
    if (method, path) != ("GET", "/health") and (not isinstance(token, str) or len(token) < 32):
        raise ValueError("Perennos Tempo gateway token is required")
    raw_body = None
    headers = {"Accept": "application/json"}
    if body is not None:
        if method != "POST" or not isinstance(body, dict):
            raise ValueError("request body must be a JSON object for a POST endpoint")
        raw_body = json.dumps(body, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if len(raw_body) > MAX_REQUEST_BYTES:
            raise ValueError("Perennos Tempo gateway request exceeded its size limit")
        headers["Content-Type"] = "application/json; charset=utf-8"
    is_health = (method, path) == ("GET", "/health")
    if token and not is_health:
        headers["Authorization"] = "Bearer " + token

    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    conn.auto_open = 0
    deadline = time.monotonic() + timeout
    timer = None
    deadline_socket = None
    request_started = False
    active_cancel = cancel_event if cancel_event is not None else threading.Event()
    try:
        conn.connect()
        if time.monotonic() >= deadline:
            raise TempoAPIError("Perennos Tempo gateway request timed out")
        deadline_socket = conn.sock.dup()

        def interrupt_at_deadline():
            try:
                deadline_socket.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

        timer = threading.Timer(max(0, deadline - time.monotonic()), interrupt_at_deadline)
        timer.daemon = True
        timer.start()
        if time.monotonic() >= deadline:
            interrupt_at_deadline()
            raise TempoAPIError("Perennos Tempo gateway request timed out")
        _remaining(conn, deadline, active_cancel)
        supplied_owner_guard = _is_bootstrap_owner_guard(on_connection)
        owner_guard = on_connection if supplied_owner_guard else _installed_windows_owner_guard(port)
        active_request = _ActiveRequest(conn, deadline_socket, active_cancel)
        if owner_guard is not None:
            try:
                owner_guard(_OwnershipRequest(conn))
            except Exception:
                raise TempoAPIError("Perennos Tempo gateway listener ownership could not be verified") from None
            if conn.sock is None:
                raise TempoAPIError("Perennos Tempo gateway listener ownership could not be verified")
        if on_connection is not None:
            if on_connection is not owner_guard:
                try:
                    on_connection(active_request)
                except TempoAPIError:
                    raise
                except Exception:
                    if _cancelled(active_cancel):
                        raise TempoCancelled() from None
                    raise TempoAPIError("Perennos Tempo gateway request could not be prepared") from None
            if active_cancel.is_set() or conn.sock is None:
                raise TempoCancelled()
        _remaining(conn, deadline, active_cancel)
        if not is_health:
            nonce = secrets.token_urlsafe(32)
            if not _CHALLENGE_RE.fullmatch(nonce):
                raise TempoAPIError("Perennos Tempo gateway challenge could not be created")
            conn.request("GET", "/health", headers={"Accept": "application/json", CHALLENGE_HEADER: nonce})
            _remaining(conn, deadline, active_cancel)
            challenge = conn.getresponse()
            challenge_raw = _read_bounded(challenge, conn, deadline, active_cancel)
            if challenge.status != 200:
                raise TempoAPIError("Perennos Tempo gateway listener could not be verified")
            proof = challenge.getheader(PROOF_HEADER)
            expected_proof = _gateway_proof(token, nonce, port)
            if (not isinstance(proof, str) or not re.fullmatch(r"[a-f0-9]{64}", proof)
                    or not hmac.compare_digest(proof, expected_proof)
                    or challenge.will_close or conn.sock is None):
                raise TempoAPIError("Perennos Tempo gateway listener could not be verified")
            # Consume the bounded secret-free challenge body before reusing this exact socket.
            del challenge_raw
            _remaining(conn, deadline, active_cancel)
        request_started = True
        conn.request(method, path, body=raw_body, headers=headers)
        _remaining(conn, deadline, active_cancel)
        response = conn.getresponse()
        if not 200 <= response.status < 300:
            kind = "gateway_error"
            if response.status not in {301, 302, 303, 307, 308}:
                try:
                    raw = _read_bounded(response, conn, deadline, active_cancel)
                    payload = json.loads(raw.decode("utf-8"))
                    err = payload.get("error", {}) if isinstance(payload, dict) else {}
                    candidate = err.get("code") or err.get("type") if isinstance(err, dict) else None
                    if isinstance(candidate, str) and re.fullmatch(r"[A-Za-z0-9_.-]{1,64}", candidate):
                        kind = candidate
                except (TempoAPIError, UnicodeDecodeError, json.JSONDecodeError, RecursionError,
                        OSError, http.client.HTTPException):
                    pass
            raise TempoAPIError(f"Perennos Tempo gateway returned HTTP {response.status} ({kind})",
                                status=response.status, code=kind)
        raw = _read_bounded(response, conn, deadline, active_cancel)
        try:
            payload = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError, RecursionError):
            raise TempoAPIError("Perennos Tempo gateway returned invalid JSON", status=response.status) from None
        if not isinstance(payload, dict):
            raise TempoAPIError("Perennos Tempo gateway returned an invalid response", status=response.status)
        safe_headers = {key.lower(): value for key, value in response.getheaders()
                        if key.lower() in SAFE_RESPONSE_HEADERS and len(value) <= 256}
        return payload, safe_headers
    except TempoAPIError as exc:
        if _cancelled(active_cancel):
            raise TempoCancelled(request_may_continue=request_started) from None
        if request_started and method == "POST" and (
                exc.status is None or 200 <= exc.status < 300):
            raise TempoAPIError(AMBIGUOUS_POST_MESSAGE, status=exc.status, code=exc.code,
                                request_may_continue=True) from None
        if request_started and exc.status is None and time.monotonic() >= deadline:
            raise TempoAPIError(
                "Perennos Tempo request timed out after send; the backend may continue. "
                "Check receipts before retrying; no automatic replay was performed.",
                request_may_continue=True,
            ) from None
        raise
    except (OSError, http.client.HTTPException, TimeoutError):
        if _cancelled(active_cancel):
            raise TempoCancelled(request_may_continue=request_started) from None
        if request_started and method == "POST":
            raise TempoAPIError(AMBIGUOUS_POST_MESSAGE, request_may_continue=True) from None
        if time.monotonic() >= deadline:
            if request_started:
                raise TempoAPIError(
                    "Perennos Tempo request timed out after send; the backend may continue. "
                    "Check receipts before retrying; no automatic replay was performed.",
                    request_may_continue=True,
                ) from None
            raise TempoAPIError("Perennos Tempo gateway request timed out") from None
        raise TempoAPIError("Perennos Tempo gateway is unavailable") from None
    finally:
        if timer is not None:
            timer.cancel()
        conn.close()
        if deadline_socket is not None:
            deadline_socket.close()


def chat(host, port, token, prompt, *, timeout=610, cancel_event=None, on_connection=None):
    if not isinstance(prompt, str) or not prompt.strip() or len(prompt) > MAX_PROMPT_CHARS:
        raise ValueError("prompt must contain 1 to 32000 characters")
    return chat_messages(host, port, token, [{"role": "user", "content": prompt}],
                         timeout=timeout, cancel_event=cancel_event, on_connection=on_connection)


def chat_messages(host, port, token, messages, *, timeout=610, cancel_event=None, on_connection=None):
    """Send explicit text turns through the same private, authenticated boundary."""
    if not isinstance(messages, list) or not 1 <= len(messages) <= 100:
        raise ValueError("messages must contain 1 to 100 text messages")
    clean = []
    for message in messages:
        if (not isinstance(message, dict) or not isinstance(message.get("role"), str)
                or message["role"] not in {"system", "user", "assistant"}
                or not isinstance(message.get("content"), str) or len(message["content"]) > MAX_PROMPT_CHARS):
            raise ValueError("each message needs a supported role and at most 32000 text characters")
        try:
            message["content"].encode("utf-8")
        except UnicodeError:
            raise ValueError("message must be valid Unicode text") from None
        clean.append({"role": message["role"], "content": message["content"]})
    body = {
        "model": "tempo/auto",
        "messages": clean,
        "metadata": {"privacy_class": "private"},
        "stream": False,
        "max_tokens": 1024,
        "temperature": 0.2,
    }
    return request_json(host, port, token, "POST", "/v1/chat/completions", body,
                        timeout=timeout, cancel_event=cancel_event, on_connection=on_connection)


def list_models(host, port, token, *, timeout=15):
    return request_json(host, port, token, "GET", "/v1/models", timeout=timeout)


def gateway_status(host, port, token, *, timeout=5):
    return request_json(host, port, token, "GET", "/health", timeout=timeout)
