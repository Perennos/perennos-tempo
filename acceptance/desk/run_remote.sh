#!/bin/sh
# Run the acceptance harness on another machine over ssh, then bring the results back.
#   run_remote.sh <ssh-host> <out-dir-from-gen_firm> <adapter.json> [extra harness arguments]
# The acceptance run has to happen on a machine other than the one that built the Desk.
# The remote machine needs python3 and, for the egress test, tcpdump with passwordless sudo.
# Before trusting a zero in the egress test, the negative control must show packets > 0
# (egress/PLAN.md, step 2).
set -eu
[ $# -ge 3 ] || { sed -n '2,6p' "$0"; exit 2; }
HOST="$1"; OUT="$2"; ADAPTER="$3"; shift 3
HERE="$(cd "$(dirname "$0")" && pwd)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"; R="desk-acceptance/run-$STAMP"
ssh "$HOST" "mkdir -p $R"
scp -q -r "$OUT/corpus" "$OUT/drop" "$OUT/eval" "$HOST:$R/"
scp -q "$HERE/harness.py" "$HERE/egress/capture.sh" "$ADAPTER" "$HOST:$R/"
RC=0
ssh "$HOST" "cd $R && chmod +x capture.sh && python3 harness.py --eval eval --adapter $(basename "$ADAPTER") --out results --builder-host '$(hostname)' $*" || RC=$?
mkdir -p "results-$HOST-$STAMP"
scp -q -r "$HOST:$R/results/." "results-$HOST-$STAMP/"
echo "results in results-$HOST-$STAMP (scorecard.txt, results.json); harness exit code $RC"
exit "$RC"
