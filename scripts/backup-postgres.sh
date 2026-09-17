#!/usr/bin/env bash
# Backs up the Veyra audit database with pg_dump, verifies the archive,
# prunes old generations, and uploads each fresh dump off-machine to R2 via
# `wrangler` when a bucket is configured. launchd runs this daily via
# cc.antonlabs.veyra.backup.
#
# The dump is written to a .partial file, verified with `pg_restore --list`
# (an archive that cannot be listed cannot be restored), and only then moved
# into place, so a failed or corrupt run never replaces a good generation.
# The remote upload is best-effort: a network or auth failure never fails the
# local backup, it only logs. Remote pruning is deliberately absent (dumps are
# tiny); use an R2 lifecycle rule if that ever matters.
# Tunables, also used by tests:
#   VEYRA_BACKUP_DIR=... VEYRA_BACKUP_GENERATIONS=2 scripts/backup-postgres.sh
#
# PostgreSQL tools come from Homebrew's postgresql@17 by default; point
# VEYRA_PG_BIN elsewhere (or have pg_dump/pg_restore on PATH) if that moves.
# `VEYRA_WRANGLER_BIN` should point at wrangler (its directory is added to
# PATH so the node shebang resolves under launchd).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${VEYRA_ENV_FILE:-$ROOT/.env}"
PG_BIN="${VEYRA_PG_BIN:-/opt/homebrew/opt/postgresql@17/bin}"
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

if [ -z "${VEYRA_DATABASE_URL:-}" ]; then
  echo "VEYRA_DATABASE_URL is not configured; nothing to back up" >&2
  exit 1
fi
if ! [ "$GENERATIONS" -ge 1 ] 2>/dev/null; then
  echo "VEYRA_BACKUP_GENERATIONS must be a positive integer" >&2
  exit 1
fi

pg_dump="$PG_BIN/pg_dump"
pg_restore="$PG_BIN/pg_restore"
[ -x "$pg_dump" ] || pg_dump="$(command -v pg_dump 2>/dev/null || true)"
[ -x "$pg_restore" ] || pg_restore="$(command -v pg_restore 2>/dev/null || true)"
if [ -z "$pg_dump" ] || [ -z "$pg_restore" ]; then
  echo "pg_dump/pg_restore not found; set VEYRA_PG_BIN or add PostgreSQL to PATH" >&2
  exit 1
fi

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

# Never echo the URL: it may carry credentials.
"$pg_dump" --dbname="$VEYRA_DATABASE_URL" --format=custom --no-owner --file="$partial"
"$pg_restore" --list "$partial" > /dev/null

mv -f "$partial" "$target"
size=$(stat -f%z "$target")
echo "backup ok: $target (${size} bytes)"

ls -1t "$BACKUP_DIR"/veyra-*.dump 2>/dev/null | tail -n +"$((GENERATIONS + 1))" | while read -r old; do
  rm -f "$old"
  echo "pruned $old"
done

# Off-machine copy: best-effort, never fails the local backup.
REMOTE_BUCKET="${VEYRA_BACKUP_R2_BUCKET:-}"
if [ -n "$REMOTE_BUCKET" ]; then
  wrangler_bin="${VEYRA_WRANGLER_BIN:-}"
  if [ -z "$wrangler_bin" ]; then
    wrangler_bin="$(command -v wrangler 2>/dev/null || true)"
  fi
  if [ -z "$wrangler_bin" ] || [ ! -x "$wrangler_bin" ]; then
    echo "wrangler not found; skipping off-machine upload of $(basename "$target")" >&2
  else
    # The wrangler shebang resolves node via PATH; launchd starts minimal.
    export PATH="$(dirname "$wrangler_bin"):$PATH"
    if "$wrangler_bin" r2 object put "$REMOTE_BUCKET/$(basename "$target")" --file "$target" --remote >/dev/null 2>&1; then
      date +%s > "$BACKUP_DIR/.last-remote-upload"
      echo "uploaded $(basename "$target") to r2://$REMOTE_BUCKET"
    else
      echo "off-machine upload failed for $(basename "$target"); the local dump is intact" >&2
    fi
  fi
fi
