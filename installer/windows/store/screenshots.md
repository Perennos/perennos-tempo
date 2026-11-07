# Screenshot plan for the Microsoft Store

## What Partner Center takes (as last checked, Nov 2026: the upload page shows the live rules)

| Item | Size | Notes |
| --- | --- | --- |
| Screenshots (required: at least 1; use 4 to 8, up to 10) | PNG, 1920 x 1080 (16:9). Accepted range: 1366 x 768 up to 3840 x 2160, landscape or portrait; each file at most 50 MB | A caption of up to 200 characters for each is optional and recommended |
| App tile icon (1:1) | 300 x 300 PNG | made: `art/app-tile-icon-300x300.png` |
| Box art (1:1) | 1080 x 1080 PNG | made: `art/box-art-1080x1080.png` |
| Poster art (3:4, optional) | 720 x 1080 PNG | made: `art/poster-art-720x1080.png` |
| Hero art (16:9, optional) | 1920 x 1080 PNG | not made; skip it, or take one of the screenshots below |
| Trailer (optional) | MP4, 1080p | skip it |

The three pictures marked "made" are drawn by `../msix/make_assets.py` from the Perennos mark and are ready to upload.
The screenshots must be of the real apps on Windows, so they are not made here.

## Rules for every screenshot

- Real window of the Store edition on Windows 11, light or dark, at 1920 x 1080 (set the display to that, or capture
  a window and place it on a 1920 x 1080 canvas; do not stretch it).
- **Generic data only.** Use a fresh user account named "Tempo Demo" or similar, a fresh Windows profile, and these
  names: vault `Tempo Vault`, rooms `#general` and `#projects`, people "Sam", "Mia", "Jo". No real names, no machine
  names, no folder paths with a real user name (show `C:\Users\Demo\...` or crop the path), no addresses, no tokens,
  no private notes, no real chat history.
- No model names that identify a private setup; showing "Ollama" or a public model name is fine.
- Nothing the app does not do. Do not retouch the picture.
- Captions say what the picture shows, in one plain sentence.

## The set

| # | Screen | What to show (generic) | Caption |
| --- | --- | --- | --- |
| 1 | The agent's chat page (`tempo-launch agent`) | A short conversation: "summarise the notes in my vault about the garden project", the agent's answer, and an approval card for one file action | The Perennos Tempo agent works with a model on your own computer and asks before it acts. |
| 2 | Tempo Vault, a note with its backlinks | A demo vault with 6 to 10 short sample notes about everyday things | Tempo Vault keeps your notes as plain Markdown files with links and backlinks. |
| 3 | Tempo Vault, the graph | The graph of the demo vault | The graph shows how your notes connect. |
| 4 | Tempo Room, a thread | `#projects` with Sam and Mia, one `@tempo` question and its answer in the thread | Tempo Room is a private group chat where anyone can write @tempo and get an answer in the thread. |
| 5 | Tempo Room, the invite sheet | The invite link and code with the example text | Invite people with a one-time link. Rooms run on your computer. |
| 6 | Tempo Studio, an analysis | `tempo-studio analyze` output for a sample WAV you own, in the terminal, or the Studio page if it has one | Tempo Studio measures loudness and masters songs, with a built-in meter. |
| 7 | Start menu | The Start menu showing Perennos Tempo, Tempo Vault and Tempo Room | One shortcut starts everything in the background. |

(Drop any row whose screen is not ready; one good screenshot is enough to submit.)

## Existing screens: what to regenerate

| Existing file | Use in the Store? | Why |
| --- | --- | --- |
| `installer/windows/screens/*.png` (installer wizard, 895 x 565, from Wine) | No | Too small for the Store (minimum 1366 x 768). The old ones were removed because they showed the first installer; new screenshots are taken from the v2 installer and belong in the installer README only. |
| Field screenshots from the private brand and GUI sources | No | They show the Field, which is not in the Store edition, and may hold real data. |
| Brand art pages (hero, flow, privacy diagrams; sources kept privately) | Maybe as hero art | Render a 1920 x 1080 hero from the private `hero.html` if you want one; it carries no personal data. |
| Any screenshot of the apps made on the owner's own machines | No | Real data. Regenerate every app screen with the generic data above. |

## How to take them

1. Make a clean Windows user (or a Windows Sandbox / VM), install the signed MSIX from `../msix/README.md` (the "own
   machines" path), and create the demo vault and rooms by hand with the names above.
2. Set the display to 1920 x 1080, 100% scaling.
3. Capture with Snipping Tool or `Win+Shift+S`, then save as PNG. Check each file's size in its properties.
4. Look at every picture at full size once before you upload it, for anything personal.
