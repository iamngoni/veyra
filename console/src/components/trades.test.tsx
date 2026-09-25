/**
 * Render tests for the Trades page: every reason label and tone, the date,
 * held-time, R and price formatting, row expansion, range switching, and the
 * loading/error/empty/truncated states.
 */

import { cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import type { CloseReason, ClosedTradeRow, TradesPage } from '../lib/api'
import { REASON_LABEL, REASON_TONE, TradesPanel, heldFor } from './trades'

afterEach(cleanup)

const trade = (overrides: Partial<ClosedTradeRow> = {}): ClosedTradeRow => ({
  ticket: 10655087,
  symbol: 'GBPUSD',
  side: 'short',
  lots: 0.01,
  // 21h 18m held, closed 25 Sep 15:08 local — the contract's own example times.
  openedAtMs: new Date(2026, 8, 24, 17, 50).getTime(),
  closedAtMs: new Date(2026, 8, 25, 15, 8).getTime(),
  openPrice: 1.32123,
  closePrice: 1.3265,
  stopLoss: 1.3299,
  takeProfit: 1.3162,
  net: -5.31,
  profit: -5.31,
  swap: 0,
  commission: 0,
  rMultiple: -1,
  closeReason: 'stop_loss',
  closeDetail: null,
  entryRationale: 'GBPUSD has the strongest aligned bearish evidence…',
  ...overrides,
})

const page = (overrides: Partial<TradesPage> = {}): TradesPage => ({
  days: 30,
  truncated: false,
  total: 1,
  page: 1,
  pageSize: 20,
  pageCount: 1,
  brokerOffsetSecs: 7200,
  summary: { count: 1, wins: 0, losses: 1, breakeven: 0, net: -5.31 },
  trades: [trade()],
  ...overrides,
})

/** A data row's cell by column index (0-based, after the header row). */
function row(index = 1): HTMLElement {
  return screen.getAllByRole('row')[index]
}

describe('heldFor', () => {
  it('formats minutes, hours and days', () => {
    const t0 = new Date(2026, 8, 25, 12, 0).getTime()
    expect(heldFor(t0, t0 + 42 * 60_000)).toBe('42m')
    expect(heldFor(t0, t0 - 60_000)).toBe('0m')
    expect(heldFor(t0, t0 + 21 * 3_600_000 + 18 * 60_000)).toBe('21h 18m')
    expect(heldFor(t0, t0 + 26 * 3_600_000)).toBe('1d 2h')
  })
})

describe('reason labels and tones', () => {
  const cases: Array<{ reason: CloseReason; label: string; tone: string }> = [
    { reason: 'take_profit', label: 'Take profit', tone: 'ok' },
    { reason: 'stop_loss', label: 'Stop loss', tone: 'bad' },
    { reason: 'break_even_stop', label: 'Break-even stop', tone: 'warn' },
    { reason: 'trailing_stop', label: 'Trailing stop', tone: 'warn' },
    { reason: 'harvest_stop', label: 'Harvest stop', tone: 'warn' },
    { reason: 'harvest_close', label: 'Harvest close', tone: 'ok' },
    { reason: 'agent_close', label: 'Closed by agent', tone: 'idle' },
    { reason: 'manual_close', label: 'Closed manually', tone: 'idle' },
    { reason: 'unknown', label: 'Closed at broker', tone: 'idle' },
  ]

  it('maps every reason to its label and tone', () => {
    for (const { reason, label, tone } of cases) {
      expect(REASON_LABEL[reason]).toBe(label)
      expect(REASON_TONE[reason]).toBe(tone)
    }
  })

  it('renders each reason as a dot plus its label', () => {
    const trades = cases.map((c, index) => trade({ ticket: index + 1, closeReason: c.reason }))
    render(
      <TradesPanel
        page={page({ trades, summary: { count: trades.length, wins: 2, losses: 4, breakeven: 0, net: 0 } })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    cases.forEach(({ label, tone }, index) => {
      const dataRow = row(index + 1)
      expect(within(dataRow).getByText(label)).toBeTruthy()
      const dot = dataRow.querySelector('.trd-reason .dot')
      expect(dot?.className).toContain(`is-${tone}`)
    })
  })
})

describe('TradesPanel formatting', () => {
  it('shows the closed time, side, lots, prices, held time, net and R', () => {
    render(<TradesPanel page={page()} range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    const dataRow = row()
    expect(within(dataRow).getByText('25 Sep 15:08')).toBeTruthy()
    expect(within(dataRow).getByText('GBPUSD')).toBeTruthy()
    const side = within(dataRow).getByText('Short')
    expect(side.className).toContain('tone-bad')
    expect(within(dataRow).getByText('0.01')).toBeTruthy()
    // 1.32123 has five decimals — the widest across this row's own prices.
    expect(within(dataRow).getByText('1.32123')).toBeTruthy()
    expect(within(dataRow).getByText('1.32650')).toBeTruthy()
    expect(within(dataRow).getByText('21h 18m')).toBeTruthy()
    const net = within(dataRow).getByText('−5.31')
    expect(net.className).toContain('tone-bad')
    expect(within(dataRow).getByText('−1.00R')).toBeTruthy()
  })

  it('tones a long side and a winning net as ok', () => {
    render(
      <TradesPanel
        page={page({ trades: [trade({ side: 'long', net: 12.4, rMultiple: 1.5 })] })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    const dataRow = row()
    expect(within(dataRow).getByText('Long').className).toContain('tone-ok')
    expect(within(dataRow).getByText('+12.40').className).toContain('tone-ok')
    expect(within(dataRow).getByText('+1.50R')).toBeTruthy()
  })

  it('reads a flat net and a missing R as plain', () => {
    render(
      <TradesPanel
        page={page({ trades: [trade({ net: 0, rMultiple: null })] })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    const dataRow = row()
    const net = within(dataRow).getByText('0.00')
    expect(net.className).not.toContain('tone-')
    expect(within(dataRow).getByText('—')).toBeTruthy()
  })

  it('reads an R multiple of exactly zero as unsigned', () => {
    render(<TradesPanel page={page({ trades: [trade({ rMultiple: 0 })] })} range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(within(row()).getByText('0.00R')).toBeTruthy()
  })

  it('quotes a whole-number price at two decimals, having no fraction of its own', () => {
    render(
      <TradesPanel
        page={page({ trades: [trade({ openPrice: 158, closePrice: 159, stopLoss: 0, takeProfit: 0 })] })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    const dataRow = row()
    expect(within(dataRow).getByText('158.00')).toBeTruthy()
    expect(within(dataRow).getByText('159.00')).toBeTruthy()
  })

  it('shows the compact summary, net toned by sign', () => {
    render(
      <TradesPanel
        page={page({ summary: { count: 48, wins: 45, losses: 3, breakeven: 0, net: 27.56 } })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    const summary = document.querySelector('.trd-summary') as HTMLElement
    expect(summary.textContent).toBe('48 trades · 45 wins · 3 losses · +27.56')
    expect(summary.querySelector('.tone-ok')).toBeTruthy()
  })

  it('names breakeven trades in the summary when there are any', () => {
    render(
      <TradesPanel
        page={page({ summary: { count: 10, wins: 4, losses: 4, breakeven: 2, net: -3.2 } })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    const summary = document.querySelector('.trd-summary') as HTMLElement
    expect(summary.textContent).toBe('10 trades · 4 wins · 4 losses · 2 breakeven · −3.20')
    expect(summary.querySelector('.tone-bad')).toBeTruthy()
  })
})

describe('row expansion', () => {
  it('opens to show the rationale, close detail and figures, then closes again', () => {
    render(
      <TradesPanel
        page={page({
          trades: [trade({ closeDetail: 'Harvest stop trailed to lock in profit.', swap: -0.4, commission: -0.1 })],
        })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    const toggle = screen.getByRole('button', { name: 'Details for GBPUSD short' })
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    expect(screen.queryByText(/aligned bearish evidence/)).toBeNull()

    fireEvent.click(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    expect(screen.getByText(/aligned bearish evidence/)).toBeTruthy()
    expect(screen.getByText('Harvest stop trailed to lock in profit.')).toBeTruthy()
    expect(screen.getByText('10655087')).toBeTruthy()
    expect(screen.getByText('24 Sep, 17:50')).toBeTruthy()
    expect(screen.getByText('1.32990')).toBeTruthy() // stop
    expect(screen.getByText('1.31620')).toBeTruthy() // target
    expect(screen.getByText('−0.40')).toBeTruthy() // swap
    expect(screen.getByText('−0.10')).toBeTruthy() // commission

    fireEvent.click(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    expect(screen.queryByText(/aligned bearish evidence/)).toBeNull()
  })

  it('omits rationale and close-detail paragraphs when the service has none', () => {
    render(
      <TradesPanel
        page={page({ trades: [trade({ entryRationale: null, closeDetail: null })] })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    fireEvent.click(screen.getByRole('button', { name: 'Details for GBPUSD short' }))
    expect(document.querySelectorAll('.trd-rationale')).toHaveLength(0)
    // A stop and target still show even with no accompanying prose.
    expect(screen.getByText('Ticket')).toBeTruthy()
  })

  it('reads an absent stop or target as a dash', () => {
    render(
      <TradesPanel
        page={page({ trades: [trade({ stopLoss: 0, takeProfit: 0 })] })}
        range={30}
        onRangeChange={() => {}} onPageChange={() => {}}
      />,
    )
    fireEvent.click(screen.getByRole('button', { name: 'Details for GBPUSD short' }))
    const dashes = screen.getAllByText('—')
    expect(dashes.length).toBeGreaterThanOrEqual(2)
  })
})

describe('range switching', () => {
  it('marks the active range and reports a change', () => {
    const onRangeChange = vi.fn()
    render(<TradesPanel page={page()} range={90} onRangeChange={onRangeChange} onPageChange={() => {}} />)
    const group = screen.getByRole('group', { name: 'Range' })
    expect(within(group).getByRole('button', { name: '90D' }).getAttribute('aria-pressed')).toBe('true')
    expect(within(group).getByRole('button', { name: '30D' }).getAttribute('aria-pressed')).toBe('false')

    fireEvent.click(within(group).getByRole('button', { name: '7D' }))
    expect(onRangeChange).toHaveBeenCalledWith(7)

    fireEvent.click(within(group).getByRole('button', { name: '1Y' }))
    expect(onRangeChange).toHaveBeenCalledWith(365)
  })
})

describe('loading, error, empty and truncated states', () => {
  it('shows a loading skeleton before the first page arrives', () => {
    const { container } = render(<TradesPanel range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(container.querySelectorAll('.trd-loading .skeleton').length).toBeGreaterThan(0)
    expect(screen.queryByRole('table')).toBeNull()
  })

  it('reads Unavailable when the poll failed', () => {
    render(<TradesPanel error="HTTP 500" range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    const unavailable = screen.getByText('Unavailable')
    expect(unavailable.className).toContain('tone-bad')
    expect(screen.queryByRole('table')).toBeNull()
  })

  it('reads Unavailable over a stale page when a later poll fails', () => {
    render(<TradesPanel page={page()} error="HTTP 500" range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(screen.getByText('Unavailable')).toBeTruthy()
    expect(screen.queryByRole('table')).toBeNull()
  })

  it('shows an empty message when the range has no closed trades', () => {
    render(<TradesPanel page={page({ trades: [] })} range={7} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(screen.getByText('No closed trades in this range')).toBeTruthy()
    expect(screen.queryByRole('table')).toBeNull()
  })

  it('shows a quiet truncated chip only when the service truncated the window', () => {
    const { rerender } = render(<TradesPanel page={page({ truncated: true })} range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(screen.getByText('Truncated').className).toContain('tone-warn')

    rerender(<TradesPanel page={page({ truncated: false })} range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(screen.queryByText('Truncated')).toBeNull()
  })

  it('names the trades panel', () => {
    render(<TradesPanel page={page()} range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(screen.getByRole('heading', { name: 'Trades' })).toBeTruthy()
  })
})

describe('TradesPanel paging', () => {
  it('shows no pager when the range fits on one page', () => {
    render(<TradesPanel page={page()} range={30} onRangeChange={() => {}} onPageChange={() => {}} />)
    expect(screen.queryByRole('button', { name: 'Next page' })).toBeNull()
  })

  it('reports the window position and asks for the neighbouring pages', () => {
    const onPageChange = vi.fn()
    const trades = [trade({ ticket: 21 }), trade({ ticket: 22 })]
    render(
      <TradesPanel
        page={page({
          page: 2,
          pageSize: 20,
          pageCount: 3,
          trades,
          summary: { count: 42, wins: 40, losses: 2, breakeven: 0, net: 12 },
        })}
        range={30}
        onRangeChange={() => {}}
        onPageChange={onPageChange}
      />,
    )
    expect(screen.getByText('21–22 of 42')).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Previous page' }))
    fireEvent.click(screen.getByRole('button', { name: 'Next page' }))
    expect(onPageChange.mock.calls).toEqual([[1], [3]])
  })
})
