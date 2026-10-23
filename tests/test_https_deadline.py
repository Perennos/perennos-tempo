"""Keep the pinned downloader compatible with the bundled Python runtime."""
from pathlib import Path
import ssl
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import tempo_bootstrap as bootstrap


class HTTPSDeadlineCompatibilityTests(unittest.TestCase):
    def test_verified_context_and_connection_factory_without_removed_keyword(self):
        handler = bootstrap._DeadlineHTTPSHandler(bootstrap.time.monotonic() + 10)
        request = bootstrap.urllib.request.Request('https://github.com/')
        with patch.object(handler, 'do_open', return_value='response') as opened:
            self.assertEqual(handler.https_open(request), 'response')
        factory, actual_request = opened.call_args.args
        kwargs = opened.call_args.kwargs
        self.assertIs(actual_request, request)
        self.assertEqual(set(kwargs), {'context'})
        self.assertTrue(kwargs['context'].check_hostname)
        self.assertEqual(kwargs['context'].verify_mode, ssl.CERT_REQUIRED)
        connection = factory('github.com', **kwargs)
        try:
            self.assertIsInstance(connection, bootstrap._DeadlineHTTPSConnection)
            self.assertIs(connection._context, kwargs['context'])
        finally:
            connection.close()


if __name__ == '__main__':
    unittest.main()
