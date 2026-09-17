# Veyra architecture

## Current implementation

The first crate is `veyra-service`, a Rust 2024 Actix Web control plane. It
exposes health, readiness, status, and a non-executing intent-evaluation
endpoint. Runtime settings are parsed into refined types before they reach
handlers. Status reports the active `broker_provider`, whether the broker link
is fresh and connected, and `trading_enabled` (still false: no execution path
exists).

## Boundaries

```text
Config -> Runtime state -> HTTP control plane
             |
             +-- DecisionEngine (selected by VEYRA_MODEL_PROVIDER)
             +-- Risk gate + typed trade intents + check-only control surface
             +-- BrokerLink (selected by VEYRA_BROKER_PROVIDER)
             |      +-- Ea  (loopback HTTP control channel)
             |      +-- ... (hosted bridge / direct API, added later)
             +-- Audit trail (PostgreSQL via SQLx)
             +-- Persistence/reconciliation
```

### Model providers (swappable)

Model access mirrors the broker pattern. `DecisionEngine` in `model/mod.rs` is
the contract: `provider()` plus a structured `answer(request)` returning a
parsed JSON value. The implementation is selected by `VEYRA_MODEL_PROVIDER`
and constructed once at startup by `ModelRuntime`.

**Implementation 1 — `agent-runtime` adapter (`model/agent_runtime_engine.rs`).**
It owns every `agent-runtime` type, so the rest of the service never imports the
dependency. The crate is pinned to a Git revision; Veyra contributed the
dynamic-schema entry point (`run_structured_with_format`) upstream so callers
can constrain responses to schemas only known at runtime. Models come from
explicitly configured tiers — never from library defaults — and are sent
through the OpenAI-compatible path (OpenRouter preset), with a bounded retry
policy for transient failures.

Operational constraint: tier models must accept forced tool calls, because the
schema is enforced through `tool_choice`. Reasoning modes that reject it (for
example DeepSeek thinking) return provider errors; verified working models are
listed in `model/settings.rs`.

Known gaps for later increments:

- Native Gemini/Cohere/Bedrock providers do not all implement the structured
  path; the OpenAI-compatible surface is what Veyra relies on today.
- Bedrock support remains bearer-token based (no SigV4/IAM).
- Rate limiting is delegated to the provider; per-provider budgets belong with
  the risk/ops layer.

### Jev (TypeSafe System One), proven live

Jev is a structured judgement interface, not an ordinary text provider, so it
is a separate boundary rather than a `DecisionEngine` implementation
(ADR 0004). `jev/mod.rs` owns the `SemanticJudge` contract and the provider
selector (`VEYRA_JEV_PROVIDER`, default `typesafe`); `jev/contract.rs` owns
meaning: typed `Choice` (2-32 rubric options), `Noul` (yes/no), and `Score`
(2-16 ordered levels) questions over validated state and instructions.

Responses are validated against the request that produced them — answer ids
and types must match, a chosen option must have been offered *and* carry the
maximum probability, a score legend must equal the requested levels, and
distributions must sum to one — so a contradictory or hallucinated answer
fails closed instead of reaching trading code. Transport is one shared
`reqwest` client with a 5 s connect timeout and a 20 s total timeout; `401/403`
maps to unauthorized, `422` to a rejected request with the service detail,
`429/529` to a backoff signal, and anything else to a transport error. The API
key never appears in `Debug` output.

Live proof on 2026-09-17: `jev-1.13.0` answered a three-question request in
~1.3 s (408 input / 73 output tokens) with a choice at 0.92 confidence, a noul
at 0.76, and a score of 1.95 on a 0-2 legend. Judgements are inputs code may
consult; they grant no execution authority — only the risk gate approves an
intent.

### Broker integrations (swappable)

Every venue integration implements the `BrokerLink` contract in
`broker/mod.rs`:

- `provider() -> BrokerProvider` identifies the implementation.
- `report() -> LinkReport` returns the latest locally held state.

`BrokerRuntime::from_settings` is the single construction point; the provider
is selected by `VEYRA_BROKER_PROVIDER`. Decision, risk, and reporting code
depend only on `Arc<dyn BrokerLink>`, so swapping venues means adding an
implementation plus a provider variant — no caller changes. Each
implementation owns its transport and its own loopback listener when it needs
one; nothing vendor-specific leaks into domain code.

