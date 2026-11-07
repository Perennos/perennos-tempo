#!/usr/bin/env bash
# Perennos Tempo installer for Linux and macOS. See README.md in this folder, or run: install.sh --help
#
# Written for bash 3.2 (the one macOS ships): no associative arrays, no mapfile, no ${x,,}.
# Never deletes: what it replaces or removes is moved to <tempo home>/trash/<date>/.
# Never overwrites a file it did not install (it keeps a manifest of what it did).
set -euo pipefail
umask 022

SELF_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# ---------------------------------------------------------------- output
if [ -t 1 ]; then
  C_B=$'\033[1m'; C_G=$'\033[32m'; C_Y=$'\033[33m'; C_R=$'\033[31m'; C_0=$'\033[0m'
else
  C_B=''; C_G=''; C_Y=''; C_R=''; C_0=''
fi
step() { printf '%s==>%s %s%s%s\n' "$C_G" "$C_0" "$C_B" "$*" "$C_0"; }
info() { printf '    %s\n' "$*"; }
ok() { printf '    %sok%s  %s\n' "$C_G" "$C_0" "$*"; }
warn() { printf '    %swarning%s  %s\n' "$C_Y" "$C_0" "$*" >&2; }
would() { printf '    %swould%s  %s\n' "$C_B" "$C_0" "$*"; }
die() { printf '%serror%s  %s\n' "$C_R" "$C_0" "$*" >&2; exit 1; }
# A crate's folder: crates/NAME in the private checkout, NAME at the top of the public repository.
crate_dir() { if [ -d "$1/crates/$2" ]; then printf '%s\n' "$1/crates/$2"; else printf '%s\n' "$1/$2"; fi; }

usage() {
  cat <<'EOF'
Perennos Tempo installer for Linux and macOS

usage: install.sh [options]

Where from (the default is --from-source inside a checkout, or the folder you unpacked a release into):
  --from-source [REPO]      build the programs with cargo (cargo build --release --locked) from REPO
  --from-release FILE|URL   install a release tarball (.tar.gz); its SHA-256 is checked first and a
                            mismatch stops everything
  --sha256 HEX              the expected SHA-256 of the tarball (else FILE.sha256 next to it)

What:
  --edition full|source-available
                            full (the default when the private crates are present) adds the Tempo Field
                            and the tempo command with VECTIS to the source-available edition
  --with-mcp                also build the Tempo MCP server (perennos-tempo; needs pkg-config and
                            libssl-dev on Debian and Ubuntu)
  --prefix DIR              put the programs in DIR (default: ~/.local/bin)
  --no-desktop              no menu entries / app bundle

Extras:
  --add-to-path             add one marked line to your shell's startup file
  --autostart               start the apps at sign-in (systemd user unit or autostart entry; LaunchAgent on macOS)
  --learn                   hourly learning from your vault (tempo-launch schedule)
  --setup                   show which models fit this computer (tempo-launch setup), then ask
                            before fetching them and setting up the agent (--yes skips the question)

Other:
  --uninstall               move everything this installer installed to <tempo home>/trash/<date>/uninstall/
                            (use the same --prefix as when installing; your vault and data are never touched)
  --dry-run                 show what would happen, change nothing
  -y, --yes                 do not ask
  --build-to DIR            build the programs into DIR/bin and stop (used by build-release.sh)
  -h, --help                this text
EOF
}

# ---------------------------------------------------------------- arguments
MODE=''; SRC_ARG=''; REL_ARG=''; SHA_ARG=''; EDITION=''; PREFIX=''
ADD_PATH=0; AUTOSTART=0; LEARN=0; SETUP=0; DESKTOP=1; WITH_MCP=0; UNINSTALL=0; DRY=0; YES=0; BUILD_TO=''
need_arg() { [ $# -ge 2 ] || die "$1 needs a value (see --help)"; }
while [ $# -gt 0 ]; do
  case $1 in
    --from-source)
      MODE=source
      case ${2:-} in ''|-*) ;; *) SRC_ARG=$2; shift ;; esac ;;
    --from-release) need_arg "$@"; REL_ARG=$2; shift ;;
    --sha256) need_arg "$@"; SHA_ARG=$2; shift ;;
    --edition) need_arg "$@"; EDITION=$2; shift ;;
    --prefix) need_arg "$@"; PREFIX=$2; shift ;;
    --build-to) need_arg "$@"; BUILD_TO=$2; shift ;;
    --with-mcp) WITH_MCP=1 ;;
    --no-desktop) DESKTOP=0 ;;
    --add-to-path) ADD_PATH=1 ;;
    --autostart) AUTOSTART=1 ;;
    --learn) LEARN=1 ;;
    --setup) SETUP=1 ;;
    --uninstall) UNINSTALL=1 ;;
    --dry-run) DRY=1 ;;
    -y|--yes) YES=1 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
  shift
done

case $(uname -s) in
  Linux) PLATFORM=linux ;;
  Darwin) PLATFORM=mac ;;
  *) die "this installer is for Linux and macOS (Windows has its own installer)" ;;
