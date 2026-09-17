#!/usr/bin/env bash
# Exercises the real binary, all diagnostic routes, and SIGINT shutdown.
# The binary is launched directly so the signal reaches it, not a cargo parent.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${VEYRA_SMOKE_PORT:-18081}"
BASE="http://127.0.0.1:${PORT}"

cargo build --quiet -p veyra-service
VEYRA_BIND_HOST=127.0.0.1 VEYRA_BIND_PORT="${PORT}" VEYRA_ENV=development \
  ./target/debug/veyra-service &
pid=$!
cleanup() {
  if kill -0 "$pid" 2>/dev/null; then
    kill -INT "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

ready=0
for _ in $(seq 1 100); do
  if curl --fail --silent --output /dev/null "${BASE}/health"; then ready=1; break; fi
  if ! kill -0 "$pid" 2>/dev/null; then echo "server exited before readiness" >&2; exit 1; fi
  sleep 0.1
done
if [ "$ready" -ne 1 ]; then echo "server did not become ready" >&2; exit 1; fi

for route in health ready status; do
  echo "== /${route}"
  curl --fail --silent --show-error "${BASE}/${route}"
  echo
done

kill -INT "$pid"
wait "$pid"
trap - EXIT
echo "SMOKE_OK"
