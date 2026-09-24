/**
 * Overview right rail: autopilot state, the recent-activity timeline and the
 * two emergency risk switches.
 *
 * Everything shown is read from live status and the event feed; a value the
 * service has not reported yet renders as a placeholder, never a guess. The
 * switches only ever request a change through an inline confirmation.
 */

import { useLayoutEffect, useRef, useState } from 'react'

import type { ClosedTrade, FeedEvent, RiskPolicy, RiskPolicyPatch, Status } from '../lib/api'
import { activityDetail, activityTitle, activityTone, isRoutine, signedAmount } from '../lib/format'
import { Dot, Hint, Panel, Skeleton, Toggle, type Tone } from './ui'

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

const pad = (value: number) => String(value).padStart(2, '0')

/** Local wall-clock `17:56`. */
function hourMinute(ms: number): string {
  const date = new Date(ms)
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`
}

/**
 * Local `23 Sep 2026, 17:56`. Built by hand because `Intl` month names vary by
 * runtime (`Sept` in recent en-GB data).
 */
function dateTime(ms: number): string {
  const date = new Date(ms)
  return `${date.getDate()} ${MONTHS[date.getMonth()]} ${date.getFullYear()}, ${hourMinute(ms)}`
}

/* ---------- autopilot ---------- */

/** Consecutive failed decisions after which the loop counts as not deciding. */
const FAILING_AFTER = 2

/** Autopilot summary: run state, cadence, the latest decision and its outcome. */
export function AutopilotCard({
  status,
  events,
  loading = false,
}: {
  status?: Status
  /** Live feed, newest first; the latest decision is read from it. */
  events: FeedEvent[]
  /** The feed has not answered yet, so an absent decision is unknown, not none. */
  loading?: boolean
}) {
  const autopilot = status?.autopilot
  const on = autopilot?.enabled === true
  const decisions = status?.decisions
  const failing = (decisions?.consecutiveFailures ?? 0) >= FAILING_AFTER
  const decision = events.find((event) => event.kind === 'proposal_evaluated')

  // The dot carries the state; the word stays in the secondary voice unless
  // the loop needs attention.
  const state: { tone: Tone; text: string; className: string } | undefined = !status
    ? undefined
    : failing
      ? { tone: 'bad', text: 'Not deciding', className: 'tone-bad' }
      : on
        ? { tone: 'ok', text: 'Running', className: 'tone-muted' }
        : { tone: 'off', text: 'Off', className: 'tone-faint' }

  let outcome: { title: string; note?: string } | undefined
  if (failing) outcome = { title: 'Not deciding', note: decisions?.lastFailure ?? undefined }
  else if (!on) outcome = { title: 'Off' }
  else if (decision) outcome = { title: activityTitle(decision), note: activityDetail(decision) }

  return (
    <Panel
      title="Autopilot"
      className="ap-panel"
      divided
      actions={
        state ? (
          <span className={`ap-state ${state.className}`}>
            <Dot tone={state.tone} />
            {state.text}
          </span>
        ) : undefined
      }
    >
      <dl className="ap-rows">
        <div className="ap-row">
          <dt>Cadence</dt>
          <dd>
            {!status ? (
              <Skeleton width={110} height={14} />
            ) : on && autopilot ? (
              `${autopilot.interval_secs} seconds · ${autopilot.timeframe}`
            ) : (
              '—'
            )}
          </dd>
        </div>
        <div className="ap-row">
          <dt>Last decision</dt>
          <dd>
            {decision ? (
              <time dateTime={new Date(decision.at_ms).toISOString()}>{dateTime(decision.at_ms)}</time>
            ) : loading ? (
              <Skeleton width={130} height={14} />
            ) : (
              '—'
            )}
          </dd>
        </div>
        <div className="ap-row">
          <dt>Status</dt>
          <dd>
            {!status || (!outcome && loading) ? (
              <Skeleton width={150} height={14} />
            ) : outcome ? (
              <>
                <span className="ap-outcome">{outcome.title}</span>
                {outcome.note ? (
                  <span className="ap-note" title={outcome.note}>
                    {outcome.note}
                  </span>
                ) : null}
              </>
            ) : (
              '—'
            )}
          </dd>
        </div>
      </dl>
    </Panel>
  )
}

/* ---------- recent activity ---------- */

/** Loop plumbing that says nothing about what the autopilot decided. */
const PLUMBING = new Set(['agent_turn', 'agent_tool_called', 'command_queued', 'command_completed'])

/** How many rows the overview previews; the Activity tab has the rest. */
const PREVIEW_ROWS = 3

/** Most rows offered when the layout gives the list room for more. */
const MOST_ROWS = 6

/**
 * How many rows of a list fit inside it, re-measured as it resizes.
 *
 * Where the overview fills the window, the list is as tall as the space left
 * in the rail, so rows that would be cut off are hidden whole rather than
 * sliced. Elsewhere the stylesheet shows the preview count and every
 * rendered row fits. Without layout (a test DOM) nothing is measured.
 */
function useFittingRows(signature: string) {
  const ref = useRef<HTMLOListElement>(null)
  const [fit, setFit] = useState(MOST_ROWS)
  useLayoutEffect(() => {
    const list = ref.current
    if (!list) return
    const measure = () => {
      const limit = list.clientHeight
      if (limit === 0) return
      let count = 0
      for (const row of Array.from(list.children) as HTMLElement[]) {
        // A row the stylesheet leaves out has no box, and neither do the rest.
        if (row.offsetHeight === 0 || row.offsetTop + row.offsetHeight > limit + 1) break
        count++
      }
      setFit(Math.max(1, count))
    }
    measure()
    if (typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(measure)
    observer.observe(list)
    return () => observer.disconnect()
  }, [signature])
  return { ref, fit }
}

/** `17:56` for a moment today, `23 Sep` for any other day. */
function dayOrTime(ms: number, now: number): string {
  const date = new Date(ms)
  return date.toDateString() === new Date(now).toDateString()
    ? hourMinute(ms)
    : `${date.getDate()} ${MONTHS[date.getMonth()]}`
}

type DigestRow = { key: string; atMs: number; title: string; detail?: string; tone: Tone }

/**
 * One close, worded and toned the same whichever source reported it. The net
 * is rounded as displayed, so a close that shows as 0.00 does not read as a gain.
 */
function closeRow(key: string, atMs: number, symbol: string, kind: string, lots: number, net: number): DigestRow {
  const rounded = Number(net.toFixed(2))
  return {
    key,
    atMs,
    title: 'Position closed',
    detail: `${symbol} ${kind === 'buy' ? 'Long' : 'Short'} ${lots.toFixed(2)} · ${signedAmount(rounded)}`,
    tone: rounded > 0 ? 'ok' : rounded < 0 ? 'bad' : 'idle',
  }
}

function feedRow(event: FeedEvent): DigestRow {
  const { symbol, kind, lots, profit } = event.payload
  if (
    event.kind === 'position_closed' &&
    typeof symbol === 'string' &&
    (kind === 'buy' || kind === 'sell') &&
    typeof lots === 'number' &&
    typeof profit === 'number'
  ) {
    return closeRow(`event-${event.seq}`, event.at_ms, symbol, kind, lots, profit)
  }
  return {
    key: `event-${event.seq}`,
    atMs: event.at_ms,
    title: activityTitle(event),
    detail: activityDetail(event),
    tone: activityTone(event),
  }
}

function tradeRow(trade: ClosedTrade): DigestRow {
  const net = trade.profit + trade.swap + trade.commission
  return closeRow(`trade-${trade.ticket}`, trade.closeTime * 1000, trade.symbol, trade.kind, trade.lots, net)
}

/**
 * The three newest things that matter: decisions, failures and closes. Closes
 * come from the feed and from the venue's trade history, since the feed ring
 * is short. A close present in both is shown once, from the history: the
 * feed records the last profit it saw, the history the realized net.
 */
export function RecentActivity({
  events,
  trades,
  connected,
  loading = false,
  onViewAll,
}: {
  events: FeedEvent[]
  /** Closed Veyra orders from /performance, newest first. */
  trades?: ClosedTrade[]
  connected: boolean
  /** Neither the feed nor the history has answered yet. */
  loading?: boolean
  onViewAll: () => void
}) {
  const history = trades ?? []
  const settled = new Set(history.map((trade) => trade.ticket))
  const rows = [
    ...events
      .filter((event) => !isRoutine(event) && !PLUMBING.has(event.kind))
      // Tickets can arrive as strings in older payloads.
      .filter((event) => event.kind !== 'position_closed' || !settled.has(Number(event.payload.ticket)))
      .map(feedRow),
    ...history.map(tradeRow),
  ]
    // Stable, so a feed row wins a tie with a trade row.
    .sort((left, right) => right.atMs - left.atMs)
    .slice(0, MOST_ROWS)
  const now = Date.now()
  const { ref, fit } = useFittingRows(rows.map((row) => row.key).join('|'))

  return (
    <Panel
      title="Recent activity"
      className="act-panel"
      divided
      actions={
        <>
          {/* An empty list already says it is reconnecting; say it once. */}
          {!connected && rows.length > 0 ? <span className="act-reconnecting">Reconnecting</span> : null}
          <button type="button" className="panel-link" onClick={onViewAll}>
            View all
          </button>
        </>
      }
    >
      {rows.length === 0 && loading ? (
        <ol className="act-list" aria-hidden="true">
          {Array.from({ length: PREVIEW_ROWS }, (_, index) => (
            <li key={index} className="act-row">
              <span className="act-time">
                <Skeleton width={32} height={12} />
              </span>
              <span className="act-mark">
                <Dot tone="off" />
              </span>
              <div className="act-text">
                <Skeleton width="70%" height={12} />
              </div>
            </li>
          ))}
        </ol>
      ) : rows.length === 0 ? (
        <p className="panel-empty act-empty">{connected ? 'No recent activity' : 'Reconnecting'}</p>
      ) : (
        <ol className="act-list" ref={ref}>
          {rows.map((row, index) => (
            <li key={row.key} className={`act-row${index >= fit ? ' is-clipped' : ''}`}>
              <time className="act-time" dateTime={new Date(row.atMs).toISOString()}>
                {dayOrTime(row.atMs, now)}
              </time>
              <span className="act-mark">
                <Dot tone={row.tone} />
              </span>
              <div className="act-text">
                <p className="act-title">{row.title}</p>
                {row.detail ? (
                  <p className="act-detail" title={row.detail}>
                    {row.detail}
                  </p>
                ) : null}
              </div>
            </li>
          ))}
        </ol>
      )}
    </Panel>
  )
}

/* ---------- risk controls ---------- */

type SwitchKey = 'killSwitch' | 'allowTradingWithoutJev'

/** Kill switch and judge bypass, each changed only through an inline confirm. */
export function RiskControls({
  policy,
  jevHealthy,
  onApply,
}: {
  policy?: RiskPolicy
  /** Whether the judge answered recently; false drives the degraded warning. */
  jevHealthy?: boolean
  /** Applies a patch; resolves to an error message or undefined on success. */
  onApply?: (patch: RiskPolicyPatch) => Promise<string | undefined>
}) {
  const [confirming, setConfirming] = useState<SwitchKey>()
  const [pending, setPending] = useState<SwitchKey>()
  const [error, setError] = useState<{ key: SwitchKey; message: string }>()

  const halted = policy?.killSwitch ?? false
  const bypass = policy?.allowTradingWithoutJev ?? false
  const disabled = !policy || !onApply || pending !== undefined

  const request = (key: SwitchKey) => {
    setError(undefined)
    setConfirming(confirming === key ? undefined : key)
  }

  const cancel = () => setConfirming(undefined)

  const submit = async (key: SwitchKey, next: boolean) => {
    // The toggles are disabled without a handler, so no confirm row can be open.
    const apply = onApply as NonNullable<typeof onApply>
    setPending(key)
    let failure: string | undefined
    try {
      failure = await apply({ [key]: next })
    } catch (cause) {
      failure = cause instanceof Error ? cause.message : String(cause)
    }
    setPending(undefined)
    setConfirming(undefined)
    if (failure) setError({ key, message: failure })
  }

  const degraded = jevHealthy === false

  return (
    <Panel title="Risk controls" className="rc-panel" divided>
      <div className="rc-list">
        <RiskSwitch
          title="Kill switch"
          description={
            halted ? 'New orders refused; open positions stay open.' : 'Blocks new orders; open positions stay open.'
          }
          checked={halted}
          tone="bad"
          disabled={disabled}
          prompt={halted ? 'Release kill switch?' : 'Engage kill switch?'}
          confirming={confirming === 'killSwitch'}
          pending={pending === 'killSwitch'}
          error={error?.key === 'killSwitch' ? error.message : undefined}
          onRequest={() => request('killSwitch')}
          onCancel={cancel}
          onConfirm={() => void submit('killSwitch', !halted)}
        />
        <RiskSwitch
          title="Judge bypass"
          info="Lets trading continue while the judge cannot answer."
          description={
            bypass ? 'Trading continues when the judge cannot answer.' : 'Trading pauses when the judge cannot answer.'
          }
          checked={bypass}
          tone="warn"
          disabled={disabled}
          prompt={bypass ? 'Disable judge bypass?' : 'Enable judge bypass?'}
          confirming={confirming === 'allowTradingWithoutJev'}
          pending={pending === 'allowTradingWithoutJev'}
          error={error?.key === 'allowTradingWithoutJev' ? error.message : undefined}
          warning={
            degraded
              ? bypass
                ? 'Judge not answering — trading without it.'
                : 'Judge not answering — decisions paused.'
              : undefined
          }
          onRequest={() => request('allowTradingWithoutJev')}
          onCancel={cancel}
          onConfirm={() => void submit('allowTradingWithoutJev', !bypass)}
        />
      </div>
    </Panel>
  )
}

function RiskSwitch({
  title,
  info,
  description,
  checked,
  tone,
  disabled,
  prompt,
  confirming,
  pending,
  error,
  warning,
  onRequest,
  onCancel,
  onConfirm,
}: {
  title: string
  /** Hover explanation behind an info mark after the title. */
  info?: string
  description: string
  checked: boolean
  tone: 'bad' | 'warn'
  disabled: boolean
  prompt: string
  confirming: boolean
  pending: boolean
  error?: string
  warning?: string
  onRequest: () => void
  onCancel: () => void
  onConfirm: () => void
}) {
  return (
    <div className="rc-row">
      <div className="rc-head">
        <h3 className="rc-title">
          {title}
          {info ? <Hint text={info} label={title} size={14} /> : null}
        </h3>
        <Toggle checked={checked} label={title} tone={tone} disabled={disabled} onClick={onRequest} />
      </div>
      <p className="rc-desc">{description}</p>
      {warning ? <p className="rc-warning">{warning}</p> : null}
      {confirming ? (
        <div className="rc-confirm" role="group" aria-label={prompt}>
          <span>{prompt}</span>
          <span className="rc-confirm-actions">
            <button type="button" className="rc-cancel" onClick={onCancel} disabled={pending}>
              Cancel
            </button>
            <button type="button" className={`rc-apply is-${tone}`} onClick={onConfirm} disabled={pending}>
              {pending ? 'Applying…' : 'Confirm'}
            </button>
          </span>
        </div>
      ) : null}
      {error ? (
        <p className="rc-error" role="alert">
          {error}
        </p>
      ) : null}
    </div>
  )
}
