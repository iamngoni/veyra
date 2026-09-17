#!/usr/bin/env bash
# Runs every local quality gate: the Rust workspace and the console.
#
# CI runs the same steps (see .github/workflows/rust.yml); this script exists
# so a change can be verified with one command before it is pushed.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== cargo fmt"
cargo fmt --all -- --check

echo "== cargo clippy"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "== cargo test"
cargo test --workspace

echo "== cargo coverage"
cargo coverage

if [ -d console/node_modules ]; then
  echo "== console typecheck"
  (cd console && npm run --silent typecheck)
  echo "== console build"
  (cd console && npm run --silent build)
else
  echo "== console checks skipped (run npm install in console/ first)"
fi

echo "CHECK_OK"
