"""Loopback OpenAI-compatible gateway. It dispatches only qualified local Ollama engines."""
from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import re
import secrets
import select
import socket
import subprocess
import sys
import threading
import time
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CODE = Path(__file__).resolve().parent
sys.path.insert(0, str(CODE))
from tempo_home import DATA, LEDGER_DIR, T
from tempo_receipts import append_receipt

MAX_BODY = 1 << 20
PREAUTH_READ_TIMEOUT = 10
MAX_HANDLER_THREADS = 32
MAX_MESSAGES = 100
MAX_MESSAGE_CHARS = 32_000
MAX_COMPLETION_TOKENS = 8192
ALIASES = {"tempo/auto": None, "tempo/fast": "inference", "tempo/heavy": "thinking",
           "tempo/research": "research", "tempo/writer": "writing-code"}
ROLES = {"thinking", "research", "inference", "writing-code"}
TASK_RE = re.compile(r"^[a-z0-9][a-z0-9-]{1,39}$")


def _canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _sha(value):
    return hashlib.sha256(value).hexdigest()


def authorized(header, token):
    if not isinstance(header, str) or not header.startswith("Bearer ") or not isinstance(token, str) or not token:
        return False
    try:
        return hmac.compare_digest(header[7:].encode("ascii"), token.encode("utf-8"))
    except UnicodeEncodeError:
        return False


def _gateway_proof(token, nonce, port):
    message = f"PerennosTempoGateway-v1\n{port}\n{nonce}".encode("ascii")
    return hmac.new(token.encode("utf-8"), message, hashlib.sha256).hexdigest()


MAX_TOOLS = 16


def _parse_tools(body):
    """Shape-check OpenAI-style tools/tool_choice. Only the tool NAMES survive: the client's descriptions and
    parameter schemas are discarded, so nothing the client wrote reaches the model or a handler. The caller's
    permissions are decided in tool_policy, not here."""
    if "tools" not in body and "tool_choice" not in body:
        return None, None
    tools = body.get("tools")
    if not isinstance(tools, list) or not 1 <= len(tools) <= MAX_TOOLS:
        raise ValueError("tools must be a list of 1 to 16 function tools")
    names = []
    for item in tools:
        fn = item.get("function") if isinstance(item, dict) else None
        if not isinstance(item, dict) or item.get("type") != "function" or not isinstance(fn, dict) or not isinstance(fn.get("name"), str):
            raise ValueError("each tool must be a function tool with a text name")
        names.append(fn["name"])
    choice = body.get("tool_choice", "auto")
    if isinstance(choice, str) and choice in ("auto", "none", "required"):
        parsed = {"mode": choice, "name": None}
    elif (isinstance(choice, dict) and choice.get("type") == "function" and isinstance(choice.get("function"), dict)
          and isinstance(choice["function"].get("name"), str)):
        parsed = {"mode": "named", "name": choice["function"]["name"]}
    else:
        raise ValueError("tool_choice is not supported")
    if body.get("stream") is True:
        raise ValueError("streaming is not supported with tools")
    return names, parsed


def validate_chat_request(body):
    if not isinstance(body, dict):
        raise ValueError("request must be a JSON object")
    if body.get("stream") not in (None, False, True):
        raise ValueError("stream must be a boolean")
    tool_names, tool_choice = _parse_tools(body)
    messages = body.get("messages")
    if not isinstance(messages, list) or not messages or len(messages) > MAX_MESSAGES:
        raise ValueError("messages must be a non-empty list of at most 100 items")
    clean = []
    for item in messages:
        if not isinstance(item, dict) or item.get("role") not in {"system", "user", "assistant"}:
            raise ValueError("each message must have a supported role and text content")
        content = item.get("content")
        if not isinstance(content, str) or len(content) > MAX_MESSAGE_CHARS:
            raise ValueError("each message content must be text of at most 32000 characters")
        clean.append({"role": item["role"], "content": content})
    model = body.get("model", "tempo/auto")
    if not isinstance(model, str) or not re.fullmatch(r"tempo/[a-z][a-z0-9-]{0,39}", model):
        raise ValueError("model must be a Perennos Tempo route or profile")
    metadata = body.get("metadata") or {}
    if not isinstance(metadata, dict):
        raise ValueError("metadata must be an object")
    if metadata.get('implicit_action') not in (None, 'regenerate'):
        raise ValueError('unsupported implicit action')
    if metadata.get('workflow') is not None and (not isinstance(metadata['workflow'], str) or len(metadata['workflow']) > 80):
        raise ValueError('metadata.workflow must be a short workflow name')
    task_class = metadata.get("task_class")
    if task_class is not None and (not isinstance(task_class, str) or not TASK_RE.fullmatch(task_class.lower())):
        raise ValueError("metadata.task_class must be a kebab-case class")
    privacy_class = metadata.get("privacy_class", "private")
    if not isinstance(privacy_class, str) or not TASK_RE.fullmatch(privacy_class.lower()):
        raise ValueError("metadata.privacy_class must be a kebab-case class")
    conversation_id = metadata.get("conversation_id", body.get("conversation_id"))
    if conversation_id is not None and (not isinstance(conversation_id, str) or not conversation_id.strip() or
                                        len(conversation_id) > 128 or any(ord(c) < 32 for c in conversation_id)):
        raise ValueError("conversation_id must be 1 to 128 printable characters")
    max_tokens = body.get("max_tokens", 1024)
    if isinstance(max_tokens, bool) or not isinstance(max_tokens, int) or not 1 <= max_tokens <= MAX_COMPLETION_TOKENS:
        raise ValueError("max_tokens must be an integer from 1 to 8192")
    temperature = body.get("temperature", 0.2)
    if isinstance(temperature, bool) or not isinstance(temperature, (int, float)) or not 0 <= temperature <= 2:
        raise ValueError("temperature must be between 0 and 2")
    stream_options = body.get("stream_options") or {}
    if not isinstance(stream_options, dict):
        raise ValueError("stream_options must be an object")
    request = {"messages": clean, "model": model, "metadata": metadata, "task_class": task_class,
               "privacy_class": privacy_class.lower(), "conversation_id": conversation_id,
               "max_tokens": max_tokens, "temperature": float(temperature), "stream": bool(body.get("stream", False)),
               "include_usage": bool(stream_options.get("include_usage", False))}
    if tool_names is not None:
        request["tools"] = tool_names
        request["tool_choice"] = tool_choice
    return request


