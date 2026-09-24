/**
 * Render tests for the overview rail: every autopilot state, the activity
 * preview's filtering and empty states, and the confirm-before-change flow of
 * both risk switches.
 */

import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { ClosedTrade, FeedEvent, RiskPolicy, Status } from '../lib/api'
import { AutopilotCard, RecentActivity, RiskControls } from './rail'

afterEach(cleanup)

const policy: RiskPolicy = {
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
}

const status: Status = {
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
  autopilot: {
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
  },
  model_budget: null,
  jev_usage: null,
  decisions: { consecutiveFailures: 0, lastFailure: null, lastFailureAt: null },
  risk_policy: policy,
}

/** Local wall-clock time, so the expected strings hold in any time zone. */
const at = (hours: number, minutes: number, day = 23) => new Date(2026, 8, day, hours, minutes).getTime()

let seq = 0
const event = (kind: string, atMs: number, payload: Record<string, unknown> = {}): FeedEvent => ({
  seq: ++seq,
  at_ms: atMs,
  kind,
  payload,
})

/** The text of the value cell next to a row label. */
const value = (label: string) => screen.getByText(label).nextElementSibling as HTMLElement

/** The header state indicator (dot plus word), when shown. */
const state = (container: HTMLElement) => container.querySelector('.ap-state')

describe('AutopilotCard', () => {
  it('shows placeholders and no state while status has not arrived', () => {
    const { container } = render(<AutopilotCard events={[]} />)
    expect(state(container)).toBeNull()
    expect(value('Cadence').querySelector('.skeleton')).toBeTruthy()
    expect(value('Status').querySelector('.skeleton')).toBeTruthy()
    expect(value('Last decision').textContent).toBe('—')
  })

  it('reads the newest decision from the feed while running', () => {
    const events = [
      event('command_completed', at(17, 58), { kind: 'rates' }),
      event('proposal_evaluated', at(17, 56), {
        outcome: 'no_trade',
        symbol: 'EURUSD',
        rationale: 'Conditions not met.',
      }),
      event('proposal_evaluated', at(16, 10), { outcome: 'queued', symbol: 'GBPUSD' }),
    ]
    const { container } = render(<AutopilotCard status={status} events={events} />)

    const indicator = state(container) as HTMLElement
    expect(indicator.textContent).toBe('Running')
    expect(indicator.className).toContain('tone-ok')
    expect(indicator.querySelector('.dot.is-ok')).toBeTruthy()

    expect(value('Cadence').textContent).toBe('30 seconds · H4')
    const last = value('Last decision').querySelector('time') as HTMLElement
    expect(last.textContent).toBe('23 Sep 2026, 17:56')
    expect(last.getAttribute('dateTime')).toBe(new Date(at(17, 56)).toISOString())
    expect(value('Status').querySelector('.ap-outcome')?.textContent).toBe('No trade')
    expect(value('Status').querySelector('.ap-note')?.textContent).toBe('EURUSD — Conditions not met.')
  })

  it('pads single-digit hours and minutes', () => {
    render(<AutopilotCard status={status} events={[event('proposal_evaluated', at(7, 5, 3), { outcome: 'held' })]} />)
    expect(value('Last decision').textContent).toBe('3 Sep 2026, 07:05')
    // A decision without a reason or symbol has no second line.
    expect(value('Status').querySelector('.ap-note')).toBeNull()
    expect(value('Status').textContent).toBe('Trade held')
  })

  it('holds placeholders for the decision until the feed has answered', () => {
    render(<AutopilotCard status={status} events={[]} loading />)
    expect(value('Last decision').querySelector('.skeleton')).toBeTruthy()
    expect(value('Status').querySelector('.skeleton')).toBeTruthy()
  })

  it('says a dash when running with no decision yet', () => {
    render(<AutopilotCard status={status} events={[event('agent_turn', at(12, 0))]} />)
    expect(value('Last decision').textContent).toBe('—')
    expect(value('Status').textContent).toBe('—')
  })

  it('reports repeated decision failures ahead of the run state', () => {
    const failing: Status = {
      ...status,
      decisions: { consecutiveFailures: 3, lastFailure: 'provider refused the request', lastFailureAt: at(17, 50) },
    }
    const events = [event('proposal_evaluated', at(17, 40), { outcome: 'no_trade' })]
    const { container } = render(<AutopilotCard status={failing} events={events} />)

    const indicator = state(container) as HTMLElement
    expect(indicator.textContent).toBe('Not deciding')
    expect(indicator.className).toContain('tone-bad')
    expect(indicator.querySelector('.dot.is-bad')).toBeTruthy()
    expect(value('Status').querySelector('.ap-outcome')?.textContent).toBe('Not deciding')
    expect(value('Status').querySelector('.ap-note')?.textContent).toBe('provider refused the request')
    // The last completed decision is still stated.
    expect(value('Last decision').textContent).toBe('23 Sep 2026, 17:40')
  })

  it('omits the failure line when the service gives no reason', () => {
    const failing: Status = {
      ...status,
      decisions: { consecutiveFailures: 2, lastFailure: null, lastFailureAt: null },
    }
    render(<AutopilotCard status={failing} events={[]} />)
    expect(value('Status').textContent).toBe('Not deciding')
  })

  it('treats a single failure as still running', () => {
    const once: Status = {
      ...status,
      decisions: { consecutiveFailures: 1, lastFailure: 'timeout', lastFailureAt: at(17, 0) },
    }
    const { container } = render(<AutopilotCard status={once} events={[]} />)
    expect(state(container)?.textContent).toBe('Running')
  })

  it('shows off when the autopilot is disabled', () => {
    const off: Status = {
      ...status,
      autopilot: { ...(status.autopilot as NonNullable<Status['autopilot']>), enabled: false },
    }
    const events = [event('proposal_evaluated', at(9, 30), { outcome: 'no_trade' })]
    const { container } = render(<AutopilotCard status={off} events={events} />)

    const indicator = state(container) as HTMLElement
    expect(indicator.textContent).toBe('Off')
    expect(indicator.className).toContain('tone-faint')
    expect(indicator.querySelector('.dot.is-off')).toBeTruthy()
    expect(value('Cadence').textContent).toBe('—')
    expect(value('Status').textContent).toBe('Off')
  })

  it('shows off when the service runs without an autopilot or decision counters', () => {
    const { container } = render(<AutopilotCard status={{ ...status, autopilot: null, decisions: null }} events={[]} />)
    expect(state(container)?.textContent).toBe('Off')
    expect(value('Cadence').textContent).toBe('—')
  })
})

