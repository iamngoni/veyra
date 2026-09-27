/**
 * Render tests for the status banner: no strip without advisories or after a
 * failed read, one strip per advisory with its severity, end times on the
 * browser's own clock, and the "+N more" disclosure.
 *
 * Every instant is built from local dates (mid-July, clear of any daylight
 * saving change), so the expectations hold in whatever zone the tests run.
 */

import { cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { Advisories, Advisory } from '../lib/api'
import { StatusBanner, countdown, untilDetail, untilLabel } from './banner'

/** Wednesday 15 July 2026 on the local clock; hours and minutes as given. */
const at = (day: number, hours: number, minutes = 0) => new Date(2026, 6, day, hours, minutes).getTime()

const NOW = at(15, 10, 5)
const MINUTE = 60_000

const killSwitch: Advisory = { id: 'kill_switch', severity: 'critical', title: 'Kill switch is on', detail: null, untilMs: null }
const stale: Advisory = { id: 'broker_stale', severity: 'warning', title: 'Terminal not reporting' }
const closed: Advisory = {
  id: 'market_closed',
  severity: 'info',
  title: 'FX, gold and indices are closed',
  detail: 'BTC and ETH still trade.',
  untilMs: at(15, 12, 0),
}
const rollover: Advisory = { id: 'rollover_pause', severity: 'info', title: 'Daily rollover pause', untilMs: at(15, 10, 50) }

const served = (...items: Advisory[]): Advisories => ({ items, generatedAtMs: NOW })

const rows = () => within(screen.getByRole('list', { name: 'Notices' })).getAllByRole('listitem')

beforeEach(() => {
  // Only the clock is faked; nothing here waits on a timer.
  vi.useFakeTimers({ toFake: ['Date'] })
  vi.setSystemTime(NOW)
})

afterEach(() => {
  cleanup()
  vi.useRealTimers()
})

describe('StatusBanner', () => {
  it('shows no strip before the first answer, with nothing to say, or when the read fails', () => {
    const { container, rerender } = render(<StatusBanner />)
    const banner = container.firstElementChild as HTMLElement
    // The live region stays mounted, empty, so a later strip is announced.
    const live = screen.getByRole('status')
    expect(banner.className).toBe('banner')
    expect(live.childElementCount).toBe(0)
    expect(banner.textContent).toBe('')

    rerender(<StatusBanner advisories={served()} />)
    expect(banner.className).toBe('banner')
    expect(screen.queryByRole('list')).toBeNull()

    // A failed read hides the last good list rather than leaving it stale.
    rerender(<StatusBanner advisories={served(killSwitch, stale, closed)} error="/advisories → 503" />)
    expect(screen.queryByRole('list')).toBeNull()
    expect(screen.queryByRole('button')).toBeNull()

    // An unreadable body is not a list of advisories.
    rerender(<StatusBanner advisories={{ generatedAtMs: NOW } as unknown as Advisories} />)
    expect(screen.queryByRole('list')).toBeNull()
  })

  it('draws a strip per advisory in the served order, severity on the dot and the strip', () => {
    const { container } = render(<StatusBanner advisories={served(killSwitch, stale)} />)
    expect(container.firstElementChild?.className).toBe('banner is-shown')

    const live = screen.getByRole('status')
    expect(live.getAttribute('aria-live')).toBe('polite')
    expect(live.contains(screen.getByRole('list', { name: 'Notices' }))).toBe(true)

    const [critical, warning] = rows()
    expect(critical.className).toBe('banner-row is-critical')
    expect(within(critical).getByRole('img', { name: 'Critical' }).className).toBe('dot is-bad')
    expect(within(critical).getByText('Kill switch is on').tagName).toBe('STRONG')
    expect(warning.className).toBe('banner-row is-warning')
    expect(within(warning).getByRole('img', { name: 'Warning' }).className).toBe('dot is-warn')
    expect(warning.textContent).toBe('Terminal not reporting')
    // Neither carries a detail or an end time.
    expect(container.querySelector('.banner-detail')).toBeNull()
    expect(container.querySelector('.banner-until')).toBeNull()
  })

  it('sets the detail in the muted voice and keeps its full text on hover', () => {
    render(<StatusBanner advisories={served(closed)} />)
    const [row] = rows()
    expect(row.className).toBe('banner-row is-info')
    expect(within(row).getByRole('img', { name: 'Info' }).className).toBe('dot is-idle')
    const detail = within(row).getByText('BTC and ETH still trade.')
    expect(detail.className).toBe('banner-detail')
    expect(detail.getAttribute('title')).toBe('BTC and ETH still trade.')
  })

  it('reads a severity it does not know as information', () => {
    render(<StatusBanner advisories={served({ id: 'new_kind', severity: 'notice' as Advisory['severity'], title: 'Something new' })} />)
    const [row] = rows()
    expect(row.className).toBe('banner-row is-info')
    expect(within(row).getByRole('img', { name: 'Info' })).toBeTruthy()
  })

  it('states the end time on the local clock, with the full time and countdown behind its mark', () => {
    render(<StatusBanner advisories={served(closed)} />)
    const until = document.querySelector('.banner-until') as HTMLElement
    const mark = within(until).getByRole('button', { name: 'About when this ends' })
    const tip = within(until).getByRole('tooltip', { hidden: true })
    expect(until.firstChild?.textContent).toBe('until 12:00')
    expect(mark.getAttribute('aria-describedby')).toBe(tip.id)
    expect(tip.textContent).toBe('Expected to end Wed 15 Jul, 12:00 your time — in 1h 55m.')
  })

  it('moves only the hidden countdown as time passes, so a repeat poll changes no visible word', () => {
    const { rerender } = render(<StatusBanner advisories={served(closed)} />)
    const until = document.querySelector('.banner-until') as HTMLElement
    const tip = within(until).getByRole('tooltip', { hidden: true })

    vi.setSystemTime(NOW + MINUTE)
    rerender(<StatusBanner advisories={served(closed)} />)
    expect(until.firstChild?.textContent).toBe('until 12:00')
    expect(tip.textContent).toBe('Expected to end Wed 15 Jul, 12:00 your time — in 1h 54m.')
  })

  it('folds past two strips behind a disclosure that sits outside the live region', () => {
    render(<StatusBanner advisories={served(killSwitch, stale, closed, rollover)} />)
    expect(rows().map((row) => row.textContent)).toEqual(['Kill switch is on', 'Terminal not reporting'])

    const more = screen.getByRole('button', { name: '+2 more' })
    expect(more.getAttribute('aria-expanded')).toBe('false')
    expect(more.getAttribute('aria-controls')).toBe(screen.getByRole('list', { name: 'Notices' }).id)
    expect(screen.getByRole('status').contains(more)).toBe(false)

    fireEvent.click(more)
    expect(rows()).toHaveLength(4)
    expect(more.textContent).toBe('Show less')
    expect(more.getAttribute('aria-expanded')).toBe('true')
    expect(within(rows()[3]).getByText('Daily rollover pause')).toBeTruthy()

    fireEvent.click(more)
    expect(rows()).toHaveLength(2)
    expect(more.textContent).toBe('+2 more')
  })

  it('needs no disclosure for two strips', () => {
    render(<StatusBanner advisories={served(killSwitch, closed)} />)
    expect(rows()).toHaveLength(2)
    expect(screen.queryByRole('button', { name: /more/ })).toBeNull()
  })
})

describe('end-time wording', () => {
  it('names the clock time today, the weekday this week and the date beyond it', () => {
    expect(untilLabel(at(15, 12, 0), NOW)).toBe('until 12:00')
    expect(untilLabel(at(16, 1, 0), NOW)).toBe('until Thu 01:00')
    expect(untilLabel(at(20, 23, 30), NOW)).toBe('until Mon 23:30')
    // Six days out a weekday could mean this week or next.
    expect(untilLabel(at(21, 10, 5), NOW)).toBe('until 21 Jul 10:05')
    expect(untilLabel(at(30, 9, 30), NOW)).toBe('until 30 Jul 09:30')
    // Overdue from an earlier day still names its day.
    expect(untilLabel(at(14, 23, 0), NOW)).toBe('until Tue 23:00')
  })

  it('counts down in the largest units, rounded up to the minute', () => {
    expect(countdown(at(15, 12, 0), NOW)).toBe('in 1h 55m')
    expect(countdown(at(15, 12, 5), NOW)).toBe('in 2h')
    expect(countdown(at(16, 10, 5), NOW)).toBe('in 1d')
    expect(countdown(at(17, 14, 5), NOW)).toBe('in 2d 4h')
    expect(countdown(NOW + 12 * MINUTE, NOW)).toBe('in 12m')
    expect(countdown(NOW + 30_000, NOW)).toBe('in 1m')
    expect(countdown(NOW, NOW)).toBe('any moment now')
    expect(countdown(NOW - MINUTE, NOW)).toBe('any moment now')
  })

  it('spells out the full local date in the tooltip', () => {
    expect(untilDetail(at(20, 1, 0), NOW)).toBe('Expected to end Mon 20 Jul, 01:00 your time — in 4d 14h.')
    expect(untilDetail(at(15, 10, 0), NOW)).toBe('Expected to end Wed 15 Jul, 10:00 your time — any moment now.')
  })
})
