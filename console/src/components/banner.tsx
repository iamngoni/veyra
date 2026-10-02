/**
 * The status banner: conditions worth knowing right now (kill switch on,
 * market closed, terminal stale), one compact strip each, under the view's
 * name on every tab.
 *
 * The service decides what is worth saying and in what order; this only
 * presents it, and treats every advisory id alike. It never invents a
 * condition: while the list is empty, not loaded yet or unreadable, nothing
 * shows, since the sidebar status already reports a lost connection.
 *
 * The live region stays mounted (empty, with no footprint) so a strip that
 * appears later is announced. Rows are keyed by the service's stable id and
 * the moving countdown lives only in the hidden tooltip, so a poll that
 * changes nothing announces nothing.
 *
 * A strip's words crawl like a news ticker when they are wider than the
 * strip (see `Ticker`), so a long notice or a schedule of releases reads in
 * full on one line.
 */

import { type CSSProperties, type ReactNode, useId, useLayoutEffect, useRef, useState } from 'react'

import type { Advisories, Advisory, AdvisorySeverity, ScheduleEntry } from '../lib/api'
import { Dot, Hint, type Tone } from './ui'

import '../styles/banner.css'

/** Strips shown before the rest fold behind "+N more". */
const VISIBLE = 2

/** Only the dot and a faint tint carry severity; the words stay plain. */
const SEVERITY: Record<AdvisorySeverity, { tone: Tone; label: string }> = {
  critical: { tone: 'bad', label: 'Critical' },
  warning: { tone: 'warn', label: 'Warning' },
  info: { tone: 'idle', label: 'Info' },
}

const WEEKDAYS = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat']
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']
const DAY_MS = 86_400_000

const pad = (value: number) => String(value).padStart(2, '0')
const clock = (at: Date) => `${pad(at.getHours())}:${pad(at.getMinutes())}`

/**
 * When a condition should end, on the browser's clock: `until 02:00` later
 * today, `until Mon 01:00` within the week, `until 10 Oct 09:30` beyond it,
 * where a weekday alone would be ambiguous.
 */
export function untilLabel(untilMs: number, nowMs: number): string {
  return `until ${moment(untilMs, nowMs)}`
}

/** `02:00` later today, `Mon 01:00` within the week, `10 Oct 09:30` beyond it. */
function moment(ms: number, nowMs: number): string {
  const at = new Date(ms)
  if (at.toDateString() === new Date(nowMs).toDateString()) return clock(at)
  if (ms - nowMs < 6 * DAY_MS) return `${WEEKDAYS[at.getDay()]} ${clock(at)}`
  return `${at.getDate()} ${MONTHS[at.getMonth()]} ${clock(at)}`
}

/**
 * A condition announced ahead, on the browser's clock: `14:00–15:00`, the end
 * by its clock alone when it falls on the start's day.
 */
export function windowLabel(startsMs: number, untilMs: number, nowMs: number): string {
  const sameDay = new Date(startsMs).toDateString() === new Date(untilMs).toDateString()
  return `${moment(startsMs, nowMs)}–${sameDay ? clock(new Date(untilMs)) : moment(untilMs, nowMs)}`
}

/**
 * Time left, rounded up to the minute so it agrees with a wall clock read to
 * the minute: `in 1h 55m`, `in 2d 4h`, `in 12m`; `any moment now` once due.
 */
export function countdown(untilMs: number, nowMs: number): string {
  const total = Math.ceil((untilMs - nowMs) / 60_000)
  if (total <= 0) return 'any moment now'
  const days = Math.floor(total / 1440)
  const hours = Math.floor((total % 1440) / 60)
  const minutes = total % 60
  if (days > 0) return hours > 0 ? `in ${days}d ${hours}h` : `in ${days}d`
  if (hours > 0) return minutes > 0 ? `in ${hours}h ${minutes}m` : `in ${hours}h`
  return `in ${minutes}m`
}

/** The tooltip behind an end time: the full local date and time, and the time left. */
export function untilDetail(untilMs: number, nowMs: number): string {
  const until = new Date(untilMs)
  const date = `${WEEKDAYS[until.getDay()]} ${until.getDate()} ${MONTHS[until.getMonth()]}`
  return `Expected to end ${date}, ${clock(until)} your time — ${countdown(untilMs, nowMs)}.`
}

/** The tooltip behind an announced window: when it starts and ends, and the time to its start. */
export function windowDetail(startsMs: number, untilMs: number, nowMs: number): string {
  const starts = new Date(startsMs)
  const date = `${WEEKDAYS[starts.getDay()]} ${starts.getDate()} ${MONTHS[starts.getMonth()]}`
  return `Expected from ${date}, ${clock(starts)} until ${clock(new Date(untilMs))} your time — starts ${countdown(startsMs, nowMs)}.`
}

/** Ticker speed, pixels per second: slow enough to read in passing. */
const TICKER_SPEED = 48

/** Space between the end of a strip's words and their next pass, pixels. */
const TICKER_GAP = 64

