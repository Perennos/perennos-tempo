import io
import json
import unittest
from unittest.mock import patch

import tempo_mcp


MODERN = "2026-07-28"
PROTOCOL_VERSION_KEY = "io.modelcontextprotocol/protocolVersion"
CLIENT_CAPABILITIES_KEY = "io.modelcontextprotocol/clientCapabilities"
SERVER_INFO_KEY = "io.modelcontextprotocol/serverInfo"


def modern_metadata(version=MODERN, *, capabilities=True):
    metadata = {PROTOCOL_VERSION_KEY: version}
    if capabilities:
        metadata[CLIENT_CAPABILITIES_KEY] = {}
    return metadata


class MCPProtocolTests(unittest.TestCase):
    def setUp(self):
        self.server = tempo_mcp.TempoMCP("127.0.0.1", 8765, "x" * 32)

    def test_modern_discovery_advertises_stateless_and_legacy_versions(self):
        response = self.server.dispatch({
            "jsonrpc": "2.0",
            "id": "discover-1",
            "method": "server/discover",
            "params": {"_meta": modern_metadata()},
        })

        self.assertEqual(response["id"], "discover-1")
        result = response["result"]
        self.assertEqual(result["resultType"], "complete")
        self.assertEqual(result["supportedVersions"], [MODERN, "2025-11-25", "2025-06-18"])
        self.assertEqual(result["capabilities"], {"tools": {"listChanged": False}})
        self.assertEqual(result["_meta"][SERVER_INFO_KEY], {
            "name": "Perennos Tempo",
            "version": "1.0.1",
        })

    def test_modern_tools_list_is_complete_and_identifies_the_server(self):
        response = self.server.dispatch({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {"_meta": modern_metadata()},
        })

        self.assertEqual(response["result"]["resultType"], "complete")
        self.assertEqual([tool["name"] for tool in response["result"]["tools"]], [
            "ask", "listmodels", "status",
        ])
        self.assertIn(SERVER_INFO_KEY, response["result"]["_meta"])

    def test_modern_tool_call_strips_protocol_metadata_before_argument_validation(self):
        with patch.object(tempo_mcp, "gateway_status", return_value=({"status": "ready", "mode": "local"}, {})):
            response = self.server.dispatch({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "status",
                    "arguments": {},
                    "_meta": modern_metadata(),
                },
            })

        result = response["result"]
        self.assertEqual(result["resultType"], "complete")
        self.assertFalse(result["isError"])
        self.assertIn(SERVER_INFO_KEY, result["_meta"])

    def test_modern_requests_require_client_capabilities(self):
        response = self.server.dispatch({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "server/discover",
            "params": {"_meta": modern_metadata(capabilities=False)},
        })

        self.assertEqual(response["error"]["code"], -32602)

    def test_modern_unknown_version_returns_supported_versions(self):
        response = self.server.dispatch({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "server/discover",
            "params": {"_meta": modern_metadata("2099-01-01")},
        })

        self.assertEqual(response["error"]["code"], -32022)
        self.assertEqual(response["error"]["data"]["requested"], "2099-01-01")
        self.assertEqual(response["error"]["data"]["supported"], [
            MODERN, "2025-11-25", "2025-06-18",
        ])

    def test_legacy_version_cannot_be_used_as_stateless_request_metadata(self):
        response = self.server.dispatch({
            "jsonrpc": "2.0",
            "id": 51,
            "method": "tools/list",
            "params": {"_meta": modern_metadata("2025-11-25")},
        })

        self.assertEqual(response["error"]["code"], -32602)

    def test_modern_initialize_is_not_downgraded_to_a_legacy_handshake(self):
        response = self.server.dispatch({
            "jsonrpc": "2.0",
            "id": 52,
            "method": "initialize",
            "params": {
                "protocolVersion": MODERN,
                "_meta": modern_metadata(),
            },
        })

        self.assertEqual(response["error"]["code"], -32601)

    def test_legacy_initialize_echoes_supported_version_and_offers_latest_legacy(self):
        supported = self.server.dispatch({
            "jsonrpc": "2.0", "id": 6, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18"},
        })
        unsupported = self.server.dispatch({
            "jsonrpc": "2.0", "id": 7, "method": "initialize",
            "params": {"protocolVersion": "2099-01-01"},
        })

        self.assertEqual(supported["result"]["protocolVersion"], "2025-06-18")
        self.assertEqual(unsupported["result"]["protocolVersion"], "2025-11-25")
        self.assertNotIn("resultType", supported["result"])

    def test_legacy_tool_list_keeps_the_existing_handshake_era_shape(self):
        response = self.server.dispatch({
            "jsonrpc": "2.0", "id": 8, "method": "tools/list", "params": {},
        })

        self.assertEqual([tool["name"] for tool in response["result"]["tools"]], [
            "ask", "listmodels", "status",
        ])
        self.assertNotIn("resultType", response["result"])
        self.assertNotIn("_meta", response["result"])

    def test_null_and_boolean_request_ids_are_rejected(self):
        for request_id in (None, True):
            with self.subTest(request_id=request_id):
                response = self.server.dispatch({
                    "jsonrpc": "2.0", "id": request_id, "method": "ping",
                })
                self.assertEqual(response["error"]["code"], -32600)

    def test_stdio_serves_modern_discovery_and_legacy_initialize(self):
        requests = [
            {
                "jsonrpc": "2.0", "id": 9, "method": "server/discover",
                "params": {"_meta": modern_metadata()},
            },
            {
                "jsonrpc": "2.0", "id": 10, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25"},
            },
        ]
        source = io.BytesIO(b"".join(
            json.dumps(request).encode("utf-8") + b"\n" for request in requests
        ))
        sink = io.BytesIO()

        tempo_mcp.serve("127.0.0.1", 8765, "x" * 32, stdin=source, stdout=sink)

        responses = [json.loads(line) for line in sink.getvalue().splitlines()]
        self.assertEqual([response["id"] for response in responses], [9, 10])
        self.assertEqual(responses[0]["result"]["supportedVersions"][0], MODERN)
        self.assertEqual(responses[1]["result"]["protocolVersion"], "2025-11-25")


if __name__ == "__main__":
    unittest.main()
