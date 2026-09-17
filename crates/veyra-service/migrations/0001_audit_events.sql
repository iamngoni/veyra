-- Append-only audit trail: commands, acknowledgements, broker snapshots, and
-- reconciliation results. Rows are never updated or deleted by the service.
create table if not exists audit_events (
    id uuid primary key,
    at timestamptz not null default now(),
    kind text not null,
    payload jsonb not null
);

create index if not exists audit_events_at_idx on audit_events (at desc);
