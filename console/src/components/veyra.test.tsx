/**
 * Render tests for the tab panels.
 *
 * These exercise the operator-visible surfaces with the same payload shapes
 * the service emits: the risk policy and its editor, the account and session,
 * decision drill-downs, routine-event focus mode, the durable trail, command
 * inspection, diagnostics and live settings.
 */

import { useState } from 'react'

import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import type {
  Account,
  AuditPage,
  CommandRecord,
  FeedEvent,
  LogRecord,
  MarketSessions,
  ModelCooldown,
  Status,
} from '../lib/api'
import { VEYRA_MAGIC } from '../lib/api'
import { auditTimeMs } from '../lib/hooks'
import {
  AccountPanel,
  ActivityFeed,
  AutopilotPanel,
  CommandsPanel,
  LogsPanel,
  MetricsPanel,
  ModelRoutePanel,
  Pager,
  RiskPanel,
  SessionPanel,
  TracePanel,
} from './veyra'

afterEach(cleanup)

const autopilot: NonNullable<Status['autopilot']> = {
  enabled: true,
  interval_secs: 60,
  timeframe: 'H4',
  tier: 'balanced',
  bars: 48,
  symbol: 'EURUSD',
  symbols: ['EURUSD'],
  jev: 'auto',
  breakeven_r: 1,
  trail_r: 1,
  model_chain: [
    'deepseek/deepseek-v4.1-flash',
    'z-ai/glm-5.3-flash',
    'xiaomi/mimo-v2.6-flash',
    'z-ai/glm-4.7-flash',
  ],
}

const status: Status = {
  service: 'veyra',
  version: '0.1.0',
  environment: 'development',
  broker_provider: 'ea',
  market_provider: 'ea',
  model_provider: 'openrouter',
  jev_provider: 'typesafe',
  persistence: 'postgres',
  broker_connected: true,
  trading_enabled: true,
  ea_live_orders: true,
  autopilot,
  model_budget: { hourLimit: 120, hourCalls: 5, dayLimit: 2000, dayCalls: 5 },
  jev_usage: { calls: 12, failures: 0, inputTokens: 4800, outputTokens: 720 },
  decisions: { consecutiveFailures: 0, lastFailure: null, lastFailureAt: null },
  risk_policy: {
    killSwitch: false,
    allowTradingWithoutJev: false,
    symbols: ['EURUSD'],
    weekendSymbols: ['BTCUSD', 'ETHUSD'],
    maxVolumePerOrder: 0.01,
    maxTotalLots: 0.01,
    maxOpenOrders: 1,
    duplicateWindowSecs: 60,
    sessionUtc: null,
    maxRiskPercent: 12,
    maxDailyLossPercent: 10,
    maxPeakDrawdownPercent: 25,
    maxNetFactorLots: 0.01,
    calendarBlackoutMinutes: 30,
    minStopAtrFraction: 0.25,
    weekendPositions: 'agent',
  },
}

const account: Account = {
  fresh: true,
  connected: true,
  tradeAllowed: true,
  liveOrders: true,
  login: 123456,
  server: 'ICMarketsSC-MT4',
  symbol: 'EURUSD',
  ageSecs: 3,
  balance: 1000,
  equity: 1005.5,
  freeMargin: 900,
  marginLevel: 357.5,
  leverage: 100,
  orders: 1,
  lots: 0.01,
  positions: [
    {
      ticket: 10650805,
      symbol: 'EURUSD',
      kind: 'sell',
      lots: 0.01,
      price: 1.14757,
      profit: -0.32,
      sl: 1.1497,
      tp: 1.14554,
      swap: -0.11,
      magic: VEYRA_MAGIC,
    },
  ],
  positionsTruncated: false,
  serverTime: 1,
}

const commands: CommandRecord[] = [
  { id: 'cmd-1111-2222', kind: 'open_order', status: 'completed', summary: { ticket: 99 }, reason: null },
  { id: 'cmd-3333-4444', kind: 'close_order', status: 'failed', summary: null, reason: 'broker_timeout' },
]

const events: FeedEvent[] = [
  { seq: 1, at_ms: 1_700_000_000_000, kind: 'broker_snapshot', payload: { orders: 1, lots: 0.01 } },
  {
    seq: 2,
    at_ms: 1_700_000_001_000,
    kind: 'proposal_evaluated',
    payload: {
      outcome: 'held',
      rationale: 'Momentum favours the upside.',
      symbol: 'EURUSD',
      ticket: 10650805,
    },
  },
  {
    seq: 3,
    at_ms: 1_700_000_002_000,
    kind: 'position_closed',
    payload: { ticket: 7, symbol: 'EURUSD', kind: 'buy', profit: 12.5 },
  },
]

/** A promise the test resolves by hand, to observe in-flight states. */
function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((settle) => {
    resolve = settle
  })
  return { promise, resolve }
}

/** The value cell beside a label in a label/value grid. */
function valueOf(label: string): HTMLElement {
  const term = screen.getByText(label, { selector: 'dt' })
  return term.nextElementSibling as HTMLElement
}

describe('Pager', () => {
  it('hides itself for a single page', () => {
    const { container } = render(
      <Pager page={0} pages={1} start={0} count={3} total={3} onPrevious={vi.fn()} onNext={vi.fn()} />,
    )
    expect(container.textContent).toBe('')
  })

  it('states the visible range and guards both ends', () => {
    const onPrevious = vi.fn()
    const onNext = vi.fn()
    const { rerender } = render(
      <Pager page={0} pages={3} start={0} count={10} total={25} onPrevious={onPrevious} onNext={onNext} />,
    )
    expect(screen.getByText('1–10 of 25')).toBeTruthy()
    expect((screen.getByLabelText('Previous page') as HTMLButtonElement).disabled).toBe(true)
    fireEvent.click(screen.getByLabelText('Next page'))
    expect(onNext).toHaveBeenCalledOnce()

    rerender(<Pager page={2} pages={3} start={20} count={5} total={25} onPrevious={onPrevious} onNext={onNext} />)
    expect((screen.getByLabelText('Next page') as HTMLButtonElement).disabled).toBe(true)
    fireEvent.click(screen.getByLabelText('Previous page'))
    expect(onPrevious).toHaveBeenCalledOnce()
  })
})

