/**
 * Render tests for the judge section: the active judge, the write-only
 * OpenAI key, the connection test and its result line, and the switch that
 * stays off-limits until a key is saved and a test has passed.
 */

import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/api')>()
  return { ...actual, judge: { select: vi.fn(), saveKey: vi.fn(), removeKey: vi.fn(), test: vi.fn() } }
})

import { judge, type JudgeSettings } from '../lib/api'
import { JudgeSection } from './judge'

const api = vi.mocked(judge)

function view(overrides: Partial<JudgeSettings> = {}, openai: Partial<JudgeSettings['openai']> = {}): JudgeSettings {
  return {
    provider: 'typesafe',
    fallbackAvailable: true,
    available: true,
    ...overrides,
    openai: { key: { set: false, hint: null }, model: 'gpt-6-luna', test: null, fallbacks: 0, ...openai },
  }
}

const saved = { key: { set: true, hint: 'WXYZ' } }
const passed = { ok: true, atMs: Date.UTC(2026, 8, 30, 9), latencyMs: 812, detail: 'Answered by gpt-6-luna', model: 'gpt-6-luna' }
const failed = { ...passed, ok: false, latencyMs: 90, detail: 'Decision API is not enabled for this user. (403)' }

beforeEach(() => {
  for (const mock of Object.values(api)) mock.mockReset()
})
afterEach(cleanup)

const token = (value = 'op-token') => fireEvent.change(screen.getByLabelText('Operator token'), { target: { value } })
const toggle = () => screen.getByRole('switch', { name: 'Use OpenAI Decisions instead of Jev' }) as HTMLButtonElement
const message = () => screen.getByRole('status').textContent
const active = () => document.querySelector('.judge-active')?.textContent

