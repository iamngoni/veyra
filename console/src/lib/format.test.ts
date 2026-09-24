/**
 * Unit tests for the console's pure formatting helpers. Payload shapes mirror
 * the audit events the service emits, so these tests double as a contract
 * check on the fields the drill-down promises to render.
 */

import { describe, expect, it } from 'vitest'

import type { FeedEvent } from './api'
import {
  activityDetail,
  activityTitle,
  activityTone,
  amount,
  detailRows,
  isNotable,
  isRoutine,
  payloadSummary,
  percent,
  signedAmount,
  signedPercent,
} from './format'

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

describe('isNotable', () => {
  it('keeps decisions, orders and failures but not plumbing or model turns', () => {
    expect(isNotable(event('proposal_evaluated', { outcome: 'no_trade' }))).toBe(true)
    expect(isNotable(event('command_queued', { kind: 'open_order' }))).toBe(true)
    expect(isNotable(event('failure', {}))).toBe(true)
    expect(isNotable(event('broker_snapshot', {}))).toBe(false)
    expect(isNotable(event('command_completed', { kind: 'rates' }))).toBe(false)
    expect(isNotable(event('agent_turn', {}))).toBe(false)
    expect(isNotable(event('agent_tool_called', {}))).toBe(false)
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

  it('hides balance samples and read-only broker requests in focus mode', () => {
    expect(isRoutine(event('balance_observed', { balance: 36.39 }))).toBe(true)
    expect(isRoutine(event('command_completed', { kind: 'symbol_spec' }))).toBe(true)
    expect(isRoutine(event('command_completed', { kind: 'order_history' }))).toBe(true)
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

describe('amount', () => {
  it('groups thousands at a fixed precision', () => {
    expect(amount(23482.17)).toBe('23,482.17')
    expect(amount(0)).toBe('0.00')
    expect(amount(-1234.5)).toBe('-1,234.50')
    expect(amount(1200, 0)).toBe('1,200')
    // The cached formatter for a precision is reused, not rebuilt.
    expect(amount(1200, 0)).toBe('1,200')
  })

  it('states an absent or non-finite value as a dash', () => {
    expect(amount(undefined)).toBe('—')
    expect(amount(null)).toBe('—')
    expect(amount(Number.NaN)).toBe('—')
    expect(amount(Number.POSITIVE_INFINITY)).toBe('—')
  })
})

describe('signedAmount', () => {
  it('signs gains and losses with a true minus', () => {
    expect(signedAmount(317.6)).toBe('+317.60')
    expect(signedAmount(-1.29)).toBe('−1.29')
    expect(signedAmount(1842.3)).toBe('+1,842.30')
    expect(signedAmount(0.5, 0)).toBe('+1')
  })

  it('leaves zero, and anything that rounds to it, unsigned', () => {
    expect(signedAmount(0)).toBe('0.00')
    expect(signedAmount(-0.001)).toBe('0.00')
    expect(signedAmount(0.004)).toBe('0.00')
  })

  it('states an absent or non-finite value as a dash', () => {
    expect(signedAmount(undefined)).toBe('—')
    expect(signedAmount(null)).toBe('—')
    expect(signedAmount(Number.NaN)).toBe('—')
  })
})

describe('signedPercent and percent', () => {
  it('formats signed and unsigned percentages', () => {
    expect(signedPercent(2.14)).toBe('+2.14%')
    expect(signedPercent(-0.4)).toBe('−0.40%')
    expect(signedPercent(0)).toBe('0.00%')
    expect(percent(79)).toBe('79%')
    expect(percent(357.5, 1)).toBe('357.5%')
  })

  it('states an absent or non-finite value as a dash', () => {
    expect(signedPercent(undefined)).toBe('—')
    expect(signedPercent(Number.NaN)).toBe('—')
    expect(percent(null)).toBe('—')
    expect(percent(Number.NEGATIVE_INFINITY)).toBe('—')
  })
})

describe('activityTitle', () => {
  it('names every decision outcome in plain words', () => {
    const titles: Array<[string, string]> = [
      ['no_trade', 'No trade'],
      ['held', 'Trade held'],
      ['queued', 'Trade queued'],
      ['approved_dry_run', 'Approved · dry run'],
      ['rejected', 'Trade rejected'],
      ['unavailable', 'Decision unavailable'],
      ['break_even', 'Stop moved to break-even'],
      ['break_even_rejected', 'Break-even rejected'],
      ['close_queued', 'Close queued'],
      ['close_rejected', 'Close rejected'],
      ['trailing_stop', 'Position adjusted'],
      ['profit_harvest_stop', 'Position adjusted'],
      ['stop_queued', 'Position adjusted'],
      ['stop_rejected', 'Stop adjustment rejected'],
      ['profit_harvest_close', 'Profit harvested'],
    ]
    for (const [outcome, title] of titles) {
      expect(activityTitle(event('proposal_evaluated', { outcome }))).toBe(title)
    }
  })

  it('names closes and failures, and falls back to the kind in sentence case', () => {
    expect(activityTitle(event('position_closed', {}))).toBe('Position closed')
    expect(activityTitle(event('failure', {}))).toBe('Decision failed')
    expect(activityTitle(event('agent_tool_called', {}))).toBe('Agent tool called')
    // An unknown or malformed outcome never invents a title.
    expect(activityTitle(event('proposal_evaluated', { outcome: 'mystery' }))).toBe('Proposal evaluated')
    expect(activityTitle(event('proposal_evaluated', { outcome: 7 }))).toBe('Proposal evaluated')
    expect(activityTitle({ seq: 1, at_ms: 1, kind: 'proposal_evaluated', payload: undefined as unknown as Record<string, unknown> })).toBe(
      'Proposal evaluated',
    )
  })
})

describe('activityDetail', () => {
  it('summarises a close with its signed result', () => {
    expect(activityDetail(event('position_closed', { symbol: 'EURUSD', kind: 'buy', profit: 12.5 }))).toBe(
      'EURUSD buy · +12.50',
    )
    expect(activityDetail(event('position_closed', { symbol: 'EURUSD', kind: 'sell', profit: -2.5 }))).toBe(
      'EURUSD sell · −2.50',
    )
    // Missing fields are left out rather than printed as "undefined".
    expect(activityDetail(event('position_closed', {}))).toBe('· 0.00')
  })

  it('gives the reason for a decision, naming the symbol once', () => {
    expect(activityDetail(event('proposal_evaluated', { reason: 'spread too wide', symbol: 'EURUSD' }))).toBe(
      'EURUSD — spread too wide',
    )
    expect(activityDetail(event('proposal_evaluated', { reason: 'EURUSD spread too wide', symbol: 'EURUSD' }))).toBe(
      'EURUSD spread too wide',
    )
    expect(activityDetail(event('proposal_evaluated', { rationale: 'Momentum favours the upside.' }))).toBe(
      'Momentum favours the upside.',
    )
    expect(activityDetail(event('proposal_evaluated', { symbol: 'GBPUSD' }))).toBe('GBPUSD')
    expect(activityDetail(event('proposal_evaluated', { reason: 42 }))).toBeUndefined()
    expect(activityDetail(event('failure', { reason: 'provider timeout' }))).toBe('provider timeout')
    expect(activityDetail(event('command_failed', { reason: 'broker rejected order' }))).toBe('broker rejected order')
  })

  it('has nothing to add for other kinds or a missing payload', () => {
    expect(activityDetail(event('broker_snapshot', { orders: 1 }))).toBeUndefined()
    expect(
      activityDetail({ seq: 1, at_ms: 1, kind: 'failure', payload: undefined as unknown as Record<string, unknown> }),
    ).toBeUndefined()
  })
})

describe('activityTone', () => {
  it('maps failures, refusals, quiet outcomes and pending work to a state', () => {
    expect(activityTone(event('failure', {}))).toBe('bad')
    expect(activityTone(event('command_failed', {}))).toBe('bad')
    expect(activityTone(event('proposal_evaluated', { outcome: 'close_rejected' }))).toBe('bad')
    expect(activityTone(event('proposal_evaluated', { outcome: 'unavailable' }))).toBe('bad')
    expect(activityTone(event('proposal_evaluated', { outcome: 'no_trade' }))).toBe('idle')
    expect(activityTone(event('proposal_evaluated', { outcome: 'approved_dry_run' }))).toBe('idle')
    expect(activityTone(event('proposal_evaluated', { outcome: 'held' }))).toBe('warn')
    expect(activityTone(event('proposal_evaluated', { outcome: 'close_queued' }))).toBe('warn')
    expect(activityTone(event('proposal_evaluated', { outcome: 'queued' }))).toBe('ok')
    expect(activityTone(event('position_closed', {}))).toBe('ok')
    expect(activityTone(event('proposal_evaluated', { outcome: 3 }))).toBe('ok')
  })
})
