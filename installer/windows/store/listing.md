# Perennos Tempo: Microsoft Store listing text

Paste these into Partner Center, one box each. Everything here describes what the Store edition really does
(the source-available edition: the agent, Tempo Vault, Tempo Room, Tempo Studio and the launcher). It leaves out the
Field and VECTIS, which are not in that edition. If a feature is not on this page, do not add it to the listing
unless it works in the package you upload.

Limits below are Partner Center's as last checked (Nov 2026); the page shows the live counters, so trust those if they differ.
No personal names, machine names or private paths belong in the listing, the screenshots or the privacy page.

## Product name (reserve this exactly)

    Perennos Tempo

## Short description (up to 1,000 characters; shown in search results)

    A private AI agent and notes app that runs on your own computer. Perennos Tempo works with the models you run yourself, keeps your notes as plain Markdown files, and does not send anything anywhere unless you connect it.

(220 characters)

## Description (up to 10,000 characters)

    Perennos Tempo runs on your own computer. It is a small AI agent, a notes app, a private group chat and a music mixing helper that work together, with no account and nothing uploaded by default.

    The agent talks to the models you run yourself (it is built for Ollama on this computer or another machine you choose) and can use tools: read and change files, run programs, read a web page, and use any MCP tool servers you add. It asks you before it does anything that matters, keeps a running plan for each conversation, and is built to keep working when the internet is down.

    Tempo Vault is your notes. A vault is a normal folder of Markdown files, so you always own them. Vault has links and backlinks, a graph, search, daily notes, templates and tasks, and keeps a history of every change. An Obsidian vault you already have opens as it is. The agent can read your vault as one of its tools.

    Tempo Room is a private group chat for the people you invite. Anyone can write @tempo and Perennos Tempo answers in the thread. Rooms run on your computer and only listen on it until you choose otherwise; to let other people join you share a one-time invite link over a network you set up, such as Tailscale or your home network. If you want, you can bridge a room to Discord, Telegram or WhatsApp.

    Tempo Studio mixes and masters songs. It has a built-in loudness meter and mastering, and it can open REAPER (a separate program you own) when you ask it to.

    One shortcut opens everything. Programs start in the background, with no console windows, and a plain page tells you if something cannot start and why.

    What you need: Windows 10 (version 2004) or Windows 11, 64-bit. To get answers from the agent you need a model host such as Ollama, which is a separate free download. Perennos Tempo does not include or download a model by itself.

    Your data stays in folders on your computer: your notes in Documents\Tempo Vault (or the folder you pick) and everything else in %LOCALAPPDATA%\Perennos Tempo. Uninstalling the app leaves your notes and data in place.

    Perennos Tempo has no ads, no tracking and no telemetry. The privacy policy says exactly what stays on your computer and what leaves it only if you connect it.

    Built and maintained in the Cayman Islands by Perennos.

## Features (up to 20 bullets, 200 characters each)

1. A local AI agent that uses tools, asks before it acts, and keeps working offline
2. Works with the models you run yourself, such as Ollama on this computer or another one you choose
3. Tempo Vault: your notes as plain Markdown files, with links, backlinks, a graph, search and daily notes
4. Opens an existing Obsidian vault as it is
5. Every change to a note is kept in a history you can look back through
6. Tempo Room: a private group chat where anyone can write @tempo and get an answer in the thread
7. Optional Discord, Telegram and WhatsApp bridges for Tempo Room (off until you set them up)
8. Tempo Studio: mix and master songs with a built-in loudness meter, or open REAPER if you have it
9. Add your own MCP tool servers to give the agent more tools
10. No account, no ads, no telemetry: nothing leaves your computer unless you connect it

## Search terms (up to 7, 30 characters each)

    local AI agent
    Ollama
    Markdown notes
    offline AI
    private chat
    MCP
    mastering

## Category

- Primary: Productivity (pick the subcategory that mentions personal productivity or notes, if one is shown).
- Alternative: Developer tools, if Partner Center's Productivity choices fit badly.
- Do not choose Music: Tempo Studio is one part of a larger app.

## Copyright and trademark info (up to 200 characters)

    Copyright 2026 Perennos. Perennos Tempo is a trademark of Perennos. Other names (Ollama, Obsidian, REAPER, Discord, Telegram, WhatsApp) belong to their owners.

