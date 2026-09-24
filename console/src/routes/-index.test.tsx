/**
 * Dashboard wiring test: renders the route component against mocked control
 * surface responses and asserts every operational panel appears. The feed
 * promise is left pending so the polling loops stay quiet in the test.
 */

import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
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

import { Dashboard } from '../components/dashboard'

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
    trades: [
      {
        ticket: 10654166,
        symbol: 'USDJPY',
        kind: 'buy',
        lots: 0.01,
        openPrice: 158.1,
        closePrice: 158.3,
        openTime: Math.floor(Date.now() / 1000) - 7200,
        closeTime: Math.floor(Date.now() / 1000) - 3600,
        profit: 1.36,
        swap: 0,
        commission: 0,
        magic: 77041,
      },
    ],
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
  /** Moves to a view through the sidebar, mirroring what an operator clicks. */
  const openTab = (label: string) =>
    fireEvent.click(within(screen.getByRole('navigation', { name: 'Main' })).getByRole('button', { name: label }))

  it('lays out the overview: status, money, growth chart, positions, rail and switches', async () => {
    render(<Dashboard />)

    expect(await screen.findByText('Veyra')).toBeTruthy()
    expect(await screen.findByText('Terminal connected')).toBeTruthy()
    expect(await screen.findByText('Equity')).toBeTruthy()
    expect(screen.getByText('Open P/L')).toBeTruthy()
    expect(screen.getByRole('button', { name: /Performance/ })).toBeTruthy()
    expect(screen.getByRole('heading', { name: /Open positions/ })).toBeTruthy()
    expect(screen.getByRole('heading', { name: '30-day performance' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: 'Autopilot' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: 'Recent activity' })).toBeTruthy()
    expect(screen.getByRole('switch', { name: 'Kill switch' })).toBeTruthy()
    expect(screen.getByRole('switch', { name: 'Judge bypass' })).toBeTruthy()
    expect(within(screen.getByRole('navigation', { name: 'Main' })).getByRole('button', { name: 'Overview' }).getAttribute('aria-current')).toBe('page')
  })

  it('reaches every operational panel through the sidebar', async () => {
    render(<Dashboard />)
    await screen.findByText('Equity')

    openTab('Activity')
    expect(screen.getByRole('heading', { name: /Commands/ })).toBeTruthy()

    openTab('Risk')
    expect(await screen.findByText('Balance')).toBeTruthy()
    expect(screen.getByRole('heading', { name: 'Risk policy' })).toBeTruthy()

    openTab('Diagnostics')
    expect(screen.getByRole('heading', { name: 'Metrics' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: 'Agent log' })).toBeTruthy()

    openTab('Trace')
    expect(screen.getByRole('heading', { name: /Audit trail/ })).toBeTruthy()
  })

  it('switches the chart to an instrument and refetches its candles at once', async () => {
    render(<Dashboard />)
    await screen.findByText('Equity')
    await waitFor(() => expect(mocks.candles).toHaveBeenCalledWith(120, 'H4', undefined))

    fireEvent.click(screen.getByRole('button', { name: /Performance/ }))
    fireEvent.click(screen.getByRole('menuitemradio', { name: 'Market · EURUSD' }))
    await waitFor(() => expect(mocks.candles).toHaveBeenCalledWith(120, 'H4', 'EURUSD'))

    fireEvent.click(screen.getByRole('button', { name: 'D1' }))
    await waitFor(() => expect(mocks.candles).toHaveBeenCalledWith(120, 'D1', 'EURUSD'))
  })

  it('routes the activity digest, the gear and the theme toggle', async () => {
    render(<Dashboard />)
    await screen.findByText('Equity')

    fireEvent.click(screen.getByRole('button', { name: 'View all' }))
    expect(screen.getByRole('heading', { level: 1, name: 'Activity' })).toBeTruthy()

    const topbar = document.querySelector<HTMLElement>('.shell-topbar')!
    fireEvent.click(within(topbar).getByRole('button', { name: 'Settings' }))
    expect(await screen.findByRole('heading', { name: 'Live settings' })).toBeTruthy()

    const before = document.documentElement.dataset.theme
    fireEvent.click(within(topbar).getByRole('button', { name: /Switch to (light|dark) theme/ }))
    expect(document.documentElement.dataset.theme).not.toBe(before)
  })

  it('shows diagnostic metadata and applies live settings through the route', async () => {
    mocks.updateConfig.mockResolvedValue({ changed: ['VEYRA_TRADING_ENABLED'], settings: {} })
    render(<Dashboard />)
    await screen.findByText('Equity')

    openTab('Diagnostics')
    expect(screen.getAllByText('development').length).toBeGreaterThanOrEqual(1)
    expect(screen.getAllByText('postgres').length).toBeGreaterThanOrEqual(1)

    openTab('Settings')
    await screen.findByRole('heading', { name: 'Live settings' })
    fireEvent.click(screen.getByRole('switch', { name: 'Trading enabled' }))
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    await screen.findByText('Applied')
    expect(mocks.updateConfig).toHaveBeenCalledWith({ VEYRA_TRADING_ENABLED: 'false' })
  })

  it('surfaces live-settings rejections from the route callback', async () => {
    mocks.updateConfig.mockRejectedValue(new Error('setting rejected'))
    render(<Dashboard />)
    await screen.findByText('Equity')
    openTab('Settings')
    await screen.findByRole('heading', { name: 'Live settings' })
    fireEvent.click(screen.getByRole('switch', { name: 'Trading enabled' }))
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
    fireEvent.click(screen.getByRole('button', { name: 'Confirm' }))
    expect(await screen.findByText('Judge not answering — decisions paused.')).toBeTruthy()

    mocks.status.mockResolvedValue({
      ...initialStatus,
      jev_usage: { ...initialStatus.jev_usage, calls: initialStatus.jev_usage.calls + 2, failures: initialStatus.jev_usage.failures + 1 },
    })
    fireEvent.click(screen.getByRole('switch', { name: 'Judge bypass' }))
    fireEvent.click(screen.getByRole('button', { name: 'Confirm' }))
    await waitFor(() => expect(screen.queryByText(/decisions paused/)).toBeNull())
  })

  it('keeps a non-Error policy rejection readable', async () => {
    mocks.updatePolicy.mockRejectedValue('policy transport failed')
    render(<Dashboard />)
    await screen.findByText('Equity')
    fireEvent.click(screen.getByRole('switch', { name: 'Kill switch' }))
    fireEvent.click(screen.getByRole('button', { name: 'Confirm' }))
    expect(await screen.findByText('policy transport failed')).toBeTruthy()
  })

  it('applies policy edits through the control surface', async () => {
    mocks.updatePolicy.mockResolvedValue({})
    render(<Dashboard />)
    await screen.findByText('Equity')
    openTab('Risk')
    await screen.findByText('Balance')

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.change(screen.getByLabelText('Max open orders'), { target: { value: '4' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await screen.findByText('Gate active')
    expect(mocks.updatePolicy).toHaveBeenCalledTimes(1)
    expect(mocks.updatePolicy.mock.calls[0][0].maxOpenOrders).toBe(4)
  })

  it('surfaces control-surface rejections in the editor', async () => {
    mocks.updatePolicy.mockRejectedValue(new Error('maxOpenOrders: too large'))
    render(<Dashboard />)
    await screen.findByText('Equity')
    openTab('Risk')
    await screen.findByText('Balance')

    fireEvent.click(screen.getByRole('button', { name: 'Edit' }))
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await screen.findByText('maxOpenOrders: too large')
    expect(screen.getByRole('button', { name: 'Save' })).toBeTruthy()
  })

  it('sends a confirmed kill switch from the overview controls', async () => {
    mocks.updatePolicy.mockResolvedValue({})
    render(<Dashboard />)
    await screen.findByText('Equity')

    fireEvent.click(screen.getByRole('switch', { name: 'Kill switch' }))
    fireEvent.click(screen.getByRole('button', { name: 'Confirm' }))

    await waitFor(() => expect(mocks.updatePolicy).toHaveBeenCalledWith({ killSwitch: true }))
  })
})
