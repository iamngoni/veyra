/**
 * Read-only Veyra assistant drawer. The browser keeps only this tab's chat
 * history; no account observations or credentials are persisted locally.
 */

import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import Markdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'

import { streamAssistant, type AssistantEvent, type AssistantTurn } from '../lib/api'
import '../styles/chat.css'

type ToolStep = { callId?: string; tool: string; label: string; available?: boolean; count?: number; reason?: string }
type Message = {
  id: number
  role: 'user' | 'assistant'
  text: string
  steps?: ToolStep[]
  status?: string
  done?: boolean
  error?: boolean
}

const SUGGESTIONS = ['What positions am I holding?', 'Why are we holding them?', 'What changed recently?']

// The SSE contract supplies tool IDs, not presentation labels. Keep unknown
// future tools readable without exposing their implementation names.
const TOOL_LABELS = new Map<string, string>([
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
])

// Model output is untrusted. Keep react-markdown's safe URL transform, omit raw
// HTML, and show image descriptions without making remote image requests.
const MARKDOWN_COMPONENTS: Components = {
  img: ({ alt }) => alt ? <span>{alt}</span> : null,
  a: ({ href, children }) => href
    ? <a href={href} target="_blank" rel="noopener noreferrer">{children}</a>
    : <span>{children}</span>,
  table: ({ children }) => (
    <div className="assistant-table-scroll" role="region" aria-label="Assistant table" tabIndex={0}>
      <table>{children}</table>
    </div>
  ),
}

/**
 * Launcher for the view's header plus a non-modal right-side drawer with a
 * live tool trace. The drawer renders at the document root so no header or
 * scroll container can clip or stack over it.
 */
