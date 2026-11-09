# Perennos Tempo for Windows: the installer

One `PerennosTempo-Setup-<version>-x64.exe` installs the whole suite: the agent and the apps it connects to
as its tools. It is built with [Inno Setup](https://jrsoftware.org/isinfo.php) 6.4, the installer Visual
Studio Code ships with, in its modern wizard style, with the Perennos art of the README (the lattice body)
rendered for every display scaling from 100% to 250%.

There are two editions, both from the same script:

| Edition | File | What is in it | License page |
| --- | --- | --- | --- |
| Full (the main one) | `PerennosTempo-Setup-<v>-x64.exe` | the agent, Vault, Studio, Room, the Field, the `tempo` command with VECTIS, `tempo-launch` | 1. PolyForm Shield 1.0.0 for the agent, Vault, Studio, Room and `tempo-launch`; 2. the proprietary notice for the Field and `tempo` with VECTIS; 3. the third-party notices |
| Source-available | `PerennosTempo-Setup-<v>-x64-source-available.exe` | the agent, Vault, Studio, Room, `tempo-launch` | 1. PolyForm Shield 1.0.0; 2. the third-party notices |

The PolyForm Shield part starts with the "Required Notice" and "Licensor Line of Business" lines of the public
repository's `NOTICE.md`, as the license asks. The texts are in `legal/`: `PolyForm-Shield-1.0.0.txt` (the
public repository's `LICENSE`; every build checks its SHA-256, `56328093...a91`), `PolyForm-Shield-notice.txt`,
and, for the full edition only, `Tempo-Field-and-VECTIS.txt`, its proprietary notice (it is not in the public
repository, which builds the source-available edition). Each edition also installs its texts in `legal\` next to the programs. Both editions share
one AppId, so installing the full edition over the source-available one adds the Field and `tempo`.

| Page | What it asks |
| --- | --- |
| Install mode | for me only (the default: no administrator prompt) or for all users |
| Welcome | nothing: what Perennos Tempo is, and that everything stays on this computer |
| License | the edition's terms, above; "I accept" goes on |
| Destination | the folder (first install only; updates go where it already is) |
| Components | Everything, the agent and Tempo Vault, or a choice |
| Your vault | the notes folder: a new one, or an Obsidian vault you already have (left as it is) |
| Additional tasks | desktop shortcut, "Open as Tempo Vault" in File Explorer, PATH, hourly learning, downloading models now (off unless ticked: several gigabytes, needs Ollama, looks up model names in Ollama's registry), start at sign-in |
| Ready | a summary that includes the vault and where Tempo keeps its memory |
| Finished | "Perennos Tempo is installed", where the vault is, whether Ollama was found (if not, https://ollama.com/download), and "Open Perennos Tempo" |

## What goes where

| | For me only | For all users |
| --- | --- | --- |
| Programs | `%LOCALAPPDATA%\Programs\Perennos Tempo\bin` | `C:\Program Files\Perennos Tempo\bin` |
| Start menu | `Perennos Tempo` folder: Perennos Tempo, Tempo Vault, Tempo Studio, Tempo Room | the same, for everyone |
| PATH (if ticked) | the user's | the machine's |
| "Open as Tempo Vault" (if ticked) | `HKCU\Software\Classes\Directory` | `HKLM\Software\Classes\Directory` |
| Uninstall entry | Settings, Apps: Perennos Tempo (the source-available build says so), by Perennos, with its icon and size | the same |

For the user who installs it, Setup also sets `TEMPO_VAULT` (the vault folder) and `TEMPO_AGENT_DATA`
(`<the Tempo home>\agent`: `TEMPO_HOME` if set, else the `TEMPO_LEGACY_HOME` folder if it exists, else `%LOCALAPPDATA%\Perennos Tempo`; the same rule as `tempo-launch`, see `docs/TEMPO-HOME.md`), only where they are not set already, so the agent finds the vault as
a tool and terminals find the same data. Hourly learning is the scheduled task "Perennos Tempo Vault Learn"
(the same name `tempo-vault schedule` uses, so there is only ever one). Start at sign-in is
`HKCU\...\Run\Perennos Tempo` running `tempo-launch start`.

Ollama: the "Download Ollama" box (about 1.4 GB) is ticked by default only when no Ollama is found (for this
user, in `Program Files\Ollama`, or on PATH); ticking it is the consent. Setup then runs
`tempo-launch setup --ollama --yes` hidden, which fetches only Ollama, checks it against its pinned SHA-256
before unpacking it, and starts it once. It never fetches a model. Silent installs tick nothing unless
`/TASKS="getollama"` is given.

Models: Setup downloads no model unless "Download models that fit this computer now" is ticked, and it is
not ticked by default (it says several gigabytes). Ticked with an Ollama there (found, or just set up by the
Ollama box), Setup runs `tempo-launch setup --apply --yes` hidden. In every other case it
runs only `tempo-launch setup --page`, which writes the plan to `<the Tempo home>\setup.html` and downloads
nothing; Perennos Tempo opens that page at its first start.

The Tempo Studio shortcut runs `tempo-launch studio`, which opens Tempo, where Studio's tools are used. It carries
`tempo.ico`: the build makes no separate Studio icon.

Every shortcut runs `tempo-launch.exe`, a program without a console window. It starts an app's server
hidden if it is not running, waits until it answers, and opens it in the browser. Each server's output goes
to `<the Tempo home>\logs\<app>.log` (usually `%LOCALAPPDATA%\Perennos Tempo\logs`). If an app cannot start, a page says why, with the last
lines of its log (keys hidden). `tempo-launch stop` ends what it started, and `tempo-launch status` shows
what runs. See `perennos-tempo-launcher/README.md`.

## Updates and uninstalling

Running a newer Setup updates in place: it stops the running apps first (`tempo-launch stop`), does not ask
for the folder again, and offers every earlier choice as the answer. Running Setup again is also how apps are
added or taken away: an app unticked there has its program removed.

Uninstalling stops the apps, then removes the programs, shortcuts, the PATH entry, the Explorer command, the
scheduled task, the sign-in entry, and the two variables if Setup made them. It never touches the vault,
Tempo's memory or the logs: an interactive uninstall ends by saying where they are, so they can be removed by
hand if wanted.

## Building

The build compiles every product from source, so the installer always matches the code.

### On Linux (what CI and this repository's tests use)

Needs Rust with `rustup target add x86_64-pc-windows-gnu`, the MinGW-w64 linker
(`apt install gcc-mingw-w64-x86-64`), Python 3 (standard library only), Wine (64- and 32-bit:
`apt install wine64 wine32:i386`), and Inno Setup's compiler, `ISCC.exe`, 6.4 or later. Two ways to get it:

- the official installer from https://jrsoftware.org/isdl.php, installed under Wine; or
- the `innosetup` npm package (it carries `bin/ISCC.exe`; it is the package VS Code's own build uses).
  Version 6.4.1's `ISCC.exe` has SHA-256 `3a34c0beaf8c9c66774c1c1286a504db403e97e1b2fba691b3ba865dbce7edb2`;
  check it before use.

Then:

```bash
ISCC=/path/to/ISCC.exe installer/windows/build.sh
```

The products are taken from this repository's crate folders. To build one from somewhere else (a branch
checked out next to this one), point `TEMPO_SRC_AGENT`, `TEMPO_SRC_VAULT`, `TEMPO_SRC_STUDIO`,
`TEMPO_SRC_ROOM`, `TEMPO_SRC_FIELD` or `TEMPO_SRC_CLI` at its crate. The launcher and the agent are required;
an app whose crate is not there is left out of that installer, and the wizard does not offer it.
`TEMPO_BUILD_DIR` moves the build folder (default `installer/windows/build`, which git ignores).
`TEMPO_LICENSE_FILE` (a `.txt` or `.rtf`) puts another license page in place of the one the build makes.

`TEMPO_APPS` chooses the apps (the launcher and the agent are always in): `vault studio room field cli`, the
full edition, by default. The source-available edition leaves out the paid Field and VECTIS (`tempo`):

```bash
TEMPO_APPS="vault studio room" ISCC=/path/to/ISCC.exe installer/windows/build.sh
```

Its file name ends in `-source-available`, so both editions can sit in the same `build/out`. Without the
Field, the Perennos Tempo shortcut opens Tempo Vault.

### On Windows

Needs Rust (MSVC toolchain; the Windows SDK's `rc.exe` adds the launcher's icon and version details),
Python 3 and Inno Setup 6.4 or later.

```powershell
.\installer\windows\build.ps1
```

`-Iscc <path>` if Inno Setup is somewhere else, `-Target x86_64-pc-windows-gnu` for the GNU toolchain,
`-LicenseFile <file>` for another license page, `-Apps vault,studio,room` for the source-available edition.

### What a build does

1. Draws the art: `art/make_art.py` (standard library only, the same files every run) makes the Perennos mark
   as `tempo.ico`, `tempo-vault.ico` and `tempo-room.ico` (16 to 256 pixels; the same P, each app's dot in its
   own colour from the products' palette) and the small wizard image, and turns `art/wizard-large-<pct>.png`
   into the large wizard images at Inno Setup's seven sizes. Those PNGs are the README's lattice look,
   rendered from brand art sources that are kept privately (the same picture at every size): to redraw them,
   follow `art/finish_wizard_art.py` (Playwright and Pillow; the build itself needs neither).
2. Compiles the programs (`--release --locked`), each into a fresh `build/stage/<version>-<time>` folder.
3. Writes `legal/THIRD-PARTY-NOTICES.txt` (`notices.py`: every crate built into the programs, with the license
   text it ships), the edition's license page (`license_page.py`: one RTF, a heading per part), and copies the
   example configs (the owner's name replaced by "Your name").
4. Compiles `PerennosTempo.iss`. LZMA2 "ultra64" compression runs in Inno Setup's 64-bit helper
   (`LZMAUseSeparateProcess`): the 32-bit compiler does not have the memory for its dictionary.
5. Writes the installer's `.sha256`.

## Installing without the wizard

For IT tools and scripts (Inno Setup's standard switches, plus `/VAULT`):

```bat
PerennosTempo-Setup-2.0.1-x64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER /VAULT="D:\Notes" /LOG="%TEMP%\perennos-tempo-setup.log"
```

| Switch | |
| --- | --- |
| `/CURRENTUSER` or `/ALLUSERS` | who it is for (default: the current user) |
| `/DIR="..."` | the program folder |
| `/COMPONENTS="agent,vault,studio,room,field,cli"` | what to install (the agent is always installed) |
| `/TASKS="openvault,addtopath,learn"` | exactly these tasks; `/MERGETASKS="desktopicon,startup"` adds to the defaults. `setupmodels` (download models now) is not a default: name it, as in `/MERGETASKS="setupmodels"`, to opt in |
| `/JOIN=4-tiger-violet` | after installing, join the main computer that showed this code (`tempo-launch fleet join`). Nothing joins unless it is given, so a silent install stays unattended |
| `/VAULT="D:\Notes"` | the vault folder (default: the last one, else `TEMPO_VAULT`, else `Documents\Tempo Vault`) |
| `/LOG="file"` | Setup's log |

Uninstall: `"%LOCALAPPDATA%\Programs\Perennos Tempo\uninstall\unins000.exe" /VERYSILENT /SUPPRESSMSGBOXES`
(Program Files for an all-users install).

## Signing

Unsigned, Windows SmartScreen says "Windows protected your PC" the first times Setup is run. Signing needs a
code-signing certificate; with one, define a sign tool and pass it:

```bat
ISCC /Stempo="signtool.exe sign /a /fd sha256 /tr http://timestamp.digicert.com /td sha256 $f" /DSignTool=tempo ...
```

`build.ps1 -SignTool '<command with $f>'` does this. Setup, the uninstaller and every program are then signed.

## Tested

Under Wine 9.0 on Linux, with the real Windows programs: a silent install for the current user and for all
users; an install into a profile that already had a PATH and an Obsidian vault (PATH appended with its
variables kept, the vault left untouched); an update over a running vault server (stopped first, no PATH
duplicate, choices kept); `tempo-launch` starting the agent, the vault, the room and the Field hidden, the agent
connecting Tempo Vault, Tempo Studio and Tempo Room as tools, and `stop` ending all of them; "Open as Tempo
Vault" on a new folder; uninstalling (everything Setup made removed, the user's PATH restored exactly, the
vault and Tempo's data kept); both editions built, installed and uninstalled, with their license pages; and
every wizard page of the full edition, driven and photographed. The old screenshots showed the first installer and were removed; new ones are taken from the v2 installer. Not testable under Wine: the
hourly task (Wine's `schtasks` has no `/TR`), SmartScreen and signing, and Windows' own look. Those need a run
on a real Windows 10 or 11 machine.
