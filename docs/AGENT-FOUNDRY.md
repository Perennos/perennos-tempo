# Agent foundry

Status: In progress

Perennos Tempo should design an agent from a goal, then make it earn its place. A recipe alone proves nothing. The foundry produces a role, a prompt, a narrowed tool set, guardrails, practice cases and a finite budget. It runs the same fixed baseline and that design against a separate frozen exam, keeps or rejects the design, and records what happened. Keeping a recipe never approves its output for training or gives it new permissions.

Three approaches were considered: reusable recipes (cheap, but no proof), exam-gated recipes (chosen), and whole-team mutation/search (riskier: more inference, harder to isolate leakage and regressions). The first version uses the existing Conductor rather than adding another scheduler.

## Boundaries

- Local engines first. A model reply cannot select a host, change a route, enable a hosted engine, increase a cap or grant a tool. Optional hosted execution must use an owner-configured route and its existing finite spend cap; the first live comparison is local only.
- Every inference is a fresh context. The designer sees the goal and practice schema, never the frozen exam inputs or answers. Generated practice checks are recorded, not used as the admission authority.
- All requested budgets are positive, bounded integers below owner ceilings. Oversized, malformed or unknown output rejects the design; no silent template substitution passes as a designed agent.
- Tools can only narrow a fixed owner allowlist. Source bundles and fixed host checks are data/services, not permission to execute generated shell commands. Guest agents retain no tools, as Sections requires.
- Reuse Conductor's root/child meter, run log, stop switch, Sections and takes contract. A foundry team has one root budget. Stop prevents new work and propagates to every child; in-flight provider cancellation limits must be disclosed.
- Reuse the Woodshed admission rule with independently scored correctness results. Token-F1 alone cannot prove research accuracy, a working fix or useful copy. Require at least five held-out cases, no canary regression, and a candidate score at least the same fixed baseline. Both failing equally is rejection: task correctness must also clear the frozen pass threshold.
- Write real, inspectable receipts with engine/model identity, source and exam hashes, fixed baseline hash, timings, actual/estimated usage distinction, outcomes and limits. No invented cache hits, token savings or producer independence. Preserve failed attempts.
- Keep/reject concerns the agent design. Woodshed receives no automatically approved training winner; human verdict and existing training gates still control learning.

## Implementation map

| Part | Responsibility | Reuse |
| --- | --- | --- |
| design | Validate a local engine's agent design | existing JSON parser and network boundary |
| exam | Fixed task correctness and baseline comparison | Woodshed gate shape and canaries |
| run | Fresh-context local inference, receipts, keep/reject | Conductor meter, log and takes |
| team | Compose retained roles through a band/Sections | Conductor shared meter and stop |

Add a separate `tempo-foundry` CLI in the existing launcher crate. Preserve the normal launcher and all current commands. No new dependency or always-on service. Output is text/JSON and notes; there is no new GUI in this unit.

## Real comparison

Freeze the baseline before designing: `Complete the task accurately. Follow the requested format. Use only the supplied information. Say when the evidence is insufficient.` Both arms use the same engine, settings, tasks and checks. Report all outcomes, not only wins.

1. Research brief: read the supplied public Sections and Woodshed source excerpts, explain their operating and evidence limits with traceable support, and abstain on unsupported measurements.
2. Code fix: repair the real wrapping additions in Conductor's atomic budget counters in an isolated copy, then check overflow, normal increments and parent propagation. Never edit the working project as an inference side effect.
3. Content draft: explain what Sections can do and what has not been measured, using supplied product facts without invented performance, training success or hosted guarantees.

The held-out exam and canaries are versioned separately from model-generated practice cases. Deterministic content checks are necessary but bounded proxies; they do not establish broad research or editorial quality. Code checks execute only the fixed reviewed harness, under an isolated resource-limited process when execution is needed.

## Acceptance

- All three roles are designed by a real owned local engine; invalid designs remain rejected with receipts.
- Each role has paired baseline/candidate results and a keep/reject result from the frozen exam. No improvement is promised in advance.
- At least one actual retained role runs again through Conductor/Sections, with real receipts; shared budget and stop refusal paths have runnable tests.
- Positive and negative tests cover schema/tool escalation, zero/overflow budgets, exam identity/drift, both-arms-fail, provider error/truncation, cancelled work and output preservation.
- Fresh locked tests, formatting, strict Clippy, a clean build and the repository leak scan pass on the tested source. Independent review inspects the final diff and receipts.
- PRs contain safe source/docs and sanitized test evidence. Private prompts, home paths, machine addresses, credentials and personal data stay local. The reviewing desk merges; this unit does not merge or deploy.

