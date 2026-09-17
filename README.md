# Veyra

Veyra is the foundation for an autonomous, provider-neutral trading service built with Rust and Actix Web. It will orchestrate a configured LLM runtime, Jev-style structured decisions, broker execution, durable risk and reconciliation logic, and a web console.

## Current status

This repository contains the first tested, safe service slice:

- Typed runtime configuration parsed into refined types.
- Read-only `/health`, `/ready`, and `/status` HTTP endpoints.
- Structured JSON logging, bind-failure propagation, and graceful shutdown.
- Rust unit, integration, and line-coverage gates.
- GitHub Actions quality workflow.
- Architecture and roadmap documentation.

Trading is disabled by construction: there is no broker connection and no code path that can execute an order. This is intentional, not a missing feature in this initial slice.

## Architecture direction

- **Service core:** Rust 2024 + Actix Web + Tokio owns durable orchestration and the execution boundary.
- **Agent runtime:** integration point for the existing `agent-runtime` crate so OpenAI-compatible, Anthropic, Gemini, Cohere, Bedrock, and self-hosted models remain configurable.
- **Jev adapter:** separate structured decision adapter, never a chat wrapper.
- **Risk gate:** deterministic policy validates every proposed action before execution. Model confidence does not override limits.
- **Broker adapter:** deliberately undecided. Candidate approaches include an official/vetted hosted broker API or a narrowly scoped, tested MQL4 EA. No account credentials or IFC Markets endpoint are configured.
- **Persistence:** PostgreSQL/SQLx when durable state is introduced.
- **Console:** TanStack Start + TypeScript + React + Tailwind + shadcn/ui when the UI begins.

See [`docs/architecture.md`](docs/architecture.md) and [`docs/roadmap.md`](docs/roadmap.md).

## Runtime

```sh
export VEYRA_BIND_HOST=127.0.0.1
export VEYRA_BIND_PORT=8080
export VEYRA_ENV=development
cargo run -p veyra-service
```

Then inspect:

- `http://127.0.0.1:8080/health`
- `http://127.0.0.1:8080/ready`
- `http://127.0.0.1:8080/status`

## Quality

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo coverage
```

## Safety

Veyra is intended to operate an existing brokerage account only after explicit risk controls, credentials, reconciliation, and staged testing are in place. It does not provide financial advice and does not guarantee profitable trading.
