/**
 * Render tests for the overview chart panel and its axis helpers.
 *
 * They cover both views (balance growth and market closes), every data state
 * (loading, failed, empty, stale series), the view menu, the range and
 * timeframe controls, the hover readout and the calendar tick rules. Clock
 * text is local time, so the suite pins the zone to UTC.
 */

process.env.TZ = 'UTC'

import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import type { Candle, CandleSeries, ClosedTrade } from '../lib/api'
import { barTicks, ChartPanel, decimalsOf, niceTicks, priceDecimals, timeTicks } from './chart'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
  delete (HTMLElement.prototype as { clientWidth?: number }).clientWidth
})

const HOUR = 3_600_000
const DAY = 24 * HOUR
/** Wednesday, Sep 23 2026, 12:00 UTC. */
const NOW = Date.UTC(2026, 8, 23, 12, 0)

function trade(closeMs: number, profit: number, extra: Partial<ClosedTrade> = {}): ClosedTrade {
  const closeTime = Math.round(closeMs / 1000)
  return {
    ticket: closeTime,
    symbol: 'EURUSD',
    kind: 'buy',
    lots: 0.01,
    openPrice: 1.1,
    closePrice: 1.1,
    openTime: closeTime - 3600,
    closeTime,
    profit,
    swap: 0,
    commission: 0,
    magic: 77041,
    ...extra,
  }
}

function candles(closes: number[], startMs: number, stepMs: number): Candle[] {
  return closes.map((close, index) => ({
    time: (startMs + index * stepMs) / 1000,
    open: close,
    high: close + 0.001,
    low: close - 0.001,
    close,
    volume: 100,
  }))
}

function series(closes: number[], extra: Partial<CandleSeries> = {}, stepMs = 4 * HOUR): CandleSeries {
  return { symbol: 'EURUSD', timeframe: 'H4', candles: candles(closes, NOW - closes.length * stepMs, stepMs), ...extra }
}

type Props = Parameters<typeof ChartPanel>[0]

function renderChart(props: Partial<Props> = {}) {
  const handlers = {
    onModeChange: vi.fn(),
    onRangeChange: vi.fn(),
    onTimeframeChange: vi.fn(),
    onSymbolChange: vi.fn(),
  }
  const all: Props = {
    mode: 'performance',
    range: 30,
    timeframe: 'H4',
    symbols: ['EURUSD', 'GBPUSD'],
    now: NOW,
    ...handlers,
    ...props,
  }
  const utils = render(<ChartPanel {...all} />)
  return { ...utils, ...handlers, rerenderWith: (next: Partial<Props>) => utils.rerender(<ChartPanel {...all} {...next} />) }
}

const stats = () => document.querySelector('.chart-stats')!.textContent
const tag = () => document.querySelector('.chart-tag-text')?.textContent
const xLabels = () => Array.from(document.querySelectorAll('.chart-xlabel')).map((node) => node.textContent)
const yLabels = () => Array.from(document.querySelectorAll('.chart-ylabel')).map((node) => node.textContent)
const tip = () => document.querySelector<HTMLElement>('.chart-tip')
const svg = () => screen.getByRole('img')

const growthTrades = [trade(NOW - 2 * DAY, 0.61), trade(NOW - DAY, 1.2, { symbol: 'GBPUSD' })]

