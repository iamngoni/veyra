#!/usr/bin/env bash
# Rotates the supervised service's log files.
#
# launchd keeps the log file descriptors open, so rotation copies the current
# file and truncates it in place (copy-truncate) rather than renaming it away.
# Threshold and generation count are configurable for tests:
#   VEYRA_LOG_MAX_BYTES=1024 VEYRA_LOG_GENERATIONS=3 scripts/rotate-logs.sh
set -euo pipefail

LOGS="${VEYRA_LOG_DIR:-$HOME/Library/Logs/veyra}"
MAX_BYTES="${VEYRA_LOG_MAX_BYTES:-5242880}"
GENERATIONS="${VEYRA_LOG_GENERATIONS:-3}"

[ -d "$LOGS" ] || exit 0

for file in "$LOGS"/*.log; do
  [ -f "$file" ] || continue
  size=$(stat -f%z "$file")
  [ "$size" -gt "$MAX_BYTES" ] || continue

  index=$((GENERATIONS - 1))
  while [ "$index" -ge 1 ]; do
    if [ -f "$file.$index" ]; then
      mv -f "$file.$index" "$file.$((index + 1))"
    fi
    index=$((index - 1))
  done

  cp "$file" "$file.1"
  : > "$file"
  echo "rotated $file (${size} bytes)"
done
