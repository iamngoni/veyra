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

## Hosting (24/7)

The stack runs unattended on this Mac through three launchd agents rendered
from portable templates (`scripts/launchd/`) by `scripts/install-launchd.sh`:
the MT4 terminal at login, the named Cloudflare tunnel, and the service (via
`scripts/run-service.sh`, which sources `.env` and execs the release binary).
Tunnel and service carry `KeepAlive`, so a crash recovers without a session;
the terminal deliberately does not, so a clean quit stays quit. Secrets remain
in `.env`; plists carry only absolute paths. Exactly one supervised instance
owns ports 8080 and 7801, and the installer stops stray session-bound
processes first. Logs land under `~/Library/Logs/veyra` and are not rotated
yet. See ADR 0005.

## Testing strategy

- Unit tests for refined types and policy rules.
- Integration tests for HTTP and transport behavior.
- Broker contract tests against an explicit adapter interface.
- Adversarial tests for malformed and unsafe model output.
- Property tests for risk invariants.
- End-to-end tests only after a broker path is selected, with paper/testing accounts first.

Coverage remains at least 95% per first-party crate.
