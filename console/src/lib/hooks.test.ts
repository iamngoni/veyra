/**
 * Tests for the console's polling helpers and pure time/money formatters.
 *
 * The hooks own the console's only stateful loops: a poll that must keep the
 * last good value and an event cursor that must reconnect without duplicating
 * events. Both run on fake timers here.
 */

import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { LogLevel } from './api'
import {
  clockTime,
  forgetSymbolCatalog,
  money,
  relativeTime,
  useEventFeed,
  useLogFeed,
  usePaged,
  usePoll,
  useSymbolCatalog,
  useTheme,
} from './hooks'

const mocks = vi.hoisted(() => ({
  events: vi.fn(),
  logs: vi.fn(),
  symbols: vi.fn(),
  refreshSymbols: vi.fn(),
}))

vi.mock('./api', () => ({
  api: { events: mocks.events, logs: mocks.logs, symbols: mocks.symbols, refreshSymbols: mocks.refreshSymbols },
}))

beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(new Date('2026-09-18T10:00:00.000Z'))
  mocks.events.mockReset()
  mocks.logs.mockReset()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('useTheme', () => {
  it('adopts a stored choice, applies it, and persists a deliberate toggle', () => {
    window.localStorage.setItem('veyra.theme', 'light')
    const { result } = renderHook(() => useTheme())

    // Hydration starts dark, then the stored choice is adopted before paint.
    expect(result.current.theme).toBe('light')
    expect(document.documentElement.dataset.theme).toBe('light')

    act(() => result.current.toggle())
    expect(result.current.theme).toBe('dark')
    expect(document.documentElement.dataset.theme).toBe('dark')
    expect(window.localStorage.getItem('veyra.theme')).toBe('dark')
  })

  it('falls back to the system preference when nothing is stored', () => {
    window.localStorage.clear()
    vi.stubGlobal('matchMedia', (query: string) => ({ matches: true, media: query }))
    const { result } = renderHook(() => useTheme())
    expect(result.current.theme).toBe('light')
    vi.unstubAllGlobals()
  })

  it('toggles from dark to light and back', () => {
    window.localStorage.clear()
    vi.stubGlobal('matchMedia', (query: string) => ({ matches: false, media: query }))
    const { result } = renderHook(() => useTheme())
    expect(result.current.theme).toBe('dark')
    act(() => result.current.toggle())
    expect(result.current.theme).toBe('light')
    act(() => result.current.toggle())
    expect(result.current.theme).toBe('dark')
    vi.unstubAllGlobals()
  })
})

describe('usePaged', () => {
  it('windows a list and keeps the page in range as it changes', () => {
    const { result, rerender } = renderHook(({ items }) => usePaged(items, 2), {
      initialProps: { items: [1, 2, 3, 4, 5] },
    })
    expect(result.current.items).toEqual([1, 2])
    expect(result.current.pages).toBe(3)

    act(() => result.current.next())
    expect(result.current.items).toEqual([3, 4])
    act(() => result.current.next())
    expect(result.current.items).toEqual([5])
    // Past the end the page clamps instead of stranding the reader.
    act(() => result.current.next())
    expect(result.current.items).toEqual([5])

    act(() => result.current.previous())
    expect(result.current.items).toEqual([3, 4])
    act(() => result.current.setPage(0))
    expect(result.current.items).toEqual([1, 2])
    act(() => result.current.previous())
    expect(result.current.page).toBe(0)

    // A shrinking list pulls the page back into range.
    act(() => result.current.setPage(2))
    rerender({ items: [1] })
    expect(result.current.pages).toBe(1)
    expect(result.current.items).toEqual([1])
  })
})

describe('relativeTime', () => {
  it('formats each magnitude', () => {
    const now = Date.now()
    expect(relativeTime(now - 5_000)).toBe('5s ago')
    expect(relativeTime(now - 90_000)).toBe('1m ago')
    expect(relativeTime(now - 2 * 3_600_000)).toBe('2h ago')
    expect(relativeTime(now - 3 * 86_400_000)).toBe('3d ago')
  })

  it('clamps future timestamps to zero', () => {
    expect(relativeTime(Date.now() + 5_000)).toBe('0s ago')
  })
})

