"""Owner-attached hosted adapters; no redirects, retries or guessed prices."""
from __future__ import annotations
import ipaddress
import json
import os
import socket
import urllib.parse
import urllib.request
from decimal import Decimal, InvalidOperation

ENDPOINTS = {"openai": "https://api.openai.com/v1", "anthropic": "https://api.anthropic.com/v1"}
KEYS = {"openai": "OPENAI_API_KEY", "anthropic": "ANTHROPIC_API_KEY", "groq": "GROQ_API_KEY",
        "xai": "XAI_API_KEY", "openrouter": "OPENROUTER_API_KEY"}
HOSTS = {"api.openai.com", "api.anthropic.com", "api.groq.com", "api.x.ai", "openrouter.ai"}

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise RuntimeError("hosted redirect refused")

def provider(engine):
    return engine.get("key_provider") or engine.get("credential_provider") or engine.get("provider")

def credential(engine, tenant):
    p = provider(engine)
    if p not in tenant.get("allowed_providers", []):
        raise ValueError("provider not attached by owner")
    key = os.environ.get(KEYS.get(p, ""), "")
    if not key:
        raise ValueError("provider credential unavailable")
    return key

def _number(value):
    if isinstance(value, bool):
        raise ValueError("invalid cost")
    try:
        d = Decimal(str(value))
    except (InvalidOperation, TypeError):
        raise ValueError("unknown cost")
    if not d.is_finite() or d < 0:
        raise ValueError("invalid cost")
    return d

def reserve(engine, profile, messages, max_tokens):
    price = engine.get("price_per_million", {})
    if price.get("currency") != "USD":
        raise ValueError("reviewed USD token prices required")
    inp, out = _number(price.get("input")), _number(price.get("output"))
    cap = _number(profile.get("max_cost_per_job"))
    if cap <= 0:
        raise ValueError("profile hosted budget disabled")
    # One token per UTF-8 byte plus deliberately generous message/protocol overhead.
    bound = sum(len(m["content"].encode("utf-8")) + 64 for m in messages) + 1024
    cost = (inp * bound + out * max_tokens) / 1_000_000
    if cost > cap:
        raise ValueError("profile cost cap would be exceeded")
    return {"reserved_usd": float(cost), "cap_usd": float(cap), "input_token_bound": bound,
            "max_output_tokens": max_tokens}

def _base(engine, tenant, overrides):
    p = engine.get("provider")
    base = ENDPOINTS.get(p) or engine.get("base_url", "")
    if overrides and provider(engine) in overrides:
        base = overrides[provider(engine)]
        parsed = urllib.parse.urlsplit(base)
        if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "::1"}:
            raise ValueError("recording test endpoint must be loopback")
    else:
        parsed = urllib.parse.urlsplit(base)
        if parsed.scheme != "https" or parsed.hostname not in HOSTS | set(tenant.get("allowed_provider_hosts", [])):
            raise ValueError("provider endpoint not reviewed")
        addresses = socket.getaddrinfo(parsed.hostname, parsed.port or 443, type=socket.SOCK_STREAM)
        if not addresses or any(not ipaddress.ip_address(x[4][0]).is_global for x in addresses):
            raise ValueError("provider address is not public")
    if parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError("invalid provider endpoint")
    return base.rstrip("/")

def complete(engine, tenant, route, overrides=None, timeout=60):
    key = credential(engine, tenant)
    base = _base(engine, tenant, overrides)
    messages = route["messages"]
    if engine.get("provider") == "anthropic":
        system = "\n".join(m["content"] for m in messages if m["role"] == "system")
        body = {"model": engine["model"], "messages": [m for m in messages if m["role"] != "system"],
                "system": system, "max_tokens": route["max_tokens"]}
        headers = {"x-api-key": key, "anthropic-version": "2023-06-01"}
        url = base + "/messages"
    elif engine.get("provider") in {"openai", "openai-compatible"}:
        body = {"model": engine["model"], "messages": messages, "stream": False,
                "max_tokens": route["max_tokens"], "temperature": route["temperature"]}
        headers = {"Authorization": "Bearer " + key}
        url = base + "/chat/completions"
    else:
        raise ValueError("provider adapter not supported")
    request = urllib.request.Request(url, json.dumps(body, ensure_ascii=False).encode("utf-8"),
                                     {**headers, "Content-Type": "application/json"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=timeout) as response:
        raw = response.read((1 << 20) + 1)
    if len(raw) > 1 << 20:
        raise ValueError("hosted response exceeds limit")
    obj = json.loads(raw)
    if engine.get("provider") == "anthropic":
        text = "".join(c.get("text", "") for c in obj.get("content", []) if c.get("type") == "text")
        u = obj.get("usage", {}); usage = {"prompt_tokens": u.get("input_tokens"), "completion_tokens": u.get("output_tokens")}
    else:
        text = obj["choices"][0]["message"]["content"]; usage = obj.get("usage", {})
    if not isinstance(text, str) or not text:
        raise ValueError("hosted response has no text")
    counts = {k: v for k, v in usage.items() if k in {"prompt_tokens", "completion_tokens"} and type(v) is int and v >= 0}
    counts["total_tokens"] = sum(counts.values())
    return text, counts
