"""Bounded, ownership-checked Ollama model pulls for desktop bootstrap."""

from __future__ import annotations

import http.client
import json
import math
import socket
import threading
import time
from contextlib import suppress


MAX_EVENT_BYTES = 64 * 1024


class PullError(RuntimeError):
    """A safe model-pull failure that never includes response content."""


def _event(line, completed_by_digest, last_status):
    try:
        value = json.loads(line.decode("utf-8"))
    except (UnicodeError, ValueError, RecursionError):
        raise PullError("The owned local engine returned invalid pull JSON.") from None
    if not isinstance(value, dict):
        raise PullError("The owned local engine returned an invalid pull event.")
    if value.get("error"):
        raise PullError("The owned local engine reported a model pull error.")

    status = value.get("status")
    moved = isinstance(status, str) and status != last_status
    if isinstance(status, str):
        last_status = status

    digest = value.get("digest")
    completed = value.get("completed")
    if isinstance(digest, str) and digest:
        if digest not in completed_by_digest:
            if len(completed_by_digest) >= 4096:
                raise PullError("The owned local engine returned too many pull parts.")
            completed_by_digest[digest] = completed if type(completed) is int and completed >= 0 else None
            moved = True
        elif type(completed) is int and completed > (completed_by_digest[digest] or 0):
            completed_by_digest[digest] = completed
            moved = True

    success = value.get("done") is True or status == "success"
    return moved or success, last_status, success


def _timeout(value, label):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
        raise ValueError(f"{label} must be a finite positive number")
    return float(value)


def _interrupt(sock):
    with suppress(OSError):
        sock.shutdown(socket.SHUT_RDWR)


def _response_socket(response, retained_socket):
    """Return HTTPResponse's socket file transport, including will-close replies."""
    file_obj = getattr(response, "fp", None)
    raw = getattr(file_obj, "raw", None)
    response_socket = getattr(raw, "_sock", None)
    if response_socket is not None and callable(getattr(response_socket, "settimeout", None)):
        return response_socket
    return retained_socket


def _remaining(deadline, clock):
    remaining = deadline - clock()
    if remaining <= 0:
        raise PullError("The local model pull exceeded its overall deadline.")
    return remaining


def pull_owned_model(model, port, owner_record, owner_executable, *,
                     assert_owned_listener, assert_connected_peer,
                     idle_timeout=60, overall_timeout=7200, max_attempts=3,
                     clock=time.monotonic):
    """Pull through an owned listener; heartbeat bytes alone never reset progress."""
    idle_timeout = _timeout(idle_timeout, "idle_timeout")
    overall_timeout = _timeout(overall_timeout, "overall_timeout")
    if type(max_attempts) is not int or not 1 <= max_attempts <= 3:
        raise ValueError("max_attempts must be an integer from 1 to 3")
    if (not isinstance(model, str) or not model or len(model) > 512
            or any(ord(char) < 32 or ord(char) == 127 for char in model)):
        raise ValueError("model must be a non-empty, bounded model name")
    if type(port) is not int or not 1 <= port <= 65535:
        raise ValueError("port must be a valid TCP port")
    if not isinstance(owner_record, dict) or owner_record.get("port") != port:
        raise PullError("The local model pull requires a matching owned-listener record.")
    if not callable(assert_owned_listener) or not callable(assert_connected_peer):
        raise PullError("The local model pull requires both listener ownership checks.")

    deadline = clock() + overall_timeout
    body = json.dumps({"name": model, "stream": True}, separators=(",", ":")).encode("utf-8")
    for attempt in range(1, max_attempts + 1):
        progress_at = clock()
        last_status = None
        completed_by_digest = {}
        remaining = _remaining(deadline, clock)

        conn = None
        transport = None
        read_socket = None
        timer = None
        deadline_socket = None
        response = None
        pull_completed = False
        try:
            try:
                assert_owned_listener(owner_record, owner_executable, port)
            except Exception:
                raise PullError("The local model pull could not verify its recorded listener.") from None
            conn = http.client.HTTPConnection("127.0.0.1", port,
                                              timeout=min(idle_timeout, _remaining(deadline, clock)))
            conn.auto_open = 0
            conn.connect()
            if conn.sock is None:
                raise PullError("The local model pull could not open its verified listener.")
            # HTTPConnection.getresponse() clears/closes conn.sock for a
            # will-close response. Keep the accepted transport through reads.
            transport = conn.sock
            _remaining(deadline, clock)
            try:
                assert_connected_peer(transport, owner_record, owner_executable, port)
            except Exception:
                raise PullError("The connected model pull peer failed ownership verification.") from None
            deadline_socket = transport.dup()
            timer = threading.Timer(_remaining(deadline, clock), _interrupt, args=(deadline_socket,))
            timer.daemon = True
            timer.start()
            remaining = _remaining(deadline, clock)
            transport.settimeout(min(idle_timeout, remaining))
            conn.request("POST", "/api/pull", body=body,
                         headers={"Accept": "application/json", "Content-Type": "application/json",
                                  "Connection": "close"})
            response = conn.getresponse()
            if response.status != 200:
                raise PullError(f"The owned local engine returned HTTP {response.status} for model pull.")
            read_socket = _response_socket(response, transport)

            pending = bytearray()
            while True:
                now = clock()
                remaining = _remaining(deadline, clock)
                idle_left = idle_timeout - (now - progress_at)
                if idle_left <= 0:
                    raise TimeoutError("The local model pull made no streamed progress.")
                read_socket.settimeout(min(idle_left, remaining))
                block = response.read1(MAX_EVENT_BYTES)
                _remaining(deadline, clock)
                if not block:
                    if not pending:
                        break
                    pending.append(10)
                elif len(block) > MAX_EVENT_BYTES:
                    raise PullError("The owned local engine returned an oversized pull event.")
                else:
                    pending.extend(block)
                while True:
                    newline = pending.find(b"\n")
                    if newline < 0:
                        break
                    if newline > MAX_EVENT_BYTES:
                        raise PullError("The owned local engine returned an oversized pull event.")
                    line = bytes(pending[:newline]).rstrip(b"\r")
                    del pending[:newline + 1]
                    if not line:
                        continue
                    moved, last_status, success = _event(line, completed_by_digest, last_status)
                    if moved:
                        progress_at = clock()
                    if success:
                        pull_completed = True
                        return None
                if len(pending) > MAX_EVENT_BYTES:
                    raise PullError("The owned local engine returned an oversized pull event.")
                if not block:
                    break
            raise PullError("The owned local engine ended the model pull without a success event.")
        except PullError:
            raise
        except (OSError, http.client.HTTPException, TimeoutError) as exc:
            now = clock()
            if now >= deadline:
                raise PullError("The local model pull exceeded its overall deadline.") from None
            if isinstance(exc, TimeoutError) and now - progress_at >= idle_timeout:
                if attempt < max_attempts:
                    continue
                raise PullError(f"The local model pull made no streamed progress after {max_attempts} attempts.") from None
            raise PullError("The local model pull failed before completion.") from None
        finally:
            if timer is not None:
                timer.cancel()
            # Abort the old pull before a retry opens a replacement request.
            if transport is not None and not pull_completed:
                _interrupt(transport)
            for resource in (response, deadline_socket, conn, transport):
                if resource is not None:
                    with suppress(OSError):
                        resource.close()

    raise PullError(f"The local model pull made no streamed progress after {max_attempts} attempts.")
