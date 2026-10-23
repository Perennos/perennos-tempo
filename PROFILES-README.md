# Perennos Tempo profiles

Profiles are generic routing policy, not customer prompt packs. The public core keeps the profile validator and privacy boundary because they are runtime safeguards. There are no customer interview prompts or paid office packs in this release.

Each installation stores `profiles.json` in its own `TEMPO_HOME`. A fresh gateway creates the five generic built-ins without overwriting an existing file:

| Profile | Default behavior |
|---|---|
| `private` | Default profile; hosted routing is denied. |
| `design-studio` | Generic public-work profile; hosted classes are listed but the default per-job cap is zero. |
| `frontier-assist` | Generic public-work profile; hosted classes are listed but the default per-job cap is zero. |
| `offline` | Hosted routing is denied and local inference must use loopback. |
| `custom` | Empty policy slot with no prompt pack or hosted permission by default. |

A public `design-studio` workflow mapping exists for compatibility. It does not bypass the zero cost cap, missing-provider defaults, screen, engine qualification, or privacy checks. An owner must explicitly configure any hosted provider and positive cap. No provider is selected automatically, and unknown or missing privacy labels stay local.

## Privacy boundaries

Selecting a profile does not declassify a request. Private labels, configured local-only classes, a screen hit, or a previously private conversation stay local. The `private` profile prevents hosted routing; its local endpoint can be a configured private network address. Choose `offline` when the endpoint must be loopback-only. A public profile or label is not anonymization.

The gateway screens request text and keeps declared attachments local because this text API does not provide a trusted file-ingestion boundary. A configured hosted provider receives any request that passes the explicit profile and policy gates. Review provider retention and current prices before enabling one. The cost cap is a local policy control, not provider invoice verification. Hosted outputs still need human review.

`TEMPO_HOME` separates workspaces but is not an OS isolation boundary. Use separate OS accounts or machines for separate owners. This source candidate has not established privacy-screen accuracy or customer acceptance.

## Receipts and priors

Receipts include route metadata and screen result codes; they do not certify that an answer is safe or correct. The public setup starts with neutral Beta(1,1) priors. It does not include owner-specific prior maps, leaderboard data, or a benchmark-prior builder.
