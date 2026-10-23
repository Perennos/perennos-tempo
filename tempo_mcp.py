"""A minimal JSON-RPC stdio adapter over the existing Perennos Tempo loopback API."""
from __future__ import annotations

import json
import sys

from tempo_client import TempoAPIError, chat, gateway_status, list_models

PROTOCOL_VERSION = "2025-06-18"
MAX_MESSAGE_BYTES = 1 << 20
MAX_OUTPUT_BYTES = 1 << 20
MAX_MODEL_ROWS = 100
MAX_OUTPUT_CHARS = 32_000
TOOLS = [
    {
        "name": "ask",
        "description": "Ask the local Perennos Tempo gateway. Requests are private by default.",
        "inputSchema": {
            "type": "object",
            "properties": {"prompt": {"type": "string", "minLength": 1, "maxLength": 32000}},
            "required": ["prompt"],
            "additionalProperties": False,
        },
    },
    {
        "name": "listmodels",
        "description": "List models exposed by the authenticated local Perennos Tempo gateway.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
    },
    {
        "name": "status",
        "description": "Check whether the local Perennos Tempo gateway process answers its health endpoint.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
    },
]
_TOOL_NAMES = {tool["name"] for tool in TOOLS}
# Fixed text: never forward exception detail for an ambiguous after-send failure.
_MAY_CONTINUE_MESSAGE = ("Perennos Tempo request outcome is unknown after send; the backend may continue. "
                         "Check receipts before retrying; no automatic replay was performed.")


