# Contributing

Run all `ngoni-rust` gates before opening a pull request:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo coverage
```

The documented coverage exclusions are `src/main.rs` (process wiring exercised by `scripts/smoke.sh`) and `tests/*_live.rs` (ignored tests that require live external systems).

Keep changes focused, document public contracts, and preserve safety boundaries. Account credentials, broker passwords, and provider keys are never accepted in source, examples, screenshots, or logs.
