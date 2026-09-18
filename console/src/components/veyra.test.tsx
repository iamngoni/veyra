/**
 * Render tests for the console panels.
 *
 * These exercise the operator-visible surfaces with the same payload shapes
 * the service emits: armed/disarmed pills, the open-position table, decision
 * drill-downs, routine-event focus mode, and command inspection.
 */

import { useState } from 'react'

import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import type { Account, CandleSeries, CommandRecord, FeedEvent, LogRecord, Status } from '../lib/api'
import { VEYRA_MAGIC } from '../lib/api'
import {
  AccountPanel,
  ActivityFeed,
  AutopilotPanel,
  CommandsPanel,
  LogsPanel,
  MarketPanel,
  MetricsPanel,
  Panel,
  Pill,
  PositionsPanel,
  RiskPanel,
  StatusPills,
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
  risk_policy: {
    killSwitch: false,
    symbols: ['EURUSD'],
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
      magic: VEYRA_MAGIC,
    },
  ],
  positionsTruncated: false,
  serverTime: 1,
}

const series: CandleSeries = {
  symbol: 'EURUSD',
  timeframe: 'H4',
  candles: [
    { time: 1, open: 1.1, high: 1.115, low: 1.095, close: 1.1, volume: 10 },
    { time: 2, open: 1.1, high: 1.112, low: 1.098, close: 1.11, volume: 12 },
    { time: 3, open: 1.11, high: 1.113, low: 1.1, close: 1.105, volume: 11 },
  ],
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

describe('Pill and StatusPills', () => {
  it('renders a connecting state without data', () => {
    render(<StatusPills />)
    expect(screen.getByText('connecting…')).toBeTruthy()
  })

  it('renders armed switches and autopilot cadence', () => {
    render(<StatusPills status={status} />)
    expect(screen.getByText('armed')).toBeTruthy()
    expect(screen.getByText('enabled')).toBeTruthy()
    expect(screen.getByText('60s · H4')).toBeTruthy()
    expect(screen.getByText('postgres')).toBeTruthy()
    expect(screen.getByText('development')).toBeTruthy()
  })

  it('renders stale and disarmed variants', () => {
    render(
      <StatusPills
        status={{ ...status, broker_connected: false, ea_live_orders: false, autopilot: { ...autopilot, enabled: false } }}
      />,
    )
    expect(screen.getByText('stale')).toBeTruthy()
    expect(screen.getByText('disarmed')).toBeTruthy()
    expect(screen.getByText('off')).toBeTruthy()
  })

  it('renders arbitrary pill tones', () => {
    render(<Pill tone="warn" label="risk" value="widened" />)
    expect(screen.getByText('widened')).toBeTruthy()
  })
})

describe('AccountPanel', () => {
  it('renders balances, exposure, and the connected server', () => {
    render(<AccountPanel account={account} />)
    expect(screen.getByText('1000.00')).toBeTruthy()
    expect(screen.getByText('1005.50')).toBeTruthy()
    expect(screen.getByText('900.00')).toBeTruthy()
    expect(screen.getByText('357.5%')).toBeTruthy()
    expect(screen.getByText('1:100')).toBeTruthy()
    expect(screen.getByText('0.01')).toBeTruthy()
    expect(screen.getByText('-0.32')).toBeTruthy()
    expect(screen.getByText(/ICMarketsSC-MT4 · #123456/)).toBeTruthy()
  })

  it('shows the waiting and error states', () => {
    const { rerender } = render(<AccountPanel />)
    expect(screen.getByText('waiting…')).toBeTruthy()
    rerender(<AccountPanel error="account down" />)
    expect(screen.getByText('account down')).toBeTruthy()
  })
})

describe('PositionsPanel', () => {
  it('renders an empty account cleanly', () => {
    render(<PositionsPanel account={{ ...account, positions: [] }} />)
    expect(screen.getByText('Flat — no open orders.')).toBeTruthy()
  })

  it('labels veyra-owned and manual positions', () => {
    render(
      <PositionsPanel
        account={{
          ...account,
          positionsTruncated: true,
          positions: [
            account.positions?.[0] ?? {
              ticket: 1,
              symbol: 'EURUSD',
              kind: 'sell',
              lots: 0.01,
              price: 1,
              profit: 0,
              sl: 0,
              tp: 0,
              magic: 0,
            },
            { ticket: 42, symbol: 'EURUSD', kind: 'buy', lots: 0.1, price: 1.15, profit: 4, sl: 0, tp: 0, magic: 0 },
          ],
        }}
      />,
    )
    expect(screen.getByText('10650805')).toBeTruthy()
    expect(screen.getByText('veyra')).toBeTruthy()
    expect(screen.getByText('manual')).toBeTruthy()
    expect(screen.getByText('truncated')).toBeTruthy()
    expect(screen.getByText('+4.00')).toBeTruthy()
  })
})

describe('MarketPanel', () => {
  it('renders the series summary', () => {
    render(<MarketPanel series={series} />)
    expect(screen.getByText('EURUSD H4')).toBeTruthy()
    expect(screen.getByText('1.10500')).toBeTruthy()
    expect(screen.getByText('+0.45%')).toBeTruthy()
    expect(screen.getByText('1.11300 / 1.10000')).toBeTruthy()
  })

  it('handles a one-candle series and an error', () => {
    const { rerender } = render(<MarketPanel series={{ ...series, candles: [series.candles[0]] }} />)
    expect(screen.getByText('+0.00%')).toBeTruthy()
    rerender(<MarketPanel error="feed down" />)
    expect(screen.getByText('feed down')).toBeTruthy()
  })
})

describe('AutopilotPanel', () => {
  it('renders cadence, stops, and budget', () => {
    render(
      <AutopilotPanel status={autopilot} budget={status.model_budget} jevUsage={status.jev_usage} />,
    )
    expect(screen.getByText('60s')).toBeTruthy()
    expect(screen.getByText('48 bars')).toBeTruthy()
    expect(screen.getByText('BE 1R · trail 1R')).toBeTruthy()
    expect(screen.getByText('5/120 h · 5/2000 d')).toBeTruthy()
    expect(screen.getByText('EURUSD')).toBeTruthy()
    expect(screen.getByText('12 calls · 5.5k tok')).toBeTruthy()
  })

  it('renders a multi-symbol rotation and the chart-symbol fallback', () => {
    const { rerender } = render(
      <AutopilotPanel status={{ ...autopilot, symbols: ['EURUSD', 'GBPUSD'] }} budget={status.model_budget} />,
    )
    expect(screen.getByText('EURUSD · GBPUSD')).toBeTruthy()
    rerender(<AutopilotPanel status={{ ...autopilot, symbols: [] }} />)
    expect(screen.getByText('chart symbol')).toBeTruthy()
  })

  it('renders the disabled shape', () => {
    render(<AutopilotPanel status={{ ...autopilot, enabled: false, breakeven_r: 0, trail_r: 0 }} />)
    expect(screen.getByText('disabled')).toBeTruthy()
    expect(screen.getByText('bracket only')).toBeTruthy()
    expect(screen.getAllByText('—').length).toBeGreaterThan(0)
  })
})

describe('RiskPanel', () => {
  it('renders the active gate with both switches', () => {
    render(<RiskPanel policy={status.risk_policy} status={status} />)
    expect(screen.getByText('EURUSD')).toBeTruthy()
    expect(screen.getByText('switch on')).toBeTruthy()
    expect(screen.getByText('armed')).toBeTruthy()
    expect(screen.getByText('always open')).toBeTruthy()
    expect(screen.getByText('30m blackout')).toBeTruthy()
    expect(screen.getByText('0.25× ATR')).toBeTruthy()
  })

  it('shows disabled valuation limits explicitly', () => {
    render(
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
    expect(screen.getAllByText('off').length).toBeGreaterThanOrEqual(3)
  })

  it('renders the kill switch and missing policy', () => {
    const { rerender } = render(<RiskPanel policy={{ ...status.risk_policy, killSwitch: true }} status={status} />)
    expect(screen.getByText('kill switch on')).toBeTruthy()
    rerender(<RiskPanel />)
    expect(screen.getAllByText('—').length).toBeGreaterThan(0)
  })
})

describe('RiskPanel editing', () => {
  it('applies a patched policy and leaves edit mode', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByText('edit'))
    fireEvent.change(screen.getByLabelText('Max open orders'), { target: { value: '7' } })
    fireEvent.change(screen.getByLabelText('Net USD cap (lots)'), { target: { value: '0.02' } })
    fireEvent.change(screen.getByLabelText('News blackout (minutes)'), { target: { value: '45' } })
    fireEvent.change(screen.getByLabelText('Min stop (× ATR)'), { target: { value: '0.75' } })
    fireEvent.click(screen.getByText('save'))

    await screen.findByText('gate active')
    expect(onApply).toHaveBeenCalledTimes(1)
    const patch = onApply.mock.calls[0][0]
    expect(patch.maxOpenOrders).toBe(7)
    expect(patch.maxNetFactorLots).toBe(0.02)
    expect(patch.calendarBlackoutMinutes).toBe(45)
    expect(patch.minStopAtrFraction).toBe(0.75)
    expect(patch.symbols).toEqual(['EURUSD'])
    expect(patch.killSwitch).toBe(false)
  })

  it('refuses non-numeric input without calling the service', () => {
    const onApply = vi.fn()
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByText('edit'))
    fireEvent.change(screen.getByLabelText('Max total (lots)'), { target: { value: 'lots' } })
    fireEvent.click(screen.getByText('save'))

    expect(onApply).not.toHaveBeenCalled()
    expect(screen.getByText(/Max total \(lots\): must be a number/)).toBeTruthy()
  })

  it('rejects fractional whole-number fields', () => {
    const onApply = vi.fn()
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByText('edit'))
    fireEvent.change(screen.getByLabelText('Max open orders'), { target: { value: '2.5' } })
    fireEvent.click(screen.getByText('save'))

    expect(onApply).not.toHaveBeenCalled()
    expect(screen.getByText(/Max open orders: must be a whole number/)).toBeTruthy()
  })

  it('shows a service rejection and stays editable', async () => {
    const onApply = vi.fn().mockResolvedValue('maxOpenOrders: must be an integer from 0 through 1000')
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByText('edit'))
    fireEvent.click(screen.getByText('save'))

    await screen.findByText(/must be an integer from 0 through 1000/)
    expect(screen.getByText('save')).toBeTruthy()
  })

  it('applies a kill-switch toggle and cancels cleanly', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByText('edit'))
    fireEvent.click(screen.getByLabelText(/Kill switch/))
    fireEvent.click(screen.getByText('save'))
    await screen.findByText('gate active')
    expect(onApply.mock.calls[0][0].killSwitch).toBe(true)

    fireEvent.click(screen.getByText('edit'))
    fireEvent.click(screen.getByText('cancel'))
    expect(screen.getByText('edit')).toBeTruthy()
    expect(screen.queryByText('save')).toBeNull()
  })

  it('projects the session and symbol edits into the patch', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<RiskPanel policy={status.risk_policy} status={status} onApply={onApply} />)

    fireEvent.click(screen.getByText('edit'))
    fireEvent.change(screen.getByLabelText(/^Symbols/), { target: { value: 'eurusd, gbpusd' } })
    fireEvent.change(screen.getByLabelText(/^Session UTC/), { target: { value: '8-17' } })
    fireEvent.click(screen.getByText('save'))
    await screen.findByText('gate active')

    const patch = onApply.mock.calls[0][0]
    expect(patch.symbols).toEqual(['eurusd', 'gbpusd'])
    expect(patch.sessionUtc).toBe('8-17')
  })

  it('offers no edit affordance without a handler', () => {
    render(<RiskPanel policy={status.risk_policy} status={status} />)
    expect(screen.queryByText('edit')).toBeNull()
  })
})

