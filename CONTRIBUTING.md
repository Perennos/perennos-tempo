# Contributing

Perennos Tempo public core is an early community source release. Keep changes small and preserve privacy, fail-closed routing, explicit admission, and metadata-only receipts.

- Do not submit prompts, customer records, credentials, private configuration, ledgers, model weights, or machine-specific paths.
- Explain the user-visible behavior and exact files changed. Include a reproducible verification command and its real result with each change.
- Keep third-party source and notices separate. Do not copy private vertical packs, desks, owner profiles, or workspace patches into the core.
- Do not claim a clean-machine install, hosted privacy, model quality, or production readiness without fresh evidence.

Run `python -m unittest discover -s tests -v` and `python -m unittest discover -s acceptance -p 'test_*.py' -v` on Python 3.12 before submitting. Keep runtime files in the closed manifest and refresh SHA-256 hashes after changes; `tests/test_manifest_consistency.py` checks them. Source checks and native installer acceptance establish different things.

For this product-quality contribution, commits are authored by **marzouca**, with **OpenAI Codex** disclosed as the implementation assistant in commit trailers and the PR. Preserve all existing authorship, third-party notices and licensing. AI assistance does not establish verification; attach actual command results.

Use the repository's issue tracker or contribution route published with the release.
