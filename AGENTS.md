# Veyra agent instructions

## Product discipline

Veyra is an autonomous trading service foundation. It must not silently imply that broker, Jev, LLM, or live-trading functionality exists. Never hardwire a vendor, desktop app, operating system, or provider into the core. Execution must always remain behind a deterministic risk gate.

## Rust standards

- Follow `ngoni-rust`: Rust 2024, Actix Web, Tokio, refined types with fallible constructors, and explicit lifecycle boundaries.
- Add `//!` source headers explaining purpose and safety/lifecycle boundaries.
- Document every public contract.
- No production `unwrap`, `expect`, `todo!`, or `unimplemented!`.
- Propagate errors; never hide malformed input with a default.
- Use one shared HTTP client with explicit connect and total timeouts when HTTP integration is added.
- Distinguish liveness from readiness, use structured tracing, and propagate bind/startup failures.
- Unit-test parsing boundaries and lifecycle; exercise the actual binary and shutdown with `scripts/smoke.sh`.

## Quality gates

Every change must pass locally before commit:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo coverage
```

Report local, CI, live, and deployment proof separately. Do not call an untested broker path safe.

## Git and repository

- Commits use GitWhisper (`gw commit`).
- Never commit secrets, account credentials, broker passwords, API keys, logs, or real account identifiers.
- Prefer focused commits and keep `main` clean.

## Scope boundaries

Do not add fake trading, demo fills, or mock broker data as if it were production functionality. Integrations must declare their boundary and tests must verify it.
