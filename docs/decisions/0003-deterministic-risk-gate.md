# ADR 0003 — Deterministic, fail-closed risk gate

## Status

Accepted 2026-09-17; implemented with unit coverage and an in-process contract
test that drives the real EA channel and the diagnostics app.

## Context

- The product goal is unattended execution, so a wrong or hallucinated model
  answer must never reach a broker.
- Model providers, broker transports, and (later) Jev are swappable
  implementations behind narrow traits; the gate must not know any of them.
- The EA channel reports real account state (heartbeat plus validated
  `account_snapshot`), but link state alone must not authorize orders.

## Decision

- Only a `TradeIntentDraft` may be proposed. Drafts parse once at the boundary
  into validated values; illegal shapes (price on a market order, zero or
  non-finite volume, unknown side, over-long comment) cannot be represented.
- `RiskGate::evaluate` is a pure function of policy, draft, account facts, and
  an explicit clock. Approval mints the only identity-bearing value,
  `TradeIntent`, through a crate-private constructor, so nothing can claim
  approval without an evaluation.
- The check order is fixed and documented: kill switch, instrument allowlist,
  UTC session window, per-order volume cap, account availability, trading
  permission, open-order cap, duplicate suppression. Every rejection carries a
  stable code and a non-sensitive detail.
- The gate fails closed: unavailable or stale account facts reject, an
  implementation that cannot report open orders rejects, and a poisoned
  duplicate-suppression lock degrades only because the guarded state is a
  single plain vector that cannot be half-written.
- Policy comes from `VEYRA_RISK_*` with restrictive defaults (no instrument is
  allowed until configured); malformed values fail startup.
- Until an execution layer exists, proposals are validated only:
  `POST /intents/evaluate` returns advisory decisions and nothing is queued or
  transmitted.

## Consequences

- A model cannot widen its own authority: it proposes, the gate decides.
- Adding a venue or provider does not touch gate code; the gate depends on
  validated domain types and a vendor-neutral link contract only.
- Exposure limits beyond per-order caps need richer account state (open
  volume), which the EA will report in a later increment.