describe('JudgeSection', () => {
  it('waits for the first poll, then says why it cannot be used', () => {
    const { rerender } = render(<JudgeSection />)
    expect(screen.getByText('Loading…')).toBeTruthy()
    rerender(<JudgeSection error="/judge → 404" />)
    expect(screen.getByText('Unavailable: /judge → 404')).toBeTruthy()
    rerender(<JudgeSection settings={view({ available: false })} />)
    expect(screen.getByText('Needs encrypted credential storage on the service.')).toBeTruthy()
    expect(active()).toBe('TypeSafe Jev')
    expect(screen.queryByLabelText('Operator token')).toBeNull()
  })

  it('offers only the key until one is saved', async () => {
    const onRefresh = vi.fn()
    api.saveKey.mockResolvedValue(view({}, saved))
    render(<JudgeSection settings={view()} onRefresh={onRefresh} />)
    expect(active()).toBe('TypeSafe Jev')
    expect(screen.queryByRole('button', { name: 'Test' })).toBeNull()
    expect(screen.queryByRole('switch')).toBeNull()

    const save = screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement
    expect(save.disabled).toBe(true)
    fireEvent.change(screen.getByLabelText('OpenAI API key'), { target: { value: ' sk-proj-abcWXYZ ' } })
    expect(save.disabled).toBe(false)

    // The token is asked for before anything is sent.
    fireEvent.click(save)
    expect(message()).toBe('Enter the operator token first.')
    expect(document.activeElement).toBe(screen.getByLabelText('Operator token'))
    expect(api.saveKey).not.toHaveBeenCalled()

    token()
    await act(async () => fireEvent.click(save))
    expect(api.saveKey).toHaveBeenCalledWith('op-token', 'sk-proj-abcWXYZ')
    expect(message()).toBe('Key saved. Test it before switching.')
    expect(screen.getByText('Saved ····WXYZ')).toBeTruthy()
    expect(screen.getByText('Not tested')).toBeTruthy()
    expect(toggle().disabled).toBe(true)
    expect(toggle().parentElement?.getAttribute('title')).toBe('Run a passing test first.')
    expect(onRefresh).toHaveBeenCalledTimes(1)
  })

  it('reports a refused key without clearing the draft', async () => {
    api.saveKey.mockRejectedValue(new Error('OpenAI API keys start with sk-'))
    render(<JudgeSection settings={view()} />)
    token()
    fireEvent.change(screen.getByLabelText('OpenAI API key'), { target: { value: 'pk-123' } })
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Save' })))
    expect(message()).toBe('Not saved. OpenAI API keys start with sk-')
    expect((screen.getByLabelText('OpenAI API key') as HTMLInputElement).value).toBe('pk-123')
  })

  it('replaces or removes a saved key', async () => {
    api.removeKey.mockResolvedValue(view())
    render(<JudgeSection settings={view({}, saved)} />)
    fireEvent.click(screen.getByRole('button', { name: 'Replace' }))
    expect((screen.getByLabelText('OpenAI API key') as HTMLInputElement).value).toBe('')
    fireEvent.click(screen.getByRole('button', { name: 'Keep saved' }))
    expect(screen.getByText('Saved ····WXYZ')).toBeTruthy()

    token()
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Remove' })))
    expect(api.removeKey).toHaveBeenCalledWith('op-token')
    expect(message()).toBe('Key removed. Jev answers.')
    expect(screen.queryByText('Saved ····WXYZ')).toBeNull()
    expect(screen.queryByRole('switch')).toBeNull()
  })

  it('shows each test result and unlocks the switch only after a pass', async () => {
    api.test.mockResolvedValueOnce({ ok: false, latencyMs: 90, detail: failed.detail })
    api.test.mockResolvedValueOnce({ ok: true, latencyMs: 812, detail: passed.detail })
    render(<JudgeSection settings={view({}, saved)} />)
    token()

    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Test' })))
    expect(screen.getByText(`Failed · ${failed.detail}`)).toBeTruthy()
    expect(toggle().disabled).toBe(true)

    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Test' })))
    const result = screen.getByText('Passed · 812 ms')
    expect(result.className).toContain('tone-ok')
    expect(result.getAttribute('title')).toMatch(/^gpt-6-luna · /)
    expect(toggle().disabled).toBe(false)
    expect(toggle().parentElement?.getAttribute('title')).toBeNull()
    expect(message()).toBe('')
  })

  it('switches to OpenAI and back, and a failed test while on switches it off', async () => {
    api.select.mockResolvedValueOnce(view({ provider: 'openai' }, { ...saved, test: passed }))
    api.select.mockResolvedValueOnce(view({}, { ...saved, test: passed }))
    api.test.mockResolvedValue({ ok: false, latencyMs: 5, detail: 'Unreachable: timeout' })
    const { rerender } = render(<JudgeSection settings={view({}, { ...saved, test: passed })} />)
    token()

    await act(async () => fireEvent.click(toggle()))
    expect(api.select).toHaveBeenLastCalledWith('op-token', 'openai')
    expect(active()).toBe('OpenAI, Jev fallback')
    expect(toggle().getAttribute('aria-checked')).toBe('true')
    expect(message()).toBe('OpenAI answers first; Jev is the fallback.')

    await act(async () => fireEvent.click(toggle()))
    expect(api.select).toHaveBeenLastCalledWith('op-token', 'typesafe')
    expect(message()).toBe('Jev answers.')

    // A new poll replaces the copy the change returned.
    rerender(<JudgeSection settings={view({ provider: 'openai' }, { ...saved, test: passed, fallbacks: 3 })} />)
    expect(active()).toBe('OpenAI, Jev fallback')
    expect(screen.getByText('3 answered by Jev')).toBeTruthy()

    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Test' })))
    expect(active()).toBe('TypeSafe Jev')
    expect(screen.getByText('Failed · Unreachable: timeout')).toBeTruthy()
  })

  it('keeps OpenAI on after a passing test and explains a missing fallback', async () => {
    api.test.mockResolvedValue({ ok: true, latencyMs: 700, detail: passed.detail })
    const { rerender } = render(<JudgeSection settings={view({ provider: 'openai' }, { ...saved, test: passed })} />)
    token()
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Test' })))
    expect(active()).toBe('OpenAI, Jev fallback')
    expect(screen.getByText('Passed · 700 ms')).toBeTruthy()

    rerender(<JudgeSection settings={view({ fallbackAvailable: false }, { ...saved, test: passed })} />)
    expect(toggle().disabled).toBe(true)
    expect(toggle().parentElement?.getAttribute('title')).toBe('Needs TypeSafe Jev configured as the fallback.')
  })

  it('reports refused changes in the service’s words', async () => {
    api.test.mockRejectedValue(new Error('Save an OpenAI API key first.'))
    api.select.mockRejectedValue('offline')
    render(<JudgeSection settings={view({}, { ...saved, test: passed })} />)
    token()
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Test' })))
    expect(message()).toBe('Test not run. Save an OpenAI API key first.')
    await act(async () => fireEvent.click(toggle()))
    expect(message()).toBe('Not switched. The service refused the change.')
    expect(screen.getByRole('status').className).toContain('tone-bad')
  })

  it('locks every control while a change is in flight', async () => {
    let finish: (value: JudgeSettings) => void = () => undefined
    api.removeKey.mockReturnValue(new Promise((resolve) => (finish = resolve)))
    render(<JudgeSection settings={view({}, { ...saved, test: passed })} />)
    token()
    fireEvent.click(screen.getByRole('button', { name: 'Remove' }))
    expect(screen.getByRole('button', { name: 'Removing…' })).toBeTruthy()
    expect((screen.getByRole('button', { name: 'Test' }) as HTMLButtonElement).disabled).toBe(true)
    expect(toggle().disabled).toBe(true)
    await act(async () => finish(view()))
    expect(screen.queryByRole('button', { name: 'Test' })).toBeNull()
  })
})
