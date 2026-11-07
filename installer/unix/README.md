# Perennos Tempo for Linux and macOS: the installers

Free, no administrator rights, no paid certificate, nothing uploaded anywhere. One script, `install.sh`,
installs Perennos Tempo for you on Linux and macOS (Intel and Apple Silicon, x86_64 and ARM Linux). It has the
same two editions as the Windows installer (`../windows/`):

| Edition | Programs |
| --- | --- |
| Full (the main one) | `tempo-launch`, `tempo-local-agent` (the agent), `tempo-vault`, `tempo-studio`, `tempo-room`, plus the Field (`tempo-dashboard`) and the `tempo` command with VECTIS |
| Source-available | the same without the Field and `tempo` |

The full edition needs the private crates (`perennos-tempo-dashboard`, `perennos-tempo-cli`)
next to the others; without them `install.sh` picks the source-available edition. Licences: PolyForm Shield 1.0.0
for the open parts; the Field and `tempo` with VECTIS are proprietary (their notice ships only with the full
edition, which the public repository does not build). They are installed into
`~/.local/share/perennos-tempo/legal/`.

## Install from source (inside a checkout)

```bash
git clone <the repository> && cd Perennos
installer/unix/install.sh                    # builds with cargo and installs
installer/unix/install.sh --add-to-path --learn --autostart
```

`--from-source [repo]` is the default inside a checkout: it runs `cargo build --release --locked` in each crate
folder (so each crate's own `Cargo.lock` is used) and honours `CARGO_TARGET_DIR` if you set one (a second build
is then almost instant). A crate that does not build is reported and the others still install; a missing
prerequisite stops only what needs it:

| Needs | For | Fix it prints |
| --- | --- | --- |
| Rust 1.85 or newer (`cargo`, `rustc`) | everything | install from https://rustup.rs, or `rustup update stable` |
| a C compiler | linking every program; `ring` in the agent and Room | Debian/Ubuntu `sudo apt install build-essential`; Fedora `sudo dnf install gcc`; Arch `sudo pacman -S base-devel`; Alpine `sudo apk add build-base`; macOS `xcode-select --install` |
| `pkg-config` and OpenSSL headers | only `--with-mcp` (the Tempo MCP server, `perennos-tempo`) | Debian/Ubuntu `sudo apt install pkg-config libssl-dev`; macOS `brew install pkg-config openssl@3` |

## Install from a release

```bash
tar xzf perennos-tempo-full-2.0.0-x86_64-unknown-linux-gnu.tar.gz && cd perennos-tempo-full-*/ && ./install.sh
# or, one step, with the checksum checked first:
installer/unix/install.sh --from-release perennos-tempo-full-2.0.0-x86_64-unknown-linux-gnu.tar.gz
installer/unix/install.sh --from-release https://example.invalid/perennos-tempo-...tar.gz --sha256 <64 hex digits>
```

`--from-release` takes a file or an `https://` URL. The SHA-256 comes from `--sha256` or from `<file>.sha256` next
to it (for a URL, `<url>.sha256`). With neither, or with a mismatch, it refuses and changes nothing. It also
refuses a tarball built for another platform and a tarball with unsafe paths. `build-release.sh` makes the
tarballs (below). Nothing is downloaded unless you give a URL, and the script never pipes anything into a shell.

## Options

| Option | |
| --- | --- |
| `--edition full\|source-available` | default: full when the private crates (or a full tarball) are there |
| `--prefix DIR` | programs go in `DIR` (default `~/.local/bin`). Letters, digits, space and `_ . / @ + , : -` only |
| `--add-to-path` | appends one line, ending in `# perennos-tempo-installer`, to `~/.bashrc` (`~/.bash_profile` on macOS, `~/.zshrc` for zsh, else `~/.profile`); a copy of the file goes to the trash first |
| `--autostart` | Linux: a systemd user unit `perennos-tempo.service` if `systemctl --user` works, else `~/.config/autostart/perennos-tempo.desktop`; macOS: the LaunchAgent `com.perennos.tempo`. Both run `tempo-launch start`; `stop` stops them |
| `--learn` | hourly learning from your vault, through `tempo-launch schedule` (cron on Linux, launchd on macOS) |
| `--setup` | runs `tempo-launch setup`: shows which models fit this computer and your goals, then asks `y/N` before it fetches them (through Ollama) and sets up the agent; `--yes` skips the question. Nothing is fetched without it. See `docs/TEMPO-SETUP.md` |
| `--with-mcp` | also the Tempo MCP server |
| `--no-desktop` | no menu entries and no app bundle |
| `--dry-run` | say what would happen, change nothing (it still checks the SHA-256 of a release) |
| `-y`, `--yes` | do not ask (needed when there is no terminal) |
| `--uninstall` | see below |

Output is coloured only in a terminal. It ends with what was installed, where, and how to start it.

## What goes where

| | Linux | macOS |
| --- | --- | --- |
| Programs | `~/.local/bin` | `~/.local/bin` |
| Menu | `~/.local/share/applications/perennos-tempo*.desktop`: Perennos Tempo (runs `tempo-launch`, which starts the apps and opens them), Tempo Vault, Tempo Room, and the Tempo Field in the full edition | `~/Applications/Perennos Tempo.app` (a plain bundle: `Info.plist`, a launcher script that runs `tempo-launch`, an `.icns`) |
| Icons | `~/.local/share/icons/hicolor/<16..256>x<..>/apps/perennos-tempo*.png`; `update-desktop-database` and `gtk-update-icon-cache` run if they exist | inside the app |
| Licences | `~/.local/share/perennos-tempo/legal/` | the same |
| Install record, trash | `<Tempo home>/install/manifest.tsv`, `<Tempo home>/trash/<date>/` | the same |