describe('MetricsPanel', () => {
  it('sorts counters by count and renders the feed cursor', () => {
    render(
      <MetricsPanel
        metrics={{
          service: 'veyra',
          version: '0.1.0',
          counters: { 'proposal.held': 9, 'command.completed': 3 },
          feedLatest: 44,
        }}
      />,
    )
    expect(screen.getByText('proposal.held')).toBeTruthy()
    expect(screen.getByText('9')).toBeTruthy()
    expect(screen.getByText('feed #44')).toBeTruthy()
  })

  it('renders empty and error states', () => {
    const { rerender } = render(
      <MetricsPanel metrics={{ service: 'veyra', version: '0.1.0', counters: {}, feedLatest: 0 }} />,
    )
    expect(screen.getByText('No counters yet.')).toBeTruthy()
    rerender(<MetricsPanel error="metrics down" />)
    expect(screen.getByText('metrics down')).toBeTruthy()
  })
})

describe('CommandsPanel', () => {
  it('renders an empty state', () => {
    render(<CommandsPanel />)
    expect(screen.getByText('No commands yet.')).toBeTruthy()
  })

  it('expands a command into its full record', () => {
    render(<CommandsPanel commands={commands} />)
    expect(screen.getByText('open_order')).toBeTruthy()
    expect(screen.getByText('broker_timeout')).toBeTruthy()

    const row = screen.getByText('open_order').closest('button')
    expect(row).toBeTruthy()
    fireEvent.click(row as HTMLButtonElement)
    expect(screen.getByText('id cmd-1111-2222')).toBeTruthy()
    expect(screen.getByText(/"ticket": 99/)).toBeTruthy()

    fireEvent.click(row as HTMLButtonElement)
    expect(screen.queryByText('id cmd-1111-2222')).toBeNull()
  })

  it('expands a failed command with no summary', () => {
    render(<CommandsPanel commands={[commands[1]]} />)
    fireEvent.click(screen.getByText('close_order').closest('button') as HTMLButtonElement)
    expect(screen.getByText(/"summary": null/)).toBeTruthy()
    expect(screen.getByText(/"reason": "broker_timeout"/)).toBeTruthy()
  })
})

