#!/bin/bash
# Fills a scratch taskguard state with a few minutes of made-up monorepo work,
# for screenshots and for trying the dashboard. Nothing touches your real
# queue or history: everything lives under $DEMO (default /tmp/taskguard-demo).
#
#   scripts/demo.sh               run the demo (about four minutes)
#   TASKGUARD_DIR=/tmp/taskguard-demo/state taskguard top   watch it
#
# The jobs are busy loops and a Python process that holds memory: they use
# real CPU and memory, so taskguard measures, learns and queues them for real.
set -euo pipefail

DEMO=${DEMO:-/tmp/taskguard-demo}
TG=${TASKGUARD:-taskguard}
# TASKGUARD_DEMO_GROUPS: the recorder records made-up groups of other
# programs (agents, browsers, ...) instead of the ones on this machine.
export TASKGUARD_DIR=$DEMO/state TASKGUARD_CONF=$DEMO/config.toml TASKGUARD_RECORDER_IDLE_EXIT=900 TASKGUARD_DEMO_GROUPS=1

rm -rf "$DEMO"
mkdir -p "$DEMO/state"
# A low CPU limit, so a laptop's worth of demo jobs has to queue.
printf 'cpu_max = 60\nmem_max = 85\nstatus_every = 5\n' > "$DEMO/config.toml"
for repo in shop billing; do
  mkdir -p "$DEMO/$repo/.git"
  for pkg in web api ui checkout search invoices ledger; do mkdir -p "$DEMO/$repo/packages/$pkg"; done
done

# work CORES SECONDS MEGABYTES: keep CORES busy and hold MEGABYTES for SECONDS.
cat > "$DEMO/work" <<'EOF'
#!/bin/bash
cores=$1 secs=$2 mb=$3
for _ in $(seq "$cores"); do ( end=$((SECONDS + secs)); while [ $SECONDS -lt $end ]; do :; done ) & done
python3 -c "import time; x = bytearray($mb * 1024 * 1024); x[::4096] = b'1' * len(x[::4096]); time.sleep($secs)" &
wait
EOF
chmod +x "$DEMO/work"

# job REPO PACKAGE KEY CORES SECONDS MEGABYTES
job() {
  (cd "$DEMO/$1/packages/$2" && "$TG" -q --key "packages/$2:$3" -- "$DEMO/work" "$4" "$5" "$6") &
}

echo "learning: a test suite that grows a little on every run"
for run in $(seq 14); do
  (cd "$DEMO/shop/packages/api" && "$TG" -q --key packages/api:vitest -- "$DEMO/work" 1 1 $((120 + run * 18)))
done

round() {
  job shop web tsc 3 20 900
  job shop ui tsc 2 14 600
  job shop checkout vitest 4 18 700
  job billing invoices tsc 3 22 1400
  job billing ledger vitest 5 16 800
  job shop search build 4 12 1100
  job billing api tsc 2 10 500
  wait
}

echo "round 1: first runs, nothing learned yet"
round
echo "round 2: learned needs, the queue orders the work"
round
echo "round 3: one more, with a job that wants more cores than it gets"
job shop web tsc 3 20 900
job billing invoices tsc 3 22 1400
job shop checkout vitest 10 20 700
job billing ledger vitest 5 16 800
job shop search build 4 12 1100
wait
echo "done: TASKGUARD_DIR=$DEMO/state taskguard top"
