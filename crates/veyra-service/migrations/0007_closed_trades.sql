-- Veyra's own record of closed trades, one row per venue ticket. Rows are
-- upserted from every account-history answer the terminal gives, so
-- performance never depends on how much history the terminal shows, and they
-- are never pruned with the audit trail. Times are the broker server clock,
-- exactly as the terminal reports them.
create table if not exists closed_trades (
    ticket bigint primary key,
    symbol text not null,
    kind text not null,
    lots double precision not null,
    open_price double precision not null,
    close_price double precision not null,
    open_time bigint not null,
    close_time bigint not null,
    profit double precision not null,
    swap double precision not null,
    commission double precision not null,
    magic bigint not null,
    first_seen_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists closed_trades_magic_close_idx
    on closed_trades (magic, close_time desc, ticket desc);
