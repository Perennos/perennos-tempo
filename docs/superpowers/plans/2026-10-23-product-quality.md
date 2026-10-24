# Product Quality Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans inline; obtain one independent final review.

**Goal:** improve supportability, conversation usefulness and developer trust without changing privacy defaults.
**Architecture:** keep root runtime entry points and Python/Tk. Separate bounded conversation state and diagnostics into small stdlib modules; integrate through the existing local client.
**Tech Stack:** Python stdlib, Tk, unittest, GitHub Actions.
**Spec:** docs/superpowers/specs/2026-10-23-product-quality-design.md

## Global Constraints
No new dependencies or paid calls; no data uploads or hosted fallback. Doctor never generates an answer or repairs configuration. Chat history is volatile. Preserve the candidate and closed release manifest. Do not merge or publish a binary.

## Review Focus
- Malformed or linked diagnostic input must reveal no source contents.
- Overlong Unicode prompts must leave the draft intact.
- Failed/cancelled requests must not poison subsequent context.
- Clearing a chat while busy must not discard an in-flight turn.
- Runtime additions must reach the native payload, with matching hashes.

### Task 1: Safe diagnostics
Files: tempo_doctor.py, tempo.py, tests/test_doctor.py, MANIFEST.json.
Produces: diagnose(home: Path, port: int = 8765, connect: bool = False) -> dict; exit_code(report: dict) -> int.
- [x] Write tests for missing/valid/broken/oversize/linked configuration, no secret echo, no writes, offline no network, optional challenge-verified GET only.
- [x] Run tests; confirm missing implementation fails.
- [x] Implement bounded readers, fixed diagnostic messages, CLI doctor before home validation so missing installs can be diagnosed.
- [x] Verify targeted tests and commit.

### Task 2: Useful conversations
Files: tempo_conversation.py, tempo_client.py, tempo_desktop.py, tests/test_conversation.py, tests/test_desktop_state.py, MANIFEST.json.
Produces: Conversation.messages(prompt), commit(prompt, answer), clear(); chat_messages(host, port, token, messages, **request_options).
- [x] Test complete-pair trimming, independent windows, Unicode byte limit, failure state, strict message validation and a real loopback multi-turn exchange.
- [x] Run tests; confirm missing implementation fails.
- [x] Implement state and validated transport. Desktop snapshots context on the UI thread; commits only on success; explicit clear/copy controls; elapsed status.
- [x] Verify targeted tests and commit.

### Task 3: Repository and delivery quality
Files: README.md, docs/brand/tempo-header.svg, docs/ARCHITECTURE.md, docs/TROUBLESHOOTING.md, docs/ROADMAP.md, docs/research/2026-10-23-product-decisions.md, .github/workflows/ci.yml, .github/workflows/windows-acceptance.yml, MANIFEST.json, payload count constants.
- [x] Document current behavior, primary-source research decisions and the Untested native acceptance limits; render and inspect SVG.
- [x] Update CI matrix and retention, refresh manifest and closed payload count.
- [x] Run source/acceptance/ratings checks, compile, whitespace and payload preparation.
- [ ] Obtain independent read-only review; fix blockers once, rerun affected checks; push dedicated branch and open stacked draft PR against release/v1-0-1 with exact evidence.