const trade = (ticket: number, closeMs: number, fields: Partial<ClosedTrade> = {}): ClosedTrade => ({
  ticket,
  symbol: 'EURUSD',
  kind: 'buy',
  lots: 0.01,
  openPrice: 1.1,
  closePrice: 1.1012,
  openTime: closeMs / 1000 - 3600,
  closeTime: closeMs / 1000,
  profit: 1.2,
  swap: 0,
  commission: 0,
  magic: 0,
  ...fields,
})

/** The rendered rows as `time | title | detail`, top to bottom. */
const digest = (container: HTMLElement) =>
  [...container.querySelectorAll('.act-row')].map((row) =>
    [row.querySelector('time')?.textContent, row.querySelector('.act-title')?.textContent, row.querySelector('.act-detail')?.textContent ?? '']
      .join(' | ')
      .trim(),
  )

describe('RecentActivity', () => {
  // "Today" is the fixtures' day, so same-day rows show a clock time.
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['Date'] })
    vi.setSystemTime(new Date(2026, 8, 23, 20, 0))
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it('previews the three newest events that matter', () => {
    const events = [
      event('broker_snapshot', at(18, 0), { orders: 1 }),
      event('command_completed', at(17, 59), { kind: 'account_snapshot' }),
      event('command_queued', at(17, 58), { kind: 'open_order' }),
      event('command_completed', at(17, 58), { kind: 'open_order' }),
      event('agent_turn', at(17, 57)),
      event('agent_tool_called', at(17, 57), { tool: 'check_risk' }),
      event('proposal_evaluated', at(17, 56), { outcome: 'no_trade', symbol: 'EURUSD', reason: 'flat market' }),
      event('position_closed', at(14, 12), { symbol: 'GBPUSD', kind: 'buy', profit: 12.5 }),
      event('command_failed', at(9, 4), { reason: 'market closed' }),
      event('failure', at(8, 0), { reason: 'older than the preview' }),
    ]
    const { container } = render(<RecentActivity events={events} connected onViewAll={vi.fn()} />)

    const rows = container.querySelectorAll('.act-row')
    expect(rows).toHaveLength(3)
    const [first, second, third] = [...rows] as HTMLElement[]

    expect(within(first).getByText('17:56').getAttribute('dateTime')).toBe(new Date(at(17, 56)).toISOString())
    expect(within(first).getByText('No trade')).toBeTruthy()
    expect(within(first).getByText('EURUSD — flat market')).toBeTruthy()
    expect(first.querySelector('.dot.is-idle')).toBeTruthy()

    expect(within(second).getByText('14:12')).toBeTruthy()
    expect(within(second).getByText('Position closed')).toBeTruthy()
    expect(within(second).getByText('GBPUSD buy · +12.50')).toBeTruthy()
    expect(second.querySelector('.dot.is-ok')).toBeTruthy()

    expect(within(third).getByText('09:04')).toBeTruthy()
    expect(within(third).getByText('Command failed')).toBeTruthy()
    expect(third.querySelector('.dot.is-bad')).toBeTruthy()

    expect(screen.queryByText('older than the preview')).toBeNull()
    expect(screen.queryByText('Reconnecting')).toBeNull()
  })

  it('shows a warn tone and no detail line where the event has none', () => {
    const events = [event('proposal_evaluated', at(13, 47), { outcome: 'held' })]
    const { container } = render(<RecentActivity events={events} connected onViewAll={vi.fn()} />)
    expect(container.querySelector('.dot.is-warn')).toBeTruthy()
    expect(container.querySelector('.act-detail')).toBeNull()
  })

  it('holds placeholder rows until the feed or the history has answered', () => {
    const { container } = render(<RecentActivity events={[]} connected loading onViewAll={vi.fn()} />)
    expect(container.querySelectorAll('.act-row .skeleton').length).toBeGreaterThan(0)
    expect(screen.queryByText('No recent activity')).toBeNull()
  })

  it('says so when nothing has happened', () => {
    render(<RecentActivity events={[event('broker_snapshot', at(10, 0))]} connected onViewAll={vi.fn()} />)
    expect(screen.getByText('No recent activity')).toBeTruthy()
  })

  it('says it is reconnecting once when the feed is down and empty', () => {
    render(<RecentActivity events={[]} connected={false} onViewAll={vi.fn()} />)
    expect(screen.getAllByText('Reconnecting')).toHaveLength(1)
    expect(screen.queryByText('No recent activity')).toBeNull()
  })

  it('flags a lost feed in the header while keeping the last rows', () => {
    const { container } = render(
      <RecentActivity
        events={[event('failure', at(11, 20), { reason: 'judge timed out' })]}
        connected={false}
        onViewAll={vi.fn()}
      />,
    )
    expect(container.querySelector('.panel-actions .act-reconnecting')?.textContent).toBe('Reconnecting')
    expect(screen.getByText('Decision failed')).toBeTruthy()
  })

  it('merges closed trades with the feed, newest first', () => {
    const events = [
      event('proposal_evaluated', at(17, 56), { outcome: 'no_trade', symbol: 'EURUSD', reason: 'flat market' }),
      event('failure', at(12, 0), { reason: 'judge timed out' }),
    ]
    const trades = [
      trade(501, at(18, 30), { symbol: 'GBPUSD', kind: 'sell', lots: 0.02, profit: 3, swap: -0.25, commission: -0.15 }),
      trade(502, at(14, 5), { symbol: 'USDJPY', lots: 0.1, profit: -2, swap: 0.5, commission: 0 }),
      trade(503, at(9, 0)),
    ]
    const { container } = render(<RecentActivity events={events} trades={trades} connected onViewAll={vi.fn()} />)

    expect(digest(container)).toEqual([
      '18:30 | Position closed | GBPUSD Short 0.02 · +2.60',
      '17:56 | No trade | EURUSD — flat market',
      '14:05 | Position closed | USDJPY Long 0.10 · −1.50',
    ])
    const dots = [...container.querySelectorAll('.act-row .dot')].map((dot) => dot.className)
    expect(dots).toEqual(['dot is-ok', 'dot is-idle', 'dot is-bad'])
    const [first] = [...container.querySelectorAll('.act-row time')]
    expect(first.getAttribute('dateTime')).toBe(new Date(at(18, 30)).toISOString())
  })

  it('shows a flat close with a neutral dot', () => {
    const flat = trade(601, at(10, 0), { profit: 0.5, swap: -0.3, commission: -0.2 })
    const { container } = render(<RecentActivity events={[]} trades={[flat]} connected onViewAll={vi.fn()} />)
    expect(digest(container)).toEqual(['10:00 | Position closed | EURUSD Long 0.01 · 0.00'])
    expect(container.querySelector('.act-row .dot.is-idle')).toBeTruthy()
  })

  it('shows a close once, from the history, when the feed also reported it', () => {
    const events = [
      // The feed's last-seen profit, superseded by the realized net below.
      event('position_closed', at(16, 0), { ticket: 700, symbol: 'EURUSD', kind: 'buy', lots: 0.01, profit: 1.1 }),
      // Tickets can arrive as strings in older payloads.
      event('position_closed', at(15, 0), { ticket: '701', symbol: 'AUDUSD', kind: 'sell', lots: 0.02, profit: -0.4 }),
      // Not in the history yet, so the feed's row stands.
      event('position_closed', at(14, 0), { ticket: 703, symbol: 'USDJPY', kind: 'sell', lots: 0.03, profit: -0.8 }),
    ]
    const trades = [
      trade(700, at(16, 1), { profit: 1.3, swap: -0.05, commission: -0.05 }),
      trade(701, at(15, 1), { symbol: 'AUDUSD', kind: 'sell', lots: 0.02, profit: -0.3, swap: 0, commission: -0.1 }),
    ]
    const { container } = render(<RecentActivity events={events} trades={trades} connected onViewAll={vi.fn()} />)
    // Feed closes read exactly like history closes, toned by their result.
    expect(digest(container)).toEqual([
      '16:01 | Position closed | EURUSD Long 0.01 · +1.20',
      '15:01 | Position closed | AUDUSD Short 0.02 · −0.40',
      '14:00 | Position closed | USDJPY Short 0.03 · −0.80',
    ])
    const dots = [...container.querySelectorAll('.act-row .dot')].map((dot) => dot.className)
    expect(dots).toEqual(['dot is-ok', 'dot is-bad', 'dot is-bad'])
  })

  it('falls back to the generic wording for a close missing its fields', () => {
    const events = [event('position_closed', at(16, 0), { ticket: 710, symbol: 'EURUSD', kind: 'buy', profit: 1.2 })]
    const { container } = render(<RecentActivity events={events} connected onViewAll={vi.fn()} />)
    expect(digest(container)).toEqual(['16:00 | Position closed | EURUSD buy · +1.20'])
  })

  it('keeps a feed row ahead of a trade closed at the same moment', () => {
    const events = [event('failure', at(13, 0), { reason: 'judge timed out' })]
    const { container } = render(
      <RecentActivity events={events} trades={[trade(800, at(13, 0))]} connected onViewAll={vi.fn()} />,
    )
    expect(digest(container).map((row) => row.split(' | ')[1])).toEqual(['Decision failed', 'Position closed'])
  })

  it('dates rows from earlier days instead of showing a clock time', () => {
    const events = [event('failure', at(22, 15, 21), { reason: 'judge timed out' })]
    const trades = [trade(900, at(9, 40)), trade(901, at(23, 50, 22))]
    const { container } = render(<RecentActivity events={events} trades={trades} connected onViewAll={vi.fn()} />)
    expect(digest(container).map((row) => row.split(' | ')[0])).toEqual(['09:40', '22 Sep', '21 Sep'])
  })

  it('works from the feed alone when no trade history is passed', () => {
    const { container } = render(
      <RecentActivity events={[event('failure', at(11, 20), { reason: 'judge timed out' })]} connected onViewAll={vi.fn()} />,
    )
    expect(digest(container)).toEqual(['11:20 | Decision failed | judge timed out'])
  })

  it('counts trade rows when deciding whether the list is empty', () => {
    render(<RecentActivity events={[]} trades={[trade(950, at(8, 0))]} connected={false} onViewAll={vi.fn()} />)
    // The row stays readable and the header flags the drop.
    expect(screen.getByText('Position closed')).toBeTruthy()
    expect(screen.getAllByText('Reconnecting')).toHaveLength(1)
  })

  it('opens the activity tab from View all', () => {
    const onViewAll = vi.fn()
    render(<RecentActivity events={[]} connected onViewAll={onViewAll} />)
    fireEvent.click(screen.getByRole('button', { name: 'View all' }))
    expect(onViewAll).toHaveBeenCalledTimes(1)
  })
})

