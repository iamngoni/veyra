/**
 * The overview's main chart: Veyra's balance growth over a range (the
 * default), or one instrument's close line.
 *
 * Data boundary: the performance view draws `balanceGrowth` — the venue's
 * closed Veyra orders walked back from the broker's current balance (see
 * lib/growth.ts). It is a realized balance line, not equity, and it carries no
 * deposits, withdrawals or manual trades. The market view draws the served
 * candle closes as they are. Nothing is sampled, smoothed or filled in: data
 * that has not arrived is a skeleton and a failed poll reads "Unavailable".
 *
 * The plot is SVG drawn at the measured pixel width, so axis text keeps its
 * real size instead of being stretched with the frame.
 */

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'

import type { CandleSeries, ClosedTrade } from '../lib/api'
import { amount, signedAmount, signedPercent } from '../lib/format'
import { balanceGrowth, tradeNet } from '../lib/growth'
import { Icon, Segmented, Skeleton, signTone } from './ui'

export type ChartMode = 'performance' | 'market'
export type PerformanceRange = 7 | 30 | 90 | 365
export type MarketTimeframe = 'M15' | 'H1' | 'H4' | 'D1' | 'W1'

export const MARKET_BARS = 120

const DAY_MS = 86_400_000
const MINUTE_MS = 60_000

const RANGES: ReadonlyArray<{ value: PerformanceRange; label: string }> = [
  { value: 7, label: '7D' },
  { value: 30, label: '30D' },
  { value: 90, label: '90D' },
  { value: 365, label: '1Y' },
]

const TIMEFRAMES: ReadonlyArray<{ value: MarketTimeframe; label: string }> = (
  ['M15', 'H1', 'H4', 'D1', 'W1'] as const
).map((value) => ({ value, label: value }))

/* ---------- plot geometry (px) ---------- */

/** Panel edge to the plot's left frame. */
const PAD_LEFT = 18
/** The value tag's right end to the panel edge. */
const PAD_RIGHT = 14
/** Space between the last point and the right axis, bridged by a leader. */
const LINE_INSET = 8
/** Right axis to the start of its labels. */
const LABEL_GAP = 12
/** Narrowest label column (axis to panel edge), so the axis holds still across views. */
const MIN_GUTTER = 71
/** Height under the plot for the time labels. */
const AXIS_BAND = 42
const TICK = 5
const TAG_HEIGHT = 20
const TAG_POINT = 6
/** About one time label per this many pixels (fewer on narrow panels). */
const X_LABEL_SPACING = 90
const X_LABEL_SPACING_NARROW = 72
/** Below this width the plot shortens and labels thin further. */
const NARROW = 560
/** jsdom and the first server render report no layout width. */
const FALLBACK_WIDTH = 960

/* ---------- pure helpers ---------- */

/** Decimal places a number is written with, e.g. `1.15978` → 5. */
export function decimalsOf(value: number): number {
  // Rounding first drops binary noise such as 1.1149000000000001.
  const text = String(Number(value.toFixed(8)))
  const dot = text.indexOf('.')
  return dot === -1 ? 0 : text.length - dot - 1
}

/** The instrument's quoted precision: the widest decimal count among prices. */
export function priceDecimals(prices: ReadonlyArray<number>): number {
  return prices.reduce((widest, price) => Math.max(widest, decimalsOf(price)), 0)
}

/**
 * Round-number ticks inside `[lo, hi]`: steps of 1, 2, 2.5 or 5 at the span's
 * magnitude, whichever lands closest to `target` ticks.
 */
export function niceTicks(lo: number, hi: number, target: number): { ticks: number[]; step: number } {
  const magnitude = 10 ** Math.floor(Math.log10((hi - lo) / target))
  let best = { ticks: [] as number[], step: 0 }
  for (const factor of [1, 2, 2.5, 5, 10]) {
    const step = Number((factor * magnitude).toPrecision(3))
    const digits = decimalsOf(step)
    const ticks: number[] = []
    for (let index = Math.ceil(lo / step); index * step <= hi; index++) ticks.push(Number((index * step).toFixed(digits)))
    if (best.step === 0 || Math.abs(ticks.length - target) < Math.abs(best.ticks.length - target)) best = { ticks, step }
  }
  return best
}