esac
[ -n "${HOME:-}" ] && [ -d "$HOME" ] || die "HOME is not set to a folder"
case $HOME in /*) ;; *) die "HOME must be an absolute path" ;; esac

[ -n "$PREFIX" ] || PREFIX=$HOME/.local/bin
case $PREFIX in "~"|"~/"*) PREFIX=$HOME${PREFIX#"~"} ;; esac
case $PREFIX in /*) ;; *) PREFIX=$PWD/$PREFIX ;; esac
PREFIX=${PREFIX%/}
# a short safe alphabet, so a path can go into a menu entry, a plist or a shell line without quoting tricks
printf '%s' "$HOME" | grep -Eq '^[[:alnum:]_./ @+,:-]+$' || die "HOME contains characters this installer does not handle (use letters, digits, space and _ . / @ + , : -)"
printf '%s' "$PREFIX" | grep -Eq '^[[:alnum:]_./ @+,:-]+$' || die "--prefix may use letters, digits, space and _ . / @ + , : - only (got: $PREFIX)"
case $EDITION in ''|full|source-available) ;; *) die "--edition is full or source-available" ;; esac
if [ -n "$SHA_ARG" ]; then
  printf '%s' "$SHA_ARG" | grep -Eq '^[0-9a-fA-F]{64}$' || die "--sha256 must be 64 hex digits"
fi

DATA_HOME=${XDG_DATA_HOME:-$HOME/.local/share}
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
case $DATA_HOME in /*) ;; *) DATA_HOME=$HOME/.local/share ;; esac
case $CONFIG_HOME in /*) ;; *) CONFIG_HOME=$HOME/.config ;; esac
DATE=$(date +%Y%m%d)

# ---------------------------------------------------------------- small helpers
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/perennos-tempo-install.XXXXXX")
# only the scratch folder made above is removed (build logs, extracted tarball); nothing of yours
cleanup() { [ -n "${SCRATCH:-}" ] && [ -d "$SCRATCH" ] && rm -rf "$SCRATCH"; }
trap cleanup EXIT

sha_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "no sha256sum or shasum found"; fi
}
have() { command -v "$1" >/dev/null 2>&1; }
render() { # render TEMPLATE OUT: replaces @LAUNCH@ and @VERSION@
  local in=$1 out=$2 line
  : > "$out"
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line//@LAUNCH@/$PREFIX/tempo-launch}
    line=${line//@VERSION@/$VERSION}
    printf '%s\n' "$line" >> "$out"
  done < "$in"
}

# ---------------------------------------------------------------- the tempo home, manifest, trash
find_home() {
  local h='' out=''
  if [ -x "$PREFIX/tempo-launch" ]; then
    # `home --json` is the launcher's answer to "where does Tempo keep its data"; older launchers refuse it
    out=$("$PREFIX/tempo-launch" home --json 2>/dev/null </dev/null) || out=''
    h=$(printf '%s' "$out" | sed -n 's/.*"home"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  fi
  case $h in /*) ;; *) h=${TEMPO_LAUNCH_HOME:-$HOME/.tempo} ;; esac
  printf '%s' "${h%/}"
}
set_home() {
  TEMPO_DIR=$(find_home)
  MANIFEST=$TEMPO_DIR/install/manifest.tsv
  TRASH=$TEMPO_DIR/trash/$DATE
}

HEADER_VERSION=''; HEADER_EDITION=''
ENTRIES=$SCRATCH/entries
: > "$ENTRIES"

save_manifest() {
  [ "$DRY" = 1 ] && return 0
  mkdir -p "$TEMPO_DIR/install"
  {
    printf '#perennos-tempo-manifest\t1\n'
    printf 'version\t%s\n' "$VERSION"
    printf 'edition\t%s\n' "$EDITION"
    printf 'prefix\t%s\n' "$PREFIX"
    cat "$ENTRIES"
  } > "$MANIFEST.tmp"
  mv "$MANIFEST.tmp" "$MANIFEST"
}
record() { # record KIND A B   (files: KIND SHA256 PATH; rcline: KIND RCFILE LINE; learn: KIND - LAUNCHER)
  local keep
  keep=$SCRATCH/entries.keep
  # a record for the same thing replaces the older one
  awk -F '\t' -v k="$1" -v b="$3" '!($1 == k && $3 == b)' "$ENTRIES" > "$keep" || true
  printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$keep"
  cat "$keep" > "$ENTRIES"
  save_manifest
}

trash_move() { # trash_move PATH BUCKET : moves PATH to <home>/trash/<date>/BUCKET/<its full path>
  local src=$1 bucket=$2 dest n=0
  dest=$TRASH/$bucket/${src#/}
  mkdir -p "$(dirname "$dest")"
  while [ -e "$dest" ] || [ -L "$dest" ]; do n=$((n + 1)); dest=$TRASH/$bucket/${src#/}.$n; done
  mv -- "$src" "$dest"
  printf '%s' "$dest"
}

# ---------------------------------------------------------------- uninstall
read_manifest() { # fills OLD_* from $MANIFEST
  OLD_VERSION=unknown; OLD_EDITION=''; OLD_PREFIX=''
  local kind a b
  while IFS=$'\t' read -r kind a b || [ -n "$kind" ]; do
    case $kind in
      version) OLD_VERSION=$a ;;
      edition) OLD_EDITION=$a ;;
      prefix) OLD_PREFIX=$a ;;
    esac
  done < "$MANIFEST"
}

stop_old_integrations() { # undo schedules and autostart that the manifest records (they live outside our files)
  local kind a b
  while IFS=$'\t' read -r kind a b || [ -n "$kind" ]; do
    case $kind in
      learn)
        if [ -x "$b" ]; then
          if [ "$DRY" = 1 ]; then would "remove the learning schedule ($b schedule --remove)"
          else "$b" schedule --remove >/dev/null 2>&1 </dev/null && ok "learning schedule removed" || warn "could not remove the learning schedule; run: $b schedule --remove"; fi
        fi ;;
      unit)
        case $b in
          *.service)
            if [ "$DRY" = 1 ]; then would "disable the systemd user unit"
            elif have systemctl; then systemctl --user disable --now perennos-tempo.service >/dev/null 2>&1 || true; fi ;;
          *.plist)
            if [ "$DRY" = 1 ]; then would "unload the LaunchAgent"
            elif have launchctl; then launchctl bootout "gui/$(id -u)/com.perennos.tempo" >/dev/null 2>&1 || launchctl unload "$b" >/dev/null 2>&1 || true; fi ;;
        esac ;;
    esac
  done < "$MANIFEST"
}

move_recorded() { # move_recorded BUCKET KINDS...  : moves the recorded files of those kinds; echoes nothing
  local bucket=$1 kinds=" ${*:2} " kind a b dest now quiet=0
  while IFS=$'\t' read -r kind a b || [ -n "$kind" ]; do
    case $kinds in *" $kind "*) ;; *) continue ;; esac
    if [ ! -e "$b" ] && [ ! -L "$b" ]; then info "already gone: $b"; continue; fi
    if [ "$DRY" = 1 ]; then would "move to trash: $b"; continue; fi
    if [ "$kind" != app ] && [ -f "$b" ] && [ "$a" != "-" ]; then
      now=$(sha_of "$b")
      [ "$now" = "$a" ] || warn "$b changed since it was installed (kept in trash)"
    fi
    dest=$(trash_move "$b" "$bucket")
    case $kind in bin|app) ok "moved $b" ;; *) quiet=$((quiet + 1)) ;; esac
  done < "$MANIFEST"
  if [ "$quiet" -gt 0 ]; then ok "and $quiet menu entries, icons and texts"; fi
}

remove_rc_lines() { # takes the marked line out of the shell file it was added to; backup in trash first
  local kind a b dest tmp
  while IFS=$'\t' read -r kind a b || [ -n "$kind" ]; do
    [ "$kind" = rcline ] || continue
    [ -f "$a" ] || continue
    grep -qxF -- "$b" "$a" || { info "the PATH line is no longer in $a"; continue; }
    if [ "$DRY" = 1 ]; then would "remove the PATH line from $a (backup in trash)"; continue; fi
    mkdir -p "$TRASH/uninstall/rc-backup"
    dest=$TRASH/uninstall/rc-backup/$(printf '%s' "${a#/}" | tr '/' '_')
    cp -p "$a" "$dest"
    tmp=$SCRATCH/rc.tmp
    grep -vxF -- "$b" "$a" > "$tmp" || true
    cat "$tmp" > "$a"
    ok "removed the PATH line from $a (copy of the old file: $dest)"
  done < "$MANIFEST"
}

refresh_desktop_caches() {
  [ "$PLATFORM" = linux ] || return 0
  [ "$DRY" = 1 ] && return 0
  if have update-desktop-database && [ -d "$DATA_HOME/applications" ]; then update-desktop-database "$DATA_HOME/applications" >/dev/null 2>&1 || true; fi
  if have gtk-update-icon-cache && [ -d "$DATA_HOME/icons/hicolor" ]; then gtk-update-icon-cache -q -t -f "$DATA_HOME/icons/hicolor" >/dev/null 2>&1 || true; fi
}

do_uninstall() {
  set_home
  step "Uninstalling Perennos Tempo"
  [ -f "$MANIFEST" ] || die "nothing to uninstall: no install record at $MANIFEST (if you used --prefix, pass it again)"
  read_manifest
  info "installed: version $OLD_VERSION, $OLD_EDITION edition, programs in $OLD_PREFIX"
  info "this moves exactly the files in $MANIFEST to $TRASH/uninstall/"
  info "your vault and the Tempo data in $TEMPO_DIR are not touched"
  [ "$DRY" = 1 ] || confirm
  stop_old_integrations
  if [ "$DRY" != 1 ] && [ -x "$OLD_PREFIX/tempo-launch" ]; then
    "$OLD_PREFIX/tempo-launch" stop >/dev/null 2>&1 </dev/null || true
  fi
  move_recorded uninstall bin app desktop icon doc unit
  remove_rc_lines
  if [ "$DRY" != 1 ]; then
    dest=$(trash_move "$MANIFEST" uninstall)
    ok "install record moved to $dest"
  fi
  refresh_desktop_caches
  step "Done"
  if [ "$DRY" = 1 ]; then info "dry run: nothing was changed"; else
    info "everything Perennos Tempo installed is in $TRASH/uninstall/ (move it back to undo)"
    info "kept as it was: your vault, and the Tempo data in $TEMPO_DIR"
  fi
}

confirm() {
  [ "$YES" = 1 ] && return 0
  [ -t 0 ] || die "not running in a terminal, so I cannot ask: add --yes"
  local a
  printf '    Continue? [y/N] '
  read -r a || a=n
  case $a in y|Y|yes|YES) ;; *) die "stopped, nothing was changed" ;; esac
}

# ---------------------------------------------------------------- uninstall goes first (needs none of the rest)
if [ "$UNINSTALL" = 1 ]; then
  VERSION=unknown; EDITION=unknown
  do_uninstall
  exit 0
fi

# ---------------------------------------------------------------- where the programs come from
VERSION=''; BUNDLE=''; REPO=''
if [ -n "$REL_ARG" ]; then MODE=release
elif [ -z "$MODE" ]; then
  if [ -f "$(crate_dir "$SELF_DIR/../.." perennos-tempo-launcher)/Cargo.toml" ]; then MODE=source
  elif [ -f "$SELF_DIR/bin/tempo-launch" ]; then MODE=bundle
  else die "run this inside a Perennos Tempo checkout (--from-source), or give --from-release FILE|URL"; fi
fi
if [ "$MODE" = bundle ]; then BUNDLE=$SELF_DIR; fi

fetch_release() { # sets BUNDLE after checking the SHA-256
  local file=$REL_ARG expect='' tarname extracted tmp
  step "Fetching the release"
  case $REL_ARG in
    https://*)
      have curl || die "curl is needed to download $REL_ARG"
      file=$SCRATCH/$(basename "$REL_ARG")
      curl -fsSL -o "$file" "$REL_ARG" || die "could not download $REL_ARG"
      if [ -z "$SHA_ARG" ]; then
        curl -fsSL -o "$file.sha256" "$REL_ARG.sha256" 2>/dev/null || true
      fi ;;
    http://*) die "refusing a plain http download; use https" ;;
    *) [ -f "$file" ] || die "no such file: $file" ;;
  esac
  expect=$SHA_ARG
  if [ -z "$expect" ] && [ -f "$file.sha256" ]; then
    expect=$(sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' "$file.sha256" | head -n 1)
  fi
  [ -n "$expect" ] || die "no checksum: give --sha256 HEX, or put $(basename "$file").sha256 next to the file. Not installing something I cannot verify."
  actual=$(sha_of "$file")
  expect=$(printf '%s' "$expect" | tr 'A-F' 'a-f')
  if [ "$actual" != "$expect" ]; then
    die "SHA-256 does not match, refusing to install. expected $expect, the file is $actual"
  fi
  ok "SHA-256 matches ($actual)"
  tarname=$(basename "$file")
  tar -tzf "$file" > "$SCRATCH/members" 2>/dev/null || die "$tarname is not a readable .tar.gz"
  if grep -Eq '(^/|(^|/)\.\.(/|$))' "$SCRATCH/members"; then die "$tarname has unsafe paths inside, refusing"; fi
  tmp=$SCRATCH/unpacked
  mkdir -p "$tmp"
  tar -xzf "$file" -C "$tmp"
  extracted=$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | head -n 1)
  [ -n "$extracted" ] && [ -f "$extracted/bin/tempo-launch" ] && [ -f "$extracted/VERSION" ] || die "$tarname does not look like a Perennos Tempo release"
  BUNDLE=$extracted
}
[ "$MODE" = release ] && fetch_release

if [ -n "$BUNDLE" ]; then
  # an installed release: what it holds decides
  [ -f "$BUNDLE/EDITION" ] && [ -f "$BUNDLE/VERSION" ] || die "$BUNDLE has no EDITION and VERSION files"
  B_EDITION=$(head -n 1 "$BUNDLE/EDITION"); VERSION=$(head -n 1 "$BUNDLE/VERSION")
  [ -z "$EDITION" ] || [ "$EDITION" = "$B_EDITION" ] || die "this release is the $B_EDITION edition, not $EDITION"
  EDITION=$B_EDITION
  if [ -f "$BUNDLE/TARGET" ]; then
    case $PLATFORM-$(uname -m) in
      linux-x86_64) HOST=x86_64-unknown-linux-gnu ;;
      linux-aarch64|linux-arm64) HOST=aarch64-unknown-linux-gnu ;;
      mac-arm64) HOST=aarch64-apple-darwin ;;
      mac-x86_64) HOST=x86_64-apple-darwin ;;
      *) HOST=unknown ;;
    esac
    T=$(head -n 1 "$BUNDLE/TARGET")
    [ "$T" = "$HOST" ] || die "this release is built for $T, and this computer is $HOST"
  fi
  SHARE_DIR=$BUNDLE/share; ICON_DIR=$BUNDLE/share/icons; LEGAL_DIR=$BUNDLE/legal
  STAGE_BIN=$BUNDLE/bin
else
  REPO=${SRC_ARG:-$SELF_DIR/../..}
  REPO=$(cd "$REPO" 2>/dev/null && pwd) || die "no such folder: $SRC_ARG"
  [ -f "$(crate_dir "$REPO" perennos-tempo-launcher)/Cargo.toml" ] || die "$REPO is not a Perennos Tempo checkout (no perennos-tempo-launcher crate)"
  VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$(crate_dir "$REPO" perennos-tempo-launcher)/Cargo.toml" | head -n 1)
  if [ -z "$EDITION" ]; then
    if [ -f "$(crate_dir "$REPO" perennos-tempo-dashboard)/Cargo.toml" ] && [ -f "$(crate_dir "$REPO" perennos-tempo-cli)/Cargo.toml" ]; then EDITION=full; else EDITION=source-available; fi
  fi
  if [ "$EDITION" = full ]; then
    { [ -f "$(crate_dir "$REPO" perennos-tempo-dashboard)/Cargo.toml" ] && [ -f "$(crate_dir "$REPO" perennos-tempo-cli)/Cargo.toml" ]; } ||
      die "the full edition needs crates/perennos-tempo-dashboard and crates/perennos-tempo-cli (the private crates); use --edition source-available"
  fi
  SHARE_DIR=$SELF_DIR/share; ICON_DIR=$SELF_DIR/icons
  STAGE_BIN=${BUILD_TO:+$BUILD_TO/bin}; STAGE_BIN=${STAGE_BIN:-$SCRATCH/bin}
  LEGAL_DIR=$SCRATCH/legal
fi
[ -n "$VERSION" ] || die "cannot work out the version"
[ -d "$SHARE_DIR" ] && [ -d "$ICON_DIR" ] || die "missing the share/ and icons/ folders next to install.sh"

# the programs of each edition: key|crate folder|program   (the same split as the Windows installer)
components() {
  printf '%s\n' 'launcher|perennos-tempo-launcher|tempo-launch' 'agent|perennos-tempo-local-agent|tempo-local-agent' \
    'vault|perennos-tempo-vault|tempo-vault' 'studio|perennos-tempo-studio|tempo-studio' 'room|perennos-tempo-room|tempo-room'
  if [ "$EDITION" = full ]; then printf '%s\n' 'field|perennos-tempo-dashboard|tempo-dashboard' 'cli|perennos-tempo-cli|tempo'; fi
  if [ "$WITH_MCP" = 1 ]; then printf '%s\n' 'mcp|perennos-tempo|perennos-tempo-mcp'; fi
}

active_bins() { # the programs this run will build: the edition's, minus what a missing prerequisite rules out
  local key dir bin
  while IFS='|' read -r key dir bin; do
    case $SKIP_KEYS in *" $key "*) ;; *) printf '%s\n' "$bin" ;; esac
  done < <(components)
}

# ---------------------------------------------------------------- prerequisites (source builds only)
OS_ID=''
if [ -f /etc/os-release ]; then OS_ID=$(. /etc/os-release; printf '%s %s' "${ID:-}" "${ID_LIKE:-}"); fi
pkg_fix() { # pkg_fix cc|ssl
  if [ "$PLATFORM" = mac ]; then
    case $1 in cc) echo "xcode-select --install" ;; ssl) echo "brew install pkg-config openssl@3" ;; esac; return
  fi
  case " $OS_ID " in
    *debian*|*ubuntu*) case $1 in cc) echo "sudo apt install build-essential" ;; ssl) echo "sudo apt install pkg-config libssl-dev" ;; esac ;;
    *fedora*|*rhel*|*centos*) case $1 in cc) echo "sudo dnf install gcc" ;; ssl) echo "sudo dnf install pkgconf-pkg-config openssl-devel" ;; esac ;;
    *arch*) case $1 in cc) echo "sudo pacman -S base-devel" ;; ssl) echo "sudo pacman -S pkgconf openssl" ;; esac ;;
    *alpine*) case $1 in cc) echo "sudo apk add build-base" ;; ssl) echo "sudo apk add pkgconf openssl-dev" ;; esac ;;
    *suse*) case $1 in cc) echo "sudo zypper install gcc" ;; ssl) echo "sudo zypper install pkg-config libopenssl-devel" ;; esac ;;
    *) case $1 in cc) echo "install a C compiler (gcc or clang) with your package manager" ;; ssl) echo "install pkg-config and the OpenSSL development files with your package manager" ;; esac ;;
  esac
}
BUILD_OK=1; SKIP_KEYS=' '; NOTES=()
check_prereqs() {
  step "Checking what the build needs"
  [ -d "$HOME/.cargo/bin" ] && case ":$PATH:" in *":$HOME/.cargo/bin:"*) ;; *) PATH=$PATH:$HOME/.cargo/bin; export PATH ;; esac
  if ! have cargo || ! have rustc; then
    BUILD_OK=0
    warn "cargo and rustc are not installed, so nothing can be built."
    info "fix: install Rust from https://rustup.rs, open a new terminal, and run this again"
    NOTES+=("Nothing was built: Rust is missing (https://rustup.rs).")
    return
  fi
  local v min=1.85.0
  v=$(rustc --version | sed -n 's/^rustc \([0-9][0-9.]*\).*/\1/p')
  if [ -z "$v" ] || [ "$(printf '%s\n%s\n' "$min" "$v" | sort -t. -k1,1n -k2,2n -k3,3n | head -n 1)" != "$min" ]; then
    BUILD_OK=0
    warn "rustc ${v:-?} is too old; Perennos Tempo needs $min or newer."
    info "fix: rustup update stable"
    NOTES+=("Nothing was built: rustc is older than $min (run: rustup update stable).")
    return
  fi
  ok "rustc $v, $(cargo --version | cut -d' ' -f1-2)"
  local cc_ok=1
  if [ "$PLATFORM" = mac ]; then
    xcode-select -p >/dev/null 2>&1 || cc_ok=0
  fi
  have cc || have gcc || have clang || cc_ok=0
  if [ "$cc_ok" = 0 ]; then
    BUILD_OK=0
    warn "no C compiler (Rust needs one to link, and Room and the agent build ring with it), so nothing can be built."
    info "fix: $(pkg_fix cc)"
    NOTES+=("Nothing was built: no C compiler. Run: $(pkg_fix cc)")
    return
  fi
  ok "C compiler"
  if [ "$WITH_MCP" = 1 ]; then
    if ! have pkg-config || ! pkg-config --exists openssl 2>/dev/null; then
      SKIP_KEYS="$SKIP_KEYS""mcp "
      warn "pkg-config and the OpenSSL headers are missing, so the Tempo MCP server (perennos-tempo) is skipped; the rest goes on."
      info "fix: $(pkg_fix ssl)"
      NOTES+=("Tempo MCP server skipped: run $(pkg_fix ssl), then install.sh --with-mcp again.")
    else ok "pkg-config and OpenSSL headers"; fi
  fi
}

