/**
 * Veyra's balance growth over a time window, for the overview chart.
 *
 * Boundary: the line is reconstructed from the venue's closed Veyra orders
 * and anchored to the broker's current balance. Walking backwards from that
 * balance, each close is undone by its realized effect (profit + swap +
 * commission) to recover the balance before it. It is not an equity curve:
 * open positions are ignored, and deposits, withdrawals and manual (non-Veyra)
 * trades are not in the source, so they are not in the line either.
 *
 * Pure: no clock, no I/O. Callers pass the window and the current time.
 */

import type { ClosedTrade } from './api'

export type GrowthPoint = {
  atMs: number
  balance: number
  /** The close that produced this balance; absent on the start and end points. */
  trade?: ClosedTrade
}

export type BalanceGrowth = {
  /** Oldest first: window start, one point per close, then now. */
  points: GrowthPoint[]
  start: number
  end: number
  change: number
  /** Change relative to the start balance; null when the start is not positive. */
  changePercent: number | null
  high: number
  low: number
  /** Closes inside the window. */
  trades: number
}

/** Money is summed in cents so a long walk back does not drift by float error. */
function cents(value: number): number {
  return Math.round(value * 100)
}

/** A trade's realized effect on the balance. */
export function tradeNet(trade: ClosedTrade): number {
  return (cents(trade.profit) + cents(trade.swap) + cents(trade.commission)) / 100
}

/** Balance path over `[fromMs, nowMs]` ending at the current broker balance. */
export function balanceGrowth(
  trades: ReadonlyArray<ClosedTrade>,
  balance: number,
  fromMs: number,
  nowMs: number,
): BalanceGrowth {
  const inWindow = trades
    .filter((trade) => trade.closeTime * 1000 >= fromMs && trade.closeTime * 1000 <= nowMs)
    // Newest first; ties keep the higher ticket as the later close.
    .sort((left, right) => right.closeTime - left.closeTime || right.ticket - left.ticket)

  let running = cents(balance)
  const closes: GrowthPoint[] = []
  for (const trade of inWindow) {
    closes.push({ atMs: trade.closeTime * 1000, balance: running / 100, trade })
    running -= cents(tradeNet(trade))
  }
  closes.reverse()

  const end = cents(balance) / 100
  const start = running / 100
  const points: GrowthPoint[] = [{ atMs: fromMs, balance: start }, ...closes, { atMs: nowMs, balance: end }]
  const balances = points.map((point) => point.balance)
  const change = (cents(end) - cents(start)) / 100

  return {
    points,
    start,
    end,
    change,
    changePercent: start > 0 ? (change / start) * 100 : null,
    high: Math.max(...balances),
    low: Math.min(...balances),
    trades: inWindow.length,
  }
}
