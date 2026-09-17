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

## Phase 2 — model integration

- [x] Pin `agent-runtime` to a known revision.
- [x] Provider/model configuration through typed, secret-aware settings.
- [x] Schema-constrained request/response contracts (`DecisionEngine`).
- [x] Dynamic-schema support contributed upstream (`run_structured_with_format`).
- [x] Retry policy for transient provider failures; deterministic adapter tests
      with an injected transport; live OpenRouter proof.
- [ ] Per-tier budget/rate-limit policy in the operations layer.
- [x] Model decisions never reach execution directly (no execution exists).

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
- [x] Live probe round trip on the terminal (heartbeat + ping/pong), via the
      Cloudflare tunnel with Wine IPv4 pinning (ADR 0002).
- [x] Idempotent command/ack queue with typed payload validation, timeouts,
      and read-only commands (`ping`, `account_snapshot`) — proven live.
- [ ] Mutating commands (open/close/modify) with the same id/ack discipline,
      restricted by the deterministic risk gate.
- [ ] Independent reconciliation against broker state.
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
- [ ] Exposure limits in lots (needs the EA to report open volume).
- [ ] Policy persistence and audit trail (needs Phase 1 storage).
- [ ] Require two independent controls for any live mode transition.

## Phase 6 — console

- TanStack Start + TypeScript + React + Tailwind + shadcn/ui.
- Authentication, authorization, and audit trails.
- Position, decision, risk, and incident views.
- Frontend unit, integration, type-check, build, and coverage gates.

## Phase 7 — deployment

- Durable 24/7 host or VPS, managed secrets, backups, and monitoring.
- Versioned deployment pipeline.
- Staged rollout: local → paper account → minimal live exposure only after explicit owner approval.
