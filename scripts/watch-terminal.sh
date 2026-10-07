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
# A running terminal whose EA Veyra has not heard from is restarted too, which
# is what clears the EA after a network drop or an update. The signal is
# Veyra's own: the service's `/account` reports whether the EA's polls are
# fresh (read through the console container, VEYRA_MT4_LINK_CHECK). The
# terminal is restarted only when that answer says stale for
# VEYRA_MT4_STALE_SECS in a row, the terminal has been up at least that long,
# and the EA's endpoint (VEYRA_EA_URL, read from the repository's .env) is
# reachable from this Mac, so an internet outage is waited out rather than
# restarted through. After a restart it waits RESTART_GAP_SECS before another;
# after three restarts without the link coming back, an hour. No answer from
# Veyra at all (Docker down) never restarts anything. The EA's log file is not
# used: since build 1490 the terminal writes it to disk only occasionally, and
# a restart rule based on it looped.
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
STALE_SECS="${VEYRA_MT4_STALE_SECS:-600}"
RESTART_GAP_SECS=900
RESTART_BACKOFF_SECS=3600
export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"
LINK_CHECK="${VEYRA_MT4_LINK_CHECK:-docker exec veyra-console-1 wget -qO- -T 5 http://veyra:8080/account}"
REPO_ENV="$(cd "$(dirname "$0")/.." && pwd)/.env"
EA_URL="${VEYRA_EA_URL:-$(/usr/bin/sed -n 's/^VEYRA_EA_URL=//p' "$REPO_ENV" 2>/dev/null | /usr/bin/head -n 1)}"

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

# Prints `fresh` or `stale` from Veyra's view of the EA; nothing when Veyra
# cannot be asked.
link_state() {
  local answer
  answer="$($LINK_CHECK 2>/dev/null)" || return 0
  case "$answer" in
    *'"fresh":true'*) echo fresh ;;
    *'"fresh":false'*) echo stale ;;
  esac
}

# Whether the EA's endpoint answers from this Mac (any HTTP status counts).
endpoint_reachable() {
  [ -n "$EA_URL" ] || return 1
  local code
  code="$(/usr/bin/curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$EA_URL" 2>/dev/null)"
  [ -n "$code" ] && [ "$code" != "000" ]
}

restart_terminal() {
  /usr/bin/osascript -e "quit app \"${APP_NAME}\"" >/dev/null 2>&1
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -z "$(terminal_pid)" ] && return
    /bin/sleep 2
  done
  /usr/bin/pkill -f -- "$PROCESS_PATTERN"
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

watched_pid=""
watched_since=0
stale_since=0
last_restart=0
failed_restarts=0
while :; do
  pid="$(terminal_pid)"
  if [ -z "$pid" ]; then
    log "MetaTrader terminal process is absent; reopening"
    open_terminal
    /bin/sleep "$START_GRACE_SECS"
    continue
  fi
  now="$(/bin/date +%s)"
  if [ "$pid" != "$watched_pid" ]; then
    watched_pid="$pid"
    watched_since="$now"
    stale_since=0
  fi
  case "$(link_state)" in
    fresh)
      stale_since=0
      failed_restarts=0
      ;;
    stale)
      [ "$stale_since" -eq 0 ] && stale_since="$now"
      gap="$RESTART_GAP_SECS"
      [ "$failed_restarts" -ge 3 ] && gap="$RESTART_BACKOFF_SECS"
      if [ $(( now - stale_since )) -ge "$STALE_SECS" ] \
        && [ $(( now - watched_since )) -ge "$STALE_SECS" ] \
        && [ $(( now - last_restart )) -ge "$gap" ]; then
        if endpoint_reachable; then
          failed_restarts=$(( failed_restarts + 1 ))
          last_restart="$now"
          log "Veyra has not heard the EA for $(( now - stale_since ))s while ${EA_URL} answers; restarting terminal ${pid} (attempt ${failed_restarts})"
          restart_terminal
          continue
        fi
      fi
      ;;
  esac
  /bin/sleep "$CHECK_SECS"
done
