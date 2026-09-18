-- Durable runtime state: counters and baselines that must survive service
-- restarts. One row per key, replaced atomically; the owning runtime
-- validates every value before use, so a hand-edited row cannot bypass the
-- boundaries that produced it.
--
-- Keys currently in use: jev_usage, model_budget, equity_baselines,
-- stop_basis, risk_policy.
create table if not exists runtime_state (
    key text primary key,
    value jsonb not null,
    updated_at timestamptz not null default now()
);
