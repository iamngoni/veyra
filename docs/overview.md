# Veyra overview

Veyra is designed as an unattended trading service rather than a chat application. The system separates probabilistic reasoning from deterministic execution control:

1. **Runtime configuration** identifies the model/provider, broker adapter, judgement provider, and policy source.
2. **Context collectors** maintain typed market/account/session state.
3. **Decision adapters** request structured outputs from the configured model (`DecisionEngine`) and calibrated judgements from Jev (`SemanticJudge`).
4. **Risk gate** validates proposed orders against hard limits and session rules.
5. **Broker adapter** carries read-only commands, broker-side validation, and gate-approved open, close, and modify orders.
6. **Reconciler** verifies actual broker state and persists durable outcomes.

## Boundaries

- Orders execute only after the risk gate approves them and both execution
  controls are armed (`VEYRA_TRADING_ENABLED` and the EA's
  `InAllowLiveOrders`).
- No vendor lock-in: integrations stay behind swappable contracts
  (`BrokerLink`, `DecisionEngine`, `SemanticJudge`, `MarketFeed`).
- The console assistant is read-only and cannot place, close, or modify
  orders.
- Supervised launchd and hybrid Docker deployments are described in
  `docs/deployment*.md`.

## Operating goal

The service should be restart-safe, observability-driven, and fail closed. Any ambiguity or infrastructure failure must not become an unapproved trade.
