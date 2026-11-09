"""The Python agent's fetch_url contacts nobody to find out whether the internet is up. Run: python -m unittest discover -s tests"""
import importlib
import os
import socket
import sys
import tempfile
import unittest
import urllib.request
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(HERE), "tempo-agent"))


def agent():
    os.environ.setdefault("TEMPO_AGENT_HOME", tempfile.mkdtemp(prefix="tempo-agent-fetch-"))
    return importlib.import_module("tempo_agent")


class FetchUrl(unittest.TestCase):
    def test_no_probe_connection_is_made_and_a_page_is_read_when_the_network_works(self):
        a = agent()
        page = mock.MagicMock()
        page.read.return_value = b"<html><body>hello <b>there</b></body></html>"
        with mock.patch.object(socket, "create_connection", side_effect=AssertionError("something was contacted to probe")), \
                mock.patch.object(urllib.request, "urlopen", return_value=page):
            self.assertEqual(a.t_fetch_url(None, "https://example.com/"), "hello there")

    def test_a_fetch_that_cannot_get_through_says_offline_instead_of_raising(self):
        a = agent()
        with mock.patch.object(urllib.request, "urlopen", side_effect=OSError("no route")):
            self.assertTrue(a.t_fetch_url(None, "https://example.com/").startswith("offline: no internet right now"))

    def test_the_environment_still_forces_offline_without_touching_the_network(self):
        a = agent()
        with mock.patch.dict(os.environ, {"TEMPO_OFFLINE": "1"}), mock.patch.object(urllib.request, "urlopen", side_effect=AssertionError("fetched")):
            self.assertTrue(a.t_fetch_url(None, "https://example.com/").startswith("offline"))


if __name__ == "__main__":
    unittest.main()
