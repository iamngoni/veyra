/**
 * Render tests for the console chrome: the status list the operator reads
 * first, the theme and settings controls, and the navigation rail.
 */

import { cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { AutopilotStatus, Status } from '../lib/api'
import { Sidebar, Topbar, type NavTab } from './shell'

afterEach(cleanup)

const autopilot: AutopilotStatus = {
  enabled: true,
  interval_secs: 30,
  timeframe: 'H4',
  tier: 'balanced',
  bars: 48,
  symbol: 'EURUSD',
  symbols: ['EURUSD'],
  jev: 'auto',
  breakeven_r: 1,
  trail_r: 1,
}

const live: Status = {
  service: 'veyra',
  version: '0.1.0',
  environment: 'development',
  broker_provider: 'ea',
  market_provider: 'ea',
  model_provider: 'openrouter',
  jev_provider: 'typesafe',
  persistence: 'postgres',
  broker_connected: true,
  trading_enabled: true,
  ea_live_orders: true,
  autopilot,
  model_budget: null,
  jev_usage: null,
  decisions: { consecutiveFailures: 0, lastFailure: null, lastFailureAt: null },
  risk_policy: {
    killSwitch: false,
    allowTradingWithoutJev: false,
    symbols: ['EURUSD'],
    maxVolumePerOrder: 0.01,
    maxTotalLots: 0.01,
    maxOpenOrders: 1,
    duplicateWindowSecs: 60,
    sessionUtc: null,
    maxRiskPercent: 12,
    maxDailyLossPercent: 10,
    maxPeakDrawdownPercent: 25,
    maxNetFactorLots: 0.01,
    calendarBlackoutMinutes: 30,
    minStopAtrFraction: 0.25,
    weekendPositions: 'agent',
  },
}

function renderTopbar(status?: Status, theme: 'dark' | 'light' = 'dark') {
  const onToggleTheme = vi.fn()
  const onOpenSettings = vi.fn()
  render(<Topbar status={status} theme={theme} onToggleTheme={onToggleTheme} onOpenSettings={onOpenSettings} />)
  return { onToggleTheme, onOpenSettings }
}

/** The strip item carrying `text`, and the tone its dot shows. */
function item(text: string) {
  const li = screen.getByText(text).closest('li') as HTMLLIElement
  const tone = li.querySelector('.dot')?.className.replace('dot is-', '')
  return { li, tone }
}

function stripLabels() {
  return within(screen.getByRole('list', { name: 'System status' }))
    .getAllByRole('listitem')
    .map((li) => li.textContent)
}

describe('Topbar status strip', () => {
  it('shows a single idle Connecting item before the first status arrives', () => {
    renderTopbar()
    expect(stripLabels()).toEqual(['Connecting'])
    expect(item('Connecting').tone).toBe('idle')
    expect(item('Connecting').li.getAttribute('title')).toBeNull()
  })

  it('reads all green when the service is live', () => {
    renderTopbar(live)
    expect(stripLabels()).toEqual(['LIVE', 'Terminal connected', 'EA armed', 'Trading enabled', 'Autopilot 30s · H4'])
    for (const label of stripLabels()) expect(item(label as string).tone).toBe('ok')
    // A healthy verdict needs no explanation.
    expect(item('LIVE').li.getAttribute('title')).toBeNull()
  })

  it.each([
    ['HALTED', 'bad', { risk_policy: { ...live.risk_policy, killSwitch: true } }],
    [
      'NOT DECIDING',
      'bad',
      { decisions: { consecutiveFailures: 3, lastFailure: 'timeout', lastFailureAt: 1, lastModel: 'm1' } },
    ],
    ['NO LINK', 'bad', { broker_connected: false }],
    ['STANDBY', 'idle', { trading_enabled: false }],
    ['DRY RUN', 'warn', { ea_live_orders: false }],
  ] as const)('shows %s with a %s dot and the reason as a tooltip', (label, tone, patch) => {
    renderTopbar({ ...live, ...patch } as Status)
    const posture = item(label)
    expect(posture.tone).toBe(tone)
    expect(posture.li.getAttribute('title')).toBeTruthy()
    // Only the dot carries the state; the words are never coloured.
    expect(screen.getByText(label).className).toBe('')
  })

  it('names the reason in the tooltip and under the verdict', () => {
    renderTopbar({ ...live, broker_connected: false })
    const posture = item('NO LINK').li
    expect(posture.getAttribute('title')).toBe('terminal is not reporting')
    expect(posture.querySelector('.shell-status-detail')?.textContent).toBe('terminal is not reporting')
  })

  it('writes no reason line while the verdict is healthy', () => {
    const { container } = render(<Topbar status={live} theme="dark" onToggleTheme={() => {}} onOpenSettings={() => {}} />)
    expect(container.querySelector('.shell-status-detail')).toBeNull()
  })

  it('keeps the status list beside the header rather than inside it', () => {
    renderTopbar(live)
    const header = screen.getByRole('banner')
    const list = screen.getByRole('list', { name: 'System status' })
    expect(header.contains(list)).toBe(false)
    expect(within(header).getAllByRole('button').map((button) => button.getAttribute('aria-label'))).toEqual([
      'Switch to light theme',
      'Settings',
    ])
  })

  it('reports each switch when it is off', () => {
    renderTopbar({ ...live, broker_connected: false, ea_live_orders: false, trading_enabled: false, autopilot: null })
    expect(item('Terminal stale').tone).toBe('bad')
    expect(item('EA disarmed').tone).toBe('idle')
    expect(item('Trading disabled').tone).toBe('idle')
    expect(item('Autopilot off').tone).toBe('idle')
  })

  it('treats a configured but disabled autopilot as off', () => {
    renderTopbar({ ...live, autopilot: { ...autopilot, enabled: false } })
    expect(item('Autopilot off').tone).toBe('idle')
  })

  it.each([
    [45, 'Autopilot 45s · H4'],
    [90, 'Autopilot 90s · H4'],
    [300, 'Autopilot 5m · H4'],
    [7200, 'Autopilot 2h · H4'],
  ])('writes a %ss interval as the largest whole unit', (secs, label) => {
    renderTopbar({ ...live, autopilot: { ...autopilot, interval_secs: secs } })
    expect(item(label).tone).toBe('ok')
  })
})

describe('Topbar controls', () => {
  it('offers the light theme while dark', () => {
    const { onToggleTheme } = renderTopbar(live, 'dark')
    const button = screen.getByRole('button', { name: 'Switch to light theme' })
    expect(button.getAttribute('title')).toBe('Switch to light theme')
    // The sun glyph has rays; the moon is a single path.
    expect(button.querySelectorAll('circle')).toHaveLength(1)
    fireEvent.click(button)
    expect(onToggleTheme).toHaveBeenCalledTimes(1)
  })

  it('offers the dark theme while light', () => {
    const { onToggleTheme } = renderTopbar(live, 'light')
    const button = screen.getByRole('button', { name: 'Switch to dark theme' })
    expect(button.getAttribute('title')).toBe('Switch to dark theme')
    expect(button.querySelectorAll('circle')).toHaveLength(0)
    fireEvent.click(button)
    expect(onToggleTheme).toHaveBeenCalledTimes(1)
  })

  it('opens settings from the gear', () => {
    const { onOpenSettings, onToggleTheme } = renderTopbar(live)
    const gear = screen.getByRole('button', { name: 'Settings' })
    // Hidden by the stylesheet wherever the Settings view is in the sidebar.
    expect(gear.className).toBe('icon-button shell-settings')
    fireEvent.click(gear)
    expect(onOpenSettings).toHaveBeenCalledTimes(1)
    expect(onToggleTheme).not.toHaveBeenCalled()
  })

  it('shows the wordmark', () => {
    renderTopbar()
    expect(screen.getByText('Veyra').className).toBe('shell-wordmark')
  })
})

const tabs: ReadonlyArray<NavTab> = [
  { id: 'overview', label: 'Overview', icon: 'overview' },
  { id: 'activity', label: 'Activity', icon: 'activity' },
  { id: 'settings', label: 'Settings', icon: 'settings' },
]

describe('Sidebar', () => {
  it('marks only the active view as the current page', () => {
    render(<Sidebar tabs={tabs} active="activity" onSelect={() => {}} />)
    const nav = screen.getByRole('navigation', { name: 'Main' })
    const buttons = within(nav).getAllByRole('button')
    expect(buttons.map((button) => button.textContent)).toEqual(['Overview', 'Activity', 'Settings'])
    expect(within(nav).getByRole('button', { name: 'Activity' }).getAttribute('aria-current')).toBe('page')
    expect(within(nav).getByRole('button', { name: 'Overview' }).getAttribute('aria-current')).toBeNull()
    expect(within(nav).getByRole('button', { name: 'Settings' }).getAttribute('aria-current')).toBeNull()
  })

  it('selects a view by id', () => {
    const onSelect = vi.fn()
    render(<Sidebar tabs={tabs} active="overview" onSelect={onSelect} />)
    fireEvent.click(screen.getByRole('button', { name: 'Settings' }))
    expect(onSelect).toHaveBeenCalledWith('settings')
  })

  it('draws each view with its icon', () => {
    render(<Sidebar tabs={tabs} active="overview" onSelect={() => {}} />)
    for (const button of screen.getAllByRole('button')) expect(button.querySelector('svg.icon')).not.toBeNull()
  })

  it('holds nothing but the views', () => {
    const { container } = render(<Sidebar tabs={tabs} active="overview" onSelect={() => {}} />)
    expect(screen.queryByRole('img')).toBeNull()
    expect(container.querySelectorAll('.shell-sidebar > *')).toHaveLength(1)
  })

  it('draws every view icon as a stroked outline, never a filled shape', () => {
    const all: ReadonlyArray<NavTab> = [
      ...tabs,
      { id: 'risk', label: 'Risk', icon: 'risk' },
      { id: 'trace', label: 'Trace', icon: 'trace' },
      { id: 'diagnostics', label: 'Diagnostics', icon: 'diagnostics' },
    ]
    const { container } = render(<Sidebar tabs={all} active="overview" onSelect={() => {}} />)
    const icons = container.querySelectorAll('.shell-nav-item svg.icon')
    expect(icons).toHaveLength(6)
    for (const icon of icons) {
      expect(icon.children.length).toBeGreaterThan(0)
      expect(icon.querySelector('[fill], [stroke]')).toBeNull()
    }
  })
})

describe('Sidebar as a sideways strip', () => {
  const scrollIntoView = vi.fn()
  let overflow = 0

  beforeEach(() => {
    overflow = 0
    scrollIntoView.mockClear()
    // jsdom has no layout: fake the strip's widths and the scroll call.
    Object.defineProperty(HTMLElement.prototype, 'scrollWidth', { configurable: true, get: () => 300 + overflow })
    Object.defineProperty(HTMLElement.prototype, 'clientWidth', { configurable: true, get: () => 300 })
    Element.prototype.scrollIntoView = scrollIntoView
  })

  afterEach(() => {
    delete (HTMLElement.prototype as { scrollWidth?: number }).scrollWidth
    delete (HTMLElement.prototype as { clientWidth?: number }).clientWidth
    delete (Element.prototype as { scrollIntoView?: unknown }).scrollIntoView
  })

  it('scrolls a view opened from elsewhere into sight', () => {
    overflow = 200
    const { rerender } = render(<Sidebar tabs={tabs} active="overview" onSelect={() => {}} />)
    scrollIntoView.mockClear()
    rerender(<Sidebar tabs={tabs} active="settings" onSelect={() => {}} />)
    expect(scrollIntoView).toHaveBeenCalledTimes(1)
    expect(scrollIntoView).toHaveBeenCalledWith({ block: 'nearest', inline: 'nearest' })
    expect(scrollIntoView.mock.contexts[0]).toBe(screen.getByRole('button', { name: 'Settings' }))
  })

  it('leaves the rail alone when every view already fits', () => {
    const { rerender } = render(<Sidebar tabs={tabs} active="overview" onSelect={() => {}} />)
    rerender(<Sidebar tabs={tabs} active="settings" onSelect={() => {}} />)
    expect(scrollIntoView).not.toHaveBeenCalled()
  })

  it('scrolls nothing when no view is current', () => {
    overflow = 200
    render(<Sidebar tabs={tabs} active="missing" onSelect={() => {}} />)
    expect(screen.getByRole('navigation', { name: 'Main' }).querySelector('[aria-current]')).toBeNull()
    expect(scrollIntoView).not.toHaveBeenCalled()
  })
})

