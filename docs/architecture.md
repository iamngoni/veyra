# Veyra architecture

## Current implementation

The first crate is `veyra-service`, a Rust 2024 Actix Web control plane. It
exposes read-only health, readiness, and status endpoints. Runtime settings are
parsed into refined types before they reach handlers. Status reports the active
`broker_provider`, whether the broker link is fresh and connected, and
`trading_enabled` (still false: no execution path exists).

## Boundaries

```text
Config -> Runtime state -> HTTP control plane
             |
             +-- Decision adapter (future)
             +-- Deterministic risk gate
             +-- BrokerLink (selected by VEYRA_BROKER_PROVIDER)
             |      +-- Ea  (loopback HTTP control channel)
             |      +-- ... (hosted bridge / direct API, added later)
             +-- Persistence/reconciliation
```

### agent-runtime

`agent-runtime` is an existing Rust crate. Its current public types include `Llm`, `LlmBuilder`, provider kinds, model tiers, and injectable transport. Veyra will consume it as a dependency pinned to a specific Git revision rather than forking the implementation.

Current integration gaps to account for before production use:

- Native Gemini, Cohere, and Bedrock providers do not all expose the structured-output path used by `run_structured`.
- Bedrock support is bearer-token based and lacks SigV4/IAM support.
- Explicit model identifiers are required; defaults must not be relied on.
- Provider defaults and rate limits need Veyra-level configuration and tests.

### Jev

Jev is a structured decision interface, not an ordinary text provider. It will be isolated behind its own adapter and response type. Veyra will model its choice, score, and truth/confidence values as fallible, validated domain data. No Jev access exists yet, so none of this slice calls it.

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

**Implementation 1 — MQL4 EA control channel (`broker/ea.rs`).** MT4's MQL4
has no socket API (verified against build 1476 and the MetaQuotes reference),
so the EA polls a loopback-only HTTP endpoint using the terminal's built-in
`WebRequest` client. The service authenticates a shared token in constant time,
validates the payload into refined types (`AccountSnapshot`, `ServerName`,
`Symbol`, `AccountLogin`), records heartbeat state, and answers the probe
protocol (`ping`/`pong`). Order commands are deliberately absent; they will
arrive as an idempotent command queue over this same channel.

**Rejected for now — hosted API bridges.** The evaluated vendors are paid
services that run their own terminals; the account owner opted for the
zero-recurring-cost EA path. A bridge can still be added later as a second
`BrokerLink` implementation without disturbing anything else, and a direct
broker API likewise if IFC ever exposes one.

No account credentials are stored in Veyra or in the repository: the MT4
terminal holds the session, and the EA token only authorizes the loopback
control channel.

### Risk gate

The gate is deterministic code, not a model prompt. It will enforce policy such as position limits, exposure limits, order quantity, instrument allowlist, market-session rules, duplicate suppression, and a kill switch. It must run before broker execution and persist an audit record.

## Testing strategy

- Unit tests for refined types and policy rules.
- Integration tests for HTTP and transport behavior.
- Broker contract tests against an explicit adapter interface.
- Adversarial tests for malformed and unsafe model output.
- Property tests for risk invariants.
- End-to-end tests only after a broker path is selected, with paper/testing accounts first.

Coverage remains at least 95% per first-party crate.
