/**
 * Dashboard wiring test: renders the route component against mocked control
 * surface responses and asserts every operational panel appears. The feed
 * promise is left pending so the polling loops stay quiet in the test.
 */

import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
  status: vi.fn(),
  account: vi.fn(),
  commands: vi.fn(),
  performance: vi.fn(),
  sessions: vi.fn(),
  candles: vi.fn(),
  balanceHistory: vi.fn(),
  metrics: vi.fn(),
  events: vi.fn(),
  logs: vi.fn(),
  audit: vi.fn(),
  config: vi.fn(),
  updatePolicy: vi.fn(),
  updateConfig: vi.fn(),
}))

vi.mock('../lib/api', () => ({
  VEYRA_MAGIC: 77041,
  LOG_LEVELS: ['error', 'warn', 'info', 'debug', 'trace'],
  api: {
    status: mocks.status,
    account: mocks.account,
    commands: mocks.commands,
    performance: mocks.performance,
    sessions: mocks.sessions,
    candles: mocks.candles,
    metrics: mocks.metrics,
    events: mocks.events,
    logs: mocks.logs,
    audit: mocks.audit,
    config: mocks.config,
    balanceHistory: mocks.balanceHistory,
    updatePolicy: mocks.updatePolicy,
    updateConfig: mocks.updateConfig,
  },
}))

import { Dashboard } from './index'

afterEach(cleanup)

beforeEach(() => {
  mocks.status.mockResolvedValue({
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
    autopilot: {
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
    },
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
      weekendPositions: 'agent',
    },
  })
  mocks.sessions.mockResolvedValue({
    now: 1_789_800_000,
    market: { state: 'closed', nextEvent: 'opens', nextAt: 1_790_000_400 },
    entries: { open: false, blockedBy: 'weekend_open', detail: 'the market has not reopened for the week' },
    policy: {
      rolloverBlackout: { startMinute: 1245, endMinute: 1335 },
      fridayEntryCutoffMinute: 1140,
      sundayEntryOpenMinute: 1380,
    },
    weekend: { policy: 'agent', closesInSecs: null },
  })
  mocks.performance.mockResolvedValue({
    days: 30,
    report: {
      trades: 1,
      wins: 1,
      losses: 0,
      breakeven: 0,
      win_rate_percent: 100,
      net_profit: 1.36,
      gross_profit: 1.36,
      gross_loss: 0,
      profit_factor: null,
      average_win: 1.36,
      average_loss: null,
      expectancy: 1.36,
      best_trade: 1.36,
      worst_trade: 1.36,
      by_symbol: [{ symbol: 'USDJPY', trades: 1, wins: 1, net_profit: 1.36 }],
    },
    trades: [],
    total: 1,
    truncated: false,
  })
  mocks.account.mockResolvedValue({
    fresh: true,
    connected: true,
    tradeAllowed: true,
    liveOrders: true,
    login: 123456,
    server: 'ICMarketsSC-MT4',
    symbol: 'EURUSD',
    ageSecs: 1,
    balance: 1000,
    equity: 1000,
    freeMargin: 1000,
    orders: 0,
    lots: 0,
    positions: [],
  })
  mocks.commands.mockResolvedValue({ commands: [] })
  mocks.candles.mockResolvedValue({
    symbol: 'EURUSD',
    timeframe: 'H4',
    candles: [
      { time: 1, open: 1.1, high: 1.11, low: 1.09, close: 1.1, volume: 5 },
      { time: 2, open: 1.1, high: 1.12, low: 1.1, close: 1.115, volume: 6 },
    ],
  })
  mocks.balanceHistory.mockResolvedValue({
    status: 'waiting_for_account',
    source: 'broker_balance',
    account: null,
    days: 30,
    retentionDays: 365,
    currency: null,
    points: [],
    firstObservedAtMs: null,
    lastObservedAtMs: null,
    sampled: false,
    fresh: false,
  })
  mocks.metrics.mockResolvedValue({ service: 'veyra', version: '0.1.0', counters: { 'event.proposal_evaluated': 3 }, feedLatest: 12 })
  mocks.events.mockImplementation(() => new Promise(() => undefined))
  mocks.logs.mockResolvedValue({ logs: [], latest: 0 })
  mocks.audit.mockResolvedValue({ status: 'ok', provider: 'postgres', events: [] })
  mocks.config.mockResolvedValue({
    settings: {
      VEYRA_TRADING_ENABLED: { value: 'true', overridden: false },
      VEYRA_AUTOPILOT_INTERVAL_SECS: { value: '60', overridden: false },
    },
    live_sections: ['trading'],
  })
  mocks.updatePolicy.mockReset()
  mocks.updateConfig.mockReset()
})

