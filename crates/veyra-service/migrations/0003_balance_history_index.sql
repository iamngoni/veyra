-- Scope read-only balance history to the active broker account and time.
create index if not exists audit_events_balance_history_idx
    on audit_events ((payload->>'login'), (payload->>'server'), ((payload->>'atMs')::bigint), at, id)
    where kind = 'balance_observed';
