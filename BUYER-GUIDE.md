# Buyer guide: Perennos Tempo 2.0

Perennos Tempo 2.0.1 is a set of small apps that run on your own computer and work with a local AI engine. Nothing is sent anywhere unless you connect it yourself. What the apps keep and what leaves your computer is in [PRIVACY.md](PRIVACY.md).

## What each app does

- **Tempo Agent**: the local AI agent. It reads and writes files, runs commands (asking you first), searches your notes and works through longer jobs, and it can use the other apps as tools.
- **Tempo Vault**: your notes as plain Markdown files, compatible with Obsidian. Tempo learns from the folder you choose; nothing in it leaves the computer.
- **Tempo Studio**: mixing and mastering, with REAPER when you ask for it.
- **Tempo Room**: private chat rooms where Tempo answers when you write `@tempo`.
- **tempo-launch**: the launcher. It starts the apps, shows which models fit your computer and your goals, and sets things up after you say yes.
- **Tempo Soundcheck**: coming in 2.1.

## What you need

- Windows 10 or 11 (x64), macOS on Apple silicon, or Linux on x86_64.
- Memory: 16 GB of RAM is a comfortable start for a medium model; with 8 GB, expect to use small ones. A graphics card helps but is not required. `tempo-launch setup` shows which models fit your computer before it fetches anything.
- Disk: the apps themselves are small (tens of megabytes). Ollama and each model you choose are extra; plan for several gigabytes of free space per model.

## Ollama comes with Tempo

If no Ollama is running on your computer, Tempo can fetch its own copy (about 1.4 GB). It does this only after you agree, and it checks the download against a pinned SHA-256 before anything is unpacked or run. If the check fails, setup stops. Models are downloaded separately, with your yes, and keep their publishers' terms.

## Licence and what is separate

The Perennos-authored files are source-available under the [PolyForm Shield License 1.0.0](LICENSE): you may use, change and share them, for yourself or your business, but not to provide a product that competes with Perennos Tempo. Third-party code, runtimes and model files keep their own terms ([NOTICE.md](NOTICE.md)).

The paid Field and VECTIS are separate products with their own terms. They are not part of this download.
