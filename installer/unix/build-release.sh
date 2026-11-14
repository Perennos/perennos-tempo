#!/usr/bin/env bash
# Build a Perennos Tempo release tarball for this computer's platform.
#
#   installer/unix/build-release.sh [--edition full|source-available] [--with-mcp] [--out DIR]
#
# Writes <out>/perennos-tempo-<edition>-<version>-<target-triple>.tar.gz and its .sha256.
# Needs Rust (cargo build --release --locked, via install.sh --build-to) and python3 (the notices).
# Honours CARGO_TARGET_DIR. --out defaults to $TEMPO_BUILD_DIR/out, else installer/unix/build/out.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
EDITION=''; OUT=${TEMPO_BUILD_DIR:+$TEMPO_BUILD_DIR/out}; OUT=${OUT:-$HERE/build/out}; WITH_MCP=()
die() { printf 'build-release.sh: %s\n' "$*" >&2; exit 1; }
# A crate's folder: crates/NAME in the private checkout, NAME at the top of the public repository.
crate_dir() { if [ -d "$ROOT/crates/$1" ]; then printf '%s\n' "$ROOT/crates/$1"; else printf '%s\n' "$ROOT/$1"; fi; }
while [ $# -gt 0 ]; do
  case $1 in
    --edition) [ $# -ge 2 ] || die "--edition needs a value"; EDITION=$2; shift ;;
    --out) [ $# -ge 2 ] || die "--out needs a folder"; OUT=$2; shift ;;
    --with-mcp) WITH_MCP=(--with-mcp) ;;
    -h|--help) sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done
if [ -z "$EDITION" ]; then
  if [ -f "$(crate_dir perennos-tempo-dashboard)/Cargo.toml" ] && [ -f "$(crate_dir perennos-tempo-cli)/Cargo.toml" ]; then EDITION=full; else EDITION=source-available; fi
fi
case $EDITION in full|source-available) ;; *) die "--edition is full or source-available" ;; esac
command -v python3 >/dev/null || die "python3 is needed for the third-party notices"
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

case $(uname -s) in
  Darwin) case $(uname -m) in arm64) TARGET=aarch64-apple-darwin ;; *) TARGET=x86_64-apple-darwin ;; esac ;;
  Linux) case $(uname -m) in x86_64) TARGET=x86_64-unknown-linux-gnu ;; aarch64|arm64) TARGET=aarch64-unknown-linux-gnu ;; *) die "no release target for $(uname -m)" ;; esac ;;
  *) die "Linux and macOS only" ;;
esac
# the triple rustc reports must be the one named in the file, so a tarball never lies about itself
if command -v rustc >/dev/null; then
  host=$(rustc -vV | sed -n 's/^host: //p')
  [ "$host" = "$TARGET" ] || die "rustc builds for $host, not $TARGET; use the matching toolchain"
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/perennos-tempo-release.XXXXXX")
trap 'rm -rf "$WORK"' EXIT   # only this scratch folder
STAGE=$WORK/stage
mkdir -p "$STAGE"

echo "==> building the $EDITION edition"
"$HERE/install.sh" --from-source "$ROOT" --edition "$EDITION" ${WITH_MCP[@]+"${WITH_MCP[@]}"} --build-to "$STAGE" --yes
VERSION=$(head -n 1 "$STAGE/VERSION")
NAME=perennos-tempo-$EDITION-$VERSION-$TARGET
TOP=$WORK/$NAME
mkdir -p "$TOP/share" "$TOP/legal"
mv "$STAGE/bin" "$TOP/bin"
cp "$STAGE/VERSION" "$STAGE/EDITION" "$TOP/"
printf '%s\n' "$TARGET" > "$TOP/TARGET"
cp -R "$HERE/icons" "$TOP/share/icons"
rm -f "$TOP/share/icons/make_icons.py"
cp -R "$HERE/share/applications" "$HERE/share/autostart" "$HERE/share/macos" "$TOP/share/"
cp "$HERE/install.sh" "$TOP/install.sh"; chmod 755 "$TOP/install.sh"

