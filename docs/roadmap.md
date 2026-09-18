# Roadmap

## Phase 0 — service foundation (this commit)

- [x] Rust/Actix workspace.
- [x] Safe read-only HTTP surface.
- [x] Configuration and observability foundation.
- [x] CI and coverage gates.
- [x] Repository bootstrap and GitHub delivery.

## Phase 1 — durable service core

- [x] PostgreSQL via SQLx for audit events and broker state: append-only
      `audit_events` (commands, acknowledgements, snapshots, restarts),
      embedded migrations, and a loopback `GET /audit` view — proven live
      against PostgreSQL 17 (rows survive restarts).
- [x] Migration and reconciliation workflow: embedded migrations run before
      listening; the reconciliation view and its periodic refresh are audited.
- [x] Operational readiness checks tied to real dependencies: `/ready`
      reports broker and audit status and degrades without gating the surface
      (proven live: database stopped → `degraded`, restored → `ready`).
- [x] Structured tracing spans and metrics: `autopilot.tick` and `ea.poll`
      run inside spans, and `GET /metrics` reports process-lifetime counters
      derived from the audit stream (`event.*`, `proposal.<outcome>`,
      `command.<event>.<kind>`).

## Phase 2 — model integration

- [x] Pin `agent-runtime` to a known revision.
- [x] Provider/model configuration through typed, secret-aware settings.
- [x] Schema-constrained request/response contracts (`DecisionEngine`).
- [x] Dynamic-schema support contributed upstream (`run_structured_with_format`).
- [x] Retry policy for transient provider failures; deterministic adapter tests
      with an injected transport; live OpenRouter proof.
- [x] Call budget in the operations layer: a `BudgetedEngine` decorator
      enforces `VEYRA_MODEL_MAX_CALLS_PER_HOUR` / `_PER_DAY` (0 = unlimited)
      over any engine implementation, and `GET /status` reports the current
      usage against the limits.
- [x] Model decisions never reach execution directly (no execution exists).

## Phase 3 — Jev decision adapter

- [x] API access validated live (key configured, `jev-1.13.0`, ~1.3 s and
      408 in / 73 out tokens for a three-question request).
- [x] Typed decision inputs and outputs (`jev::contract`): choice, noul, and
      score questions over validated state and instructions, with answers
      validated against the request that produced them.
- [x] Adversarial parsing tests: mismatched answer ids and types, unoffered or
      non-maximal choices, malformed probabilities, out-of-range confidence,
      and legends that disagree with the requested levels.
- [ ] Confirm current pricing and per-request budget from the console.
- [x] Benchmark latency and cost under realistic batch questions — see
      `docs/benchmarks.md`: Jev 1.11-1.99 s (408/73 tokens per three-question
      set), model 1.40-2.84 s, measured 5× each on the live machine.

## Phase 4 — broker integration

- [x] Verify IFC Markets-supported access methods and account restrictions
      (no public retail REST/FIX API; MT4 has no MQL4 socket API).
- [x] Compare hosted bridge and MQL4 EA paths (bridges are paid; EA chosen as
      the zero-recurring-cost first implementation).
- [x] Broker integration boundary (`BrokerLink`) with provider selection and
      the EA control channel as the first implementation.
- [x] Live probe round trip on the terminal (heartbeat + ping/pong), via the
      Cloudflare tunnel with Wine IPv4 pinning (ADR 0002).
- [x] Idempotent command/ack queue with typed payload validation, timeouts,
      and read-only commands (`ping`, `account_snapshot`) — proven live.
- [x] Broker-side order validation (`order_check`) for gate-approved intents,
      proven live on the real terminal (retcode 0 for a valid market buy, 129
      for a wrong-side limit) without sending an order; exposed through the
      loopback `POST /intents/check` and `GET /commands/{id}` control surface.
- [x] Mutating command plumbing with the same id/ack discipline: an approved
      intent can queue `open_order`, which the terminal validates and reports
      as a **dry run** unless a second, operator-set input arms live order
      placement. Proven live end to end without sending an order.