describe('ActivityFeed', () => {
  function Harness({ initialFocus = true, connected = true }: { initialFocus?: boolean; connected?: boolean }) {
    const [focus, setFocus] = useState(initialFocus)
    return <ActivityFeed events={events} connected={connected} focus={focus} onFocusChange={setFocus} />
  }

  it('renders the empty state', () => {
    render(<ActivityFeed events={[]} connected={false} focus onFocusChange={vi.fn()} />)
    expect(screen.getByText('Waiting for events…')).toBeTruthy()
    expect(screen.getByText('reconnecting…')).toBeTruthy()
  })

  it('hides routine plumbing in focus mode and restores it on toggle', () => {
    render(<Harness />)
    expect(screen.queryByText('broker_snapshot')).toBeNull()
    expect(screen.getByText('proposal_evaluated')).toBeTruthy()

    fireEvent.click(screen.getByText('focus'))
    expect(screen.getByText('broker_snapshot')).toBeTruthy()
    expect(screen.getByText('all')).toBeTruthy()
  })

  it('reports an idle decision stream when everything is routine', () => {
    render(<ActivityFeed events={[events[0]]} connected focus onFocusChange={vi.fn()} />)
    expect(screen.getByText('No decisions yet — routine activity hidden.')).toBeTruthy()
  })

  it('summarises closed positions with a signed profit', () => {
    render(<Harness initialFocus={false} />)
    expect(screen.getByText('ticket 7 EURUSD buy · P/L +12.50')).toBeTruthy()
  })

  it('expands a decision into ordered detail and raw payload', () => {
    render(<Harness />)
    const row = screen.getByText('held · #10650805').closest('button') as HTMLButtonElement
    fireEvent.click(row)

    expect(row.getAttribute('aria-expanded')).toBe('true')
    expect(screen.getByText('outcome')).toBeTruthy()
    expect(screen.getAllByText('held').length).toBeGreaterThan(0)
    expect(screen.getByText('symbol')).toBeTruthy()
    expect(screen.getByText('rationale')).toBeTruthy()
    expect(screen.getByText('Momentum favours the upside.')).toBeTruthy()
    expect(screen.getByText(/"outcome": "held"/)).toBeTruthy()

    fireEvent.click(row)
    expect(screen.queryByText(/"outcome": "held"/)).toBeNull()
  })

  it('colours held reviews as healthy and shows streaming state', () => {
    render(<Harness />)
    const summary = screen.getByText('held · #10650805')
    expect(summary.className).toContain('text-sky-300')
    expect(screen.getByText('streaming')).toBeTruthy()
  })
})

