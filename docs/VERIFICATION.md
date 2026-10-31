# Verification record

Status: **In progress** pending exact-head GitHub review and MCP SDK interoperability. Native Windows installation and live model inference are **Untested** for this change.

Evidence below is from the current local source tree based on `release/v1-0-1`, including the modern MCP adapter changes. Exact-head GitHub CI and review remain pending. These checks do not establish installer, GUI, GPU, or production behavior.

| Check | Fresh result |
| --- | --- |
| `python -m unittest discover -s tests -v` | 119 tests passed; 1 skipped |
| `python -m unittest discover -s acceptance -p 'test_*.py' -v` | 6 tests passed |
| Closed payload preparation | Passed with all 69 manifest inputs; temporary payload contained 70 files including `MANIFEST.json` |
| Manifest integrity | Passed in the source suite; all 69 listed entries match |
| Byte compilation and whitespace check | Passed |
| Modern MCP protocol fixtures | Passed for discovery, metadata, tools/list, tools/call, unsupported versions, invalid IDs, and stdio framing |
| External SDK interoperability | Untested; exact-build client handshake is a merge/release gate |
| Native Windows installer, GUI, accessibility, GPU inference, and live gateway | Untested |
| Visual presentation gates | Not applicable; no visual assets or interface changed |

The source tests ran under CPython 3.12. The skipped case is the existing opt-in native integration check; no live model/provider calls were made. The payload preparation used a temporary directory and left no generated payload in the checkout.

The MCP adapter now supports stateless discovery using protocol version `2026-07-28` while retaining legacy `2025-11-25` and `2025-06-18` initialization. This is source-level compatibility coverage only; an independent MCP SDK client has not yet exercised the bridge.

No release, installer, or production readiness is claimed. Rebuild from the exact merged source and complete native Windows acceptance before shipping an installer.