**Implementation 1 — MQL4 EA control channel (`broker/ea.rs`), proven live.**
Transport and platform constraints are recorded in ADR 0002: MQL4
has no socket API (verified against build 1476 and the MetaQuotes reference),
so the EA polls a loopback-only HTTP endpoint using the terminal's built-in
`WebRequest` client. The service authenticates a shared token in constant time,
validates the payload into refined types (`AccountSnapshot`, `ServerName`,
`Symbol`, `AccountLogin`), records heartbeat state, and answers the probe
protocol (`ping`/`pong`) and carries the command queue. Commands are typed
(`ping`, `account_snapshot`, `order_check` today), delivered on a poll,
re-delivered until acknowledged, and failed after a timeout; acknowledgements
carry a stable id and are validated against the command's typed payload before
being recorded. The heartbeat also carries open volume in lots, and `account_snapshot`
returns a bounded, validated order list (ticket, symbol, kind, lots, price,
profit, plus a truncation flag) that feeds the gate's exposure cap and the
future reconciler. `order_check` carries a gate-approved intent to the terminal,
which applies its own market rules and margin engine (`MarketInfo`,
`AccountFreeMarginCheck`) and returns a classic MT4 trade code (for example 129
wrong-side price, 131 volume, 134 margin) without ever sending an order; the
loopback control surface (`control.rs`) exposes `POST /intents/check` and
`GET /commands/{id}` for operators and tests. Mutating commands are
deliberately absent; they will reuse this id/ack discipline and must be
idempotent per id. The transport is HTTPS through a Cloudflare
Tunnel to the loopback listener; MQL4 supports no sockets and no explicit
ports.

**Rejected for now — hosted API bridges.** The evaluated vendors are paid
services that run their own terminals; the account owner opted for the
zero-recurring-cost EA path. A bridge can still be added later as a second
`BrokerLink` implementation without disturbing anything else, and a direct
broker API likewise if IFC ever exposes one.

No account credentials are stored in Veyra or in the repository: the MT4
terminal holds the session, and the EA token only authorizes the loopback
control channel.

### Trade intents and the proposal pipeline

`trading/intent.rs` defines the only shape a strategy or model may propose
(`TradeIntentDraft`) plus the model answer contract (`TradeProposal`). Drafts
parse once at the boundary into validated values; illegal states such as a
price on a market order or a zero volume cannot be represented, and a draft
has no identity. `trading/pipeline.rs` runs one structured `DecisionEngine`
answer through the gate and returns no-trade, a rejection, or an approved
`TradeIntent`.

### Risk gate

The gate is deterministic code, not a model prompt. `risk/mod.rs` parses the
`VEYRA_RISK_*` policy; `risk/gate.rs` evaluates one draft in a fixed order —
kill switch, instrument allowlist, UTC session window, per-order volume cap,
account availability, trading permission, open-order cap, total-exposure cap,
duplicate suppression — and mints a `TradeIntent` only on approval. The
exposure cap compares open volume (reported by the link) plus the requested
volume against `VEYRA_RISK_MAX_TOTAL_LOTS`. Rejections carry stable codes and
non-sensitive details.

The gate fails closed: missing or stale account facts reject, and defaults
allow no instrument until one is configured. `POST /intents/evaluate` on the
loopback diagnostics listener returns advisory decisions only; it never
queues, transmits, or executes, and an approved intent still requires the
(future) command layer, which must reuse this gate. Account facts come from
the fresh link report plus the open-order count every `BrokerLink`
implementation reports from locally held state. Audit persistence arrives with
the storage phase.

### Execution (staged, two switches)

`POST /intents/execute` is the only execution entry point. It refuses with
`403 trading_disabled` unless the operator sets `VEYRA_TRADING_ENABLED=true`,
and it refuses outright when no command channel exists. With the service switch
on, a gate-approved intent becomes a typed `open_order` command carrying the
Veyra magic number; the terminal validates the request against its market rules
and margin engine and, unless it was deliberately recompiled with
`InAllowLiveOrders = true`, acknowledges a dry run (`executed:false`,
`retcode:0`) without sending anything. Real money therefore needs a gate
approval plus two independent, deliberate switches. Order ids, acks, and
timeouts reuse the same at-least-once discipline as read-only commands.

`POST /intents/close` closes one Veyra-owned position by ticket. Ownership is
enforced twice: the service only accepts tickets present in the latest
completed `account_snapshot` whose magic number is the Veyra magic, and the
terminal re-checks the magic on the selected order before touching it. Pending
orders are refused (they need cancellation, not a close), and everything else
goes through the same switch, dry-run, and ack validation as `open_order`.
`POST /intents/modify` changes stops on the same terms: at least one finite,
positive stop is required, `0`/absent stops keep their current values, and the
terminal re-validates distance rules before acting. All three mutating commands
(open, close, modify) share one contract, one switch pair, and one validated
ack shape.