# ---------------------------------------------------------------- build / collect the programs
BUILT_FAILED=0
build_source() {
  step "Building the $EDITION edition (cargo build --release --locked, one crate at a time)"
  mkdir -p "$STAGE_BIN"
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    case $CARGO_TARGET_DIR in /*) ;; *) CARGO_TARGET_DIR=$PWD/$CARGO_TARGET_DIR ;; esac
    export CARGO_TARGET_DIR
  fi
  local key dir bin tdir log cdir
  while IFS='|' read -r key dir bin; do
    case $SKIP_KEYS in *" $key "*) continue ;; esac
    cdir=$(crate_dir "$REPO" "$dir")
    if [ ! -f "$cdir/Cargo.toml" ]; then
      if [ "$key" = launcher ]; then die "no crate at $cdir"; fi
      info "$bin: no crate $dir, left out"; continue
    fi
    log=$SCRATCH/build-$bin.log
    info "building $bin ..."
    if (cd "$cdir" && cargo build --release --locked </dev/null) > "$log" 2>&1; then
      tdir=${CARGO_TARGET_DIR:-$cdir/target}
      cp "$tdir/release/$bin" "$STAGE_BIN/$bin"
      ok "$bin"
    else
      BUILT_FAILED=1
      warn "$bin did not build. Last lines of the log:"
      tail -n 12 "$log" | sed 's/^/        /' >&2
      NOTES+=("$bin was not built (see above).")
    fi
  done < <(components)
  [ -f "$STAGE_BIN/tempo-launch" ] || die "tempo-launch did not build; without it nothing else can start, so I stop here"
}

legal_for_source() { # the licence texts of the edition, same names as the release tarballs use
  local L=$REPO/installer/windows/legal
  mkdir -p "$LEGAL_DIR"
  cp "$L/PolyForm-Shield-1.0.0.txt" "$LEGAL_DIR/LICENSE-PolyForm-Shield-1.0.0.txt"
  cp "$L/PolyForm-Shield-notice.txt" "$LEGAL_DIR/NOTICE.txt"
  if [ "$EDITION" = full ]; then cp "$L/Tempo-Field-and-VECTIS.txt" "$LEGAL_DIR/LICENSE-Tempo-Field-and-VECTIS.txt"; fi
}

# ---------------------------------------------------------------- placing files (never over a file we did not install)
SKIPPED=(); INSTALLED=()
place_file() { # place_file KIND SRC DST MODE
  local kind=$1 src=$2 dst=$3 mode=$4
  if { [ -e "$dst" ] || [ -L "$dst" ]; } && awk -F '\t' -v p="$dst" '$3 == p { f = 1 } END { exit !f }' "$ENTRIES"; then
    return 0   # ours already (kept from the install being upgraded)
  fi
  if [ -e "$dst" ] || [ -L "$dst" ]; then
    SKIPPED+=("$dst")
    warn "left alone: $dst exists and Perennos Tempo did not install it"
    return 0
  fi
  if [ "$DRY" = 1 ]; then would "install $dst"; return 0; fi
  mkdir -p "$(dirname "$dst")"
  install -m "$mode" "$src" "$dst"
  record "$kind" "$(sha_of "$dst")" "$dst"
  INSTALLED+=("$dst")
}
render_to() { # render_to KIND TEMPLATE DST MODE
  local kind=$1 tpl=$2 dst=$3 mode=$4 tmp
  tmp=$SCRATCH/rendered
  render "$tpl" "$tmp"
  place_file "$kind" "$tmp" "$dst" "$mode"
}

have_bin() { [ -f "$STAGE_BIN/$1" ]; }

install_programs() {
  step "Installing the programs into $PREFIX"
  local f name
  for f in "$STAGE_BIN"/*; do
    [ -f "$f" ] || continue
    name=$(basename "$f")
    place_file bin "$f" "$PREFIX/$name" 755
  done
  [ "$DRY" = 1 ] || ok "${#INSTALLED[@]} programs"
  # the licence texts, as the Windows installer installs them in legal\
  local d=$DATA_HOME/perennos-tempo/legal
  for f in "$LEGAL_DIR"/*; do
    [ -f "$f" ] || continue
    place_file doc "$f" "$d/$(basename "$f")" 644
  done
}

APPS_TABLE='perennos-tempo|tempo|tempo-launch
perennos-tempo-vault|tempo-vault|tempo-vault
perennos-tempo-room|tempo-room|tempo-room
perennos-tempo-field|tempo-field|tempo-dashboard'

linux_desktop() {
  step "Menu entries and icons"
  local name icon bin s src
  while IFS='|' read -r name icon bin; do
    have_bin "$bin" || continue
    render_to desktop "$SHARE_DIR/applications/$name.desktop.in" "$DATA_HOME/applications/$name.desktop" 644
    for s in 16 32 48 64 128 256; do
      src=$ICON_DIR/$icon-$s.png
      [ -f "$src" ] && place_file icon "$src" "$DATA_HOME/icons/hicolor/${s}x${s}/apps/$name.png" 644
    done
  done <<EOF
$APPS_TABLE
EOF
  refresh_desktop_caches
}

mac_app() {
  step "Perennos Tempo.app in ~/Applications"
  local app="$HOME/Applications/Perennos Tempo.app" tmp
  if [ -e "$app" ] || [ -L "$app" ]; then
    SKIPPED+=("$app"); warn "left alone: $app exists and Perennos Tempo did not install it"; return 0
  fi
  if [ "$DRY" = 1 ]; then would "make $app"; return 0; fi
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  render "$SHARE_DIR/macos/Info.plist.in" "$app/Contents/Info.plist"
  render "$SHARE_DIR/macos/launcher.in" "$app/Contents/MacOS/perennos-tempo"
  chmod 755 "$app/Contents/MacOS/perennos-tempo"
  cp "$ICON_DIR/tempo.icns" "$app/Contents/Resources/tempo.icns"
  if [ "$MODE" != source ] && have codesign; then codesign --force -s - "$app" >/dev/null 2>&1 || warn "ad-hoc signing of the app failed (it still opens)"; fi
  record app "$(sha_of "$app/Contents/Info.plist")" "$app"
  INSTALLED+=("$app")
  ok "$app"
}

mac_prepare_release() { # release installs only: free ad-hoc signing, and no quarantine flag on what we installed
  step "macOS: Gatekeeper"
  # work on a copy, so the unpacked release is left as it was
  mkdir -p "$SCRATCH/bin-signed"
  cp "$STAGE_BIN"/* "$SCRATCH/bin-signed/"
  STAGE_BIN=$SCRATCH/bin-signed
  have xattr && xattr -dr com.apple.quarantine "$STAGE_BIN" 2>/dev/null || true
  if have codesign; then
    local f
    for f in "$STAGE_BIN"/*; do [ -f "$f" ] && codesign --force -s - "$f" >/dev/null 2>&1 || warn "could not ad-hoc sign $(basename "$f")"; done
    ok "ad-hoc signed (no paid certificate needed), quarantine flag removed"
  else
    warn "codesign is not installed (xcode-select --install); Apple Silicon will not run unsigned programs"
  fi
}

do_path() {
  step "PATH"
  local rc marker='# perennos-tempo-installer' line shell_name
  case ${SHELL:-} in *zsh) rc=$HOME/.zshrc ;; *bash) if [ "$PLATFORM" = mac ]; then rc=$HOME/.bash_profile; else rc=$HOME/.bashrc; fi ;; *) rc=$HOME/.profile ;; esac
  line="export PATH=\"$PREFIX:\$PATH\" $marker"
  if [ -f "$rc" ] && grep -qxF -- "$line" "$rc"; then info "$rc already has the line"; return 0; fi
  if [ "$DRY" = 1 ]; then would "add one line to $rc: $line"; return 0; fi
  if [ -f "$rc" ]; then
    mkdir -p "$TRASH/rc-backup"
    cp -p "$rc" "$TRASH/rc-backup/$(printf '%s' "${rc#/}" | tr '/' '_')"
  fi
  if [ -s "$rc" ] && [ -n "$(tail -c 1 "$rc")" ]; then printf '\n' >> "$rc"; fi
  printf '%s\n' "$line" >> "$rc"
  record rcline "$rc" "$line"
  ok "added to $rc (open a new terminal); a copy of the old file is in $TRASH/rc-backup"
}

do_autostart() {
  step "Start at sign-in"
  if [ "$PLATFORM" = mac ]; then
    local plist=$HOME/Library/LaunchAgents/com.perennos.tempo.plist
    render_to unit "$SHARE_DIR/autostart/launchagent.plist.in" "$plist" 644
    if [ "$DRY" != 1 ] && [ -f "$plist" ] && have launchctl; then
      launchctl bootstrap "gui/$(id -u)" "$plist" >/dev/null 2>&1 || launchctl load -w "$plist" >/dev/null 2>&1 || warn "could not load the LaunchAgent now; it loads at your next sign-in"
    fi
    return 0
  fi
  if have systemctl && systemctl --user show-environment >/dev/null 2>&1; then
    local unit=$CONFIG_HOME/systemd/user/perennos-tempo.service
    render_to unit "$SHARE_DIR/autostart/perennos-tempo.service.in" "$unit" 644
    if [ "$DRY" != 1 ] && [ -f "$unit" ]; then
      systemctl --user daemon-reload >/dev/null 2>&1 || true
      systemctl --user enable perennos-tempo.service >/dev/null 2>&1 && ok "systemd user unit enabled (starts at sign-in)" || warn "could not enable the unit: systemctl --user enable perennos-tempo.service"
    fi
  else
    render_to unit "$SHARE_DIR/autostart/perennos-tempo-autostart.desktop.in" "$CONFIG_HOME/autostart/perennos-tempo.desktop" 644
    info "systemd --user is not available here, so an autostart entry was used"
  fi
}

do_learn() {
  step "Hourly learning"
  if [ "$DRY" = 1 ]; then would "run: $PREFIX/tempo-launch schedule"; return 0; fi
  if "$PREFIX/tempo-launch" schedule </dev/null; then
    record learn - "$PREFIX/tempo-launch"
    ok "scheduled (remove with: $PREFIX/tempo-launch schedule --remove)"
  else
    warn "tempo-launch could not set the schedule (on Linux it needs crontab: install cron). Nothing else is affected."
    NOTES+=("Hourly learning was not set up.")
  fi
}

do_setup() {
  step "Models for this computer"
  if [ "$DRY" = 1 ]; then would "run: $PREFIX/tempo-launch setup (then setup --apply, after you say yes)"; return 0; fi
  if ! "$PREFIX/tempo-launch" setup </dev/null; then
    warn "tempo-launch setup could not finish. Nothing was fetched. Run it again later: $PREFIX/tempo-launch setup"
    NOTES+=("Models were not set up.")
    return 0
  fi
  local a=n
  if [ "$YES" = 1 ]; then a=y
  elif [ -t 0 ]; then
    printf '    Fetch what is missing and set up the agent now? [y/N] '
    read -r a || a=n
  fi
  case $a in
    y|Y|yes|YES)
      # the plan with sizes is on screen above: this is the consent, so the launcher is not asked again
      "$PREFIX/tempo-launch" setup --apply --yes </dev/null \
        || { warn "setup --apply did not finish (see the lines above). Run it again later: $PREFIX/tempo-launch setup --apply"; NOTES+=("Models were not fully set up."); } ;;
    *) info "nothing was fetched. When you want it: $PREFIX/tempo-launch setup --apply" ;;
  esac
}

# ---------------------------------------------------------------- the plan
set_home
OLD_FOUND=0
if [ -z "$BUILD_TO" ] && [ -f "$MANIFEST" ]; then OLD_FOUND=1; read_manifest; fi

if [ -z "$BUILD_TO" ]; then
  step "Perennos Tempo $VERSION, $EDITION edition"
  case $MODE in source) info "from source: $REPO" ;; release) info "from release: $REL_ARG" ;; bundle) info "from this release folder: $BUNDLE (no checksum to check: you unpacked it yourself)" ;; esac
  info "programs  $PREFIX"
  info "records   $TEMPO_DIR/install, old files go to $TRASH/"
  [ "$OLD_FOUND" = 1 ] && info "an install of version $OLD_VERSION is there: this is an upgrade (its programs move to trash first)"
  [ "$DESKTOP" = 1 ] && info "menu      $([ "$PLATFORM" = mac ] && echo "$HOME/Applications/Perennos Tempo.app" || echo "$DATA_HOME/applications")"
  [ "$DRY" = 1 ] && info "dry run: nothing will be changed"
  [ "$DRY" = 1 ] || confirm
fi

if [ "$MODE" = source ]; then
  check_prereqs
  if [ "$DRY" = 1 ]; then
    would "build: $(active_bins | tr '\n' ' ')"
    [ "$BUILD_OK" = 1 ] || die "the build cannot run until the prerequisites above are fixed"
  else
    [ "$BUILD_OK" = 1 ] || die "cannot build; fix the prerequisite above and run again"
    build_source
    legal_for_source
  fi
fi

if [ -n "$BUILD_TO" ]; then
  printf '%s\n' "$EDITION" > "$BUILD_TO/EDITION"; printf '%s\n' "$VERSION" > "$BUILD_TO/VERSION"
  step "Built into $BUILD_TO/bin"
  exit "$BUILT_FAILED"
fi

if [ "$MODE" != source ] && [ "$DRY" != 1 ] && [ "$PLATFORM" = mac ]; then mac_prepare_release; fi

# ---------------------------------------------------------------- do it
if [ "$OLD_FOUND" = 1 ]; then
  step "Moving the previous install to trash"
  # what the old install set up outside its files (schedules) stays; its files are replaced
  # keep the records that are not files we replace: PATH line, autostart, learning
  awk -F '\t' '$1 == "rcline" || $1 == "unit" || $1 == "learn"' "$MANIFEST" > "$ENTRIES"
  if [ "$DRY" != 1 ] && [ -x "$OLD_PREFIX/tempo-launch" ]; then "$OLD_PREFIX/tempo-launch" stop >/dev/null 2>&1 </dev/null || true; fi
  move_recorded "upgrade-$OLD_VERSION" bin app desktop icon doc
fi

if [ "$DRY" = 1 ]; then
  step "Dry run: what would be installed"
  if [ "$MODE" = source ]; then for b in $(active_bins); do would "install $PREFIX/$b"; done
  else for f in "$STAGE_BIN"/*; do would "install $PREFIX/$(basename "$f")"; done; fi
  [ "$DESKTOP" = 1 ] && would "menu entries and icons"
  [ "$ADD_PATH" = 1 ] && do_path
  [ "$AUTOSTART" = 1 ] && would "start at sign-in"
  [ "$LEARN" = 1 ] && do_learn
  [ "$SETUP" = 1 ] && do_setup
  info "dry run: nothing was changed"
  exit 0
fi

mkdir -p "$TEMPO_DIR/install"
save_manifest
install_programs
if [ "$DESKTOP" = 1 ]; then
  if [ "$PLATFORM" = mac ]; then mac_app; else linux_desktop; fi
fi
# the launcher is in place now: it may know a different Tempo home than the fallback
OLD_MANIFEST=$MANIFEST
set_home
if [ "$MANIFEST" != "$OLD_MANIFEST" ]; then
  save_manifest
  dest=$(trash_move "$OLD_MANIFEST" "moved-record")
  info "tempo-launch says Tempo's home is $TEMPO_DIR; the install record moved there (old copy: $dest)"
fi
[ "$ADD_PATH" = 1 ] && do_path
[ "$AUTOSTART" = 1 ] && do_autostart
[ "$LEARN" = 1 ] && do_learn
[ "$SETUP" = 1 ] && do_setup

# ---------------------------------------------------------------- summary
step "Done: Perennos Tempo $VERSION ($EDITION edition)"
info "installed  ${#INSTALLED[@]} items; the list is $MANIFEST"
info "programs   $PREFIX"
if [ "$DESKTOP" = 1 ]; then
  if [ "$PLATFORM" = mac ]; then info "app        $HOME/Applications/Perennos Tempo.app"; else info "menu       Perennos Tempo, Tempo Vault, Tempo Room$(have_bin tempo-dashboard && echo ', Tempo Field') in your applications menu"; fi
fi
info "legal      $DATA_HOME/perennos-tempo/legal"
if [ "${#SKIPPED[@]}" -gt 0 ]; then
  warn "${#SKIPPED[@]} existing files were left alone (not installed by Perennos Tempo):"
  for f in "${SKIPPED[@]}"; do info "  $f"; done
fi
if [ "${#NOTES[@]}" -gt 0 ]; then for n in "${NOTES[@]}"; do warn "$n"; done; fi
case ":$PATH:" in *":$PREFIX:"*) ;; *) [ "$ADD_PATH" = 1 ] || info "$PREFIX is not on your PATH: add --add-to-path, or run the programs by their full path" ;; esac
printf '\n%sTo start:%s %s\n' "$C_B" "$C_0" "$([ "$DESKTOP" = 1 ] && { [ "$PLATFORM" = mac ] && echo "open \"Perennos Tempo\" from ~/Applications, or" || echo "pick Perennos Tempo in your applications menu, or"; }) run: \"$PREFIX/tempo-launch\""
info "Stop:      \"$PREFIX/tempo-launch\" stop"
info "Uninstall: install.sh --uninstall$([ "$PREFIX" = "$HOME/.local/bin" ] || echo " --prefix \"$PREFIX\"")"
exit "$BUILT_FAILED"