`<Tempo home>` is what `tempo-launch home --json` reports for the installed launcher, else `$TEMPO_LAUNCH_HOME`,
else `~/.tempo`. Tempo Studio is a tool of the agent with no window of its own (as on Windows), so it has no menu
entry. `XDG_DATA_HOME` and `XDG_CONFIG_HOME` are honoured.

## Upgrading and uninstalling

- **Upgrade**: run the installer again. It stops what `tempo-launch` started, moves the programs, menu entries,
  icons and texts of the old install to `<home>/trash/<date>/upgrade-<old version>/`, and installs the new
  ones. A PATH line, autostart and schedule from before stay. Changing edition works the same way.
- **Uninstall**: `install.sh --uninstall` (same `--prefix` if you used one). It removes the schedule, stops the
  apps, and moves exactly the files in the manifest, and removes exactly the one PATH line it added (a
  copy of the file is in the trash), into `<home>/trash/<date>/uninstall/`. Move them back to undo. It never
  touches your vault, Tempo's data, its logs, or anything it did not install.
- **Never overwrites**: a file that is not in the manifest is never replaced; it is reported and left alone.
  Nothing is ever deleted: the trash is the undo. (The only thing the scripts remove is their own scratch
  folder in `$TMPDIR`.)

## macOS without an Apple Developer ID

There is no paid signing here, and none is needed for your own machines.

- **Built from source (or Homebrew)**: nothing you build locally carries the quarantine flag, so Gatekeeper
  does not look at it. On Apple Silicon the Rust linker already signs binaries ad hoc.
- **Release tarball**: a browser-downloaded file is quarantined. `install.sh` runs
  `xattr -dr com.apple.quarantine` on what it installs, and `codesign --force -s -` (free ad-hoc signing, which
  Apple Silicon requires) when `codesign` is there (`xcode-select --install`). Fetching with `curl` adds no
  quarantine at all.
- **If macOS still refuses an app** ("cannot be opened because the developer cannot be verified"): in Finder
  right-click `Perennos Tempo.app`, choose Open, then Open again. Or System Settings, Privacy and Security,
  "Open Anyway". That is needed once per app, and only if the quarantine flag came back (for example after
  copying the app through AirDrop or a download).
- A notarized `.pkg` needs the paid Apple Developer ID and is not built.

## Other ways to get it

- **Release tarballs**: `build-release.sh [--edition ...]` builds `perennos-tempo-<edition>-<version>-<triple>.tar.gz`
  and its `.sha256` for the platform it runs on, in `installer/unix/build/out` (git ignores it). Inside: `bin/`,
  `share/` (icons, menu entries, the app bundle and autostart templates), `legal/` (the edition's licence texts and
  `THIRD-PARTY-NOTICES.txt` made by `../windows/notices.py`), `install.sh`, `README.txt`. It needs Rust and python3.
- **`.deb`**: `build-deb.sh` turns a tarball (or builds one) into `perennos-tempo_<version>-1_<arch>.deb` (full) or
  `perennos-tempo-source-available_...` with `dpkg-deb`: programs in `/usr/lib/perennos-tempo/bin`, symlinks in
  `/usr/bin`, menu entries, icons, and the licences in `/usr/share/doc/<package>/copyright`. The two packages
  conflict. Depends is worked out from the binaries (the glibc they need): build on the oldest distribution you
  want to support. The maintainer address `packages@perennos.com` is a placeholder. The package has no
  maintainer scripts; the desktop and icon caches refresh through the system's own triggers.
- **Homebrew**: `homebrew/perennos-tempo.rb` is a DRAFT formula for the source-available edition. Not published;
  it needs a tap repository (free) and a real tag's url and sha256.
- **CI**: `ci/release.yml` is a DRAFT GitHub Actions workflow (tests, source-available tarballs, a smoke test, artifacts)
  for ubuntu-latest, ubuntu-24.04-arm, macos-14 and macos-13. It is in this folder, not in `.github/`, so nothing runs
  here. It belongs in the public repository, where Actions is free. This is the way to get a real macOS and ARM
  Linux test without owning one. (GitHub retired `macos-13`; use `macos-15-intel` if that job will not start.)

## Icons

`icons/` holds the PNGs (16 to 256, and 512 for the main one) and `tempo.icns`, drawn once by
`icons/make_icons.py` from the Perennos mark in `../windows/art/make_art.py` (same mark and per-app colours as the
Windows icons). Re-run it only if the art changes.

## Tested, and not

Tested on Linux x86_64 (this one distribution) with a throwaway `HOME`: install from source, upgrade,
uninstall, dry runs, release tarball with a good, a bad and a missing checksum, prefix with a space, an existing
file left alone, the `.deb` contents and its programs run through the `/usr/bin` symlinks, `bash -n` on every
script.

Not tested: a real macOS run of anything (the `uname`-pretending dry runs only exercised the macOS file layout
and the plist and bundle text), `codesign` and `xattr`, `launchctl`, bash 3.2 itself (the script avoids bash 4
features on purpose), ARM Linux, `systemctl --user` with a real session (a stand-in was used), real `cron`
(a stand-in `crontab`), other distributions, installing the `.deb` with `dpkg -i`, the Homebrew formula
(only `ruby -c`), and the CI workflow (only parsed as YAML). `tempo-launch home --json` does not exist yet in the
launcher this was written against, so the fallback `~/.tempo` was what ran; the call and its JSON reading
(`"home": "..."`) were checked against a stand-in.
