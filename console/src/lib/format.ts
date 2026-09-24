/**
 * Pure formatting and classification helpers for the Veyra console.
 *
 * Audit events arrive as untyped JSON payloads. This module owns the mapping
 * from event kind to presentation (titles, summaries, drill-down rows) so the
 * components stay declarative and every rule is unit-testable without a DOM.
 */

import type { FeedEvent } from './api'

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
  if (event.kind === 'broker_snapshot' || event.kind === 'balance_observed') return true
  if (event.kind === 'command_queued' || event.kind === 'command_completed') {
    const kind = String(event.payload?.kind ?? '')
    return ['account_snapshot', 'rates', 'ping', 'symbol_spec', 'order_history'].includes(kind)
  }
  return false
}

/**
 * Events worth keeping in view after the plumbing scrolls past: decisions,
 * orders, closes and failures. Routine reads and the model's intermediate
 * turns are left to the full feed.
 */
export function isNotable(event: FeedEvent): boolean {
  if (isRoutine(event)) return false
  return event.kind !== 'agent_turn' && event.kind !== 'agent_tool_called'
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

/* ---------- money and ratios ---------- */

const GROUPED = new Map<number, Intl.NumberFormat>()

function grouped(digits: number): Intl.NumberFormat {
  let format = GROUPED.get(digits)
  if (!format) {
    format = new Intl.NumberFormat('en-US', {
      minimumFractionDigits: digits,
      maximumFractionDigits: digits,
    })
    GROUPED.set(digits, format)
  }
  return format
}

/**
 * An account-currency amount with thousands separators, e.g. `23,482.17`.
 *
 * No currency symbol: the venue does not report the account currency, so the
 * console states the number and nothing it cannot back. Absent or non-finite
 * values render as an em dash rather than as `NaN`.
 */
export function amount(value: number | null | undefined, digits = 2): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return grouped(digits).format(value)
}

/** A signed amount: `+317.60`, `−1.29`, `0.00`. Uses a true minus sign. */
export function signedAmount(value: number | null | undefined, digits = 2): string {
  if (value == null || !Number.isFinite(value)) return '—'
  const rounded = Number(value.toFixed(digits))
  if (rounded === 0) return grouped(digits).format(0)
  return `${rounded > 0 ? '+' : '−'}${grouped(digits).format(Math.abs(rounded))}`
}

/** A signed percentage: `+2.14%`, `−0.40%`. */
export function signedPercent(value: number | null | undefined, digits = 2): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return `${signedAmount(value, digits)}%`
}

/** An unsigned percentage: `79%`. */
export function percent(value: number | null | undefined, digits = 0): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return `${grouped(digits).format(value)}%`
}

/* ---------- activity digest ---------- */

/** Short human title for one feed event, e.g. `No trade`, `Position closed`. */
export function activityTitle(event: FeedEvent): string {
  if (event.kind === 'proposal_evaluated') {
    const outcome = typeof event.payload?.outcome === 'string' ? event.payload.outcome : ''
    const titles: Record<string, string> = {
      no_trade: 'No trade',
      held: 'Trade held',
      queued: 'Trade queued',
      approved_dry_run: 'Approved · dry run',
      rejected: 'Trade rejected',
      unavailable: 'Decision unavailable',
      break_even: 'Stop moved to break-even',
      break_even_rejected: 'Break-even rejected',
      close_queued: 'Close queued',
      close_rejected: 'Close rejected',
      trailing_stop: 'Position adjusted',
      profit_harvest_stop: 'Position adjusted',
      stop_queued: 'Position adjusted',
      stop_rejected: 'Stop adjustment rejected',
      profit_harvest_close: 'Profit harvested',
    }
    if (titles[outcome]) return titles[outcome]
  }
  if (event.kind === 'position_closed') return 'Position closed'
  if (event.kind === 'failure') return 'Decision failed'
  return event.kind
    .split('_')
    .map((word, index) => (index === 0 ? word.charAt(0).toUpperCase() + word.slice(1) : word))
    .join(' ')
}

/** The one-line explanation shown under an activity title, when there is one. */
export function activityDetail(event: FeedEvent): string | undefined {
  const payload = event.payload ?? {}
  if (event.kind === 'position_closed') {
    const profit = Number(payload.profit ?? 0)
    return `${payload.symbol ?? ''} ${payload.kind ?? ''} · ${signedAmount(profit)}`.trim()
  }
  if (event.kind === 'proposal_evaluated' || event.kind === 'failure' || event.kind === 'command_failed') {
    const detail = payload.reason ?? payload.rationale
    const symbol = typeof payload.symbol === 'string' ? payload.symbol : undefined
    if (typeof detail === 'string') return symbol && !detail.includes(symbol) ? `${symbol} — ${detail}` : detail
    return symbol
  }
  return undefined
}

/** State tone for an activity row's dot. */
export function activityTone(event: FeedEvent): 'ok' | 'warn' | 'bad' | 'idle' {
  const outcome = typeof event.payload?.outcome === 'string' ? event.payload.outcome : undefined
  if (event.kind === 'failure' || event.kind === 'command_failed') return 'bad'
  if (outcome?.endsWith('rejected') || outcome === 'unavailable') return 'bad'
  if (outcome === 'no_trade' || outcome === 'approved_dry_run') return 'idle'
  if (outcome === 'held' || outcome === 'close_queued') return 'warn'
  return 'ok'
}
