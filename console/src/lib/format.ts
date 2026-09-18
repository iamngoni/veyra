/**
 * Pure formatting and classification helpers for the Veyra console.
 *
 * Audit events arrive as untyped JSON payloads. This module owns the mapping
 * from event kind to presentation (badges, summaries, drill-down rows) so the
 * components stay declarative and every rule is unit-testable without a DOM.
 */

import type { CommandRecord, FeedEvent } from './api'

/** Kind → badge classes for the activity feed. Unknown kinds fall back to slate. */
export const kindTone: Record<string, string> = {
  proposal_evaluated: 'text-violet-300 bg-violet-500/10',
  agent_tool_called: 'text-fuchsia-300 bg-fuchsia-500/10',
  command_queued: 'text-sky-300 bg-sky-500/10',
  command_completed: 'text-emerald-300 bg-emerald-500/10',
  command_failed: 'text-rose-300 bg-rose-500/10',
  broker_snapshot: 'text-slate-400 bg-slate-700/30',
  position_closed: 'text-cyan-300 bg-cyan-500/10',
  service_started: 'text-amber-300 bg-amber-500/10',
  reconciliation_drift: 'text-rose-300 bg-rose-500/15',
}

/** Proposal/review outcome → text colour. Unknown outcomes keep the default. */
export const outcomeTone: Record<string, string> = {
  queued: 'text-emerald-300',
  approved_dry_run: 'text-cyan-300',
  no_trade: 'text-slate-400',
  held: 'text-sky-300',
  break_even: 'text-emerald-300',
  break_even_rejected: 'text-rose-300',
  close_queued: 'text-amber-300',
  close_rejected: 'text-rose-300',
  rejected: 'text-amber-300',
  unavailable: 'text-rose-300',
}

/** Command lifecycle status → text colour. */
export const commandTone: Record<CommandRecord['status'], string> = {
  pending: 'text-sky-400',
  completed: 'text-emerald-400',
  failed: 'text-rose-400',
}

/** One-line summary of an event payload for the activity list. */
export function payloadSummary(event: FeedEvent): string {
  const payload = event.payload ?? {}
  if (event.kind === 'proposal_evaluated') {
    const parts = [
      payload.outcome,
      payload.side,
      payload.volume,
      payload.ticket ? `#${payload.ticket}` : undefined,
      payload.reason,
    ].filter(Boolean)
    return parts.join(' · ')
  }
  if (event.kind === 'agent_tool_called') {
    const tool = String(payload.tool ?? 'tool')
    const result = payload.result as Record<string, unknown> | undefined
    const error = result?.error
    const decision = result?.decision
    if (typeof error === 'string') return `${tool} · ${error}`
    if (typeof decision === 'string') return `${tool} · ${decision}`
    return tool
  }
  if (event.kind === 'broker_snapshot') {
    return `orders=${payload.orders} lots=${payload.lots}`
  }
  if (event.kind === 'position_closed') {
    const profit = Number(payload.profit ?? 0)
    return `ticket ${payload.ticket} ${payload.symbol} ${payload.kind} · P/L ${profit >= 0 ? '+' : ''}${profit.toFixed(2)}`
  }
  return JSON.stringify(payload)
}

/// High-frequency plumbing the focus mode hides: snapshots and the read-only
/// commands the loop issues every cycle. Decisions, orders, and lifecycle
/// events always show.
export function isRoutine(event: FeedEvent): boolean {
  if (event.kind === 'broker_snapshot') return true
  if (event.kind === 'command_queued' || event.kind === 'command_completed') {
    const kind = String(event.payload?.kind ?? '')
    return kind === 'account_snapshot' || kind === 'rates' || kind === 'ping'
  }
  return false
}

/** Ordered keys shown first in the drill-down; everything else follows. */
const FIELD_ORDER = [
  'outcome',
  'rationale',
  'judgements',
  'tool',
  'result',
  'step',
  'agent_tools',
  'reason',
  'origin',
  'status',
  'symbol',
  'side',
  'kind',
  'volume',
  'lots',
  'ticket',
  'position_ticket',
  'price',
  'profit',
  'stop_loss',
  'take_profit',
  'intent_id',
  'command_id',
  'order_type',
]

export type DetailRow = { label: string; value: string }

/** Formats one payload value for display; nested values keep their JSON shape. */
function formatValue(value: unknown): string {
  if (value === null || value === undefined) return '—'
  if (typeof value === 'string') return value
  if (typeof value === 'number' || typeof value === 'boolean') return String(value)
  return JSON.stringify(value)
}

/** Stable, priority-ordered rows for a payload drill-down. */
export function detailRows(payload: Record<string, unknown>): DetailRow[] {
  const rank = (key: string) => {
    const index = FIELD_ORDER.indexOf(key)
    return index === -1 ? FIELD_ORDER.length : index
  }
  return Object.keys(payload)
    .sort((left, right) => rank(left) - rank(right) || left.localeCompare(right))
    .map((key) => ({ label: key.replaceAll('_', ' '), value: formatValue(payload[key]) }))
}
