/**
 * Overview main column: headline figures, open positions and the 30-day
 * realized performance summary.
 *
 * Every figure comes from the service. A value that has not arrived yet holds
 * its place with a skeleton, a failed poll with nothing to show reads
 * "Unavailable", and a field the venue did not report reads as an em dash —
 * nothing here estimates, back-fills or rounds a number into something else.
 */

import { useState, type ReactNode } from 'react'

import type { Account, ClosedTrade, Performance, Position } from '../lib/api'
import { VEYRA_MAGIC } from '../lib/api'
import { amount, percent, signedAmount, signedPercent } from '../lib/format'
import { Dot, Icon, Panel, Skeleton, signTone, type Tone } from './ui'

/* ---------- shared bits ---------- */

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`
}

/** Sign tone for a figure, falling back to the muted voice when it is flat. */
function toneOrMuted(value: number): string {
  return signTone(value) || 'tone-muted'
}

function Unavailable({ className }: { className: string }) {
  return <span className={`${className} tone-bad`}>Unavailable</span>
}

function Truncated({ className }: { className: string }) {
  return <span className={`${className} tone-warn`}>Truncated</span>
}

/* ---------- headline cards ---------- */

type Kpi = { label: string; value: ReactNode; tone?: string; sub?: ReactNode; subTone?: string }

/** Unix seconds of the most recent local midnight at `now` (ms). */
function localMidnight(now: number): number {
  const day = new Date(now)
  day.setHours(0, 0, 0, 0)
  return day.getTime() / 1000
}

/** Net realized result (profit, swap and commission) of trades closed since local midnight. */
function realizedSince(trades: ClosedTrade[], since: number): number {
  return trades
    .filter((trade) => trade.closeTime >= since)
    .reduce((sum, trade) => sum + trade.profit + trade.swap + trade.commission, 0)
}

/** A position's floating result with its charges, so it matches what closing it would realize. */
function positionResult(position: Position): number {
  return position.profit + (position.swap ?? 0) + (position.commission ?? 0)
}

function kpis(account: Account, trades: ClosedTrade[] | undefined, now: number): Kpi[] {
  const positions = account.positions ?? []
  const open = positions.reduce((sum, position) => sum + positionResult(position), 0)
  const today = trades ? realizedSince(trades, localMidnight(now)) : undefined
  const { balance, equity, freeMargin, lots, orders } = account
  return [
    {
      label: 'Equity',
      value: amount(equity),
      sub: today === undefined ? undefined : `${signedAmount(today)} today`,
      subTone: today === undefined ? undefined : toneOrMuted(today),
    },
    {
      label: 'Open P/L',
      value: signedAmount(open),
      tone: signTone(open),
      sub: positions.length > 0 && balance ? signedPercent((open / balance) * 100) : undefined,
      subTone: toneOrMuted(open),
    },
    {
      label: 'Exposure',
      value: lots === undefined ? '—' : `${lots.toFixed(2)} lots`,
      sub: orders === undefined ? undefined : plural(orders, 'open order', 'open orders'),
    },
    {
      label: 'Free margin',
      value: amount(freeMargin),
      sub: freeMargin !== undefined && equity ? `${percent((freeMargin / equity) * 100)} available` : undefined,
    },
  ]
}

const KPI_LABELS = ['Equity', 'Open P/L', 'Exposure', 'Free margin']

/** The four account figures heading the overview: equity, open P/L, exposure and free margin. */
export function KpiRow({
  account,
  error,
  trades,
  now = Date.now(),
}: {
  account?: Account
  error?: string
  /** Closed Veyra trades (any window covering today) for the day's realized change. */
  trades?: ClosedTrade[]
  /** Clock for "today"; tests pin it. */
  now?: number
}) {
  const placeholder = error ? <Unavailable className="kpi-unavailable" /> : <Skeleton width={128} height={24} />
  const cards: Kpi[] = account
    ? kpis(account, trades, now)
    : KPI_LABELS.map((label) => ({ label, value: placeholder }))
  return (
    <section className="kpi-row" aria-label="Account">
      <dl className="kpi-grid">
        {cards.map((card) => (
          <div key={card.label} className="kpi-card">
            <dt className="kpi-label">{card.label}</dt>
            <dd className={`kpi-value ${card.tone ?? ''}`}>{card.value}</dd>
            <dd className={`kpi-sub ${card.subTone ?? ''}`}>{card.sub}</dd>
          </div>
        ))}
      </dl>
    </section>
  )
}

/* ---------- open positions ---------- */

/**
 * The instrument's quote precision, taken as the widest seen across the row's
 * prices: any single one can be short a digit (an entry that lands on 1.3 says
 * nothing about how finely the pair is quoted), and rounding every price to it
 * would misreport the others.
 */
function quotedDecimals(position: Position): number {
  const prices = [position.price, position.current, position.sl, position.tp]
  const widest = prices.reduce<number>((most, price) => {
    if (!price) return most
    return Math.max(most, (String(price).split('.')[1] ?? '').length)
  }, 0)
  return widest || 2
}

/** A price at the instrument's precision; zero or absent (no stop, no quote) reads as a dash. */
function price(value: number | undefined, decimals: number): string {
  return value ? value.toFixed(decimals) : '—'
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

/**
 * `23 Sep, 14:05` in local time. Built by hand rather than through Intl, whose
 * short month for September varies by ICU version ("Sep" / "Sept").
 */
/** Wall-clock open time: `09:14` today, `24 Sep 09:14` on any earlier day. */
function openedClock(seconds: number, now: number): string {
  const date = new Date(seconds * 1000)
  const today = new Date(now)
  const pad = (value: number) => String(value).padStart(2, '0')
  const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`
  return date.toDateString() === today.toDateString() ? clock : `${date.getDate()} ${MONTHS[date.getMonth()]} ${clock}`
}

