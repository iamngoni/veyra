#!/usr/bin/env bash
# Keeps the native MetaTrader terminal alive for unattended Veyra operation.
#
# The macOS app launcher exits as soon as it has asked LaunchServices to open
# MT4, so launchd cannot supervise terminal.exe directly. This long-running
# wrapper watches the actual Wine process and reopens the app when it vanishes.
set -u

APP_NAME="${VEYRA_MT4_APP_NAME:-MetaTrader 4}"
PROCESS_PATTERN="${VEYRA_MT4_PROCESS_PATTERN:-[t]erminal\.exe}"
CHECK_SECS="${VEYRA_MT4_CHECK_SECS:-15}"
START_GRACE_SECS="${VEYRA_MT4_START_GRACE_SECS:-30}"

log() {
  printf '%s %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"
}

terminal_running() {
  /usr/bin/pgrep -f -- "$PROCESS_PATTERN" >/dev/null 2>&1
}

while :; do
  if terminal_running; then
    /bin/sleep "$CHECK_SECS"
    continue
  fi

  log "MetaTrader terminal process is absent; opening ${APP_NAME}"
  if /usr/bin/open -a "$APP_NAME"; then
    /bin/sleep "$START_GRACE_SECS"
  else
    log "failed to open ${APP_NAME}; retrying"
    /bin/sleep "$CHECK_SECS"
  fi
done
