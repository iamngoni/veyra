/** Subscription UI distinguishes saved connections, pending sign-in and failed reads. */
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
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

it('names the selected provider distinctly from a merely connected one', async () => {
  vi.mocked(subscriptions.status).mockResolvedValue({ subscriptions: { codex: { connected: true }, claude_code: { connected: true } } })
  render(<SubscriptionConnections enabled selectedProvider="codex" />)
  const codexRow = (await screen.findByText('ChatGPT')).closest('li') as HTMLElement
  const claudeRow = screen.getByText('Claude').closest('li') as HTMLElement
  expect(within(codexRow).getByText('Selected')).toBeTruthy()
  expect(within(claudeRow).getByText('Connected')).toBeTruthy()
  expect(within(claudeRow).queryByText('Selected')).toBeNull()
})

it('reports a plain sign-in failure and a message-less one identically to the operator', async () => {
  vi.mocked(subscriptions.start).mockRejectedValueOnce('boom').mockRejectedValueOnce(new Error('provider unreachable'))
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  const connect = screen.getByRole('button', { name: 'Connect ChatGPT' })
  await waitFor(() => expect((connect as HTMLButtonElement).disabled).toBe(false))
  fireEvent.click(connect)
  expect(await screen.findByText('Could not start sign-in.')).toBeTruthy()
  fireEvent.click(connect)
  expect(await screen.findByText('provider unreachable')).toBeTruthy()
})

it('finishing Claude sign-in asks for the code#state pair, not a callback URL', async () => {
  vi.mocked(subscriptions.start).mockResolvedValue({ provider: 'claude_code', authorize_url: 'https://example.test/claude', state: 'test-state' })
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  const connect = screen.getByRole('button', { name: 'Connect Claude' })
  await waitFor(() => expect((connect as HTMLButtonElement).disabled).toBe(false))
  fireEvent.click(connect)
  await screen.findByText('Finish Claude sign-in')
  expect(screen.getByText(/paste the complete code#state value shown by Claude/)).toBeTruthy()
  expect(screen.getByLabelText('Claude authorization code')).toBeTruthy()

  fireEvent.change(screen.getByLabelText('Claude authorization code'), { target: { value: 'code#state' } })
  fireEvent.click(screen.getByRole('button', { name: 'Cancel' }))
  expect(screen.queryByText('Finish Claude sign-in')).toBeNull()
  expect(screen.queryByRole('button', { name: 'Finish connection' })).toBeNull()
})

it('surfaces a completion failure without dropping the pending sign-in', async () => {
  vi.mocked(subscriptions.start).mockResolvedValue({ provider: 'codex', authorize_url: 'https://example.test/authorize', state: 'test-state' })
  vi.mocked(subscriptions.complete).mockRejectedValueOnce({ weird: true })
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  const connect = screen.getByRole('button', { name: 'Connect ChatGPT' })
  await waitFor(() => expect((connect as HTMLButtonElement).disabled).toBe(false))
  fireEvent.click(connect)
  await screen.findByRole('link', { name: /Open sign-in/ })
  fireEvent.change(screen.getByLabelText('Localhost callback URL'), { target: { value: 'test-callback' } })
  fireEvent.click(screen.getByRole('button', { name: 'Finish connection' }))
  expect(await screen.findByText('Could not complete sign-in.')).toBeTruthy()
  expect(screen.getByRole('button', { name: 'Finish connection' })).toBeTruthy()
})

it('requires a token before disconnecting, then disconnects and clears the token', async () => {
  vi.mocked(subscriptions.status).mockResolvedValue({ subscriptions: { codex: { connected: true }, claude_code: { connected: false } } })
  vi.mocked(subscriptions.remove).mockResolvedValue({ deleted: true })
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  const disconnect = await screen.findByRole('button', { name: 'Disconnect ChatGPT' })
  fireEvent.click(disconnect)
  expect(document.activeElement).toBe(screen.getByLabelText('Operator token'))
  expect(subscriptions.remove).not.toHaveBeenCalled()
  expect(screen.getByText('Enter the operator token to disconnect.')).toBeTruthy()

  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  fireEvent.click(disconnect)
  await screen.findByText('Subscription disconnected.')
  expect(subscriptions.remove).toHaveBeenCalledWith('codex', 'test-operator')
  expect((screen.getByLabelText('Operator token') as HTMLInputElement).value).toBe('')
})

it('notes when disconnecting succeeds but the refreshed status cannot be read', async () => {
  vi.mocked(subscriptions.status).mockResolvedValueOnce({ subscriptions: { codex: { connected: true }, claude_code: { connected: false } } })
  vi.mocked(subscriptions.remove).mockResolvedValue({ deleted: true })
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  const disconnect = await screen.findByRole('button', { name: 'Disconnect ChatGPT' })
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  vi.mocked(subscriptions.status).mockRejectedValueOnce(new Error('offline'))
  fireEvent.click(disconnect)
  expect(await screen.findByText(/Subscription disconnected\./)).toBeTruthy()
  expect(screen.getByText('Subscription disconnected. Connection status could not be refreshed.')).toBeTruthy()
})

it('reports a disconnect failure, plain and message-less alike', async () => {
  vi.mocked(subscriptions.status).mockResolvedValue({ subscriptions: { codex: { connected: true }, claude_code: { connected: false } } })
  vi.mocked(subscriptions.remove).mockRejectedValueOnce(new Error('token rejected')).mockRejectedValueOnce('nope')
  render(<SubscriptionConnections enabled selectedProvider="openrouter" />)
  const disconnect = await screen.findByRole('button', { name: 'Disconnect ChatGPT' })
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  fireEvent.click(disconnect)
  expect(await screen.findByText('token rejected')).toBeTruthy()
  fireEvent.change(screen.getByLabelText('Operator token'), { target: { value: 'test-operator' } })
  fireEvent.click(disconnect)
  expect(await screen.findByText('Could not disconnect.')).toBeTruthy()
})
