# ADR 0005 — Supervised 24/7 hosting

## Status

Accepted 2026-09-17; installed and proven (service and tunnel both restarted
after `SIGKILL`, and the EA link reconnected automatically).

## Context

- Everything ran from developer terminal sessions. When a session ended, the
  tunnel and the terminal died with it, the Cloudflare hostname answered `530`,
  and the EA had nothing to poll.
- The product goal is unattended operation, so supervision comes before more
  execution features.
- launchd cannot read dotenv files, and plists are machine-specific.

## Decision

- Three LaunchAgents, rendered from portable templates in `scripts/launchd/`
  by `scripts/install-launchd.sh` (idempotent, removable with `--uninstall`):
  - `cc.antonlabs.veyra.terminal` starts MT4 at login (`open -a`). No
    KeepAlive: a clean quit should stay quit, and Wine exit codes are not
    trustworthy enough for crash classification.
  - `cc.antonlabs.veyra.tunnel` runs the named Cloudflare tunnel with
    KeepAlive.
  - `cc.antonlabs.veyra.service` runs `scripts/run-service.sh`, which sources
    `.env` and execs the **release** binary, with KeepAlive.
- Secrets stay in `.env` (0600); plists carry only absolute paths.
- Logs go to `~/Library/Logs/veyra/{service,tunnel,terminal}.{out,err}.log`.
- Exactly one supervised instance owns ports 8080 and 7801; the installer stops
  stray session-bound processes before bootstrapping.

## Consequences

- Code changes require `cargo build --release` and
  `launchctl kickstart -k gui/$(id -u)/cc.antonlabs.veyra.service`.
- Log rotation, monitoring, and alerting are still manual; a durable
  host/VPS, managed secrets, and backups remain the deployment phase.
- MT4 crash-restart and checkpointed reconciliation across service restarts are
  deliberate later steps (persistence lands with the storage phase).