describe('RiskControls', () => {
  const killSwitch = () => screen.getByRole('switch', { name: 'Kill switch' }) as HTMLButtonElement
  const bypass = () => screen.getByRole('switch', { name: 'Judge bypass' }) as HTMLButtonElement

  it('disables both switches until the policy has loaded', () => {
    render(<RiskControls onApply={vi.fn()} />)
    expect(killSwitch().disabled).toBe(true)
    expect(bypass().disabled).toBe(true)
    expect(killSwitch().getAttribute('aria-checked')).toBe('false')
  })

  it('disables both switches without a way to apply them', () => {
    render(<RiskControls policy={policy} />)
    expect(killSwitch().disabled).toBe(true)
    expect(bypass().disabled).toBe(true)
  })

  it('describes each switch in its current state', () => {
    const { rerender } = render(<RiskControls policy={policy} onApply={vi.fn()} />)
    expect(screen.getByText('Blocks new orders; open positions stay open.')).toBeTruthy()
    expect(screen.getByText('Trading pauses when the judge cannot answer.')).toBeTruthy()
    expect(screen.getByRole('img', { name: 'Lets trading continue while the judge cannot answer.' }).getAttribute('title')).toBe(
      'Lets trading continue while the judge cannot answer.',
    )

    rerender(<RiskControls policy={{ ...policy, killSwitch: true, allowTradingWithoutJev: true }} onApply={vi.fn()} />)
    expect(screen.getByText('New orders refused; open positions stay open.')).toBeTruthy()
    expect(screen.getByText('Trading continues when the judge cannot answer.')).toBeTruthy()
    expect(killSwitch().getAttribute('aria-checked')).toBe('true')
    expect(bypass().getAttribute('aria-checked')).toBe('true')
  })

  it('asks before engaging the kill switch and can be cancelled', () => {
    const onApply = vi.fn()
    render(<RiskControls policy={policy} onApply={onApply} />)

    fireEvent.click(killSwitch())
    expect(screen.getByText('Engage kill switch?')).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }))
    expect(screen.queryByText('Engage kill switch?')).toBeNull()

    // A second click on the switch itself also withdraws the question.
    fireEvent.click(killSwitch())
    fireEvent.click(killSwitch())
    expect(screen.queryByText('Engage kill switch?')).toBeNull()
    expect(onApply).not.toHaveBeenCalled()
  })

  it('moves the question to the other switch when it is clicked', () => {
    render(<RiskControls policy={policy} onApply={vi.fn()} />)
    fireEvent.click(killSwitch())
    fireEvent.click(bypass())
    expect(screen.queryByText('Engage kill switch?')).toBeNull()
    expect(screen.getByText('Enable judge bypass?')).toBeTruthy()
  })

  it('locks both switches while a change is applying', async () => {
    let resolve: (value: string | undefined) => void = () => {}
    const onApply = vi.fn(() => new Promise<string | undefined>((done) => (resolve = done)))
    render(<RiskControls policy={policy} onApply={onApply} />)

    fireEvent.click(killSwitch())
    fireEvent.click(screen.getByRole('button', { name: 'Confirm' }))
    expect(onApply).toHaveBeenCalledWith({ killSwitch: true })

    const applying = screen.getByRole('button', { name: 'Applying…' }) as HTMLButtonElement
    expect(applying.disabled).toBe(true)
    expect((screen.getByRole('button', { name: 'Cancel' }) as HTMLButtonElement).disabled).toBe(true)
    expect(killSwitch().disabled).toBe(true)
    expect(bypass().disabled).toBe(true)

    await act(async () => resolve(undefined))
    expect(screen.queryByText('Engage kill switch?')).toBeNull()
    expect(killSwitch().disabled).toBe(false)
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('releases an engaged kill switch', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<RiskControls policy={{ ...policy, killSwitch: true }} onApply={onApply} />)
    fireEvent.click(killSwitch())
    expect(screen.getByText('Release kill switch?')).toBeTruthy()
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(onApply).toHaveBeenCalledWith({ killSwitch: false })
  })

  it('enables the judge bypass and shows a refusal under that switch only', async () => {
    const onApply = vi.fn().mockResolvedValue('invalid_policy')
    const { container } = render(<RiskControls policy={policy} onApply={onApply} />)

    fireEvent.click(bypass())
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(onApply).toHaveBeenCalledWith({ allowTradingWithoutJev: true })

    const alert = screen.getByRole('alert')
    expect(alert.textContent).toBe('invalid_policy')
    const [killRow, bypassRow] = [...container.querySelectorAll('.rc-row')] as HTMLElement[]
    expect(within(bypassRow).getByRole('alert')).toBe(alert)
    expect(within(killRow).queryByRole('alert')).toBeNull()

    // Asking again clears the stale refusal.
    fireEvent.click(bypass())
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('disables an active judge bypass', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<RiskControls policy={{ ...policy, allowTradingWithoutJev: true }} onApply={onApply} />)
    fireEvent.click(bypass())
    expect(screen.getByText('Disable judge bypass?')).toBeTruthy()
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(onApply).toHaveBeenCalledWith({ allowTradingWithoutJev: false })
  })

  it('reports a handler that throws instead of resolving', async () => {
    const onApply = vi.fn().mockRejectedValueOnce(new Error('network down')).mockRejectedValueOnce('refused')
    render(<RiskControls policy={policy} onApply={onApply} />)

    fireEvent.click(killSwitch())
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(screen.getByRole('alert').textContent).toBe('network down')

    fireEvent.click(killSwitch())
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(screen.getByRole('alert').textContent).toBe('refused')
  })

  it('warns when the judge is not answering', () => {
    const { rerender } = render(<RiskControls policy={policy} jevHealthy={false} onApply={vi.fn()} />)
    expect(screen.getByText('Judge not answering — decisions paused.')).toBeTruthy()

    rerender(<RiskControls policy={{ ...policy, allowTradingWithoutJev: true }} jevHealthy={false} onApply={vi.fn()} />)
    expect(screen.getByText('Judge not answering — trading without it.')).toBeTruthy()

    rerender(<RiskControls policy={policy} jevHealthy onApply={vi.fn()} />)
    expect(screen.queryByText(/Judge not answering/)).toBeNull()

    rerender(<RiskControls policy={policy} onApply={vi.fn()} />)
    expect(screen.queryByText(/Judge not answering/)).toBeNull()
  })
})
