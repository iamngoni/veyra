#!/usr/bin/env bash
# Installs (or removes) the Veyra LaunchAgents that keep the stack running.
#
#   scripts/install-launchd.sh              # build-free install from current release binary
#   scripts/install-launchd.sh --minimal    # only the service and the tunnel
#   scripts/install-launchd.sh --uninstall  # remove agents and stop the supervised stack
#
# --minimal is for a setup where the console runs on Cloudflare and Postgres in
# Docker: it skips the local console server, the MT4 terminal watcher, the
# outage watchdog, backups, and log rotation, and needs none of their files.
#
# The installer renders scripts/launchd/*.plist.template with this checkout's
# absolute paths, so the committed templates stay portable. It stops stray
# service/tunnel processes first: exactly one supervised instance must own
# ports 8080 and 7801.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AGENTS="$HOME/Library/LaunchAgents"
LOGS="$HOME/Library/Logs/veyra"
TEMPLATES="$ROOT/scripts/launchd"
DOMAIN="gui/$(id -u)"
LABELS=(
  cc.antonlabs.veyra.terminal
  cc.antonlabs.veyra.tunnel
  cc.antonlabs.veyra.service
  cc.antonlabs.veyra.logrotate
  cc.antonlabs.veyra.backup
  cc.antonlabs.veyra.console
  cc.antonlabs.veyra.alerts
)

ALL_LABELS=("${LABELS[@]}")
if [ "${1:-}" = "--minimal" ]; then
  LABELS=(
    cc.antonlabs.veyra.tunnel
    cc.antonlabs.veyra.service
  )
elif [ -n "${1:-}" ] && [ "${1:-}" != "--uninstall" ]; then
  echo "usage: $0 [--minimal|--uninstall]" >&2
  exit 2
fi

uninstall() {
  for label in "${ALL_LABELS[@]}"; do
    launchctl bootout "$DOMAIN/$label" 2>/dev/null || true
    rm -f "$AGENTS/$label.plist"
  done
  echo "Veyra LaunchAgents removed; supervised processes stopped."
}

if [ "${1:-}" = "--uninstall" ]; then
  uninstall
  exit 0
fi

if [ ! -x "$ROOT/target/release/veyra-service" ]; then
  echo "missing $ROOT/target/release/veyra-service; run: cargo build --release" >&2
  exit 1
fi

if printf '%s\n' "${LABELS[@]}" | grep -q 'veyra.tunnel$' && [ ! -e "$HOME/.cloudflared/veyra-config.yml" ]; then
  echo "missing $HOME/.cloudflared/veyra-config.yml (the tunnel agent reads it)." >&2
  echo "if your config is config.yml: ln -s config.yml $HOME/.cloudflared/veyra-config.yml" >&2
  exit 1
fi

mkdir -p "$AGENTS" "$LOGS"

for label in "${LABELS[@]}"; do
  template="$TEMPLATES/$label.plist.template"
  name="$label.plist"
  sed -e "s|__HOME__|$HOME|g" -e "s|__ROOT__|$ROOT|g" "$template" > "$AGENTS/$name"
  chmod 644 "$AGENTS/$name"
done
echo "rendered ${#LABELS[@]} LaunchAgents into $AGENTS"

# One supervised owner per port: stop session-bound instances first.
pkill -f 'target/(debug|release)/veyra-service' 2>/dev/null && echo "stopped stray veyra-service" || true
pkill -f 'cloudflared .*tunnel run veyra' 2>/dev/null && echo "stopped stray tunnel" || true
sleep 1

for label in "${LABELS[@]}"; do
  launchctl bootout "$DOMAIN/$label" 2>/dev/null || true
done
# launchd occasionally returns a transient "Input/output error" while a
# previous instance is still tearing down; one short retry makes the installer
# idempotent in practice.
for label in "${LABELS[@]}"; do
  if ! launchctl bootstrap "$DOMAIN" "$AGENTS/$label.plist" 2>/dev/null; then
    sleep 2
    launchctl bootstrap "$DOMAIN" "$AGENTS/$label.plist"
  fi
done

sleep 3
launchctl print "$DOMAIN/cc.antonlabs.veyra.service" 2>/dev/null | grep -E '^\s+(state|pid) = ' | head -2 || true
echo "logs: $LOGS"
