-- Non-trade account entries from the terminal's history: dividend
-- adjustments on index CFDs, corrections, deposits, withdrawals and credit.
-- They carry no magic number, so they are kept apart from closed_trades and
-- never pruned with the audit trail. Times are the broker server clock,
-- exactly as the terminal reports them; `comment` is the broker's own text.
create table if not exists balance_operations (
    ticket bigint primary key,
    kind text not null,
    amount double precision not null,
    op_time bigint not null,
    comment text not null default '',
    first_seen_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists balance_operations_time_idx
    on balance_operations (op_time desc, ticket desc);
