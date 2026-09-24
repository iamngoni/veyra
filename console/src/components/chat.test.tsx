/** The assistant shows each model-requested read separately, including repeats. */

import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

import { streamAssistant, type AssistantEvent } from '../lib/api'
import { AssistantChat } from './chat'

vi.mock('../lib/api', () => ({ streamAssistant: vi.fn() }))

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn()
})

afterEach(() => {
  cleanup()
  vi.resetAllMocks()
  vi.unstubAllGlobals()
  delete (Element.prototype as { scrollIntoView?: unknown }).scrollIntoView
})

it('renders actual SSE tool payloads with omitted labels and nullable counts', async () => {
  const actualApi = await vi.importActual<typeof import('../lib/api')>('../lib/api')
  vi.mocked(streamAssistant).mockImplementation(actualApi.streamAssistant)
  const tools = [
    ['positions', 'Reading open positions'],
    ['account', 'Reading account summary'],
    ['activity', 'Reading recent activity'],
    ['model_status', 'Checking model health'],
    ['balance_history', 'Reading balance history'],
    ['performance', 'Reading trading performance'],
    ['recent_commands', 'Reading recent command outcomes'],
    ['market_sessions', 'Checking market sessions'],
    ['market_spec', 'Reading market contract details'],
    ['market_candles', 'Reading recent market candles'],
    ['calendar', 'Reading economic calendar'],
    ['__proto__', 'Reading additional data'],
  ]
  // Matches SseProgress in assistant_chat.rs: start has no label, result count
  // is null for tools whose response has neither events nor positions arrays.
  const frames = tools.flatMap(([tool], index) => [
    `event: tool_start\ndata: ${JSON.stringify({ call_id: `call-${index}`, tool })}\n\n`,
    `event: tool_result\ndata: ${JSON.stringify({ call_id: `call-${index}`, tool, available: index !== 3, count: index === 0 ? 0 : index === 2 ? 1 : null })}\n\n`,
  ])
  frames.push('event: answer\ndata: {"text":"The requested observations were checked."}\n\n')
  const fetchMock = vi.fn().mockResolvedValue(new Response(frames.join(''), {
    headers: { 'content-type': 'text/event-stream' },
  }))
  vi.stubGlobal('fetch', fetchMock)
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))

  expect(await screen.findByText('The requested observations were checked.')).toBeTruthy()
  const rows = within(screen.getByRole('list', { name: 'Read-only tool activity' })).getAllByRole('listitem')
  expect(rows).toHaveLength(tools.length)
  for (const [index, [, label]] of tools.entries()) {
    expect(within(rows[index]).getByText(label)).toBeTruthy()
    const result = index === 0 ? '0 records' : index === 2 ? '1 record' : index === 3 ? 'Unavailable' : 'Complete'
    expect(within(rows[index]).getByText(result)).toBeTruthy()
  }
  expect(screen.queryByText(/null records/)).toBeNull()
  expect(fetchMock.mock.calls[0][0]).toBe('/api/assistant/chat')
})

it('shows repeated read-only tool calls as distinct progress steps', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'tool_start', call_id: 'read-1', tool: 'performance', label: 'Reading daily performance' })
    onEvent({ event: 'tool_result', call_id: 'read-1', tool: 'performance', available: true, count: 7 })
    onEvent({ event: 'tool_start', call_id: 'read-2', tool: 'performance', label: 'Reading monthly performance' })
    onEvent({ event: 'tool_result', call_id: 'read-2', tool: 'performance', available: false, reason: 'unavailable' })
    onEvent({ event: 'answer', text: 'Seven daily observations are available.' })
  })

  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.change(screen.getByRole('textbox', { name: /Ask about positions/ }), { target: { value: 'How is performance?' } })
  fireEvent.click(screen.getByRole('button', { name: /Send/ }))

  expect(await screen.findByText('Seven daily observations are available.')).toBeTruthy()
  expect(screen.getByText('Reading daily performance')).toBeTruthy()
  expect(screen.getByText('Reading monthly performance')).toBeTruthy()
  expect(screen.getByText('7 records')).toBeTruthy()
  expect(screen.getByText('unavailable')).toBeTruthy()
})

