# Veyra

Veyra is the foundation for an autonomous, provider-neutral trading service built with Rust and Actix Web. It will orchestrate a configured LLM runtime, Jev-style structured decisions, broker execution, durable risk and reconciliation logic, and a web console.

## Current status

This repository contains the first tested, safe service slice:

- Typed runtime configuration parsed into refined types.
- Read-only `/health`, `/ready`, and `/status` HTTP endpoints.
- Broker integration abstraction (`BrokerLink`) with a configuration-selected
  provider; the MetaTrader 4 EA control channel is the first implementation.
- Model-provider abstraction (`DecisionEngine`) over `agent-runtime` (pinned git
  revision), configurable to OpenRouter or any OpenAI-compatible endpoint, with
  schema-constrained answers — **proven live** with a real structured response.
- Jev (TypeSafe System One) behind its own `SemanticJudge` contract: typed
  choice/noul/score questions, answers validated against the request that
  produced them — **proven live** (`jev-1.13.0`, about 1.3 s per request).
- Loopback-only EA endpoint with token authentication, refined payloads,
  heartbeat state, and an **idempotent command queue** — proven live end-to-end
  with MT4 under Wine through a Cloudflare Tunnel (heartbeat, ping/pong, and a
  real `account_snapshot` command round trip).
- Deterministic, fail-closed risk gate (kill switch, instrument allowlist, UTC
  session, volume/order caps, duplicate suppression) in front of every intent,
  plus broker-side order validation (`order_check`) that never sends an order —
  proven live (retcode 0 for a valid request, 129 for a wrong-side limit).
- 24/7 supervision: launchd agents for the terminal, tunnel, and service with
  crash restart, proven by `SIGKILL` (ADR 0005).
- Structured JSON logging, bind-failure propagation, and graceful shutdown.
- Rust unit, integration, and line-coverage gates.
- GitHub Actions quality workflow.
- Architecture and roadmap documentation.

Trading is disabled by construction: no code path can execute an order yet, and
credentials never reach the service — the MT4 terminal owns the session.

## Architecture direction

- **Service core:** Rust 2024 + Actix Web + Tokio owns durable orchestration and the execution boundary.
- **Agent runtime:** integration point for the existing `agent-runtime` crate so OpenAI-compatible, Anthropic, Gemini, Cohere, Bedrock, and self-hosted models remain configurable.
- **Jev adapter:** separate structured decision adapter, never a chat wrapper; implemented with typed, validated judgements (`SemanticJudge`).
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

For unattended operation, install the supervised stack (release build; the
tunnel and service restart automatically, MT4 starts at login; logs under
`~/Library/Logs/veyra`):

```sh
cargo build --release
./scripts/install-launchd.sh                 # render + bootstrap the agents
./scripts/install-launchd.sh --uninstall     # remove them again
```

After changing code, rebuild and restart the supervised service:
`cargo build --release && launchctl kickstart -k gui/$(id -u)/cc.antonlabs.veyra.service`
(see ADR 0005).

Then inspect:

- `http://127.0.0.1:8080/health`
- `http://127.0.0.1:8080/ready`
- `http://127.0.0.1:8080/status`

## Model configuration

`VEYRA_MODEL_PROVIDER=openrouter` plus `VEYRA_MODEL_API_KEY` and three explicit
tier models (`VEYRA_MODEL_FAST`, `VEYRA_MODEL_BALANCED`, `VEYRA_MODEL_REASONING`)
enable structured decisions. Partial configuration fails closed at startup.

Tier models must accept forced tool calls — the structured path enforces the
schema through `tool_choice`, so reasoning modes that reject it cannot be used.
Live check:

```sh
set -a; source .env; set +a
cargo test --test model_live -- --ignored --nocapture
```

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
   `VEYRA_EA_URL` to the tunnel endpoint when using Cloudflare). Live order
   placement additionally requires compiling with `InAllowLiveOrders = true`.
2. MT4 → Options → Expert Advisors: enable automated trading, and add the
   endpoint URL (for example `https://veyra.antonlabs.cc/ea/poll`) to the
   WebRequest allowlist. Listing the exact URL is the habit that has held on
   every build; a host-only entry has also matched by prefix in practice.
3. Attach `VeyraProbe` to a chart. After recompiling the EA, restart the
   terminal (or remove and re-attach the EA): MT4 does not hot-reload
   externally rebuilt `.ex4` files.

Commands: the service delivers typed commands (`ping`, `account_snapshot`,
`order_check`) on a poll; the EA executes and acknowledges by id. Delivery is
at-least-once, a command fails after a 15 s timeout, and acknowledgements are
validated against the command's typed payload before they are recorded.
`order_check` only validates a gate-approved request through the terminal's
market rules and margin engine — it never sends an order, and mutating commands
will keep the same id/ack discipline and must be idempotent per id.

Loopback control surface (`127.0.0.1:8080`): `POST /intents/evaluate` returns a
risk decision without queueing anything, `POST /intents/check` queues one
`order_check` for an approved intent, `POST /intents/execute` queues
`open_order` and `POST /intents/close` closes one Veyra-owned ticket (both
refused unless `VEYRA_TRADING_ENABLED=true`), and
`POST /commands/account_snapshot` refreshes venue state (orders, open volume,
bounded position list with magic numbers); every command is pollable through
`GET /commands/{id}`.

Staged execution: with the service switch on, the terminal still validates the
request and reports a dry run until the EA is recompiled with
`InAllowLiveOrders = true` — two deliberate acts before real money moves.

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
