/**
 * Tests for reconciling MetaTrader's broker clock with UTC: the offset is
 * measured from the account snapshot, rounded to a real zone, and applied to
 * every broker stamp without touching anything else.
 */

import { describe, expect, it } from 'vitest'

import type { Account, CandleSeries, ClosedTrade } from './api'
import { accountInUtc, brokerOffsetSecs, seriesInUtc, tradesInUtc } from './broker-time'

const NOW_MS = Date.UTC(2026, 8, 24, 8, 43, 24)
const nowSecs = NOW_MS / 1000

const account = (fields: Partial<Account> = {}): Account => ({
  fresh: true,
  connected: true,
  tradeAllowed: true,
  liveOrders: true,
  ageSecs: 2,
  ...fields,
})

const trade = (closeTime: number): ClosedTrade => ({
  ticket: 1,
  symbol: 'USDJPY',
  kind: 'buy',
  lots: 0.01,
  openPrice: 158.3,
  closePrice: 158.4,
  openTime: closeTime - 3600,
  closeTime,
  profit: 0.62,
  swap: 0,
  commission: 0,
  magic: 77041,
})

describe('brokerOffsetSecs', () => {
  it('measures the broker clock against the snapshot age, to the quarter hour', () => {
    // Two hours ahead, observed two seconds ago, with a few seconds of drift.
    expect(brokerOffsetSecs(account({ serverTime: nowSecs - 2 + 7200 + 5 }), NOW_MS)).toBe(7200)
    expect(brokerOffsetSecs(account({ serverTime: nowSecs - 2 - 3 * 3600 - 20 }), NOW_MS)).toBe(-10800)
    expect(brokerOffsetSecs(account({ serverTime: nowSecs - 2 + 5.5 * 3600 }), NOW_MS)).toBe(19800)
  })

  it('is unknown without a snapshot clock or with an impossible one', () => {
    expect(brokerOffsetSecs(undefined, NOW_MS)).toBeUndefined()
    expect(brokerOffsetSecs(account(), NOW_MS)).toBeUndefined()
    expect(brokerOffsetSecs(account({ serverTime: nowSecs + 20 * 3600 }), NOW_MS)).toBeUndefined()
  })
})

describe('broker stamps in UTC', () => {
  it('moves trade times back by the offset and leaves the rest alone', () => {
    const [shifted] = tradesInUtc([trade(nowSecs + 180)], 7200)
    expect(shifted.closeTime).toBe(nowSecs + 180 - 7200)
    expect(shifted.openTime).toBe(nowSecs + 180 - 3600 - 7200)
    expect(shifted.profit).toBe(0.62)
  })

  it('shifts position open times, keeping positions that report none', () => {
    const utc = accountInUtc(
      account({
        positions: [
          { ticket: 1, symbol: 'AUDUSD', kind: 'sell', lots: 0.01, price: 0.7, profit: 0, sl: 0, tp: 0, magic: 77041, openedAt: 10_000 },
          { ticket: 2, symbol: 'EURUSD', kind: 'buy', lots: 0.01, price: 1.1, profit: 0, sl: 0, tp: 0, magic: 0 },
        ],
      }),
      7200,
    )
    expect(utc.positions?.[0].openedAt).toBe(2_800)
    expect(utc.positions?.[1].openedAt).toBeUndefined()
  })

  it('shifts bar times', () => {
    const series: CandleSeries = {
      symbol: 'EURUSD',
      timeframe: 'H4',
      candles: [{ time: 14_400, open: 1, high: 1, low: 1, close: 1, volume: 1 }],
    }
    expect(seriesInUtc(series, 3600).candles[0].time).toBe(10_800)
  })

  it('passes data through untouched when the clocks agree or nothing is held', () => {
    const trades = [trade(nowSecs)]
    const flat = account()
    const series: CandleSeries = { symbol: 'EURUSD', timeframe: 'H4', candles: [] }
    expect(tradesInUtc(trades, 0)).toBe(trades)
    expect(accountInUtc(flat, 0)).toBe(flat)
    expect(accountInUtc(flat, 7200)).toBe(flat)
    expect(seriesInUtc(series, 0)).toBe(series)
  })
})
