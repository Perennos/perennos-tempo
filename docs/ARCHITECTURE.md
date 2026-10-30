# Architecture and source map

Tempo keeps its existing root-level entry points so installed launchers and imports remain compatible. Documentation lives under `docs/`, tests under `tests/`, and native delivery fixtures under `acceptance/`.

| Responsibility | Files |
| --- | --- |
| CLI, desktop and volatile turn context | `tempo.py`, `tempo_desktop.py`, `tempo_conversation.py` |
| Safe diagnostics | `tempo_doctor.py` |
| Authenticated, bounded loopback client | `tempo_client.py` |
| Request validation, resource admission, local inference | `tempo_gateway.py`, `tempo_standalone.py`, `tempo_runtime.py`, `tempo_local_http.py` |
| Profiles, privacy and hosted boundaries | `tempo_profile_gateway.py`, `tempo_profiles.py`, `tempo_content_screen.py`, `tempo_hosted.py` |
| Routing quality and receipts | `tempo_decision_head.py`, `tempo_ratings.py`, `tempo_verdicts.py`, `tempo_receipts.py` |
| MCP stdio adapter | `tempo_mcp.py` |
| Native installation, qualified setup and downloads | `desktop/Bootstrap.cs`, `tempo_bootstrap.py`, `tempo_setup.py`, `tempo_pull.py`, `fetch_models.py` |
| Closed delivery payload | `MANIFEST.json`, `acceptance/prepare_payload.py`, `build_desktop_installer.py` |

A desktop send snapshots bounded turn context on the Tk thread. A worker sends it through `chat_messages`; results return through a queue. Only a successful visible answer commits a user/assistant pair. Tk widgets are updated by the UI event loop. Context is never loaded from or written to a history file.

Each request crosses listener identity verification, bearer authentication, request validation, privacy/profile evaluation, engine eligibility and resource admission before inference. Receipts record metadata; they do not establish semantic correctness. Hosted configuration is separate and remains opt-in.

`doctor` deliberately avoids importing the model stack or `tempo_home` at startup. It checks file shapes and recorded gates without initializing a tenant. Offline checks do not open sockets. `--connect` performs the authenticated model-listing GET only, with a five-second client limit; no inference readiness claim follows from a passing listing.
