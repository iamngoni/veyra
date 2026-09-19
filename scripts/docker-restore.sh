#!/usr/bin/env bash
# Restores one verified custom-format dump into a fresh Docker PostgreSQL
# database. It refuses to merge into a populated schema.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DUMP="${1:-}"

if [ -z "$DUMP" ]; then
  echo "usage: $0 /absolute/path/to/veyra.dump" >&2
  exit 2
fi
if [ ! -f "$DUMP" ]; then
  echo "dump does not exist: $DUMP" >&2
  exit 1
fi
if [ ! -f "$ROOT/.env" ]; then
  echo "missing environment file: $ROOT/.env" >&2
  exit 1
fi

cd "$ROOT"
docker compose up --detach postgres

for _ in $(seq 1 30); do
  if docker compose exec -T postgres pg_isready -U veyra -d veyra >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
docker compose exec -T postgres pg_isready -U veyra -d veyra >/dev/null

table_count="$(docker compose exec -T postgres \
  psql -U veyra -d veyra -Atqc \
  "select count(*) from pg_tables where schemaname = 'public';")"
if [ "$table_count" != "0" ]; then
  echo "refusing to restore into a populated database ($table_count public tables)" >&2
  exit 1
fi

docker compose exec -T postgres \
  pg_restore --username=veyra --dbname=veyra --no-owner --no-privileges --exit-on-error \
  < "$DUMP"

restored_tables="$(docker compose exec -T postgres \
  psql -U veyra -d veyra -Atqc \
  "select count(*) from pg_tables where schemaname = 'public';")"
echo "restore ok: $restored_tables public tables"