describe('AccountPanel', () => {
  it('renders balances, exposure, and the connected server', () => {
    render(<AccountPanel account={account} />)
    expect(valueOf('Balance').textContent).toBe('1,000.00')
    expect(valueOf('Equity').textContent).toBe('1,005.50')
    expect(valueOf('Free margin').textContent).toBe('900.00')
    expect(valueOf('Margin level').textContent).toBe('357.5%')
    expect(valueOf('Leverage').textContent).toBe('1:100')
    expect(valueOf('Open orders').textContent).toBe('1')
    expect(valueOf('Open lots').textContent).toBe('0.01')
    expect(valueOf('Open P/L').textContent).toBe('−0.32')
    expect(valueOf('Open P/L').className).toContain('tone-bad')
    expect(valueOf('Server').textContent).toBe('ICMarketsSC-MT4')
    expect(valueOf('Login').textContent).toBe('123456')
    expect(screen.getByText('Updated 3s ago').className).toBe('')
  })

  it('holds skeletons before the first poll and dashes after a failed one', () => {
    const { container, rerender } = render(<AccountPanel />)
    expect(container.querySelectorAll('.skeleton').length).toBe(10)
    expect(screen.queryByText('Unavailable')).toBeNull()

    rerender(<AccountPanel error="account down" />)
    expect(screen.getByText('Unavailable').closest('[title]')?.getAttribute('title')).toBe('account down')
    expect(container.querySelectorAll('.skeleton').length).toBe(0)
    expect(valueOf('Balance').textContent).toBe('—')
  })

  it('marks a stale snapshot and dashes what the terminal did not report', () => {
    render(
      <AccountPanel
        account={{
          ...account,
          fresh: false,
          balance: undefined,
          equity: undefined,
          freeMargin: undefined,
          marginLevel: 0,
          leverage: 0,
          orders: undefined,
          lots: undefined,
          positions: [],
          server: undefined,
          login: undefined,
        }}
      />,
    )
    expect(screen.getByText('Updated 3s ago').className).toBe('tone-warn')
    for (const label of ['Balance', 'Equity', 'Margin level', 'Leverage', 'Open orders', 'Open P/L', 'Server', 'Login']) {
      expect(valueOf(label).textContent).toBe('—')
    }
  })

  it('treats a missing profit as zero and a missing book as flat', () => {
    const { rerender } = render(
      <AccountPanel account={{ ...account, positions: [{ ...account.positions![0], profit: undefined as unknown as number }] }} />,
    )
    expect(valueOf('Open P/L').textContent).toBe('0.00')
    rerender(<AccountPanel account={{ ...account, positions: undefined }} />)
    expect(valueOf('Open P/L').textContent).toBe('—')
  })
})

describe('SessionPanel', () => {
  const closedSessions: MarketSessions = {
    now: 1_789_776_000, // Saturday 2026-09-19 00:00 UTC
    market: { state: 'closed', nextEvent: 'opens', nextAt: 1_789_938_000 }, // Sunday 21:00 UTC
    entries: { open: false, blockedBy: 'weekend_open', detail: 'the market has not reopened for the week' },
    policy: {
      rolloverBlackout: { startMinute: 1245, endMinute: 1335 },
      fridayEntryCutoffMinute: 1140,
      sundayEntryOpenMinute: 1380,
    },
    weekend: { policy: 'agent', closesInSecs: null },
  }

  it('holds its rows while the first poll is in flight', () => {
    const { container } = render(<SessionPanel />)
    expect(screen.getByText('Next')).toBeTruthy()
    expect(container.querySelectorAll('.skeleton').length).toBe(5)
  })

  it('reports the closed week, the entry block, and what is held', () => {
    render(<SessionPanel sessions={closedSessions} account={account} />)
    expect(screen.getByText('Closed')).toBeTruthy()
    expect(valueOf('Opens').textContent).toBe('Sun 21:00 UTC · in 1d 21h')
    expect(valueOf('Entries').textContent).toBe('Blocked · weekend')
    expect(valueOf('Entries').className).toBe('tone-warn')
    expect(valueOf('Holding').textContent).toBe('EURUSD')
    expect(valueOf('Rollover').textContent).toBe('20:45–22:15 UTC')
    expect(valueOf('Entry window').textContent).toBe('Sun 23:00 – Fri 19:00 UTC')
    expect(screen.queryByText('Weekend close')).toBeNull()
  })

  it('counts down to the weekend checkpoint and names the preference', () => {
    const runUp: MarketSessions = {
      ...closedSessions,
      now: 1_789_761_600, // Friday 2026-09-18 20:00 UTC
      market: { state: 'open', nextEvent: 'closes', nextAt: 1_789_765_200 },
      entries: { open: false, blockedBy: 'weekend_approach', detail: 'the weekend entry cutoff has passed' },
      weekend: { policy: 'agent', closesInSecs: 3_600 },
    }
    const { rerender } = render(<SessionPanel sessions={runUp} account={account} />)
    expect(valueOf('Closes').textContent).toBe('Fri 21:00 UTC · in 1h 0m')
    expect(valueOf('Weekend close').textContent).toBe('Fri 21:00 UTC · in 1h 0m')
    expect(valueOf('Weekend positions').textContent).toBe('Analyst decides')
    expect(valueOf('Entries').textContent).toBe('Blocked · weekend cutoff')

    rerender(<SessionPanel sessions={{ ...runUp, weekend: { policy: 'flatten', closesInSecs: 1_800 } }} account={account} />)
    expect(valueOf('Weekend positions').textContent).toBe('Flattened before close')
  })

  it('reports an open market, a rollover pause, and a flat book', () => {
    const { rerender } = render(
      <SessionPanel
        sessions={{
          ...closedSessions,
          market: { state: 'open', nextEvent: 'pauses', nextAt: 1_789_776_000 + 3_600 },
          entries: { open: true, blockedBy: null, detail: null },
        }}
        account={{ ...account, positions: undefined }}
      />,
    )
    expect(screen.getAllByText('Open')).toHaveLength(2) // week state and entries
    expect(valueOf('Entries').className).toBe('tone-ok')
    expect(valueOf('Pauses').textContent).toBe('Sat 01:00 UTC · in 1h 0m')
    expect(valueOf('Holding').textContent).toBe('None')

    rerender(
      <SessionPanel
        sessions={{
          ...closedSessions,
          market: { state: 'rollover', nextEvent: 'resumes', nextAt: 1_789_776_000 + 600 },
          entries: { open: false, blockedBy: 'rollover_blackout', detail: 'the daily rollover blackout is active' },
        }}
      />,
    )
    expect(screen.getByText('Rollover', { selector: '.tab-state' })).toBeTruthy()
    expect(valueOf('Resumes').textContent).toBe('Sat 00:10 UTC · in 10m')
    expect(valueOf('Entries').textContent).toBe('Blocked · rollover blackout')
  })

  it('renders an unrecognised block reason verbatim and a missing one plainly', () => {
    const { rerender } = render(
      <SessionPanel
        sessions={{ ...closedSessions, entries: { open: false, blockedBy: 'broker_holiday', detail: 'holiday' } }}
      />,
    )
    expect(valueOf('Entries').textContent).toBe('Blocked · broker_holiday')
    rerender(<SessionPanel sessions={{ ...closedSessions, entries: { open: false, blockedBy: null, detail: null } }} />)
    expect(valueOf('Entries').textContent).toBe('Blocked')
  })
})

