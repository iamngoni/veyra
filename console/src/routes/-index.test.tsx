/**
 * Dashboard wiring test: renders the route component against mocked control
 * surface responses and asserts every operational panel appears. The feed
 * promise is left pending so the polling loops stay quiet in the test.
 */

import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
  status: vi.fn(),
  account: vi.fn(),
  commands: vi.fn(),
  candles: vi.fn(),
  metrics: vi.fn(),
  events: vi.fn(),
}))

vi.mock('../lib/api', () => ({
  VEYRA_MAGIC: 77041,
  api: {
    status: mocks.status,
    account: mocks.account,
    commands: mocks.commands,
    candles: mocks.candles,
    metrics: mocks.metrics,
    events: mocks.events,
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
      jev: 'auto',
      breakeven_r: 1,
      trail_r: 1,
    },
    model_budget: { hourLimit: 120, hourCalls: 5, dayLimit: 2000, dayCalls: 5 },
    risk_policy: {
      killSwitch: false,
      symbols: ['EURUSD'],
      maxVolumePerOrder: 0.01,
      maxTotalLots: 0.01,
      maxOpenOrders: 1,
      duplicateWindowSecs: 60,
      sessionUtc: null,
    },
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
  mocks.metrics.mockResolvedValue({ service: 'veyra', version: '0.1.0', counters: { 'event.proposal_evaluated': 3 }, feedLatest: 12 })
  mocks.events.mockImplementation(() => new Promise(() => undefined))
})

describe('Dashboard', () => {
  it('renders every operational panel', async () => {
    render(<Dashboard />)

    expect(await screen.findByText('VEYRA')).toBeTruthy()
    expect(await screen.findByText('Balance')).toBeTruthy()
    expect(screen.getByText('Market')).toBeTruthy()
    expect(screen.getByText('Autopilot')).toBeTruthy()
    expect(screen.getByText('Activity')).toBeTruthy()
    expect(screen.getByText('Positions')).toBeTruthy()
    expect(screen.getByText('Commands')).toBeTruthy()
    expect(screen.getByText('Risk')).toBeTruthy()
    expect(screen.getByText('Metrics')).toBeTruthy()
    expect(screen.getByText(/v0.1.0/)).toBeTruthy()
  })
})