describe('ChartPanel · performance', () => {
  it('holds the plot footprint with a skeleton until trades and balance arrive', () => {
    const { rerenderWith } = renderChart()
    expect(document.querySelector('.chart-fill .skeleton')).not.toBeNull()
    expect(document.querySelector('.chart-stats .skeleton')).not.toBeNull()
    expect(screen.queryByRole('img')).toBeNull()

    rerenderWith({ trades: growthTrades })
    expect(document.querySelector('.chart-fill .skeleton')).not.toBeNull()
  })

  it('says Unavailable when the history poll failed', () => {
    renderChart({ trades: growthTrades, balance: 37, tradesError: 'boom' })
    expect(screen.getByText('Unavailable')).toBeTruthy()
    expect(stats()).toBe('')
    expect(screen.queryByRole('img')).toBeNull()
  })

  it('states balance, trade count, change and range extremes', () => {
    renderChart({ trades: growthTrades, balance: 37 })
    expect(stats()).toBe('Balance37.002 trades+1.81 (+5.14%)High37.00Low35.19')
    expect(screen.getByText('+1.81 (+5.14%)').className).toContain('tone-ok')
    expect(tag()).toBe('37.00')
    expect(svg().getAttribute('aria-label')).toBe('Balance')
    expect(document.querySelector('.chart-line')!.classList.contains('is-up')).toBe(true)
    // A balance only moves at a close, so the line holds and then steps.
    expect(document.querySelector('.chart-line')!.getAttribute('d')).toMatch(/^M[\d.]+ [\d.]+H[\d.]+V/)
  })

  it('draws at the measured size and follows resizes', () => {
    let width = 390
    Object.defineProperty(HTMLElement.prototype, 'clientWidth', { configurable: true, get: () => width })
    const observers: Array<{ callback: () => void; disconnect: ReturnType<typeof vi.fn> }> = []
    vi.stubGlobal(
      'ResizeObserver',
      class {
        disconnect = vi.fn()
        constructor(public callback: () => void) {
          observers.push(this)
        }
        observe() {}
      },
    )
    const { unmount } = renderChart({ trades: growthTrades, balance: 37 })
    expect(svg().getAttribute('width')).toBe('390')
    expect(svg().getAttribute('height')).toBe('242')

    width = 1000
    act(() => observers[0].callback())
    expect(svg().getAttribute('width')).toBe('1000')
    expect(svg().getAttribute('height')).toBe('294')

    unmount()
    expect(observers[0].disconnect).toHaveBeenCalled()
  })

  it('draws a flat line at the balance when nothing closed in the window', () => {
    renderChart({ trades: [trade(NOW - 40 * DAY, 3)], balance: 37 })
    expect(stats()).toBe('Balance37.000 trades0.00 (0.00%)High37.00Low37.00')
    expect(screen.getByText('0.00 (0.00%)').className).not.toContain('tone-')
    expect(tag()).toBe('37.00')
    const d = document.querySelector('.chart-line')!.getAttribute('d')!
    const ys = new Set(Array.from(d.matchAll(/V([\d.]+)/g), (match) => match[1]))
    expect(ys.size).toBe(1)
  })

  it('keeps a band around a zero balance', () => {
    renderChart({ trades: [], balance: 0 })
    expect(stats()).toBe('Balance0.000 trades0.00High0.00Low0.00')
    expect(yLabels()).toContain('0.50')
  })

  it('colours a losing window red and reads one trade in the singular', () => {
    renderChart({ trades: [trade(NOW - DAY, -2.5)], balance: 20 })
    expect(stats()).toContain('1 trade−2.50 (−11.11%)')
    expect(screen.getByText('−2.50 (−11.11%)').className).toContain('tone-bad')
    expect(document.querySelector('.chart-line')!.classList.contains('is-down')).toBe(true)
    expect(document.querySelector('.chart-tag')!.classList.contains('is-down')).toBe(true)
  })

  it('leaves out the percentage when the start balance is not positive', () => {
    renderChart({ trades: [trade(NOW - DAY, 10)], balance: 10 })
    expect(stats()).toContain('1 trade+10.00High')
  })

  it('groups thousands on the tag', () => {
    renderChart({ trades: [trade(NOW - DAY, 120)], balance: 23482.17 })
    expect(tag()).toBe('23,482.17')
  })

  it('starts a truncated history at its oldest close and marks the count open-ended', () => {
    const trades = [trade(NOW - DAY, 1), trade(NOW - 5 * DAY, 1)]
    renderChart({ trades, balance: 10, tradesTruncated: true })
    expect(stats()).toContain('2+ trades+2.00 (+25.00%)')
    const firstX = Number(/^M([\d.]+)/.exec(document.querySelector('.chart-line')!.getAttribute('d')!)![1])
    expect(firstX).toBeGreaterThan(500)
  })

  it('treats a truncated history as complete when it reaches past the window', () => {
    renderChart({ trades: [trade(NOW - DAY, 1), trade(NOW - 60 * DAY, 1)], balance: 10, tradesTruncated: true })
    expect(stats()).toContain('1 trade+1.00')
    cleanup()
    renderChart({ trades: [], balance: 10, tradesTruncated: true })
    expect(stats()).toContain('0 trades')
  })

  it('switches range through the segmented control', () => {
    const { onRangeChange } = renderChart({ trades: growthTrades, balance: 37 })
    const group = screen.getByRole('group', { name: 'Range' })
    expect(within(group).getAllByRole('button').map((button) => button.textContent)).toEqual(['7D', '30D', '90D', '1Y'])
    expect(within(group).getByRole('button', { name: '30D' }).getAttribute('aria-pressed')).toBe('true')
    fireEvent.click(within(group).getByRole('button', { name: '7D' }))
    expect(onRangeChange).toHaveBeenCalledWith(7)
  })

  it('labels the time axis with dates, weeks and months as the range grows', () => {
    renderChart({ trades: growthTrades, balance: 37 })
    expect(xLabels()).toEqual(['Aug 31', 'Sep 7', 'Sep 14', 'Sep 21'])
    cleanup()
    renderChart({ trades: growthTrades, balance: 37, range: 7 })
    expect(xLabels()).toEqual(['Sep 17', 'Sep 18', 'Sep 19', 'Sep 20', 'Sep 21', 'Sep 22', 'Sep 23'])
    cleanup()
    renderChart({ trades: growthTrades, balance: 37, range: 90 })
    expect(xLabels().every((label) => /^[A-Z][a-z]{2} \d{1,2}$/.test(label!))).toBe(true)
    cleanup()
    renderChart({ trades: growthTrades, balance: 37, range: 365 })
    expect(xLabels()).toContain('2026')
    expect(xLabels()).toContain('May')
  })

  it('reads out the nearest point on hover and hides on leave', () => {
    renderChart({ trades: growthTrades, balance: 37 })
    fireEvent.mouseMove(svg(), { clientX: 852 })
    expect(tip()!.textContent).toBe('Sep 22, 12:0037.00GBPUSD +1.20')
    expect(tip()!.style.right).not.toBe('')
    expect(document.querySelector('.chart-cross')).not.toBeNull()

    fireEvent.mouseMove(svg(), { clientX: 10 })
    expect(tip()!.textContent).toBe('Aug 24, 12:0035.19')
    expect(tip()!.style.left).not.toBe('')

    fireEvent.mouseLeave(svg())
    expect(tip()).toBeNull()
    expect(document.querySelector('.chart-cross')).toBeNull()
  })

  it('follows touch and clears when the finger lifts', () => {
    renderChart({ trades: growthTrades, balance: 37 })
    fireEvent.touchStart(svg(), { touches: [{ clientX: 824 }] })
    expect(tip()!.textContent).toContain('EURUSD +0.61')
    fireEvent.touchMove(svg(), { touches: [{ clientX: 900 }] })
    expect(tip()!.textContent).toBe('Sep 23, 12:0037.00')
    fireEvent.touchEnd(svg())
    expect(tip()).toBeNull()
  })

  it('uses the real clock when none is given', () => {
    renderChart({ now: undefined, trades: [], balance: 5 })
    expect(stats()).toContain('Balance5.00')
  })
})

