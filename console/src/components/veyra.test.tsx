/**
 * Render tests for the console panels.
 *
 * These exercise the operator-visible surfaces with the same payload shapes
 * the service emits: armed/disarmed pills, the open-position table, decision
 * drill-downs, routine-event focus mode, and command inspection.
 */

import { useState } from 'react'

import { cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import type {
  Account,
  CandleSeries,
  CommandRecord,
  FeedEvent,
  LogRecord,
  MarketSessions,
  Performance,
  Status,
} from '../lib/api'
import { VEYRA_MAGIC } from '../lib/api'
import { auditTimeMs } from '../lib/hooks'
import {
  AccountPanel,
  ActivityFeed,
  AutopilotPanel,
  CommandsPanel,
  HeroMetrics,
  LogsPanel,
  MarketPanel,
  MetricsPanel,
  Panel,
  PerformancePanel,
  Pill,
  PositionsPanel,
  PostureBanner,
  RiskPanel,
  SafetyControls,
  StatusPills,
  systemPosture,
  Tabs,
  ThemeToggle,
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

describe('Tabs, ThemeToggle, and HeroMetrics', () => {
  it('renders a badge, selects tabs, and flips the theme label', () => {
    const onSelect = vi.fn()
    render(
      <Tabs
        tabs={[
          { id: 'overview', label: 'Overview' },
          { id: 'trace', label: 'Trace', badge: 7 },
        ]}
        active="overview"
        onSelect={onSelect}
      />,
    )
    expect(screen.getByText('7')).toBeTruthy()
    fireEvent.click(screen.getByText('Trace'))
    expect(onSelect).toHaveBeenCalledWith('trace')

    const onToggle = vi.fn()
    const view = render(<ThemeToggle theme="dark" onToggle={onToggle} />)
    fireEvent.click(view.getByLabelText('Switch to light theme'))
    expect(onToggle).toHaveBeenCalledTimes(1)
    view.rerender(<ThemeToggle theme="light" onToggle={onToggle} />)
    expect(view.getByLabelText('Switch to dark theme')).toBeTruthy()
  })

  it('sums open P/L with singular hints', () => {
    render(<HeroMetrics account={account} />)
    expect(screen.getByText('-0.32')).toBeTruthy()
    expect(screen.getByText('1 position')).toBeTruthy()
    expect(screen.getByText('1 order')).toBeTruthy()
  })

  it('pluralises a two-position book and skips absent money fields', () => {
    const first = account.positions?.[0]
    const { rerender } = render(
      <HeroMetrics
        account={{
          ...account,
          orders: 2,
          lots: 0.02,
          positions: first
            ? [
                { ...first, profit: 0.5 },
                { ...first, ticket: 2, profit: -0.1 },
              ]
            : [],
        }}
      />,
    )
    expect(screen.getByText('+0.40')).toBeTruthy()
    expect(screen.getByText('2 positions')).toBeTruthy()
    expect(screen.getByText('2 orders')).toBeTruthy()

    // Absent money fields keep their skeletons instead of rendering numbers.
    rerender(
      <HeroMetrics
        account={{
          ...account,
          equity: undefined,
          lots: undefined,
          freeMargin: undefined,
          marginLevel: undefined,
        }}
      />,
    )
    expect(screen.queryByText('1 order')).toBeNull()
    expect(screen.queryByText(/balance/)).toBeNull()
  })

  it('treats a missing profit as zero in the open total', () => {
    const first = account.positions?.[0]
    render(
      <HeroMetrics
        account={{
          ...account,
          positions: first ? [{ ...first, profit: undefined as unknown as number }] : [],
        }}
      />,
    )
    expect(screen.getByText('+0.00')).toBeTruthy()
  })

  it('renders pending skeletons and an explicit outage', () => {
    const { rerender } = render(<HeroMetrics />)
    expect(screen.queryByText('unavailable')).toBeNull()
    rerender(<HeroMetrics error="account down" />)
    expect(screen.getAllByText('unavailable').length).toBe(4)
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
            { ticket: 42, symbol: 'USDJPY', kind: 'buy', lots: 0.1, price: 156.2, profit: 4, sl: 155.9, tp: 156.4, magic: 0 },
            { ticket: 43, symbol: 'XAUUSD', kind: 'buy', lots: 0.01, price: 2_400, profit: 0.5, sl: 2_390, tp: 2_420, swap: 0.02, magic: 0 },
          ],
        }}
      />,
    )
    expect(screen.getByText('10650805')).toBeTruthy()
    expect(screen.getByText('EURUSD')).toBeTruthy()
    expect(screen.getByText('USDJPY')).toBeTruthy()
    expect(screen.getByText('XAUUSD')).toBeTruthy()
    expect(screen.getByText('-0.11')).toBeTruthy()
    expect(screen.getByText('+0.02')).toBeTruthy()
    // One missing swap, plus a current price none of these fixtures carry.
    expect(screen.getAllByText('—').length).toBe(4)
    expect(screen.getByText('veyra')).toBeTruthy()
    expect(screen.getAllByText('manual').length).toBe(2)
    expect(screen.getByText('truncated')).toBeTruthy()
    expect(screen.getByText('+4.00')).toBeTruthy()
  })

  it('reports the live price and signs the move against the side', () => {
    render(
      <PositionsPanel
        account={{
          ...account,
          positions: [
            // A sell in profit: price fell below the entry.
            { ticket: 1, symbol: 'EURUSD', kind: 'sell', lots: 0.01, price: 1.14757, current: 1.14585, profit: 1.72, sl: 1.1497, tp: 1.14554, magic: VEYRA_MAGIC },
            // A buy against it: price fell below the entry too.
            { ticket: 2, symbol: 'GBPUSD', kind: 'buy', lots: 0.01, price: 1.3, current: 1.295, profit: -5, sl: 1.29, tp: 1.31, magic: 0 },
          ],
        }}
      />,
    )
    expect(screen.getByText('1.14585')).toBeTruthy()
    // The same downward move is favourable for the sell and adverse for the buy.
    expect(screen.getByText('+0.00172')).toBeTruthy()
    expect(screen.getByText('-0.005')).toBeTruthy()
  })

  it('falls back to two decimals when every price is zero', () => {
    render(
      <PositionsPanel
        account={{
          ...account,
          positions: [
            {
              ticket: 9,
              symbol: 'XAUUSD',
              kind: 'buy',
              lots: 0.01,
              price: 0,
              profit: 0,
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

  it('widens to two decimals when every quoted price is an integer', () => {
    render(
      <PositionsPanel
        account={{
          ...account,
          positions: [
            {
              ticket: 11,
              symbol: 'XAUUSD',
              kind: 'buy',
              lots: 0.01,
              price: 2,
              current: 2,
              profit: 1.5,
              sl: 0,
              tp: 0,
              magic: 0,
            },
          ],
        }}
      />,
    )
    expect(screen.getByText('+0.00')).toBeTruthy()
    expect(screen.getByText('+1.50')).toBeTruthy()
  })

  it('states a missing current price rather than implying one', () => {
    render(
      <PositionsPanel
        account={{
          ...account,
          positions: [
            { ticket: 3, symbol: 'EURUSD', kind: 'buy', lots: 0.01, price: 1.1, current: 0, profit: 0, sl: 0, tp: 0, magic: 0 },
          ],
        }}
      />,
    )
    // A zero from the terminal means "not reported", never a real price.
    expect(screen.queryByText('+0.0')).toBeNull()
    expect(screen.getAllByText('—').length).toBeGreaterThanOrEqual(1)
  })
})

describe('PerformancePanel', () => {
  const performance: Performance = {
    days: 30,
    report: {
      trades: 2,
      wins: 1,
      losses: 1,
      breakeven: 0,
      win_rate_percent: 50,
      net_profit: 0.55,
      gross_profit: 1.36,
      gross_loss: 0.81,
      profit_factor: 1.68,
      average_win: 1.36,
      average_loss: 0.81,
      expectancy: 0.28,
      best_trade: 1.36,
      worst_trade: -0.81,
      by_symbol: [
        { symbol: 'USDJPY', trades: 1, wins: 1, net_profit: 1.36 },
        { symbol: 'EURUSD', trades: 1, wins: 0, net_profit: -0.81 },
      ],
    },
    trades: [],
    total: 2,
    truncated: false,
  }

  it('renders realized performance and per-symbol totals', () => {
    render(<PerformancePanel performance={performance} />)
    expect(screen.getByText('Win rate')).toBeTruthy()
    expect(screen.getByText('50.0%')).toBeTruthy()
    expect(screen.getByText('1W · 1L')).toBeTruthy()
    expect(screen.getByText('+0.55')).toBeTruthy()
    expect(screen.getByText('1.68')).toBeTruthy()
    expect(screen.getAllByText('+1.36').length).toBe(2)
    expect(screen.getAllByText('-0.81').length).toBe(2)
    expect(screen.getByText(/last 30d · 2 closed/)).toBeTruthy()
  })

  it('reports a truncated window with breakeven trades and a cost-adjusted loss', () => {
    render(
      <PerformancePanel
        performance={{
          ...performance,
          truncated: true,
          report: {
            ...performance.report,
            trades: 3,
            wins: 1,
            losses: 1,
            breakeven: 1,
            net_profit: -0.25,
            average_loss: 0.4,
            by_symbol: [],
          },
        }}
      />,
    )
    expect(screen.getByText(/last 30d · 2 closed · truncated/)).toBeTruthy()
    expect(screen.getByText('1W · 1L · 1F')).toBeTruthy()
    expect(screen.getByText('-0.25')).toBeTruthy()
    expect(screen.getByText('-0.40')).toBeTruthy()
  })

  it('shows waiting, error, and empty states', () => {
    const { rerender } = render(<PerformancePanel />)
    expect(screen.getByText('waiting…')).toBeTruthy()
    rerender(<PerformancePanel error="performance down" />)
    expect(screen.getByText('performance down')).toBeTruthy()

    rerender(
      <PerformancePanel
        performance={{
          ...performance,
          report: {
            ...performance.report,
            trades: 0,
            wins: 0,
            losses: 0,
            net_profit: 0,
            profit_factor: null,
            average_win: null,
            average_loss: null,
            by_symbol: [],
          },
        }}
      />,
    )
    expect(screen.getAllByText('—').length).toBeGreaterThanOrEqual(4)
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

  it('reports the weekend checkpoint while the window is open', () => {
    const runUp: MarketSessions = {
      now: 1_789_761_600, // Friday 2026-09-18 20:00 UTC
      market: { state: 'open', nextEvent: 'closes', nextAt: 1_789_765_200 },
      entries: {
        open: false,
        blockedBy: 'weekend_approach',
        detail: 'the weekend entry cutoff has passed',
      },
      policy: {
        rolloverBlackout: { startMinute: 1245, endMinute: 1335 },
        fridayEntryCutoffMinute: 1140,
        sundayEntryOpenMinute: 1380,
      },
      weekend: { policy: 'agent', closesInSecs: 3_600 },
    }
    const { rerender } = render(<MarketPanel series={series} sessions={runUp} account={account} />)
    expect(screen.getByText(/Weekend checkpoint/)).toBeTruthy()
    // Both the session line and the checkpoint line count down to the same
    // close, so the label appears twice with the same instant.
    expect(screen.getAllByText(/closes Fri 21:00 UTC/)).toHaveLength(2)
    expect(screen.getAllByText(/in 1h 0m/)).toHaveLength(2)
    expect(screen.getByText(/the analyst settles each open position/)).toBeTruthy()

    // The operator's override reads differently on the same line.
    rerender(
      <MarketPanel
        series={series}
        sessions={{ ...runUp, weekend: { policy: 'flatten', closesInSecs: 1_800 } }}
        account={account}
      />,
    )
    expect(screen.getByText(/every open position is flattened/)).toBeTruthy()
  })

  it('reports the closed week, the entry block, and what is held', () => {
    render(<MarketPanel series={series} sessions={closedSessions} account={account} />)
    expect(screen.getByText('closed')).toBeTruthy()
    expect(screen.getByText(/opens Sun 21:00 UTC/)).toBeTruthy()
    expect(screen.getByText(/in 1d 21h/)).toBeTruthy()
    expect(screen.getByText('blocked — weekend')).toBeTruthy()
    expect(screen.getByText('Holding EURUSD — market closed; stops rest at the broker')).toBeTruthy()
  })

  it('reports an open market, an open rollover pause, and open entries', () => {
    const { rerender } = render(
      <MarketPanel
        series={series}
        sessions={{
          ...closedSessions,
          now: 1_789_776_000,
          market: { state: 'open', nextEvent: 'pauses', nextAt: 1_789_776_000 + 3_600 },
          entries: { open: true, blockedBy: null, detail: null },
        }}
      />,
    )
    expect(screen.getAllByText('open')).toHaveLength(2) // week state and entries
    expect(screen.getByText(/rollover pause/)).toBeTruthy()
    expect(screen.queryByText(/Holding/)).toBeNull()

    // An open market with a position keeps the plain holding line.
    rerender(
      <MarketPanel
        series={series}
        account={account}
        sessions={{
          ...closedSessions,
          market: { state: 'open', nextEvent: 'pauses', nextAt: 1_789_776_000 + 3_600 },
          entries: { open: true, blockedBy: null, detail: null },
        }}
      />,
    )
    expect(screen.getByText('Holding EURUSD')).toBeTruthy()

    rerender(
      <MarketPanel
        series={series}
        sessions={{
          ...closedSessions,
          market: { state: 'rollover', nextEvent: 'resumes', nextAt: 1_789_776_000 + 600 },
          entries: { open: false, blockedBy: 'rollover_blackout', detail: 'the daily rollover blackout is active' },
        }}
      />,
    )
    expect(screen.getByText('rollover')).toBeTruthy()
    expect(screen.getByText('blocked — rollover blackout')).toBeTruthy()
    expect(screen.getByText(/in 10m/)).toBeTruthy()
  })

  it('renders an unrecognised block reason verbatim instead of guessing', () => {
    render(
      <MarketPanel
        series={series}
        sessions={{
          ...closedSessions,
          entries: { open: false, blockedBy: 'broker_holiday', detail: 'closed for a holiday' },
        }}
      />,
    )
    expect(screen.getByText('blocked — broker_holiday')).toBeTruthy()
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

  it('reports small token counts and judge failures verbatim', () => {
    render(
      <AutopilotPanel
        status={autopilot}
        budget={status.model_budget}
        jevUsage={{ calls: 3, failures: 2, inputTokens: 420, outputTokens: 80 }}
      />,
    )
    expect(screen.getByText('3 calls · 500 tok · 2 failed')).toBeTruthy()
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

  it('states whether a judge outage keeps trading or pauses decisions', () => {
    const { rerender } = render(<RiskPanel policy={status.risk_policy} status={status} />)
    expect(screen.getByText('pauses decisions')).toBeTruthy()
    rerender(
      <RiskPanel
        policy={{ ...status.risk_policy, allowTradingWithoutJev: true }}
        status={status}
      />,
    )
    expect(screen.getByText('keeps trading')).toBeTruthy()
  })

  it('renders the kill switch and missing policy', () => {
    const { rerender } = render(<RiskPanel policy={{ ...status.risk_policy, killSwitch: true }} status={status} />)
    expect(screen.getByText('kill switch on')).toBeTruthy()
    rerender(<RiskPanel />)
    expect(screen.getAllByText('—').length).toBeGreaterThan(0)
  })
})

describe('TracePanel', () => {
  const page = {
    status: 'ok' as const,
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

    // Scoped to the rows: the same kind also names a filter button above.
    const rows = screen.getByRole('list')
    fireEvent.click(within(rows).getByText('agent_turn'))
    // The prompt is readable in full: that is the point of the view.
    expect(screen.getByText('the exact prompt the model was shown')).toBeTruthy()
    expect(screen.getByText('inputChars')).toBeTruthy()
  })

  it('filters by kind and states an empty trail plainly', () => {
    const { rerender } = render(<TracePanel page={page} kind="failure" onKindChange={() => {}} />)
    const rows = screen.getByRole('list')
    expect(within(rows).getByText('panic · 2 fields')).toBeTruthy()
    expect(within(rows).queryByText('agent_turn')).toBeNull()

    rerender(<TracePanel page={{ status: 'ok', events: [] }} kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('No durable events recorded yet.')).toBeTruthy()
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
    render(
      <TracePanel page={{ status: 'disabled', events: [] }} kind="all" onKindChange={() => {}} />,
    )
    expect(screen.getByText('disabled')).toBeTruthy()
  })

  it('states an absent page and an errored trail explicitly', () => {
    const { rerender } = render(<TracePanel kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('No durable events recorded yet.')).toBeTruthy()

    rerender(<TracePanel error="audit down" kind="all" onKindChange={() => {}} />)
    expect(screen.getByText('audit down')).toBeTruthy()
    expect(screen.getByText('Trail unavailable.')).toBeTruthy()
  })

  it('collapses a row, and tolerates an unknown kind, no payload, and a bad time', () => {
    const rows = {
      status: 'ok' as const,
      events: [
        { id: 'mystery-1', at: 'not a timestamp', kind: 'mystery_event' },
        { id: 'plain-2', at: '2026-09-18 17:47:46.844116+00', kind: 'service_started', payload: {} },
      ],
    }
    const onKindChange = vi.fn()
    render(<TracePanel page={rows as never} kind="all" onKindChange={onKindChange} />)

    // An unreadable instant is a dash, never a confident wrong time.
    expect(screen.getByText('—')).toBeTruthy()
    // Payload-less rows still report themselves honestly.
    expect(screen.getAllByText('0 fields').length).toBe(2)

    // Clicking a kind chip asks the dashboard to filter; clicking a row twice
    // expands and collapses it.
    fireEvent.click(screen.getAllByText('mystery_event')[0])
    expect(onKindChange).toHaveBeenCalledWith('mystery_event')
    const list = screen.getByRole('list')
    fireEvent.click(within(list).getByText('mystery_event'))
    const expanded = screen.queryAllByRole('button', { expanded: true })
    expect(expanded.length).toBe(1)
    fireEvent.click(within(list).getByText('mystery_event'))
    expect(screen.queryAllByRole('button', { expanded: true }).length).toBe(0)
  })
})

describe('systemPosture', () => {
  it('reports the most restrictive true statement first', () => {
    expect(systemPosture(undefined).label).toBe('CONNECTING')
    // A halted gate outranks everything else that looks healthy.
    expect(systemPosture({ ...status, risk_policy: { ...status.risk_policy, killSwitch: true } }).label).toBe(
      'HALTED',
    )
    expect(systemPosture({ ...status, broker_connected: false }).label).toBe('NO LINK')
    expect(systemPosture({ ...status, trading_enabled: false }).label).toBe('STANDBY')
    // Service armed but the terminal still validating only.
    expect(systemPosture({ ...status, ea_live_orders: false }).label).toBe('DRY RUN')
    expect(systemPosture(status).label).toBe('LIVE')
  })

  it('names a decision outage that every other signal reports as healthy', () => {
    // The exact shape of today's failures: service up, terminal live, trading
    // armed — and every request refused.
    const refusing = {
      ...status,
      decisions: {
        consecutiveFailures: 4,
        lastFailure: 'openrouter call returned 400: Thinking mode does not support this tool_choice',
        lastFailureAt: 1_700_000_000,
      },
    }
    expect(systemPosture(refusing).label).toBe('NOT DECIDING')
    expect(systemPosture(refusing).tone).toBe('bad')
    expect(systemPosture(refusing).detail).toContain('Thinking mode')

    // The reason can be missing; the banner says so rather than guessing.
    expect(
      systemPosture({ ...status, decisions: { consecutiveFailures: 2, lastFailure: null, lastFailureAt: 0 } })
        .detail,
    ).toContain('no reason reported')

    // One failure is a hiccup, not an outage.
    expect(
      systemPosture({ ...status, decisions: { consecutiveFailures: 1, lastFailure: 'x', lastFailureAt: 1 } }).label,
    ).toBe('LIVE')

    // A halted gate still outranks it: the owner stopped this deliberately.
    expect(
      systemPosture({ ...refusing, risk_policy: { ...status.risk_policy, killSwitch: true } }).label,
    ).toBe('HALTED')
  })

  it('renders the verdict and its explanation', () => {
    render(<PostureBanner status={status} />)
    expect(screen.getByText('LIVE')).toBeTruthy()
    expect(screen.getByText('orders reach the market')).toBeTruthy()
  })
})

describe('SafetyControls', () => {
  it('requires a confirmation before changing either switch', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<SafetyControls policy={status.risk_policy} onApply={onApply} />)

    // Tripping the switch only asks the question; nothing is sent yet.
    fireEvent.click(screen.getByRole('switch', { name: 'Kill switch' }))
    expect(onApply).not.toHaveBeenCalled()
    expect(screen.getByText('Halt all new intents?')).toBeTruthy()

    fireEvent.click(screen.getByText('Confirm'))
    await screen.findByRole('switch', { name: 'Kill switch' })
    expect(onApply).toHaveBeenCalledWith({ killSwitch: true })
  })

  it('sends the judge override and reports a refusal', async () => {
    const onApply = vi.fn().mockResolvedValue('invalid_policy')
    render(<SafetyControls policy={status.risk_policy} onApply={onApply} />)

    fireEvent.click(screen.getByRole('switch', { name: 'Trade without the judge' }))
    fireEvent.click(screen.getByText('Confirm'))
    expect(await screen.findByText('invalid_policy')).toBeTruthy()
    expect(onApply).toHaveBeenCalledWith({ allowTradingWithoutJev: true })
  })

  it('cancels a confirmation and stays quiet while the judge is healthy', () => {
    render(<SafetyControls policy={status.risk_policy} jevHealthy={true} onApply={vi.fn()} />)
    const killSwitch = screen.getByRole('switch', { name: 'Kill switch' })
    fireEvent.click(killSwitch)
    expect(screen.getByText('Halt all new intents?')).toBeTruthy()
    fireEvent.click(killSwitch)
    expect(screen.queryByText('Halt all new intents?')).toBeNull()
    expect(screen.queryByText(/judge is not answering/)).toBeNull()
  })

  it('cancels the judge-override confirmation', () => {
    render(<SafetyControls policy={status.risk_policy} onApply={vi.fn()} />)
    const judge = screen.getByRole('switch', { name: 'Trade without the judge' })
    fireEvent.click(judge)
    expect(screen.getByText('Allow trading without the judge?')).toBeTruthy()
    fireEvent.click(judge)
    expect(screen.queryByText('Allow trading without the judge?')).toBeNull()
  })

  it('describes engaged switches and their release actions', () => {
    render(
      <SafetyControls
        policy={{ ...status.risk_policy, killSwitch: true, allowTradingWithoutJev: true }}
        onApply={vi.fn()}
      />,
    )
    expect(screen.getByText(/Engaged. Every new intent is refused/)).toBeTruthy()
    expect(screen.getByText(/Override active/)).toBeTruthy()
    fireEvent.click(screen.getByRole('switch', { name: 'Kill switch' }))
    expect(screen.getByText('Release the kill switch?')).toBeTruthy()
  })

  it('warns while the judge is failing, differently for each setting', () => {
    const { rerender } = render(<SafetyControls policy={status.risk_policy} jevHealthy={false} />)
    expect(screen.getByText(/new decisions are paused right now/)).toBeTruthy()

    rerender(
      <SafetyControls
        policy={{ ...status.risk_policy, allowTradingWithoutJev: true }}
        jevHealthy={false}
      />,
    )
    expect(screen.getByText(/the model is deciding alone/)).toBeTruthy()
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
    fireEvent.change(screen.getByLabelText(/Weekend positions/), { target: { value: 'flatten' } })
    fireEvent.click(screen.getByText('save'))
    await screen.findByText('gate active')

    const patch = onApply.mock.calls[0][0]
    expect(patch.symbols).toEqual(['eurusd', 'gbpusd'])
    expect(patch.sessionUtc).toBe('8-17')
    expect(patch.weekendPositions).toBe('flatten')
  })

  it('names the weekend preference in the read-only summary', () => {
    const { rerender } = render(<RiskPanel policy={status.risk_policy} status={status} />)
    expect(screen.getByText('analyst decides')).toBeTruthy()
    rerender(
      <RiskPanel policy={{ ...status.risk_policy, weekendPositions: 'flatten' }} status={status} />,
    )
    expect(screen.getByText('flattened before close')).toBeTruthy()
    rerender(
      <RiskPanel policy={{ ...status.risk_policy, weekendPositions: 'hold' }} status={status} />,
    )
    expect(screen.getByText('held through')).toBeTruthy()
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

  it('renders the waiting state before any poll lands', () => {
    render(<MetricsPanel />)
    expect(screen.getByText('No counters yet.')).toBeTruthy()
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
    expect(summary.className).toContain('text-[var(--color-info)]')
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
    expect(summary.className).toContain('text-[var(--color-ink)]')
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