def classify_request(body, allowed_classes=None):
    request = validate_chat_request(body)
    text = "\n".join(m["content"] for m in request["messages"] if m["role"] in ("system", "user"))[-8000:].lower()
    alias_role = ALIASES.get(request["model"])
    if alias_role:
        role = alias_role
    else:
        explicit_role = request["metadata"].get("role")
        if explicit_role in ROLES:
            role = explicit_role
        elif any(w in text for w in ("write code", "coding", "bug", "python", "javascript", "implement", "refactor")):
            role = "writing-code"
        elif any(w in text for w in ("research", "sources", "cite", "find evidence", "look up")):
            role = "research"
        elif any(w in text for w in ("plan", "compare", "reason", "decide", "tradeoff", "strategy")):
            role = "thinking"
        else:
            role = "inference"
    task_class = request["task_class"]
    if not task_class:
        if role == "writing-code":
            task_class = "code"
        elif role == "research":
            task_class = "research"
        elif any(w in text for w in ("summarize", "summary", "recap")):
            task_class = "summary"
        elif role == "thinking":
            task_class = "analysis"
        else:
            task_class = "docs"
        if allowed_classes is not None and task_class not in allowed_classes:
            task_class = "docs" if "docs" in allowed_classes else sorted(allowed_classes)[0]
    return {**request, "role": role, "task_class": task_class,
            "privacy_class": request["privacy_class"] or "private", "conversation_id": request["conversation_id"]}


def choose_engine(role, engines, qualified_ids, ratings=None, preferred_ids=None):
    if role not in ROLES:
        raise ValueError("unknown role")
    local = [e for e in engines if e.get("provider") == "ollama" and e.get("locality") == "local" and
             role in e.get("roles", []) and (e.get("qualified") is True or e.get("id") in qualified_ids)]
    if not local:
        raise ValueError("no qualified local engine is registered for this role")
    grouped = {}
    for engine in local:
        tier = str(engine.get("policy_tier") or "local-default")
        grouped.setdefault(tier, []).append(engine)
    tiers = sorted(grouped.items(), key=lambda item: (min(int(e.get("policy_order", 0)) for e in item[1]), item[0]))
    tier_name, candidates = tiers[0]
    preferred_ids = [preferred_ids] if isinstance(preferred_ids,str) else (preferred_ids or [])
    candidates.sort(key=lambda e: (preferred_ids.index(e['id']) if e['id'] in preferred_ids else len(preferred_ids),
                                   int(e.get("policy_order", 0)), e.get("id", "")))
    selected = candidates[0]
    reason = "first eligible engine in deterministic local policy order"
    if isinstance(ratings, dict) and ratings:
        scored = [(float(ratings[e["id"]]), i, e) for i, e in enumerate(candidates) if e["id"] in ratings]
        if scored:
            _, _, selected = max(scored, key=lambda item: (item[0], -item[1]))
            reason = "highest reviewed Tempo rating inside the highest eligible local policy tier"
    return {"engine": selected, "policy_tier": tier_name, "deterministic_rank": candidates.index(selected),
            "candidate_ids": [e["id"] for e in candidates], "reason": reason}


def qualified_engine_ids(data_dir):
    path = Path(data_dir) / "setup-receipt.json"
    if not path.exists():
        return set()
    try:
        receipt = json.loads(path.read_text(encoding="utf-8"))
        name = (receipt.get("steps", {}).get("qualification") or {}).get("qualified")
        if not name:
            return set()
        engines = json.loads((Path(data_dir) / "engines.json").read_text(encoding="utf-8")).get("engines", [])
        return {e["id"] for e in engines if e.get("model") == name or e.get("id") == name}
    except (OSError, ValueError, KeyError, TypeError):
        return set()


