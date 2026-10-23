"""Small, bounded stdlib client for the local Perennos Tempo gateway."""
from __future__ import annotations

import http.client
import json
import math
import os
import re
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
    if token:
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
        if on_connection is not None:
            on_connection(_ActiveRequest(conn, deadline_socket, active_cancel))
            if active_cancel.is_set() or conn.sock is None:
                raise TempoCancelled()
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
                except (TempoAPIError, UnicodeDecodeError, json.JSONDecodeError, OSError, http.client.HTTPException):
                    pass
            raise TempoAPIError(f"Perennos Tempo gateway returned HTTP {response.status} ({kind})",
                                status=response.status, code=kind)
        raw = _read_bounded(response, conn, deadline, active_cancel)
        try:
            payload = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
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
    body = {
        "model": "tempo/auto",
        "messages": [{"role": "user", "content": prompt}],
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
