import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import { TOAST_MS, clearToasts, showToast } from '../lib/toast'
import { Toaster } from './toast'

afterEach(() => {
  cleanup()
  clearToasts()
  vi.useRealTimers()
})

describe('Toaster', () => {
  it('shows a raised failure with its reason and repeat count, and dismisses it on request', () => {
    render(<Toaster />)
    act(() => {
      showToast({ key: 'k', title: 'Refused: POST /risk/policy', detail: 'symbols: must list 1-64 comma-separated instrument symbols' })
      showToast({ key: 'k', title: 'Refused: POST /risk/policy', detail: 'symbols: must list 1-64 comma-separated instrument symbols' })
    })
    const alert = screen.getByRole('alert')
    expect(alert.textContent).toContain('Refused: POST /risk/policy')
    expect(alert.textContent).toContain('symbols: must list 1-64')
    expect(alert.textContent).toContain('×2')
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }))
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('leaves on its own after a while, but not while the operator is reading it', () => {
    vi.useFakeTimers()
    render(<Toaster />)
    act(() => {
      showToast({ title: 'Refused', detail: 'why' })
    })
    fireEvent.mouseEnter(screen.getByRole('alert'))
    act(() => vi.advanceTimersByTime(TOAST_MS.bad + 1))
    expect(screen.getByRole('alert')).toBeTruthy()
    fireEvent.mouseLeave(screen.getByRole('alert'))
    act(() => vi.advanceTimersByTime(TOAST_MS.bad + 1))
    expect(screen.queryByRole('alert')).toBeNull()
  })
})
