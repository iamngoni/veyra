#!/usr/bin/env bash
# Serves the built Veyra console through Vite's preview server.
#
# Build first (`npm run build`); this script only serves. It binds 127.0.0.1 so
# browsers that resolve `localhost` to IPv4 reach it, and it proxies /api to
# the loopback service (see vite.config.ts).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${VEYRA_CONSOLE_PORT:-3000}"

# launchd starts with a minimal PATH; Homebrew's node/npm live here.
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"

cd "$ROOT"
if [ ! -d dist ]; then
  echo "missing console/dist; run: npm run build" >&2
  exit 1
fi

exec npx vite preview --port "$PORT" --host 127.0.0.1
