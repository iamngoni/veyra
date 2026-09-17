# Veyra

Veyra is the foundation for an autonomous, provider-neutral trading service built with Rust and Actix Web. It will orchestrate a configured LLM runtime, Jev-style structured decisions, broker execution, durable risk and reconciliation logic, and a web console.

## Current status

This repository contains the first tested, safe service slice:

- Typed runtime configuration parsed into refined types.
- Read-only `/health`, `/ready`, and `/status` HTTP endpoints.
- Broker integration abstraction (`BrokerLink`) with a configuration-selected
  provider; the MetaTrader 4 EA control channel is the first implementation.
- Loopback-only EA endpoint with token authentication, refined payloads, and
  heartbeat state (no command execution yet) — **proven live** end-to-end with
  MT4 under Wine through a Cloudflare Tunnel (heartbeat plus ping/pong).
- Structured JSON logging, bind-failure propagation, and graceful shutdown.
- Rust unit, integration, and line-coverage gates.
- GitHub Actions quality workflow.
- Architecture and roadmap documentation.

Trading is disabled by construction: no code path can execute an order yet, and
credentials never reach the service — the MT4 terminal owns the session.

## Architecture direction

- **Service core:** Rust 2024 + Actix Web + Tokio owns durable orchestration and the execution boundary.
- **Agent runtime:** integration point for the existing `agent-runtime` crate so OpenAI-compatible, Anthropic, Gemini, Cohere, Bedrock, and self-hosted models remain configurable.
- **Jev adapter:** separate structured decision adapter, never a chat wrapper.
- **Risk gate:** deterministic policy validates every proposed action before execution. Model confidence does not override limits.
- **Broker integration:** every venue implements the `BrokerLink` contract and
  is selected by `VEYRA_BROKER_PROVIDER` at startup. The first implementation is
  the MQL4 EA control channel (loopback HTTP, token-authenticated); a hosted
  bridge or direct API can be added later without touching callers.
- **Persistence:** PostgreSQL/SQLx when durable state is introduced.
- **Console:** TanStack Start + TypeScript + React + Tailwind + shadcn/ui when the UI begins.

See [`docs/architecture.md`](docs/architecture.md) and [`docs/roadmap.md`](docs/roadmap.md).

## Runtime

```sh
set -a; source .env; set +a
cargo run -p veyra-service
```

With `VEYRA_BROKER_PROVIDER=ea` the service also serves the EA control channel
on loopback (`VEYRA_EA_BIND_HOST:VEYRA_EA_BIND_PORT`). Build and install the
probe EA with `./scripts/compile_ea.sh` (reads `VEYRA_EA_TOKEN` from `.env`).

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

## EA channel operation (MT4)

One-time terminal setup:

1. `./scripts/compile_ea.sh` (reads `VEYRA_EA_TOKEN` from `.env`; set
   `VEYRA_EA_URL` to the tunnel endpoint when using Cloudflare).
2. MT4 → Options → Expert Advisors: enable automated trading, and add the
   **exact endpoint URL** (for example `https://veyra.antonlabs.cc/ea/poll`)
   to the WebRequest allowlist — MT4 matches the full URL, not the host.
3. Attach `VeyraProbe` to a chart. After recompiling the EA, remove and
   re-attach it: MT4 does not hot-reload externally rebuilt `.ex4` files.

Platform notes (see docs/decisions/0002):

- MQL4 has no sockets, and WebRequest only supports the scheme-default port
  (80/443), so the channel runs over HTTPS via the tunnel.
- Wine does not fall back from IPv6 to IPv4; the tunnel hostname is pinned to
  its Cloudflare IPv4 addresses in `/etc/hosts`.

Live proof (requires MT4 with the probe attached):

```sh
set -a; source .env; set +a
cargo test --test ea_channel_live -- --ignored --nocapture
```

## Safety

Veyra is intended to operate an existing brokerage account only after explicit risk controls, credentials, reconciliation, and staged testing are in place. It does not provide financial advice and does not guarantee profitable trading.
