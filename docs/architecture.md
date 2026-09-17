# Veyra architecture

## Current implementation

The first crate is `veyra-service`, a Rust 2024 Actix Web control plane. It exposes read-only health, readiness, and status endpoints. Runtime settings are parsed into refined types before they reach handlers. The public status contract reports `broker_connected: false` and `trading_enabled: false`.

## Boundaries

```text
Config -> Runtime state -> HTTP control plane
             |
             +-- Decision adapter (future)
             +-- Deterministic risk gate
             +-- Broker adapter
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

### Broker and IFC Markets

IFC Markets documents MT4/MQL4 program trading; no public retail broker REST/FIX API has been verified. The candidate adapters are:

1. A vetted hosted API bridge, if it is independently evaluated, supports the required operations, and the account owner explicitly approves credentials and costs.
2. A narrowly scoped MQL4 EA fallback using a private control protocol, idempotent command IDs, local execution safety, and independent reconciliation.

No bridge is selected. No credentials are stored. The Veyra core will not depend on either choice.

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