it('shows a stopped read clearly and ignores late events after cancellation', async () => {
  let finish: () => void = () => {}
  let emit: (event: AssistantEvent) => void = () => {}
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    emit = onEvent
    onEvent({ event: 'tool_start', call_id: 'pending', tool: 'positions', label: 'Reading positions' })
    await new Promise<void>((resolve) => { finish = resolve })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  expect(screen.queryByRole('button', { name: 'Ask Veyra' })).toBeNull()
  fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Read positions' } })
  fireEvent.click(screen.getByRole('button', { name: /Send/ }))
  fireEvent.click(screen.getByRole('button', { name: 'Stop' }))
  expect(screen.getByText('Stopped')).toBeTruthy()
  expect(screen.getByText('Not completed')).toBeTruthy()
  await act(async () => { emit({ event: 'answer', text: 'Late answer' }); finish() })
  expect(screen.queryByText('Late answer')).toBeNull()
  expect(screen.queryByText(/connection ended/)).toBeNull()
  expect(screen.getByText('Stopped')).toBeTruthy()
})

it('renders assistant Markdown and GFM tables while keeping the user message plain', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'answer', text: [
      '### Current summary',
      '',
      '**Read only** with *recorded* evidence and `position_id`.',
      '',
      '- First observation',
      '- Second observation',
      '',
      '| Item | Result |',
      '| --- | --- |',
      '| Checks | Available |',
      '',
      '[Reference](https://example.com/reference)',
    ].join('\n') })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.change(screen.getByRole('textbox'), { target: { value: '**Keep this literal** <b>question</b>' } })
  fireEvent.click(screen.getByRole('button', { name: /Send/ }))

  expect(await screen.findByRole('heading', { name: 'Current summary' })).toBeTruthy()
  expect(screen.getByText('Read only').tagName).toBe('STRONG')
  expect(screen.getByText('recorded').tagName).toBe('EM')
  expect(screen.getByText('position_id').tagName).toBe('CODE')
  expect(screen.getAllByRole('listitem')).toHaveLength(2)
  const tableRegion = screen.getByRole('region', { name: 'Assistant table' })
  expect(tableRegion.tabIndex).toBe(0)
  expect(tableRegion.className).toBe('assistant-table-scroll')
  const table = within(tableRegion).getByRole('table')
  expect(within(table).getByRole('columnheader', { name: 'Item' })).toBeTruthy()
  expect(within(table).getByRole('cell', { name: 'Available' })).toBeTruthy()
  const reference = screen.getByRole('link', { name: 'Reference' })
  expect(reference.getAttribute('href')).toBe('https://example.com/reference')
  expect(reference.getAttribute('rel')).toBe('noopener noreferrer')
  expect(document.body.querySelector('.is-user .assistant-answer')?.textContent).toBe('**Keep this literal** <b>question</b>')
  expect(document.body.querySelector('.is-user strong, .is-user b')).toBeNull()
})

it('does not render raw HTML, executable links, or remote images from model output', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'answer', text: [
      '<script>alert(1)</script>',
      '',
      '<img src="https://example.com/track" onerror="alert(1)">',
      '',
      '[Unsafe script](javascript:alert%281%29)',
      '',
      '[Unsafe data](data:text/html;base64,PHNjcmlwdD4=)',
      '',
      '![Image description](https://example.com/track)',
      '',
      '```html',
      '<script>shown as code</script>',
      '```',
    ].join('\n') })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))

  expect(await screen.findByText('Unsafe script')).toBeTruthy()
  expect(screen.getByText('Unsafe data')).toBeTruthy()
  expect(screen.getByText('Image description')).toBeTruthy()
  expect(document.body.querySelector('.assistant-answer script, .assistant-answer img, .assistant-answer iframe')).toBeNull()
  expect(screen.queryByRole('link')).toBeNull()
  expect(document.body.querySelector('.assistant-answer pre code')?.textContent).toBe('<script>shown as code</script>\n')
})

it('keeps assistant error messages unformatted', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'error', reason: '**Literal failure** <img src=x>' })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))

  expect(await screen.findByText('**Literal failure** <img src=x>')).toBeTruthy()
  expect(document.body.querySelector('.is-error strong, .is-error img')).toBeNull()
})
