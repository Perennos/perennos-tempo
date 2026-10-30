# Troubleshooting Tempo

Start with `python tempo.py doctor --home "YOUR_DATA_FOLDER"`. Use the Python bundled with your installation if Python is not on PATH. Keep your actual data-folder selection. This command works even if that folder is missing.

| Diagnostic code | Meaning and next step |
| --- | --- |
| `home_unavailable` | Select your existing data folder. Linked folders/junctions are refused. Complete setup if no data folder exists. |
| `tenant_invalid`, `config_invalid` | Missing, linked, oversized, malformed or duplicate-key JSON. Review locally or re-run reviewed setup. Do not post the file contents. |
| `setup_incomplete`, `setup_invalid` | Required setup gates are absent or failed. A `ready` flag alone is insufficient. Re-run setup, keeping your data. |
| `local_engine_missing`, `engines_invalid` | Review setup, start the approved local engine and sync its registry. A registered engine is not proof it can answer. |
| `token_unavailable` | The existing local gateway credential is absent or invalid. Complete setup; do not invent a credential or paste it into an issue. |
| `gateway_not_probed` | Offline checks finished. Add `--connect` to verify authenticated gateway access without running inference. |
| `gateway_unavailable` | Open Tempo; check its chosen port and local process status. A foreign or untrusted listener is intentionally rejected before any credential is sent. |

`doctor --connect --port PORT` sends no prompts. It reads the existing token internally and uses the same challenge-verified client as desktop/MCP. It does not create tokens, refresh ACLs or repair files. Recorded setup success can become stale after a model, runtime or configuration change. Only a new private answer and receipt can establish inference works now.

## A chat follow-up seems forgotten

Recent successful turns are retained only in the current window, within a 12-turn / 12,000-byte budget including the next prompt. Oldest complete pairs drop first; long answers can clear retained context. New chat, a new window or closing the window starts fresh. Failed and cancelled requests do not enter context. Repeat necessary context explicitly when needed.

## A message is too large

Shorten it or send a focused excerpt. The draft stays in the editor; Tempo does not silently truncate or upload it. The byte limit counts UTF-8, so emoji and some non-Latin scripts use more than one byte per character.

## Waiting or cancellation

The desktop shows elapsed wait time. Esc or Stop waiting ends the client's wait. A request already sent may finish in the backend. Check receipts before retrying; Tempo does not automatically replay it. New chat is disabled while waiting. Closing the window leaves the gateway running.

## Reporting a problem

Provide the commit or release tag, Windows version, the safe `doctor` JSON, and which step failed. Remove anything private you manually add. Do not upload tenant/config files, tokens, raw logs, prompts, conversation transcripts, or owner data. Public CI acceptance fixtures use synthetic data only.

## Native visual qualification — Untested in this source change

Read-only final source review confirms explicit control labels, wrapped subtitle/status, non-color error text, and keyboard send/cancel controls. No native screenshot audit was possible in the Linux editing environment. Before release, check 1024×740 and 760×560 controls, long status text, busy/error/success/clear/copy states, 125–200% scaling, keyboard traversal, screen readers, clipboard behavior after closing, and an actual follow-up. Native Windows acceptance is still required for this exact commit.