/**
 * Words that crawl right to left, like a news ticker, while they are wider
 * than their strip, and stand still while they fit. Hover or keyboard focus
 * pauses them; with reduced motion they stand still, faded at the edge, and
 * `text` stays on hover either way. The second pass that makes the loop
 * seamless is hidden from assistive technology, so the words are read once.
 */
function Ticker({ text, children }: { text: string; children: ReactNode }) {
  const viewport = useRef<HTMLSpanElement>(null)
  const content = useRef<HTMLSpanElement>(null)
  // The words' own width while it exceeds the strip; 0 while they fit.
  const [width, setWidth] = useState(0)

  useLayoutEffect(() => {
    const measure = () => {
      const natural = content.current?.offsetWidth ?? 0
      const room = viewport.current?.clientWidth ?? 0
      setWidth(natural > room ? natural : 0)
    }
    measure()
    if (typeof ResizeObserver === 'undefined') return undefined
    const observer = new ResizeObserver(measure)
    for (const element of [viewport.current, content.current]) if (element) observer.observe(element)
    return () => observer.disconnect()
  }, [text])

  const moving = width > 0
  const distance = width + TICKER_GAP
  const style = moving
    ? ({
        '--ticker-gap': `${TICKER_GAP}px`,
        '--ticker-distance': `${distance}px`,
        '--ticker-duration': `${(distance / TICKER_SPEED).toFixed(2)}s`,
      } as CSSProperties)
    : undefined
  return (
    <span
      ref={viewport}
      className={`ticker${moving ? ' is-moving' : ''}`}
      style={style}
      title={moving ? text : undefined}
      tabIndex={moving ? 0 : undefined}
    >
      <span className="ticker-track">
        <span className="ticker-copy">
          <span ref={content} className="ticker-content">
            {children}
          </span>
        </span>
        {moving ? (
          <span className="ticker-copy" aria-hidden="true">
            <span className="ticker-content">{children}</span>
          </span>
        ) : null}
      </span>
    </span>
  )
}

function AdvisoryRow({ item, now }: { item: Advisory; now: number }) {
  // A severity this console does not know yet reads as information rather
  // than breaking the row.
  const level: AdvisorySeverity = Object.hasOwn(SEVERITY, item.severity) ? item.severity : 'info'
  const { tone, label } = SEVERITY[level]
  const until = item.untilMs ?? undefined
  // Still ahead, the strip shows the whole window; once in effect, its end.
  const starts = until !== undefined && item.startsMs != null && item.startsMs > now ? item.startsMs : undefined
  const schedule: ScheduleEntry[] = Array.isArray(item.schedule) ? item.schedule : []
  const text = [
    item.title,
    ...schedule.map((entry) => `${moment(entry.atMs, now)} ${entry.label}`),
    item.detail,
  ]
    .filter(Boolean)
    .join(' · ')
  return (
    <li className={`banner-row is-${level}`}>
      <Dot tone={tone} label={label} />
      <Ticker text={text}>
        <strong className="banner-title">{item.title}</strong>
        {schedule.map((entry) => (
          <span className="banner-entry" key={`${entry.atMs}:${entry.label}`}>
            <time dateTime={new Date(entry.atMs).toISOString()}>{moment(entry.atMs, now)}</time> {entry.label}
          </span>
        ))}
        {item.detail ? <span className="banner-detail">{item.detail}</span> : null}
      </Ticker>
      {until !== undefined && starts !== undefined ? (
        <span className="banner-until">
          {windowLabel(starts, until, now)}
          <Hint text={windowDetail(starts, until, now)} label="when this starts and ends" />
        </span>
      ) : until !== undefined ? (
        <span className="banner-until">
          {untilLabel(until, now)}
          <Hint text={untilDetail(until, now)} label="when this ends" />
        </span>
      ) : null}
    </li>
  )
}

/**
 * The advisories as strips, most severe first, at most two before a
 * "+N more" disclosure. Renders no strip while there is nothing to say or
 * the last read failed; no dismiss, since a strip leaves when its condition
 * clears.
 */
export function StatusBanner({ advisories, error }: { advisories?: Advisories; error?: string }) {
  const [expanded, setExpanded] = useState(false)
  const listId = useId()
  const served = advisories?.items
  const items = error === undefined && Array.isArray(served) ? served : []
  const folded = items.length - VISIBLE
  const shown = expanded || folded <= 0 ? items : items.slice(0, VISIBLE)
  const now = Date.now()

  return (
    <div className={`banner${items.length > 0 ? ' is-shown' : ''}`}>
      <div className="banner-live" role="status" aria-live="polite">
        {items.length > 0 ? (
          <ul className="banner-list" id={listId} aria-label="Notices">
            {shown.map((item) => (
              <AdvisoryRow key={item.id} item={item} now={now} />
            ))}
          </ul>
        ) : null}
      </div>
      {/* Outside the live region: its own label changing is not news. */}
      {folded > 0 ? (
        <button
          type="button"
          className="banner-more"
          aria-expanded={expanded}
          aria-controls={listId}
          onClick={() => setExpanded((open) => !open)}
        >
          {expanded ? 'Show less' : `+${folded} more`}
        </button>
      ) : null}
    </div>
  )
}
