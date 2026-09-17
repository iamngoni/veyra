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
import { clockTime, money, relativeTime, useEventFeed, useLogFeed, usePoll } from './hooks'

const mocks = vi.hoisted(() => ({ events: vi.fn(), logs: vi.fn() }))

vi.mock('./api', () => ({ api: { events: mocks.events, logs: mocks.logs } }))

beforeEach(() => {
  vi.useFakeTimers()
  vi.setSystemTime(new Date('2026-09-18T10:00:00.000Z'))
  mocks.events.mockReset()
  mocks.logs.mockReset()
})

afterEach(() => {
  vi.useRealTimers()
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
    expect(mocks.events).toHaveBeenLastCalledWith(2)
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
