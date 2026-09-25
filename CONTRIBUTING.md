# Contributing

Veyra places real orders with a real brokerage account. A contribution is
judged first on whether it keeps that path safe, and only then on the feature
it adds.

## Before you start

- **License.** Veyra is source-available under a personal-use license (see
  [`LICENSE`](LICENSE)); it is not open source. By submitting a contribution you
  confirm you have the right to submit it, and you grant the copyright holder a
  perpetual, irrevocable, royalty-free right to use, modify, relicense, and
  distribute it as part of Veyra.
- **Talk first for anything large.** Open an issue before changing the risk
  gate, the execution path, a broker or model integration, or the database
  schema.
- **Security problems** go through [`SECURITY.md`](SECURITY.md), never a public
  issue.

## Setup

[`GETTING_STARTED.md`](GETTING_STARTED.md) covers the full install. For
development you need:

- Rust from `rust-toolchain.toml` (installed automatically by `rustup`) and the
  `llvm-tools-preview` component for coverage.
- Node 22 for the console (`cd console && npm install`).
- PostgreSQL only for the ignored `store_live` test and a real run; unit and
  contract tests do not need it.

## Layout

| Path | What lives there |
| --- | --- |
| `crates/veyra-service/src` | Service: config, HTTP routes, risk gate, autopilot, integrations |
| `crates/veyra-service/tests` | Contract tests; `*_live.rs` need real external systems |
| `crates/veyra-service/migrations` | SQLx migrations, applied in order at startup |
| `console/` | Operations console (TanStack Start, React, Tailwind) |
| `ea/` | MQL4 Expert Advisor for the MT4 control channel |
| `scripts/` | Quality gates, smoke test, install, and backup scripts |
| `docs/` | Architecture, roadmap, deployment, and decision records |

## Quality gates

Run everything with one command before you commit:

```sh
./scripts/check.sh
```

It runs the same steps as CI (`.github/workflows/quality.yml`):

- **Rust:** `cargo fmt --all -- --check`, `cargo clippy --all-targets
  --all-features -- -D warnings`, `cargo test --all-targets`, and
  `cargo coverage`.
- **Console:** `npm run typecheck`, `npm run test:coverage`, and
  `npm run build`. It skips these if `console/node_modules` is missing, but CI
  does not.

Coverage thresholds:

- **Service:** at least 94% of lines. Excluded: `src/main.rs` (process wiring,
  exercised by `scripts/smoke.sh`), `src/store.rs` (PostgreSQL, exercised by
  `store_live`), and `tests/*_live.rs`.
- **Console:** at least 95% of lines, functions, branches, and statements.

Don't lower a threshold to land a change; add the tests instead.

When you touch startup, shutdown, or routing, also run `./scripts/smoke.sh`.
It starts the real binary, calls the diagnostic routes, and checks that
`SIGINT` shuts it down cleanly.

### Live tests

Tests in `tests/*_live.rs` are `#[ignore]`d and talk to real systems (the
model provider, Jev, MT4, PostgreSQL). Run one on purpose, with its
configuration loaded:

```sh
set -a; source .env; set +a
cargo test --test model_live -- --ignored --nocapture
```

Report proof separately: local gates, CI, live tests, and deployment are four
different claims. Never describe a broker path as safe until it has been
tested live.

## Code standards

- Rust 2024, Actix Web, Tokio.
- Parse input into refined types with fallible constructors. Don't replace
  malformed input with a default.
- No `unwrap`, `expect`, `todo!`, or `unimplemented!` outside tests. Propagate
  errors instead.
- Every source file starts with a `//!` header that explains its purpose and
  its safety or lifecycle boundary.
- Document every public item.
- Use structured `tracing` fields, not formatted strings.
- Share one HTTP client with explicit connect and total timeouts.
- Console components follow the existing patterns in
  `console/src/components`. Keep copy terse and give every control a clear
  purpose.

## Safety boundaries

These are not negotiable:

- **Every order passes the deterministic risk gate.** Model output is untrusted
  input. It can propose an action but never bypass or tune the gate.
- **Execution stays behind two controls:** `VEYRA_TRADING_ENABLED` in the
  service and `InAllowLiveOrders` in the EA (ADR 0006).
- **Integrations sit behind narrow traits** (`BrokerLink`, `DecisionEngine`,
  `SemanticJudge`, `MarketFeed`), chosen by configuration and constructed once
  at startup. Domain, risk, and reporting code never calls a vendor SDK or
  transport directly, and no vendor, OS, or desktop app is hardwired into the
  core.
- **No fake trading.** Never add demo fills, mock broker data, or simulated
  results that could pass for production behaviour. Test doubles belong in
  tests.
- **The assistant is read-only.** It never gets an order, close, or modify
  tool.

## Secrets

Never commit or paste credentials, API keys, broker passwords, account
numbers, real account identifiers, logs, or order history. That includes
code, examples, tests, screenshots, and issue text. `.env` stays local, and
`.env.example` holds placeholders only.

## Decision records

A change to architecture, a safety boundary, or an integration contract needs
an ADR in [`docs/decisions`](docs/decisions). Number it after the latest one
and cover the status, the context, the decision, and its consequences. Update
[`docs/architecture.md`](docs/architecture.md) when behaviour it describes
changes.

## Commits and pull requests

- Keep each commit to one concern, and keep `main` releasable.
- Commit with GitWhisper (`gw commit`), in the imperative style:
  `feat: Add trades pagination`, `fix: …`, `perf: …`, `chore: …`.
- In the pull request, say what changed, why, and how you verified it. List
  which gates and live tests you ran, and call out any change to risk,
  execution, or the schema.
