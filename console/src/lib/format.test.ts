/**
 * Unit tests for the console's pure formatting helpers. Payload shapes mirror
 * the audit events the service emits, so these tests double as a contract
 * check on the fields the drill-down promises to render.
 */

import { describe, expect, it } from 'vitest'

import type { FeedEvent } from './api'
import { commandTone, detailRows, isRoutine, payloadSummary } from './format'

function event(kind: string, payload: Record<string, unknown>): FeedEvent {
  return { seq: 1, at_ms: 1_700_000_000_000, kind, payload }
}

describe('payloadSummary', () => {
  it('joins proposal fields and skips the missing ones', () => {
    expect(
      payloadSummary(
        event('proposal_evaluated', { outcome: 'queued', side: 'sell', volume: 0.01, ticket: 42, reason: 'ok' }),
      ),
    ).toBe('queued · sell · 0.01 · #42 · ok')
    expect(payloadSummary(event('proposal_evaluated', { outcome: 'no_trade' }))).toBe('no_trade')
  })

  it('summarises broker snapshots', () => {
    expect(payloadSummary(event('broker_snapshot', { orders: 1, lots: 0.01 }))).toBe('orders=1 lots=0.01')
  })

  it('formats closed-position profit with a sign', () => {
    expect(payloadSummary(event('position_closed', { ticket: 7, symbol: 'EURUSD', kind: 'buy', profit: 12.5 }))).toBe(
      'ticket 7 EURUSD buy · P/L +12.50',
    )
    expect(payloadSummary(event('position_closed', { ticket: 7, symbol: 'EURUSD', kind: 'sell', profit: -3 }))).toBe(
      'ticket 7 EURUSD sell · P/L -3.00',
    )
    expect(payloadSummary(event('position_closed', { ticket: 7, symbol: 'EURUSD', kind: 'sell' }))).toBe(
      'ticket 7 EURUSD sell · P/L +0.00',
    )
  })

  it('summarises agent tool calls with their outcome', () => {
    expect(
      payloadSummary(
        event('agent_tool_called', { tool: 'get_judgements', result: { direction: 'long' } }),
      ),
    ).toBe('get_judgements')
    expect(
      payloadSummary(event('agent_tool_called', { tool: 'get_market', result: { error: 'outside allowlist' } })),
    ).toBe('get_market · outside allowlist')
    expect(
      payloadSummary(event('agent_tool_called', { tool: 'check_risk', result: { decision: 'rejected' } })),
    ).toBe('check_risk · rejected')
    expect(payloadSummary(event('agent_tool_called', {}))).toBe('tool')
  })

  it('falls back to the raw payload JSON', () => {
    expect(payloadSummary(event('service_started', { version: '0.1.0' }))).toBe('{"version":"0.1.0"}')
  })

  it('treats a missing payload as empty', () => {
    expect(
      payloadSummary({ seq: 1, at_ms: 1, kind: 'orphan', payload: undefined as unknown as Record<string, unknown> }),
    ).toBe('{}')
  })
})

describe('isRoutine', () => {
  it('hides snapshots and read-only commands', () => {
    expect(isRoutine(event('broker_snapshot', {}))).toBe(true)
    expect(isRoutine(event('command_queued', { kind: 'account_snapshot' }))).toBe(true)
    expect(isRoutine(event('command_completed', { kind: 'rates' }))).toBe(true)
    expect(isRoutine(event('command_completed', { kind: 'ping' }))).toBe(true)
  })

  it('keeps decisions, orders, and lifecycle events visible', () => {
    expect(isRoutine(event('proposal_evaluated', { outcome: 'held' }))).toBe(false)
    expect(isRoutine(event('command_queued', { kind: 'open_order' }))).toBe(false)
    expect(isRoutine(event('command_failed', { kind: 'account_snapshot' }))).toBe(false)
    expect(isRoutine(event('position_closed', {}))).toBe(false)
    expect(isRoutine(event('command_queued', {}))).toBe(false)
  })
})

describe('detailRows', () => {
  it('orders known keys first and formats values', () => {
    const rows = detailRows({
      zeta: 'last',
      outcome: 'held',
      result: { lots: 1 },
      stop_loss: 1.085,
      empty: null,
      note: undefined,
      enabled: true,
    })
    expect(rows.map((row) => row.label)).toEqual([
      'outcome',
      'result',
      'stop loss',
      'empty',
      'enabled',
      'note',
      'zeta',
    ])
    expect(rows.map((row) => row.value)).toEqual(['held', '{"lots":1}', '1.085', '—', 'true', '—', 'last'])
  })

  it('shows the rationale directly after the outcome', () => {
    const rows = detailRows({
      symbol: 'EURUSD',
      rationale: 'Momentum favours the upside.',
      outcome: 'queued',
    })
    expect(rows.map((row) => row.label)).toEqual(['outcome', 'rationale', 'symbol'])
  })

  it('returns no rows for an empty payload', () => {
    expect(detailRows({})).toEqual([])
  })
})

describe('commandTone', () => {
  it('maps every lifecycle status to a colour', () => {
    expect(commandTone.pending).toBe('text-sky-400')
    expect(commandTone.completed).toBe('text-emerald-400')
    expect(commandTone.failed).toBe('text-rose-400')
  })
})