describe('clockTime', () => {
  it('renders a 24-hour wall clock', () => {
    expect(clockTime(Date.now())).toMatch(/^\d{2}:\d{2}:\d{2}$/)
  })
})

describe('money', () => {
  it('formats numbers and keeps gaps explicit', () => {
    expect(money(undefined)).toBe('—')
    expect(money(2.5)).toBe('2.50')
    expect(money(0)).toBe('0.00')
  })
})

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (cause: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

describe('usePoll', () => {
  it('polls, keeps the last good value through an error, and stops on unmount', async () => {
    const load = vi
      .fn<() => Promise<string>>()
      .mockResolvedValueOnce('first')
      .mockRejectedValueOnce(new Error('boom'))
      .mockResolvedValue('third')

    const { result, unmount } = renderHook(() => usePoll(load, 1_000))
    await act(async () => undefined)
    expect(result.current.data).toBe('first')
    expect(result.current.error).toBeUndefined()

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })
    expect(result.current.error).toBe('boom')
    expect(result.current.data).toBe('first')

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })
    expect(result.current.error).toBeUndefined()
    expect(result.current.data).toBe('third')

    const calls = load.mock.calls.length
    unmount()
    await vi.advanceTimersByTimeAsync(5_000)
    expect(load.mock.calls.length).toBe(calls)
  })

  it('stringifies non-Error failures', async () => {
    const load = vi.fn().mockRejectedValue('nope')
    const { result } = renderHook(() => usePoll(load, 1_000))
    await act(async () => undefined)
    expect(result.current.error).toBe('nope')
  })

  it('ignores late resolution and rejection after unmount', async () => {
    const lateValue = deferred<string>()
    const resolved = renderHook(() => usePoll(() => lateValue.promise, 1_000))
    resolved.unmount()
    await act(async () => {
      lateValue.resolve('late')
      await Promise.resolve()
    })
    expect(resolved.result.current.data).toBeUndefined()

    const lateFailure = deferred<string>()
    const rejected = renderHook(() => usePoll(() => lateFailure.promise, 1_000))
    rejected.unmount()
    await act(async () => {
      lateFailure.reject(new Error('late'))
      await Promise.resolve()
    })
    expect(rejected.result.current.error).toBeUndefined()
  })
})

