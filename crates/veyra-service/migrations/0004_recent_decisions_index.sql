-- Keep the recent-decision feed ordered while skipping unrelated audit events.
create index if not exists audit_events_recent_decisions_idx
    on audit_events (at desc, id desc)
    where kind in ('proposal_evaluated', 'position_closed', 'command_failed');
