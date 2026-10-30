import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

_original_tempo_home = os.environ.get("TEMPO_HOME")
_import_tempo_home = tempfile.TemporaryDirectory(prefix="tempo-gpu-lifecycle-")
os.environ["TEMPO_HOME"] = _import_tempo_home.name
import tempo_gateway
import tempo_runtime
import tempo_setup
import tempo_standalone
import tempo_local_http
if _original_tempo_home is None:
    os.environ.pop("TEMPO_HOME", None)
else:
    os.environ["TEMPO_HOME"] = _original_tempo_home


MODEL = "tempo-test:latest"
REVISION = "a" * 64
URL = "http://127.0.0.1:11434"


class _Response(io.BytesIO):
    def __init__(self, value):
        super().__init__(json.dumps(value).encode("utf-8"))
        self.line = json.dumps(value).encode("utf-8") + b"\n"

    def __iter__(self):
        return iter((self.line,))


class _FakeLock:
    def __init__(self):
        self.held = False

    def __enter__(self):
        self.held = True
        return self

    def __exit__(self, *_args):
        self.held = False


class _TrickledResponse:
    def __init__(self, now):
        self.now = now
        self.closed = False

    def read1(self, _size):
        self.now[0] += 0.25
        return b" "

    def close(self):
        self.closed = True


class _TrackedLock(_FakeLock):
    def __init__(self, runtime):
        super().__init__()
        self.runtime = runtime
        runtime.locks.append(self)

    def __enter__(self):
        if self.runtime.current_lock and self.runtime.current_lock.held:
            raise AssertionError("standalone machine lock was already held")
        super().__enter__()
        self.runtime.current_lock = self
        return self

    def __exit__(self, *_args):
        super().__exit__(*_args)
        if self.runtime.current_lock is self:
            self.runtime.current_lock = None


class _FakeOllama:
    def __init__(self, *, require_lock=False):
        self.require_lock = require_lock
        self.models = {MODEL: {"name": MODEL, "digest": REVISION, "size": 1_000_000,
                               "details": {"parameter_size": "1B"}}}
        self.resident = set()
        self.unload_after_ps = {}
        self.payloads = []
        self.ps_calls = 0
        self.current_lock = None
        self.locks = []

    def _check_lock(self):
        if self.require_lock and (self.current_lock is None or not self.current_lock.held):
            raise AssertionError("Ollama was inspected or called without the standalone lock")

    def open_local(self, target, *, timeout):
        url = target.full_url if hasattr(target, "full_url") else target
        if url.endswith("/api/ps"):
            self._check_lock()
            self.ps_calls += 1
            for model in tuple(self.resident):
                remaining = self.unload_after_ps.get(model)
                if remaining is not None:
                    if remaining <= 0:
                        self.resident.remove(model)
                        self.unload_after_ps.pop(model, None)
                    else:
                        self.unload_after_ps[model] = remaining - 1
            return _Response({"models": [{"name": name} for name in sorted(self.resident)]})
        if url.endswith("/api/tags"):
            self._check_lock()
            return _Response({"models": list(self.models.values())})
        if url.endswith("/api/show"):
            self._check_lock()
            return _Response({"model_info": {
                "general.architecture": "test",
                "test.block_count": 1,
                "test.embedding_length": 8,
                "test.attention.head_count": 2,
                "test.attention.head_count_kv": 1,
            }})
        if url.endswith("/api/chat"):
            self._check_lock()
            payload = json.loads(target.data)
            if self.resident:
                raise RuntimeError("standalone runtime is still busy")
            self.payloads.append(payload)
            self.resident.add(payload["model"])
            self.unload_after_ps[payload["model"]] = 1 if payload.get("keep_alive") == 0 else None
            return _Response({"message": {"content": "OK"}, "done": True,
                              "done_reason": "stop", "prompt_eval_count": 1,
                              "eval_count": 1, "eval_duration": 1_000_000_000})
        raise AssertionError(f"unexpected local-runtime endpoint: {url}")


