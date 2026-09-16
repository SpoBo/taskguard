#!/bin/bash
# Removes tsc-queue completely and puts every original compiler back.
#
#   ./uninstall.sh          remove the wrapper, keep your config and history
#   ./uninstall.sh --purge  also delete the config file and the memory history
#
# The order matters. The launchd jobs are stopped FIRST: a repair job that is
# still loaded would re-wrap the compilers a few seconds after they were
# restored.
#
# Only launchd jobs under this installation's own label prefix are removed. A
# second, unrelated installation with a different prefix is left alone. Set
# TSC_QUEUE_LABEL_PREFIX to clean up an installation that used another one.

set -uo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"
TARGET="$BIN/tsc-queue"
CONF="${TSC_QUEUE_CONF:-$HOME/.config/tsc-queue.conf}"
QDIR="${TSC_QUEUE_DIR:-$HOME/.cache/tsc-queue}"
PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

say() { printf '==> %s\n' "$*"; }

# The label prefix this installation used. The config file wins, because that is
# what the tool itself reads.
LABEL_PREFIX="${TSC_QUEUE_LABEL_PREFIX:-io.github.tsc-queue}"
if [ -r "$CONF" ]; then
  # shellcheck disable=SC1090
  LABEL_PREFIX="$( . "$CONF" >/dev/null 2>&1; echo "${TSC_QUEUE_LABEL_PREFIX:-$LABEL_PREFIX}" )"
fi

# Jobs belonging to THIS installation only. A label matches when it is the
# prefix itself or the prefix followed by a dot, never merely a substring.
our_labels() {
  launchctl list 2>/dev/null | awk -v p="$LABEL_PREFIX" '
    $3==p || index($3, p ".")==1 { print $3 }'
}

drop_jobs() {
  local label n=0
  for label in $(our_labels); do
    launchctl bootout "gui/$(id -u)/$label" 2>/dev/null
    rm -f "$HOME/Library/LaunchAgents/$label.plist"
    say "removed launchd job $label"
    n=$((n+1))
  done
  # A plist left on disk but not loaded still gets picked up at the next login.
  for label in "$LABEL_PREFIX.watch" "$LABEL_PREFIX.repair"; do
    [ -f "$HOME/Library/LaunchAgents/$label.plist" ] || continue
    rm -f "$HOME/Library/LaunchAgents/$label.plist"
    say "removed leftover plist for $label"
  done
}

if [ ! -x "$TARGET" ]; then
  say "no tsc-queue at $TARGET"
  drop_jobs
  exit 0
fi

# ---- 1. stop the background jobs
"$TARGET" unload || true

# ---- 2. put the original compilers back
"$TARGET" uninstall || true

# ---- 3. catch a job or a plist the tool itself did not clear
drop_jobs

# ---- 4. the script
rm -f "$TARGET"
say "removed $TARGET"

# ---- 5. state
if [ "$PURGE" -eq 1 ]; then
  rm -rf "$QDIR"; say "deleted $QDIR"
  rm -f "$CONF"; say "deleted $CONF"
else
  say "kept your config ($CONF) and memory history ($QDIR/history.tsv)"
  say "pass --purge to delete those too"
fi

echo
say "Done. If a compiler is still wrapped somewhere, reinstall its package."