describe('useEventFeed', () => {
  const feedEvent = (seq: number) => ({ seq, at_ms: seq, kind: 'proposal_evaluated', payload: {} })

  it('reconnects after a failure and prepends batches newest-first', async () => {
    mocks.events
      .mockRejectedValueOnce(new Error('down'))
      .mockResolvedValueOnce({ events: [feedEvent(1), feedEvent(2)], latest: 2, next: 2 })
      .mockImplementation(() => new Promise(() => undefined))

    const { result } = renderHook(() => useEventFeed(80))
    await act(async () => undefined)
    expect(result.current.connected).toBe(false)

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000)
    })
    expect(result.current.connected).toBe(true)
    expect(result.current.events.map((event) => event.seq)).toEqual([2, 1])
    // A short first page ends the backfill, so the next call long-polls.
    expect(mocks.events).toHaveBeenNthCalledWith(1, 0, 0, 200)
    expect(mocks.events).toHaveBeenLastCalledWith(2, 15000, 200)
  })

  it('pages through the whole ring before long-polling and keeps notable events apart', async () => {
    const page = Array.from({ length: 200 }, (_, index) => ({
      seq: index + 1,
      at_ms: index + 1,
      kind: index === 5 ? 'proposal_evaluated' : 'broker_snapshot',
      payload: {},
    }))
    mocks.events
      .mockResolvedValueOnce({ events: page, latest: 201, next: 200 })
      .mockResolvedValueOnce({ events: [{ seq: 201, at_ms: 201, kind: 'agent_turn', payload: {} }], latest: 201, next: 201 })
      .mockImplementation(() => new Promise(() => undefined))

    const { result } = renderHook(() => useEventFeed(80))
    expect(result.current.settled).toBe(false)
    await act(async () => undefined)

    expect(mocks.events).toHaveBeenNthCalledWith(1, 0, 0, 200)
    // A full page means more may be buffered: keep paging without waiting.
    expect(mocks.events).toHaveBeenNthCalledWith(2, 200, 0, 200)
    expect(mocks.events).toHaveBeenNthCalledWith(3, 201, 15000, 200)
    expect(result.current.events).toHaveLength(80)
    expect(result.current.notable.map((event) => event.seq)).toEqual([6])
    expect(result.current.settled).toBe(true)
  })

  it('uses the default ring capacity for an empty batch', async () => {
    mocks.events
      .mockResolvedValueOnce({ events: [], latest: 0, next: 0 })
      .mockImplementation(() => new Promise(() => undefined))

    const { result } = renderHook(() => useEventFeed())
    await act(async () => undefined)
    expect(result.current.connected).toBe(true)
    expect(result.current.events).toEqual([])
  })

  it('drops a transport failure that lands after unmount', async () => {
    const pending = deferred<never>()
    mocks.events.mockImplementation(() => pending.promise)

    const { result, unmount } = renderHook(() => useEventFeed(80))
    await act(async () => undefined)
    unmount()
    await act(async () => {
      pending.reject(new Error('late'))
      await Promise.resolve()
    })
    expect(result.current.connected).toBe(false)
    expect(result.current.settled).toBe(false)
  })

  it('caps the ring and ignores a batch that arrives after unmount', async () => {
    let release: ((feed: { events: ReturnType<typeof feedEvent>[]; latest: number; next: number }) => void) | undefined
    mocks.events.mockImplementation(
      () =>
        new Promise((resolve) => {
          release = resolve
        }),
    )

    const { result, unmount } = renderHook(() => useEventFeed(1))
    await act(async () => undefined)

    await act(async () => {
      release?.({ events: [feedEvent(1), feedEvent(2)], latest: 2, next: 2 })
      await vi.advanceTimersByTimeAsync(0)
    })
    expect(result.current.events.map((event) => event.seq)).toEqual([2])

    unmount()
    await act(async () => {
      release?.({ events: [feedEvent(3)], latest: 3, next: 3 })
      await vi.advanceTimersByTimeAsync(0)
    })
    expect(result.current.events.map((event) => event.seq)).toEqual([2])
  })
})