/** How long a position has been held: `42m`, `3h 12m`, `2d 5h`. */
export function heldFor(seconds: number, now: number): string {
  const minutes = Math.max(0, Math.floor((now / 1000 - seconds) / 60))
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ${minutes % 60}m`
  return `${Math.floor(hours / 24)}d ${hours % 24}h`
}

function openedAt(seconds: number | undefined): string {
  if (!seconds) return '—'
  const date = new Date(seconds * 1000)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getDate()} ${MONTHS[date.getMonth()]}, ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/**
 * What profit harvesting is doing with a position. The API reports whether the
 * policy is on, not whether it has armed on a given position, so the most this
 * can truthfully say for a Veyra position is that it is being watched.
 */
function harvest(position: Position, enabled: boolean): { tone: Tone; label: string } {
  if (position.magic !== VEYRA_MAGIC) return { tone: 'idle', label: 'Manual' }
  return enabled ? { tone: 'ok', label: 'Monitoring' } : { tone: 'off', label: 'Off' }
}

const COLUMNS = ['Symbol', 'Side', 'Lots', 'Entry', 'Current', 'SL', 'TP', 'P/L', 'Opened', 'Profit harvest']

/** A manual close the operator has asked for on one row. */
type Closing = { ticket: number; phase: 'confirm' | 'pending' | 'queued'; error?: string }

function PositionRow({
  position,
  harvestEnabled,
  expanded,
  onToggle,
  now,
  closing,
  closable,
  closeBlocked,
  onRequestClose,
  onCancelClose,
  onConfirmClose,
}: {
  position: Position
  harvestEnabled: boolean
  expanded: boolean
  onToggle: () => void
  now: number
  closing?: Closing
  /** Veyra-owned and a close handler exists. */
  closable: boolean
  /** Why closing is unavailable right now, e.g. trading is disabled. */
  closeBlocked?: string
  onRequestClose: () => void
  onCancelClose: () => void
  onConfirmClose: () => void
}) {
  const decimals = quotedDecimals(position)
  const result = positionResult(position)
  const state = harvest(position, harvestEnabled)
  const detailId = `pos-detail-${position.ticket}`
  const side = position.kind === 'buy' ? 'long' : 'short'
  return (
    <>
      <tr className={`pos-row${expanded ? ' is-expanded' : ''}`}>
        <td>{position.symbol}</td>
        <td className={position.kind === 'buy' ? 'tone-ok' : 'tone-bad'}>{position.kind === 'buy' ? 'Long' : 'Short'}</td>
        <td>{position.lots.toFixed(2)}</td>
        <td>{price(position.price, decimals)}</td>
        <td>{price(position.current, decimals)}</td>
        <td>{price(position.sl, decimals)}</td>
        <td>{price(position.tp, decimals)}</td>
        <td className={signTone(result)}>{signedAmount(result)}</td>
        <td className="pos-opened">
          {position.openedAt ? (
            <>
              <time dateTime={new Date(position.openedAt * 1000).toISOString()}>{openedClock(position.openedAt, now)}</time>
              <span className="pos-held" title="Time held">
                {heldFor(position.openedAt, now)}
              </span>
            </>
          ) : (
            '—'
          )}
        </td>
        <td>
          <span className="pos-harvest">
            <Dot tone={state.tone} />
            {state.label}
          </span>
        </td>
        <td className="pos-actions">
          {closable ? (
            <button
              type="button"
              className="icon-button pos-close"
              aria-label={`Close ${position.symbol} ${side}`}
              title={closeBlocked ?? 'Close at market'}
              disabled={Boolean(closeBlocked) || closing?.phase === 'pending' || closing?.phase === 'queued'}
              onClick={onRequestClose}
            >
              <Icon name="close" size={14} />
            </button>
          ) : null}
          <button
            type="button"
            className="icon-button pos-more"
            aria-label={`Details for ${position.symbol}`}
            aria-expanded={expanded}
            aria-controls={detailId}
            onClick={onToggle}
          >
            <Icon name="chevron-down" size={14} />
          </button>
        </td>
      </tr>
      {closing ? (
        <tr className="pos-confirm">
          <td colSpan={COLUMNS.length + 1}>
            <div className="pos-confirm-row" role="group" aria-label={`Close ${position.symbol}`}>
              {closing.phase === 'queued' ? (
                <span className="tone-ok">Close queued — waiting for the terminal</span>
              ) : (
                <>
                  <span>
                    Close {position.symbol} {side} {position.lots.toFixed(2)} at market?{' '}
                    <span className={signTone(result)}>{signedAmount(result)}</span>
                  </span>
                  {closing.error ? <span className="tone-bad pos-confirm-error">{closing.error}</span> : null}
                  <span className="pos-confirm-actions">
                    <button type="button" className="tab-button" onClick={onCancelClose} disabled={closing.phase === 'pending'}>
                      Cancel
                    </button>
                    <button
                      type="button"
                      className="tab-button pos-confirm-close"
                      onClick={onConfirmClose}
                      disabled={closing.phase === 'pending'}
                    >
                      {closing.phase === 'pending' ? 'Closing…' : 'Close position'}
                    </button>
                  </span>
                </>
              )}
            </div>
          </td>
        </tr>
      ) : null}
      {expanded ? (
        <tr className="pos-detail" id={detailId}>
          <td colSpan={COLUMNS.length + 1}>
            <dl className="pos-detail-list">
              <div>
                <dt>Ticket</dt>
                <dd>{position.ticket}</dd>
              </div>
              <div>
                <dt>Opened</dt>
                <dd>{openedAt(position.openedAt)}</dd>
              </div>
              <div>
                <dt>Swap</dt>
                <dd>{signedAmount(position.swap)}</dd>
              </div>
              <div>
                <dt>Commission</dt>
                <dd>{signedAmount(position.commission)}</dd>
              </div>
            </dl>
          </td>
        </tr>
      ) : null}
    </>
  )
}

/** The open book: one row per position, with a per-row detail drawer. */
export function OpenPositions({
  account,
  harvestEnabled,
  onClose,
  tradingEnabled = true,
  now = Date.now(),
}: {
  account?: Account
  /** Whether the profit-harvest policy is enabled for Veyra-owned positions. */
  harvestEnabled: boolean
  /** Queues a market close; resolves to an error message or undefined. Absent hides the action. */
  onClose?: (ticket: number) => Promise<string | undefined>
  /** The service refuses closes while its trading switch is off. */
  tradingEnabled?: boolean
  /** Clock for time held; tests pin it. */
  now?: number
}) {
  const [expanded, setExpanded] = useState<number>()
  const [closing, setClosing] = useState<Closing>()
  const confirmClose = async (ticket: number) => {
    if (!onClose) return
    setClosing({ ticket, phase: 'pending' })
    const failure = await onClose(ticket)
    setClosing(failure ? { ticket, phase: 'confirm', error: failure } : { ticket, phase: 'queued' })
  }
  const positions = account?.positions ?? []
  let body: ReactNode
  if (!account) {
    body = (
      <div className="pos-loading">
        <Skeleton width="100%" height={14} />
        <Skeleton width="100%" height={14} />
      </div>
    )
  } else if (positions.length === 0) {
    body = <div className="panel-empty">No open positions</div>
  } else {
    body = (
      <div className="pos-scroll" role="region" aria-label="Open positions table" tabIndex={0}>
        <table className="pos-table">
          <colgroup>
            <col className="pos-col-symbol" />
            <col className="pos-col-side" />
            <col className="pos-col-lots" />
            <col className="pos-col-entry" />
            <col className="pos-col-current" />
            <col className="pos-col-sl" />
            <col className="pos-col-tp" />
            <col className="pos-col-pl" />
            <col className="pos-col-opened" />
            <col className="pos-col-harvest" />
            <col className="pos-col-actions" />
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
            {positions.map((position) => (
              <PositionRow
                key={position.ticket}
                position={position}
                harvestEnabled={harvestEnabled}
                expanded={expanded === position.ticket}
                onToggle={() => setExpanded(expanded === position.ticket ? undefined : position.ticket)}
                now={now}
                closing={closing?.ticket === position.ticket ? closing : undefined}
                closable={Boolean(onClose) && position.magic === VEYRA_MAGIC}
                closeBlocked={tradingEnabled ? undefined : 'Trading is disabled'}
                onRequestClose={() => setClosing({ ticket: position.ticket, phase: 'confirm' })}
                onCancelClose={() => setClosing(undefined)}
                onConfirmClose={() => void confirmClose(position.ticket)}
              />
            ))}
          </tbody>
        </table>
      </div>
    )
  }
  return (
    <Panel
      className="pos-panel"
      title="Open positions"
      count={account ? positions.length : undefined}
      actions={account?.positionsTruncated ? <Truncated className="pos-truncated" /> : undefined}
    >
      {body}
    </Panel>
  )
}

/* ---------- 30-day performance ---------- */

type Stat = { label: string; value: ReactNode; tone?: string; sub?: string; subTone?: string }

function stats(performance: Performance, balance: number | undefined): Stat[] {
  const report = performance.report
  if (report.trades === 0) {
    return [
      { label: 'Win rate', value: '—' },
      { label: 'Net P/L', value: '—' },
      { label: 'Closed trades', value: '—' },
    ]
  }
  const net = report.net_profit
  // Return on the balance the window started from; meaningless when the
  // window's result is the whole balance or more.
  const start = balance === undefined ? 0 : balance - net
  const record = [
    plural(report.wins, 'win', 'wins'),
    plural(report.losses, 'loss', 'losses'),
    ...(report.breakeven > 0 ? [`${report.breakeven} flat`] : []),
  ].join(' / ')
  return [
    { label: 'Win rate', value: percent(report.win_rate_percent, 0), sub: record },
    {
      label: 'Net P/L',
      value: signedAmount(net),
      tone: signTone(net),
      sub: start > 0 ? signedPercent((net / start) * 100, 1) : undefined,
      subTone: toneOrMuted(net),
    },
    {
      label: 'Closed trades',
      value: String(report.trades),
      sub: report.expectancy == null ? undefined : `Avg ${signedAmount(report.expectancy)}`,
    },
  ]
}

const STAT_LABELS = ['Win rate', 'Net P/L', 'Closed trades']

/** Realized results over the service's 30-day window: win rate, net P/L and trade count. */
export function PerformanceSummary({
  performance,
  error,
  balance,
}: {
  performance?: Performance
  error?: string
  /** Current broker balance, for the window's return on starting balance. */
  balance?: number
}) {
  const placeholder = error ? <Unavailable className="perf-unavailable" /> : <Skeleton width={96} height={24} />
  const items: Stat[] = performance
    ? stats(performance, balance)
    : STAT_LABELS.map((label) => ({ label, value: placeholder }))
  return (
    <Panel
      className="perf-panel"
      title="30-day performance"
      actions={performance?.truncated ? <Truncated className="perf-truncated" /> : undefined}
    >
      <div className="perf-body">
        <dl className="perf-grid">
          {items.map((item) => (
            <div key={item.label} className="perf-stat">
              <dt className="perf-label">{item.label}</dt>
              <dd className={`perf-value ${item.tone ?? ''}`}>{item.value}</dd>
              <dd className={`perf-sub ${item.subTone ?? ''}`}>{item.sub}</dd>
            </div>
          ))}
        </dl>
      </div>
    </Panel>
  )
}