describe('Dashboard', () => {
  /** Moves to a tab by its control, mirroring what an operator clicks. */
  const openTab = (label: string) => fireEvent.click(screen.getByRole('tab', { name: label }))

  it('pins posture, money and the safety switches above the tabs', async () => {
    render(<Dashboard />)

    expect(await screen.findByText('VEYRA')).toBeTruthy()
    // Always visible, whichever tab is selected.
    expect(await screen.findByText('Equity')).toBeTruthy()
    expect(screen.getByText('Open P/L')).toBeTruthy()
    expect(screen.getByRole('switch', { name: 'Kill switch' })).toBeTruthy()
    expect(screen.getByRole('switch', { name: 'Trade without the judge' })).toBeTruthy()
    expect(screen.getByText(/v0.1.0/)).toBeTruthy()
  })

  it('reaches every operational panel through its tab', async () => {
    render(<Dashboard />)
    await screen.findByText('Equity')

    // Overview is the landing tab.
    expect(screen.getByText('Positions')).toBeTruthy()
    expect(screen.getByRole('heading', { name: 'Autopilot' })).toBeTruthy()
    expect(screen.getByText('Market')).toBeTruthy()

    openTab('Activity')
    expect(screen.getByText('Commands')).toBeTruthy()

    openTab('Risk')
    expect(await screen.findByText('Balance')).toBeTruthy()

    openTab('Diagnostics')
    expect(screen.getByText('Metrics')).toBeTruthy()
    expect(screen.getByText('Agent log')).toBeTruthy()

    openTab('Trace')
    expect(screen.getAllByText('Trace').length).toBeGreaterThanOrEqual(1)
  })

  it('opens the market view and routes the overview digest to Activity', async () => {
    render(<Dashboard />)
    await screen.findByText('Equity')

    openTab('Market')
    expect(screen.getAllByRole('heading', { name: 'Market' }).length).toBeGreaterThanOrEqual(1)

    openTab('Overview')
    fireEvent.click(screen.getByRole('button', { name: 'View all' }))
    expect(screen.getAllByRole('heading', { name: 'Activity' }).length).toBeGreaterThanOrEqual(1)
  })

  it('shows diagnostic metadata and applies live settings through the route', async () => {
    mocks.updateConfig.mockResolvedValue({ changed: ['VEYRA_TRADING_ENABLED'], settings: {} })
    render(<Dashboard />)
    await screen.findByText('Equity')

    openTab('Diagnostics')
    expect(screen.getAllByText('development').length).toBeGreaterThanOrEqual(1)
    expect(screen.getAllByText('postgres').length).toBeGreaterThanOrEqual(1)

    openTab('Settings')
    expect(await screen.findByText('Live settings')).toBeTruthy()
    fireEvent.change(screen.getByDisplayValue('true'), { target: { value: 'false' } })
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    await screen.findByText('applied')
    expect(mocks.updateConfig).toHaveBeenCalledWith({ VEYRA_TRADING_ENABLED: 'false' })
  })

  it('surfaces live-settings rejections from the route callback', async () => {
    mocks.updateConfig.mockRejectedValue(new Error('setting rejected'))
    render(<Dashboard />)
    await screen.findByText('Equity')
    openTab('Settings')
    await screen.findByText('Live settings')
    fireEvent.change(screen.getByDisplayValue('true'), { target: { value: 'false' } })
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    expect((await screen.findByRole('alert')).textContent).toContain('setting rejected')
  })

  it('marks the judge degraded after a failed usage update', async () => {
    mocks.updatePolicy.mockResolvedValue({})
    render(<Dashboard />)
    await screen.findByText('Equity')
    const initialStatus = await mocks.status.mock.results[0].value
    mocks.status.mockResolvedValue({
      ...initialStatus,
      jev_usage: { ...initialStatus.jev_usage, calls: initialStatus.jev_usage.calls + 1, failures: initialStatus.jev_usage.failures + 1 },
    })

    fireEvent.click(screen.getByRole('switch', { name: 'Kill switch' }))
    fireEvent.click(screen.getByText('Confirm'))
    expect(await screen.findByText(/judge is not answering — new decisions are paused/)).toBeTruthy()

    mocks.status.mockResolvedValue({
      ...initialStatus,
      jev_usage: { ...initialStatus.jev_usage, calls: initialStatus.jev_usage.calls + 2, failures: initialStatus.jev_usage.failures + 1 },
    })
    fireEvent.click(screen.getByRole('switch', { name: 'Trade without the judge' }))
    fireEvent.click(screen.getByText('Confirm'))
    await waitFor(() => expect(screen.queryByText(/new decisions are paused/)).toBeNull())
  })

  it('keeps a non-Error policy rejection readable', async () => {
    mocks.updatePolicy.mockRejectedValue('policy transport failed')
    render(<Dashboard />)
    await screen.findByText('Equity')
    fireEvent.click(screen.getByRole('switch', { name: 'Kill switch' }))
    fireEvent.click(screen.getByText('Confirm'))
    expect(await screen.findByText('policy transport failed')).toBeTruthy()
  })

  it('applies policy edits through the control surface', async () => {
    mocks.updatePolicy.mockResolvedValue({})
    render(<Dashboard />)
    await screen.findByText('Equity')
    openTab('Risk')
    await screen.findByText('Balance')

    fireEvent.click(screen.getByText('edit'))
    fireEvent.change(screen.getByLabelText('Max open orders'), { target: { value: '4' } })
    fireEvent.click(screen.getByText('save'))

    await screen.findByText('gate active')
    expect(mocks.updatePolicy).toHaveBeenCalledTimes(1)
    expect(mocks.updatePolicy.mock.calls[0][0].maxOpenOrders).toBe(4)
  })

  it('surfaces control-surface rejections in the editor', async () => {
    mocks.updatePolicy.mockRejectedValue(new Error('maxOpenOrders: too large'))
    render(<Dashboard />)
    await screen.findByText('Equity')
    openTab('Risk')
    await screen.findByText('Balance')

    fireEvent.click(screen.getByText('edit'))
    fireEvent.click(screen.getByText('save'))

    await screen.findByText('maxOpenOrders: too large')
    expect(screen.getByText('save')).toBeTruthy()
  })

  it('sends a confirmed kill switch from the pinned controls', async () => {
    mocks.updatePolicy.mockResolvedValue({})
    render(<Dashboard />)
    await screen.findByText('Equity')

    fireEvent.click(screen.getByRole('switch', { name: 'Kill switch' }))
    fireEvent.click(screen.getByText('Confirm'))

    await screen.findByText('Equity')
    expect(mocks.updatePolicy).toHaveBeenCalledWith({ killSwitch: true })
  })
})
