# Testing Perennos Tempo

Thank you for trying Perennos Tempo early. It's a set of local AI tools that run on your own computer: an agent, a notes vault, a mixing studio and a private group chat. This page says what to try and how to tell us what you found. It takes about twenty minutes, plus download time.

## Before you start

- **What you need:** Windows 10 or 11 (x64), a Mac with Apple silicon, or Linux (x86_64). 16 GB of memory is comfortable, and 8 GB works with small models. Allow about 10 GB of free disk for Ollama and a first model.
- **The downloads are big.** Ollama is about 1.4 GB, and a model is several more. Use a connection you don't pay by the gigabyte for.
- **Windows will warn you.** The installer isn't code-signed yet, so Windows shows "Windows protected your PC". Click **More info**, then **Run anyway**. If you'd like to check the file first, compare it with `SHA256SUMS.txt` on the release page, or run `gh attestation verify <file> --repo Perennos/perennos-tempo`.
- **It's a beta.** Things will break. That's what we're looking for.

## What to try

Download the latest release: https://github.com/Perennos/perennos-tempo/releases/latest

1. **Install.** On Windows, double-click `PerennosTempo-Setup-<version>-x64-source-available.exe` and go through the wizard. Leave "Download Ollama" ticked if you don't have it. On macOS or Linux, unpack the tarball and run `./install.sh`, or install the `.deb`.
   *Tell us:* was anything unclear? How long did it take? Did anything sit still with no explanation?
2. **First start.** Open Perennos Tempo. The welcome page scans your computer and says what it can run.
   *Tell us:* was the scan right about your memory and graphics card? Was it clear what to do next?
3. **Get a model.** Run `tempo-launch setup --apply` and say yes to the plan.
4. **Ask the agent something real.** For example: "What's in my Downloads folder, and what could I clear out?", or "Summarise this file" with `@` and a file name. It asks before it changes anything; try saying no, and try `/undo` after it changes a file.
   *Tell us:* were the answers useful? Did it ever do something you didn't expect?
5. **Try one more app.** Pick whichever fits you:
   - **Tempo Vault:** point it at a folder of notes, then search them and look at the graph.
   - **Tempo Room:** make a room, invite someone with the one-time key, and both of you ask `@tempo` something.
   - **Tempo Studio:** ask it to analyse or master a song in your Music folder.
6. **Several computers? (optional)** If you have a second machine with a graphics card, run setup on it too, then `tempo-launch link <its address>` on the one you sit at.
7. **Let your own AI help (optional).** The welcome page shows how to connect Claude Desktop, Claude Code or another MCP client to `tempo-launch mcp`. Ask your AI to look at your setup and suggest settings. It can only propose changes; you apply them with `tempo-launch setup --apply-proposal`.

## How to tell us

- **Something broke:** open an issue with the "Something went wrong" form: https://github.com/Perennos/perennos-tempo/issues/new/choose
- **How it felt:** use the "How it felt" form on the same page. Short is fine.
- **A security problem:** please report it privately: https://github.com/Perennos/perennos-tempo/security/advisories/new (see [SECURITY.md](SECURITY.md)).

Please leave keys, passwords, your name, your email, personal file paths and home network addresses out of anything you post. Write `<removed>` instead.

## What we already know

- The Windows installer isn't code-signed, so Windows warns before it runs.
- Nobody has yet installed this release on an ordinary home PC and gone all the way to a first answer. You may be the first, and we'd love to hear how it went.
- Linux desktops and Room with guests on other networks have had the least testing.

## What stays on your computer

Everything, unless you set it up otherwise: your files, notes, chats and models all stay local. Nothing is sent anywhere to "phone home". [PRIVACY.md](PRIVACY.md) has the details.

To remove Tempo: on Windows use Settings, Apps; on macOS and Linux run `install.sh --uninstall`. Your vault and Tempo's memory are kept, in case you come back.