describe('ChartPanel · market', () => {
  const closes = Array.from({ length: 30 }, (_, index) => Number((1.15 + index * 0.0001).toFixed(5)))

  it('titles the view with the served symbol and the timeframe', () => {
    renderChart({ mode: 'market', series: series(closes) })
    expect(screen.getByRole('button', { name: 'Market · EURUSD H4' }).getAttribute('aria-haspopup')).toBe('menu')
  })

  it('titles the view with the timeframe alone before anything is known', () => {
    renderChart({ mode: 'market', symbols: [] })
    expect(screen.getByRole('button', { name: 'Market · H4' })).toBeTruthy()
  })

  it('states last close, bar count, change and extremes at the quoted precision', () => {
    renderChart({ mode: 'market', series: series([...closes, 1.15325]) })
    expect(stats()).toBe('Last close1.1532531 bars+0.00325 (+0.28%)High1.15425Low1.14900')
    expect(tag()).toBe('1.15325')
    expect(svg().getAttribute('aria-label')).toBe('EURUSD H4')
  })

  it('colours a falling series red', () => {
    renderChart({ mode: 'market', series: series([1.2, 1.1]) })
    expect(stats()).toContain('2 bars−0.1 (−8.33%)')
    expect(document.querySelector('.chart-line')!.classList.contains('is-down')).toBe(true)
  })

  it('has no percentage from a zero first close', () => {
    renderChart({ mode: 'market', series: series([0, 2]) })
    expect(stats()).toContain('2 bars+2High')
  })

  it('draws a single candle as a point', () => {
    renderChart({ mode: 'market', series: series([1.1]) })
    expect(document.querySelector('.chart-line')).toBeNull()
    expect(document.querySelector('.chart-point')).not.toBeNull()
    expect(stats()).toContain('1 bar0.0 (0.00%)')
  })

  it('shows a skeleton while the requested series is still loading', () => {
    const { rerenderWith } = renderChart({ mode: 'market' })
    expect(document.querySelector('.chart-fill .skeleton')).not.toBeNull()
    rerenderWith({ series: series(closes, { timeframe: 'H1' }) })
    expect(document.querySelector('.chart-fill .skeleton')).not.toBeNull()
    rerenderWith({ symbol: 'GBPUSD', series: series(closes) })
    expect(document.querySelector('.chart-fill .skeleton')).not.toBeNull()
    rerenderWith({ symbol: 'EURUSD', series: series(closes) })
    expect(document.querySelector('.chart-fill')).toBeNull()
    expect(svg()).toBeTruthy()
  })

  it('says Unavailable on a failed poll or an empty series', () => {
    const { rerenderWith } = renderChart({ mode: 'market', series: series(closes), seriesError: 'boom' })
    expect(screen.getByText('Unavailable')).toBeTruthy()
    rerenderWith({ seriesError: undefined, series: series([]) })
    expect(screen.getByText('Unavailable')).toBeTruthy()
  })

  it('switches timeframe through the segmented control', () => {
    const { onTimeframeChange } = renderChart({ mode: 'market', series: series(closes) })
    const group = screen.getByRole('group', { name: 'Timeframe' })
    expect(within(group).getAllByRole('button').map((button) => button.textContent)).toEqual(['M15', 'H1', 'H4', 'D1', 'W1'])
    expect(within(group).getByRole('button', { name: 'H4' }).getAttribute('aria-pressed')).toBe('true')
    fireEvent.click(within(group).getByRole('button', { name: 'D1' }))
    expect(onTimeframeChange).toHaveBeenCalledWith('D1')
  })

  it('labels intraday spans with clock times', () => {
    const quarter = 15 * 60_000
    renderChart({ mode: 'market', timeframe: 'M15', series: series(closes, { timeframe: 'M15' }, quarter) })
    expect(xLabels().length).toBeGreaterThan(1)
    expect(xLabels().every((label) => /^\d{2}:\d{2}$/.test(label!))).toBe(true)
  })

  it('labels daily bars by month and reads the tooltip with the year', () => {
    const daily = Array.from({ length: 150 }, (_, index) => 1.1 + index / 10_000)
    renderChart({ mode: 'market', timeframe: 'D1', series: series(daily, { timeframe: 'D1' }, DAY) })
    expect(xLabels()).toEqual(['May', 'Jun', 'Jul', 'Aug', 'Sep'])
    fireEvent.mouseMove(svg(), { clientX: 880 })
    expect(tip()!.textContent).toBe('Sep 22, 20261.1149')
  })

  it('keeps time labels clear of the frame and of each other', () => {
    // Day opens at bar 1 (by the left frame), 40 and 41 (too close together)
    // and 79 (by the axis); only the one at 40 has room.
    const halfHours = (dayOfMonth: number, count: number) =>
      Array.from({ length: count }, (_, index) => Date.UTC(2026, 8, dayOfMonth) + index * 30 * 60_000)
    const times = [...halfHours(1, 1), ...halfHours(2, 39), ...halfHours(3, 1), ...halfHours(4, 38), ...halfHours(5, 1)]
    const bars = times.map((ms, index) => ({ time: ms / 1000, open: 1, high: 1, low: 1, close: 1 + index / 1000, volume: 1 }))
    renderChart({ mode: 'market', series: { symbol: 'EURUSD', timeframe: 'H4', candles: bars } })
    expect(xLabels()).toEqual(['Sep 3'])
  })

  it('drops y ticks that would crowd the frame', () => {
    renderChart({ mode: 'market', series: series([109.5, 100]) })
    expect(yLabels()).not.toContain('110.0')
    expect(yLabels()).toContain('102.0')
    cleanup()
    renderChart({ mode: 'market', series: series([100.66, 110.16]) })
    expect(yLabels()).not.toContain('100.00')
    expect(yLabels()).toContain('102.00')
  })

  it('drops a hover whose point no longer exists', () => {
    const { rerenderWith } = renderChart({ mode: 'market', series: series([1.1, 1.2, 1.3]) })
    fireEvent.mouseMove(svg(), { clientX: 900 })
    expect(tip()!.textContent).toContain('1.3')
    rerenderWith({ series: series([1.1]) })
    expect(tip()).toBeNull()
  })
})