describe('AutopilotPanel', () => {
  it('renders cadence, stops, the chain, and budgets', () => {
    render(
      <AutopilotPanel status={autopilot} budget={status.model_budget} jevUsage={status.jev_usage} decisions={status.decisions} />,
    )
    expect(screen.getByText('Running')).toBeTruthy()
    expect(valueOf('Cadence').textContent).toBe('60 seconds')
    expect(valueOf('Timeframe').textContent).toBe('H4')
    expect(valueOf('Window').textContent).toBe('48 bars')
    expect(valueOf('Tier').textContent).toBe('balanced')
    expect(valueOf('Judgements').textContent).toBe('auto')
    expect(valueOf('Stops').textContent).toBe('Break-even 1R · trail 1R')
    expect(valueOf('Symbols').textContent).toBe('EURUSD')
    expect(valueOf('Profit harvest').textContent).toBe('Off')
    expect(valueOf('Model chain').textContent).toBe(
      'deepseek/deepseek-v4.1-flash → z-ai/glm-5.3-flash → xiaomi/mimo-v2.6-flash → z-ai/glm-4.7-flash',
    )
    expect(valueOf('Calls per hour').textContent).toBe('5 / 120')
    expect(valueOf('Calls per day').textContent).toBe('5 / 2,000')
    expect(valueOf('Judge calls').textContent).toBe('12')
    expect(valueOf('Judge tokens').textContent).toBe('5.5k')
    expect(valueOf('Last LLM').textContent).toBe('Not called yet')
    expect(valueOf('Last answer').textContent).toBe('None yet')
    expect(screen.queryByText('Last failure')).toBeNull()
  })

  it('reports small and large token counts and judge failures', () => {
    const { rerender } = render(
      <AutopilotPanel status={autopilot} jevUsage={{ calls: 3, failures: 2, inputTokens: 420, outputTokens: 80 }} />,
    )
    expect(valueOf('Judge calls').textContent).toBe('3 · 2 failed')
    expect(valueOf('Judge calls').className).toBe('tone-warn')
    expect(valueOf('Judge tokens').textContent).toBe('500')

    rerender(
      <AutopilotPanel status={autopilot} jevUsage={{ calls: 4429, failures: 0, inputTokens: 2_164_642, outputTokens: 314_388 }} />,
    )
    expect(valueOf('Judge calls').textContent).toBe('4,429')
    expect(valueOf('Judge tokens').textContent).toBe('2.5M')

    rerender(<AutopilotPanel status={autopilot} jevUsage={{ calls: 0, failures: 0, inputTokens: 0, outputTokens: 0 }} budget={null} />)
    expect(valueOf('Judge calls').textContent).toBe('—')
    expect(valueOf('Judge tokens').textContent).toBe('—')
    expect(valueOf('Calls per hour').textContent).toBe('—')
  })

  it('names the models last asked and last answering', () => {
    render(
      <AutopilotPanel
        status={autopilot}
        decisions={{ ...status.decisions!, lastModel: 'openai/gpt-5.6-mini', lastSuccessfulModel: 'openai/gpt-4.1-mini' }}
      />,
    )
    expect(valueOf('Last LLM').textContent).toBe('openai/gpt-5.6-mini')
    expect(valueOf('Last LLM').className).toBe('')
    expect(valueOf('Last answer').textContent).toBe('openai/gpt-4.1-mini')
  })

  it('surfaces a run of decision failures', () => {
    render(
      <AutopilotPanel status={autopilot} decisions={{ consecutiveFailures: 2, lastFailure: 'provider timeout', lastFailureAt: 1 }} />,
    )
    expect(valueOf('Last failure').textContent).toBe('2 in a row · provider timeout')
    expect(valueOf('Last failure').className).toBe('tone-bad')
  })

  it('renders rotation, stop, harvest and budget variants', () => {
    const { rerender } = render(
      <AutopilotPanel
        status={{
          ...autopilot,
          symbols: ['EURUSD', 'GBPUSD'],
          breakeven_r: 0,
          trail_r: 2,
          profit_harvest: {
            arm_r: 0.2,
            trail_r: 0.2,
            min_profit: 0.5,
            giveback_fraction: 0.35,
            min_hold_secs: 300,
            reentry_cooldown_secs: 900,
          },
        }}
        budget={{ hourLimit: 0, hourCalls: 1, dayLimit: 0, dayCalls: 1 }}
      />,
    )
    expect(valueOf('Symbols').textContent).toBe('EURUSD · GBPUSD')
    expect(valueOf('Stops').textContent).toBe('Trail 2R')
    expect(valueOf('Profit harvest').textContent).toBe('0.2R arm · 0.2R trail · 0.50 floor')
    expect(valueOf('Calls per hour').textContent).toBe('1 / ∞')
    expect(valueOf('Calls per day').textContent).toBe('1 / ∞')

    rerender(<AutopilotPanel status={{ ...autopilot, symbols: [], breakeven_r: 1, trail_r: 0 }} />)
    expect(valueOf('Symbols').textContent).toBe('Chart symbol')
    expect(valueOf('Stops').textContent).toBe('Break-even 1R')
  })

  it('renders the disabled shape and a chain without fallbacks', () => {
    const { rerender } = render(
      <AutopilotPanel status={{ ...autopilot, enabled: false, breakeven_r: 0, trail_r: 0, model_chain: [], model_fallbacks: ['backup/model'] }} />,
    )
    expect(screen.getByText('Off', { selector: '.tab-state' })).toBeTruthy()
    expect(valueOf('Cadence').textContent).toBe('—')
    expect(valueOf('Window').textContent).toBe('—')
    expect(valueOf('Stops').textContent).toBe('Bracket only')
    expect(valueOf('Model chain').textContent).toBe('Fallbacks: backup/model')

    rerender(<AutopilotPanel status={{ ...autopilot, model_chain: undefined, model_fallbacks: undefined }} />)
    expect(valueOf('Model chain').textContent).toBe('No fallbacks')
    expect(valueOf('Model chain').className).toBe('tone-warn')
  })

  it('distinguishes no autopilot from one that has not reported yet', () => {
    const { container, rerender } = render(<AutopilotPanel status={null} budget={null} jevUsage={null} decisions={null} />)
    expect(screen.getByText('Off', { selector: '.tab-state' })).toBeTruthy()
    for (const label of ['Cadence', 'Timeframe', 'Tier', 'Judgements', 'Symbols', 'Stops', 'Model chain']) {
      expect(valueOf(label).textContent).toBe('—')
    }
    expect(valueOf('Last LLM').textContent).toBe('Not called yet')

    rerender(<AutopilotPanel />)
    expect(screen.queryByText('Off', { selector: '.tab-state' })).toBeNull()
    expect(container.querySelectorAll('.skeleton').length).toBeGreaterThan(10)
  })
})

