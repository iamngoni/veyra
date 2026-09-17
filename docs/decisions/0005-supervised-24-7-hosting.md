# ADR 0005 — Supervised 24/7 hosting

## Status

Accepted 2026-09-17; installed and proven (service and tunnel both restarted
after `SIGKILL`, and the EA link reconnected automatically). Amended the same
day: hourly log rotation, a daily verified audit backup, the operations
console, and a two-minute alert probe joined the agent set (see the hosting
section of `docs/architecture.md`).

## Context

- Everything ran from developer terminal sessions. When a session ended, the
  tunnel and the terminal died with it, the Cloudflare hostname answered `530`,
  and the EA had nothing to poll.
- The product goal is unattended operation, so supervision comes before more
  execution features.
- launchd cannot read dotenv files, and plists are machine-specific.

## Decision

- Seven LaunchAgents, rendered from portable templates in `scripts/launchd/`
  by `scripts/install-launchd.sh` (idempotent, removable with `--uninstall`):
  - `cc.antonlabs.veyra.terminal` starts MT4 at login (`open -a`). No
    KeepAlive: a clean quit should stay quit, and Wine exit codes are not
    trustworthy enough for crash classification.
  - `cc.antonlabs.veyra.tunnel` runs the named Cloudflare tunnel with
    KeepAlive.
  - `cc.antonlabs.veyra.service` runs `scripts/run-service.sh`, which sources
    `.env` and execs the **release** binary, with KeepAlive.
  - `cc.antonlabs.veyra.logrotate` runs hourly, copy-truncating logs above
    5 MiB in place (launchd holds the descriptors open) and keeping three
    generations.
  - `cc.antonlabs.veyra.backup` runs at load and daily at 03:30, dumping the
    audit database in custom format, verifying the archive with
    `pg_restore --list` before it replaces the previous generation, and
    keeping the newest fourteen dumps under
    `~/Library/Application Support/veyra/backups`.
  - `cc.antonlabs.veyra.console` serves the built operations console on
    loopback, proxying `/api` to the service.
  - `cc.antonlabs.veyra.alerts` runs every two minutes, comparing the stack
    against the previous run and pushing findings to `VEYRA_ALERT_WEBHOOK`
    (logged under `~/Library/Logs/veyra` when unset).
- Secrets stay in `.env` (0600); plists carry only absolute paths.
- Logs go to `~/Library/Logs/veyra/{service,tunnel,terminal,logrotate,backup}.{out,err}.log`,
  and the logrotate agent keeps them bounded.
- Exactly one supervised instance owns ports 8080 and 7801; the installer stops
  stray session-bound processes before bootstrapping.

## Consequences

- Code changes require `cargo build --release` and
  `launchctl kickstart -k gui/$(id -u)/cc.antonlabs.veyra.service`.
- Log rotation and local audit backups are supervised; monitoring and
  alerting, a durable host/VPS, managed secrets, and off-machine backups
  remain the deployment phase.
- MT4 crash-restart and checkpointed reconciliation across service restarts are
  deliberate later steps (persistence lands with the storage phase).
