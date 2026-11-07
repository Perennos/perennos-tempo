#!/usr/bin/env bash
# Build a .deb of Perennos Tempo from a release tarball (made by build-release.sh).
#
#   installer/unix/build-deb.sh [--edition full|source-available] [--tarball FILE] [--out DIR] [--with-mcp]
#
# Without --tarball it builds one first. Package names: perennos-tempo (full edition) and
# perennos-tempo-source-available; the two conflict, so only one is installed at a time.
# Layout: /usr/lib/perennos-tempo/bin, symlinks in /usr/bin, menu entries, icons, and the licence
# texts (copyright file). Needs dpkg-deb. Nothing is installed or published.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
die() { printf 'build-deb.sh: %s\n' "$*" >&2; exit 1; }
EDITION=''; TARBALL=''; OUT=${TEMPO_BUILD_DIR:+$TEMPO_BUILD_DIR/out}; OUT=${OUT:-$HERE/build/out}; WITH_MCP=()
while [ $# -gt 0 ]; do
  case $1 in
    --edition) [ $# -ge 2 ] || die "--edition needs a value"; EDITION=$2; shift ;;
    --tarball) [ $# -ge 2 ] || die "--tarball needs a file"; TARBALL=$2; shift ;;
    --out) [ $# -ge 2 ] || die "--out needs a folder"; OUT=$2; shift ;;
    --with-mcp) WITH_MCP=(--with-mcp) ;;
    -h|--help) sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done
command -v dpkg-deb >/dev/null || die "dpkg-deb is needed (Debian, Ubuntu)"
mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)

if [ -z "$TARBALL" ]; then
  EDARG=(); [ -z "$EDITION" ] || EDARG=(--edition "$EDITION")
  "$HERE/build-release.sh" ${EDARG[@]+"${EDARG[@]}"} ${WITH_MCP[@]+"${WITH_MCP[@]}"} --out "$OUT" | tee "$OUT/.last-build" >&2
  TARBALL=$(grep '\.tar\.gz$' "$OUT/.last-build" | tail -n 1); rm -f "$OUT/.last-build"
fi
[ -f "$TARBALL" ] || die "no such tarball: $TARBALL"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/perennos-tempo-deb.XXXXXX")
trap 'rm -rf "$WORK"' EXIT   # only this scratch folder
tar -xzf "$TARBALL" -C "$WORK"
SRC=$(find "$WORK" -mindepth 1 -maxdepth 1 -type d | head -n 1)
[ -f "$SRC/EDITION" ] && [ -f "$SRC/bin/tempo-launch" ] || die "$TARBALL is not a Perennos Tempo release"
EDITION=$(head -n 1 "$SRC/EDITION"); VERSION=$(head -n 1 "$SRC/VERSION"); TARGET=$(head -n 1 "$SRC/TARGET")
case $TARGET in
  x86_64-unknown-linux-gnu) ARCH=amd64 ;; aarch64-unknown-linux-gnu) ARCH=arm64 ;;
  *) die "a .deb needs a Linux tarball (this one is $TARGET)" ;;
esac
if [ "$EDITION" = full ]; then PKG=perennos-tempo; OTHER=perennos-tempo-source-available; else PKG=perennos-tempo-source-available; OTHER=perennos-tempo; fi

