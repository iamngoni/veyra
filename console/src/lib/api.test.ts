/**
 * Tests for the typed control-surface client. The service contract is a set
 * of loopback routes behind the proxy, so these tests pin the exact paths,
 * query parameters, and failure behaviour the console relies on.
 */

import { afterEach, describe, expect, it, vi } from 'vitest'

import { api, streamAssistant, type AssistantEvent } from './api'

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
    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/commands?limit=25',
      '/api/account/balance-history?days=30',
      '/api/performance?days=30',
      '/api/audit?limit=200',
    ])
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
})
