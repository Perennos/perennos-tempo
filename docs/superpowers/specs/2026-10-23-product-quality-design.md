# Tempo product quality design

Intent: make the existing local-first desktop and developer core useful, diagnosable and credible. Build on release/v1-0-1 (43ee6d9) without overwriting its Windows fixes or claiming a release is qualified.

## Approaches
A cosmetic rewrite would leave installation and follow-up failures intact. A new agent framework would add dependencies and widen privacy boundaries. Selected engineering approach (maintainer judgment under the requested broad improvement scope): retain Python/Tk and the current routing core; improve the actual user journey and supply reproducible checks.

## Deliverables
- Read-only, standard-library doctor: bounded local configuration inspection, fixed safe diagnostic codes and recovery hints; optional authenticated loopback GET probes, never inference or external traffic. Report no tokens, paths, model names, raw config or exception text. No automatic repair.
- Desktop follow-ups: a per-window in-memory conversation, at most 12 completed turn pairs, 12000 UTF-8 bytes including the next prompt, pairs dropped oldest first. Only successful turns enter history. No automatic retry, persistence, cloud fallback, or hidden summarization. Reject an oversized new message before clearing it.
- Client: preserve the prompt-only API; introduce a separately validated chat_messages function. Always private; numeric loopback and same-connection authentication unchanged.
- Desktop utility: new conversation clears in-memory state only while idle, copy last answer is explicit clipboard action, busy elapsed indicator and readable error recovery. Preserve established paper/ink/lime visual language.
- Repository: branded SVG header, accurate quick start, troubleshooting, architecture map, research decisions and roadmap. Keep installer entry points at root; new documentation lives in docs. Include new runtime files in the closed manifest.
- CI: exercise source on Linux and Windows; retain separate native Windows acceptance. Retain public synthetic acceptance evidence for 14 days.

## Constraints and acceptance
No added dependencies, downloads, private data uploads, paid calls, provider credential changes, or model swaps. No merge into main and no release publication. Build on candidate in a dedicated clone/branch. Source tests, installer fixtures, ratings checks, byte compilation, manifest hashes and a final independent review must pass. Tk rendering, actual inference, high DPI, screen reader, signing and user-machine installation are Untested unless fresh evidence proves otherwise.
