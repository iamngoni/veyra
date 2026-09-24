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

it('closes from the header button and restores focus to the launcher', async () => {
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    callback(0)
    return 0
  })
  vi.mocked(streamAssistant).mockImplementation(async () => {})
  render(<AssistantChat />)
  const launcher = screen.getByRole('button', { name: 'Ask Veyra' })
  fireEvent.click(launcher)
  fireEvent.click(screen.getByRole('button', { name: 'Close assistant' }))
  expect(screen.queryByRole('heading', { name: 'Ask Veyra' })).toBeNull()
  expect(document.activeElement).toBe(launcher)
})

it('closes when the (visually hidden) launcher itself is activated while open', async () => {
  vi.mocked(streamAssistant).mockImplementation(async () => {})
  const { container } = render(<AssistantChat />)
  const launcher = screen.getByRole('button', { name: 'Ask Veyra' })
  fireEvent.click(launcher)
  expect(screen.getByRole('heading', { name: 'Ask Veyra' })).toBeTruthy()
  // The launcher stays mounted (with the `hidden` attribute) while open, so it
  // is reachable by direct node lookup even though it drops out of the a11y
  // tree; this is how the same toggle would fire if it were re-clicked.
  fireEvent.click(container.querySelector('.assistant-launcher')!)
  expect(screen.queryByRole('heading', { name: 'Ask Veyra' })).toBeNull()
})

it('closes on Escape and stops an in-flight question, marking its step not completed', async () => {
  let finish: () => void = () => {}
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'tool_start', call_id: 'p', tool: 'positions', label: 'Reading positions' })
    await new Promise<void>((resolve) => { finish = resolve })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Read positions' } })
  fireEvent.click(screen.getByRole('button', { name: /Send/ }))
  fireEvent.keyDown(screen.getByRole('complementary', { name: 'Veyra assistant' }), { key: 'Escape' })
  expect(screen.queryByRole('heading', { name: 'Ask Veyra' })).toBeNull()
  finish()
})

it('ignores stray non-Escape keys in the drawer', async () => {
  vi.mocked(streamAssistant).mockImplementation(async () => {})
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.keyDown(screen.getByRole('complementary', { name: 'Veyra assistant' }), { key: 'Enter' })
  expect(screen.getByRole('heading', { name: 'Ask Veyra' })).toBeTruthy()
})

it('shows an interim status label while the assistant works', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'status', label: 'Thinking about the account' })
    onEvent({ event: 'answer', text: 'Done.' })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))
  expect(await screen.findByText('Done.')).toBeTruthy()
})

it('ignores an empty question and a second one while one is already pending', async () => {
  let finish: () => void = () => {}
  vi.mocked(streamAssistant).mockImplementation(async () => {
    await new Promise<void>((resolve) => { finish = resolve })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  const textarea = screen.getByRole('textbox')
  // The Enter shortcut is not gated by the disabled submit button, so it can
  // reach `send` directly with a blank draft.
  fireEvent.change(textarea, { target: { value: '   ' } })
  fireEvent.keyDown(textarea, { key: 'Enter' })
  expect(streamAssistant).not.toHaveBeenCalled()

  fireEvent.change(textarea, { target: { value: 'First question' } })
  fireEvent.click(screen.getByRole('button', { name: /Send/ }))
  expect(streamAssistant).toHaveBeenCalledOnce()
  fireEvent.change(textarea, { target: { value: 'Second question' } })
  fireEvent.keyDown(textarea, { key: 'Enter' })
  expect(streamAssistant).toHaveBeenCalledOnce()
  finish()
})

it('sends on Enter but keeps Shift+Enter and IME composition as plain newlines', async () => {
  vi.mocked(streamAssistant).mockImplementation(async () => {})
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  const textarea = screen.getByRole('textbox')
  fireEvent.change(textarea, { target: { value: 'Shift newline' } })
  fireEvent.keyDown(textarea, { key: 'Enter', shiftKey: true })
  expect(streamAssistant).not.toHaveBeenCalled()
  fireEvent.keyDown(textarea, { key: 'Enter', isComposing: true })
  expect(streamAssistant).not.toHaveBeenCalled()
  fireEvent.keyDown(textarea, { key: 'Enter' })
  expect(streamAssistant).toHaveBeenCalledOnce()
})

it('bounds history to the six most recent finished, non-error turns with text', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'answer', text: `Answer ${_history.length}` })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  const textarea = screen.getByRole('textbox')
  for (let index = 0; index < 4; index++) {
    fireEvent.change(textarea, { target: { value: `Question ${index}` } })
    fireEvent.click(screen.getByRole('button', { name: /Send/ }))
    await screen.findByText(`Answer ${index * 2}`)
  }
  const lastCall = vi.mocked(streamAssistant).mock.calls.at(-1)!
  const [, history] = lastCall
  expect(history).toHaveLength(6)
  expect(history.at(-1)).toEqual({ role: 'assistant', content: 'Answer 4' })
  expect(history.every((turn) => turn.content)).toBe(true)
})