def _ollama_chunks(url, model, messages, temperature, max_tokens, timeout, cpu=False, family=None, context_tokens=None, *, resident_policy=None, backend_witness=None, witness_provider=None):
    from tempo_runtime import request,policy,visible_answer
    resident=resident_policy is not None or backend_witness is not None or witness_provider is not None
    extra={}
    if resident:
        if not callable(witness_provider):raise ValueError('trusted resident witness provider is required')
        extra=dict(resident_policy=resident_policy,backend_witness=backend_witness)
    endpoint,payload=request(model,messages,temperature,max_tokens,cpu=cpu,family=family,**extra)
    if resident and context_tokens is not None and context_tokens!=payload['options']['num_ctx']:
        raise ValueError('resident context override is forbidden')
    if resident and backend_witness['valid_until']-time.time()<float(timeout):
        raise ValueError('resident resource window is shorter than request timeout')
    if context_tokens:payload['options']['num_ctx']=context_tokens
    req = urllib.request.Request(url.rstrip("/") + endpoint, _canonical(payload), {"Content-Type": "application/json"})
    deadline=time.monotonic()+float(timeout)
    from tempo_local_http import open_local
    response = open_local(req, timeout=timeout)
    def iterator():
        buffered=[]
        qwen=policy(model,family).get('think') is False
        last=None;has_visible_text=False
        try:
            stream=iter(response)
            while True:
                remaining=deadline-time.monotonic()
                if remaining<=0:raise TimeoutError('local engine request deadline exceeded')
                sock=getattr(getattr(getattr(response,'fp',None),'raw',None),'_sock',None)
                if sock is not None:sock.settimeout(remaining)
                try:raw=next(stream)
                except StopIteration:break
                if not raw.strip():
                    continue
                item = json.loads(raw)
                if not isinstance(item,dict) or ('done' in item and type(item['done']) is not bool):
                    raise RuntimeError('invalid local engine terminal flag')
                if time.monotonic()>=deadline:raise TimeoutError('local engine request deadline exceeded')
                message = item.get("message") or {}
                chunk={"text": message.get("content") or item.get('response') or "", "done": bool(item.get("done")),
                       "prompt_tokens": item.get("prompt_eval_count"), "completion_tokens": item.get("eval_count"),
                       "total_duration": item.get("total_duration"), "eval_duration": item.get("eval_duration"),
                       "finish_reason": item.get("done_reason")}
                if resident and chunk['done']:
                    from tempo_runtime import confirm_resident_backend
                    confirm_resident_backend(resident_policy,backend_witness,witness_provider(),model,family)
                    if time.monotonic()>=deadline:raise TimeoutError('resident completion witness exceeded request deadline')
                last=chunk
                if not qwen:
                    has_visible_text=has_visible_text or bool(chunk['text'].strip())
                    yield chunk
                    continue
                # Hold Qwen deltas until completion: a lone closing tag may arrive
                # after arbitrary reasoning text, so no prefix is safe to stream.
                buffered.append(chunk['text']);last=chunk
                if not chunk['done']:
                    # Internal progress lets the caller enforce its deadline and
                    # notice disconnects without emitting reasoning in SSE.
                    yield {**chunk,'text':'','finish_reason':None}
            if not last or not last['done']:raise RuntimeError('incomplete local engine response')
            if qwen:
                text=visible_answer(''.join(buffered),model,family,require_envelope=policy(model,family)['wire']=='qwen3-empty-think')
                yield {**last,'text':text}
            elif not has_visible_text:raise RuntimeError('empty final answer; no answer returned')
        finally:
            response.close()
    return iterator()


def _client_disconnected(sock):
    try:
        readable, _, _ = select.select([sock], [], [], 0)
        if readable:
            return sock.recv(1, socket.MSG_PEEK) == b""
    except (OSError, ValueError):
        return True
    return False


def _atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(path.suffix + ".tmp")
    temp.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")), encoding="utf-8")
    os.replace(temp, path)


