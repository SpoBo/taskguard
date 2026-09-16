#!/bin/bash
# Installs tsc-queue: copies the script, writes a config file, wraps the
# compiler in every configured checkout, and loads the two launchd jobs.
#
#   ./install.sh                       install, then tell me what to configure
#   ./install.sh ~/code/repo-a ~/code/repo-b
#                                      install and manage those checkouts
#   PREFIX=/usr/local ./install.sh     install somewhere other than ~/.local
#
# Safe to run again. It never overwrites a config file you already have.

set -euo pipefail

[ "$(uname -s)" = "Darwin" ] || { echo "tsc-queue is macOS only." >&2; exit 1; }

SRC="$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"
CONF="${TSC_QUEUE_CONF:-$HOME/.config/tsc-queue.conf}"
TARGET="$BIN/tsc-queue"

say() { printf '==> %s\n' "$*"; }

# ---- 1. the script
mkdir -p "$BIN"
install -m 0755 "$SRC/bin/tsc-queue" "$TARGET"
say "installed $TARGET"

# ---- 2. the config file
mkdir -p "$(dirname "$CONF")"
if [ -f "$CONF" ]; then
  say "kept your existing config at $CONF"
else
  cp "$SRC/tsc-queue.conf.example" "$CONF"
  # An example path helps nobody as a live default, so start with no scope.
  /usr/bin/sed -i '' 's|^TSC_QUEUE_ROOTS=.*|TSC_QUEUE_ROOTS=""|' "$CONF"
  say "wrote a starter config at $CONF"
fi

# ---- 3. the checkouts named on the command line
if [ "$#" -gt 0 ]; then
  roots=""
  for p in "$@"; do
    [ -d "$p" ] || { echo "not a directory: $p" >&2; exit 1; }
    roots="$roots $(cd -P "$p" && pwd)"
  done
  roots="${roots# }"
  /usr/bin/sed -i '' "s|^TSC_QUEUE_ROOTS=.*|TSC_QUEUE_ROOTS=\"$roots\"|" "$CONF"
  say "managing: $roots"
fi

# ---- 4. wrap, and load the background jobs
if grep -qE '^TSC_QUEUE_(ROOTS|SCAN)="[^"]+"' "$CONF"; then
  "$TARGET" install
  "$TARGET" repair
  "$TARGET" watch
  echo
  "$TARGET" doctor || true
else
  echo
  say "No checkouts are configured yet. Two steps left:"
  echo "    1. set TSC_QUEUE_ROOTS (or TSC_QUEUE_SCAN) in $CONF"
  echo "    2. run: tsc-queue install && tsc-queue repair && tsc-queue watch"
fi

# ---- 5. PATH warning
case ":$PATH:" in
  *":$BIN:"*) ;;
  *)
    echo
    say "WARNING: $BIN is not on your PATH."
    echo "    Add this to ~/.zshrc:  export PATH=\"$BIN:\$PATH\""
    echo "    The queue itself still works: the shims call $TARGET by its full path."
    ;;
esac

echo
say "Done. Try: tsc-queue status --watch"
