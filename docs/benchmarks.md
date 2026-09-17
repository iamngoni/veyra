# Benchmarks

Measured on the live machine (2026-09-17, evening) while the supervised stack
was running. Method: the ignored live tests run 5× each against the real
providers with the configured keys; the numbers are the per-run latencies the
tests print.

```sh
set -a; source .env; set +a
for i in 1 2 3 4 5; do cargo test --test jev_live -- --ignored --nocapture; done
for i in 1 2 3 4 5; do cargo test --test model_live -- --ignored --nocapture; done
```

## Jev (TypeSafe System One, `jev-1.13.0`)

- Three-question judgement set (direction choice, trending noul, momentum
  score) over a market narrative.
- Latency: **1.11 s min, 1.45 s mean, 1.99 s max** (5 runs).
- Usage: **408 input / 73 output tokens** per request, stable across runs.
  Pricing depends on the TypeSafe plan (console item, still open).
- Formatting note: in a separate 20-run sample of the autopilot's question
  shape, 2 answers (10%) returned probabilities summing to 0.99 — two-decimal
  rounding. The contract tolerates 0.02 (`PROBABILITY_TOLERANCE`); before
  that, whole ticks were skipped for pure formatting.

## Model (OpenRouter, balanced tier = `openai/gpt-4.1-mini`)

- One structured trade-proposal call (forced tool schema).
- Latency: **1.40 s min, ~2.07 s mean, 2.84 s max** (5 runs).

## Autopilot tick

- One tick costs one Jev call, one model call, and one `rates` round trip to
  the terminal (through the tunnel). Ticks log a few seconds after their
  interval fires, consistent with the sums above.
- At the live 60 s cadence the loop can spend up to 1440 model calls and 1440
  judgement calls per day when positioned continuously; the call budget
  (`VEYRA_MODEL_MAX_CALLS_PER_HOUR` / `_PER_DAY`) bounds accidents, not
  normal use.