describe('useLogFeed', () => {
  const record = (seq: number) => ({
    seq,
    atMs: seq,
    level: 'info',
    target: 'veyra_service::test',
    message: `line ${seq}`,
    fields: {},
  })

  it('tails, follows the cursor, and caps the list', async () => {
    mocks.logs
      .mockResolvedValueOnce({ logs: [record(1), record(2)], latest: 2 })
      .mockResolvedValueOnce({ logs: [record(3)], latest: 3 })
      .mockImplementation(() => new Promise(() => undefined))

    const { result } = renderHook(() => useLogFeed('info', 2))
    await act(async () => undefined)
    expect(result.current.logs.map((entry) => entry.seq)).toEqual([1, 2])

    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000)
    })
    expect(result.current.logs.map((entry) => entry.seq)).toEqual([2, 3])
    expect(mocks.logs).toHaveBeenLastCalledWith(2, 'info')
  })

  it('re-tails from the start when the level changes', async () => {
    mocks.logs
      .mockResolvedValueOnce({ logs: [record(1)], latest: 1 })
      .mockResolvedValueOnce({ logs: [record(2)], latest: 2 })
      .mockImplementation(() => new Promise(() => undefined))

    const { result, rerender } = renderHook(({ level }) => useLogFeed(level), {
      initialProps: { level: 'info' as LogLevel },
    })
    await act(async () => undefined)
    expect(result.current.logs.map((entry) => entry.seq)).toEqual([1])

    rerender({ level: 'error' })
    await act(async () => undefined)
    expect(mocks.logs).toHaveBeenLastCalledWith(undefined, 'error')
    expect(result.current.logs.map((entry) => entry.seq)).toEqual([2])
  })

  it('keeps the last page when a poll fails', async () => {
    mocks.logs
      .mockResolvedValueOnce({ logs: [record(1)], latest: 1 })
      .mockRejectedValueOnce(new Error('logs down'))
      .mockImplementation(() => new Promise(() => undefined))

    const { result } = renderHook(() => useLogFeed('warn'))
    await act(async () => undefined)
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_000)
    })
    expect(result.current.error).toBe('logs down')
    expect(result.current.logs.map((entry) => entry.seq)).toEqual([1])
  })

  it('stringifies non-Error failures', async () => {
    mocks.logs.mockRejectedValueOnce('nope').mockImplementation(() => new Promise(() => undefined))
    const { result } = renderHook(() => useLogFeed('info'))
    await act(async () => undefined)
    expect(result.current.error).toBe('nope')
  })

  it('ignores a page that resolves after unmount', async () => {
    const late = deferred<{ logs: ReturnType<typeof record>[]; latest: number }>()
    mocks.logs.mockImplementation(() => late.promise)

    const { result, unmount } = renderHook(() => useLogFeed('info'))
    await act(async () => undefined)
    unmount()
    await act(async () => {
      late.resolve({ logs: [record(1)], latest: 1 })
      await Promise.resolve()
    })
    expect(result.current.logs).toEqual([])
  })

  it('ignores a failure that lands after unmount', async () => {
    const late = deferred<{ logs: ReturnType<typeof record>[]; latest: number }>()
    mocks.logs.mockImplementation(() => late.promise)

    const { result, unmount } = renderHook(() => useLogFeed('info'))
    await act(async () => undefined)
    unmount()
    await act(async () => {
      late.reject(new Error('late'))
      await Promise.resolve()
    })
    expect(result.current.error).toBeUndefined()
  })

  it('stops polling after unmount', async () => {
    mocks.logs
      .mockResolvedValueOnce({ logs: [], latest: 0 })
      .mockImplementation(() => new Promise(() => undefined))
    const { unmount } = renderHook(() => useLogFeed('trace'))
    await act(async () => undefined)
    const calls = mocks.logs.mock.calls.length
    unmount()
    await vi.advanceTimersByTimeAsync(6_000)
    expect(mocks.logs.mock.calls.length).toBe(calls)
  })
})

