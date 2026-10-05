import { afterEach, describe, expect, it, vi } from 'vitest'

import { MAX_TOASTS, clearToasts, currentToasts, dismissToast, resolveToast, showToast, subscribeToasts } from './toast'

afterEach(() => clearToasts())

describe('toast store', () => {
  it('refreshes a repeated notice instead of stacking a copy', () => {
    const first = showToast({ key: 'load /x', title: 'Could not load /x', detail: 'HTTP 500' })
    const second = showToast({ key: 'load /x', title: 'Could not load /x', detail: 'HTTP 502' })
    expect(currentToasts()).toHaveLength(1)
    expect(currentToasts()[0]).toMatchObject({ id: second, count: 2, detail: 'HTTP 502' })
    expect(second).not.toBe(first)
  })

  it('keys an unkeyed notice by its words, defaults to an error, and keeps the newest few', () => {
    showToast({ title: 'Refused', detail: 'a' })
    showToast({ title: 'Refused', detail: 'a' })
    expect(currentToasts()).toHaveLength(1)
    expect(currentToasts()[0].tone).toBe('bad')
    for (let index = 0; index < MAX_TOASTS + 2; index++) showToast({ title: `n${index}` })
    expect(currentToasts()).toHaveLength(MAX_TOASTS)
    expect(currentToasts().at(-1)?.title).toBe(`n${MAX_TOASTS + 1}`)
  })

  it('dismisses by id, resolves by key, and tells subscribers', () => {
    const listener = vi.fn()
    const stop = subscribeToasts(listener)
    const id = showToast({ title: 'one' })
    showToast({ key: 'two', title: 'two' })
    dismissToast(id)
    resolveToast('two')
    resolveToast('missing')
    expect(currentToasts()).toEqual([])
    expect(listener).toHaveBeenCalledTimes(4)
    stop()
    showToast({ title: 'three' })
    expect(listener).toHaveBeenCalledTimes(4)
  })
})