## Known limits

Small local models may fail to design a valid agent or solve the task. That is an exam result, not a reason to weaken the gate or substitute a hosted model. A passing small exam is narrow evidence only. No autonomous training, recursive agent search, new tool implementation, live publication, general shell access or broad superiority follows.

Visual gates are inapplicable to this nonvisual CLI/library unit. Domain checks, source review, real local inference and evidence inspection replace them.

The first code task is deliberately bounded to three atomic counter methods in a frozen real source file. Its replacement grammar permits the required atomic ordering and integer paths, and refuses imports, process APIs, output APIs, literals and macros. The native checker must launch with explicit address-space, CPU, process, output-file and wall-time limits; missing isolation or limiting tools refuse execution. A compile pass cannot rescue a truncated, failed or mismatched provider response.

The network helper enforces one total request deadline across connection, writes and reads. A stop flag prevents later work; active requests can continue until that deadline. Transport timeout errors retain an error receipt, while partial HTTP response bytes are discarded. Provider token counts are recorded when available; fallback estimates include system, user and answer text and remain labelled estimates. A meter can stop later calls after an active call overruns its remaining allowance; it does not establish an exact tokenizer-based preflight cap.

## MVP commands and receipts

`tempo-foundry` is a separate console binary. It accepts only the loopback OpenAI-compatible endpoint supplied by the owner; there is no hosted fallback.

```text
tempo-foundry design research --port PORT --model MODEL --goal TEXT
tempo-foundry design code --port PORT --model MODEL --goal TEXT
tempo-foundry design content --port PORT --model MODEL --goal TEXT
tempo-foundry exam RECIPE_ID
tempo-foundry exam RECIPE_ID --code-check CHECK_REQUEST.json
tempo-foundry exam --artifacts RUN_ID
tempo-foundry task RECIPE_ID --task-file FILE [--baseline]
tempo-foundry team --research RECIPE_ID --code RECIPE_ID --content RECIPE_ID --task-file FILE
tempo-foundry stop RUN_ID
```

`design` saves the raw model answer and a revalidated recipe; that is not a keep. `exam` runs recorded generated practice cases first, then the paired frozen exam in fresh contexts. It writes raw answers and provider bodies under `agent_data/foundry/exams/<run>/`, and references them in the chained Conductor run log. Text-role keeps require the independently scored held-out and canary gate plus a verified receipt chain and saved unapproved takes. Every qualification binds the exact recipe file hash; editing its prompt, model, endpoint, guards or budget invalidates its old keep.

Code stays `Untested` until `--code-check` runs both saved model artifacts through the bundled, hash-pinned compiler harness. The request JSON identifies the saved run, recipe and artifact hashes; imported test results cannot authorize a keep. The checker uses fixed Python and Rust tools inside a private no-network bubblewrap sandbox. It preserves compiler/test output and requires all original budget tests and five fixed overflow regressions. Missing tools or incomplete evidence refuse qualification; there is no unsandboxed fallback or execution of generated commands. This native code-check route requires Linux, Python 3, Rust via rustup, and bubblewrap; Windows code-check execution remains untested.

`exam --artifacts RUN_ID` returns the saved report, artifact paths and hashes for owner review. Research and content runs also save readable draft files beside their raw JSON answers. Task files are UTF-8 text up to 64 KiB. `task` refuses a recipe without a current kept exam receipt; `--baseline` uses the fixed baseline prompt in a fresh context before the retained candidate. A completed call means only that an output and receipt were saved; task quality remains `Untested` until a human reviews it.

`team` requires one kept recipe per role and calls the existing Conductor Sections tree. All three roles use the minimum positive token and wall-time budget among their recipes, one shared Tree meter and loopback-only routes. The adapter permits one active provider request at a time, including judging, so its host reservation covers a bounded engine workload. Each call charges at least its provider-reported usage or the Tree's existing character estimate, whichever is larger. Receipts show actual usage separately from that conservative meter. Identical model/endpoint routes share one engine identity; the existing host-owned producer prompt cannot masquerade as an independent engine. Tree take files are confined to `agent_data/foundry/teams/<run>/isolated-takes.jsonl`; their automatic winner flags are never copied into the canonical Woodshed takes path. The team result is a draft and its quality remains `Untested`.

`stop RUN_ID` creates a durable foundry stop flag. Tasks check it after a bounded request; teams poll it while Sections runs and stop the shared meter. The current loopback socket call cannot be interrupted by this flag and runs until its finite timeout (at most 90 seconds); later calls are refused. Stop flags and all failure receipts are retained for inspection.