describe('useSymbolCatalog', () => {
  const held = {
    ready: true as const,
    server: 'Demo-Server',
    fetchedAt: 1_000,
    total: 1,
    skipped: 0,
    categories: [{ id: 'forex' as const, label: 'Forex', count: 1 }],
    symbols: [{ name: 'EURUSD', description: '', path: 'Forex\\EURUSD', category: 'forex' as const, riskAllowed: true }],
  }
  const waiting = { ready: false as const, reason: 'waiting for the terminal', categories: [] as [], symbols: [] as [] }

  beforeEach(() => {
    forgetSymbolCatalog()
    mocks.symbols.mockReset()
    mocks.refreshSymbols.mockReset()
  })

  it('asks once when the list is ready and then leaves the service alone', async () => {
    mocks.symbols.mockResolvedValue(held)
    const { result } = renderHook(() => useSymbolCatalog())
    await act(async () => {})
    expect(result.current.catalog).toEqual(held)
    expect(result.current.error).toBeUndefined()

    await act(async () => {
      await vi.advanceTimersByTimeAsync(120_000)
    })
    expect(mocks.symbols).toHaveBeenCalledTimes(1)
  })

  it('keeps asking every ten seconds until the terminal has listed its instruments', async () => {
    mocks.symbols.mockResolvedValueOnce(waiting).mockResolvedValueOnce(waiting).mockResolvedValue(held)
    const { result } = renderHook(() => useSymbolCatalog())
    await act(async () => {})
    expect(result.current.catalog).toEqual(waiting)

    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000)
    })
    expect(result.current.catalog).toEqual(waiting)
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000)
    })
    expect(result.current.catalog).toEqual(held)
    expect(mocks.symbols).toHaveBeenCalledTimes(3)
  })

  it('never lets a later not-ready answer replace a list it already holds', async () => {
    mocks.symbols.mockResolvedValueOnce(held).mockResolvedValue(waiting)
    const { result } = renderHook(() => useSymbolCatalog())
    await act(async () => {})
    await act(async () => {
      await result.current.reload()
    })
    expect(mocks.symbols).toHaveBeenCalledTimes(2)
    expect(result.current.catalog).toEqual(held)
  })

  it('keeps the list across a remount and a page reload', async () => {
    mocks.symbols.mockResolvedValue(held)
    const first = renderHook(() => useSymbolCatalog())
    await act(async () => {})
    first.unmount()

    // Remount in the same page: the list is there before any request answers.
    mocks.symbols.mockReturnValue(new Promise(() => undefined))
    const remounted = renderHook(() => useSymbolCatalog())
    expect(remounted.result.current.catalog).toEqual(held)
    remounted.unmount()

    // A reload keeps only session storage.
    expect(sessionStorage.getItem('veyra.symbolCatalog.v1')).toContain('EURUSD')
    const reloaded = renderHook(() => useSymbolCatalog())
    expect(reloaded.result.current.catalog).toEqual(held)
  })

  it('ignores stored data it cannot use', async () => {
    mocks.symbols.mockReturnValue(new Promise(() => undefined))
    for (const bad of ['not json', JSON.stringify({ ready: false }), JSON.stringify({ ready: true, symbols: 'x' })]) {
      forgetSymbolCatalog()
      sessionStorage.setItem('veyra.symbolCatalog.v1', bad)
      const { result, unmount } = renderHook(() => useSymbolCatalog())
      expect(result.current.catalog).toBeUndefined()
      unmount()
    }
  })

  it('works when session storage is unavailable', async () => {
    const getItem = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked')
    })
    const setItem = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('blocked')
    })
    const removeItem = vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
      throw new Error('blocked')
    })
    mocks.symbols.mockResolvedValue(held)
    const { result } = renderHook(() => useSymbolCatalog())
    await act(async () => {})
    expect(result.current.catalog).toEqual(held)
    expect(() => forgetSymbolCatalog()).not.toThrow()
    getItem.mockRestore()
    setItem.mockRestore()
    removeItem.mockRestore()
  })

  it('reports an unreachable service and recovers on the next answer', async () => {
    mocks.symbols.mockRejectedValueOnce(new Error('/symbols → 503')).mockResolvedValue(held)
    const { result } = renderHook(() => useSymbolCatalog())
    await act(async () => {})
    expect(result.current.error).toBe('/symbols → 503')
    expect(result.current.catalog).toBeUndefined()

    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000)
    })
    expect(result.current.error).toBeUndefined()
    expect(result.current.catalog).toEqual(held)
  })

  it('asks the terminal again on refresh and reports a refusal', async () => {
    mocks.symbols.mockResolvedValue(held)
    mocks.refreshSymbols.mockResolvedValueOnce({ status: 'refreshed', count: 1 })
    const { result } = renderHook(() => useSymbolCatalog())
    await act(async () => {})
    await act(async () => {
      await result.current.refresh()
    })
    expect(mocks.refreshSymbols).toHaveBeenCalledTimes(1)
    expect(mocks.symbols).toHaveBeenCalledTimes(2)
    expect(result.current.refreshing).toBe(false)

    mocks.refreshSymbols.mockRejectedValueOnce(new Error('the terminal is not connected'))
    await act(async () => {
      await result.current.refresh()
    })
    expect(result.current.error).toBe('the terminal is not connected')
    expect(result.current.refreshing).toBe(false)
    expect(result.current.catalog).toEqual(held)
  })
})
