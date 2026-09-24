/**
 * Tests for the balance-growth reconstruction behind the overview chart.
 *
 * The line is walked back from the broker balance through closed Veyra
 * trades, so these pin the window bounds, the realized-effect sum, ordering,
 * the percentage guard and the cent rounding that keeps a long walk exact.
 */

import { describe, expect, it } from 'vitest'

import type { ClosedTrade } from './api'
import { balanceGrowth, tradeNet } from './growth'

const DAY = 86_400_000
const NOW = Date.UTC(2026, 8, 23, 12, 0)
const FROM = NOW - 30 * DAY

function trade(closeMs: number, profit: number, extra: Partial<ClosedTrade> = {}): ClosedTrade {
  return {
    ticket: Math.round(closeMs / 1000),
    symbol: 'EURUSD',
    kind: 'buy',
    lots: 0.01,
    openPrice: 1.1,
    closePrice: 1.1,
    openTime: Math.round(closeMs / 1000) - 3600,
    closeTime: Math.round(closeMs / 1000),
    profit,
    swap: 0,
    commission: 0,
    magic: 77041,
    ...extra,
  }
}

describe('tradeNet', () => {
  it('sums profit, swap and commission in cents', () => {
    expect(tradeNet(trade(NOW, 1.1, { swap: -0.2, commission: -0.07 }))).toBe(0.83)
    expect(tradeNet(trade(NOW, 0.1, { swap: 0.2 }))).toBe(0.3)
  })
})

describe('balanceGrowth', () => {
  it('is a flat line at the current balance when no trade closed in the window', () => {
    const growth = balanceGrowth([], 37, FROM, NOW)
    expect(growth.points).toEqual([
      { atMs: FROM, balance: 37 },
      { atMs: NOW, balance: 37 },
    ])
    expect(growth).toMatchObject({ start: 37, end: 37, change: 0, changePercent: 0, high: 37, low: 37, trades: 0 })
  })

  it('ignores closes outside the window and keeps the bounds inclusive', () => {
    const trades = [
      trade(NOW + 1000, 5),
      trade(NOW, 1),
      trade(FROM, 2),
      trade(FROM - 1000, 7),
    ]
    const growth = balanceGrowth(trades, 50, FROM, NOW)
    expect(growth.trades).toBe(2)
    expect(growth.start).toBe(47)
    expect(growth.points.map((point) => point.balance)).toEqual([47, 49, 50, 50])
    expect(growth.points.map((point) => point.atMs)).toEqual([FROM, FROM, NOW, NOW])
  })

  it('walks back newest to oldest and returns the points oldest first', () => {
    const early = trade(NOW - 3 * DAY, 1.5)
    const middle = trade(NOW - 2 * DAY, -0.5, { swap: -0.1 })
    const late = trade(NOW - DAY, 2, { commission: -0.25 })
    // Served order is not trusted.
    const growth = balanceGrowth([middle, late, early], 40, FROM, NOW)
    expect(growth.points).toEqual([
      { atMs: FROM, balance: 37.35 },
      { atMs: early.closeTime * 1000, balance: 38.85, trade: early },
      { atMs: middle.closeTime * 1000, balance: 38.25, trade: middle },
      { atMs: late.closeTime * 1000, balance: 40, trade: late },
      { atMs: NOW, balance: 40 },
    ])
    expect(growth).toMatchObject({ start: 37.35, end: 40, change: 2.65, high: 40, low: 37.35, trades: 3 })
    expect(growth.changePercent).toBeCloseTo((2.65 / 37.35) * 100, 10)
  })

  it('orders simultaneous closes by ticket', () => {
    const at = NOW - DAY
    const first = trade(at, 1, { ticket: 10 })
    const second = trade(at, 2, { ticket: 11 })
    const growth = balanceGrowth([first, second], 10, FROM, NOW)
    expect(growth.points.map((point) => point.trade?.ticket)).toEqual([undefined, 10, 11, undefined])
    expect(growth.points.map((point) => point.balance)).toEqual([7, 8, 10, 10])
  })

  it('reports the low and high reached inside the window', () => {
    const growth = balanceGrowth([trade(NOW - 2 * DAY, -4), trade(NOW - DAY, 6)], 12, FROM, NOW)
    expect(growth.points.map((point) => point.balance)).toEqual([10, 6, 12, 12])
    expect(growth.high).toBe(12)
    expect(growth.low).toBe(6)
  })

  it('has no percentage when the start balance is not positive', () => {
    expect(balanceGrowth([trade(NOW - DAY, 10)], 10, FROM, NOW)).toMatchObject({ start: 0, change: 10, changePercent: null })
    expect(balanceGrowth([trade(NOW - DAY, 10)], 5, FROM, NOW)).toMatchObject({ start: -5, changePercent: null })
  })

  it('rounds to cents so a long walk back does not drift', () => {
    const trades = Array.from({ length: 10 }, (_, index) => trade(NOW - (index + 1) * 3600_000, 0.1))
    const growth = balanceGrowth(trades, 1.3, FROM, NOW)
    expect(growth.start).toBe(0.3)
    expect(growth.change).toBe(1)
    expect(growth.points.map((point) => point.balance)).toEqual([0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1, 1.1, 1.2, 1.3, 1.3])
    expect(balanceGrowth([], 37.004999, FROM, NOW).end).toBe(37)
  })
})
