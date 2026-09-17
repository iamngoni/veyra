# Veyra overview

Veyra is designed as an unattended trading service rather than a chat application. The system separates probabilistic reasoning from deterministic execution control:

1. **Runtime configuration** identifies the model/provider, broker adapter, and policy source.
2. **Context collectors** maintain typed market/account/session state.
3. **Decision adapters** request structured outputs from the configured model or Jev.
4. **Risk gate** validates proposed orders against hard limits and session rules.
5. **Broker adapter** executes only risk-approved operations.
6. **Reconciler** verifies actual broker state and persists durable outcomes.

## Non-goals for this slice

- No credentials, live account, or execution capability.
- No vendor lock-in or desktop automation.
- No Jev call, provider key, or deployment environment.
- No frontend yet.

## Operating goal

The service should be restart-safe, observability-driven, and fail closed. Any ambiguity or infrastructure failure must not become an unapproved trade.
