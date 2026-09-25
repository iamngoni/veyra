#!/usr/bin/env bash
# Runs the Veyra service in a supervised context (LaunchAgent or terminal).
# Arguments pass through, so `run-service.sh watchdog` runs the outage
# watchdog with the same environment.
#
# launchd cannot read dotenv files, so this wrapper sources .env for the
# process environment and execs the release binary, letting the supervised PID
# be the service itself. Secrets therefore stay in .env (0600) and never appear
# in a plist.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${VEYRA_ENV_FILE:-$ROOT/.env}"
BINARY="${VEYRA_BINARY:-$ROOT/target/release/veyra-service}"

if [ ! -f "$ENV_FILE" ]; then
  echo "missing environment file: $ENV_FILE" >&2
  exit 1
fi
if [ ! -x "$BINARY" ]; then
  echo "missing release binary: $BINARY (run: cargo build --release)" >&2
  exit 1
fi

set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a

exec "$BINARY" "$@"
