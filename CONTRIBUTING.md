# Contributing

Run all `ngoni-rust` gates before opening a pull request:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo coverage
```

`main.rs` is the only documented coverage exclusion: it is process wiring exercised by `scripts/smoke.sh` rather than unit-testable domain logic.

Keep changes focused, document public contracts, and preserve safety boundaries. Account credentials, broker passwords, and provider keys are never accepted in source, examples, screenshots, or logs.