describe('Panel and Pill edges', () => {
  it('renders a bare panel without detail and a pill without value', () => {
    render(
      <>
        <Panel title="Bare">child</Panel>
        <Pill tone="off" label="empty" />
      </>,
    )
    expect(screen.getByText('Bare')).toBeTruthy()
    expect(screen.getByText('child')).toBeTruthy()
    expect(screen.getByText('empty')).toBeTruthy()
  })
})

describe('StatusPills switch-off variant', () => {
  it('renders disabled trading, missing audit, and no autopilot', () => {
    render(<StatusPills status={{ ...status, trading_enabled: false, persistence: null, autopilot: null }} />)
    expect(screen.getByText('disabled')).toBeTruthy()
    expect(screen.getAllByText('off').length).toBeGreaterThanOrEqual(2)
  })
})

describe('AccountPanel edge shapes', () => {
  it('renders a flat account and tolerates a missing profit', () => {
    const { rerender } = render(<AccountPanel account={{ ...account, positions: [] }} />)
    expect(screen.getAllByText('—').length).toBeGreaterThan(0)

    rerender(
      <AccountPanel
        account={{
          ...account,
          positions: [
            {
              ticket: 1,
              symbol: 'EURUSD',
              kind: 'buy',
              lots: 0.01,
              price: 1.1,
              profit: undefined as unknown as number,
              sl: 0,
              tp: 0,
              magic: 0,
            },
          ],
        }}
      />,
    )
    expect(screen.getByText('+0.00')).toBeTruthy()
  })

  it('renders an account without connection labels', () => {
    render(<AccountPanel account={{ ...account, server: undefined, login: undefined, symbol: undefined }} />)
    expect(screen.queryByText(/ICMarketsSC-MT4/)).toBeNull()
  })
})

