/**
 * Trades page: every closed Veyra trade in the selected range, newest first,
 * with the dates it spans and why it closed.
 *
 * Nothing here is actionable — a closed trade cannot be touched — so the row
 * expands (like open positions) to explain itself: the model's entry case,
 * the close detail when the service has one, and the figures the table has
 * no room for. As elsewhere, only the reason's dot carries state; the label
 * stays plain so a column of colour is never something to decode.
 */

import { useState, type ReactNode } from 'react'

import type { CloseReason, ClosedTradeRow, TradesPage, TradesSummary } from '../lib/api'
import { signedAmount } from '../lib/format'
import { Dot, Icon, Panel, Segmented, Skeleton, signTone, type Tone } from './ui'
import { Pager } from './veyra'

export type TradesRange = 7 | 30 | 90 | 365

const RANGES: ReadonlyArray<{ value: TradesRange; label: string }> = [
  { value: 7, label: '7D' },
  { value: 30, label: '30D' },
  { value: 90, label: '90D' },
  { value: 365, label: '1Y' },
]

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`
}

/** Label shown beside the reason's dot. */
export const REASON_LABEL: Record<CloseReason, string> = {
  take_profit: 'Take profit',
  stop_loss: 'Stop loss',
  break_even_stop: 'Break-even stop',
  trailing_stop: 'Trailing stop',
  harvest_stop: 'Harvest stop',
  harvest_close: 'Harvest close',
  agent_close: 'Closed by agent',
  manual_close: 'Closed manually',
  unknown: 'Closed at broker',
}

/**
 * State tone for the reason's dot: a realized-profit reason reads ok, a stop
 * that can be a loss reads bad, a protective stop that need not be reads
 * warn, and an operational close that says nothing about the result reads
 * idle.
 */
export const REASON_TONE: Record<CloseReason, Tone> = {
  take_profit: 'ok',
  harvest_close: 'ok',
  stop_loss: 'bad',
  break_even_stop: 'warn',
  trailing_stop: 'warn',
  harvest_stop: 'warn',
  agent_close: 'idle',
  manual_close: 'idle',
  unknown: 'idle',
}

/** Wall-clock close time: `25 Sep 15:08`, always dated (the table spans many days). */
function closedClock(ms: number): string {
  const date = new Date(ms)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getDate()} ${MONTHS[date.getMonth()]} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/** Wall-clock open time for the detail drawer: `25 Sep, 15:08`. */
function openedClock(ms: number): string {
  const date = new Date(ms)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getDate()} ${MONTHS[date.getMonth()]}, ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/** How long a closed trade was held: `42m`, `21h 18m`, `2d 5h`. */
export function heldFor(openedAtMs: number, closedAtMs: number): string {
  const minutes = Math.max(0, Math.round((closedAtMs - openedAtMs) / 60_000))
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ${minutes % 60}m`
  return `${Math.floor(hours / 24)}d ${hours % 24}h`
}

/**
 * The instrument's quote precision, taken as the widest seen across the
 * row's own prices — see the identical reasoning in overview.tsx.
 */
function quotedDecimals(trade: ClosedTradeRow): number {
  const prices = [trade.openPrice, trade.closePrice, trade.stopLoss, trade.takeProfit]
  const widest = prices.reduce<number>((most, value) => {
    if (!value) return most
    return Math.max(most, (String(value).split('.')[1] ?? '').length)
  }, 0)
  return widest || 2
}

/** A price at the instrument's precision; zero or absent reads as a dash. */
function price(value: number | null | undefined, decimals: number): string {
  return value ? value.toFixed(decimals) : '—'
}

/** A signed R multiple, e.g. `+1.50R`, `−1.00R`; absent (no stop to measure against) reads as a dash. */
function rMultiple(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—'
  const rounded = Number(value.toFixed(2))
  if (rounded === 0) return '0.00R'
  return `${rounded > 0 ? '+' : '−'}${Math.abs(rounded).toFixed(2)}R`
}

const COLUMNS = ['Closed', 'Symbol', 'Side', 'Lots', 'Entry → Exit', 'Held', 'Net', 'R', 'Reason']