- [x] Live order placement: both controls enabled after explicit owner
      approval (EA compiled with `InAllowLiveOrders = true` through
      `VEYRA_EA_ALLOW_LIVE`, service `VEYRA_TRADING_ENABLED=true`), proven by
      the first autonomous order — EURUSD 0.01 sell, ticket 10650805, retcode
      0, placed 2026-09-17 by the autopilot with no human in the loop.
- [x] Market data: a read-only `rates` command returns closed candles from the
      terminal; `GET /market/candles` exposes the validated series, proven
      live (EURUSD H4, oldest-first, forming bar excluded).
- [x] Autonomous loop (`trading/autopilot.rs`): candles → optional Jev
      judgements → one structured proposal → risk gate → staged execution, on
      a configurable cadence, disabled by default, audited every tick — proven
      live end to end against the real terminal.
- [x] Position review: while a managed position is open, the loop asks for
      `hold` or `close` with its own constrained schema, guarded by a minimum
      hold (`VEYRA_AUTOPILOT_MIN_HOLD_SECS`, default 300 s), verified position
      age, and the same staged-close path as the control surface.
- [x] Stop policies: `VEYRA_AUTOPILOT_BREAKEVEN_R` moves the stop to the
      entry price once the trade has travelled that multiple of its entry
      risk in favour, and `VEYRA_AUTOPILOT_TRAIL_R` then keeps it that far
      behind the best price; the most protective candidate wins, stops never
      move backwards, and an entry-risk memory supplies the basis after a move
      (off by default; both enabled at 1R in the live environment).
- [x] Venue contract reporting: a read-only `symbol_spec` command returns the
      instrument's spread and stop level (points), lot band and step, margin
      per lot, swap rates, and trade permission; `GET /market/spec` exposes it
      and the autopilot feeds it (with ATR(14) and account free margin) into
      both model inputs. A pre-queue contract check rejects volumes off the
      lot grid, margin above free margin, and stops inside the spread, the
      broker's minimum distance, or a console-editable ATR(14) noise floor
      (`minStopAtrFraction`, default 0.25) — proven live against IFC Markets
      for the whole menu (EURUSD/USDJPY/GBPUSD/XAUUSD).
- [x] Realized performance: a read-only `order_history` command returns the
      terminal's closed fills (profit + swap + commission), `GET /performance`
      aggregates wins/losses/win rate, net P/L, profit factor, and per-symbol
      totals, and the console shows it as the Performance panel — proven live
      (the USDJPY take-profit: open 156.198, close 156.410, net +1.36, 1W/0L).
- [x] Economic calendar: a provider-neutral `EventCalendar` trait with the
      keyless ForexFactory weekly export as the first implementation (cached),
      `GET /calendar` for operators, per-asset `upcoming_events` in the entry
      model input, and a deterministic high-impact blackout window that is
      console-editable through the risk policy (`calendarBlackoutMinutes`,
      default 30, 0 disables). A configured feed that cannot answer fails the
      entry sweep closed.
- [x] `close_order` for Veyra-owned positions: only tickets from the latest
      completed `account_snapshot` that carry the Veyra magic number are
      accepted, and the terminal re-validates before a dry run or close —
      guards proven live (403 disabled, 409 no state, 404 unknown ticket,
      409 foreign position).
- [x] `modify_order` for Veyra-owned positions: same ownership guards as
      closing, at least one finite positive stop required, absent stops keep
      their current values, and the terminal re-validates distances before a
      dry run or `OrderModify` — guards proven live (403/400/409/404).
- [x] Reconciliation view over broker state: `GET /reconciliation` classifies
      every open order as Veyra-managed or unknown by magic number, flags a
      truncated list, and reports snapshot age. A periodic refresh
      (`VEYRA_RECONCILE_SECS`, default 30 s) keeps the retained snapshot
      current while the terminal polls — proven live.
