# Deploying Veyra to an always-on machine

For the final home Mac mini migration, follow the more complete
[Mac mini deployment runbook](deployment-mac-mini.md). This document remains the
general architecture reference.

The stack currently runs on one Mac with launchd supervision (ADR 0005).
This guide moves it to another always-on Mac with the same shape, and lists
what to verify before arming live trading there. `scripts/preflight.sh`
checks every prerequisite it can read; run it on the target first.

## What runs where

| Component | Role | Runtime |
| --- | --- | --- |
| MT4 terminal (`MetaTrader 4.app`) | native host application holding the account and `VeyraProbe`; the EA polls the tunnel | `cc.antonlabs.veyra.terminal` (starts at login) |
| Cloudflare tunnel (`cloudflared`) | `veyra.antonlabs.cc` → `127.0.0.1:7801` | `cc.antonlabs.veyra.tunnel` (KeepAlive) |
| Veyra service (Rust) | EA channel on 7801, diagnostics on 8080, autopilot | `cc.antonlabs.veyra.service` (KeepAlive) |
| PostgreSQL 17 | audit trail (required once `VEYRA_DATABASE_URL` is set) | Homebrew service |
| Console (TanStack Start) | operations UI on `http://127.0.0.1:3000` | `cc.antonlabs.veyra.console` (KeepAlive) |
| Alert probe | webhook notifications, every two minutes | `cc.antonlabs.veyra.alerts` |
| Log rotation, backups | bounded logs, daily verified `pg_dump` | `cc.antonlabs.veyra.logrotate`, `cc.antonlabs.veyra.backup` |

Everything is loopback-only except the tunnel, which carries only the EA
channel. The console and diagnostics are never exposed publicly.

## Target requirements

- macOS (Apple Silicon) — the stack is macOS + launchd; a Linux/Windows target
  would need the scripts ported to systemd/Windows services first.
- Homebrew with `postgresql@17`, `cloudflared`, and Node.js >= 20.
- Rust toolchain (`rustup`) to build the service.
- `MetaTrader 4.app` installed on the host and launched once. The terminal
  profile/data location is installation-specific; do not assume a Wine prefix.
- Cloudflare account access for the `veyra` tunnel and the
  `veyra.antonlabs.cc` DNS record.
- Network allowed to reach the broker (IFC Markets), OpenRouter, TypeSafe,
  and Cloudflare.

## Prepare on the current machine

1. Confirm a fresh backup exists:
   `ls -lt "$HOME/Library/Application Support/veyra/backups" | head -3`
2. Copy `.env` securely (password manager, `scp` over SSH — never email or a
   chat). The EA token, model key, Jev key, and `VEYRA_CONSOLE_SECRET_KEY`
   live there (without that key, model and subscription credentials saved from
   the console cannot be decrypted); rotate them after
   transfer if the channel was not trusted. The trading password is never
   stored anywhere.
3. Note the terminal setup: chart symbol/timeframe (EURUSD H4 here) and the
   EA inputs (`InUrl` is the tunnel URL).
4. Keep the old machine running until the new one is verified.

## Install on the target

```sh
# 1. Prerequisites
brew install postgresql@17 cloudflared node
brew services start postgresql@17
rustup-init  # or: brew install rustup-init && rustup-init

# 2. Checkout and build
git clone git@github.com:iamngoni/veyra.git ~/Developer/Projects/veyra
cd ~/Developer/Projects/veyra
cargo build --release
(cd console && npm install && npm run build)

# 3. Secrets
#    Write .env from the template; every variable is documented in .env.example.
cp .env.example .env   # then fill from the secure copy
chmod 600 .env

# 4. Database: create the role/db and restore the newest dump
createdb veyra
pg_restore --dbname veyra --no-owner \
  "$HOME/Library/Application Support/veyra/backups/veyra-YYYYMMDD-HHMMSS.dump"
# The service also runs embedded migrations on start, so an empty database works too.

# 5. Tunnel
cloudflared tunnel login
cloudflared tunnel create veyra         # or reuse the existing tunnel + credentials
cloudflared tunnel route dns veyra veyra.antonlabs.cc
cat > ~/.cloudflared/veyra-config.yml <<'YAML'
tunnel: veyra
credentials-file: /Users/<you>/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: veyra.antonlabs.cc
    service: http://127.0.0.1:7801
  - service: http_status:404
YAML

# 6. Terminal: install MT4 on the host, launch it once, attach VeyraProbe to
#    the chart, and install the compiled EA through MetaTrader's own data
#    folder/MetaEditor. The legacy compile_ea.sh helper is Wine-specific.
set -a && source .env && set +a
# Use MetaEditor on the host for a native MT4 installation.

# 7. Supervision
./scripts/preflight.sh      # everything it can check must pass
./scripts/install-launchd.sh
```

## Verify before arming

- `curl http://127.0.0.1:8080/ready` → `ready` within a minute of the terminal
  polling (`broker: connected`).
- `curl http://127.0.0.1:8080/status` → providers correct and
  `trading_enabled: false`. `ea_live_orders` reflects `VEYRA_EA_ALLOW_LIVE` at
  EA compile time, which defaults to `true`; set it to `false` before
  compiling if the terminal should stay disarmed during verification.
- Console on `http://127.0.0.1:3000` shows the stream; queue one snapshot:
  `curl -X POST http://127.0.0.1:8080/commands/account_snapshot`.
- Set `VEYRA_ALERT_WEBHOOK` and confirm the probe logs to
  `~/Library/Logs/veyra/alert.out.log` (or delivers to the webhook).

Arming live trading is a deliberate, two-step act. The EA must have
`InAllowLiveOrders` on (compiled from `VEYRA_EA_ALLOW_LIVE`, default `true`, and
editable in the EA inputs). Then arm the service switch from the console or
with `POST /config {"VEYRA_TRADING_ENABLED": true}`. A setting saved from the
console is persisted and wins over `.env` after a restart, so check `GET
/config` for `overridden` before relying on `.env`. Read the roadmap's
staged-rollout checklist first.

## Cutover (avoid two bots on one account)

1. Stop the old machine first: `./scripts/install-launchd.sh --uninstall`
   (on the old checkout). Closing the old terminal disarms its EA.
2. Start the new machine's agents and verify readiness as above.
3. Exactly one tunnel may run a named tunnel at a time — `cloudflared`
   rejects a second run, which is a useful safety net, but do not rely on it.
4. Only arm the new machine after the verification steps pass.

## Rollback

- Stop the new machine's agents (`./scripts/install-launchd.sh --uninstall`).
- Restart the old machine's agents (`./scripts/install-launchd.sh`).
- The audit trail is the record of what happened; restore
  `~/Library/Application Support/veyra/backups` dumps with `pg_restore` if a
  database is lost.

## Notes

- The alert probe posts to any endpoint accepting a JSON `text` body
  (Slack, Discord, ntfy).
- Off-machine backups upload each fresh dump to the R2 bucket named by
  `VEYRA_BACKUP_R2_BUCKET` through the `wrangler` CLI; the target machine
  needs `wrangler` logged in to the same Cloudflare account (and
  `VEYRA_WRANGLER_BIN` pointing at the binary if nvm's node is not on the
  launchd PATH). The alert probe warns when the last upload is over a day old.
- Logs, backups, and the alert state all live under `~/Library/...` and are
  recreated on first run; nothing outside `.env` is machine-specific.
