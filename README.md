# Perennos Tempo

Perennos Tempo is a local-first open-source core for routing requests to local engines, applying privacy and admission rules, and recording route receipts. It includes a desktop chat, a native bootstrap source, a loopback HTTP API, headless operation, and an MCP stdio bridge.

Download Windows builds from [GitHub Releases](https://github.com/Perennos/perennos-tempo/releases). Each release records its tested Windows image, installer checksum, and acceptance results. A source snapshot alone does not establish a working installation. The installer is unsigned; multi-user security and shared office deployment are outside this core release.

Project links: [Perennos](https://perennos.com) · [Perennos Tempo](https://perennos.com/tempo).

## What is in the public core

- Generic routing, profiles, local inference, privacy screening, receipts, append-only ledgers, and resource admission/lease controls.
- Desktop chat and bootstrap source, plus the headless API, status command, and MCP stdio adapter.
- Generic profile infrastructure. The built-ins contain no customer prompts; the default is `private`, and the `offline` profile requires loopback inference.
- Apache-2.0 terms for Perennos-authored core files. Third-party source, runtimes, dependencies, and model files keep their separate terms; see [NOTICE.md](NOTICE.md).

The package excludes private onboarding interviews, customer or office profile packs, vertical-specific setup choices, desks and approvals UX, owner configuration and history, owner priors, benchmark data, model weights, the private resident adapter, and optional workspace integrations. The export manifest records its exact file set and exclusions.

## Defaults and limits

The default profile is local and has a zero hosted cost cap. Provider attachments and positive spend caps require explicit owner configuration. A public label does not anonymize content. If a hosted engine is configured, that provider receives the request; never send sensitive material to an unreviewed hosted route.

`TEMPO_HOME` separates one workspace's data; it is not an operating-system security boundary. Use a separate OS account or machine for separate owners. The API binds to `127.0.0.1`; this release does not provide a shared office server or LAN authorization. Receipts record routing and outcome metadata; they are not proof that a response is correct.

The Windows installer bundles an app-local Python/Tk runtime and preserves its upstream license. Models, Ollama, and Python dependencies download during first setup, which needs network access and several gigabytes of free storage. The source export contains no runtime binaries or model weights. The installer is unsigned; keep Windows security protections enabled. See the [Setup Guide](SETUP-GUIDE.md), [Profiles](PROFILES-README.md), [Deployment Options](DEPLOYMENT-OPTIONS.md), and [Buyer Guide](BUYER-GUIDE.md).
