# Veyra overview

Veyra is designed as an unattended trading service rather than a chat application. The system separates probabilistic reasoning from deterministic execution control:

1. **Runtime configuration** identifies the model/provider, broker adapter, judgement provider, and policy source.
2. **Context collectors** maintain typed market/account/session state.
3. **Decision adapters** request structured outputs from the configured model (`DecisionEngine`) and calibrated judgements from Jev (`SemanticJudge`).
4. **Risk gate** validates proposed orders against hard limits and session rules.
5. **Broker adapter** carries read-only commands and broker-side validation today; execution will accept only risk-approved operations.
6. **Reconciler** verifies actual broker state and persists durable outcomes.

## Non-goals for this slice

- No order execution: every broker-facing command is read-only or a validation
  (`order_check`), and nothing can place an order.
- No vendor lock-in: integrations stay behind swappable contracts
  (`BrokerLink`, `DecisionEngine`, `SemanticJudge`).
- No deployment environment or 24/7 host yet; see the roadmap's deployment
  phase.
- No frontend yet.

## Operating goal

The service should be restart-safe, observability-driven, and fail closed. Any ambiguity or infrastructure failure must not become an unapproved trade.
