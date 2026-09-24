/**
 * MetaTrader's clocks, reconciled with UTC.
 *
 * The EA stamps closed orders, open positions and candles with the broker's
 * server clock, and reports the terminal host's clock as the account's
 * `serverTime` (MQL `TimeLocal`). Neither is UTC. In this deployment the
 * broker and the host keep the same offset — checked against the audit
 * trail's own UTC stamps for the same tickets — so the host offset, measured
 * against the snapshot's age and rounded to the quarter hour every real zone
 * uses, turns broker stamps back into true instants.
 *
 * Without an account snapshot the offset is unknown, and callers hold broker
 * times back rather than place them hours away from where they belong.
 */

import type { Account, CandleSeries, ClosedTrade } from './api'

const QUARTER_HOUR = 900

/** Offsets beyond any real zone mean a bad clock, not a zone. */
const MOST_OFFSET = 14 * 3600

/** Seconds the broker clock runs ahead of UTC, or undefined before a snapshot. */
export function brokerOffsetSecs(account: Account | undefined, nowMs: number): number | undefined {
  if (!account?.serverTime) return undefined
  const observedAt = nowMs / 1000 - account.ageSecs
  const offset = Math.round((account.serverTime - observedAt) / QUARTER_HOUR) * QUARTER_HOUR
  return Math.abs(offset) > MOST_OFFSET ? undefined : offset
}

/** Closed trades with open and close times as true unix seconds. */
export function tradesInUtc(trades: ClosedTrade[], offset: number): ClosedTrade[] {
  if (offset === 0) return trades
  return trades.map((trade) => ({ ...trade, openTime: trade.openTime - offset, closeTime: trade.closeTime - offset }))
}

/** The account with each position's open time as true unix seconds. */
export function accountInUtc(account: Account, offset: number): Account {
  if (offset === 0 || !account.positions) return account
  return {
    ...account,
    positions: account.positions.map((position) =>
      position.openedAt === undefined ? position : { ...position, openedAt: position.openedAt - offset },
    ),
  }
}

/** A candle series with bar times as true unix seconds. */
export function seriesInUtc(series: CandleSeries, offset: number): CandleSeries {
  if (offset === 0) return series
  return { ...series, candles: series.candles.map((candle) => ({ ...candle, time: candle.time - offset })) }
}
