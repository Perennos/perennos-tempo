#!/bin/sh
# For a maintainer with network access: fetch each entry of voice.json and print its real SHA-256
# and size, ready to paste back with "verified": true. Tempo never runs this.
# Usage: sh catalog/pin-voice.sh catalog/voice.json
set -eu
cat_file=${1:-catalog/voice.json}
dir=$(mktemp -d)
grep -o '"url": "[^"]*"' "$cat_file" | cut -d'"' -f4 | while read -r url; do
  f="$dir/$(basename "$url")"
  curl -fsSL --retry 3 -o "$f" "$url" || { echo "FAILED $url"; continue; }
  printf '%s\n  "sha256": "%s", "bytes": %s\n' "$url" \
    "$(sha256sum "$f" | cut -d' ' -f1)" "$(wc -c < "$f" | tr -d ' ')"
done
echo "Files are in $dir. Move them to the trash when you're done."