describe('ModelRoutePanel', () => {
  it('renders nothing for a service that does not report its route', () => {
    const { container } = render(<ModelRoutePanel status={{ ...status, model_route: undefined }} />)
    expect(container.textContent).toBe('')
  })

  it('states an empty route plainly, defaulting an unreported cooldown list to none', () => {
    render(<ModelRoutePanel status={{ ...status, model_route: [], model_cooldowns: undefined }} />)
    expect(screen.getByText('No model configured')).toBeTruthy()
  })

  it('numbers the route, marks the answering model, and benches a candidate with its reason and a future retry clock', () => {
    const now = Date.now()
    const route = ['z-ai/glm-5.3-flash', 'deepseek/deepseek-v4.1-flash', 'chatgpt:gpt-5.1-codex']
    const cooldowns: ModelCooldown[] = [
      { provider: 'openrouter', model: 'deepseek/deepseek-v4.1-flash', reason: 'rate_limited', untilMs: now + 5 * 60_000, failures: 2 },
    ]
    render(
      <ModelRoutePanel
        status={{
          ...status,
          model_route: route,
          model_cooldowns: cooldowns,
          decisions: { ...status.decisions!, lastSuccessfulModel: 'z-ai/glm-5.3-flash' },
        }}
      />,
    )
    const rows = within(screen.getByRole('list')).getAllByRole('listitem')
    expect(rows).toHaveLength(3)
    expect(within(rows[0]).getByText('1')).toBeTruthy()
    expect(within(rows[0]).getByText('z-ai/glm-5.3-flash')).toBeTruthy()
    expect(within(rows[0]).getByText('Answering')).toBeTruthy()
    expect(within(rows[1]).getByText('2')).toBeTruthy()
    expect(within(rows[1]).getByText('Rate limited')).toBeTruthy()
    expect(within(rows[1]).getByText(/^retry \d{2}:\d{2}$/)).toBeTruthy()
    expect(within(rows[1]).getByTitle('2 failed in a row')).toBeTruthy()
    expect(within(rows[1]).queryByText('Answering')).toBeNull()
    // The ChatGPT subscription's candidate reads by its `chatgpt:` label, not
    // the bare model id, and is not itself benched.
    expect(within(rows[2]).getByText('chatgpt:gpt-5.1-codex')).toBeTruthy()
    expect(within(rows[2]).queryByText('Answering')).toBeNull()
  })

  it('marks a due retry and falls back to sentence case for an unlisted reason', () => {
    const now = Date.now()
    const cooldowns: ModelCooldown[] = [
      { provider: 'openrouter', model: 'z-ai/glm-5.3-flash', reason: 'quota_exhausted_oddly', untilMs: now - 1_000, failures: 1 },
    ]
    render(<ModelRoutePanel status={{ ...status, model_route: ['z-ai/glm-5.3-flash'], model_cooldowns: cooldowns }} />)
    expect(screen.getByText('Quota exhausted oddly')).toBeTruthy()
    expect(screen.getByText('retry due')).toBeTruthy()
  })

  it('lists a cooldown outside the current route separately, unnumbered', () => {
    const cooldowns: ModelCooldown[] = [
      { provider: 'codex', model: 'gpt-5.1-codex', reason: 'overloaded', untilMs: Date.now() + 60_000, failures: 1 },
    ]
    render(<ModelRoutePanel status={{ ...status, model_route: ['z-ai/glm-5.3-flash'], model_cooldowns: cooldowns }} />)
    const rows = within(screen.getByRole('list')).getAllByRole('listitem')
    expect(rows).toHaveLength(2)
    expect(rows[0].textContent).toContain('z-ai/glm-5.3-flash')
    expect(within(rows[1]).getByText('chatgpt:gpt-5.1-codex')).toBeTruthy()
    expect(within(rows[1]).getByText('Overloaded')).toBeTruthy()
    expect(rows[1].querySelector('.tab-route-position')?.textContent).toBe('')
  })

  it('retries every benched model, disables the action meanwhile, and surfaces a failure', async () => {
    const pending = deferred<string | undefined>()
    const onRetryAll = vi.fn().mockReturnValue(pending.promise)
    const cooldowns: ModelCooldown[] = [
      { provider: 'openrouter', model: 'z-ai/glm-5.3-flash', reason: 'overloaded', untilMs: Date.now() + 60_000, failures: 1 },
    ]
    render(
      <ModelRoutePanel
        status={{ ...status, model_route: ['z-ai/glm-5.3-flash'], model_cooldowns: cooldowns }}
        onRetryAll={onRetryAll}
      />,
    )
    const retry = screen.getByRole('button', { name: 'Retry all' })
    fireEvent.click(retry)
    expect(onRetryAll).toHaveBeenCalledOnce()
    expect(screen.getByRole('button', { name: 'Retrying…' }).hasAttribute('disabled')).toBe(true)

    await act(async () => pending.resolve('Retry failed: service unavailable'))
    expect(screen.getByRole('alert').textContent).toBe('Retry failed: service unavailable')
    expect(screen.getByRole('button', { name: 'Retry all' })).toBeTruthy()
  })

  it('hides the retry action without a benched model or a handler', () => {
    const { rerender } = render(
      <ModelRoutePanel status={{ ...status, model_route: ['a'], model_cooldowns: [] }} onRetryAll={vi.fn()} />,
    )
    expect(screen.queryByRole('button', { name: /Retry/ })).toBeNull()

    rerender(
      <ModelRoutePanel
        status={{
          ...status,
          model_route: ['a'],
          model_cooldowns: [{ provider: 'openrouter', model: 'a', reason: 'overloaded', untilMs: Date.now(), failures: 1 }],
        }}
      />,
    )
    expect(screen.queryByRole('button', { name: /Retry/ })).toBeNull()
  })
})

