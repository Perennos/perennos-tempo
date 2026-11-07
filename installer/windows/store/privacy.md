# Perennos Tempo privacy policy

Effective 6 November 2026. Published by Perennos, built and maintained in the Cayman Islands.

**Short version: Perennos Tempo runs on your computer and keeps your data there. We do not collect it. There is no
account, no advertising, no analytics and no telemetry. Nothing is sent anywhere unless you connect it.**

## What Perennos Tempo is

Perennos Tempo is a set of programs that run on your own computer (Windows, macOS or Linux): an AI agent, Tempo Vault
(notes), Tempo Room (a private group chat) and Tempo Studio (music mixing and mastering). They talk to each other on
your computer.

## What is stored, and where

Everything is stored on your computer, in files you can open, copy and delete:

- Your notes (Tempo Vault): the folder you chose (on Windows, by default `Documents\Tempo Vault`).
- Conversations with the agent, its memory, Tempo Room's rooms and messages, settings, and logs of what the programs
  did: the Tempo home folder, `%LOCALAPPDATA%\Perennos Tempo` on Windows and `~/.tempo` on macOS and Linux (or the
  folder you set with `TEMPO_HOME`).
- Tempo Room keeps the display names of the people you invite and their session records; invite codes are kept only
  as hashes.

Perennos never receives any of it. Uninstalling the app does not delete your notes or data, so you never lose them
by removing the app; you can delete these folders yourself at any time.

## What Perennos Tempo does not do

- It does not collect, upload, sell or share your data with Perennos or anyone else.
- It does not send usage statistics, crash reports or any "telemetry".
- It has no account or sign-in with Perennos, and shows no advertising.

## What leaves your computer, only if you choose it

Perennos Tempo's programs listen only on your own computer (127.0.0.1) unless you change that. The following reach other
computers only because you set them up or ask for them:

- **A model host.** The agent sends your messages and the files or notes it is working with to the model host you
  configure. By default that is Ollama on your own computer. If you point it at another machine, your messages go to
  that machine. If you point it at an online model service, your messages go to that service and its privacy policy applies.
- **Web pages.** If you ask the agent to read a web page, it fetches that page. It asks you first for each new site.
- **Tool servers you add (MCP).** A tool server you add is a program you chose; what it sends over the internet is
  decided by that program.
- **Bridges.** If you turn on a Discord, Telegram or WhatsApp bridge for Tempo Room, the messages of that room pass
  through that service, under its own terms and privacy policy. Bridges are off until you set them up.
- **Other people in your rooms.** Tempo Room is private: only people you invite can join, with a one-time link. Messages
  are visible to the members of that room. If you let others reach your computer over Tailscale or your network, their
  messages travel over that network.
- **Models and Ollama.** Perennos Tempo includes no model. When you run its setup, it looks up the names of the
  models it might recommend in the public Ollama registry (only those model names are sent). Only when you say yes does
  it ask Ollama to download the chosen models (from the Ollama registry or Hugging Face) and, if this computer has no
  Ollama, fetch Ollama itself from Ollama's official GitHub release, checked against a pinned fingerprint (SHA-256)
  before it is used. Those downloads come from those services, and their policies apply.
- **REAPER.** If you ask Tempo Studio to open REAPER, it starts that separate program on your computer.

## What the agent can do on your computer

At your request, and asking for approval first for actions that matter, the agent can read and change files and run
programs on your computer. It only does this inside the conversations you start, and it keeps a record of what it did
in your own data folder.

## Children

Perennos Tempo is a general-purpose tool and is not directed at children. We do not knowingly collect information from
anyone, children included, because we collect none.

## Changes

If this policy changes, the new version will be published at this address with a new effective date.

## Contact

Perennos: https://perennos.com
