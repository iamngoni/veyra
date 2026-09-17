# ADR 0001 — initial architecture

## Status

Accepted.

## Context

Veyra must eventually operate an existing IFC Markets MT4 account unattended. A general LLM cannot safely decide execution directly, IFC has no verified public retail API, and model/provider flexibility is required.

## Decision

Use a Rust 2024 + Actix Web/Tokio service core. Keep decision adapters, deterministic risk policy, broker adapters, and persistence as explicit modules. Integrate `agent-runtime` for provider neutrality and treat Jev as a separate structured adapter. Do not select the broker bridge yet.

## Consequences

- No order execution exists in this initial slice, which is a deliberate safety boundary.
- Veyra remains cross-platform and avoids hardwired Windows/UI automation.
- The broker path can be changed later without contaminating risk policy.
- Live trading requires explicit credentials, policy, reconciliation, and staged proof.
