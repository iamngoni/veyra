#!/usr/bin/env bash
# Checks a machine for everything the Veyra stack needs to run.
#
# Run this on the target of a migration before installing the LaunchAgents:
# it only reads state and reports, so it is always safe. Required checks that
# fail make the script exit non-zero; informational notes do not.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
PASS=0
FAIL=0
NOTES=()

ok()   { PASS=$((PASS + 1)); printf '[pass] %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '[fail] %s\n' "$1"; }
note() { NOTES+=("$1"); printf '[note] %s\n' "$1"; }

echo "== veyra preflight ($ROOT)"
echo

[ "$(uname -s)" = "Darwin" ] && ok "macOS host" || bad "macOS host (this stack is macOS + launchd)"

if command -v brew >/dev/null 2>&1; then
  ok "Homebrew ($(command -v brew))"
else
  bad "Homebrew (needed for postgresql@17 and cloudflared)"
fi

PG_BIN="/opt/homebrew/opt/postgresql@17/bin"
if [ -x "$PG_BIN/psql" ]; then
  ok "PostgreSQL 17 client ($PG_BIN)"
else
  bad "PostgreSQL 17 (brew install postgresql@17)"
fi
if [ -x "$PG_BIN/pg_isready" ] && "$PG_BIN/pg_isready" -h 127.0.0.1 -q 2>/dev/null; then
  ok "PostgreSQL accepting connections on 127.0.0.1"
else
  bad "PostgreSQL not running (brew services start postgresql@17)"
fi

if command -v node >/dev/null 2>&1; then
  node_major="$(node --version | sed 's/^v//' | cut -d. -f1)"
  if [ "${node_major:-0}" -ge 20 ]; then
    ok "Node.js $(node --version)"
  else
    bad "Node.js >= 20 (found $(node --version))"
  fi
else
  bad "Node.js >= 20 (brew install node)"
fi
command -v npm >/dev/null 2>&1 && ok "npm $(npm --version)" || bad "npm"

if command -v cargo >/dev/null 2>&1; then
  ok "Rust toolchain ($(cargo --version))"
else
  bad "Rust toolchain (rustup, needed to build the service)"
fi

if [ -d "/Applications/MetaTrader 4.app" ]; then
  ok "MetaTrader 4.app installed on the host"
else
  bad "MetaTrader 4.app (install the terminal before migrating the EA)"
fi
if [ -d "$HOME/Library/Application Support/net.metaquotes.wine.MetaTrader4" ]; then
  note "legacy MT4 Wine prefix present; native installations may use another data location"
else
  note "native MT4 data location must be verified in the terminal before attaching VeyraProbe"
fi

if command -v cloudflared >/dev/null 2>&1; then
  ok "cloudflared ($(command -v cloudflared))"
else
  bad "cloudflared (brew install cloudflared)"
fi
TUNNEL_CONFIG="$HOME/.cloudflared/veyra-config.yml"
if [ -f "$TUNNEL_CONFIG" ]; then
  ok "tunnel config $TUNNEL_CONFIG"
  credentials="$(awk '/credentials-file:/ {print $2}' "$TUNNEL_CONFIG")"
  if [ -n "$credentials" ] && [ -f "$credentials" ]; then
    ok "tunnel credentials $(basename "$credentials")"
  else
    bad "tunnel credentials referenced by the config are missing"
  fi
else
  bad "tunnel config (create it after 'cloudflared tunnel login'; see docs/deployment.md)"
fi

if [ -f "$ROOT/.env" ]; then
  ok "environment file .env"
else
  bad ".env (copy it securely from the current machine; see docs/deployment.md)"
fi

for port in 8080 7801 3000; do
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    note "port $port already has a listener (ok if this stack is running)"
  else
    ok "port $port free"
  fi
done

[ -d "$ROOT/console/node_modules" ] && ok "console dependencies installed" \
  || note "console dependencies missing (cd console && npm install)"

[ -d "$ROOT/target/release" ] && ok "release build present" \
  || note "release build missing (cargo build --release)"

available_kb="$(df -k "$ROOT" | awk 'NR==2 {print $4}')"
# 2 GiB floor: backups, logs, and the build tree need room.
if [ "${available_kb:-0}" -gt 2097152 ]; then
  ok "disk space ($((available_kb / 1024)) MiB free)"
else
  note "low disk space ($((available_kb / 1024)) MiB free); backups and logs need room"
fi

echo
echo "== $PASS passed, $FAIL failed, ${#NOTES[@]} notes"
if [ "$FAIL" -gt 0 ]; then
  echo "PREFLIGHT_FAILED"
  exit 1
fi
echo "PREFLIGHT_OK"