describe('RiskPanel', () => {
  it('renders the active gate with both switches', () => {
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={vi.fn()} />)
    expect(screen.getByText('Gate active')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Edit' })).toBeTruthy()
    expect(valueOf('Symbols').textContent).toBe('EURUSD')
    expect(valueOf('Weekend markets').textContent).toBe('BTCUSD · ETHUSD')
    expect(valueOf('Session UTC').textContent).toBe('Always open')
    expect(valueOf('Weekend positions').textContent).toBe('Analyst decides')
    expect(valueOf('Max per order').textContent).toBe('0.01 lots')
    expect(valueOf('Max total').textContent).toBe('0.01 lots')
    expect(valueOf('Max open orders').textContent).toBe('1')
    expect(valueOf('Duplicate window').textContent).toBe('60s')
    expect(valueOf('Max risk').textContent).toBe('12% per trade')
    expect(valueOf('Loss brakes').textContent).toBe('Day 10% · peak 25%')
    expect(valueOf('Net USD cap').textContent).toBe('0.01 lots')
    expect(valueOf('News blackout').textContent).toBe('30m')
    expect(valueOf('Stop floor').textContent).toBe('0.25× ATR')
    expect(valueOf('Judge outage').textContent).toBe('Pauses decisions')
    expect(valueOf('Execution').textContent).toBe('Enabled')
    expect(valueOf('Execution').className).toBe('tone-warn')
    expect(valueOf('Terminal').textContent).toBe('Armed')
    expect(valueOf('Terminal').className).toBe('tone-ok')
  })

  it('shows disabled limits explicitly', () => {
    const { rerender } = render(
      <RiskPanel
        policy={{
          ...status.risk_policy,
          maxRiskPercent: 0,
          maxDailyLossPercent: 0,
          maxPeakDrawdownPercent: 0,
          maxNetFactorLots: 0,
          calendarBlackoutMinutes: 0,
          minStopAtrFraction: 0,
        }}
        status={status}
      />,
    )
    for (const label of ['Max risk', 'Loss brakes', 'Net USD cap', 'News blackout', 'Stop floor']) {
      expect(valueOf(label).textContent).toBe('Off')
    }
    rerender(<RiskPanel policy={{ ...status.risk_policy, maxDailyLossPercent: 0 }} status={status} />)
    expect(valueOf('Loss brakes').textContent).toBe('Peak 25%')
  })

  it('states whether a judge outage keeps trading or pauses decisions', () => {
    render(<RiskPanel policy={{ ...status.risk_policy, allowTradingWithoutJev: true }} status={status} />)
    expect(valueOf('Judge outage').textContent).toBe('Keeps trading')
    expect(valueOf('Judge outage').className).toBe('tone-warn')
  })

  it('names the kill switch, and holds its place before the first status', () => {
    const { container, rerender } = render(<RiskPanel policy={{ ...status.risk_policy, killSwitch: true }} status={status} />)
    expect(screen.getByText('Kill switch on')).toBeTruthy()
    expect(screen.queryByRole('button', { name: 'Edit' })).toBeNull()

    rerender(<RiskPanel />)
    expect(screen.queryByText('Gate active')).toBeNull()
    expect(container.querySelectorAll('.skeleton').length).toBe(16)
  })

  it('renders an empty allowlist, a session window, and disarmed switches', () => {
    render(
      <RiskPanel
        policy={{ ...status.risk_policy, symbols: [], weekendSymbols: undefined, sessionUtc: '07:00-21:00' }}
        status={{ ...status, trading_enabled: false, ea_live_orders: false }}
      />,
    )
    expect(valueOf('Symbols').textContent).toBe('None allowed')
    expect(valueOf('Weekend markets').textContent).toBe('None')
    expect(valueOf('Session UTC').textContent).toBe('07:00-21:00')
    expect(valueOf('Execution').textContent).toBe('Disabled')
    expect(valueOf('Terminal').textContent).toBe('Disarmed')
  })

  it('names the weekend preference in the read-only summary', () => {
    const { rerender } = render(<RiskPanel policy={{ ...status.risk_policy, weekendPositions: 'flatten' }} status={status} />)
    expect(valueOf('Weekend positions').textContent).toBe('Flattened before close')
    rerender(<RiskPanel policy={{ ...status.risk_policy, weekendPositions: 'hold' }} status={status} />)
    expect(valueOf('Weekend positions').textContent).toBe('Held through')
  })
})

