#!/bin/bash
# Checks for the queue lock. macOS only, like the tool.
#   bash test/lock.sh
set -u
here="$(cd "$(dirname "$0")" && pwd)"
Q="$here/../bin/tsc-queue"
fail=0

# A fake compiler that takes a moment, so runs overlap.
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
real="$tmp/tsc-real"
printf '#!/bin/bash\nsleep 0.3\n' > "$real"; chmod +x "$real"

run() { # queue dir -> stderr of one queued compile
  # shellcheck disable=SC2069  # keep stderr, drop stdout
  TSC_QUEUE_CONF=/dev/null TSC_QUEUE_DIR="$1" TSC_QUEUE_TRACE="$1/trace" \
    "$Q" exec "$real" -p . 2>&1 >/dev/null
}
check() { # name, then a command that must succeed
  local name="$1"; shift
  if "$@"; then echo "ok   $name"; else echo "FAIL $name"; fail=1; fi
}
# shellcheck disable=SC2329  # called through check
queued() { ! grep -q "lock stuck" <<<"$1" && grep -q ADMIT "$2/trace"; }

# 1. The state left behind on a real machine: a lock whose holder died before it
#    stamped the time. The next compile must still be queued, not bypass it.
d="$tmp/q1"; mkdir -p "$d/lock.d"
check "dead unstamped lock does not force a bypass" queued "$(run "$d")" "$d"

# 2. A lock whose holder died after stamping it is reclaimed once it is old.
d="$tmp/q2"; mkdir -p "$d"; ln -s "$(( $(date +%s) - 100 ))" "$d/lock"
start=$(date +%s); out="$(run "$d")"; took=$(( $(date +%s) - start ))
check "stale stamped lock is reclaimed" queued "$out" "$d"
check "reclaimed at once (${took}s)" test "$took" -lt 10

# 3. The lock still excludes: many compiles at once never exceed the slot limit.
d="$tmp/q3"; mkdir -p "$d"
for _ in $(seq 12); do TSC_QUEUE_MAX_SLOTS=2 run "$d" >/dev/null & done; wait
check "12 parallel compiles, all admitted" test "$(grep -c ADMIT "$d/trace")" -eq 12
max=$(sed -n 's/.*saw_running=\([0-9]*\).*/\1/p' "$d/trace" | sort -n | tail -1)
check "never more than 2 running (saw $max before admitting)" test "$max" -lt 2
check "no lock left behind" test ! -e "$d/lock"

exit "$fail"
