# Notices and license scope

Copyright 2026 Perennos. From 6 November 2026, Perennos-authored files in this public core are offered under the PolyForm Shield License 1.0.0; see [LICENSE](LICENSE). In plain words: you may use, change and share Perennos Tempo, for yourself or for your business, and you may contribute back. You may not use it to provide a product that competes with Perennos Tempo or with another Perennos product built on it.

Everything published here before that change (including release v1.0.0) was offered under Apache-2.0, and anyone who received those copies keeps the Apache-2.0 terms for them.

None of this relicenses third-party source, runtimes, dependencies, models, datasets, or their outputs.

Required Notice: Copyright 2026 Perennos (https://perennos.com)

Licensor Line of Business: Perennos Tempo, a local AI agent harness with routing, workflows, fleet tools and companion apps (https://perennos.com/tempo)

## Included third-party material and downloads

- **Sev inference source:** `sev-src/` retains its upstream MIT license and notice in `sev-src/LICENSE`. It is separate from the Perennos-authored core.
- **Sev decision-head model files:** not bundled. The optional fetcher uses pinned upstream files; the source records Apache-2.0 for these model files. Confirm the upstream terms before redistribution.
- **ModernBERT configuration and tokenizer files:** not bundled. Optional pinned downloads are identified in `fetch_models.py`; upstream Apache-2.0 terms apply to those files.
- **Local engine model:** weights are not bundled. The public setup catalog contains the default `qwen3:4b` engine entry; review the model publisher's current terms before downloading or using it.
- **CPython and Tcl/Tk:** the Windows setup EXE bundles an app-local runtime built from the pinned official CPython 3.12.10 distribution. Its `runtime/python/LICENSE.txt` and included upstream notices retain their own terms, including the Python Software Foundation license. The source export contains no runtime binary. The end-user setup extracts this runtime without running the vendor Python installer or changing an existing Python installation.
- **Ollama runtime:** downloaded separately during setup; its upstream terms apply.
- **Python packages:** exact pins are in `requirements.txt`. Direct and transitive packages retain their own licenses and notices.

No leaderboard dataset or model weights are redistributed. The public core starts with neutral priors and omits owner-specific engine maps and priors. Optional Odysseus workspace integration and its patch are not part of this package. No additional third-party license is granted by the Perennos license.
