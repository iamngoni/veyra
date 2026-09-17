/**
 * Tests for the typed control-surface client. The service contract is a set
 * of loopback routes behind the proxy, so these tests pin the exact paths,
 * query parameters, and failure behaviour the console relies on.
 */

import { afterEach, describe, expect, it, vi } from 'vitest'

import { api } from './api'

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
  it('builds the diagnostic route URLs', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.status()
    await api.account()
    await api.reconciliation()
    await api.metrics()
    await api.commands(10)
    await api.candles(24)

    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/status',
      '/api/account',
      '/api/reconciliation',
      '/api/metrics',
      '/api/commands?limit=10',
      '/api/market/candles?timeframe=H4&bars=24',
    ])
    for (const call of fetchMock.mock.calls) {
      expect(call[1]?.headers).toEqual({ accept: 'application/json' })
    }
  })

  it('defaults command and candle windows', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.commands()
    await api.candles()

    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/commands?limit=25',
      '/api/market/candles?timeframe=H4&bars=48',
    ])
  })

  it('builds cursor and wait parameters for the event feed', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse({}))
    vi.stubGlobal('fetch', fetchMock)

    await api.events(undefined)
    await api.events(7, 30_000)

    expect(fetchMock.mock.calls.map((call) => call[0])).toEqual([
      '/api/events',
      '/api/events?after=7&wait_ms=30000',
    ])
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
