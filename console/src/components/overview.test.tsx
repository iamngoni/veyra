/**
 * Render tests for the overview main column: the headline cards, the open
 * positions table and the 30-day performance summary. Payloads use the shapes
 * the service emits; the clock is pinned wherever "today" matters.
 */

import { cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import type { Account, ClosedTrade, Performance, PerformanceReport, Position } from '../lib/api'
import { VEYRA_MAGIC } from '../lib/api'
import { KpiRow, OpenPositions, PerformanceSummary } from './overview'

afterEach(cleanup)

const position = (overrides: Partial<Position> = {}): Position => ({
  ticket: 101,
  symbol: 'EURUSD',
  kind: 'buy',
  lots: 0.5,
  price: 1.0785,
  profit: 280,
  sl: 1.074,
  tp: 1.092,
  swap: 3,
  commission: 2,
  current: 1.0842,
  magic: VEYRA_MAGIC,
  ...overrides,
})

const account = (overrides: Partial<Account> = {}): Account => ({
  fresh: true,
  connected: true,
  tradeAllowed: true,
  liveOrders: true,
  ageSecs: 2,
  balance: 23_000,
  equity: 23_482.17,
  freeMargin: 18_482.17,
  lots: 0.5,
  orders: 2,
  positions: [position()],
  positionsTruncated: false,
  ...overrides,
})

const trade = (overrides: Partial<ClosedTrade> = {}): ClosedTrade => ({
  ticket: 1,
  symbol: 'EURUSD',
  kind: 'buy',
  lots: 0.1,
  openPrice: 1.07,
  closePrice: 1.08,
  openTime: 0,
  closeTime: 0,
  profit: 0,
  swap: 0,
  commission: 0,
  magic: VEYRA_MAGIC,
  ...overrides,
})

/** A card's value and sub line, located by its label. */
function card(label: string) {
  const root = screen.getByText(label).closest('.kpi-card') as HTMLElement
  return {
    value: root.querySelector('.kpi-value') as HTMLElement,
    sub: root.querySelector('.kpi-sub') as HTMLElement,
  }
}

// 10:30 local time on 23 September 2026, and that day's local midnight.
const NOW = new Date(2026, 8, 23, 10, 30).getTime()
const MIDNIGHT = new Date(2026, 8, 23, 0, 0).getTime() / 1000

describe('KpiRow', () => {
  it('shows the four account figures', () => {
    render(<KpiRow account={account()} trades={[]} now={NOW} />)
    expect(card('Equity').value.textContent).toBe('23,482.17')
    expect(card('Open P/L').value.textContent).toBe('+285.00')
    expect(card('Open P/L').value.className).toContain('tone-ok')
    expect(card('Open P/L').sub.textContent).toBe('+1.24%')
    expect(card('Open P/L').sub.className).toContain('tone-ok')
    expect(card('Exposure').value.textContent).toBe('0.50 lots')
    expect(card('Exposure').sub.textContent).toBe('2 open orders')
    expect(card('Free margin').value.textContent).toBe('18,482.17')
    expect(card('Free margin').sub.textContent).toBe('79% available')
  })

  it("counts only trades closed since local midnight in today's change", () => {
    const trades = [
      trade({ closeTime: MIDNIGHT, profit: 40, swap: -1.5, commission: -0.5 }),
      trade({ closeTime: MIDNIGHT + 3600, profit: 10 }),
      trade({ closeTime: MIDNIGHT - 1, profit: 999 }),
    ]
    render(<KpiRow account={account()} trades={trades} now={NOW} />)
    const { sub } = card('Equity')
    expect(sub.textContent).toBe('+48.00 today')
    expect(sub.className).toContain('tone-ok')
  })

  it('tones a losing day red and a flat day muted', () => {
    const { rerender } = render(
      <KpiRow account={account()} trades={[trade({ closeTime: MIDNIGHT + 60, profit: -12.5 })]} now={NOW} />,
    )
    expect(card('Equity').sub.textContent).toBe('−12.50 today')
    expect(card('Equity').sub.className).toContain('tone-bad')

    rerender(<KpiRow account={account()} trades={[trade({ closeTime: MIDNIGHT - 60, profit: 5 })]} now={NOW} />)
    expect(card('Equity').sub.textContent).toBe('0.00 today')
    expect(card('Equity').sub.className).toContain('tone-muted')
  })

  it("omits today's change until the trade history arrives", () => {
    render(<KpiRow account={account()} now={NOW} />)
    expect(card('Equity').sub.textContent).toBe('')
  })

  it('uses the real clock when none is pinned', () => {
    render(<KpiRow account={account()} trades={[trade({ closeTime: Date.now() / 1000, profit: 7 })]} />)
    expect(card('Equity').sub.textContent).toBe('+7.00 today')
  })

  it('shows a losing book in red, charges included', () => {
    const losing = position({ profit: -40, swap: -1, commission: undefined })
    render(<KpiRow account={account({ positions: [losing, position({ ticket: 2, profit: 10, swap: undefined })] })} />)
    expect(card('Open P/L').value.textContent).toBe('−29.00')
    expect(card('Open P/L').value.className).toContain('tone-bad')
    expect(card('Open P/L').sub.textContent).toBe('−0.13%')
    expect(card('Open P/L').sub.className).toContain('tone-bad')
  })

  it('reads a flat book as zero with no percentage', () => {
    render(<KpiRow account={account({ positions: [], orders: 1, lots: 0 })} />)
    expect(card('Open P/L').value.textContent).toBe('0.00')
    expect(card('Open P/L').value.className).not.toContain('tone-')
    expect(card('Open P/L').sub.textContent).toBe('')
    expect(card('Exposure').value.textContent).toBe('0.00 lots')
    expect(card('Exposure').sub.textContent).toBe('1 open order')
  })

  it('treats a snapshot without a position list as flat', () => {
    render(<KpiRow account={account({ positions: undefined })} />)
    expect(card('Open P/L').value.textContent).toBe('0.00')
  })

  it('leaves out ratios it cannot compute', () => {
    render(
      <KpiRow
        account={account({ balance: 0, equity: undefined, freeMargin: undefined, lots: undefined, orders: undefined })}
      />,
    )
    expect(card('Open P/L').sub.textContent).toBe('')
    expect(card('Equity').value.textContent).toBe('—')
    expect(card('Exposure').value.textContent).toBe('—')
    expect(card('Exposure').sub.textContent).toBe('')
    expect(card('Free margin').value.textContent).toBe('—')
    expect(card('Free margin').sub.textContent).toBe('')
  })

  it('omits the available share when equity is zero', () => {
    render(<KpiRow account={account({ equity: 0, freeMargin: 0, balance: undefined })} />)
    expect(card('Free margin').value.textContent).toBe('0.00')
    expect(card('Free margin').sub.textContent).toBe('')
    expect(card('Open P/L').sub.textContent).toBe('')
  })

  it('holds each value with a skeleton while the account loads', () => {
    const { container } = render(<KpiRow />)
    expect(container.querySelectorAll('.kpi-value .skeleton')).toHaveLength(4)
    expect(screen.queryByText('Unavailable')).toBeNull()
  })

  it('reads Unavailable when the account poll failed with nothing to show', () => {
    render(<KpiRow error="HTTP 502" />)
    expect(screen.getAllByText('Unavailable')).toHaveLength(4)
  })

  it('keeps showing the last account when a later poll fails', () => {
    render(<KpiRow account={account()} error="HTTP 502" />)
    expect(screen.queryByText('Unavailable')).toBeNull()
    expect(card('Equity').value.textContent).toBe('23,482.17')
  })
})

describe('OpenPositions', () => {
  it('lists each position with side, precision and result', () => {
    const short = position({
      ticket: 202,
      symbol: 'USDJPY',
      kind: 'sell',
      lots: 1,
      price: 158.3,
      current: 158.317,
      sl: 0,
      tp: 157.25,
      profit: -12.4,
      swap: -0.6,
      commission: 0,
    })
    render(<OpenPositions account={account({ positions: [position(), short] })} harvestEnabled />)

    expect(screen.getByRole('heading').textContent).toBe('Open positions (2)')
    const [head, first, second] = screen.getAllByRole('row')
    expect(within(head).getAllByRole('columnheader').map((cell) => cell.textContent)).toEqual([
      'Symbol',
      'Side',
      'Lots',
      'Entry',
      'Current',
      'SL',
      'TP',
      'P/L',
      'Profit harvest',
      'Details',
    ])

    const long = within(first).getAllByRole('cell').map((cell) => cell.textContent)
    expect(long.slice(0, 9)).toEqual([
      'EURUSD',
      'Long',
      '0.50',
      '1.0785',
      '1.0842',
      '1.0740',
      '1.0920',
      '+285.00',
      'Monitoring',
    ])
    expect(within(first).getByText('Long').className).toContain('tone-ok')
    expect(within(first).getByText('+285.00').className).toContain('tone-ok')

    // A price short a digit is padded to the instrument's quote precision.
    const sell = within(second).getAllByRole('cell').map((cell) => cell.textContent)
    expect(sell.slice(0, 8)).toEqual(['USDJPY', 'Short', '1.00', '158.300', '158.317', '—', '157.250', '−13.00'])
    expect(within(second).getByText('Short').className).toContain('tone-bad')
    expect(within(second).getByText('−13.00').className).toContain('tone-bad')
  })

  it('dashes a missing quote and defaults to two decimals with no fractional prices', () => {
    const bare = position({ price: 2400, current: undefined, sl: 0, tp: 0, profit: 0, swap: 0, commission: 0 })
    render(<OpenPositions account={account({ positions: [bare] })} harvestEnabled />)
    const cells = within(screen.getAllByRole('row')[1]).getAllByRole('cell').map((cell) => cell.textContent)
    expect(cells.slice(3, 8)).toEqual(['2400.00', '—', '—', '—', '0.00'])
  })

  it('reports the profit-harvest state by owner and policy', () => {
    const manual = position({ ticket: 303, symbol: 'GBPUSD', magic: 0 })
    const { rerender } = render(<OpenPositions account={account({ positions: [position(), manual] })} harvestEnabled />)
    expect(screen.getByText('Monitoring').querySelector('.dot.is-ok')).toBeTruthy()
    expect(screen.getByText('Manual').querySelector('.dot.is-idle')).toBeTruthy()
    expect(screen.queryByText('Armed')).toBeNull()

    rerender(<OpenPositions account={account({ positions: [position(), manual] })} harvestEnabled={false} />)
    expect(screen.getByText('Off').querySelector('.dot.is-off')).toBeTruthy()
    expect(screen.getByText('Manual')).toBeTruthy()
    expect(screen.queryByText('Monitoring')).toBeNull()
  })

  it('opens and closes a detail row per position', () => {
    const opened = new Date(2026, 8, 23, 14, 5).getTime() / 1000
    const other = position({ ticket: 404, symbol: 'AUDUSD', openedAt: undefined, swap: undefined, commission: undefined })
    render(
      <OpenPositions
        account={account({ positions: [position({ openedAt: opened, swap: -1.25, commission: 0 }), other] })}
        harvestEnabled
      />,
    )
    const toggle = screen.getByRole('button', { name: 'Details for EURUSD' })
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    expect(toggle.getAttribute('aria-controls')).toBe('pos-detail-101')
    expect(screen.queryByText('Ticket')).toBeNull()

    fireEvent.click(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    const detail = document.getElementById('pos-detail-101') as HTMLElement
    const pairs = Array.from(detail.querySelectorAll('dl > div')).map((item) => [
      item.querySelector('dt')?.textContent,
      item.querySelector('dd')?.textContent,
    ])
    expect(pairs).toEqual([
      ['Ticket', '101'],
      ['Opened', '23 Sep, 14:05'],
      ['Swap', '−1.25'],
      ['Commission', '0.00'],
    ])

    // Opening another row closes the first; absent fields read as dashes.
    fireEvent.click(screen.getByRole('button', { name: 'Details for AUDUSD' }))
    expect(document.getElementById('pos-detail-101')).toBeNull()
    const second = document.getElementById('pos-detail-404') as HTMLElement
    expect(Array.from(second.querySelectorAll('dd')).map((cell) => cell.textContent)).toEqual(['404', '—', '—', '—'])

    fireEvent.click(screen.getByRole('button', { name: 'Details for AUDUSD' }))
    expect(document.getElementById('pos-detail-404')).toBeNull()
  })

  it('says so when the book is flat', () => {
    render(<OpenPositions account={account({ positions: [] })} harvestEnabled />)
    expect(screen.getByText('No open positions')).toBeTruthy()
    expect(screen.getByRole('heading').textContent).toBe('Open positions (0)')
    expect(screen.queryByRole('table')).toBeNull()
  })

  it('treats a snapshot without a position list as flat', () => {
    render(<OpenPositions account={account({ positions: undefined })} harvestEnabled />)
    expect(screen.getByText('No open positions')).toBeTruthy()
  })

  it('flags a truncated book', () => {
    const { rerender } = render(<OpenPositions account={account({ positionsTruncated: true })} harvestEnabled />)
    expect(screen.getByText('Truncated').className).toContain('tone-warn')
    rerender(<OpenPositions account={account()} harvestEnabled />)
    expect(screen.queryByText('Truncated')).toBeNull()
  })

  it('holds its place with skeletons and no count while loading', () => {
    const { container } = render(<OpenPositions harvestEnabled />)
    expect(screen.getByRole('heading').textContent).toBe('Open positions')
    expect(container.querySelectorAll('.pos-loading .skeleton').length).toBeGreaterThan(0)
    expect(screen.queryByText('No open positions')).toBeNull()
  })
})

const report = (overrides: Partial<PerformanceReport> = {}): PerformanceReport => ({
  trades: 50,
  wins: 34,
  losses: 16,
  breakeven: 0,
  win_rate_percent: 68,
  net_profit: 1842.3,
  gross_profit: 2600,
  gross_loss: 757.7,
  profit_factor: 3.43,
  average_win: 76.47,
  average_loss: 47.36,
  expectancy: 36.85,
  best_trade: 210,
  worst_trade: -80,
  by_symbol: [],
  ...overrides,
})

const performance = (overrides: Partial<PerformanceReport> = {}, truncated = false): Performance => ({
  days: 30,
  report: report(overrides),
  trades: [],
  total: 50,
  truncated,
})

/** A summary figure's value and sub line, located by its label. */
function stat(label: string) {
  const root = screen.getByText(label).closest('.perf-stat') as HTMLElement
  return {
    value: root.querySelector('.perf-value') as HTMLElement,
    sub: root.querySelector('.perf-sub') as HTMLElement,
  }
}

describe('PerformanceSummary', () => {
  it('summarises the window', () => {
    render(<PerformanceSummary performance={performance()} balance={23_517.3} />)
    expect(screen.getByRole('heading', { name: '30-day performance' })).toBeTruthy()
    expect(stat('Win rate').value.textContent).toBe('68%')
    expect(stat('Win rate').sub.textContent).toBe('34 wins / 16 losses')
    expect(stat('Net P/L').value.textContent).toBe('+1,842.30')
    expect(stat('Net P/L').value.className).toContain('tone-ok')
    // 1,842.30 on a starting balance of 21,675.00.
    expect(stat('Net P/L').sub.textContent).toBe('+8.5%')
    expect(stat('Net P/L').sub.className).toContain('tone-ok')
    expect(stat('Closed trades').value.textContent).toBe('50')
    expect(stat('Closed trades').sub.textContent).toBe('Avg +36.85')
    expect(screen.queryByText('Truncated')).toBeNull()
  })

  it('counts flat trades and uses singular words', () => {
    render(
      <PerformanceSummary
        performance={performance({ trades: 4, wins: 1, losses: 1, breakeven: 2, win_rate_percent: 25 })}
        balance={100}
      />,
    )
    expect(stat('Win rate').value.textContent).toBe('25%')
    expect(stat('Win rate').sub.textContent).toBe('1 win / 1 loss / 2 flat')
  })

  it('shows a losing window in red', () => {
    render(<PerformanceSummary performance={performance({ net_profit: -250, expectancy: -5 })} balance={750} />)
    expect(stat('Net P/L').value.textContent).toBe('−250.00')
    expect(stat('Net P/L').value.className).toContain('tone-bad')
    expect(stat('Net P/L').sub.textContent).toBe('−25.0%')
    expect(stat('Net P/L').sub.className).toContain('tone-bad')
    expect(stat('Closed trades').sub.textContent).toBe('Avg −5.00')
  })

  it('leaves the return out when the starting balance is unknown or not positive', () => {
    const { rerender } = render(<PerformanceSummary performance={performance()} />)
    expect(stat('Net P/L').sub.textContent).toBe('')
    rerender(<PerformanceSummary performance={performance()} balance={1842.3} />)
    expect(stat('Net P/L').sub.textContent).toBe('')
    rerender(<PerformanceSummary performance={performance()} balance={1000} />)
    expect(stat('Net P/L').sub.textContent).toBe('')
  })

  it('omits the average when the service has none', () => {
    render(<PerformanceSummary performance={performance({ expectancy: null })} balance={30_000} />)
    expect(stat('Closed trades').sub.textContent).toBe('')
  })

  it('reads a window without trades as dashes', () => {
    render(
      <PerformanceSummary
        performance={performance({ trades: 0, wins: 0, losses: 0, win_rate_percent: 0, net_profit: 0, expectancy: null })}
        balance={1000}
      />,
    )
    for (const label of ['Win rate', 'Net P/L', 'Closed trades']) {
      expect(stat(label).value.textContent).toBe('—')
      expect(stat(label).sub.textContent).toBe('')
    }
  })

  it('flags a truncated window', () => {
    render(<PerformanceSummary performance={performance({}, true)} balance={30_000} />)
    expect(screen.getByText('Truncated').className).toContain('tone-warn')
  })

  it('holds each value with a skeleton while loading', () => {
    const { container } = render(<PerformanceSummary />)
    expect(container.querySelectorAll('.perf-value .skeleton')).toHaveLength(3)
  })

  it('reads Unavailable when the poll failed with nothing to show', () => {
    const { rerender } = render(<PerformanceSummary error="HTTP 504" />)
    expect(screen.getAllByText('Unavailable')).toHaveLength(3)
    rerender(<PerformanceSummary error="HTTP 504" performance={performance()} balance={30_000} />)
    expect(screen.queryByText('Unavailable')).toBeNull()
    expect(stat('Win rate').value.textContent).toBe('68%')
  })
})
