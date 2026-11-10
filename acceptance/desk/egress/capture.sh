#!/bin/sh
# Packet capture for the egress test. Linux, needs tcpdump and root (or passwordless sudo).
#   capture.sh begin   start capturing on every interface
#   capture.sh end     stop, then print {"packets": n, "bytes": n} counting non-loopback packets only
# The one thing left out is TCP port 22, so the person running the test over ssh does not count.
# Everything else counts: DNS, ARP, mDNS, NTP, updates. Run the test on a quiet machine.
set -eu
DIR="${EGRESS_DIR:-/tmp/desk-egress}"
SUDO=""; [ "$(id -u)" -eq 0 ] || SUDO="sudo -n"
FILTER='not tcp port 22 and not (host 127.0.0.1 or host ::1)'
case "${1:-}" in
  begin)
    mkdir -p "$DIR"; : > "$DIR/egress.pcap"
    $SUDO sh -c "tcpdump -i any -nn -U -w '$DIR/egress.pcap' $FILTER >/dev/null 2>&1 & echo \$! > '$DIR/pid'"
    sleep 2 ;;
  end)
    sleep 1; $SUDO kill "$(cat "$DIR/pid")" 2>/dev/null || true; sleep 1
    $SUDO tcpdump -nn -q -r "$DIR/egress.pcap" 2>/dev/null | awk '{n++; b+=$NF} END {printf "{\"packets\": %d, \"bytes\": %d}\n", n+0, b+0}' ;;
  *) echo "usage: $0 begin|end" >&2; exit 2 ;;
esac
