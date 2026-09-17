# ADR 0006 — Staged execution behind two independent controls

## Status

Accepted 2026-09-17; plumbing implemented and proven live as a dry run
(nothing sent to the broker). Live order placement is not yet enabled.

## Context

- The goal is unattended execution, but the account is real money and the
  roadmap requires staged rollout: read-only first, then demo/validation, then
  minimal live exposure only after explicit owner approval.
- Gate approval alone must never be sufficient to move money: a bug in the
  pipeline, a stale process, or an operator mistake must not trade silently.
- The EA is the only component with the broker session, so it is the natural
  place for a final, physical control.

## Decision

- One execution entry point: `POST /intents/execute`. It refuses unless
  `VEYRA_TRADING_ENABLED=true` (default false) and refuses when no command
  channel is available. A rejected draft returns the deterministic risk
  rejection; an approved one becomes a typed `open_order` command with the
  Veyra magic number.
- The terminal validates every execution request with the same rules as
  `order_check` and, while compiled with the default `InAllowLiveOrders =
  false`, acknowledges a dry run (`executed:false`, `retcode:0`) without
  calling `OrderSend`. The live branch is compiled and reviewed but
  deliberately unexercised.
- Acks are typed and validated: an `executed` order must report a ticket and a
  price, so a malformed or contradictory ack fails closed instead of being
  recorded as a fill.
- Enabling live trading therefore requires two independent, deliberate acts:
  the service switch in `.env` and a recompiled EA input.

## Consequences

- The whole path up to the broker call is exercised live today, which is as
  close to a demo account as the current setup allows.
- Going live is a single reviewed step: `InAllowLiveOrders = true`, with the
  service switch already understood.
- `close_order` shipped on the same pattern: the service accepts only tickets
  from the latest completed `account_snapshot` carrying the Veyra magic, and
  the terminal re-checks the magic before closing. `modify` still follows.
  Position-tracing reconciliation (magic number to intent) lands with the
  reconciler before unattended live trading.
