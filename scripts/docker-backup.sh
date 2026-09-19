#!/usr/bin/env bash
# Creates and verifies a custom-format PostgreSQL dump through the Compose
# database container, keeping archives on the macOS host for recovery and
# uploading them to R2 when the existing backup settings are configured.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${VEYRA_ENV_FILE:-$ROOT/.env}"
BACKUP_DIR="${VEYRA_BACKUP_DIR:-$HOME/Library/Application Support/veyra/backups}"
GENERATIONS="${VEYRA_BACKUP_GENERATIONS:-14}"

if [ ! -f "$ENV_FILE" ]; then
  echo "missing environment file: $ENV_FILE" >&2
  exit 1
fi

set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a

if ! [ "$GENERATIONS" -ge 1 ] 2>/dev/null; then
  echo "VEYRA_BACKUP_GENERATIONS must be a positive integer" >&2
  exit 1
fi

cd "$ROOT"
docker compose exec -T postgres pg_isready -U veyra -d veyra >/dev/null

mkdir -p "$BACKUP_DIR"
stamp="$(date +%Y%m%d-%H%M%S)"
target="$BACKUP_DIR/veyra-$stamp.dump"
partial="$target.partial"

cleanup() {
  if [ -f "$partial" ]; then
    rm -f "$partial"
  fi
}
trap cleanup EXIT

docker compose exec -T postgres \
  pg_dump --username=veyra --dbname=veyra --format=custom --no-owner \
  > "$partial"
docker compose exec -T postgres pg_restore --list < "$partial" >/dev/null
mv -f "$partial" "$target"

size="$(stat -f%z "$target")"
echo "backup ok: $target ($size bytes)"

ls -1t "$BACKUP_DIR"/veyra-*.dump 2>/dev/null \
  | tail -n "+$((GENERATIONS + 1))" \
  | while IFS= read -r old; do
      rm -f "$old"
      echo "pruned $old"
    done

# Match the host backup path: an off-machine failure is reported but never
# invalidates the already verified local archive.
REMOTE_BUCKET="${VEYRA_BACKUP_R2_BUCKET:-}"
if [ -n "$REMOTE_BUCKET" ]; then
  wrangler_bin="${VEYRA_WRANGLER_BIN:-}"
  if [ -z "$wrangler_bin" ]; then
    wrangler_bin="$(command -v wrangler 2>/dev/null || true)"
  fi
  if [ -z "$wrangler_bin" ] || [ ! -x "$wrangler_bin" ]; then
    echo "wrangler not found; skipping off-machine upload of $(basename "$target")" >&2
  else
    export PATH="$(dirname "$wrangler_bin"):$PATH"
    if "$wrangler_bin" r2 object put \
      "$REMOTE_BUCKET/$(basename "$target")" --file "$target" --remote \
      >/dev/null 2>&1; then
      date +%s > "$BACKUP_DIR/.last-remote-upload"
      echo "uploaded $(basename "$target") to r2://$REMOTE_BUCKET"
    else
      echo "off-machine upload failed for $(basename "$target"); the local dump is intact" >&2
    fi
  fi
fi