describe('RiskPanel editing', () => {
  it('applies a patched policy and leaves edit mode', async () => {
    const pending = deferred<string | undefined>()
    const onApply = vi.fn().mockReturnValue(pending.promise)
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.change(screen.getByLabelText('Max open orders'), { target: { value: '7' } })
    fireEvent.change(screen.getByLabelText('Net USD cap (lots)'), { target: { value: '0.02' } })
    fireEvent.change(screen.getByLabelText('News blackout (minutes)'), { target: { value: '45' } })
    fireEvent.change(screen.getByLabelText('Min stop (× ATR)'), { target: { value: '0.75' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    // In flight: the save cannot be sent twice.
    const saving = screen.getByRole('button', { name: 'Saving…' }) as HTMLButtonElement
    expect(saving.disabled).toBe(true)

    await act(async () => pending.resolve(undefined))
    expect(screen.getByText('Gate active')).toBeTruthy()
    expect(onApply).toHaveBeenCalledTimes(1)
    const patch = onApply.mock.calls[0][0]
    expect(patch.maxOpenOrders).toBe(7)
    expect(patch.maxNetFactorLots).toBe(0.02)
    expect(patch.calendarBlackoutMinutes).toBe(45)
    expect(patch.minStopAtrFraction).toBe(0.75)
    expect(patch.symbols).toEqual(['EURUSD'])
    expect(patch.weekendSymbols).toEqual(['BTCUSD', 'ETHUSD'])
    expect(patch.killSwitch).toBe(false)
  })

  it('refuses non-numeric input without calling the service', () => {
    const onApply = vi.fn()
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.change(screen.getByLabelText('Max total (lots)'), { target: { value: 'lots' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    expect(onApply).not.toHaveBeenCalled()
    expect(screen.getByRole('alert').textContent).toBe('Max total (lots): must be a number')
  })

  it('rejects empty and fractional whole-number fields', () => {
    const onApply = vi.fn()
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.change(screen.getByLabelText('Max open orders'), { target: { value: '2.5' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    expect(screen.getByRole('alert').textContent).toBe('Max open orders: must be a whole number')

    fireEvent.change(screen.getByLabelText('Max open orders'), { target: { value: ' ' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    expect(screen.getByRole('alert').textContent).toBe('Max open orders: must be a number')
    expect(onApply).not.toHaveBeenCalled()
  })

  it('shows a service rejection and stays editable', async () => {
    const onApply = vi.fn().mockResolvedValue('maxOpenOrders: must be an integer from 0 through 1000')
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await screen.findByText(/must be an integer from 0 through 1000/)
    expect(screen.getByRole('button', { name: 'Save' })).toBeTruthy()
  })

  it('flips both switches in the draft and cancels cleanly', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    const killSwitch = screen.getByRole('switch', { name: 'Kill switch' })
    fireEvent.click(killSwitch)
    expect(killSwitch.getAttribute('aria-checked')).toBe('true')
    fireEvent.click(screen.getByRole('switch', { name: 'Judge bypass' }))
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    await screen.findByText('Gate active')
    expect(onApply.mock.calls[0][0].killSwitch).toBe(true)
    expect(onApply.mock.calls[0][0].allowTradingWithoutJev).toBe(true)

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }))
    expect(screen.getByRole('button', { name: 'Edit' })).toBeTruthy()
    expect(screen.queryByRole('button', { name: 'Save' })).toBeNull()
  })

  it('projects the session and symbol edits into the patch', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<RiskPanel policy={{ ...status.risk_policy, weekendSymbols: undefined }} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.change(screen.getByLabelText('Symbols'), { target: { value: 'eurusd, gbpusd' } })
    fireEvent.change(screen.getByLabelText('Weekend symbols'), { target: { value: ' btcusd ,' } })
    fireEvent.change(screen.getByLabelText('Session UTC'), { target: { value: '8-17' } })
    fireEvent.change(screen.getByLabelText('Weekend positions'), { target: { value: 'flatten' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))
    await screen.findByText('Gate active')

    const patch = onApply.mock.calls[0][0]
    expect(patch.symbols).toEqual(['eurusd', 'gbpusd'])
    expect(patch.weekendSymbols).toEqual(['btcusd'])
    expect(patch.sessionUtc).toBe('8-17')
    expect(patch.weekendPositions).toBe('flatten')
  })

  it('offers no edit affordance without a handler', () => {
    render(<RiskPanel policy={status.risk_policy} status={status} />)
    expect(screen.queryByRole('button', { name: 'Edit' })).toBeNull()
  })
})

describe('TracePanel', () => {
  const page: AuditPage = {
    status: 'ok',
    provider: 'postgres',
    events: [
      {
        id: '35769e04-3e73-4d7d-a175-6bac7a6e6295',
        at: '2026-09-18 17:47:46.844116+00',
        kind: 'agent_turn',
        payload: {
          outcome: 'agent_turn',
          step: 1,
          input: 'the exact prompt the model was shown',
          inputChars: 36,
          answer: { action: 'none' },
        },
      },
      {
        id: 'c3eff87c-2b31-4734-b79b-87e48b7b7923',
        at: '2026-09-18 17:47:45.100000+00',
        kind: 'failure',
        payload: { outcome: 'panic', detail: 'boom' },
      },
    ],
  }

  it('shows the whole payload of a turn rather than a summary', () => {
    render(<TracePanel page={page} kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('(2)')).toBeTruthy()

    // Scoped to the rows: the same kind also names a filter button above.
    const rows = screen.getByRole('list')
    fireEvent.click(within(rows).getByText('Agent turn'))
    // The prompt is readable in full: that is the point of the view.
    expect(screen.getByText('the exact prompt the model was shown')).toBeTruthy()
    expect(screen.getByText('inputChars')).toBeTruthy()
    expect(screen.getByText(/"action": "none"/)).toBeTruthy()
  })

  it('keeps long and multi-line values in their own block', () => {
    const long = 'x'.repeat(121)
    render(
      <TracePanel
        page={{
          status: 'ok',
          events: [
            {
              id: 'blocks-1',
              at: '2026-09-18 17:47:46+00',
              kind: 'agent_turn',
              payload: { long, lines: 'first\nsecond', flag: true, empty: null, count: 3 },
            },
          ],
        }}
        kind="all"
        onKindChange={() => {}}
      />,
    )
    fireEvent.click(within(screen.getByRole('list')).getByText('Agent turn'))
    expect(screen.getByText(long).tagName).toBe('PRE')
    expect(screen.getByText(/first\s+second/).tagName).toBe('PRE')
    expect(screen.getByText('true').tagName).toBe('DD')
    expect(screen.getByText('null').tagName).toBe('DD')
    expect(screen.getByText('3').tagName).toBe('DD')
  })

  it('filters by kind and states an empty trail plainly', () => {
    const onKindChange = vi.fn()
    const { rerender } = render(<TracePanel page={page} kind="failure" onKindChange={onKindChange} />)
    const rows = screen.getByRole('list')
    expect(within(rows).getByText('Panic')).toBeTruthy()
    expect(within(rows).queryByText('Agent turn')).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Agent turn' }))
    expect(onKindChange).toHaveBeenCalledWith('agent_turn')

    rerender(<TracePanel page={{ status: 'ok', events: [] }} kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('No events yet')).toBeTruthy()
    expect(screen.queryByRole('group', { name: 'Trail kind' })).toBeNull()
  })

  it('reads a Postgres timestamp rather than printing Invalid Date', () => {
    render(<TracePanel page={page} kind="all" onKindChange={() => {}} />)
    const rows = screen.getByRole('list')
    expect(within(rows).queryByText(/Invalid Date/)).toBeNull()
    // The offset form Postgres emits has to resolve to a real instant.
    expect(Number.isNaN(auditTimeMs('2026-09-18 17:47:46.844116+00'))).toBe(false)
    // Anything unreadable is absent, never a confident wrong time.
    expect(Number.isNaN(auditTimeMs('not a timestamp'))).toBe(true)
  })

  it('surfaces a disabled or unreachable trail instead of looking empty', () => {
    const { rerender } = render(<TracePanel page={{ status: 'disabled', events: [] }} kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('Disabled')).toBeTruthy()

    // Rows still on screen from before: the header qualifies them.
    rerender(
      <TracePanel page={{ ...page, status: 'unavailable', error: 'pool timed out' }} kind="all" onKindChange={() => {}} />,
    )
    const state = screen.getByText('Unavailable', { selector: '.tab-state' })
    expect(state.className).toContain('is-warn')
    expect(state.getAttribute('title')).toBe('pool timed out')

    rerender(<TracePanel page={page} error="audit down" kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('Unavailable', { selector: '.tab-state' }).className).toContain('is-bad')
  })

  it('holds skeleton rows while loading and names an outage without data', () => {
    const { container, rerender } = render(<TracePanel kind="all" onKindChange={() => {}} />)
    expect(container.querySelectorAll('.tab-skeleton-row').length).toBe(3)

    rerender(<TracePanel error="audit down" kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('Unavailable').getAttribute('title')).toBe('audit down')
  })

  it('collapses a row, and tolerates an unknown kind, no payload, and a bad time', () => {
    const rows = {
      status: 'ok' as const,
      events: [
        { id: 'mystery-1', at: 'not a timestamp', kind: 'mystery_event', payload: null },
        { id: 'plain-2', at: '2026-09-18 17:47:46.844116+00', kind: 'service_started', payload: {} },
      ],
    }
    render(<TracePanel page={rows} kind="all" onKindChange={vi.fn()} />)

    // An unreadable instant is a dash, never a confident wrong time.
    expect(screen.getByText('—')).toBeTruthy()
    // Payload-less rows still report themselves honestly.
    expect(screen.getAllByText('0 fields').length).toBe(2)

    const list = screen.getByRole('list')
    fireEvent.click(within(list).getByText('Mystery event'))
    expect(screen.queryAllByRole('button', { expanded: true }).length).toBe(1)
    fireEvent.click(within(list).getByText('Mystery event'))
    expect(screen.queryAllByRole('button', { expanded: true }).length).toBe(0)
  })

  it('pages a long trail', () => {
    const many: AuditPage = {
      status: 'ok',
      events: Array.from({ length: 14 }, (_, index) => ({
        id: `row-${index}`,
        at: '2026-09-18 17:47:46+00',
        kind: 'command_completed',
        payload: { kind: 'rates', symbol: 'EURUSD' },
      })),
    }
    render(<TracePanel page={many} kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('1–12 of 14')).toBeTruthy()
    expect(screen.getAllByText('Rates · EURUSD')).toHaveLength(12)
    fireEvent.click(screen.getByLabelText('Next page'))
    expect(screen.getByText('13–14 of 14')).toBeTruthy()
  })
})

describe('MetricsPanel', () => {
  const metrics = {
    service: 'veyra',
    version: '0.1.0',
    counters: { 'proposal.held': 9, 'command.completed': 1200 },
    feedLatest: 44,
  }

  it('sorts counters by count and reports the build and feed cursor', () => {
    render(<MetricsPanel metrics={metrics} status={status} />)
    const items = within(screen.getByRole('list')).getAllByRole('listitem')
    expect(items[0].textContent).toContain('command.completed')
    expect(items[0].textContent).toContain('1,200')
    expect(screen.getByText('proposal.held')).toBeTruthy()
    expect(valueOf('Version').textContent).toBe('0.1.0')
    expect(valueOf('Environment').textContent).toBe('development')
    expect(valueOf('Audit').textContent).toBe('postgres')
    expect(valueOf('Feed').textContent).toBe('#44')
  })

  it('holds skeletons before any poll lands', () => {
    const { container } = render(<MetricsPanel />)
    expect(container.querySelectorAll('.tab-skeleton-row').length).toBe(3)
    expect(container.querySelectorAll('dd .skeleton').length).toBe(4)
  })

  it('renders empty, audit-off, and error states', () => {
    const { rerender } = render(
      <MetricsPanel metrics={{ ...metrics, counters: {} }} status={{ ...status, persistence: null }} />,
    )
    expect(screen.getByText('No counters yet')).toBeTruthy()
    expect(valueOf('Audit').textContent).toBe('Off')

    rerender(<MetricsPanel error="metrics down" status={status} />)
    expect(screen.getByText('Unavailable').closest('[title]')?.getAttribute('title')).toBe('metrics down')
    expect(valueOf('Feed').textContent).toBe('—')
    expect(screen.queryByRole('list')).toBeNull()

    // A failed refresh keeps the last good counters without the outage label.
    rerender(<MetricsPanel metrics={metrics} error="metrics down" status={status} />)
    expect(screen.queryByText('Unavailable')).toBeNull()
  })

  it('orders equal counts alphabetically', () => {
    render(<MetricsPanel metrics={{ ...metrics, counters: { beta: 2, alpha: 2 } }} />)
    const items = within(screen.getByRole('list')).getAllByRole('listitem')
    expect(items[0].textContent).toContain('alpha')
  })
})

describe('CommandsPanel', () => {
  it('renders loading and empty states', () => {
    const { container, rerender } = render(<CommandsPanel />)
    expect(container.querySelectorAll('.tab-skeleton-row').length).toBe(3)
    rerender(<CommandsPanel commands={[]} />)
    expect(screen.getByText('No commands yet')).toBeTruthy()
  })

  it('expands a command into its full record', () => {
    render(<CommandsPanel commands={commands} />)
    expect(screen.getByText('(2)')).toBeTruthy()
    expect(screen.getByText('Open order')).toBeTruthy()
    expect(screen.getByText('{"ticket":99}')).toBeTruthy()
    expect(screen.getByText('broker_timeout')).toBeTruthy()
    expect(screen.getByText('Completed')).toBeTruthy()
    expect(screen.getByText('Failed')).toBeTruthy()

    const row = screen.getByText('Open order').closest('button') as HTMLButtonElement
    fireEvent.click(row)
    expect(row.getAttribute('aria-expanded')).toBe('true')
    expect(screen.getByText('cmd-1111-2222')).toBeTruthy()
    expect(screen.getByText(/"ticket": 99/)).toBeTruthy()

    fireEvent.click(row)
    expect(screen.queryByText('cmd-1111-2222')).toBeNull()
  })

  it('expands a failed command with no summary, and a pending one with neither', () => {
    render(
      <CommandsPanel
        commands={[commands[1], { id: 'cmd-5555-6666', kind: 'rates', status: 'pending', summary: null, reason: null }]}
      />,
    )
    expect(screen.getByText('Pending')).toBeTruthy()
    fireEvent.click(screen.getByText('Close order').closest('button') as HTMLButtonElement)
    expect(screen.getByText(/"summary": null/)).toBeTruthy()
    expect(screen.getByText(/"reason": "broker_timeout"/)).toBeTruthy()
  })

  it('pages a long command list', () => {
    const many = Array.from({ length: 13 }, (_, index) => ({ ...commands[0], id: `cmd-${index}` }))
    render(<CommandsPanel commands={many} />)
    expect(screen.getByText('1–12 of 13')).toBeTruthy()
    fireEvent.click(screen.getByLabelText('Next page'))
    expect(screen.getByText('13–13 of 13')).toBeTruthy()
  })
})

describe('ActivityFeed', () => {
  function Harness({
    initialFocus = true,
    connected = true,
    feed = events,
  }: {
    initialFocus?: boolean
    connected?: boolean
    feed?: FeedEvent[]
  }) {
    const [focus, setFocus] = useState(initialFocus)
    return <ActivityFeed events={feed} connected={connected} focus={focus} onFocusChange={setFocus} />
  }

  it('holds skeleton rows while the feed first connects', () => {
    const { container } = render(<ActivityFeed events={[]} connected={false} focus onFocusChange={vi.fn()} />)
    expect(screen.getByText('Connecting')).toBeTruthy()
    expect(container.querySelectorAll('.tab-skeleton-row').length).toBe(3)
  })

  it('states an empty stream and a reconnecting one', () => {
    const { rerender } = render(<ActivityFeed events={[]} connected focus onFocusChange={vi.fn()} />)
    expect(screen.getByText('Streaming')).toBeTruthy()
    expect(screen.getByText('No events yet')).toBeTruthy()

    rerender(<ActivityFeed events={events} connected={false} focus onFocusChange={vi.fn()} />)
    expect(screen.getByText('Reconnecting')).toBeTruthy()
    expect(screen.getByText('Trade held')).toBeTruthy()
  })

  it('hides routine plumbing in focus mode and restores it on toggle', () => {
    render(<Harness />)
    expect(screen.queryByText('Broker snapshot')).toBeNull()
    expect(screen.getByText('Trade held')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Focus' }).getAttribute('aria-pressed')).toBe('true')

    fireEvent.click(screen.getByRole('button', { name: 'All' }))
    expect(screen.getByText('Broker snapshot')).toBeTruthy()
    expect(screen.getByText('1 order · 0.01 lots')).toBeTruthy()

    fireEvent.click(screen.getByRole('button', { name: 'Focus' }))
    expect(screen.queryByText('Broker snapshot')).toBeNull()
  })

  it('reports an idle decision stream when everything is routine', () => {
    render(<ActivityFeed events={[events[0]]} connected focus onFocusChange={vi.fn()} />)
    expect(screen.getByText('No decisions yet')).toBeTruthy()
  })

  it('titles each event and colours its dot by what it means', () => {
    const { container } = render(<Harness initialFocus={false} />)
    expect(screen.getByText('Position closed')).toBeTruthy()
    expect(screen.getByText('EURUSD buy · +12.50')).toBeTruthy()
    expect(screen.getByText('EURUSD — Momentum favours the upside.')).toBeTruthy()
    // Held reviews are amber, closes green, and routine snapshots stay grey.
    const dots = Array.from(container.querySelectorAll('.tab-event .dot')).map((dot) => dot.className)
    expect(dots).toEqual(['dot is-idle', 'dot is-warn', 'dot is-ok'])
  })

  it('expands a decision into ordered detail and raw payload', () => {
    render(<Harness />)
    const row = screen.getByText('Trade held').closest('button') as HTMLButtonElement
    fireEvent.click(row)

    expect(row.getAttribute('aria-expanded')).toBe('true')
    expect(screen.getByText('outcome')).toBeTruthy()
    expect(screen.getByText('held')).toBeTruthy()
    expect(screen.getByText('rationale')).toBeTruthy()
    expect(screen.getByText('Momentum favours the upside.')).toBeTruthy()
    expect(screen.getByText('proposal_evaluated')).toBeTruthy()
    expect(screen.getByText('Raw payload')).toBeTruthy()
    expect(screen.getByText(/"outcome": "held"/)).toBeTruthy()

    fireEvent.click(row)
    expect(screen.queryByText(/"outcome": "held"/)).toBeNull()
  })

  it('digests model, command, snapshot and balance events without raw JSON where it can', () => {
    render(
      <Harness
        initialFocus={false}
        feed={[
          { seq: 20, at_ms: 1, kind: 'agent_turn', payload: { answer: { action: 'none', rationale: 'Nothing lines up.' } } },
          { seq: 21, at_ms: 2, kind: 'agent_tool_called', payload: { tool: 'check_risk', result: { decision: 'rejected' } } },
          { seq: 22, at_ms: 3, kind: 'command_completed', payload: { kind: 'open_order', result: { ticket: 1 } } },
          { seq: 23, at_ms: 4, kind: 'broker_snapshot', payload: { orders: 2 } },
          { seq: 24, at_ms: 5, kind: 'balance_observed', payload: { balance: 36.39 } },
          { seq: 25, at_ms: 6, kind: 'command_queued', payload: { command_id: 'abc' } },
          { seq: 26, at_ms: 7, kind: 'agent_turn', payload: { answer: null } },
        ]}
      />,
    )
    expect(screen.getByText('Nothing lines up.')).toBeTruthy()
    expect(screen.getByText('check_risk · rejected').className).toBe('tab-event-line')
    expect(screen.getByText('Open order')).toBeTruthy()
    expect(screen.getByText('2 orders · 0 lots')).toBeTruthy()
    expect(screen.getByText('Balance 36.39')).toBeTruthy()
    // What cannot be digested is shown as it is, in the mono face.
    expect(screen.getByText('{"command_id":"abc"}').className).toBe('tab-event-line mono')
    expect(screen.getByText('{"answer":null}')).toBeTruthy()
  })

  it('falls back for unknown kinds and marks nested values in the drill-down', () => {
    render(
      <ActivityFeed
        events={[{ seq: 9, at_ms: 1, kind: 'strategy_note', payload: { outcome: 'mystery', extra: { depth: 1 } } }]}
        connected
        focus
        onFocusChange={vi.fn()}
      />,
    )
    expect(screen.getByText('Strategy note')).toBeTruthy()
    fireEvent.click(screen.getByText('Strategy note').closest('button') as HTMLButtonElement)
    expect(screen.getByText('{"depth":1}').className).toBe('mono')
    expect(screen.getByText('mystery').className).toBe('')
  })

  it('renders an event without a payload', () => {
    render(
      <ActivityFeed
        events={[{ seq: 10, at_ms: Number.NaN, kind: 'orphan', payload: undefined as unknown as Record<string, unknown> }]}
        connected
        focus
        onFocusChange={vi.fn()}
      />,
    )
    const row = screen.getByText('Orphan').closest('button') as HTMLButtonElement
    expect(row.querySelector('.tab-event-line')).toBeNull()
    expect(row.querySelector('time')?.hasAttribute('datetime')).toBe(false)
    fireEvent.click(row)
    expect(screen.getByText('{}')).toBeTruthy()
  })

  it('pages a busy feed', () => {
    const busy = Array.from({ length: 15 }, (_, index) => ({ ...events[2], seq: 100 + index }))
    render(<Harness feed={busy} />)
    expect(screen.getByText('1–14 of 15')).toBeTruthy()
    fireEvent.click(screen.getByLabelText('Next page'))
    expect(screen.getByText('15–15 of 15')).toBeTruthy()
  })
})

describe('LogsPanel', () => {
  const records: LogRecord[] = [
    {
      seq: 1,
      atMs: 1_700_000_000_000,
      level: 'info',
      target: 'veyra_service::trading',
      message: 'autopilot tick',
      fields: { symbol: 'EURUSD' },
    },
    {
      seq: 2,
      atMs: 1_700_000_001_000,
      level: 'warn',
      target: 'veyra_service::broker',
      message: 'stale heartbeat',
      fields: {},
    },
  ]

  it('renders records newest first with levels and fields', () => {
    render(<LogsPanel logs={records} level="info" onLevelChange={vi.fn()} />)
    const items = within(screen.getByRole('list')).getAllByRole('listitem')
    expect(items[0].textContent).toContain('stale heartbeat')
    expect(items[0].querySelector('.tab-log-fields')).toBeNull()
    expect(screen.getByText('Warn', { selector: '.tab-level' }).className).toBe('tab-level is-warn')
    expect(screen.getByText(/"symbol":"EURUSD"/)).toBeTruthy()
  })

  it('reports filter changes', () => {
    const onLevelChange = vi.fn()
    render(<LogsPanel logs={records} level="info" onLevelChange={onLevelChange} />)
    expect(screen.getByRole('button', { name: 'Info' }).getAttribute('aria-pressed')).toBe('true')
    fireEvent.click(screen.getByRole('button', { name: 'Warn' }))
    expect(onLevelChange).toHaveBeenCalledWith('warn')
  })

  it('renders unknown levels in the neutral voice', () => {
    render(<LogsPanel logs={[{ ...records[0], seq: 9, level: 'notice', message: 'custom level' }]} level="info" onLevelChange={vi.fn()} />)
    expect(screen.getByText('custom level')).toBeTruthy()
    expect(screen.getByText('Notice').className).toBe('tab-level is-notice')
  })

  it('renders empty and error states', () => {
    const { rerender } = render(<LogsPanel logs={[]} level="info" onLevelChange={vi.fn()} />)
    expect(screen.getByText('No log lines yet')).toBeTruthy()
    rerender(<LogsPanel logs={[]} error="logs down" level="info" onLevelChange={vi.fn()} />)
    expect(screen.getByText('Unavailable').getAttribute('title')).toBe('logs down')
  })

  it('pages a long tail', () => {
    const many = Array.from({ length: 21 }, (_, index) => ({ ...records[0], seq: index }))
    render(<LogsPanel logs={many} level="info" onLevelChange={vi.fn()} />)
    expect(screen.getByText('1–20 of 21')).toBeTruthy()
  })
})
