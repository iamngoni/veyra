#!/usr/bin/env bash
# Deploys origin/main to the Raspberry Pi, and nothing else.
#
# The Pi runs the backend for real; it must only ever run what is on main.
# This script exports origin/main from git (not your working tree, so a
# feature branch or uncommitted edits can never leak onto the Pi), syncs it
# without touching the Pi's own .env, rebuilds the image there, restarts, and
# records the deployed commit in ~/veyra/.deployed-commit.
#
#   scripts/deploy-pi.sh            backend only
#   scripts/deploy-pi.sh --console  backend, then the Cloudflare dashboard
set -euo pipefail

HOST="${VEYRA_PI_HOST:-ras@100.118.226.126}"
KEY="${VEYRA_PI_KEY:-$HOME/.ssh/id_ed25519_pi}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ssh_pi() { ssh -o BatchMode=yes -o IdentitiesOnly=yes -i "$KEY" "$HOST" "$@"; }

cd "$ROOT"
git fetch --quiet origin main
COMMIT="$(git rev-parse origin/main)"
echo "Deploying origin/main @ ${COMMIT:0:7}: $(git log -1 --format=%s "$COMMIT")"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
git archive "$COMMIT" | tar -x -C "$STAGE"

rsync -a --delete -e "ssh -o IdentitiesOnly=yes -i $KEY" \
  --exclude='.env' --exclude='.env.*' --exclude='.deployed-commit' \
  "$STAGE"/ "$HOST":veyra/

ssh_pi 'cd veyra && docker compose build --build-arg BUILD_JOBS=2 veyra && docker compose up --detach --no-build veyra watchdog'
ssh_pi "echo $COMMIT > veyra/.deployed-commit"

for _ in $(seq 1 30); do
  [ "$(ssh_pi 'docker inspect -f "{{.State.Health.Status}}" veyra-veyra-1' 2>/dev/null)" = healthy ] && break
  sleep 4
done
ssh_pi 'cd veyra && docker compose exec -T veyra curl -s http://127.0.0.1:8080/ready'
echo

if [ "${1:-}" = "--console" ]; then
  # Build the dashboard from the same exported tree.
  (cd "$STAGE/console" && npm ci --silent && VEYRA_TARGET=cloudflare npm run build && npx wrangler deploy)
fi
echo "Pi is running main @ ${COMMIT:0:7}"