export function AssistantChat() {
  const [open, setOpen] = useState(false)
  const [draft, setDraft] = useState('')
  const [messages, setMessages] = useState<Message[]>([])
  const [pendingId, setPendingId] = useState<number>()
  const nextId = useRef(1)
  const controller = useRef<AbortController | null>(null)
  const launcher = useRef<HTMLButtonElement>(null)
  const input = useRef<HTMLTextAreaElement>(null)
  const end = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (open) input.current?.focus()
  }, [open])

  useEffect(() => {
    const reducedMotion = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches
    end.current?.scrollIntoView({ block: 'end', behavior: reducedMotion ? 'instant' : 'smooth' })
  }, [messages])

  useEffect(() => () => controller.current?.abort(), [])

  const updateMessage = (id: number, change: (message: Message) => Message) =>
    setMessages((current) => current.map((message) => (message.id === id ? change(message) : message)))

  const stop = () => {
    controller.current?.abort()
    if (pendingId !== undefined) {
      updateMessage(pendingId, (message) => ({ ...message, status: 'Stopped', done: true }))
      setPendingId(undefined)
    }
  }

  const close = () => {
    stop()
    setOpen(false)
    requestAnimationFrame(() => launcher.current?.focus())
  }

  const onEvent = (id: number, event: AssistantEvent) => {
    updateMessage(id, (message) => {
      switch (event.event) {
        case 'status':
          return { ...message, status: event.label }
        case 'tool_start':
          return {
            ...message,
            steps: [...(message.steps ?? []), {
              callId: event.call_id,
              tool: event.tool,
              label: event.label?.trim() || TOOL_LABELS.get(event.tool) || 'Reading additional data',
            }],
          }
        case 'tool_result': {
          const steps = [...(message.steps ?? [])]
          let index = -1
          for (let i = steps.length - 1; i >= 0; i--) {
            const step = steps[i]
            if (event.call_id ? step.callId === event.call_id : step.tool === event.tool && step.available === undefined) {
              index = i
              break
            }
          }
          if (index >= 0) steps[index] = { ...steps[index], available: event.available, count: event.count ?? undefined, reason: event.reason }
          return { ...message, steps }
        }
        case 'answer':
          return { ...message, text: event.text, status: undefined, done: true }
        case 'error':
          return { ...message, text: event.reason, status: undefined, done: true, error: true }
      }
    })
  }

  const send = async (text: string) => {
    const question = text.trim()
    if (!question || pendingId !== undefined) return
    const history: AssistantTurn[] = messages
      .filter((message) => message.done && !message.error && message.text)
      .slice(-6)
      .map((message) => ({ role: message.role, content: message.text }))
    const userId = nextId.current++
    const answerId = nextId.current++
    setDraft('')
    setMessages((current) => [
      ...current,
      { id: userId, role: 'user', text: question, done: true },
      { id: answerId, role: 'assistant', text: '', steps: [], status: 'Starting…' },
    ])
    setPendingId(answerId)
    const abort = new AbortController()
    controller.current = abort
    let answered = false
    try {
      await streamAssistant(
        question,
        history,
        (event) => {
          if (abort.signal.aborted) return
          if (event.event === 'answer' || event.event === 'error') answered = true
          onEvent(answerId, event)
        },
        abort.signal,
      )
      if (!answered && !abort.signal.aborted) {
        updateMessage(answerId, (message) => ({
          ...message,
          text: 'The connection ended before the assistant answered. Try again.',
          status: undefined,
          done: true,
          error: true,
        }))
      }
    } catch (error) {
      if (!abort.signal.aborted) {
        updateMessage(answerId, (message) => ({
          ...message,
          text: error instanceof Error ? error.message : 'The assistant could not answer.',
          status: undefined,
          done: true,
          error: true,
        }))
      }
    } finally {
      if (controller.current === abort) controller.current = null
      setPendingId((current) => (current === answerId ? undefined : current))
    }
  }

  return (
    <>
      <button
        ref={launcher}
        type="button"
        className="assistant-launcher"
        hidden={open}
        aria-controls="veyra-assistant"
        aria-expanded={open}
        onClick={() => (open ? close() : setOpen(true))}
      >
        <span className="assistant-launcher-mark" aria-hidden="true">✳</span>
        <span>Ask Veyra</span>
      </button>
      {open ? createPortal(
        <aside
          id="veyra-assistant"
          className="assistant-drawer"
          aria-label="Veyra assistant"
          onKeyDown={(event) => {
            if (event.key === 'Escape') close()
          }}
        >
          <header className="assistant-head">
            <div>
              <div className="assistant-eyebrow">READ-ONLY ASSISTANT</div>
              <h2>Ask Veyra</h2>
              <p>Answers grounded in current positions and recorded decisions.</p>
            </div>
            <button type="button" className="assistant-close" aria-label="Close assistant" onClick={close}>×</button>
          </header>

          <div className="assistant-conversation" role="log" aria-live="polite" aria-relevant="additions text">
            {messages.length === 0 ? (
              <div className="assistant-empty">
                <p>Find out what the system sees and why it acted.</p>
                <div className="assistant-suggestions">
                  {SUGGESTIONS.map((suggestion) => (
                    <button key={suggestion} type="button" disabled={pendingId !== undefined} onClick={() => void send(suggestion)}>
                      {suggestion}<span aria-hidden="true">↗</span>
                    </button>
                  ))}
                </div>
              </div>
            ) : null}
            {messages.map((message) => (
              <article key={message.id} className={`assistant-message is-${message.role}${message.error ? ' is-error' : ''}`}>
                {message.role === 'assistant' ? <div className="assistant-message-label">Veyra</div> : null}
                {message.steps && message.steps.length > 0 ? (
                  <ol className="assistant-steps" aria-label="Read-only tool activity">
                    {message.steps.map((step, index) => {
                      const unfinished = step.available === undefined
                      const stopped = unfinished && message.done
                      const state = stopped ? 'Not completed' : unfinished ? 'Reading' : step.available ? 'Complete' : 'Unavailable'
                      return (
                        <li key={step.callId ?? `${step.tool}-${index}`} className={step.available === false ? 'is-unavailable' : ''}>
                          <span className="assistant-step-icon" aria-hidden="true">{stopped ? '–' : unfinished ? '◌' : step.available ? '✓' : '!'}</span>
                          <span className="assistant-step-detail">
                            <span>{step.label}</span>
                            {step.reason ? <span className="assistant-step-reason">{step.reason.replaceAll('_', ' ')}</span> : null}
                          </span>
                          <small>{step.count !== undefined ? `${step.count} ${step.count === 1 ? 'record' : 'records'}` : state}</small>
                        </li>
                      )
                    })}
                  </ol>
                ) : null}
                {message.status ? <p className="assistant-status">{message.status}</p> : null}
                {message.text ? message.role === 'assistant' && !message.error ? (
                  <div className="assistant-answer assistant-markdown">
                    <Markdown remarkPlugins={[remarkGfm]} components={MARKDOWN_COMPONENTS} skipHtml>
                      {message.text}
                    </Markdown>
                  </div>
                ) : <p className="assistant-answer">{message.text}</p> : null}
              </article>
            ))}
            <div ref={end} />
          </div>

          <form
            className="assistant-compose"
            onSubmit={(event) => {
              event.preventDefault()
              void send(draft)
            }}
          >
            <label htmlFor="assistant-question">Ask about positions, activity, or model health</label>
            <textarea
              ref={input}
              id="assistant-question"
              value={draft}
              maxLength={2000}
              rows={2}
              placeholder="What's happening right now?"
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
                  event.preventDefault()
                  void send(draft)
                }
              }}
            />
            <div className="assistant-compose-actions">
              <span>Reads only · never places trades</span>
              {pendingId === undefined ? (
                <button type="submit" disabled={!draft.trim()}>Send <span aria-hidden="true">↗</span></button>
              ) : (
                <button type="button" onClick={stop}>Stop</button>
              )}
            </div>
          </form>
        </aside>,
        document.body,
      ) : null}
    </>
  )
}