## Additional license terms

    The open parts of Perennos Tempo are licensed under the PolyForm Shield License 1.0.0. The full text is in the app's legal folder and at the link Perennos gives on its website.

(This is the Store edition: the PolyForm Shield parts and third-party notices only. Have a lawyer read it before anything is sold.)

## Website and support

- Website: https://perennos.com
- Support contact: use a mailbox Perennos reads. Partner Center asks for an email address or a web page; give a real one before you submit.
- Privacy policy URL: the public address where you host `privacy.html` (see `checklist.md`). Required.

## Release notes (up to 1,500 characters): the template

    What's new in Perennos Tempo <VERSION>

    - <one plain sentence about each change a person would notice>
    - <fix: what used to go wrong, and that it now works>

    Your notes and data are not touched by updates.

## What's new: the first release

    Perennos Tempo 1.0
    - First release in the Microsoft Store.
    - One shortcut opens the agent, Tempo Vault, Tempo Room and Tempo Studio.
    - Command names (tempo-launch, tempo-local-agent, tempo-vault, tempo-room, tempo-studio) work in any terminal without editing PATH.
    - Optional: start Perennos Tempo in the background when you sign in (Settings, Apps, Startup).

(Use this only if the version you upload really is 1.0. If it is another version, say that number.)

## Notes for certification (the box Microsoft's testers read)

The package declares two restricted capabilities. Paste this, adjusted to the final build:

    runFullTrust: Perennos Tempo is a desktop program package (a Win32 app packaged as MSIX). It is made of several native
    Rust programs (an agent, a notes server, a chat server and a mixing tool) that run as normal processes at the user's
    own permission level: no administrator rights, no services, no drivers. They start each other and read and write
    the user's own files, which is their purpose, so they cannot run in an app container.

    unvirtualizedResources (with write virtualization turned off): the app's data folder is %LOCALAPPDATA%\Perennos Tempo.
    The same folder is also used by the command-line tools in a terminal (the app registers command names such as
    tempo-launch) and by the user's own scripts, so all of them must see one real folder. It must also survive an
    uninstall and a reinstall: it holds the user's conversations, memory and chat rooms, which the user must never lose
    by removing the app. With virtualization on, that data would be hidden in a package-private copy that Windows deletes
    on uninstall. The app does not write to any other protected location. (User notes live in Documents\Tempo Vault,
    which Windows does not virtualize.)

    How to test: install, open "Perennos Tempo" from Start. A browser tab opens on this computer (127.0.0.1) with the
    agent. Open "Tempo Vault" from Start for the notes app. No sign-in is needed. Without a local model host the agent
    explains that it cannot answer yet, which is expected: the app does not include a model.

If the Store refuses `unvirtualizedResources`, see `../msix/README.md`, "If the Store says no".

## Age rating (IARC questionnaire): honest answers

Partner Center runs the International Age Rating Coalition questionnaire. Answer it for what the app is, not for what scores
best. The questions change; these are the answers that apply today, and why.

| Topic | Answer | Why |
| --- | --- | --- |
| Violence, sex, language, drugs, gambling written into the app | None | The app itself ships no such content. |
| In-app purchases, ads, real-money gambling | No | None exist. |
| Users can interact or communicate with each other | Yes | Tempo Room is a group chat. It is private: only people who were invited can join; there is no public room, no search for strangers and no profile page. Answer that interaction is with people you invite, and that it can be bridged to Discord, Telegram or WhatsApp if the owner sets that up. |
| Users can share content they create | Yes, but not publicly by default | Messages and files are shared inside the invited room. Nothing is published to the public by default. |
| Shares the user's location with others | No | |
| Gives access to the internet or web pages | Yes | The agent can read a web page when you ask (it asks first for each new site). |
| Uses generative AI to make content | Yes | Answers come from the model the person chooses to run. The app ships no model. Say so plainly: what the model writes depends on the model, and the app cannot promise it is free of mature content. |
| Personal information collected by you (the publisher) | None | See the privacy policy: no telemetry, nothing sent to Perennos. |

Expect a result that reflects open-ended AI chat and user communication (often 12+ or 16+, depending on the region's board).
Accept the rating you are given. Do not change an answer to lower it: a wrong answer can get the app removed.

## Pricing and availability

- Price: Free.
- Markets: all.
- Visibility: Public. Schedule: as soon as certified, or hold it with a "Publish manually" setting so you pick the moment.