class TempoGateway:
    def __init__(self, data_dir, lease_script, lease_need_mib=4096, ollama_url=None, lease_ttl=20, timeout=600,
                 device="gpu", cpu_booking=None, cpu_until=0, guard_mode='auto', *, resident_policy=None, witness_provider=None):
        self.data_dir = Path(data_dir).resolve()
        self.ledger = self.data_dir / "ledger"
        self.lease_script = Path(lease_script).resolve() if lease_script else None
        self.lease_need_mib = int(lease_need_mib)
        from tempo_standalone import is_standalone
        try:self.standalone=is_standalone(self.data_dir,lease_script,cpu_booking,cpu_until) and guard_mode!='fleet'
        except RuntimeError as exc:raise ValueError(str(exc)) from exc
        import copy
        self.resident_policy=copy.deepcopy(resident_policy)
        self.witness_provider=witness_provider
        if resident_policy is not None or witness_provider is not None:
            if self.standalone or device!='cpu' or type(resident_policy) is not dict or not callable(witness_provider):
                raise ValueError('owner resident backend requires fleet CPU mode and a trusted witness provider')
        self.requested_device=device
        if device=='auto':device='cpu' if cpu_booking or self.standalone else 'gpu'
        if device not in ("cpu", "gpu"):
            raise ValueError("device must be cpu or gpu")
        self.device, self.cpu_booking, self.cpu_until = device, cpu_booking, float(cpu_until)
        if device == "cpu" and not self.standalone:
            from tempo_cpu import check_window
            check_window(cpu_booking, cpu_until)
        if not self.standalone and device == "gpu" and (not self.lease_script or not self.lease_script.is_file()):
            raise ValueError("GPU lease script is required before the gateway can start")
        if self.lease_need_mib < 1:
            raise ValueError("a positive GPU memory request is required before the gateway can start")
        self.ollama_url = (ollama_url or T["ollama_url"]).rstrip("/")
        if device == "cpu":
            from tempo_cpu import require_loopback
            require_loopback(self.ollama_url)
            os.environ.update(TEMPO_HEAD_DEVICE="cpu", TEMPO_HEAD_THREADS="2", CUDA_VISIBLE_DEVICES="")
        self.lease_ttl = max(1, int(lease_ttl))
        self.timeout = max(5, int(timeout))
        self.active_guard=None;self.guard_context=None;self.active_holder=None
        if self.standalone:os.environ.update(TEMPO_HEAD_DEVICE='cpu',TEMPO_HEAD_THREADS='2')

    def _lease(self, holder, engine=None):
        if self.standalone:
            from tempo_standalone import StandaloneGuard,decision_head_guard
            context=(StandaloneGuard(engine['model'],self.ollama_url,self.requested_device,min(self.timeout,600),engine.get('revision'))
                     if engine else decision_head_guard(min(self.timeout,600)))
            guard=context.__enter__()
            self.guard_context,self.active_guard,self.active_holder=context,guard,holder
            self.device=guard.device
            return
        if self.device == "cpu":
            from tempo_cpu import check_window
            check_window(self.cpu_booking, self.cpu_until, self.timeout)
            return
        if not self.lease_script or not self.lease_script.is_file():
            raise RuntimeError("GPU lease script is not configured; refusing model inference")
        result = subprocess.run([sys.executable, str(self.lease_script), "acquire", "--holder", holder,
                                 "--portfolio", "perennos", "--need-mib", str(self.lease_need_mib),
                                  "--ttl-min", str(self.lease_ttl), "--wait-min", "0", "--max-util", "10"],
                                capture_output=True, text=True, timeout=30)
        if result.returncode != 0:
            raise RuntimeError("GPU lease was not granted")

    def _release(self, holder):
        if self.standalone:
            if self.active_holder!=holder:raise RuntimeError('standalone lock ownership changed')
            context=self.guard_context
            self.guard_context=self.active_guard=self.active_holder=None
            context.__exit__(*sys.exc_info())
            return
        if self.device == "cpu":
            return
        if self.lease_script:
            released = subprocess.run([sys.executable, str(self.lease_script), "release", "--holder", holder],
                                      capture_output=True, text=True, timeout=30)
            if released.returncode != 0:
                raise RuntimeError("GPU lease release was not confirmed")

    def _rating_plan(self, route, candidates):
        import tempo_ratings
        tiers_by_order = {}
        for engine in candidates:
            tiers_by_order.setdefault(int(engine.get("policy_order", 0)), []).append(
                {"id": engine["id"], "revision": engine.get("revision") or "as-served"})
        tiers = [tiers_by_order[k] for k in sorted(tiers_by_order)]
        return tempo_ratings.plan({"task_class": route["task_class"], "role": route["role"],
                                   "privacy_class": route["privacy_class"], "summary": "local gateway job",
                                   "model":route['model'], "workflow":route.get('metadata',{}).get('workflow'),
                                   "metadata":route.get('metadata',{}), "tiers": tiers}, lease_held=True)

    def _transition(self, route, request_hash):
        from tempo_verdicts import ConversationTracker, infer_transition, finalize_due
        finalize_due(self.ledger / "outcomes.jsonl")
        if not route["conversation_id"]:
            return None, None
        key = _sha((T["tenant"] + ":" + route["conversation_id"]).encode())
        tracker = ConversationTracker(self.ledger / "conversations", key)
        tracker.acquire()
        try:
            prior = tracker.load()
            feedback_hash = _sha(_canonical([m for m in route['messages'] if m['role'] != 'system']))
            outcome = infer_transition(prior, route["messages"], request_hash, feedback_sha256=feedback_hash,
                                       implicit_action=route.get('metadata', {}).get('implicit_action'))
            if outcome:
                import tempo_ratings
                tempo_ratings.record(outcome)
            return tracker, {"conversation_hash": key, "feedback_sha256": feedback_hash}
        except Exception:
            tracker.release()
            raise

    def run(self, route, on_stream=None, on_start=None, client_socket=None):
        job_id, attempt_id = route.get("_job_id") or "chatcmpl-" + uuid.uuid4().hex, uuid.uuid4().hex
        profile_context = route.get("_profile_context", {})
        profile_context = dict(profile_context, execution_device=self.device,
                               cpu_booking=self.cpu_booking if self.device == "cpu" else None,
                               num_gpu=0 if self.device == "cpu" else "runtime-default")
        request_hash = _sha(_canonical({"messages": route["messages"], "task_class": route["task_class"],
                                        "role": route["role"], "privacy_class": route["privacy_class"]}))
        engines = json.loads((self.data_dir / "engines.json").read_text(encoding="utf-8")).get("engines", [])
        qualified = qualified_engine_ids(self.data_dir)
        eligible = [e for e in engines if e.get("provider") == "ollama" and e.get("locality") == "local" and
                    route["role"] in e.get("roles", []) and (e.get("qualified") is True or e.get("id") in qualified)]
        profile_policy=route.get('_profile_policy',{})
        if profile_policy and not profile_policy.get('uncensored'):
            eligible=[e for e in eligible if not e.get('uncensored') and not any(x in e.get('model','').lower() for x in ('uncensored','abliterat','heretic','dolphin'))]
        preferred=profile_policy.get('preferred',{}).get(route['role'],[])
        selection = choose_engine(route["role"], eligible, qualified, ratings=None, preferred_ids=preferred)
        tracker, conversation = self._transition(route, request_hash)
        holder = f"tempo-gateway-{os.getpid()}-{attempt_id[:8]}"
        started = time.monotonic()
        lease_acquired = False
        plan = None
        try:
            if self.standalone:self._lease(holder,selection['engine'])
            else:self._lease(holder)
            lease_acquired = True
            plan = self._rating_plan(route, eligible)
            ratings = plan.get("ratings") if plan.get("status") == "rated" else None
            selection = choose_engine(route["role"], eligible, qualified, ratings, preferred_ids=preferred)
            engine = selection["engine"]
            if self.standalone:
                if engine['model']!=self.active_guard.model:
                    self._release(holder);lease_acquired=False
                    self._lease(holder,engine);lease_acquired=True
                profile_context.update(execution_device=self.device,cpu_booking=None,
                    num_gpu=0 if self.device=='cpu' else 'runtime-default',standalone_guard=self.active_guard.receipt)
            from tempo_runtime import policy
            profile_context['runtime_policy']=policy(engine['model'],engine.get('family'))
            if self.device == "cpu":
                from tempo_local_http import open_local
                tags = json.load(open_local(self.ollama_url + '/api/tags', timeout=10))
                live = next((e for e in tags.get('models', []) if e.get('name') == engine['model']), None)
                if not live or live.get('digest') != engine.get('revision'):
                    raise RuntimeError('CPU engine digest changed; qualification is required')
            if self.resident_policy is not None:
                profile_context.update(processing_host_id=self.resident_policy['host_id'],
                    processing_guest_id=self.resident_policy['guest_id'],owner_resident_backend=True)
            started_receipt = append_receipt(self.ledger / "receipts.jsonl", {
                "job_id": job_id, "attempt_id": attempt_id, "status": "started", "task_class": route["task_class"],
                "role": route["role"], "privacy_class": route["privacy_class"], "engine_id": engine["id"],
                "engine_revision": engine.get("revision"), "policy_tier": selection["policy_tier"],
                "deterministic_rank": selection["deterministic_rank"],
                "decision_id": (plan.get("receipt") or {}).get("decision_id"), "request_sha256": request_hash,
                **profile_context})
            if on_start:
                on_start(started_receipt, job_id, engine["model"])
            if self.standalone:self.active_guard.dispatched()
            extra={'context_tokens':self.active_guard.context_tokens} if self.standalone else {}
            remaining=self.timeout-(time.monotonic()-started)
            if self.standalone:remaining=min(remaining,self.active_guard.deadline-time.monotonic())
            if remaining<=0:raise TimeoutError('gateway request deadline exceeded before generation')
            if self.resident_policy is not None:
                from tempo_runtime import begin_resident_request
                witness=begin_resident_request(self.resident_policy,self.witness_provider,engine['model'],
                    engine.get('family'),engine.get('revision'),self.cpu_booking,self.cpu_until,remaining)
                remaining=self.timeout-(time.monotonic()-started)
                if self.standalone:remaining=min(remaining,self.active_guard.deadline-time.monotonic())
                if remaining<=0:raise TimeoutError('gateway request deadline exceeded before generation')
                extra.update(resident_policy=self.resident_policy,backend_witness=witness,witness_provider=self.witness_provider)
            chunks = _ollama_chunks(self.ollama_url, engine["model"], route["messages"], route["temperature"],
                                    route["max_tokens"], remaining, cpu=self.device == "cpu",family=engine.get('family'),**extra)
            text, prompt_tokens, completion_tokens, finish_reason = [], 0, 0, "stop"
            for chunk in chunks:
                if self.standalone:self.active_guard.check()
                if self.device == "cpu" and not self.standalone:
                    from tempo_cpu import check_window
                    check_window(self.cpu_booking, self.cpu_until)
                if client_socket and _client_disconnected(client_socket):
                    raise ConnectionAbortedError("client disconnected")
                if chunk["text"]:
                    text.append(chunk["text"])
                    if on_stream:
                        on_stream(chunk["text"], job_id, engine["model"])
                if isinstance(chunk.get("prompt_tokens"), int):
                    prompt_tokens = chunk["prompt_tokens"]
                if isinstance(chunk.get("completion_tokens"), int):
                    completion_tokens = chunk["completion_tokens"]
                if chunk.get("finish_reason"):
                    finish_reason = str(chunk["finish_reason"])
            output_text = "".join(text)
            if not output_text:
                raise RuntimeError("local engine returned an empty answer")
            result = {"id": job_id, "object": "chat.completion", "created": int(time.time()),
                      "model": engine["model"], "choices": [{"index": 0,
                      "message": {"role": "assistant", "content": output_text}, "finish_reason": finish_reason}],
                      "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens,
                                "total_tokens": prompt_tokens + completion_tokens}}
            ended = time.monotonic()
            output_hash = _sha(output_text.encode("utf-8"))
            complete = append_receipt(self.ledger / "receipts.jsonl", {
                "job_id": job_id, "attempt_id": attempt_id, "status": "completed", "engine_id": engine["id"],
                "engine_revision": engine.get("revision"), "request_sha256": request_hash,
                "response_sha256": output_hash, "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens, "elapsed_ms": round((ended - started) * 1000, 2),
                "decision_id": (plan.get("receipt") or {}).get("decision_id"), **profile_context})
            attempt = {"kind": "tempo.attempt.v1", "job_id": job_id, "attempt_id": attempt_id,
                       "at": started_receipt["recorded_at"], "completed_at": complete["recorded_at"],
                       "task_class": route["task_class"], "role": route["role"], "privacy_class": route["privacy_class"],
                       "engine_id": engine["id"], "engine_revision": engine.get("revision"),
                       "policy_tier": selection["policy_tier"], "deterministic_rank": selection["deterministic_rank"],
                       "decision_id": (plan.get("receipt") or {}).get("decision_id"), "request_sha256": request_hash,
                       "response_sha256": output_hash, "prompt_tokens": prompt_tokens,
                       "completion_tokens": completion_tokens, "conversation_hash": (conversation or {}).get("conversation_hash"), **profile_context}
            from tempo_store import append_jsonl
            append_jsonl(self.ledger / "attempts.jsonl", attempt)
            if tracker:
                tracker.save({"attempt_id": attempt_id, "request_sha256": request_hash, "response_sha256": output_hash,
                              "feedback_sha256": conversation['feedback_sha256'],
                              "completed_at": complete["recorded_at"], "task_class": route["task_class"],
                              "role": route["role"], "privacy_class": route["privacy_class"], "engine_id": engine["id"],
                              "revision": engine.get("revision"), "policy_tier": selection["policy_tier"],
                              "deterministic_rank": selection["deterministic_rank"]})
            result["_tempo"] = {"receipt_id": started_receipt["receipt_id"],
                                "completion_receipt_id": complete["receipt_id"], "job_id": job_id}
            return result
        except ConnectionAbortedError:
            try:
                append_receipt(self.ledger / "receipts.jsonl", {"job_id": job_id, "attempt_id": attempt_id,
                    "status": "cancelled", "task_class": route["task_class"], "role": route["role"],
                    "privacy_class": route["privacy_class"], "elapsed_ms": round((time.monotonic() - started) * 1000, 2), **profile_context})
            except Exception:
                pass
            raise
        except Exception as exc:
            try:
                append_receipt(self.ledger / "receipts.jsonl", {"job_id": job_id, "attempt_id": attempt_id,
                    "status": "failed", "error_code": type(exc).__name__, "task_class": route["task_class"],
                    "role": route["role"], "privacy_class": route["privacy_class"],
                    "elapsed_ms": round((time.monotonic() - started) * 1000, 2), **profile_context})
            except Exception:
                pass
            raise
        finally:
            if lease_acquired:
                self._release(holder)
            if tracker:
                tracker.release()


