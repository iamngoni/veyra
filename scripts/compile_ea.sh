#!/usr/bin/env bash
# Compiles ea/VeyraProbe.mq4 into the locally installed MT4 terminal (Wine).
#
# The versioned source keeps __VEYRA_URL__, __VEYRA_TOKEN__, and
# __VEYRA_ALLOW_LIVE__ placeholders; all three are injected from the
# environment at compile time, so no secret or environment hostname is
# committed. VEYRA_EA_URL defaults to the loopback endpoint; set it to the
# tunnel URL (https://veyra.antonlabs.cc/ea/poll) when the terminal reaches
# Veyra through Cloudflare instead of localhost. VEYRA_EA_ALLOW_LIVE defaults
# to false, so a fresh setup compiles an EA that only reports dry runs; set it
# to true once live trading is approved. The script prints which one it
# compiled.
# Run: `set -a; source .env; set +a; ./scripts/compile_ea.sh`
set -euo pipefail
cd "$(dirname "$0")/.."

TOKEN="${VEYRA_EA_TOKEN:?VEYRA_EA_TOKEN must be set (source .env first)}"
URL="${VEYRA_EA_URL:-http://127.0.0.1:7801/ea/poll}"
# Defaults to disarmed. This is the compiled-in default for the EA's
# InAllowLiveOrders input, not the last word on it: the input stays editable in
# the terminal's EA properties without recompiling, and the service still has
# to agree before any command reaches the terminal. Arming is an explicit
# choice, so an unset or forgotten value never produces a live EA.
ALLOW_LIVE="${VEYRA_EA_ALLOW_LIVE:-false}"
case "$ALLOW_LIVE" in
  true|false) ;;
  *) echo "VEYRA_EA_ALLOW_LIVE must be true or false" >&2; exit 1 ;;
esac
echo "compiling EA with live order placement: $ALLOW_LIVE" >&2
WP="${MT4_WINEPREFIX:-$HOME/Library/Application Support/net.metaquotes.wine.MetaTrader4}"
SUPPORT="${MT4_SUPPORT:-/Applications/MetaTrader 4.app/Contents/SharedSupport/wine}"
MT4_DIR="$WP/drive_c/Program Files (x86)/MetaTrader 4"

if [ ! -d "$MT4_DIR" ]; then
  echo "MT4 installation not found at: $MT4_DIR" >&2
  exit 1
fi

sed -e "s|__VEYRA_URL__|$URL|" -e "s/__VEYRA_TOKEN__/$TOKEN/" \
  -e "s/__VEYRA_ALLOW_LIVE__/$ALLOW_LIVE/" \
  ea/VeyraProbe.mq4 > "$MT4_DIR/MQL4/Experts/VeyraProbe.mq4"

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

if [ "$ALLOW_LIVE" = "true" ]; then
  echo "EA compiled ARMED (InAllowLiveOrders=true): live orders will be placed on approved commands."
else
  echo "EA compiled disarmed (InAllowLiveOrders=false): execution commands report dry runs."
fi
echo "EA compiled: $MT4_DIR/MQL4/Experts/VeyraProbe.ex4"
