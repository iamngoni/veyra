#!/usr/bin/env bash
# Installs only the host duties required by the hybrid Docker deployment:
# native MetaTrader 4 startup and verified backups of Docker PostgreSQL.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AGENTS="$HOME/Library/LaunchAgents"
LOGS="$HOME/Library/Logs/veyra"
TEMPLATES="$ROOT/scripts/launchd-docker"
DOMAIN="gui/$(id -u)"
LABELS=(
  cc.antonlabs.veyra.terminal
  cc.antonlabs.veyra.docker-backup
)
INCOMPATIBLE_LABELS=(
  cc.antonlabs.veyra.service
  cc.antonlabs.veyra.tunnel
  cc.antonlabs.veyra.console
  cc.antonlabs.veyra.alerts
  cc.antonlabs.veyra.backup
  cc.antonlabs.veyra.logrotate
)

uninstall() {
  for label in "${LABELS[@]}"; do
    launchctl bootout "$DOMAIN/$label" 2>/dev/null || true
    rm -f "$AGENTS/$label.plist"
  done
  echo "Veyra Docker host LaunchAgents removed."
}

if [ "${1:-}" = "--uninstall" ]; then
  uninstall
  exit 0
fi

if [ ! -d "/Applications/MetaTrader 4.app" ]; then
  echo "MetaTrader 4 is not installed in /Applications" >&2
  exit 1
fi
if ! docker info >/dev/null 2>&1; then
  echo "Docker is not available; start OrbStack or Docker Desktop first" >&2
  exit 1
fi
if ! (cd "$ROOT" && docker compose config --quiet); then
  echo "the Veyra Compose profile is invalid" >&2
  exit 1
fi

for label in "${INCOMPATIBLE_LABELS[@]}"; do
  if launchctl print "$DOMAIN/$label" >/dev/null 2>&1; then
    echo "refusing to mix Docker and host agents: $label is loaded" >&2
    exit 1
  fi
done

mkdir -p "$AGENTS" "$LOGS"
for template in "$TEMPLATES"/*.plist.template; do
  name="$(basename "$template" .template)"
  sed -e "s|__HOME__|$HOME|g" -e "s|__ROOT__|$ROOT|g" \
    "$template" > "$AGENTS/$name"
  plutil -lint "$AGENTS/$name" >/dev/null
  chmod 644 "$AGENTS/$name"
done

for label in "${LABELS[@]}"; do
  launchctl bootout "$DOMAIN/$label" 2>/dev/null || true
  launchctl bootstrap "$DOMAIN" "$AGENTS/$label.plist"
done

echo "Veyra Docker host LaunchAgents installed."
echo "logs: $LOGS"