describe('ChartPanel · view menu', () => {
  it('opens a menu of views with the current one checked', () => {
    renderChart({ trades: growthTrades, balance: 37 })
    const button = screen.getByRole('button', { name: 'Performance' })
    expect(button.getAttribute('aria-expanded')).toBe('false')
    fireEvent.click(button)
    expect(button.getAttribute('aria-expanded')).toBe('true')
    const items = within(screen.getByRole('menu')).getAllByRole('menuitemradio')
    expect(items.map((item) => item.textContent)).toEqual(['Performance', 'Market · EURUSD', 'Market · GBPUSD'])
    expect(items.map((item) => item.getAttribute('aria-checked'))).toEqual(['true', 'false', 'false'])
    fireEvent.click(button)
    expect(screen.queryByRole('menu')).toBeNull()
  })

  it('switches to an instrument and closes', () => {
    const { onModeChange, onSymbolChange } = renderChart()
    fireEvent.click(screen.getByRole('button', { name: 'Performance' }))
    fireEvent.click(screen.getByRole('menuitemradio', { name: 'Market · GBPUSD' }))
    expect(onSymbolChange).toHaveBeenCalledWith('GBPUSD')
    expect(onModeChange).toHaveBeenCalledWith('market')
    expect(screen.queryByRole('menu')).toBeNull()
  })

  it('switches back to performance', () => {
    const { onModeChange } = renderChart({ mode: 'market', series: series([1.1, 1.2]) })
    fireEvent.click(screen.getByRole('button', { name: 'Market · EURUSD H4' }))
    const items = screen.getAllByRole('menuitemradio')
    expect(items.map((item) => item.getAttribute('aria-checked'))).toEqual(['false', 'true', 'false'])
    fireEvent.click(screen.getByRole('menuitemradio', { name: 'Performance' }))
    expect(onModeChange).toHaveBeenCalledWith('performance')
  })

  it('closes on Escape and returns focus to the button', () => {
    renderChart()
    const button = screen.getByRole('button', { name: 'Performance' })
    fireEvent.click(button)
    fireEvent.keyDown(document, { key: 'ArrowDown' })
    expect(screen.getByRole('menu')).toBeTruthy()
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(screen.queryByRole('menu')).toBeNull()
    expect(document.activeElement).toBe(button)
  })

  it('closes on a click outside, not inside', () => {
    renderChart()
    fireEvent.click(screen.getByRole('button', { name: 'Performance' }))
    fireEvent.mouseDown(screen.getByRole('menu'))
    expect(screen.getByRole('menu')).toBeTruthy()
    fireEvent.mouseDown(document.body)
    expect(screen.queryByRole('menu')).toBeNull()
  })

  it('offers the served symbol when there is no rotation', () => {
    const { onSymbolChange } = renderChart({ symbols: [], mode: 'market', series: series([1.1, 1.2]) })
    fireEvent.click(screen.getByRole('button', { name: 'Market · EURUSD H4' }))
    fireEvent.click(screen.getByRole('menuitemradio', { name: 'Market · EURUSD' }))
    expect(onSymbolChange).toHaveBeenCalledWith('EURUSD')
  })

  it('offers a plain market view when no instrument is known yet', () => {
    const { onModeChange } = renderChart({ symbols: [] })
    fireEvent.click(screen.getByRole('button', { name: 'Performance' }))
    const market = screen.getByRole('menuitemradio', { name: 'Market' })
    expect(market.getAttribute('aria-checked')).toBe('false')
    fireEvent.click(market)
    expect(onModeChange).toHaveBeenCalledWith('market')
  })
})