class GpuLifecycleTests(unittest.TestCase):
    def test_request_releases_gpu_only_when_requested_and_preserves_cpu_residency(self):
        _, gpu_default = tempo_runtime.request(MODEL, [], 0.2, 16)
        _, gpu_release = tempo_runtime.request(MODEL, [], 0.2, 16, release_after=True)
        _, cpu = tempo_runtime.request(MODEL, [], 0.2, 16, cpu=True)
        with patch.object(tempo_runtime, "_resident_options", return_value={"num_ctx": 8192}):
            _, resident = tempo_runtime.request(MODEL, [], 0.2, 16, cpu=True,
                resident_policy={}, backend_witness={})

        self.assertNotIn("keep_alive", gpu_default)
        self.assertEqual(gpu_release["keep_alive"], 0)
        self.assertEqual(gpu_release["options"], gpu_default["options"])
        self.assertEqual(cpu["keep_alive"], 0)
        self.assertEqual(cpu["options"], {"temperature": 0.2, "num_predict": 16,
                                          "num_gpu": 0, "num_thread": 2, "num_ctx": 8192})
        self.assertEqual(resident["keep_alive"], -1)
        self.assertEqual(resident["options"]["num_ctx"], 8192)

    def test_standalone_gpu_qualification_requests_release_after(self):
        fake = _FakeOllama()
        admission = type("Admission", (), {
            "device": "gpu", "context_tokens": 4096, "receipt": {"guard": "test"},
            "dispatched": lambda self: None, "check": lambda self: None,
        })()
        guard = type("Guard", (), {
            "__enter__": lambda self: admission, "__exit__": lambda self, *_args: None,
        })()
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            (home / "engines.json").write_text(json.dumps({"engines": [{
                "id": "test-engine", "model": MODEL, "revision": REVISION,
                "provider": "ollama", "roles": ["inference"], "size": "1B",
            }]}), encoding="utf-8")
            with (patch.object(tempo_standalone, "StandaloneGuard", return_value=guard),
                  patch.object(tempo_local_http, "open_local", side_effect=fake.open_local),
                  patch.object(tempo_runtime, "request", wraps=tempo_runtime.request) as request):
                result = tempo_setup.qualify(home, URL, {MODEL}, None, {}, device="gpu",
                                             only_engine="test-engine")

        self.assertEqual(result["qualified"], "test-engine")
        self.assertTrue(request.call_args.kwargs["release_after"])
        self.assertEqual(fake.payloads[0]["keep_alive"], 0)

    def test_two_sequential_standalone_gpu_gateway_requests_wait_for_delayed_unload(self):
        fake = _FakeOllama(require_lock=True)
        old_head_device = os.environ.get("TEMPO_HEAD_DEVICE")
        old_head_threads = os.environ.get("TEMPO_HEAD_THREADS")
        with tempfile.TemporaryDirectory() as directory:
            data = Path(directory)
            (data / "ledger").mkdir()
            (data / "engines.json").write_text(json.dumps({"engines": [{
                "id": "test-engine", "model": MODEL, "revision": REVISION,
                "provider": "ollama", "locality": "local", "roles": ["inference"],
                "qualified": True, "policy_tier": "test", "policy_order": 0,
            }]}), encoding="utf-8")
            try:
                with (patch.object(tempo_standalone, "free_vram", return_value=16384),
                      patch.object(tempo_standalone, "MachineLock", side_effect=lambda: _TrackedLock(fake)),
                      patch.object(tempo_local_http, "open_local", side_effect=fake.open_local),
                      patch.object(tempo_standalone.time, "sleep", return_value=None)):
                    gateway = tempo_gateway.TempoGateway(data, None, lease_need_mib=1,
                        ollama_url=URL, timeout=20, device="gpu")
                    gateway._rating_plan = lambda _route, _engines: {"status": "neutral", "ratings": None}
                    gateway._transition = lambda _route, _digest: (None, None)
                    route = {"task_class": "writing", "role": "inference", "privacy_class": "private",
                             "conversation_id": None, "messages": [{"role": "user", "content": "hello"}],
                             "temperature": 0.2, "max_tokens": 16}
                    first = gateway.run(route)
                    second = gateway.run(route)
            finally:
                if old_head_device is None:
                    os.environ.pop("TEMPO_HEAD_DEVICE", None)
                else:
                    os.environ["TEMPO_HEAD_DEVICE"] = old_head_device
                if old_head_threads is None:
                    os.environ.pop("TEMPO_HEAD_THREADS", None)
                else:
                    os.environ["TEMPO_HEAD_THREADS"] = old_head_threads

        self.assertEqual(first["choices"][0]["message"]["content"], "OK")
        self.assertEqual(second["choices"][0]["message"]["content"], "OK")
        self.assertEqual(len(fake.payloads), 2)
        self.assertTrue(all(payload.get("keep_alive") == 0 for payload in fake.payloads))
        self.assertGreaterEqual(fake.ps_calls, 6)
        self.assertFalse(fake.resident)
        self.assertTrue(all(not lock.held for lock in fake.locks))

    def test_unload_confirmation_failure_quarantines_the_held_lock(self):
        lock = _FakeLock()
        now = [0.0]
        info = {"size": 1024, "digest": REVISION, "details": {"parameter_size": "1B"}}

        def monotonic():
            return now[0]

        def sleep(seconds):
            now[0] += seconds

        def still_busy(url, *, timeout):
            self.assertTrue(lock.held)
            self.assertTrue(url.endswith("/api/ps"))
            self.assertLessEqual(timeout, 1)
            return _Response({"models": [{"name": "foreign-model"}]})

        with (patch.object(tempo_standalone, "MachineLock", return_value=lock),
              patch.object(tempo_standalone, "_quarantine", []) as quarantine,
              patch.object(tempo_standalone.time, "monotonic", side_effect=monotonic),
              patch.object(tempo_standalone.time, "sleep", side_effect=sleep),
              patch.object(tempo_local_http, "open_local", side_effect=still_busy)):
            guard = tempo_standalone.StandaloneGuard(MODEL, URL, requested="cpu", timeout=1,
                                                       internal_info=info)
            guard.__enter__()
            guard.dispatched()
            with self.assertRaises(tempo_standalone.GuardError):
                guard.__exit__(None, None, None)

        self.assertTrue(lock.held)
        self.assertEqual(quarantine, [lock])

    def test_trickled_unload_body_hits_absolute_deadline_and_quarantines_lock(self):
        lock = _FakeLock()
        now = [0.0]
        response = _TrickledResponse(now)
        info = {"size": 1024, "digest": REVISION, "details": {"parameter_size": "1B"}}

        def monotonic():
            return now[0]

        def open_trickled(url, *, timeout):
            self.assertTrue(lock.held)
            self.assertTrue(url.endswith("/api/ps"))
            self.assertLessEqual(timeout, 1)
            return response

        with (patch.object(tempo_standalone, "MachineLock", return_value=lock),
              patch.object(tempo_standalone, "_quarantine", []) as quarantine,
              patch.object(tempo_standalone.time, "monotonic", side_effect=monotonic),
              patch.object(tempo_local_http, "open_local", side_effect=open_trickled)):
            guard = tempo_standalone.StandaloneGuard(MODEL, URL, requested="cpu", timeout=1,
                                                       internal_info=info)
            guard.__enter__()
            guard.dispatched()
            with self.assertRaises(tempo_standalone.GuardError):
                guard.__exit__(None, None, None)

        self.assertTrue(response.closed)
        self.assertTrue(lock.held)
        self.assertEqual(quarantine, [lock])

    def test_failed_and_internal_requests_keep_the_existing_no_probe_rules(self):
        info = {"size": 1024, "digest": REVISION, "details": {"parameter_size": "1B"}}
        failed_lock = _FakeLock()
        with (patch.object(tempo_standalone, "MachineLock", return_value=failed_lock),
              patch.object(tempo_standalone, "_quarantine", []) as quarantine,
              patch.object(tempo_local_http, "open_local") as open_local):
            failed = tempo_standalone.StandaloneGuard(MODEL, URL, requested="cpu", internal_info=info)
            failed.__enter__()
            failed.dispatched()
            failed.__exit__(TimeoutError, TimeoutError(), None)
            self.assertTrue(failed_lock.held)
            self.assertEqual(quarantine, [failed_lock])
            open_local.assert_not_called()

        internal_lock = _FakeLock()
        with (patch.object(tempo_standalone, "MachineLock", return_value=internal_lock),
              patch.object(tempo_standalone, "_quarantine", []) as quarantine,
              patch.object(tempo_local_http, "open_local") as open_local):
            internal = tempo_standalone.StandaloneGuard("decision-head", URL, requested="cpu",
                                                         internal_info=info)
            internal.__enter__()
            internal.__exit__(None, None, None)
            self.assertFalse(internal_lock.held)
            self.assertEqual(quarantine, [])
            open_local.assert_not_called()


if __name__ == "__main__":
    unittest.main()