def _read_jsonl(path):
    path = Path(path)
    if not path.exists():
        return []
    rows = [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines() if line.strip()]
    if any(not isinstance(row, dict) for row in rows):
        raise ValueError('invalid attempts ledger')
    return rows


def _restrict_token_file(path):
    if os.name != 'nt':
        os.chmod(path, 0o600)
        return
    # Build the DACL from numeric identity objects. Resolving retired account
    # names through icacls can fail even when their SID grants can be removed.
    env = dict(os.environ, TEMPO_TOKEN_ACL_PATH=str(path))
    windows = Path(os.environ['SystemRoot'])
    powershell = windows/'System32/WindowsPowerShell/v1.0/powershell.exe'
    env['PSModulePath'] = str(powershell.parent/'Modules')
    query = "$ErrorActionPreference='Stop'; $s=[System.Security.Principal.WindowsIdentity]::GetCurrent().User; $a=New-Object System.Security.AccessControl.FileSecurity; $a.SetSecurityDescriptorSddlForm(('D:P(A;;FA;;;'+$s.Value+')'),[System.Security.AccessControl.AccessControlSections]::Access); [System.IO.File]::SetAccessControl($env:TEMPO_TOKEN_ACL_PATH,$a); $v=Get-Acl -LiteralPath $env:TEMPO_TOKEN_ACL_PATH; if(!$v.AreAccessRulesProtected -or @($v.Access | Where-Object { $_.AccessControlType -eq 'Allow' -and $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value -ne $s.Value }).Count -ne 0){throw 'DACL verification failed'}"
    try:
        subprocess.run([str(powershell), '-NoProfile', '-NonInteractive', '-Command', query],
                                env=env, capture_output=True, text=True, timeout=15, check=True)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError):
        raise RuntimeError('gateway token NTFS access could not be restricted; refusing startup') from None


