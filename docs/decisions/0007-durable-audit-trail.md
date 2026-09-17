# ADR 0007 — Durable audit trail in PostgreSQL

## Status

Accepted 2026-09-17; implemented and proven live against PostgreSQL 17
(migration, record, restart-persistent read-back, and the periodic refresh
cycle audited end to end).

## Context

- Everything the service knew lived in memory: a restart erased command
  history, broker snapshots, and any evidence an operator would need after an
  incident.
- The roadmap's first phase calls for PostgreSQL via SQLx for audit events and
  broker state; the reconciliation phase also needs ticket-level tracing.
- The audit trail must never become a way for a database outage to silently
  change trading behaviour.

## Decision

- One append-only `audit_events` table (id, `timestamptz`, kind, JSONB payload)
  created by embedded SQLx migrations that run before the service listens.
- An `AuditTrail` trait keeps storage swappable; the service ships a
  PostgreSQL implementation, and an in-memory trail exists for tests only.
- Recorded events: command queueing (including periodic refreshes), terminal
  acknowledgements (completed or failed), validated broker snapshots, and
  service starts. Payloads are bounded summaries, never credentials.
- A configured `VEYRA_DATABASE_URL` that cannot be reached fails startup, so a
  missing trail is never mistaken for an empty one. Once running, writes are
  best-effort: failures are logged and never block or fail a command.
- `GET /audit?limit=N` exposes the newest rows; `/status` reports the
  persistence provider.

## Consequences

- Restart-safe history now spans the command lifecycle and broker state.
  Ticket-level intent tracing can build on `command_completed` payloads once
  live orders exist.
- Retention, rotation, and backups are not implemented yet; the table grows
  with every refresh, so a maintenance job belongs in the deployment phase.
- `/ready` remains process-only until readiness is tied to real dependencies.