describe('MarketPanel edge shapes', () => {
  const candle = (close: number) => ({ time: 1, open: close, high: close + 0.001, low: close - 0.001, close, volume: 1 })

  it('renders falling, flat, and empty series', () => {
    const { rerender } = render(
      <MarketPanel series={{ ...series, candles: [candle(1.12), candle(1.115), candle(1.1)] }} />,
    )
    expect(screen.getByText('-1.79%')).toBeTruthy()

    rerender(<MarketPanel series={{ ...series, candles: [candle(1.1), candle(1.1)] }} />)
    expect(screen.getByText('+0.00%')).toBeTruthy()

    rerender(<MarketPanel series={{ ...series, candles: [] }} />)
    expect(screen.getAllByText('—').length).toBeGreaterThan(0)
  })
})

describe('AutopilotPanel stop permutations', () => {
  it('renders trailing-only, break-even-only, and unbounded budgets', () => {
    const { rerender } = render(<AutopilotPanel status={{ ...autopilot, breakeven_r: 0, trail_r: 2 }} />)
    expect(screen.getByText('trail 2R')).toBeTruthy()

    rerender(<AutopilotPanel status={{ ...autopilot, breakeven_r: 1, trail_r: 0 }} />)
    expect(screen.getByText('BE 1R')).toBeTruthy()

    rerender(
      <AutopilotPanel status={autopilot} budget={{ hourLimit: 0, hourCalls: 1, dayLimit: 0, dayCalls: 1 }} />,
    )
    expect(screen.getByText('1/∞ h · 1/∞ d')).toBeTruthy()
  })
})