function TradeRow({
  trade,
  expanded,
  onToggle,
}: {
  trade: ClosedTradeRow
  expanded: boolean
  onToggle: () => void
}) {
  const decimals = quotedDecimals(trade)
  const detailId = `trd-detail-${trade.ticket}`
  const side = trade.side === 'long' ? 'Long' : 'Short'
  return (
    <>
      <tr className={`trd-row${expanded ? ' is-expanded' : ''}`}>
        <td className="trd-closed">
          <time dateTime={new Date(trade.closedAtMs).toISOString()}>{closedClock(trade.closedAtMs)}</time>
        </td>
        <td>{trade.symbol}</td>
        <td className={trade.side === 'long' ? 'tone-ok' : 'tone-bad'}>{side}</td>
        <td>{trade.lots.toFixed(2)}</td>
        <td className="trd-prices">
          <span>{price(trade.openPrice, decimals)}</span> <span aria-hidden="true">→</span>{' '}
          <span>{price(trade.closePrice, decimals)}</span>
        </td>
        <td>{heldFor(trade.openedAtMs, trade.closedAtMs)}</td>
        <td className={signTone(trade.net)}>{signedAmount(trade.net)}</td>
        <td>{rMultiple(trade.rMultiple)}</td>
        <td>
          <span className="trd-reason">
            <Dot tone={REASON_TONE[trade.closeReason]} />
            {REASON_LABEL[trade.closeReason]}
          </span>
        </td>
        <td className="trd-actions">
          <button
            type="button"
            className="icon-button trd-more"
            aria-label={`Details for ${trade.symbol} ${trade.side}`}
            aria-expanded={expanded}
            aria-controls={detailId}
            onClick={onToggle}
          >
            <Icon name="chevron-down" size={14} />
          </button>
        </td>
      </tr>
      {expanded ? (
        <tr className="trd-detail" id={detailId}>
          <td colSpan={COLUMNS.length + 1}>
            {trade.entryRationale ? <p className="trd-rationale">{trade.entryRationale}</p> : null}
            {trade.closeDetail ? <p className="trd-rationale">{trade.closeDetail}</p> : null}
            <dl className="trd-detail-list">
              <div>
                <dt>Ticket</dt>
                <dd>{trade.ticket}</dd>
              </div>
              <div>
                <dt>Opened</dt>
                <dd>{openedClock(trade.openedAtMs)}</dd>
              </div>
              <div>
                <dt>Stop</dt>
                <dd>{price(trade.stopLoss, decimals)}</dd>
              </div>
              <div>
                <dt>Target</dt>
                <dd>{price(trade.takeProfit, decimals)}</dd>
              </div>
              <div>
                <dt>Swap</dt>
                <dd>{signedAmount(trade.swap)}</dd>
              </div>
              <div>
                <dt>Commission</dt>
                <dd>{signedAmount(trade.commission)}</dd>
              </div>
            </dl>
          </td>
        </tr>
      ) : null}
    </>
  )
}

/** `48 trades · 45 wins · 3 losses · +27.56`, the net toned by its sign. */
function TradesSummaryLine({ summary }: { summary: TradesSummary }) {
  const parts = [
    plural(summary.count, 'trade', 'trades'),
    plural(summary.wins, 'win', 'wins'),
    plural(summary.losses, 'loss', 'losses'),
    ...(summary.breakeven > 0 ? [plural(summary.breakeven, 'breakeven', 'breakeven')] : []),
  ]
  return (
    <span className="trd-summary">
      {parts.join(' · ')} · <span className={signTone(summary.net) || 'tone-muted'}>{signedAmount(summary.net)}</span>
    </span>
  )
}

/**
 * The closed-trade book: a range control, a compact summary for the whole
 * range, and one page of rows. The service pages the list because each row's
 * close reason costs an audit lookup.
 */
export function TradesPanel({
  page,
  error,
  range,
  onRangeChange,
  onPageChange,
}: {
  page?: TradesPage
  error?: string
  range: TradesRange
  onRangeChange: (range: TradesRange) => void
  /** Asks for another 1-based page. */
  onPageChange: (page: number) => void
}) {
  const [expanded, setExpanded] = useState<number>()
  const trades = page?.trades ?? []

  let body: ReactNode
  if (error) {
    body = <div className="panel-empty tone-bad">Unavailable</div>
  } else if (!page) {
    body = (
      <div className="trd-loading">
        <Skeleton width="100%" height={14} />
        <Skeleton width="100%" height={14} />
        <Skeleton width="100%" height={14} />
        <Skeleton width="100%" height={14} />
      </div>
    )
  } else if (trades.length === 0) {
    body = <div className="panel-empty">No closed trades in this range</div>
  } else {
    body = (
      <div className="trd-scroll" role="region" aria-label="Closed trades table" tabIndex={0}>
        <table className="trd-table">
          <colgroup>
            <col className="trd-col-closed" />
            <col className="trd-col-symbol" />
            <col className="trd-col-side" />
            <col className="trd-col-lots" />
            <col className="trd-col-prices" />
            <col className="trd-col-held" />
            <col className="trd-col-net" />
            <col className="trd-col-r" />
            <col className="trd-col-reason" />
            <col className="trd-col-actions" />
          </colgroup>
          <thead>
            <tr>
              {COLUMNS.map((column) => (
                <th key={column} scope="col">
                  {column}
                </th>
              ))}
              <th scope="col">
                <span className="sr-only">Details</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {trades.map((trade) => (
              <TradeRow
                key={trade.ticket}
                trade={trade}
                expanded={expanded === trade.ticket}
                onToggle={() => setExpanded(expanded === trade.ticket ? undefined : trade.ticket)}
              />
            ))}
          </tbody>
        </table>
        <Pager
          page={page.page - 1}
          pages={page.pageCount}
          start={(page.page - 1) * page.pageSize}
          count={trades.length}
          total={page.summary.count}
          onPrevious={() => onPageChange(page.page - 1)}
          onNext={() => onPageChange(page.page + 1)}
        />
      </div>
    )
  }

  return (
    <Panel
      className="tab-panel trd-panel"
      title="Trades"
      actions={
        <>
          <Segmented options={RANGES} value={range} onChange={onRangeChange} label="Range" />
          {page && !error ? <TradesSummaryLine summary={page.summary} /> : null}
          {page?.truncated ? <span className="trd-truncated tone-warn">Truncated</span> : null}
        </>
      }
    >
      {body}
    </Panel>
  )
}
