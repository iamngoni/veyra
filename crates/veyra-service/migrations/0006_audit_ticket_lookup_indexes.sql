-- Ticket-episode lookups (the Trades page and the assistant's position
-- journal) filter audit rows by ticket or command id inside the JSON payload.
-- Without these, each lookup walks the whole table. The query builder
-- (store.rs `filtered_query`) fences these filters so the planner uses them.
create index if not exists audit_events_ticket_idx
    on audit_events ((payload->>'ticket'))
    where payload->>'ticket' is not null;

create index if not exists audit_events_result_ticket_idx
    on audit_events ((payload->'result'->>'ticket'))
    where payload->'result'->>'ticket' is not null;

create index if not exists audit_events_command_id_idx
    on audit_events ((payload->>'command_id'))
    where payload->>'command_id' is not null;
