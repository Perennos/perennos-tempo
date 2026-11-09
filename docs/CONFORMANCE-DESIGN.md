# Perennos Tempo: evidence for Answerable AI

Conformance mode assembles the records for one AI use. It tells you what was
recorded, where the evidence came from, and what still needs a person. It does
not issue a compliance certificate or decide which law applies to your firm.

The Caymanian Integrated Systems edition follows [Cayman Considerations for
Answerable AI, version 1.0](https://perennos.com/cayman-considerations/ai-governance/),
current to 5 November 2026. The general edition keeps the same evidence structure
without implying that Cayman requirements apply to another jurisdiction.

## The choice

Three approaches serve different purposes:

| Approach | What it gives you | Limit |
| --- | --- | --- |
| Source map | A requirement linked to the code that can support it | Code alone says nothing about a particular use or its operation |
| Per-use evidence report, selected | A dated, local export of the records and the gaps for one registered use | A person must establish responsibility, lawful grounds, contracts and review |
| Automatic enforcement across every tool, higher risk | A possible future gate that prevents use without approved records | Requires authority and integration at every execution path; an exporter cannot safely imply it exists |

A self-scoring compliance dashboard was rejected: a percentage would hide the
facts that only a responsible person can establish.

## One command, one report

```text
tempo-launch conformance --case CASE --out NEW_DIRECTORY --edition cayman
```

The command reads the fixed evidence files for that case and the known local
model configuration. It makes no network request, asks no model, and changes no
running service. It writes a new output directory and refuses to replace an
existing one. Unix output uses private permissions. Windows output inherits its
parent permissions; their suitability must be checked independently. The
human-readable entry point is `report.md`; structured evidence and a completion
marker sit beside it.

The report answers six practical questions:

| Record | What the report must distinguish |
| --- | --- |
| Signed receipt export | Recorded bytes, chain integrity, signer identity and the source of trust are separate checks |
| Data-flow statement | The declared input, processing, storage, logs, backups, support access, countries and fallback routes |
| Model and version register | A configured model name, an observed artifact digest and the version that actually ran are different facts |
| Human sign-off records | A recorded decision and a reference to its approver; an agent setting never supplies human authority |
| Incident and override log | Discovery, reasonable awareness, actions, reversals and notification records, with separate clocks |
| Evaluation and canary record | The exact model and test identities, baseline, candidate, canary, review and rollback references |

The full clock file also needs identity, scope, approval, data conditions,
supplier terms, fairness, retention, challenge, reversal and rehearsal records.
The requirement matrix maps each field and control to an evidence path. Blank
fields remain visible. A document reference is a pointer for the reviewer, not
proof that its contents were independently validated.

## Evidence boundaries

The three status labels describe evidence, not permission to use a system:

- `Verified complete` needs a reproducible check and its supporting artifact.
- `In progress` identifies an incomplete record or a finding that needs review.
- `Untested` identifies an absent record or a check that has not been performed.

Present human-supplied records remain assertions until independently checked.
A file hash binds bytes; it does not establish the author's identity or the
truth of the record. A model alias does not establish a model artifact version.
An intact receipt chain does not prove that every action was recorded.

Signed receipts reuse the existing receipt system's signature work. Until that
verifier is available and bound to a trusted node identity, the report must say
that signed verification is `Untested`. A supplied `signed` or `verified` flag
cannot change that result. This mode adds no second signing system.

## What needs a person

Software cannot establish that an owner is in the Cayman Islands, that a deputy
has real authority, that a processing condition applies, that a supplier is
bound by a contract, or that a reviewer understood a decision. It can retain
references to the records supporting those facts and show what is missing.

The standard distinguishes law, regulator rules, regulator guidance, government
statements and Perennos recommendations. The matrix preserves those boundaries.
Its incident clocks have different triggers, recipients and scopes. Generating
a report neither sends a notification nor extends a deadline. A responsible
person must resolve applicability and preserve evidence without waiting for the
export to be complete.

## Small, inspectable boundary

Inputs are fixed, bounded JSON files. Unknown fields and content-bearing fields
are not copied into the export. Duplicate keys, unsafe paths, links, wrong
types and oversized files must be rejected or reported explicitly. Error text
must not echo rejected values. Interrupted output has no valid completion
marker, and a retry uses a new directory.

Conformance mode does not turn on hosted processing, change model routing,
grant tool authority, notify a regulator, or approve a live use. The matrix is
the route from a requirement to its evidence; independent review is the route
from that evidence to a conclusion.

## Check the matrix

Run `python tests/check_conformance_matrix.py .` from the repository root. This checks record coverage and source anchors; it does not establish legal compliance.
