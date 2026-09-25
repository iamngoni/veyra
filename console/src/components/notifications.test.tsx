/**
 * Render tests for the notification settings: events and channels as the
 * service reports them, the patches each control sends with the operator
 * token, write-only secrets, test messages, refusals and the local-time
 * daily summary.
 */

import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/api')>()
  return { ...actual, updateNotifications: vi.fn(), testNotification: vi.fn() }
})

import { NotificationError, testNotification, updateNotifications, type NotificationSettings } from '../lib/api'
import { localTimeOfUtcHour, NotificationsPanel, summaryHourOptions } from './notifications'

const update = vi.mocked(updateNotifications)
const sendTest = vi.mocked(testNotification)

const off = { set: false, hint: null }

function fixture(): NotificationSettings {
  return {
    available: true,
    summaryHourUtc: 18,
    events: {
      breaker_tripped: true,
      trading_halted: true,
      broker_link: true,
      reconciliation_drift: true,
      order_failed: true,
      model_trouble: true,
      service_down: true,
      trade_opened: true,
      trade_closed: true,
      daily_summary: true,
    },
    providers: {
      email: {
        enabled: false,
        fields: { host: 'smtp.example.com', port: '587', security: 'starttls', from: 'veyra@example.com', to: 'a@example.com' },
        secrets: { password: off },
      },
      telegram: { enabled: true, fields: { chatId: '-100123' }, secrets: { botToken: { set: true, hint: 'wxYZ' } } },
      discord: { enabled: false, fields: {}, secrets: { webhookUrl: off } },
      slack: { enabled: false, fields: {}, secrets: { webhookUrl: off } },
      ntfy: { enabled: false, fields: { server: 'https://ntfy.sh' }, secrets: { topic: { set: true, hint: 'abcd' }, accessToken: { set: true, hint: 'tokn' } } },
      pushover: { enabled: false, fields: {}, secrets: { appToken: off, userKey: off } },
      webhook: { enabled: false, fields: {}, secrets: { url: off, bearerToken: off } },
    },
    status: { pending: 0, dropped: 0, delivered: 11, failed: 1 },
    recent: [
      { atMs: Date.now(), provider: 'telegram', event: 'trade_closed', title: 'EURUSD closed +12.40', ok: true, attempts: 1, detail: null },
      { atMs: Date.UTC(2026, 0, 2, 9, 5), provider: 'discord', event: 'order_failed', title: 'Order failed', ok: false, attempts: 3, detail: 'HTTP 404: Unknown Webhook' },
      { atMs: Date.UTC(2026, 0, 2, 9, 4), provider: 'mystery', event: 'order_failed', title: 'Lost', ok: false, attempts: 1, detail: 'timeout' },
      { atMs: Date.UTC(2026, 0, 2, 9, 3), provider: 'slack', event: 'order_failed', title: 'Quiet', ok: false, attempts: 1, detail: null },
    ],
  }
}

const TZ = process.env.TZ

beforeEach(() => {
  update.mockReset()
  sendTest.mockReset()
})
afterEach(() => {
  cleanup()
  process.env.TZ = TZ
})

const token = (value = 'op-token') =>
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value } })
const status = (container: HTMLElement) => within(container).getAllByRole('status')
const help = (label: string) => {
  const mark = screen.getByRole('button', { name: `About ${label}` })
  return document.getElementById(mark.getAttribute('aria-describedby')!)?.textContent
}
const channel = (name: string) => screen.getByRole('switch', { name })
const openEditor = (label: string) => fireEvent.click(screen.getByRole('button', { name: label }))

