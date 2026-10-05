#!/usr/bin/env bash
# Starts, stops, or reports on the whole local Veyra stack in dependency order:
# Docker Desktop -> Postgres container -> Veyra service -> Cloudflare tunnel.
#
# Usage: scripts/start.sh [start|stop|restart|status]   (default: start)
#
# Safe to run repeatedly: a part that is already healthy is left alone. The
# service starts exactly as `scripts/run-service.sh` does, so the safety flags
# in .env (autopilot, live execution) are what decide whether anything trades;
# this script never changes them. The console is not started here because it
# runs on Cloudflare (see start.md). Logs go to ~/Library/Logs/veyra.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOGS="${VEYRA_LOG_DIR:-$HOME/Library/Logs/veyra}"
API="http://127.0.0.1:8080"
PG_CONTAINER="${VEYRA_PG_CONTAINER:-veyra-postgres}"
TUNNEL="${VEYRA_TUNNEL_NAME:-veyra}"
mkdir -p "$LOGS"

say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

# Polls `check` once a second until it succeeds or `seconds` elapse.
wait_for() {
  local what="$1" seconds="$2"
  shift 2
  local i
  for ((i = 0; i < seconds; i++)); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  fail "$what did not come up within ${seconds}s (see $LOGS)"
}

AGENTS="$HOME/Library/LaunchAgents"
DOMAIN="gui/$(id -u)"
SERVICE_LABEL="cc.antonlabs.veyra.service"
TUNNEL_LABEL="cc.antonlabs.veyra.tunnel"

# True when scripts/install-launchd.sh --minimal has installed this agent, so
# launchd (not this script) owns the process and would respawn a plain kill.
agent_installed() { [ -f "$AGENTS/$1.plist" ]; }
agent_loaded() { launchctl print "$DOMAIN/$1" >/dev/null 2>&1; }
agent_start() { agent_loaded "$1" || launchctl bootstrap "$DOMAIN" "$AGENTS/$1.plist"; }
agent_stop() { launchctl bootout "$DOMAIN/$1" 2>/dev/null || true; }

docker_up() { docker info >/dev/null 2>&1; }
pg_ready() { docker exec "$PG_CONTAINER" pg_isready -U veyra -d veyra >/dev/null 2>&1; }
service_up() { curl -fsS -m 3 "$API/health" >/dev/null 2>&1; }
service_pid() { pgrep -f "$ROOT/target/release/veyra-service" | head -1 || true; }
tunnel_pid() { pgrep -f "cloudflared.*tunnel run $TUNNEL" | head -1 || true; }

start_docker() {
  if docker_up; then return; fi
  say "starting Docker Desktop..."
  open -a Docker
  wait_for "Docker" 180 docker_up
}

start_postgres() {
  if pg_ready; then return; fi
  say "starting Postgres container..."
  docker start "$PG_CONTAINER" >/dev/null 2>&1 \
    || fail "container '$PG_CONTAINER' is missing; recreate it (see start.md)"
  wait_for "Postgres" 60 pg_ready
}

start_service() {
  if service_up; then return; fi
  [ -f "$ROOT/.env" ] || fail "missing $ROOT/.env"
  if [ ! -x "$ROOT/target/release/veyra-service" ]; then
    say "building release binary (first run takes a few minutes)..."
    (cd "$ROOT" && cargo build --release)
  fi
  say "starting Veyra service..."
  if agent_installed "$SERVICE_LABEL"; then
    agent_start "$SERVICE_LABEL"
  else
    nohup "$ROOT/scripts/run-service.sh" >>"$LOGS/service.log" 2>&1 &
  fi
  wait_for "Veyra service" 60 service_up
}

start_tunnel() {
  if [ -n "$(tunnel_pid)" ]; then return; fi
  command -v cloudflared >/dev/null || fail "cloudflared is not installed (brew install cloudflared)"
  say "starting Cloudflare tunnel '$TUNNEL'..."
  if agent_installed "$TUNNEL_LABEL"; then
    agent_start "$TUNNEL_LABEL"
  else
    nohup cloudflared tunnel run "$TUNNEL" >>"$LOGS/tunnel.log" 2>&1 &
  fi
  # cloudflared logs to stderr, so the launchd log is tunnel.err.log.
  wait_for "tunnel" 30 bash -c "tail -n 20 '$LOGS'/tunnel*.log 2>/dev/null | grep -q 'Registered tunnel connection'"
}

stop_stack() {
  local pid
  # Under launchd a plain kill is undone within seconds, so unload the agent.
  if agent_loaded "$TUNNEL_LABEL"; then say "stopping tunnel (launchd)"; agent_stop "$TUNNEL_LABEL"; fi
  if agent_loaded "$SERVICE_LABEL"; then say "stopping Veyra service (launchd)"; agent_stop "$SERVICE_LABEL"; fi
  pid="$(tunnel_pid)"
  if [ -n "$pid" ]; then say "stopping tunnel ($pid)"; kill "$pid" || true; fi
  pid="$(service_pid)"
  if [ -n "$pid" ]; then say "stopping Veyra service ($pid)"; kill "$pid" || true; fi
  # Postgres and Docker are left running; stopping them is rarely wanted.
}

show_status() {
  say "docker:    $(docker_up && echo up || echo DOWN)"
  say "postgres:  $(pg_ready && echo ready || echo DOWN)"
  say "service:   $(service_up && echo up || echo DOWN)"
  say "tunnel:    $([ -n "$(tunnel_pid)" ] && echo up || echo DOWN)"
  if service_up; then
    say "ready:     $(curl -fsS -m 3 "$API/ready" 2>/dev/null || echo unavailable)"
  fi
}

case "${1:-start}" in
  start)
    start_docker
    start_postgres
    start_service
    start_tunnel
    show_status
    ;;
  stop)
    stop_stack
    ;;
  restart)
    stop_stack
    sleep 2
    "${BASH_SOURCE[0]}" start
    ;;
  status)
    show_status
    ;;
  *)
    fail "usage: scripts/start.sh [start|stop|restart|status]"
    ;;
esac