describe('axis helpers', () => {
  it('reads quoted precision from prices', () => {
    expect(decimalsOf(1.15978)).toBe(5)
    expect(decimalsOf(158)).toBe(0)
    expect(priceDecimals([158.3, 158.353, 158])).toBe(3)
    expect(priceDecimals([])).toBe(0)
  })

  it('picks round steps near the target count', () => {
    expect(niceTicks(20.46, 38.8, 5.5)).toEqual({ ticks: [22.5, 25, 27.5, 30, 32.5, 35, 37.5], step: 2.5 })
    expect(niceTicks(1.1375, 1.1675, 5.5)).toEqual({ ticks: [1.14, 1.145, 1.15, 1.155, 1.16, 1.165], step: 0.005 })
    expect(niceTicks(0, 100, 4).step).toBe(25)
  })

  it('steps a short window in clock time and falls back to the widest step', () => {
    const from = Date.UTC(2026, 8, 22, 6, 0)
    const ticks = timeTicks(from, from + 30 * HOUR, 6)
    expect(ticks.map((tick) => tick.label)).toEqual(['12:00', '18:00', '00:00', '06:00', '12:00'])
    expect(timeTicks(from, from + 30 * HOUR, 1).map((tick) => tick.label)).toEqual(['12:00', '00:00', '12:00'])
    expect(timeTicks(Date.UTC(2020, 0, 1), Date.UTC(2026, 0, 2), 2).map((tick) => tick.label)).toEqual([
      '2021',
      '2022',
      '2023',
      '2024',
      '2025',
      '2026',
    ])
  })

  it('ticks bar series at clock, trading-day and month opens', () => {
    const quarter = Array.from({ length: 120 }, (_, index) => Date.UTC(2026, 8, 22, 0, 0) + index * 15 * 60_000)
    expect(barTicks(quarter, 5).map((tick) => tick.label)).toEqual(['06:00', '12:00', '18:00', '00:00'])
    expect(barTicks(quarter, 1).map((tick) => tick.label)).toEqual(['12:00', '00:00'])

    const h4 = Array.from({ length: 60 }, (_, index) => Date.UTC(2026, 8, 1) + index * 4 * HOUR)
    expect(barTicks(h4, 5).map((tick) => tick.label)).toEqual(['Sep 2', 'Sep 4', 'Sep 6', 'Sep 8', 'Sep 10'])

    const weekly = Array.from({ length: 300 }, (_, index) => Date.UTC(2020, 0, 6) + index * 7 * DAY)
    expect(barTicks(weekly, 1).map((tick) => tick.label)).toEqual(['2021', '2022', '2023', '2024', '2025'])
  })
})