describe('NotificationsPanel', () => {
  it('shows a placeholder, then the poll error when there is nothing to show', () => {
    const { rerender } = render(<NotificationsPanel />)
    expect(document.querySelector('.tab-skeleton-row')).toBeTruthy()
    rerender(<NotificationsPanel error="/notifications → 404" />)
    expect(screen.getByRole('alert').textContent).toBe('Notifications unavailable: /notifications → 404')
  })

  it('lists events, channels, counters and recent deliveries', () => {
    render(<NotificationsPanel settings={fixture()} />)
    for (const label of ['Breaker tripped', 'Trading halted', 'Service down', 'Trade opened', 'Daily summary']) {
      expect(screen.getByRole('switch', { name: label }).getAttribute('aria-checked')).toBe('true')
    }
    expect(help('Breaker tripped')).toBe('A daily-loss or peak-drawdown breaker started blocking new entries.')
    expect(help('Daily summary')).toMatch(/Your local time; 18:00 UTC\.$/)

    const rows = [...document.querySelectorAll('.notify-channel-row')].map((row) => row.textContent)
    expect(rows).toEqual(['EmailOffEdit', 'TelegramOnEdit', 'DiscordNot set upSet up', 'SlackNot set upSet up', 'ntfyOffEdit', 'PushoverNot set upSet up', 'WebhookNot set upSet up'])
    expect(channel('Telegram').getAttribute('aria-checked')).toBe('true')
    // A channel missing required values cannot be switched on.
    expect((channel('Discord') as HTMLButtonElement).disabled).toBe(true)
    expect((channel('Email') as HTMLButtonElement).disabled).toBe(false)
    expect((channel('ntfy') as HTMLButtonElement).disabled).toBe(false)

    expect(screen.getByText(/11 sent · 1 failed · 0 dropped/)).toBeTruthy()
    const deliveries = [...document.querySelectorAll('.notify-delivery')]
    expect(deliveries).toHaveLength(4)
    expect(deliveries[0].textContent).toMatch(/^\d\d:\d\dTelegramEURUSD closed \+12\.40Sent$/)
    expect(deliveries[1].querySelector('.tab-time')?.textContent).toMatch(/Jan/)
    expect(help('Discord failure')).toBe('HTTP 404: Unknown Webhook (3 attempts)')
    expect(help('mystery failure')).toBe('timeout (1 attempt)')
    expect(deliveries[3].querySelector('.hint')).toBeNull()
  })

  it('says when nothing has been sent and caps the list at ten', () => {
    const empty = { ...fixture(), recent: [] }
    const { rerender } = render(<NotificationsPanel settings={empty} />)
    expect(screen.getByText('Nothing sent yet.')).toBeTruthy()
    const many = { ...fixture(), recent: Array.from({ length: 14 }, () => fixture().recent[0]) }
    rerender(<NotificationsPanel settings={many} />)
    expect(document.querySelectorAll('.notify-delivery')).toHaveLength(10)
  })

  it('asks for the operator token before changing anything', () => {
    const { container } = render(<NotificationsPanel settings={fixture()} />)
    fireEvent.click(screen.getByRole('switch', { name: 'Trade opened' }))
    expect(update).not.toHaveBeenCalled()
    expect(status(container).map((line) => line.textContent)).toContain('Enter the operator token first.')
    expect(document.activeElement).toBe(screen.getByLabelText('Operator token'))
    expect((screen.getByLabelText('Operator token') as HTMLInputElement).type).toBe('password')
  })

  it('saves an event switch at once and shows the response', async () => {
    const onRefresh = vi.fn()
    const saved = fixture()
    saved.events.trade_opened = false
    update.mockResolvedValue(saved)
    render(<NotificationsPanel settings={fixture()} onRefresh={onRefresh} />)
    token()
    await act(async () => fireEvent.click(screen.getByRole('switch', { name: 'Trade opened' })))
    expect(update).toHaveBeenCalledWith('op-token', { events: { trade_opened: false } })
    expect(screen.getByRole('switch', { name: 'Trade opened' }).getAttribute('aria-checked')).toBe('false')
    expect(screen.getByText('Trade opened off.')).toBeTruthy()
    expect(onRefresh).toHaveBeenCalledTimes(1)
  })

  it('turns an event back on', async () => {
    const settings = fixture()
    settings.events.order_failed = false
    settings.providers.telegram.secrets.botToken = { set: true, hint: null }
    update.mockResolvedValue(fixture())
    render(<NotificationsPanel settings={settings} />)
    openEditor('Edit Telegram')
    expect(screen.getByText('Saved ····')).toBeTruthy()
    token()
    await act(async () => fireEvent.click(screen.getByRole('switch', { name: 'Order failed' })))
    expect(update).toHaveBeenCalledWith('op-token', { events: { order_failed: true } })
    expect(screen.getByText('Order failed on.')).toBeTruthy()
  })

  it('reports a refused switch without changing it', async () => {
    update.mockRejectedValueOnce(new Error('invalid operator token')).mockRejectedValueOnce('nope')
    render(<NotificationsPanel settings={fixture()} />)
    token()
    await act(async () => fireEvent.click(channel('Telegram')))
    expect(update).toHaveBeenCalledWith('op-token', { providers: { telegram: { enabled: false } } })
    expect(screen.getByText('Not saved. invalid operator token')).toBeTruthy()
    expect(channel('Telegram').getAttribute('aria-checked')).toBe('true')
    await act(async () => fireEvent.click(channel('ntfy')))
    expect(update).toHaveBeenLastCalledWith('op-token', { providers: { ntfy: { enabled: true } } })
    expect(screen.getByText('Not saved. The service refused the change.')).toBeTruthy()
  })

  it('turns a channel on and reports it', async () => {
    const saved = fixture()
    saved.providers.ntfy.enabled = true
    update.mockResolvedValue(saved)
    render(<NotificationsPanel settings={fixture()} />)
    token()
    await act(async () => fireEvent.click(channel('ntfy')))
    expect(screen.getByText('ntfy on.')).toBeTruthy()
    expect(channel('ntfy').getAttribute('aria-checked')).toBe('true')
  })

  it('picks the daily summary in local time and sends it in UTC', async () => {
    process.env.TZ = 'Africa/Harare'
    update.mockResolvedValue({ ...fixture(), summaryHourUtc: 5 })
    render(<NotificationsPanel settings={fixture()} />)
    const select = screen.getByLabelText('Daily summary time') as HTMLSelectElement
    expect(select.selectedOptions[0].textContent).toBe('20:00')
    expect(select.options[0].textContent).toBe('00:00')
    expect(select.options[0].value).toBe('22')
    token()
    await act(async () => fireEvent.change(select, { target: { value: '5' } }))
    expect(update).toHaveBeenCalledWith('op-token', { summaryHourUtc: 5 })
    expect(screen.getByText('Daily summary at 07:00.')).toBeTruthy()
  })

  it('converts UTC hours to local clock times, half-hour zones included', () => {
    const day = new Date(Date.UTC(2026, 6, 1, 12))
    process.env.TZ = 'Africa/Harare'
    expect(localTimeOfUtcHour(18, day)).toBe('20:00')
    process.env.TZ = 'Asia/Kolkata'
    expect(localTimeOfUtcHour(18, day)).toBe('23:30')
    const options = summaryHourOptions(day)
    expect(options).toHaveLength(24)
    expect(options[0]).toEqual({ utc: 19, label: '00:30' })
    process.env.TZ = 'America/New_York'
    // Daylight saving follows the date asked about.
    expect(localTimeOfUtcHour(18, day)).toBe('14:00')
    expect(localTimeOfUtcHour(18, new Date(Date.UTC(2026, 0, 15, 12)))).toBe('13:00')
    expect(localTimeOfUtcHour(0)).toMatch(/^\d\d:\d\d$/)
    expect(summaryHourOptions()).toHaveLength(24)
  })

  it('opens one channel at a time with its setup guide', () => {
    render(<NotificationsPanel settings={fixture()} />)
    openEditor('Edit Telegram')
    expect(screen.getByRole('button', { name: 'Close Telegram' }).getAttribute('aria-expanded')).toBe('true')
    expect(screen.getByText('How to set up')).toBeTruthy()
    expect(screen.getByText(/open @BotFather/)).toBeTruthy()
    openEditor('Set up Discord')
    expect(screen.queryByRole('button', { name: 'Close Telegram' })).toBeNull()
    expect(screen.getByText(/Server Settings → Integrations → Webhooks/)).toBeTruthy()
    openEditor('Close Discord')
    expect(document.querySelector('.notify-editor')).toBeNull()
  })

  it('shows a saved secret by its last characters and saves fields, secrets and the token', async () => {
    const onRefresh = vi.fn()
    update.mockResolvedValue(fixture())
    const { container } = render(<NotificationsPanel settings={fixture()} onRefresh={onRefresh} />)
    openEditor('Edit Telegram')
    expect(screen.getByText('Saved ····wxYZ')).toBeTruthy()
    const save = screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement
    const test = screen.getByRole('button', { name: 'Send test' }) as HTMLButtonElement
    expect(save.disabled).toBe(true)
    expect(test.disabled).toBe(false)

    fireEvent.change(screen.getByLabelText('Chat ID'), { target: { value: ' 42 ' } })
    fireEvent.click(screen.getByRole('button', { name: 'Replace Bot token' }))
    const secret = screen.getByLabelText('Bot token') as HTMLInputElement
    expect(secret.type).toBe('password')
    fireEvent.change(secret, { target: { value: '123:abc' } })
    expect(test.disabled).toBe(true)
    expect(test.parentElement?.getAttribute('title')).toBe('Save first; a test uses the saved settings.')

    // No token: nothing is sent.
    fireEvent.click(save)
    expect(update).not.toHaveBeenCalled()
    expect(status(container).map((line) => line.textContent)).toContain('Enter the operator token first.')

    token()
    await act(async () => fireEvent.click(save))
    expect(update).toHaveBeenCalledWith('op-token', {
      providers: { telegram: { fields: { chatId: '42' }, secrets: { botToken: '123:abc' } } },
    })
    expect(screen.getByText('Saved.')).toBeTruthy()
    expect(onRefresh).toHaveBeenCalled()
    expect(screen.getByText('Saved ····wxYZ')).toBeTruthy()
  })

  it('clears a secret or a field with null, and can undo before saving', async () => {
    update.mockResolvedValue(fixture())
    render(<NotificationsPanel settings={fixture()} />)
    token()
    openEditor('Edit ntfy')
    fireEvent.click(screen.getByRole('button', { name: 'Clear Topic' }))
    expect(screen.getByText('Cleared on save')).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Keep Topic' }))
    expect(screen.getByText('Saved ····abcd')).toBeTruthy()

    // Replacing, then keeping the saved value, sends nothing for it.
    fireEvent.click(screen.getByRole('button', { name: 'Replace Topic' }))
    fireEvent.click(screen.getByRole('button', { name: 'Keep saved' }))
    expect(screen.getByText('Saved ····abcd')).toBeTruthy()
    // An empty replacement is no change either.
    fireEvent.click(screen.getByRole('button', { name: 'Replace Topic' }))
    expect((screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(true)
    fireEvent.click(screen.getByRole('button', { name: 'Keep saved' }))

    fireEvent.click(screen.getByRole('button', { name: 'Clear Access token' }))
    fireEvent.change(screen.getByLabelText('Server'), { target: { value: '' } })
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Save' })))
    expect(update).toHaveBeenCalledWith('op-token', {
      providers: { ntfy: { fields: { server: null }, secrets: { accessToken: null } } },
    })
  })

  it('edits email security from a menu and discards a draft', () => {
    render(<NotificationsPanel settings={fixture()} />)
    openEditor('Edit Email')
    const security = screen.getByLabelText('Security') as HTMLSelectElement
    expect(security.value).toBe('starttls')
    fireEvent.change(security, { target: { value: 'tls' } })
    expect(security.className).toContain('is-dirty')
    expect(screen.getByLabelText('Password').getAttribute('placeholder')).toBe('Not set')
    fireEvent.change(screen.getByLabelText('Password'), { target: { value: 'app-password' } })
    fireEvent.click(screen.getByRole('button', { name: 'Discard' }))
    expect(security.value).toBe('starttls')
    expect((screen.getByLabelText('Password') as HTMLInputElement).value).toBe('')
    expect(screen.queryByRole('button', { name: 'Discard' })).toBeNull()
  })

  it('falls back to the default security and keeps an unknown value selectable', () => {
    const settings = fixture()
    delete settings.providers.email.fields.security
    const { rerender } = render(<NotificationsPanel settings={settings} />)
    openEditor('Edit Email')
    expect((screen.getByLabelText('Security') as HTMLSelectElement).value).toBe('starttls')
    const odd = fixture()
    odd.providers.email.fields.security = 'ssl'
    rerender(<NotificationsPanel settings={odd} />)
    expect((screen.getByLabelText('Security') as HTMLSelectElement).value).toBe('ssl')
  })

  it('marks each refused field and names it in the status', async () => {
    update
      .mockRejectedValueOnce(
        new NotificationError('refused', [
          { field: 'providers.discord.webhookUrl', reason: 'must_be_https' },
          { field: 'providers.discord.extra', reason: 'unknown' },
          { field: 'events.trade_opened', reason: 'ignored' },
        ]),
      )
      .mockRejectedValueOnce(new NotificationError('credential store unavailable'))
      .mockRejectedValueOnce('bare')
    render(<NotificationsPanel settings={fixture()} />)
    token()
    openEditor('Set up Discord')
    expect(screen.getByLabelText('Webhook URL').getAttribute('placeholder')).toBe('Required')
    fireEvent.change(screen.getByLabelText('Webhook URL'), { target: { value: 'http://example.com' } })
    const save = screen.getByRole('button', { name: 'Save' })
    await act(async () => fireEvent.click(save))
    expect(screen.getByText('Not saved. Webhook URL: must be https; extra: unknown.')).toBeTruthy()
    expect(screen.getByText('must be https')).toBeTruthy()
    expect(screen.getByLabelText('Webhook URL').getAttribute('aria-invalid')).toBe('true')
    // The draft stays for correction.
    expect((screen.getByLabelText('Webhook URL') as HTMLInputElement).value).toBe('http://example.com')

    await act(async () => fireEvent.click(save))
    expect(screen.getByText('Not saved. credential store unavailable')).toBeTruthy()
    await act(async () => fireEvent.click(save))
    expect(screen.getByText('Not saved. The service refused the change.')).toBeTruthy()
  })

  it('shows a refused text field and a refused saved secret beside their labels', async () => {
    update.mockRejectedValue(
      new NotificationError('refused', [
        { field: 'providers.telegram.chatId', reason: 'required' },
        { field: 'providers.telegram.botToken', reason: 'invalid' },
      ]),
    )
    render(<NotificationsPanel settings={fixture()} />)
    token()
    openEditor('Edit Telegram')
    fireEvent.change(screen.getByLabelText('Chat ID'), { target: { value: '' } })
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Save' })))
    expect(screen.getByText('Not saved. Chat ID: required; Bot token: invalid.')).toBeTruthy()
    expect(screen.getByText('required').closest('.tab-control-aside')).toBeTruthy()
    expect(screen.getByText('invalid').closest('.tab-control-aside')).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Clear Bot token' }))
    expect(screen.getByText('invalid')).toBeTruthy()
  })

  it('sends a test through the saved settings and reports the outcome', async () => {
    const onRefresh = vi.fn()
    sendTest.mockResolvedValueOnce({ ok: true }).mockRejectedValueOnce(new Error('HTTP 401: Unauthorized')).mockRejectedValueOnce('x')
    render(<NotificationsPanel settings={fixture()} onRefresh={onRefresh} />)
    openEditor('Edit Telegram')
    const test = screen.getByRole('button', { name: 'Send test' })
    fireEvent.click(test)
    expect(sendTest).not.toHaveBeenCalled()
    token()
    await act(async () => fireEvent.click(test))
    expect(sendTest).toHaveBeenCalledWith('op-token', 'telegram')
    expect(screen.getByText('Test sent. Check Telegram.')).toBeTruthy()
    expect(onRefresh).toHaveBeenCalledTimes(1)
    await act(async () => fireEvent.click(test))
    expect(screen.getByText('Test failed. HTTP 401: Unauthorized')).toBeTruthy()
    await act(async () => fireEvent.click(test))
    expect(screen.getByText('Test failed. No reason given.')).toBeTruthy()
  })

  it('holds the test until a channel has its required values', () => {
    render(<NotificationsPanel settings={fixture()} />)
    openEditor('Set up Pushover')
    const test = screen.getByRole('button', { name: 'Send test' }) as HTMLButtonElement
    expect(test.disabled).toBe(true)
    expect(test.parentElement?.getAttribute('title')).toBe('Save the required values first.')
    expect(screen.getByText(/pushover\.net/, { selector: 'a' })).toBeTruthy()
  })

  it('shows every control disabled with one note when the service cannot store settings', () => {
    render(<NotificationsPanel settings={{ ...fixture(), available: false }} />)
    expect(screen.getByText('Notifications need encrypted credential storage and a database on the service.')).toBeTruthy()
    expect(screen.queryByLabelText('Operator token')).toBeNull()
    for (const toggle of screen.getAllByRole('switch')) expect((toggle as HTMLButtonElement).disabled).toBe(true)
    expect((screen.getByLabelText('Daily summary time') as HTMLSelectElement).disabled).toBe(true)
    openEditor('Edit Telegram')
    expect((screen.getByLabelText('Chat ID') as HTMLInputElement).disabled).toBe(true)
    expect((screen.getByRole('button', { name: 'Replace Bot token' }) as HTMLButtonElement).disabled).toBe(true)
  })

  it('has a guide for every channel', () => {
    render(<NotificationsPanel settings={fixture()} />)
    for (const label of ['Edit Email', 'Set up Slack', 'Edit ntfy', 'Set up Webhook']) {
      openEditor(label)
      expect(document.querySelectorAll('.notify-guide li').length).toBeGreaterThanOrEqual(3)
    }
  })

  it('drops the saved copy when a newer poll arrives', async () => {
    const saved = fixture()
    saved.events.trade_closed = false
    update.mockResolvedValue(saved)
    const { rerender } = render(<NotificationsPanel settings={fixture()} />)
    token()
    await act(async () => fireEvent.click(screen.getByRole('switch', { name: 'Trade closed' })))
    expect(screen.getByRole('switch', { name: 'Trade closed' }).getAttribute('aria-checked')).toBe('false')
    rerender(<NotificationsPanel settings={fixture()} />)
    expect(screen.getByRole('switch', { name: 'Trade closed' }).getAttribute('aria-checked')).toBe('true')
    expect(screen.getByText('Trade closed off.')).toBeTruthy()
  })
})