ROOT=$WORK/pkg
LIB=$ROOT/usr/lib/perennos-tempo
DOC=$ROOT/usr/share/doc/$PKG
mkdir -p "$LIB" "$ROOT/usr/bin" "$ROOT/usr/share/applications" "$DOC/legal" "$ROOT/DEBIAN"
cp -R "$SRC/bin" "$LIB/bin"
chmod 755 "$LIB/bin"/*
for f in "$LIB/bin"/*; do ln -s "../lib/perennos-tempo/bin/$(basename "$f")" "$ROOT/usr/bin/$(basename "$f")"; done

# menu entries (@LAUNCH@ is /usr/bin/tempo-launch) and icons, for the apps that are in this edition
render() { local line; : > "$2"; while IFS= read -r line || [ -n "$line" ]; do printf '%s\n' "${line//@LAUNCH@//usr/bin/tempo-launch}" >> "$2"; done < "$1"; }
for entry in 'perennos-tempo|tempo|tempo-launch' 'perennos-tempo-vault|tempo-vault|tempo-vault' 'perennos-tempo-room|tempo-room|tempo-room' 'perennos-tempo-field|tempo-field|tempo-dashboard'; do
  IFS='|' read -r name icon bin <<< "$entry"
  [ -f "$SRC/bin/$bin" ] || continue
  render "$SRC/share/applications/$name.desktop.in" "$ROOT/usr/share/applications/$name.desktop"
  chmod 644 "$ROOT/usr/share/applications/$name.desktop"
  for s in 16 32 48 64 128 256; do
    d=$ROOT/usr/share/icons/hicolor/${s}x${s}/apps; mkdir -p "$d"
    install -m 644 "$SRC/share/icons/$icon-$s.png" "$d/$name.png"
  done
done

# licences: the copyright file carries the edition's texts; the third-party notices sit beside it
cp "$SRC/legal/THIRD-PARTY-NOTICES.txt" "$DOC/legal/THIRD-PARTY-NOTICES.txt"
{
  echo "Perennos Tempo, $EDITION edition. https://perennos.com"
  echo
  echo "Part 1. The agent, Tempo Vault, Tempo Studio, Tempo Room and tempo-launch: PolyForm Shield License 1.0.0"
  echo "------------------------------------------------------------------------------------------------"
  cat "$SRC/legal/NOTICE.txt"; echo; cat "$SRC/legal/LICENSE-PolyForm-Shield-1.0.0.txt"; echo
  if [ "$EDITION" = full ]; then
    echo "Part 2. The Tempo Field and the tempo command, including VECTIS: proprietary"
    echo "---------------------------------------------------------------------------"
    cat "$SRC/legal/LICENSE-Tempo-Field-and-VECTIS.txt"; echo
  fi
  echo "Third-party notices: /usr/share/doc/$PKG/legal/THIRD-PARTY-NOTICES.txt"
} > "$DOC/copyright"
install -m 644 "$SRC/README.txt" "$DOC/README"
chmod 644 "$DOC/copyright" "$DOC/legal/THIRD-PARTY-NOTICES.txt"
find "$ROOT" -type d -exec chmod 755 {} +

# Depends: what the programs link to, from dpkg-shlibdeps when it is there (else libc6 of this machine)
DEPS=''
if command -v dpkg-shlibdeps >/dev/null; then
  mkdir -p "$WORK/shl/debian"; printf 'Source: x\n\nPackage: x\nArchitecture: any\n' > "$WORK/shl/debian/control"
  DEPS=$(cd "$WORK/shl" && dpkg-shlibdeps -O -e"$LIB"/bin/* 2>/dev/null | sed -n 's/^shlibs:Depends=//p') || DEPS=''
fi
if [ -z "$DEPS" ]; then
  # the newest glibc symbol the programs use, else this machine's libc6: build on the oldest distro you support
  GLIBC=''
  if command -v objdump >/dev/null; then
    GLIBC=$(objdump -T "$LIB"/bin/* 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1)
  fi
  [ -n "$GLIBC" ] || GLIBC=$(dpkg-query -W -f='${Version}' libc6 | sed 's/-.*//')
  DEPS="libc6 (>= $GLIBC), libgcc-s1"
fi
# the Tempo MCP server (perennos-tempo crate) links libssl; shlibdeps names it, this covers the rule when it is absent
if [ -f "$SRC/bin/perennos-tempo-mcp" ]; then case $DEPS in *libssl*) ;; *) DEPS="$DEPS, libssl3 | libssl3t64" ;; esac; fi
SIZE=$(du -sk "$ROOT" | cut -f1)
cat > "$ROOT/DEBIAN/control" <<EOT
Package: $PKG
Version: $VERSION-1
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: Perennos <packages@perennos.com>
Installed-Size: $SIZE
Depends: $DEPS
Conflicts: $OTHER
Replaces: $OTHER
Homepage: https://perennos.com
Description: Perennos Tempo, the local AI agent with its apps ($EDITION edition)
 Perennos Tempo runs an AI agent on your own computer, with Tempo Vault (your
 notes as Markdown), Tempo Room (private chat rooms) and Tempo Studio (mixing
 and mastering) as its tools, started by tempo-launch.
$( [ "$EDITION" = full ] && printf ' .\n This is the full edition: it adds the Tempo Field dashboard and the tempo\n command with VECTIS.\n' )
EOT
chmod 644 "$ROOT/DEBIAN/control"
DEB=$OUT/${PKG}_${VERSION}-1_${ARCH}.deb
dpkg-deb --root-owner-group -Zxz --build "$ROOT" "$DEB" >/dev/null
echo "$DEB"
