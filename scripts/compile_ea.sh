#!/usr/bin/env bash
# Compiles ea/VeyraProbe.mq4 into the locally installed MT4 terminal (Wine).
#
# The versioned source keeps a __VEYRA_TOKEN__ placeholder; the real token is
# injected from the environment at compile time, so no secret is committed.
# Run: `set -a; source .env; set +a; ./scripts/compile_ea.sh`
set -euo pipefail
cd "$(dirname "$0")/.."

TOKEN="${VEYRA_EA_TOKEN:?VEYRA_EA_TOKEN must be set (source .env first)}"
WP="${MT4_WINEPREFIX:-$HOME/Library/Application Support/net.metaquotes.wine.MetaTrader4}"
SUPPORT="${MT4_SUPPORT:-/Applications/MetaTrader 4.app/Contents/SharedSupport/wine}"
MT4_DIR="$WP/drive_c/Program Files (x86)/MetaTrader 4"

if [ ! -d "$MT4_DIR" ]; then
  echo "MT4 installation not found at: $MT4_DIR" >&2
  exit 1
fi

sed "s/__VEYRA_TOKEN__/$TOKEN/" ea/VeyraProbe.mq4 > "$MT4_DIR/MQL4/Experts/VeyraProbe.mq4"

cat > "$WP/drive_c/veyra_compile.bat" <<'BAT'
@echo off
"C:\Program Files (x86)\MetaTrader 4\metaeditor.exe" /compile:"C:\Program Files (x86)\MetaTrader 4\MQL4\Experts\VeyraProbe.mq4" /log:"C:\veyra_compile.log"
BAT

# MetaEditor returns a non-zero exit code even on success in this Wine build,
# so the log and the produced .ex4 are the authoritative result.
WINEPREFIX="$WP" DYLD_FALLBACK_LIBRARY_PATH="$SUPPORT/lib/external" WINEDEBUG=-all \
  "$SUPPORT/bin/wine64" cmd /c 'C:\veyra_compile.bat' >/dev/null 2>&1 || true

LOG="$WP/drive_c/veyra_compile.log"
if [ ! -f "$LOG" ]; then
  echo "MetaEditor produced no compile log" >&2
  exit 1
fi

RESULT="$(iconv -f UTF-16 -t UTF-8 "$LOG" 2>/dev/null || cat "$LOG")"
echo "$RESULT"

if ! grep -q "0 errors" <<<"$RESULT"; then
  echo "EA compilation failed" >&2
  exit 1
fi
if [ ! -f "$MT4_DIR/MQL4/Experts/VeyraProbe.ex4" ]; then
  echo "compile reported success but VeyraProbe.ex4 is missing" >&2
  exit 1
fi

echo "EA compiled: $MT4_DIR/MQL4/Experts/VeyraProbe.ex4"