describe('RiskPanel edge shapes', () => {
  it('renders an empty allowlist, a session window, and disarmed switches', () => {
    render(
      <RiskPanel
        policy={{ ...status.risk_policy, symbols: [], sessionUtc: '07:00-21:00' }}
        status={{ ...status, trading_enabled: false, ea_live_orders: false }}
      />,
    )
    expect(screen.getByText('none allowed')).toBeTruthy()
    expect(screen.getByText('07:00-21:00')).toBeTruthy()
    expect(screen.getByText('switch off')).toBeTruthy()
    expect(screen.getByText('disarmed')).toBeTruthy()
  })
})

describe('MetricsPanel tie-breaking', () => {
  it('orders equal counts alphabetically', () => {
    render(
      <MetricsPanel
        metrics={{ service: 'veyra', version: '0.1.0', counters: { beta: 2, alpha: 2 }, feedLatest: 1 }}
      />,
    )
    const items = screen.getAllByRole('listitem')
    expect(items[0].textContent).toContain('alpha')
  })
})

describe('ActivityFeed unknown shapes', () => {
  it('falls back for unknown kinds and outcomes', () => {
    render(
      <ActivityFeed
        events={[{ seq: 9, at_ms: 1, kind: 'strategy_note', payload: { outcome: 'mystery' } }]}
        connected
        focus
        onFocusChange={vi.fn()}
      />,
    )
    expect(screen.getByText('strategy_note')).toBeTruthy()
    const summary = screen.getByText('{"outcome":"mystery"}')
    expect(summary.className).toContain('text-slate-300')
  })

  it('renders an event without a payload', () => {
    render(
      <ActivityFeed
        events={[{ seq: 10, at_ms: 1, kind: 'orphan', payload: undefined as unknown as Record<string, unknown> }]}
        connected
        focus
        onFocusChange={vi.fn()}
      />,
    )
    fireEvent.click(screen.getByText('{}').closest('button') as HTMLButtonElement)
    expect(screen.getAllByText('{}').length).toBeGreaterThan(1)
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
    const items = screen.getAllByRole('listitem')
    expect(items[0].textContent).toContain('stale heartbeat')
    expect(screen.getByText('autopilot tick')).toBeTruthy()
    expect(screen.getByText(/"symbol":"EURUSD"/)).toBeTruthy()
  })

  it('reports filter changes', () => {
    const onLevelChange = vi.fn()
    render(<LogsPanel logs={records} level="info" onLevelChange={onLevelChange} />)
    fireEvent.click(screen.getByRole('button', { name: 'warn' }))
    expect(onLevelChange).toHaveBeenCalledWith('warn')
  })

  it('renders unknown levels with the fallback tone', () => {
    render(
      <LogsPanel
        logs={[{ ...records[0], seq: 9, level: 'notice', message: 'custom level' }]}
        level="info"
        onLevelChange={vi.fn()}
      />,
    )
    expect(screen.getByText('custom level')).toBeTruthy()
    expect(screen.getByText('notice')).toBeTruthy()
  })

  it('renders empty and error states', () => {
    const { rerender } = render(<LogsPanel logs={[]} level="info" onLevelChange={vi.fn()} />)
    expect(screen.getByText('No log records yet.')).toBeTruthy()
    rerender(<LogsPanel logs={[]} error="logs down" level="info" onLevelChange={vi.fn()} />)
    expect(screen.getByText('logs down')).toBeTruthy()
    expect(screen.getByText('Log tail unavailable.')).toBeTruthy()
  })
})
