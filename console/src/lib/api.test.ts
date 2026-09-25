/**
 * Tests for the typed control-surface client. The service contract is a set
 * of loopback routes behind the proxy, so these tests pin the exact paths,
 * query parameters, and failure behaviour the console relies on.
 */

import { afterEach, describe, expect, it, vi } from 'vitest'

import {
  api,
  changeModelCredential,
  NotificationError,
  streamAssistant,
  subscriptions,
  testNotification,
  updateNotifications,
  type AssistantEvent,
} from './api'

function jsonResponse(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
  } as unknown as Response
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('api', () => {
  it('accepts a long answer as bounded history on the next question', async () => {
    const longAnswer = 'Opening context. ' + 'profit against days '.repeat(100) + 'Current conclusion.'
    const fetchMock = vi.fn().mockResolvedValueOnce(
      new Response(`event: answer\ndata: ${JSON.stringify({ text: longAnswer })}\n\n`, { status: 200 }),
    ).mockResolvedValueOnce(
      new Response(`event: answer\ndata: ${JSON.stringify({ text: 'Answered the follow-up.' })}\n\n`, { status: 200 }),
    )
    vi.stubGlobal('fetch', fetchMock)
    const events: AssistantEvent[] = []
    const signal = new AbortController().signal

    await streamAssistant('How is performance?', [], (event) => events.push(event), signal)
    expect(events[0]).toEqual({ event: 'answer', text: longAnswer })
    await streamAssistant('How does that compare with last week?', [
      { role: 'user', content: 'How is performance?' },
      { role: 'assistant', content: longAnswer },
    ], (event) => events.push(event), signal)

    const request = JSON.parse(fetchMock.mock.calls[1][1].body as string) as {
      history: Array<{ role: string; content: string }>
    }
    expect(Array.from(request.history[1].content).length).toBe(1_000)
    expect(request.history[1].content).toMatch(/^Opening context/)
    expect(request.history[1].content).toMatch(/Current conclusion\.$/)
    expect(events.at(-1)).toEqual({ event: 'answer', text: 'Answered the follow-up.' })
  })

  it('builds the diagnostic route URLs', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.status()
    await api.account()
    await api.reconciliation()
    await api.metrics()
    await api.commands(10)
    await api.candles(24)
    await api.balanceHistory(7)
    await api.performance(7)
    await api.sessions()
    await api.audit(50)
    await api.trades(7)
    await api.notifications()

    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/status',
      '/api/account',
      '/api/reconciliation',
      '/api/metrics',
      '/api/commands?limit=10',
      '/api/market/candles?timeframe=H4&bars=24',
      '/api/account/balance-history?days=7',
      '/api/performance?days=7',
      '/api/market/sessions',
      '/api/audit?limit=50',
      '/api/trades?days=7&page=1&pageSize=20',
      '/api/notifications',
    ])
    for (const call of fetchMock.mock.calls) {
      expect(call[1]?.headers).toEqual({ accept: 'application/json' })
    }
  })

  it('keeps the status detail when an error body is not JSON', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        new Response('gateway said no', { status: 502, headers: { 'content-type': 'text/plain' } }),
      ),
    )
    await expect(api.updatePolicy({ killSwitch: true })).rejects.toThrow('/risk/policy → 502')
  })

  it('defaults the audit, performance, and command windows', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.commands()
    await api.balanceHistory()
    await api.performance()
    await api.audit()
    await api.trades()
    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/commands?limit=25',
      '/api/account/balance-history?days=30',
      '/api/performance?days=30',
      '/api/audit?limit=200',
      '/api/trades?days=30&page=1&pageSize=20',
    ])
  })

  it('resolves a trades page as served', async () => {
    const page = {
      days: 30,
      truncated: false,
      total: 1,
      page: 1,
      pageSize: 20,
      pageCount: 1,
      brokerOffsetSecs: 7200,
      summary: { count: 1, wins: 1, losses: 0, breakeven: 0, net: 1.36 },
      trades: [
        {
          ticket: 10655087,
          symbol: 'GBPUSD',
          side: 'short',
          lots: 0.01,
          openedAtMs: 1_790_271_000_000,
          closedAtMs: 1_790_341_680_000,
          openPrice: 1.32123,
          closePrice: 1.3265,
          stopLoss: 1.3265,
          takeProfit: 1.3162,
          net: -5.31,
          profit: -5.31,
          swap: 0,
          commission: 0,
          rMultiple: -1,
          closeReason: 'stop_loss',
          closeDetail: null,
          entryRationale: 'GBPUSD has the strongest aligned bearish evidence…',
        },
      ],
    }
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(page))
    vi.stubGlobal('fetch', fetchMock)
    await expect(api.trades(30)).resolves.toEqual(page)
    expect(fetchMock.mock.calls[0][0]).toBe('/api/trades?days=30&page=1&pageSize=20')
    expect(fetchMock.mock.calls[0][1]).toEqual({ signal: undefined, headers: { accept: 'application/json' } })
  })

  it('defaults command and candle windows', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.commands()
    await api.candles()
    await api.candles(120, 'D1', 'SP500m')

    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/commands?limit=25',
      '/api/market/candles?timeframe=H4&bars=48',
      '/api/market/candles?timeframe=D1&bars=120&symbol=SP500m',
    ])
  })

  it('builds cursor and wait parameters for the event feed', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.events(undefined)
    await api.events(7, 30_000)

    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/events?limit=200',
      '/api/events?after=7&wait_ms=30000&limit=200',
    ])
  })

  it('builds the log tail with level and cursor', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.logs(undefined, 'info')
    await api.logs(7, 'error', 50)

    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/logs?limit=300&level=info',
      '/api/logs?limit=50&level=error&after=7',
    ])
  })

  it('posts policy patches as JSON to the control surface', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({ killSwitch: true }))
    vi.stubGlobal('fetch', fetchMock)

    await api.updatePolicy({ killSwitch: true, maxOpenOrders: 3 })

    expect(fetchMock.mock.calls[0][0]).toBe('/api/risk/policy')
    expect(fetchMock.mock.calls[0][1]).toMatchObject({
      method: 'POST',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body: JSON.stringify({ killSwitch: true, maxOpenOrders: 3 }),
    })
  })

  it('surfaces the field and reason of a rejected patch', async () => {
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValue(
          jsonResponse({ error: 'invalid_policy', field: 'maxOpenOrders', reason: 'too large' }, 400),
        ),
    )
    await expect(api.updatePolicy({ maxOpenOrders: 1001 })).rejects.toThrow(
      'maxOpenOrders: too large',
    )
  })

  it('surfaces every rejected field when a patch has multiple validation errors', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        jsonResponse({ rejected: [{ field: 'maxOpenOrders', reason: 'too large' }, { field: 'sessionUtc', reason: 'invalid window' }] }, 400),
      ),
    )
    await expect(api.updateConfig({ maxOpenOrders: 1001, sessionUtc: 'bad' })).rejects.toThrow(
      'maxOpenOrders: too large; sessionUtc: invalid window',
    )
  })

  it('returns parsed JSON', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({ status: 'ok' })))
    await expect(api.status()).resolves.toEqual({ status: 'ok' })
  })

  it('throws a route-scoped error on a non-ok response', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({ error: 'audit_unavailable' }, 503)))
    await expect(api.status()).rejects.toThrow('/status → 503')
  })

  it('reports a bare reason when a rejected patch names no field', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({ reason: 'settings are locked during a restart' }, 409)))
    await expect(api.updateConfig({ VEYRA_TRADING_ENABLED: 'true' })).rejects.toThrow(
      'settings are locked during a restart',
    )
  })

  it('clears every benched model in one call', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({ cleared: 2, model_cooldowns: [] }))
    vi.stubGlobal('fetch', fetchMock)
    await expect(api.clearCooldowns()).resolves.toEqual({ cleared: 2, model_cooldowns: [] })
    expect(fetchMock.mock.calls[0][0]).toBe('/api/model/cooldowns/clear')
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ method: 'POST', body: '{}' })
  })

  describe('streamAssistant failure modes', () => {
    const signal = new AbortController().signal

    it('reads the reason from a non-ok response before opening the stream', async () => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({ reason: 'assistant disabled' }, 503)))
      await expect(streamAssistant('q', [], () => {}, signal)).rejects.toThrow('assistant disabled')
    })

    it('falls back to a status-only message when the error body is not JSON', async () => {
      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValue(new Response('gateway down', { status: 502, headers: { 'content-type': 'text/plain' } })),
      )
      await expect(streamAssistant('q', [], () => {}, signal)).rejects.toThrow('Assistant unavailable (502)')
    })

    it('refuses a response with no body to stream', async () => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, body: null } as unknown as Response))
      await expect(streamAssistant('q', [], () => {}, signal)).rejects.toThrow('The assistant stream did not open.')
    })

    it('rejects a frame whose data is not parseable JSON', async () => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(
        new Response('event: answer\ndata: {not json\n\n', { status: 200 }),
      ))
      await expect(streamAssistant('q', [], () => {}, signal)).rejects.toThrow('The assistant sent an unreadable update.')
    })

    it('rejects a stream that ends mid-frame', async () => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(
        new Response('event: answer\ndata: {"text":"cut off"', { status: 200 }),
      ))
      await expect(streamAssistant('q', [], () => {}, signal)).rejects.toThrow(
        'The assistant stream ended partway through an update.',
      )
    })
  })

  describe('changeModelCredential', () => {
    it('saves a key with POST and confirms an active model', async () => {
      const fetchMock = vi.fn().mockResolvedValue(
        jsonResponse({ saved: true, active: true, secret: { set: true, source: 'console', hint: 'ab12' } }),
      )
      vi.stubGlobal('fetch', fetchMock)
      await expect(changeModelCredential('operator-token', 'sk-live')).resolves.toMatchObject({ active: true })
      expect(fetchMock.mock.calls[0][0]).toBe('/api/model/credential')
      expect(fetchMock.mock.calls[0][1]).toMatchObject({
        method: 'POST',
        headers: { accept: 'application/json', 'content-type': 'application/json', 'x-veyra-admin-token': 'operator-token' },
        body: JSON.stringify({ key: 'sk-live' }),
      })
    })

    it('removes a key with DELETE and no body', async () => {
      const fetchMock = vi.fn().mockResolvedValue(
        jsonResponse({ saved: true, active: false, secret: { set: false, source: null, hint: null } }),
      )
      vi.stubGlobal('fetch', fetchMock)
      await changeModelCredential('operator-token')
      expect(fetchMock.mock.calls[0][1]).toMatchObject({ method: 'DELETE' })
      expect(fetchMock.mock.calls[0][1].body).toBeUndefined()
    })

    it('surfaces the reason, then the error code, then the bare status for a rejected update', async () => {
      const fetchMock = vi.fn()
        .mockResolvedValueOnce(jsonResponse({ reason: 'token rejected' }, 401))
        .mockResolvedValueOnce(jsonResponse({ error: 'rate_limited' }, 429))
        .mockResolvedValueOnce(jsonResponse({}, 500))
      vi.stubGlobal('fetch', fetchMock)
      await expect(changeModelCredential('bad-token', 'k')).rejects.toThrow('token rejected')
      await expect(changeModelCredential('bad-token', 'k')).rejects.toThrow('rate limited')
      await expect(changeModelCredential('bad-token', 'k')).rejects.toThrow('Credential update failed (500)')
    })

    it('refuses an ok response the service never actually confirmed', async () => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({ saved: false })))
      await expect(changeModelCredential('operator-token', 'k')).rejects.toThrow(
        'The service did not confirm the credential update.',
      )
    })
  })

  describe('subscriptions', () => {
    it('reads subscription status', async () => {
      const fetchMock = vi.fn().mockResolvedValue(jsonResponse({ subscriptions: { codex: { connected: true }, claude_code: { connected: false } } }))
      vi.stubGlobal('fetch', fetchMock)
      await expect(subscriptions.status()).resolves.toMatchObject({ subscriptions: { codex: { connected: true } } })
      expect(fetchMock.mock.calls[0][0]).toBe('/api/model/subscriptions')
    })

    it('starts, completes, and removes a connection with the admin token header', async () => {
      const fetchMock = vi.fn()
        .mockResolvedValueOnce(jsonResponse({ provider: 'codex', authorize_url: 'https://example.test/authorize', state: 's' }))
        .mockResolvedValueOnce(jsonResponse({ connected: true }))
        .mockResolvedValueOnce(jsonResponse({ deleted: true }))
      vi.stubGlobal('fetch', fetchMock)

      await subscriptions.start('codex', 'operator-token')
      expect(fetchMock.mock.calls[0]).toMatchObject([
        '/api/model/subscriptions/start',
        { method: 'POST', headers: { 'x-veyra-admin-token': 'operator-token' }, body: JSON.stringify({ provider: 'codex' }) },
      ])

      await subscriptions.complete('codex', 'callback-value', 'operator-token')
      expect(fetchMock.mock.calls[1]).toMatchObject([
        '/api/model/subscriptions/complete',
        { method: 'POST', body: JSON.stringify({ provider: 'codex', callback_value: 'callback-value' }) },
      ])

      await subscriptions.remove('codex', 'operator-token')
      expect(fetchMock.mock.calls[2]).toMatchObject(['/api/model/subscriptions/codex', { method: 'DELETE' }])
      expect(fetchMock.mock.calls[2][1].body).toBeUndefined()
    })

    it('surfaces the reason, then the error code, then a bare status for a failed mutation', async () => {
      const fetchMock = vi.fn()
        .mockResolvedValueOnce(jsonResponse({ reason: 'invalid callback' }, 400))
        .mockResolvedValueOnce(jsonResponse({ error: 'not_found' }, 404))
        .mockResolvedValueOnce(jsonResponse(null, 500))
      vi.stubGlobal('fetch', fetchMock)
      await expect(subscriptions.complete('codex', 'bad', 't')).rejects.toThrow('invalid callback')
      await expect(subscriptions.remove('codex', 't')).rejects.toThrow('not found')
      await expect(subscriptions.remove('codex', 't')).rejects.toThrow('Connection failed (500)')
    })

    it('refuses an ok response with no confirming body', async () => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, status: 200, json: async () => null } as unknown as Response))
      await expect(subscriptions.remove('codex', 't')).rejects.toThrow('The service did not confirm the connection change.')
    })
  })
  describe('notifications', () => {
    it('saves a patch with PUT and the operator token', async () => {
      const settings = { available: true }
      const fetchMock = vi.fn().mockResolvedValue(jsonResponse(settings))
      vi.stubGlobal('fetch', fetchMock)
      const patch = { events: { trade_opened: false }, providers: { telegram: { secrets: { botToken: null } } } }
      await expect(updateNotifications('operator-token', patch)).resolves.toEqual(settings)
      expect(fetchMock.mock.calls[0]).toEqual([
        '/api/notifications',
        {
          method: 'PUT',
          headers: { accept: 'application/json', 'content-type': 'application/json', 'x-veyra-admin-token': 'operator-token' },
          body: JSON.stringify(patch),
        },
      ])
    })

    it('sends a test with POST naming the provider', async () => {
      const fetchMock = vi.fn().mockResolvedValue(jsonResponse({ ok: true }))
      vi.stubGlobal('fetch', fetchMock)
      await expect(testNotification('operator-token', 'telegram')).resolves.toEqual({ ok: true })
      expect(fetchMock.mock.calls[0]).toMatchObject([
        '/api/notifications/test',
        { method: 'POST', headers: { 'x-veyra-admin-token': 'operator-token' }, body: JSON.stringify({ provider: 'telegram' }) },
      ])
    })

    it('keeps every rejected field, then falls back to the reason, the error code and the status', async () => {
      const rejected = [
        { field: 'providers.telegram.chatId', reason: 'required' },
        { field: 'providers.webhook.url', reason: 'must_be_https' },
      ]
      const fetchMock = vi.fn()
        .mockResolvedValueOnce(jsonResponse({ error: 'invalid_notifications', rejected }, 400))
        .mockResolvedValueOnce(jsonResponse({ error: 'notification_failed', reason: 'HTTP 401: Unauthorized' }, 502))
        .mockResolvedValueOnce(jsonResponse({ error: 'invalid_operator_token' }, 401))
        .mockResolvedValueOnce({ ok: false, status: 500, json: async () => { throw new SyntaxError('html') } } as unknown as Response)
      vi.stubGlobal('fetch', fetchMock)

      const first = await updateNotifications('t', {}).catch((error: unknown) => error)
      expect(first).toBeInstanceOf(NotificationError)
      expect((first as NotificationError).rejected).toEqual(rejected)
      expect((first as NotificationError).message).toBe('providers.telegram.chatId: required; providers.webhook.url: must be https')
      await expect(testNotification('t', 'telegram')).rejects.toThrow('HTTP 401: Unauthorized')
      const third = await updateNotifications('t', {}).catch((error: unknown) => error)
      expect((third as NotificationError).message).toBe('invalid operator token')
      expect((third as NotificationError).rejected).toEqual([])
      await expect(updateNotifications('t', {})).rejects.toThrow('Request failed (500)')
    })

    it('refuses an ok response with no body', async () => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, status: 200, json: async () => null } as unknown as Response))
      await expect(updateNotifications('t', {})).rejects.toThrow('The service did not confirm the change.')
    })
  })
})