### Persistence (audit trail)

Durable history lives in one append-only `audit_events` table (id, timestamp,
kind, JSONB payload) managed by embedded SQLx migrations. Command queueing,
acknowledgements, validated broker snapshots, and process starts are recorded;
a configured-but-unreachable database fails startup, while individual writes
are best-effort so a storage hiccup never blocks a command. `GET /audit`
returns the newest rows. PostgreSQL runs as a Homebrew service on this machine
(auto-start at login). Rows older than `VEYRA_AUDIT_RETENTION_DAYS` (default
30, zero keeps everything) are pruned hourly, best-effort. Backups and
monitoring are still open.

### Reconciliation

`reconciliation.rs` classifies every order in the retained `account_snapshot`
as Veyra-managed (its magic number is `ORDER_MAGIC`) or unknown, and treats a
truncated list as drift even when every visible order is ours. `GET
/reconciliation` exposes the assessment with its snapshot age and one of
`unavailable`, `stale`, `no_snapshot`, `reconciled`, or `drift`. A background
refresh (`VEYRA_RECONCILE_SECS`, default 30 s, zero disables) queues one
`account_snapshot` per interval but only while the channel is fresh and no
snapshot is already pending, so a terminal that is down cannot accumulate
stale commands. Ticket-level tracing to specific approved intents lands with
durable storage.

## Hosting (24/7)

The stack runs unattended on this Mac through six launchd agents rendered
from portable templates (`scripts/launchd/`) by `scripts/install-launchd.sh`:
the MT4 terminal at login, the named Cloudflare tunnel, the service (via
`scripts/run-service.sh`, which sources `.env` and execs the release binary),
hourly log rotation, a daily verified audit-trail backup, and the console. Tunnel and
service carry `KeepAlive`, so a crash recovers without a session; the terminal
deliberately does not, so a clean quit stays quit. The rotation agent
copy-truncates logs under `~/Library/Logs/veyra` above 5 MiB, keeping three
generations. The backup agent dumps PostgreSQL in custom format, verifies the
archive with `pg_restore --list` before it replaces the previous generation,
keeps the newest fourteen dumps under `~/Library/Application Support/veyra/backups`,
and runs once at load plus daily at 03:30. `/ready` reports broker and audit
health, degrading instead of hiding an unhealthy dependency. Secrets remain
in `.env`; plists carry only absolute paths. Exactly one supervised instance
owns ports 8080 and 7801, and the installer stops stray session-bound
processes first. See ADR 0005.

### Autonomy (autopilot)

`trading/autopilot.rs` runs one decision tick per configured interval
(`VEYRA_AUTOPILOT_*`, disabled by default; the first tick lands one interval
after startup): it resolves the symbol (configured or the chart's), fetches a
closed-candle window through the market feed, optionally asks Jev for
calibrated judgements over the same state, then asks the decision engine for
one structured proposal with the account facts embedded. The proposal passes
`normalize_proposal` (two narrow tolerances: a reference `price` echoed on a
market order and an embellished `comment` are dropped, and a decline carrying
a stray `intent` is treated as a decline — none of these change execution
meaning), then the strict draft parser, then the risk gate. Approvals go
through the same `queue_staged_order` path as the control surface, so both
operator controls and the audit trail still apply. Every tick records a
`proposal_evaluated` audit event with its outcome — `no_trade`, `rejected`,
`approved_dry_run`, `queued`, or `unavailable` — plus the command events when
one is queued. Every autonomous entry must carry both a stop loss and a take
profit; unbracketed proposals are rejected before the command layer sees them.

### Console

`console/` is a TanStack Start application served by a supervised Vite
preview process on `http://127.0.0.1:3000`. It reads only the loopback control
surface, proxying `/api` so the browser never needs cross-origin access:
`/status` and `/account` for state, `/events` (cursor long-poll over an
in-memory ring of recent audit events) for the activity feed, `/commands` for
command lifecycle, `/reconciliation`, and `/market/candles` for the chart.
`/account` exposes owner-facing money fields (balance, equity, positions) and
is loopback-only by design; exposing it beyond loopback requires
authentication first.

## Testing strategy

- Unit tests for refined types and policy rules.
- Integration tests for HTTP and transport behavior.
- Broker contract tests against an explicit adapter interface.
- Adversarial tests for malformed and unsafe model output.
- Property tests for risk invariants.
- End-to-end tests only after a broker path is selected, with paper/testing accounts first.

Coverage remains at least 95% per first-party crate.