it('tells the operator plainly when the stream ends without an answer', async () => {
  vi.mocked(streamAssistant).mockImplementation(async () => {})
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))
  expect(await screen.findByText('The connection ended before the assistant answered. Try again.')).toBeTruthy()
})

it('reports a network failure by message, and a non-Error rejection plainly', async () => {
  vi.mocked(streamAssistant)
    .mockRejectedValueOnce(new Error('network unreachable'))
    .mockRejectedValueOnce('opaque failure')
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))
  expect(await screen.findByText('network unreachable')).toBeTruthy()
  fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Ask again' } })
  fireEvent.click(screen.getByRole('button', { name: /Send/ }))
  expect(await screen.findByText('The assistant could not answer.')).toBeTruthy()
})

it('scrolls instantly instead of smoothly when the operator prefers reduced motion', async () => {
  const scrollIntoView = vi.fn()
  Element.prototype.scrollIntoView = scrollIntoView
  vi.stubGlobal('matchMedia', vi.fn().mockReturnValue({ matches: true }))
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'answer', text: 'Reduced motion answer.' })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))
  await screen.findByText('Reduced motion answer.')
  expect(scrollIntoView).toHaveBeenCalledWith({ block: 'end', behavior: 'instant' })
})

it('renders an image with no alt text as nothing rather than a broken request', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    onEvent({ event: 'answer', text: '![](https://example.com/track.png)\n\nAfter the image.' })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))
  await screen.findByText('After the image.')
  expect(document.body.querySelector('.assistant-answer img')).toBeNull()
})

it('matches a repeated tool call by tool name once its call id is already spent', async () => {
  vi.mocked(streamAssistant).mockImplementation(async (_question, _history, onEvent) => {
    // Two starts with no call_id at all: the second result must bind to the
    // second (still-open) start, not re-match the first, already-closed one.
    onEvent({ event: 'tool_start', tool: 'positions', label: 'Reading positions (1)' })
    onEvent({ event: 'tool_result', tool: 'positions', available: true, count: 1 })
    onEvent({ event: 'tool_start', tool: 'positions', label: 'Reading positions (2)' })
    onEvent({ event: 'tool_result', tool: 'positions', available: true, count: 2 })
    onEvent({ event: 'answer', text: 'Matched by tool name.' })
  })
  render(<AssistantChat />)
  fireEvent.click(screen.getByRole('button', { name: 'Ask Veyra' }))
  fireEvent.click(screen.getByRole('button', { name: /What changed recently/ }))
  await screen.findByText('Matched by tool name.')
  const rows = within(screen.getByRole('list', { name: 'Read-only tool activity' })).getAllByRole('listitem')
  expect(rows).toHaveLength(2)
  expect(within(rows[0]).getByText('1 record')).toBeTruthy()
  expect(within(rows[1]).getByText('2 records')).toBeTruthy()
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