def _token_from_file(path):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        _restrict_token_file(path)
        token = path.read_text(encoding="utf-8").strip()
        if len(token) < 32:
            raise RuntimeError("gateway token file is invalid; replace it through local setup")
        return token
    token = secrets.token_urlsafe(48)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(fd)
    _restrict_token_file(path)  # Empty file until its permissions are restricted.
    with path.open("w", encoding="utf-8") as stream:
        stream.write(token + "\n")
    return token


def make_handler(gateway, token, tools=None):
    class Handler(BaseHTTPRequestHandler):
        server_version = "PerennosTempoGateway/1"
        protocol_version = "HTTP/1.1"

        def setup(self):
            super().setup()
            self.connection.settimeout(PREAUTH_READ_TIMEOUT)

        def log_message(self, fmt, *args):
            return

        def _json(self, status, value, headers=None, *, close=True):
            body = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Connection", "close" if close else "keep-alive")
            for key, value in (headers or {}).items():
                self.send_header(key, str(value))
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = close

        def do_GET(self):
            if self.path == "/health":
                nonce = self.headers.get("X-Tempo-Gateway-Nonce")
                if nonce is not None:
                    if re.fullmatch(r"[A-Za-z0-9_-]{43}", nonce):
                        port = int(self.server.server_address[1])
                        proof = _gateway_proof(token, nonce, port)
                        return self._json(200, {"status": "ok", "mode": "profile-gated"},
                                          {"X-Tempo-Gateway-Proof": proof}, close=False)
                    return self._json(400, {"error": {"message": "invalid challenge", "type": "invalid_request_error"}})
                return self._json(200, {"status": "ok", "mode": "profile-gated"})
            if self.path == "/v1/tools":
                return self._tools_listing()
            if self.path != "/v1/models":
                return self._json(404, {"error": {"message": "not found", "type": "not_found_error"}})
            if not authorized(self.headers.get("Authorization"), token):
                return self._json(401, {"error": {"message": "unauthorized", "type": "authentication_error"}})
            self.connection.settimeout(None)
            models = gateway.models() if hasattr(gateway, "models") else []
            models += [{"id": name, "object": "model", "owned_by": "perennos-tempo"} for name in ALIASES]
            return self._json(200, {"object": "list", "data": models})

        def do_POST(self):
            if self.path == "/v1/feedback":
                return self._feedback()
            if self.path != "/v1/chat/completions":
                return self._json(404, {"error": {"message": "not found", "type": "not_found_error"}})
            header = self.headers.get("Authorization")
            main_ok = authorized(header, token)
            if not main_ok and not (tools is not None and tools.authenticates(header)):
                return self._json(401, {"error": {"message": "unauthorized", "type": "authentication_error"}})
            try:
                length = int(self.headers.get("Content-Length", "-1"))
                if length < 0 or length > MAX_BODY:
                    return self._json(413, {"error": {"message": "request body exceeds limit", "type": "invalid_request_error"}})
                body = json.loads(self.rfile.read(length))
                self.connection.settimeout(None)
                cfg = json.loads((gateway.data_dir / "config.json").read_text(encoding="utf-8"))
                route = classify_request(body, allowed_classes=set(cfg.get("task_classes", [])) or None)
            except (ValueError, TypeError, OSError, json.JSONDecodeError, RecursionError):
                return self._json(400, {"error": {"message": "invalid request", "type": "invalid_request_error"}})
            if route.get("tools") is not None:
                return self._chat_with_tools(route, header)
            if not main_ok:  # a policy caller token only opens tool requests
                return self._json(401, {"error": {"message": "unauthorized", "type": "authentication_error"}})
            stream = route["stream"]
            headers_sent = False
            def start(receipt, job_id, engine_model):
                nonlocal headers_sent
                if not stream:
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream; charset=utf-8")
                self.send_header("Cache-Control", "no-cache, no-store")
                self.send_header("X-Content-Type-Options", "nosniff")
                self.send_header("X-Tempo-Receipt-Id", receipt["receipt_id"])
                self.send_header("X-Tempo-Job-Id", job_id)
                self.send_header("X-Tempo-Profile", receipt.get("profile", "private"))
                self.send_header("X-Tempo-Review-Required", str(receipt.get("requires_review", True)).lower())
                self.send_header("Connection", "close")
                self.end_headers()
                headers_sent = True
                self._event({"id": job_id, "object": "chat.completion.chunk", "created": int(time.time()),
                             "model": engine_model, "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]})
            def chunk(content, job_id, engine_model):
                if stream:
                    self._event({"id": job_id, "object": "chat.completion.chunk", "created": int(time.time()),
                                 "model": engine_model, "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": None}]})
            try:
                result = gateway.run(route, on_stream=chunk, on_start=start, client_socket=self.connection if not stream else None)
                extra = result.pop("_tempo")
                if stream:
                    self._event({"id": extra["job_id"], "object": "chat.completion.chunk", "created": int(time.time()),
                                 "model": result["model"], "choices": [{"index": 0, "delta": {}, "finish_reason": result['choices'][0]['finish_reason']}]})
                    if route["include_usage"]:
                        self._event({"id": extra["job_id"], "object": "chat.completion.chunk", "created": int(time.time()),
                                     "model": result["model"], "choices": [], "usage": result["usage"]})
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                    self.close_connection = True
                    return
                return self._json(200, result, {"X-Tempo-Receipt-Id": extra["receipt_id"],
                                                "X-Tempo-Completion-Receipt-Id": extra["completion_receipt_id"],
                                                "X-Tempo-Job-Id": extra["job_id"],
                                                "X-Tempo-Profile": result.get("tempo", {}).get("profile", "private"),
                                                "X-Tempo-Review-Required": str(result.get("tempo", {}).get("review_before_use", True)).lower()})
            except (ConnectionAbortedError, BrokenPipeError, ConnectionResetError):
                self.close_connection = True
                return
            except Exception as exc:
                if headers_sent:
                    try:
                        self.wfile.write(b"data: [DONE]\n\n")
                        self.wfile.flush()
                    except OSError:
                        pass
                    self.close_connection = True
                    return
                status = 503 if "lease" in str(exc).lower() or "qualified" in str(exc).lower() else 502
                return self._json(status, {"error": {"message": "local route unavailable", "type": "server_error",
                                                    "code": type(exc).__name__}})

        def _tool_runtime(self):
            if tools is not None:
                return tools
            import tool_policy
            return tool_policy.ToolRuntime.unconfigured(tool_policy.ReceiptAudit(gateway.ledger / "receipts.jsonl"))

        def _tools_listing(self):
            header = self.headers.get("Authorization")
            try:
                status, payload = self._tool_runtime().describe(gateway, header, authorized(header, token))
            except Exception:
                status, payload = 403, {"error": {"message": "tool use denied", "type": "tool_denied", "code": "tool_denied"},
                                        "tempo_tool_denials": [{"tool": "*", "reason": "policy_missing"}]}
            return self._json(status, payload)

        def _chat_with_tools(self, route, header):
            try:
                status, payload = self._tool_runtime().handle(gateway, route, header)
            except (ConnectionAbortedError, BrokenPipeError, ConnectionResetError):
                self.close_connection = True
                return
            except ImportError:
                return self._json(403, {"error": {"message": "tool use denied", "type": "tool_denied", "code": "tool_denied"},
                                        "tempo_tool_denials": [{"tool": "*", "reason": "policy_missing"}]})
            except Exception as exc:
                status = 503 if "lease" in str(exc).lower() or "qualified" in str(exc).lower() else 502
                return self._json(status, {"error": {"message": "local route unavailable", "type": "server_error",
                                                    "code": type(exc).__name__}})
            extra = payload.pop("_tempo", None)
            headers = {}
            if extra and status == 200:
                headers = {"X-Tempo-Receipt-Id": extra["receipt_id"],
                           "X-Tempo-Completion-Receipt-Id": extra["completion_receipt_id"],
                           "X-Tempo-Job-Id": extra["job_id"],
                           "X-Tempo-Profile": payload.get("tempo", {}).get("profile", "private"),
                           "X-Tempo-Review-Required": str(payload.get("tempo", {}).get("review_before_use", True)).lower()}
            return self._json(status, payload, headers)

        def _feedback(self):
            if not authorized(self.headers.get("Authorization"), token):
                return self._json(401, {"error": {"message": "unauthorized", "type": "authentication_error"}})
            try:
                length = int(self.headers.get("Content-Length", "-1"))
                if length < 0 or length > 4096:
                    return self._json(413, {"error": {"message": "feedback body exceeds limit", "type": "invalid_request_error"}})
                body = json.loads(self.rfile.read(length))
                self.connection.settimeout(None)
                if not isinstance(body, dict) or not isinstance(body.get("attempt_id"), str):
                    raise ValueError("invalid feedback")
                verdict = body.get("verdict")
                attempt_id = body["attempt_id"]
                if not re.fullmatch(r"[a-f0-9]{32}", attempt_id):
                    raise ValueError("invalid feedback")
                matches = [row for row in _read_jsonl(gateway.ledger / "attempts.jsonl") if row.get("attempt_id") == attempt_id]
                if not matches:
                    return self._json(404, {"error": {"message": "attempt not found", "type": "not_found_error"}})
                from tempo_verdicts import explicit_feedback
                outcome = explicit_feedback(matches[-1], verdict, body.get("evidence"))
                import tempo_ratings
                tempo_ratings.record(outcome)
                append_receipt(gateway.ledger / "receipts.jsonl", {
                    "job_id": matches[-1].get("job_id"), "attempt_id": attempt_id,
                    "status": "verdict_recorded", "verdict": outcome["verdict"], "verdict_by": "owner",
                    **{k:matches[-1][k] for k in ('profile','profile_source','profile_policy_sha256','screen_result','hosted','requires_review') if k in matches[-1]}})
                return self._json(200, {"ok": True, "attempt_id": attempt_id, "verdict": verdict})
            except (ValueError, TypeError, OSError, json.JSONDecodeError, RecursionError):
                return self._json(400, {"error": {"message": "invalid feedback", "type": "invalid_request_error"}})

        def _event(self, obj):
            self.wfile.write(b"data: " + json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n\n")
            self.wfile.flush()

    return Handler


class _CappedThreadingHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 16

    def __init__(self, *args, max_handlers=MAX_HANDLER_THREADS, **kwargs):
        super().__init__(*args, **kwargs)
        self._handler_slots = threading.BoundedSemaphore(max_handlers)

    def process_request(self, request, client_address):
        if not self._handler_slots.acquire(blocking=False):
            try:
                request.settimeout(1)
                request.sendall(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\nRetry-After: 1\r\n\r\n")
            except OSError:
                pass
            finally:
                self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self._handler_slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._handler_slots.release()


def serve(host, port, gateway, token, tools=None):
    if host not in {"127.0.0.1", "::1", "localhost"}:
        raise ValueError("gateway must bind to loopback")
    server = _CappedThreadingHTTPServer((host, port), make_handler(gateway, token, tools))
    server.serve_forever(poll_interval=0.25)


def main():
    ap = argparse.ArgumentParser(description="Local-only Perennos Tempo OpenAI-compatible gateway")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--lease-script", default=os.environ.get("TEMPO_GPU_LEASE_SCRIPT"))
    ap.add_argument("--lease-need-mib", type=int, default=int(os.environ.get("TEMPO_GPU_LEASE_NEED_MIB", "4096")))
    ap.add_argument("--lease-ttl-min", type=int, default=20)
    ap.add_argument("--timeout-seconds", type=int, default=600)
    ap.add_argument("--device", choices=["auto", "gpu", "cpu"], default="auto")
    ap.add_argument("--cpu-booking")
    ap.add_argument("--cpu-until", type=float, default=0)
    ap.add_argument("--tool-policy", default=os.environ.get("TEMPO_TOOL_POLICY"),
                    help="owner tool policy (.yaml/.json); without it every tools request is denied with policy_missing")
    args = ap.parse_args()
    if not 1024 <= args.port <= 65535 or args.lease_need_mib < 1:
        ap.error("invalid port or lease memory request")
    token = os.environ.get("TEMPO_GATEWAY_TOKEN") or _token_from_file(LEDGER_DIR / "gateway-token")
    if len(token) < 32:
        ap.error("TEMPO_GATEWAY_TOKEN must be at least 32 characters")
    gateway = TempoGateway(DATA, args.lease_script, args.lease_need_mib, lease_ttl=args.lease_ttl_min,
                           timeout=args.timeout_seconds, device=args.device,
                           cpu_booking=args.cpu_booking, cpu_until=args.cpu_until)
    from tempo_profile_gateway import ProfileGateway
    gateway = ProfileGateway(gateway)
    print(json.dumps({"status": "starting", "bind": args.host, "port": args.port, "mode": "local-only",
                      "token_file": str(LEDGER_DIR / "gateway-token"), "lease_script_configured": bool(args.lease_script),
                      "lease_need_mib": args.lease_need_mib}), flush=True)
    tools = None
    if args.tool_policy:
        import tool_policy
        try:
            tools = tool_policy.build_runtime(gateway, args.tool_policy, LEDGER_DIR / "receipts.jsonl")
        except tool_policy.PolicyError as exc:
            ap.error("tool policy refused: " + str(exc))  # malformed policy refuses startup, like profiles.json
    serve(args.host, args.port, gateway, token, tools)


if __name__ == "__main__":
    main()