class TempoMCP:
    def __init__(self, host, port, token):
        self.host = host
        self.port = port
        self.token = token

    @staticmethod
    def _error(message, code, request_id=None):
        return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}

    def _call_tool(self, params):
        if not isinstance(params, dict) or set(params) - {"name", "arguments"}:
            raise ValueError("invalid tools/call parameters")
        name = params.get("name")
        arguments = params.get("arguments", {})
        if not isinstance(name, str) or name not in _TOOL_NAMES:
            raise LookupError("unknown Perennos Tempo tool")
        if not isinstance(arguments, dict):
            raise ValueError("tool arguments must be an object")
        if name == "ask":
            if set(arguments) != {"prompt"}:
                raise ValueError("ask accepts only prompt")
            prompt = arguments["prompt"]
            if not isinstance(prompt, str) or not prompt.strip() or len(prompt) > 32_000:
                raise ValueError("prompt must contain 1 to 32000 characters")
            payload, headers = chat(self.host, self.port, self.token, prompt)
            choices = payload.get("choices")
            if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
                raise TempoAPIError("Perennos Tempo gateway returned no answer")
            message = choices[0].get("message")
            text = message.get("content") if isinstance(message, dict) else None
            if not isinstance(text, str) or len(text) > MAX_OUTPUT_CHARS:
                raise TempoAPIError("Perennos Tempo gateway answer exceeded its output limit")
            result = {"answer": text}
            receipt = headers.get("x-tempo-receipt-id")
            if receipt:
                result["receipt_id"] = receipt
            review = payload.get("tempo")
            if isinstance(review, dict) and isinstance(review.get("review_before_use"), bool):
                result["review_before_use"] = review["review_before_use"]
            return self._tool_result(result)
        if arguments:
            raise ValueError(f"{name} accepts no arguments")
        if name == "listmodels":
            payload, _ = list_models(self.host, self.port, self.token)
            rows = payload.get("data")
            if not isinstance(rows, list) or len(rows) > MAX_MODEL_ROWS:
                raise TempoAPIError("Perennos Tempo gateway returned an invalid model list")
            models = []
            for row in rows:
                if not isinstance(row, dict) or not isinstance(row.get("id"), str) or len(row["id"]) > 256:
                    raise TempoAPIError("Perennos Tempo gateway returned an invalid model list")
                models.append(row["id"])
            return self._tool_result({"models": models})
        payload, _ = gateway_status(self.host, self.port, self.token)
        return self._tool_result({
            "gateway_reachable": True,
            "process_status": payload.get("status") if isinstance(payload.get("status"), str) else "unknown",
            "mode": payload.get("mode") if isinstance(payload.get("mode"), str) else "unknown",
        })

    @staticmethod
    def _tool_result(value, is_error=False):
        text = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        return {"content": [{"type": "text", "text": text}], "isError": is_error}

    def dispatch(self, request):
        if not isinstance(request, dict) or request.get("jsonrpc") != "2.0" or not isinstance(request.get("method"), str):
            return self._error("Invalid Request", -32600)
        request_id = request.get("id")
        has_id = "id" in request
        if has_id and (isinstance(request_id, bool) or not isinstance(request_id, (str, int, type(None)))):
            return self._error("Invalid Request", -32600, None)
        method = request["method"]
        params = request.get("params", {})
        if not has_id:
            if method.startswith("notifications/"):
                return None
            return None
        if method == "initialize":
            if not isinstance(params, dict):
                return self._error("Invalid params", -32602, request_id)
            version = params.get("protocolVersion", PROTOCOL_VERSION)
            if not isinstance(version, str) or len(version) > 32:
                return self._error("Invalid params", -32602, request_id)
            return {"jsonrpc": "2.0", "id": request_id, "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": "Perennos Tempo", "version": "1.0.1"},
            }}
        if method == "ping":
            return {"jsonrpc": "2.0", "id": request_id, "result": {}}
        if method == "tools/list":
            if params not in ({}, None):
                return self._error("Invalid params", -32602, request_id)
            return {"jsonrpc": "2.0", "id": request_id, "result": {"tools": TOOLS}}
        if method == "tools/call":
            try:
                result = self._call_tool(params)
            except LookupError:
                result = self._tool_result({"error": "unknown Perennos Tempo tool"}, is_error=True)
            except ValueError as exc:
                result = self._tool_result({"error": str(exc)}, is_error=True)
            except TempoAPIError as exc:
                message = (_MAY_CONTINUE_MESSAGE if exc.request_may_continue else
                           "Perennos Tempo gateway request failed or was unavailable")
                result = self._tool_result({"error": message}, is_error=True)
            except (OSError, TypeError, KeyError):
                result = self._tool_result({"error": "Perennos Tempo gateway response was invalid"}, is_error=True)
            return {"jsonrpc": "2.0", "id": request_id, "result": result}
        return self._error("Method not found", -32601, request_id)


def _response_bytes(server, raw):
    if len(raw) > MAX_MESSAGE_BYTES:
        return server._error("Parse error", -32700)
    try:
        request = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError):
        return server._error("Parse error", -32700)
    return server.dispatch(request)


def serve(host, port, token, *, stdin=None, stdout=None):
    """Serve line-delimited JSON-RPC; stdout is reserved for protocol messages."""
    server = TempoMCP(host, port, token)
    source = stdin if stdin is not None else sys.stdin.buffer
    sink = stdout if stdout is not None else sys.stdout.buffer
    while True:
        raw = source.readline(MAX_MESSAGE_BYTES + 1)
        if not raw:
            return
        if len(raw) > MAX_MESSAGE_BYTES:
            response = server._error("Parse error", -32700)
            sink.write(json.dumps(response, separators=(",", ":")).encode("utf-8") + b"\n")
            sink.flush()
            return
        response = _response_bytes(server, raw.rstrip(b"\r\n"))
        if response is None:
            continue
        encoded = json.dumps(response, ensure_ascii=True, separators=(",", ":")).encode("utf-8")
        if len(encoded) > MAX_OUTPUT_BYTES:
            encoded = json.dumps(server._error("Response exceeded its size limit", -32603,
                                                None), separators=(",", ":")).encode("utf-8")
        sink.write(encoded + b"\n")
        sink.flush()
