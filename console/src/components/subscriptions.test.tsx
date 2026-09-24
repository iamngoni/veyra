/** Subscription UI distinguishes saved connections, pending sign-in and failed reads. */
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

import { subscriptions } from '../lib/api'
import { SubscriptionConnections } from './subscriptions'

vi.mock('../lib/api', () => ({ subscriptions: { status: vi.fn(), start: vi.fn(), complete: vi.fn(), remove: vi.fn() } }))

const disconnected = { subscriptions: { codex: { connected: false }, claude_code: { connected: false } } }

beforeEach(() => {
  vi.mocked(subscriptions.status).mockResolvedValue(disconnected)
})
afterEach(() => { cleanup(); vi.resetAllMocks() })

it('offers status recovery instead of leaving failed connections as Checking', async () => {
  vi.mocked(subscriptions.status).mockRejectedValueOnce(new Error('offline'))
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  const retry = await screen.findByRole('button', { name: 'Retry' })
  expect(screen.getAllByText('Unavailable')).toHaveLength(2)
  fireEvent.click(retry)
  await screen.findAllByText('Not connected')
  expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
})

it('focuses the required token without starting sign-in when it is missing', async () => {
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  const connect = screen.getByRole('button', { name: 'Connect ChatGPT' })
  await waitFor(() => expect((connect as HTMLButtonElement).disabled).toBe(false))
  fireEvent.click(connect)
  expect(document.activeElement).toBe(screen.getByLabelText('Operator token'))
  expect(subscriptions.start).not.toHaveBeenCalled()
  expect(screen.getByText('Enter the operator token to start sign-in.')).toBeTruthy()
})

it('clears authorization input after success even when the status refresh fails', async () => {
  vi.mocked(subscriptions.start).mockResolvedValue({ provider: 'codex', authorize_url: 'https://example.test/authorize', state: 'test-state' })
  vi.mocked(subscriptions.complete).mockResolvedValue({ connected: true })
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  const connect = screen.getByRole('button', { name: 'Connect ChatGPT' })
  await waitFor(() => expect((connect as HTMLButtonElement).disabled).toBe(false))
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  fireEvent.click(connect)
  await screen.findByRole('link', { name: /Open sign-in/ })
  const callback = screen.getByLabelText('Localhost callback URL') as HTMLInputElement
  expect(callback.type).toBe('password')
  fireEvent.change(callback, { target: { value: 'test-callback' } })
  vi.mocked(subscriptions.status).mockRejectedValueOnce(new Error('offline'))
  fireEvent.click(screen.getByRole('button', { name: 'Finish connection' }))
  await screen.findByText(/Subscription connected\./)
  expect(screen.queryByLabelText('Localhost callback URL')).toBeNull()
  expect((screen.getByLabelText('Operator token') as HTMLInputElement).value).toBe('')
  expect(subscriptions.complete).toHaveBeenCalledOnce()
})
