#!/usr/bin/env bash
# Keeps the native MetaTrader terminal alive, and running the Veyra EA, for
# unattended operation.
#
# The macOS app launcher exits as soon as it has asked LaunchServices to open
# MT4, so launchd cannot supervise terminal.exe directly. This long-running
# wrapper watches the actual Wine process and reopens the terminal when it
# exits (a crash, or the vendor's LiveUpdate closing it).
#
# On reopening, when no chart of the last-used profile carries the EA (the
# vendor's LiveUpdate has come back with it detached from every chart), the
# terminal is started with a startup configuration (MT4 "Configuration at
# Startup") naming the EA, VEYRA_MT4_EA_SYMBOL and VEYRA_MT4_EA_PERIOD; it
# carries no credentials, and the EA's inputs keep their compiled defaults.
# Otherwise the app opens normally and its saved charts bring the EA back.
# Unverified: on 2026-10-05 a start with this file did not show the EA as
# loaded, so a detached EA may still need attaching by hand; the terminal's
# log says which.
#
# The watcher does not restart a running terminal whose EA has gone quiet:
# the EA's log file is not a liveness signal (since build 1490 the terminal
# writes it to disk only occasionally), and restarting on it looped.
set -u

APP_NAME="${VEYRA_MT4_APP_NAME:-MetaTrader 4}"
PROCESS_PATTERN="${VEYRA_MT4_PROCESS_PATTERN:-[t]erminal\.exe}"
CHECK_SECS="${VEYRA_MT4_CHECK_SECS:-15}"
START_GRACE_SECS="${VEYRA_MT4_START_GRACE_SECS:-30}"
EA_NAME="${VEYRA_MT4_EA:-VeyraProbe}"
EA_SYMBOL="${VEYRA_MT4_EA_SYMBOL:-EURUSD}"
EA_PERIOD="${VEYRA_MT4_EA_PERIOD:-H4}"
WINE_PREFIX="${MT4_WINEPREFIX:-$HOME/Library/Application Support/net.metaquotes.wine.MetaTrader4}"
WINE_SUPPORT="${MT4_SUPPORT:-/Applications/MetaTrader 4.app/Contents/SharedSupport/wine}"
MT4_DIR="$WINE_PREFIX/drive_c/Program Files (x86)/MetaTrader 4"
STARTUP_FILE="$WINE_PREFIX/drive_c/veyra_startup.ini"

log() {
  printf '%s %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"
}

terminal_pid() {
  /usr/bin/pgrep -f -- "$PROCESS_PATTERN" 2>/dev/null | /usr/bin/head -n 1
}

# Whether a chart of the last-used profile carries the EA. Profiles are saved
# when the terminal closes, so this reads what the next start will load.
profile_has_ea() {
  local profile charts chart
  profile="$(/usr/bin/tr -d '\000\r' < "$MT4_DIR/profiles/lastprofile.ini" 2>/dev/null)"
  charts="$MT4_DIR/profiles/${profile:-default}"
  for chart in "$charts"/*.chr "$charts"/*.CHR; do
    [ -f "$chart" ] || continue
    if /usr/bin/tr -d '\000' < "$chart" | /usr/bin/grep -qi "^name=${EA_NAME}"; then
      return 0
    fi
  done
  return 1
}

open_terminal() {
  if profile_has_ea; then
    log "opening ${APP_NAME}; its saved charts carry ${EA_NAME}"
    /usr/bin/open -a "$APP_NAME"
    return
  fi
  log "opening ${APP_NAME} with ${EA_NAME} attached to ${EA_SYMBOL} ${EA_PERIOD}; no saved chart carries it"
  printf '[StartUp]\r\nExpert=%s\r\nSymbol=%s\r\nPeriod=%s\r\n' "$EA_NAME" "$EA_SYMBOL" "$EA_PERIOD" > "$STARTUP_FILE"
  WINEPREFIX="$WINE_PREFIX" DYLD_FALLBACK_LIBRARY_PATH="$WINE_SUPPORT/lib/external" WINEDEBUG=-all \
    /usr/bin/nohup "$WINE_SUPPORT/bin/wine64" 'C:\Program Files (x86)\MetaTrader 4\terminal.exe' \
    /portable '/config:C:\veyra_startup.ini' >/dev/null 2>&1 &
}

while :; do
  if [ -n "$(terminal_pid)" ]; then
    /bin/sleep "$CHECK_SECS"
    continue
  fi
  log "MetaTrader terminal process is absent; reopening"
  open_terminal
  /bin/sleep "$START_GRACE_SECS"
done
