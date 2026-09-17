# ADR 0004 — Jev is a separate semantic-judge boundary

## Status

Accepted 2026-09-17; implemented and proven live (`jev-1.13.0`, about 1.3 s
for a three-question request).

## Context

- Model providers (`DecisionEngine`) return schema-constrained generations
  with tiers and budgets; that contract fits chat-style LLMs.
- Jev is a System One model: it returns calibrated judgements — a choice with
  a probability distribution, a noul probability, or a score with a legend —
  each with confidence. It does not generate text or reasoning.
- Forcing both through one trait would either bend Jev into a schema it does
  not have or leak untyped JSON into trading code.

## Decision

- Jev gets its own module (`jev/`), its own narrow contract
  (`SemanticJudge`), its own provider selector (`VEYRA_JEV_PROVIDER`, default
  `typesafe`), and its own validated contract types.
- Requests are parsed once at the boundary (state, instructions, question ids,
  option and level shapes); responses are validated against the request that
  produced them: ids and types must match, a chosen option must have been
  offered and carry the maximum probability, a score legend must equal the
  requested levels, and distributions must sum to one within tolerance.
- Transport is one shared HTTP client with explicit connect and total timeouts;
  status codes map to typed errors, and the API key is redacted from `Debug`.
- Judgements are inputs code may consult. They grant no execution authority:
  only the deterministic risk gate can approve an intent.

## Consequences

- Adding another System One provider is an implementation plus a variant; no
  caller changes.
- Trade decisions can combine model proposals with cheap calibrated judgements
  without weakening the gate.
- Pricing and per-request budget accounting for Jev belong to the operations
  layer and are not implemented yet.
