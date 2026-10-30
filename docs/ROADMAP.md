# Product priorities and acceptance gates

| Priority | Work | Evidence required before claiming it works |
| --- | --- | --- |
| 1 | Reliable Windows install, reopen, repair and clean uninstall | Rebuilt installer from this exact source; fresh default-folder run; GUI/API/MCP probes; first private completion receipt; restart and cleanup evidence. |
| 2 | Release trust | A reviewed signing plan, checksum verification, immutable release policy and build provenance. Do not disable Windows protections. |
| 3 | Measured routing quality | Public synthetic task set; per-engine revision; quality/admission failure rates, cold/warm latency and memory measurements. Evaluate before replacing routing models. |
| 4 | Better live feedback | Receipt-aware streaming with visible partial/cancel states; prove ambiguous requests are never replayed. |
| 5 | Hardware-aware context | Tokenizer/engine-specific context admission including output reserve; report truncation explicitly. No guessed memory or speed claims. |
| 6 | Accessible desktop | Native screenshots at supported sizes/scales, keyboard and screen-reader tests, long-text and failure-state checks. |
| 7 | More providers and backends | Explicit owner-selected credentials, budgets, reviewed endpoints and protocol fixtures; no private-to-hosted failover. |

The current patch implements read-only diagnostics, bounded follow-ups, desktop utility controls, documentation and a source CI matrix. These do not complete the native release gates above. Persistent chat storage, silent automatic summarization, telemetry collection and arbitrary remote tools are outside this change.
