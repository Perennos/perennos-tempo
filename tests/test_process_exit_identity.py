import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import tempo_bootstrap as B


class _Call:
    def __init__(self, function):
        self.function = function

    def __call__(self, *args):
        return self.function(*args)


class _Kernel:
    def __init__(self, exit_code=1, query_ok=True, open_ok=True):
        self.image_queries = 0
        self.closed = []
        self.OpenProcess = _Call(lambda *_: 99 if open_ok else 0)
        self.GetExitCodeProcess = _Call(lambda _h, out: self.exit_status(out, exit_code, query_ok))
        self.QueryFullProcessImageNameW = _Call(self.image)
        self.GetProcessTimes = _Call(lambda *_: 0)
        self.CloseHandle = _Call(lambda handle: self.closed.append(handle) or 1)

    def exit_status(self, pointer, code, okay):
        if okay:
            pointer._obj.value = code
        return int(okay)

    def image(self, *_):
        self.image_queries += 1
        return 0


class ProcessExitIdentityTests(unittest.TestCase):
    def probe(self, kernel, last_error=87):
        with patch.object(B.os, 'name', 'nt'), \
             patch.object(B.ctypes, 'WinDLL', return_value=kernel, create=True), \
             patch.object(B.ctypes, 'get_last_error', return_value=last_error, create=True):
            return B._process_identity(1234)

    def test_exited_process_with_retained_handle_is_absent_before_parent_lookup(self):
        kernel = _Kernel(exit_code=1)
        with patch.object(B, '_parent_pid', side_effect=AssertionError('dead parent must not be queried')):
            self.assertIsNone(self.probe(kernel))
        self.assertEqual(kernel.image_queries, 0)
        self.assertEqual(kernel.closed, [99])

    def test_still_active_259_is_never_treated_as_absent(self):
        kernel = _Kernel(exit_code=259)
        self.assertEqual(self.probe(kernel), {'unavailable': True})
        self.assertEqual(kernel.image_queries, 1)

    def test_failed_exit_query_is_unknown(self):
        kernel = _Kernel(query_ok=False)
        self.assertEqual(self.probe(kernel), {'unavailable': True})
        self.assertEqual(kernel.image_queries, 0)

    def test_access_denied_is_unknown(self):
        self.assertEqual(self.probe(_Kernel(open_ok=False), last_error=5), {'unavailable': True})

    def test_reused_pid_with_changed_creation_still_fails_custody(self):
        with tempfile.TemporaryDirectory() as folder:
            exe = Path(folder) / 'engine.exe'
            exe.write_bytes(b'fixture')
            digest = hashlib.sha256(exe.read_bytes()).hexdigest()
            record = dict(pid=7, created=1, path=str(exe), sha256=digest, runtime_mode='direct',
                          runtime_parent_pid=11, runtime_parent_created=2, launcher_pid=11,
                          launcher_created=2, launcher_path=str(exe), launcher_sha256=digest)
            same = dict(path=str(exe), created=1, parent_pid=11)
            with patch.object(B, '_process_identity', side_effect=[same, None]):
                self.assertTrue(B._owned_process(record, exe))
            with patch.object(B, '_process_identity', return_value={**same, 'created': 9}):
                self.assertFalse(B._owned_process(record, exe))

    def test_gateway_failure_retains_only_safe_status_and_code(self):
        from tempo_client import TempoAPIError
        message = B._private_error(TempoAPIError('PRIVATE_PROMPT_MARKER', status=502, code='GuardError'))
        self.assertIn('502', message)
        self.assertIn('GuardError', message)
        self.assertNotIn('PRIVATE_PROMPT_MARKER', message)

    def test_gateway_unknown_outcome_is_not_erased_or_echoed(self):
        from tempo_client import TempoAPIError
        message = B._private_error(TempoAPIError('PRIVATE_PROMPT_MARKER', status=True,
                                                code='PRIVATE_PROMPT_MARKER', request_may_continue=True))
        self.assertIn('gateway_error', message)
        self.assertIn('outcome is unknown', message)
        self.assertNotIn('PRIVATE_PROMPT_MARKER', message)


if __name__ == '__main__':
    unittest.main()