type TimeStep = { unit: 'minute' | 'day' | 'month'; size: number }

const MINUTE_STEPS: TimeStep[] = [5, 10, 15, 30, 60, 120, 180, 240, 360, 720].map((size) => ({ unit: 'minute', size }))
const DAY_STEPS: TimeStep[] = [1, 2, 3, 7, 14].map((size) => ({ unit: 'day', size }))
const MONTH_STEPS: TimeStep[] = [1, 2, 3, 6, 12].map((size) => ({ unit: 'month', size }))
const LONG_STEPS = [...DAY_STEPS, ...MONTH_STEPS]

/** Local-calendar bucket a moment falls in; a change of bucket is a tick. */
function bucket(ms: number, step: TimeStep): number {
  const date = new Date(ms)
  if (step.unit === 'month') return Math.floor((date.getFullYear() * 12 + date.getMonth()) / step.size)
  const local = ms - date.getTimezoneOffset() * MINUTE_MS
  if (step.unit === 'minute') return Math.floor(local / (step.size * MINUTE_MS))
  // Week-sized steps start on Mondays; the epoch was a Thursday.
  return Math.floor((Math.floor(local / DAY_MS) + (step.size >= 7 ? 3 : 0)) / step.size)
}

/** First moment of a bucket, in local time. */
function bucketStart(index: number, step: TimeStep): number {
  if (step.unit === 'month') {
    const month = index * step.size
    return new Date(Math.floor(month / 12), month % 12, 1).getTime()
  }
  if (step.unit === 'minute') {
    const local = index * step.size * MINUTE_MS
    return local + new Date(local).getTimezoneOffset() * MINUTE_MS
  }
  const day = new Date((index * step.size - (step.size >= 7 ? 3 : 0)) * DAY_MS)
  return new Date(day.getUTCFullYear(), day.getUTCMonth(), day.getUTCDate()).getTime()
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec']

function clock(date: Date): string {
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`
}

function day(date: Date): string {
  return `${MONTHS[date.getMonth()]} ${date.getDate()}`
}

/** Axis text: `14:00`, `Sep 18`, or `Oct` with January shown as its year. */
function tickLabel(ms: number, step: TimeStep): string {
  const date = new Date(ms)
  if (step.unit === 'minute') return clock(date)
  if (step.unit === 'day') return day(date)
  return date.getMonth() === 0 ? String(date.getFullYear()) : MONTHS[date.getMonth()]
}

/** Tooltip time: `Sep 18, 14:32`, or `Sep 18, 2026` for daily and weekly bars. */
function pointTime(ms: number, daily = false): string {
  const date = new Date(ms)
  return `${day(date)}, ${daily ? date.getFullYear() : clock(date)}`
}

/** The first step whose tick count fits, else the widest. */
function fitting(steps: TimeStep[], count: (step: TimeStep) => number, maxCount: number): TimeStep {
  return steps.find((step) => count(step) <= maxCount) ?? steps[steps.length - 1]
}

export type TimeTick = { atMs: number; label: string }

/**
 * Calendar-aligned ticks after `fromMs` up to `toMs`, at most `maxCount`
 * where any step allows it. Spans under two days are labelled with clock
 * times, longer ones with dates, so a step never splits a day.
 */
export function timeTicks(fromMs: number, toMs: number, maxCount: number): TimeTick[] {
  const steps = toMs - fromMs < 2 * DAY_MS ? MINUTE_STEPS : LONG_STEPS
  const step = fitting(steps, (candidate) => bucket(toMs, candidate) - bucket(fromMs, candidate), maxCount)
  const ticks: TimeTick[] = []
  for (let index = bucket(fromMs, step) + 1; index <= bucket(toMs, step); index++) {
    const atMs = bucketStart(index, step)
    ticks.push({ atMs, label: tickLabel(atMs, step) })
  }
  return ticks
}

/**
 * Ticks for an evenly spaced bar series: the bars that open a new calendar
 * bucket. Bars sit side by side across weekend gaps, so over days the ticks
 * are every n-th trading day rather than every n calendar days, which keeps
 * the labels evenly spaced.
 */
export function barTicks(timesMs: ReadonlyArray<number>, maxCount: number): Array<{ index: number; label: string }> {
  const opens = (step: TimeStep) =>
    timesMs.flatMap((ms, index) => (index > 0 && bucket(ms, step) !== bucket(timesMs[index - 1], step) ? [index] : []))
  const label = (indices: number[], step: TimeStep) =>
    indices.map((index) => ({ index, label: tickLabel(timesMs[index], step) }))

  if (timesMs[timesMs.length - 1] - timesMs[0] < 2 * DAY_MS) {
    const step = fitting(MINUTE_STEPS, (candidate) => opens(candidate).length, maxCount)
    return label(opens(step), step)
  }
  const days = opens(DAY_STEPS[0])
  for (const every of [1, 2, 3, 5, 10]) {
    const kept = days.filter((_, position) => position % every === 0)
    if (kept.length <= maxCount) return label(kept, DAY_STEPS[0])
  }
  const step = fitting(MONTH_STEPS, (candidate) => opens(candidate).length, maxCount)
  return label(opens(step), step)
}

/** Approximate advance of 12px Avenir Next digits, for sizing the label column. */
function textWidth(text: string): number {
  let width = 0
  for (const char of text) width += char === '.' || char === ',' ? 3.4 : 7
  return width
}

/* ---------- plot ---------- */

type PlotPoint = { x: number; value: number; atMs: number; trade?: ClosedTrade }

type PlotSpec = {
  label: string
  points: PlotPoint[]
  xDomain: [number, number]
  xTicks: (maxCount: number) => Array<{ x: number; label: string }>
  /** Precision of the tag and y labels. */
  decimals: number
  /** Hold each value until the next point (a balance moves only at a close). */
  stepped: boolean
  up: boolean
  tip: (point: PlotPoint) => ReactNode
}

function LinePlot({ spec, width, height }: { spec: PlotSpec; width: number; height: number }) {
  const [hover, setHover] = useState<number | null>(null)
  const { points, xDomain, decimals } = spec

  const values = points.map((point) => point.value)
  const min = Math.min(...values)
  const max = Math.max(...values)
  // A flat line still gets a band around it rather than collapsing to zero height.
  const pad = max > min ? (max - min) * 0.08 : Math.abs(max) * 0.01 || 1
  const lo = min - pad
  const hi = max + pad
  const { ticks, step } = niceTicks(lo, hi, height < 220 ? 4 : 5.5)
  const labelDigits = Math.max(decimals, decimalsOf(step))

  const last = points[points.length - 1]
  const tagText = amount(last.value, decimals)
  const tagWidth = TAG_POINT + 6 + textWidth(tagText) + 7
  const labelWidth = Math.max(...ticks.map((tick) => textWidth(amount(tick, labelDigits))))
  const axisX = Math.round(width - Math.max(MIN_GUTTER, Math.max(tagWidth + 1, LABEL_GAP + labelWidth) + PAD_RIGHT)) + 0.5
  const bottom = height + 0.5
  const left = PAD_LEFT + 0.5
  const right = axisX - LINE_INSET
  const span = xDomain[1] - xDomain[0]
  const sx = (x: number) => (span > 0 ? left + ((x - xDomain[0]) / span) * (right - left) : right)
  const sy = (value: number) => 0.5 + ((hi - value) / (hi - lo)) * height

  const xs = points.map((point) => sx(point.x))
  const ys = points.map((point) => sy(point.value))
  const path = xs
    .map((x, index) => {
      const y = ys[index].toFixed(1)
      if (index === 0) return `M${x.toFixed(1)} ${y}`
      return spec.stepped ? `H${x.toFixed(1)}V${y}` : `L${x.toFixed(1)} ${y}`
    })
    .join('')
  const lastX = xs[xs.length - 1]
  const lastY = ys[ys.length - 1]
  const tagY = Math.min(Math.max(lastY, TAG_HEIGHT / 2), height - TAG_HEIGHT / 2)

  const spacing = width < NARROW ? X_LABEL_SPACING_NARROW : X_LABEL_SPACING
  const xTicks: Array<{ x: number; label: string }> = []
  for (const tick of spec.xTicks(Math.max(1, Math.floor((right - left) / spacing)))) {
    const x = Math.round(sx(tick.x)) + 0.5
    const previous = xTicks[xTicks.length - 1]
    // Labels are about 40px wide: keep them clear of the frame and of each other.
    if (x < left + 16 || x > axisX - 18 || (previous && x - previous.x < 56)) continue
    xTicks.push({ x, label: tick.label })
  }
  // Ticks hugging the frame would be clipped or crowd the time labels.
  const yTicks = ticks
    .map((value) => ({ y: Math.round(sy(value)) + 0.5, label: amount(value, labelDigits) }))
    .filter((tick) => tick.y >= 8 && tick.y <= height - 4)

  const active = hover === null ? undefined : points[hover]
  const hoverX = active ? xs[hover!] : 0
  const track = (clientX: number, target: Element) => {
    const x = clientX - target.getBoundingClientRect().left
    let nearest = 0
    for (let index = 1; index < xs.length; index++) {
      if (Math.abs(xs[index] - x) < Math.abs(xs[nearest] - x)) nearest = index
    }
    setHover(nearest)
  }
  const tone = spec.up ? 'is-up' : 'is-down'

  return (
    <>
      <svg
        className="chart-svg"
        width={width}
        height={height + AXIS_BAND}
        viewBox={`0 0 ${width} ${height + AXIS_BAND}`}
        role="img"
        aria-label={spec.label}
        onMouseMove={(event) => track(event.clientX, event.currentTarget)}
        onMouseLeave={() => setHover(null)}
        onTouchStart={(event) => track(event.touches[0].clientX, event.currentTarget)}
        onTouchMove={(event) => track(event.touches[0].clientX, event.currentTarget)}
        onTouchEnd={() => setHover(null)}
      >
        <g className="chart-grid">
          <line x1={left} x2={axisX} y1={0.5} y2={0.5} />
          <line x1={left} x2={left} y1={0.5} y2={bottom} />
          {yTicks.map((tick) => (
            <line key={`y${tick.label}`} x1={left} x2={axisX} y1={tick.y} y2={tick.y} />
          ))}
          {xTicks.map((tick) => (
            <line key={`x${tick.x}`} x1={tick.x} x2={tick.x} y1={0.5} y2={bottom} />
          ))}
        </g>
        <g className="chart-axis">
          <line x1={left} x2={axisX} y1={bottom} y2={bottom} />
          <line x1={axisX} x2={axisX} y1={0.5} y2={bottom} />
          {yTicks.map((tick) => (
            <line key={`y${tick.label}`} x1={axisX} x2={axisX + TICK} y1={tick.y} y2={tick.y} />
          ))}
          {xTicks.map((tick) => (
            <line key={`x${tick.x}`} x1={tick.x} x2={tick.x} y1={bottom} y2={bottom + TICK} />
          ))}
        </g>
        <g className="chart-labels">
          {yTicks
            .filter((tick) => Math.abs(tick.y - tagY) >= TAG_HEIGHT / 2 + 5)
            .map((tick) => (
              <text key={tick.label} className="chart-ylabel" x={axisX + LABEL_GAP} y={tick.y + 4}>
                {tick.label}
              </text>
            ))}
          {xTicks.map((tick) => (
            <text key={tick.x} className="chart-xlabel" x={tick.x} y={bottom + 22} textAnchor="middle">
              {tick.label}
            </text>
          ))}
        </g>
        {points.length > 1 ? (
          <path className={`chart-line ${tone}`} d={path} />
        ) : (
          <circle className={`chart-point ${tone}`} cx={lastX} cy={lastY} r={3} />
        )}
        <line className={`chart-leader ${tone}`} x1={lastX} x2={axisX} y1={lastY} y2={lastY} />
        {active ? (
          <g className="chart-hover">
            <line className="chart-cross" x1={hoverX} x2={hoverX} y1={0.5} y2={bottom} />
            <circle className={`chart-point ${tone}`} cx={hoverX} cy={ys[hover!]} r={3.5} />
          </g>
        ) : null}
        <g className={`chart-tag ${tone}`}>
          <path
            d={`M${axisX + 0.5} ${tagY}l${TAG_POINT} ${-TAG_HEIGHT / 2}h${tagWidth - TAG_POINT - 3}q3 0 3 3v${TAG_HEIGHT - 6}q0 3 -3 3h${-(tagWidth - TAG_POINT - 3)}z`}
          />
          <text className="chart-tag-text" x={axisX + TAG_POINT + 6.5} y={tagY + 4}>
            {tagText}
          </text>
        </g>
      </svg>
      {active ? (
        // The tip sits beside the crosshair, on whichever side has room.
        <div className="chart-tip" style={hoverX > width * 0.6 ? { right: width - hoverX + 12 } : { left: hoverX + 12 }}>
          {spec.tip(active)}
        </div>
      ) : null}
    </>
  )
}

/* ---------- views ---------- */

type View = { state: 'loading' } | { state: 'error' } | { state: 'ready'; stats: ReactNode; plot: PlotSpec }

type ChartProps = Parameters<typeof ChartPanel>[0]

function Stat({ label, value, tone = '' }: { label: string; value: string; tone?: string }) {
  return (
    <span className="chart-stat">
      <span className="chart-stat-label">{label}</span>
      <span className={`chart-stat-value ${tone}`}>{value}</span>
    </span>
  )
}

function counted(count: number, noun: string, open = false): string {
  return `${count}${open ? '+' : ''} ${noun}${count === 1 ? '' : 's'}`
}

function changeText(change: number, digits: number, changePercent: number | null): string {
  const signed = signedAmount(change, digits)
  return changePercent === null ? signed : `${signed} (${signedPercent(changePercent)})`
}

function performanceView(props: ChartProps, now: number): View {
  const { trades, balance } = props
  if (props.tradesError) return { state: 'error' }
  if (!trades || balance == null) return { state: 'loading' }

  const fromMs = now - props.range * DAY_MS
  // A truncated history only reaches back to its oldest close. The line starts
  // there instead of implying a flat balance over trades it cannot see.
  const oldest = Math.min(...trades.map((trade) => trade.closeTime * 1000))
  const partial = Boolean(props.tradesTruncated) && trades.length > 0 && oldest > fromMs
  const growth = balanceGrowth(trades, balance, partial ? oldest : fromMs, now)

  return {
    state: 'ready',
    stats: (
      <>
        <Stat label="Balance" value={amount(growth.end)} />
        <Stat
          label={counted(growth.trades, 'trade', partial)}
          value={changeText(growth.change, 2, growth.changePercent)}
          tone={signTone(growth.change)}
        />
        <Stat label="High" value={amount(growth.high)} />
        <Stat label="Low" value={amount(growth.low)} />
      </>
    ),
    plot: {
      label: 'Balance',
      points: growth.points.map((point) => ({ x: point.atMs, value: point.balance, atMs: point.atMs, trade: point.trade })),
      xDomain: [fromMs, now],
      xTicks: (maxCount) => timeTicks(fromMs, now, maxCount).map((tick) => ({ x: tick.atMs, label: tick.label })),
      decimals: 2,
      stepped: true,
      up: growth.change >= 0,
      tip: (point) => (
        <>
          <span className="chart-tip-time">{pointTime(point.atMs)}</span>
          <span className="chart-tip-value">{amount(point.value)}</span>
          {point.trade ? (
            <span className="chart-tip-trade">
              {point.trade.symbol} <span className={signTone(tradeNet(point.trade))}>{signedAmount(tradeNet(point.trade))}</span>
            </span>
          ) : null}
        </>
      ),
    },
  }
}

/** The served series is only drawn once it answers the instrument and timeframe asked for. */
function servesRequest(series: CandleSeries, props: ChartProps): boolean {
  return series.timeframe === props.timeframe && (props.symbol === undefined || series.symbol === props.symbol)
}

function marketView(props: ChartProps): View {
  const { series } = props
  if (props.seriesError) return { state: 'error' }
  if (!series || !servesRequest(series, props)) return { state: 'loading' }
  const candles = series.candles
  if (candles.length === 0) return { state: 'error' }

  const closes = candles.map((candle) => candle.close)
  const decimals = priceDecimals(closes)
  const first = closes[0]
  const lastClose = closes[closes.length - 1]
  const change = Number((lastClose - first).toFixed(decimals))
  const times = candles.map((candle) => candle.time * 1000)
  const daily = props.timeframe === 'D1' || props.timeframe === 'W1'

  return {
    state: 'ready',
    stats: (
      <>
        <Stat label="Last close" value={amount(lastClose, decimals)} />
        <Stat
          label={counted(candles.length, 'bar')}
          value={changeText(change, decimals, first > 0 ? (change / first) * 100 : null)}
          tone={signTone(change)}
        />
        <Stat label="High" value={amount(Math.max(...candles.map((candle) => candle.high)), decimals)} />
        <Stat label="Low" value={amount(Math.min(...candles.map((candle) => candle.low)), decimals)} />
      </>
    ),
    plot: {
      label: `${series.symbol} ${series.timeframe}`,
      points: candles.map((candle, index) => ({ x: index, value: candle.close, atMs: times[index] })),
      xDomain: [0, candles.length - 1],
      xTicks: (maxCount) => barTicks(times, maxCount).map((tick) => ({ x: tick.index, label: tick.label })),
      decimals,
      stepped: false,
      up: change >= 0,
      tip: (point) => (
        <>
          <span className="chart-tip-time">{pointTime(point.atMs, daily)}</span>
          <span className="chart-tip-value">{amount(point.value, decimals)}</span>
        </>
      ),
    },
  }
}

/* ---------- header ---------- */

type MenuItem = { key: string; label: string; checked: boolean; select: () => void }

function ViewMenu({ current, items }: { current: ReactNode; items: MenuItem[] }) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const button = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    if (!open) return
    const onPointer = (event: MouseEvent) => {
      if (!root.current!.contains(event.target as Node)) setOpen(false)
    }
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      setOpen(false)
      button.current!.focus()
    }
    document.addEventListener('mousedown', onPointer)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onPointer)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  return (
    <div className="chart-menu" ref={root}>
      <button
        ref={button}
        type="button"
        className="chart-menu-button"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        {current}
        <Icon name="chevron-down" className="chart-menu-chevron" />
      </button>
      {open ? (
        <div className="chart-menu-list" role="menu" aria-label="Chart view">
          {items.map((item) => (
            <button
              key={item.key}
              type="button"
              role="menuitemradio"
              aria-checked={item.checked}
              className="chart-menu-item"
              onClick={() => {
                item.select()
                setOpen(false)
              }}
            >
              {item.label}
              {item.checked ? <Icon name="check" size={16} className="chart-menu-check" /> : null}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  )
}

/** Measured content width of the plot container, following resizes. */
function useWidth() {
  const ref = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(0)
  useLayoutEffect(() => {
    const element = ref.current!
    const measure = () => setWidth(element.clientWidth)
    measure()
    if (typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    return () => observer.disconnect()
  }, [])
  return { ref, width: width || FALLBACK_WIDTH }
}

/**
 * The overview chart panel. Performance (balance growth from closed Veyra
 * trades, anchored to `balance`) is the default view; the title menu switches
 * to the market line for any instrument in `symbols`.
 */
export function ChartPanel(props: {
  mode: ChartMode
  onModeChange: (mode: ChartMode) => void
  range: PerformanceRange
  onRangeChange: (range: PerformanceRange) => void
  timeframe: MarketTimeframe
  onTimeframeChange: (timeframe: MarketTimeframe) => void
  /** Instrument the market view shows; undefined means the chart symbol. */
  symbol?: string
  /** Instruments offered in the view menu (autopilot rotation). */
  symbols: string[]
  onSymbolChange: (symbol: string) => void
  /** Closed Veyra trades over the longest range (newest first, as served). */
  trades?: ClosedTrade[]
  /** True when the venue truncated the trade history window. */
  tradesTruncated?: boolean
  tradesError?: string
  /** Current broker balance; the growth line is anchored to it. */
  balance?: number
  series?: CandleSeries
  seriesError?: string
  /** Clock for range windows; tests pin it. */
  now?: number
}) {
  const { mode, series, onModeChange } = props
  const { ref, width } = useWidth()
  const height = width < NARROW ? 200 : 252
  const view = mode === 'performance' ? performanceView(props, props.now ?? Date.now()) : marketView(props)

  const shown = props.symbol ?? series?.symbol
  const offered = props.symbols.length > 0 ? props.symbols : series ? [series.symbol] : []
  const items: MenuItem[] = [
    { key: 'performance', label: 'Performance', checked: mode === 'performance', select: () => onModeChange('performance') },
    ...(offered.length > 0
      ? offered.map((symbol) => ({
          key: symbol,
          label: `Market · ${symbol}`,
          checked: mode === 'market' && symbol === shown,
          select: () => {
            props.onSymbolChange(symbol)
            onModeChange('market')
          },
        }))
      : [{ key: 'market', label: 'Market', checked: mode === 'market', select: () => onModeChange('market') }]),
  ]
  const title =
    mode === 'performance' ? (
      'Performance'
    ) : (
      <>
        Market <span className="chart-title-sep">·</span> {shown ? `${shown} ${props.timeframe}` : props.timeframe}
      </>
    )

  return (
    <section className="panel chart" aria-label="Chart">
      <header className="chart-head">
        <h2 className="panel-title chart-title">
          <ViewMenu current={title} items={items} />
        </h2>
        {mode === 'performance' ? (
          <Segmented options={RANGES} value={props.range} onChange={props.onRangeChange} label="Range" />
        ) : (
          <Segmented options={TIMEFRAMES} value={props.timeframe} onChange={props.onTimeframeChange} label="Timeframe" />
        )}
      </header>
      <div className="chart-stats">
        {view.state === 'ready' ? view.stats : view.state === 'loading' ? <Skeleton width={300} height={14} /> : null}
      </div>
      <div className="chart-plot" ref={ref} style={{ height: height + AXIS_BAND }}>
        {view.state === 'ready' ? (
          <LinePlot key={mode} spec={view.plot} width={width} height={height} />
        ) : view.state === 'loading' ? (
          <div className="chart-fill" style={{ height }}>
            <Skeleton width="100%" height="100%" />
          </div>
        ) : (
          <div className="chart-fill chart-unavailable panel-empty" style={{ height }}>
            Unavailable
          </div>
        )}
      </div>
    </section>
  )
}
