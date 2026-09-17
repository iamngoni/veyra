# Roadmap

## Phase 0 — service foundation (this commit)

- [x] Rust/Actix workspace.
- [x] Safe read-only HTTP surface.
- [x] Configuration and observability foundation.
- [x] CI and coverage gates.
- [x] Repository bootstrap and GitHub delivery.

## Phase 1 — durable service core

- PostgreSQL via SQLx for decisions, audit events, risk changes, and broker state.
- Migration and reconciliation workflow.
- Structured tracing spans and metrics.
- Operational readiness checks tied to real dependencies.

## Phase 2 — agent-runtime integration

- Pin `agent-runtime` to a known revision.
- Provider/model configuration through typed, secret-aware settings.
- Schema-constrained request/response contracts.
- Timeout, retry, transport, and provider-failure tests.
- Do not place model decisions directly into execution.

## Phase 3 — Jev decision adapter

- Validate early access and pricing.
- Define typed decision inputs, outputs, and invalid/unknown handling.
- Adversarial tests for hallucinated instruments and unsupported actions.
- Benchmark latency and cost under realistic batch questions.

## Phase 4 — broker integration

- [x] Verify IFC Markets-supported access methods and account restrictions
      (no public retail REST/FIX API; MT4 has no MQL4 socket API).
- [x] Compare hosted bridge and MQL4 EA paths (bridges are paid; EA chosen as
      the zero-recurring-cost first implementation).
- [x] Broker integration boundary (`BrokerLink`) with provider selection and
      the EA control channel as the first implementation.
- [ ] Live probe round trip on the terminal (heartbeat + ping/pong).
- [ ] Replace the probe protocol with an idempotent command/ack queue; add
      command acknowledgements and independent reconciliation.
- [ ] Staged testing: read-only first, then demo, then minimal live exposure
      only after explicit owner approval.

## Phase 5 — deterministic risk and control

- Explicit policy configuration with schema and audit trail.
- Position/exposure/order limits, instrument and session allowlists.
- Kill switch and fail-closed behavior.
- Require two independent controls for any live mode transition.

## Phase 6 — console

- TanStack Start + TypeScript + React + Tailwind + shadcn/ui.
- Authentication, authorization, and audit trails.
- Position, decision, risk, and incident views.
- Frontend unit, integration, type-check, build, and coverage gates.

## Phase 7 — deployment

- Durable 24/7 host or VPS, managed secrets, backups, and monitoring.
- Versioned deployment pipeline.
- Staged rollout: local → paper account → minimal live exposure only after explicit owner approval.