- [x] Ticket-level tracing: decisions record the approved `intent_id` and the
      `command_id` they produced, so a position ticket can be followed back
      through its command and ack to the decision that opened it.
- [ ] Staged testing: read-only first, then demo, then minimal live exposure
      only after explicit owner approval.

## Phase 5 — deterministic risk and control

- [x] Typed trade intents parsed at the boundary; a draft carries no identity
      until the gate approves it, and no execution path exists.
- [x] Deterministic, fail-closed gate: kill switch, instrument allowlist, UTC
      session window, per-order volume cap, open-order cap, duplicate
      suppression.
- [x] Model proposals run through the gate (`trading::pipeline`); rejections
      are normal outcomes and approvals are never queued or executed.
- [x] Non-executing `POST /intents/evaluate` on the loopback diagnostics
      listener, backed by live link state.
- [x] `VEYRA_RISK_*` configuration with restrictive defaults; malformed values
      fail startup.
- [x] Exposure limits in lots: the EA reports open volume (heartbeat) and a
      bounded order list (`account_snapshot`), and the gate rejects when open
      volume plus the requested volume exceeds `VEYRA_RISK_MAX_TOTAL_LOTS` —
      proven live (`exposure_above_limit` with a 0.005 cap, approved at the
      0.01 default).
- [x] Policy persistence: the effective risk policy is recorded with the
      audit trail on every service start, so any decision can be read against
      the rules that were in force when it was made.
- [x] Two independent controls in front of real money: the service refuses to
      queue execution unless `VEYRA_TRADING_ENABLED=true`, and the terminal
      refuses to trade unless recompiled with `InAllowLiveOrders = true`.

## Phase 6 — console

- [x] TanStack Start console served on loopback: account/positions, streaming
      activity feed (`/events` cursor long-poll), command lifecycle, market
      window, autopilot and control state — supervised as a launchd agent.
- [ ] Authentication and authorization for access beyond loopback.
- [ ] Decision, risk, and incident views beyond the activity feed.
- [ ] Frontend unit, integration, type-check, build, and coverage gates.

## Phase 7 — deployment

- [x] Local 24/7 supervision: launchd agents for the terminal, tunnel,
      service, hourly log rotation, and daily verified audit backups (release
      build, crash restart for the service and tunnel, copy-truncate logs
      above 5 MiB keeping three generations, portable templates plus
      installer) — restart proven live with `SIGKILL` (ADR 0005) and rotation
      proven with a low threshold.
- [x] Audit retention: rows older than `VEYRA_AUDIT_RETENTION_DAYS` (default
      30) are pruned hourly — proven live (an aged row was removed while
      recent rows survived).
- [x] Local audit-trail backups: a daily launchd agent dumps PostgreSQL in
      custom format, verifies each archive with `pg_restore --list`, keeps the
      newest fourteen under `~/Library/Application Support/veyra/backups`,
      and prunes older generations.
- [x] Local alerting: a two-minute probe watches readiness, both execution
      controls, repeated autopilot failures, reconciliation drift, executed
      opens, and closed positions, pushing JSON webhook alerts (any provider
      that accepts `text`) — proven live with a database bounce (degraded and
      recovery alerts delivered) and a silent steady state.
- [x] Deployment readiness: `docs/deployment.md` documents the move to an
      always-on Mac step by step and `scripts/preflight.sh` checks every
      prerequisite on the target (verified on this machine: 16 pass, 0 fail);
      `scripts/check.sh` is the single local gate for the Rust workspace and
      the console.
- [x] Off-machine backups: every fresh dump uploads to R2
      (`VEYRA_BACKUP_R2_BUCKET`) through the authenticated `wrangler` CLI —
      proven live (byte-identical retrieval of an uploaded dump, launchd-path
      run included), with a freshness marker the alert probe watches.
- [ ] Durable 24/7 host or VPS, managed secrets, and remote monitoring.
- [ ] Versioned deployment pipeline.
- [ ] Staged rollout: local → paper account → minimal live exposure only after explicit owner approval.
