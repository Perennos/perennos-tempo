#!/usr/bin/env bash
# Build the Perennos Tempo installer for Windows, on Linux.
#
#   ISCC=/path/to/ISCC.exe installer/windows/build.sh
#
# Cross-compiles every product for x86_64-pc-windows-gnu, draws the art, writes the third-party
# notices, then runs Inno Setup's compiler under Wine. The result is
# build/out/PerennosTempo-Setup-<version>-x64.exe and its .sha256. README.md says what each step needs.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BUILD=${TEMPO_BUILD_DIR:-$HERE/build}
TARGET=x86_64-pc-windows-gnu

# Where each product's crate is: crates/NAME in the private checkout, NAME at the top of the public
# repository, unless TEMPO_SRC_<NAME> points somewhere else (a branch checked out next to this one).
src() { local var="TEMPO_SRC_$1" d="$ROOT/crates/$2"; [ -d "$d" ] || d="$ROOT/$2"; echo "${!var:-$d}"; }
LAUNCHER=$(src LAUNCHER perennos-tempo-launcher)
VERSION=${TEMPO_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$LAUNCHER/Cargo.toml" | head -1)}
AGENT=$(src AGENT perennos-tempo-local-agent)
VAULT=$(src VAULT perennos-tempo-vault)
STUDIO=$(src STUDIO perennos-tempo-studio)
ROOM=$(src ROOM perennos-tempo-room)
FIELD=$(src FIELD perennos-tempo-dashboard)
CLI=$(src CLI perennos-tempo-cli)

# Which apps go in; the launcher and the agent always do. The source-available edition leaves out
# the paid Field and VECTIS (the tempo command): TEMPO_APPS="vault studio room".
APPS=" $(tr ',' ' ' <<< "${TEMPO_APPS:-vault studio room field cli}") "
want() { [[ $APPS == *" $1 "* ]]; }

say() { printf '\033[1m%s\033[0m\n' "$*"; }
die() { printf 'build.sh: %s\n' "$*" >&2; exit 1; }

[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "the version must be numbers like 0.1.0 (got \"$VERSION\")"
for a in $APPS; do
  [[ " vault studio room field cli " == *" $a "* ]] || die "TEMPO_APPS: \"$a\" is not an app (vault, studio, room, field, cli)"
done
# The full edition has the paid parts (the Field, VECTIS's tempo command); without them it is
# the source-available edition, and its file name says so.
if want field || want cli; then EDITION=full SUFFIX=""; else EDITION=source-available SUFFIX=-source-available; fi
LEGAL=$HERE/legal
echo "56328093d57c87dcf2811ddcc824caf2723a07ffc332e6fbe4f9f108a2893a91  $LEGAL/PolyForm-Shield-1.0.0.txt" |
  sha256sum -c --quiet - || die "legal/PolyForm-Shield-1.0.0.txt is not the PolyForm Shield 1.0.0 text of the public repo"
command -v cargo >/dev/null || die "cargo is needed (https://rustup.rs)"
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" || die "run: rustup target add $TARGET"
command -v x86_64-w64-mingw32-gcc >/dev/null || die "the MinGW-w64 linker is needed (apt install gcc-mingw-w64-x86-64)"
command -v python3 >/dev/null || die "python3 is needed for the art and the notices"
command -v wine >/dev/null || die "wine is needed to run Inno Setup's compiler on Linux"
[[ -n ${ISCC:-} && -f $ISCC ]] || die "set ISCC to Inno Setup 6.4 or later's ISCC.exe (README.md says where to get it)"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER:-x86_64-w64-mingw32-gcc}

# a fresh stage for every build, so nothing from an earlier one can slip in
STAGE=$BUILD/stage/$VERSION-$(date +%Y%m%d-%H%M%S)
ART=$BUILD/art
OUT=$BUILD/out
mkdir -p "$STAGE/legal" "$STAGE/examples" "$ART" "$OUT"

say "1/5 the art"
python3 "$HERE/art/make_art.py" "$ART"
export TEMPO_LAUNCH_ICON=$ART/tempo.ico

say "2/5 the programs ($TARGET)"
BUILT=()
build() { # name crate-dir program required
  local name=$1 dir=$2 bin=$3 need=$4
  if [[ ! -f $dir/Cargo.toml ]]; then
    [[ $need == yes ]] && die "$name: no crate at $dir"
    echo "  $name: no crate at $dir, so not in this installer"
    return 0
  fi
  echo "  $name: $dir"
  CARGO_TARGET_DIR="$BUILD/target/$name" cargo build --quiet --release --locked \
    --target "$TARGET" --manifest-path "$dir/Cargo.toml"
  cp "$BUILD/target/$name/$TARGET/release/$bin.exe" "$STAGE/"
  BUILT+=("$dir")
}
build launcher "$LAUNCHER" tempo-launch yes
build agent "$AGENT" tempo-local-agent yes
for pair in "vault:$VAULT:tempo-vault" "studio:$STUDIO:tempo-studio" "room:$ROOM:tempo-room" \
  "field:$FIELD:tempo-dashboard" "cli:$CLI:tempo"; do
  IFS=: read -r name dir bin <<< "$pair"
  if want "$name"; then build "$name" "$dir" "$bin" no; else echo "  $name: not chosen (TEMPO_APPS)"; fi
done

say "3/5 licenses, notices and examples ($EDITION edition)"
python3 "$HERE/notices.py" "$STAGE/legal/THIRD-PARTY-NOTICES.txt" "$TARGET" "${BUILT[@]}"
cp "$LEGAL/PolyForm-Shield-1.0.0.txt" "$STAGE/legal/LICENSE-PolyForm-Shield-1.0.0.txt"
cp "$LEGAL/PolyForm-Shield-notice.txt" "$STAGE/legal/NOTICE.txt"
# the license page: what is under PolyForm Shield, what is proprietary, then the third-party notices
open_names="Tempo Agent"
for a in vault:"Tempo Vault" studio:"Tempo Studio" room:"Tempo Room"; do
  if want "${a%%:*}" && [[ -f $STAGE/tempo-${a%%:*}.exe ]]; then open_names+=", ${a#*:}"; fi
done
open_names+=" and tempo-launch"
PAGE=$STAGE/license-page.rtf
if [[ $EDITION == full ]]; then
  cp "$LEGAL/Tempo-Field-and-VECTIS.txt" "$STAGE/legal/LICENSE-Tempo-Field-and-VECTIS.txt"
  python3 "$HERE/license_page.py" "$PAGE" \
    --intro "Perennos Tempo is made of parts under different terms. Please read them; accepting below accepts all three." \
    --part "1. $open_names: PolyForm Shield License 1.0.0" "$LEGAL/PolyForm-Shield-notice.txt" "$LEGAL/PolyForm-Shield-1.0.0.txt" \
    --part "2. Tempo Field and the tempo command, including VECTIS: proprietary" "$LEGAL/Tempo-Field-and-VECTIS.txt" \
    --part "3. Third-party notices" "$STAGE/legal/THIRD-PARTY-NOTICES.txt"
else
  python3 "$HERE/license_page.py" "$PAGE" \
    --part "1. Perennos Tempo, source-available edition ($open_names): PolyForm Shield License 1.0.0" "$LEGAL/PolyForm-Shield-notice.txt" "$LEGAL/PolyForm-Shield-1.0.0.txt" \
    --part "2. Third-party notices" "$STAGE/legal/THIRD-PARTY-NOTICES.txt"
fi
for pair in "agent:$AGENT" "studio:$STUDIO" "room:$ROOM"; do
  name=${pair%%:*} dir=${pair#*:}
  if [[ -f $dir/config.example.json ]]; then
    sed 's/"owner_name": *"[^"]*"/"owner_name": "Your name"/' "$dir/config.example.json" > "$STAGE/examples/$name-config.example.json"
  fi
done

say "4/5 the installer (Inno Setup, under Wine)"
export WINEPREFIX=${WINEPREFIX:-$BUILD/wine} WINEDEBUG=${WINEDEBUG:--all}
win() { winepath -w "$1" | tr -d '\r'; }
# TEMPO_LICENSE_FILE: another license page in place of the one made above
EXTRA=("/DLicenseFile=$(win "${TEMPO_LICENSE_FILE:-$PAGE}")")
[[ $EDITION == full ]] || EXTRA+=("/DEdition=$EDITION")
wine "$ISCC" /Qp "/DAppVersion=$VERSION" "/DStage=$(win "$STAGE")" "/DArtDir=$(win "$ART")" \
  "/DOutDir=$(win "$OUT")" "${EXTRA[@]}" "$(win "$HERE/PerennosTempo.iss")"

say "5/5 the checksum"
SETUP=PerennosTempo-Setup-$VERSION-x64$SUFFIX.exe
[[ -f $OUT/$SETUP ]] || die "Inno Setup made no $SETUP"
(cd "$OUT" && sha256sum "$SETUP" > "$SETUP.sha256" && cat "$SETUP.sha256")
echo "$EDITION edition, built from: ${BUILT[*]}"