echo "==> licences and third-party notices ($EDITION)"
L=$ROOT/installer/windows/legal
echo "56328093d57c87dcf2811ddcc824caf2723a07ffc332e6fbe4f9f108a2893a91  $L/PolyForm-Shield-1.0.0.txt" | { sha256sum -c --quiet - 2>/dev/null || shasum -a 256 -c --quiet - ; } || die "legal/PolyForm-Shield-1.0.0.txt is not the PolyForm Shield 1.0.0 text"
cp "$L/PolyForm-Shield-1.0.0.txt" "$TOP/legal/LICENSE-PolyForm-Shield-1.0.0.txt"
cp "$L/PolyForm-Shield-notice.txt" "$TOP/legal/NOTICE.txt"
[ "$EDITION" = full ] && cp "$L/Tempo-Field-and-VECTIS.txt" "$TOP/legal/LICENSE-Tempo-Field-and-VECTIS.txt"
CRATES=()
for f in "$TOP"/bin/*; do
  case $(basename "$f") in
    tempo-launch) c=perennos-tempo-launcher ;; tempo-local-agent) c=perennos-tempo-local-agent ;;
    tempo-vault) c=perennos-tempo-vault ;; tempo-studio) c=perennos-tempo-studio ;; tempo-room) c=perennos-tempo-room ;;
    tempo-dashboard) c=perennos-tempo-dashboard ;; tempo) c=perennos-tempo-cli ;; perennos-tempo-mcp) c=perennos-tempo ;;
    *) continue ;;
  esac
  CRATES+=("$(crate_dir "$c")")
done
python3 -I "$ROOT/installer/windows/notices.py" "$TOP/legal/THIRD-PARTY-NOTICES.txt" "$TARGET" "${CRATES[@]}"
python3 -I "$ROOT/installer/control_bundle.py" \
  --version "$VERSION" --target "$TARGET" \
  --launcher "$TOP/bin/tempo-launch" --agent "$TOP/bin/tempo-local-agent" \
  --license "$TOP/legal/LICENSE-PolyForm-Shield-1.0.0.txt" \
  --notice "$TOP/legal/NOTICE.txt" \
  --third-party-notices "$TOP/legal/THIRD-PARTY-NOTICES.txt" \
  --out-dir "$OUT"

cat > "$TOP/README.txt" <<EOT
Perennos Tempo $VERSION, $EDITION edition, for $TARGET

Install (no administrator needed, nothing is paid for):
  ./install.sh                          programs into ~/.local/bin, menu entries, icons
  ./install.sh --add-to-path --learn    also put them on your PATH and learn from your vault hourly
  ./install.sh --help                   every option
  ./install.sh --uninstall              moves what it installed to <tempo home>/trash/<date>/ (your vault is never touched)

Start: pick Perennos Tempo in the applications menu (macOS: ~/Applications), or run tempo-launch.

Licences are in legal/: PolyForm Shield 1.0.0 for the open parts$([ "$EDITION" = full ] && echo ", the proprietary notice for the Tempo Field and the tempo command with VECTIS"), and the third-party notices.

macOS without a paid Apple Developer ID: install.sh removes the quarantine flag from what it installs and signs it ad hoc
(codesign -s -). If macOS still refuses an app the first time, right-click it, choose Open, then Open.
Full notes: installer/unix/README.md in the repository.
EOT

echo "==> packing $NAME.tar.gz"
if tar --version 2>/dev/null | grep -q GNU; then
  tar --sort=name --owner=0 --group=0 --numeric-owner -czf "$OUT/$NAME.tar.gz" -C "$WORK" "$NAME"
else
  tar -czf "$OUT/$NAME.tar.gz" -C "$WORK" "$NAME"
fi
( cd "$OUT" && { sha256sum "$NAME.tar.gz" 2>/dev/null || shasum -a 256 "$NAME.tar.gz"; } > "$NAME.tar.gz.sha256" )
echo "$OUT/$NAME.tar.gz"
cat "$OUT/$NAME.tar.gz.sha256"
