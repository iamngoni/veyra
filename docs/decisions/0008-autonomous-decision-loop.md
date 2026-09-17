# ADR 0008 — Autonomous decision loop

## Status

Accepted 2026-09-17; live-proven against the real terminal after explicit
owner approval of both execution controls.

## Context

- The decision pipeline (`trading::pipeline`) existed but only ran when an
  operator posted a draft; nothing decided on its own, which is the product's
  whole point ("fully autonomous, works 24/7").
- Market data had no path at all: MT4's terminal owns the candles and exposes
  only the EA control channel.
- LLMs occasionally fill nullable or optional fields that the strict draft
  contract rejects (a reference `price` on a market order, an over-long
  `comment`, a stray `intent` beside `action: "none"`); failing the whole
  decision on phrasing that has no execution meaning wastes ticks without
  making anything safer.
- An autonomous loop that can place real orders needs guardrails that cannot
  be talked out of by a model, and full auditability.

## Decision

- `trading/autopilot.rs` runs one tick per `VEYRA_AUTOPILOT_INTERVAL_SECS`
  (disabled by default; the first tick lands one interval after startup).
  Every missing input skips the tick; provider failures are audited as
  `unavailable`; nothing is ever queued without a gate approval.
- Market data is a first-class integration boundary (`market::MarketFeed`,
  provider `ea`): a read-only `rates` command returns closed candles (oldest
  first, forming bar excluded) and the strategy layer only sees validated
  `CandleSeries` values.
- The model continues to answer with the constrained trade-proposal schema;
  `normalize_proposal` carries exactly three transport tolerances, each
  provably execution-neutral — echoed market `price`, embellished `comment`,
  stray `intent` beside `action: none`. Everything else still fails strict
  parsing, and the gate re-validates every draft.
- Every autonomous entry must include both `stop_loss` and `take_profit`;
  unbracketed proposals are rejected before the command layer.
- Approved entries reuse `queue_staged_order`, the same single execution path
  as the control surface, so the two operator controls (service switch and
  armed EA), the Veyra magic stamp, and the audit trail apply unchanged.
- Each tick records a `proposal_evaluated` audit event (`no_trade`,
  `rejected`, `approved_dry_run`, `queued`, `unavailable`) so an operator can
  reconstruct every decision from the durable trail.

## Consequences

- The bot can open positions unattended; exposure is bounded by the risk gate
  (symbol allowlist, per-order and total volume caps, one open order by
  default, duplicate window), and both operator switches still gate real
  money.
- Exit management is currently limited to the bracket attached at entry
  (broker-side SL/TP); active position management is a later iteration.
- Model latency and cost scale with cadence; the balanced tier answers a tick
  in a few seconds and every tick is visible in the console and audit trail.
