/** Operator controlled ChatGPT and Claude subscription connections. */

import { useEffect, useId, useRef, useState } from 'react'

import { subscriptions, type SubscriptionProvider, type SubscriptionStatus } from '../lib/api'

const PROVIDERS: Array<{ id: SubscriptionProvider; name: string; via: string; mark: string }> = [
  { id: 'codex', name: 'ChatGPT', via: 'via Codex', mark: '◎' },
  { id: 'claude_code', name: 'Claude', via: 'via Claude Code', mark: '✳' },
]

type Flow = { provider: SubscriptionProvider; authorizeUrl: string }

/** A connection can be saved without changing the active trading model. */
export function SubscriptionConnections({ enabled, selectedProvider, onRefresh }: {
  enabled: boolean
  selectedProvider: string
  onRefresh?: () => void
}) {
  const [status, setStatus] = useState<SubscriptionStatus>()
  const [statusFailed, setStatusFailed] = useState(false)
  const [checking, setChecking] = useState(true)
  const [token, setToken] = useState('')
  const [callback, setCallback] = useState('')
  const [flow, setFlow] = useState<Flow>()
  const [busy, setBusy] = useState(false)
  const [message, setMessage] = useState('')
  const [action, setAction] = useState<{ provider: SubscriptionProvider; kind: 'start' | 'complete' | 'remove' }>()
  const tokenInput = useRef<HTMLInputElement>(null)
  const flowLink = useRef<HTMLAnchorElement>(null)
  const tokenId = useId()
  const tokenHintId = `${tokenId}-hint`

  useEffect(() => {
    let active = true
    void subscriptions.status().then((value) => {
      if (active) { setStatus(value); setChecking(false) }
    }).catch(() => {
      if (active) { setStatusFailed(true); setChecking(false) }
    })
    return () => { active = false }
  }, [])

  const refresh = async () => {
    setChecking(true)
    try {
      setStatus(await subscriptions.status())
      setStatusFailed(false)
      onRefresh?.()
      return true
    } catch {
      setStatusFailed(true)
      return false
    } finally {
      setChecking(false)
    }
  }

  useEffect(() => {
    if (flow) flowLink.current?.focus()
  }, [flow])

  const start = async (provider: SubscriptionProvider) => {
    if (!token.trim()) {
      setMessage('Enter the operator token to start sign-in.')
      tokenInput.current?.focus()
      return
    }
    setBusy(true)
    setAction({ provider, kind: 'start' })
    setMessage('')
    try {
      const result = await subscriptions.start(provider, token)
      setFlow({ provider, authorizeUrl: result.authorize_url })
      setCallback('')
    } catch (error) {
      setMessage(error instanceof Error ? error.message : 'Could not start sign-in.')
    } finally {
      setBusy(false)
      setAction(undefined)
    }
  }

  const complete = async () => {
    if (!flow || !callback.trim()) return
    setBusy(true)
    setAction({ provider: flow.provider, kind: 'complete' })
    setMessage('')
    try {
      await subscriptions.complete(flow.provider, callback.trim(), token)
      setFlow(undefined)
      setCallback('')
      setToken('')
      const refreshed = await refresh()
      setMessage(`Subscription connected. Choose its provider and model IDs below, then Apply.${refreshed ? '' : ' Connection status could not be refreshed.'}`)
    } catch (error) {
      setMessage(error instanceof Error ? error.message : 'Could not complete sign-in.')
    } finally {
      setBusy(false)
      setAction(undefined)
    }
  }

  const remove = async (provider: SubscriptionProvider) => {
    if (!token.trim()) {
      setMessage('Enter the operator token to disconnect.')
      tokenInput.current?.focus()
      return
    }
    setBusy(true)
    setAction({ provider, kind: 'remove' })
    setMessage('')
    try {
      await subscriptions.remove(provider, token)
      setToken('')
      const refreshed = await refresh()
      setMessage(`Subscription disconnected.${refreshed ? '' : ' Connection status could not be refreshed.'}`)
    } catch (error) {
      setMessage(error instanceof Error ? error.message : 'Could not disconnect.')
    } finally {
      setBusy(false)
      setAction(undefined)
    }
  }

  return (
    <section className="tab-subscriptions" aria-label="Subscription connections">
      <div className="tab-subscriptions-heading">
        <div>
          <h4>Subscription connections</h4>
          <p>Use a ChatGPT or Claude account with access to Codex or Claude Code. Connect first, then choose its provider and model IDs below.</p>
        </div>
      </div>
      <div className="tab-subscriptions-grid">
        {PROVIDERS.map(({ id, name, via, mark }) => {
          const connection = status?.subscriptions[id]
          return (
            <div className="tab-subscription" key={id}>
              <span className="tab-subscription-mark" aria-hidden="true">{mark}</span>
              <div className="tab-subscription-main">
                <strong>{name}</strong>
                <span>{via}</span>
              </div>
              <span className={`tab-subscription-state${connection?.connected && !statusFailed && !checking ? ' is-connected' : ''}`}>
                {checking ? 'Checking…' : statusFailed ? 'Unavailable' : connection?.connected ? selectedProvider === id ? 'Selected' : 'Connected' : 'Not connected'}
              </span>
              <div className="tab-subscription-action">
                <button type="button" aria-label={`${connection?.connected ? 'Reconnect' : 'Connect'} ${name}`} disabled={!enabled || busy || checking || statusFailed || !!flow} onClick={() => void start(id)}>
                  {action?.provider === id && action.kind === 'start' ? 'Starting…' : connection?.connected ? 'Reconnect' : 'Connect'}
                </button>
                {connection?.connected ? <button type="button" aria-label={`Disconnect ${name}`} disabled={!enabled || busy || checking || statusFailed || !!flow} onClick={() => void remove(id)}>{action?.provider === id && action.kind === 'remove' ? 'Disconnecting…' : 'Disconnect'}</button> : null}
              </div>
            </div>
          )
        })}
      </div>
      {statusFailed ? <p className="tab-subscriptions-message" role="status">Connection status is unavailable. <button type="button" className="tab-link" disabled={checking || busy} onClick={() => void refresh()}>Retry</button></p> : null}
      {enabled ? (
        <div className="tab-subscriptions-token">
          <label htmlFor={tokenId}>Operator token</label>
          <input id={tokenId} ref={tokenInput} type="password" autoComplete="off" aria-describedby={tokenHintId} value={token} disabled={busy} onChange={(event) => setToken(event.target.value)} />
          <span id={tokenHintId}>Required to connect or disconnect. Cleared when you leave Settings.</span>
        </div>
      ) : <p className="tab-group-note">Configure encrypted console credential storage to connect subscriptions.</p>}
      {flow ? (
        <div className="tab-subscriptions-flow">
          <strong>Finish {flow.provider === 'codex' ? 'ChatGPT' : 'Claude'} sign-in</strong>
          <p>{flow.provider === 'codex'
            ? 'Approve access, then paste the complete localhost callback URL from your browser address bar. The localhost page may not load.'
            : 'Approve access, then paste the complete code#state value shown by Claude.'}</p>
          <a ref={flowLink} href={flow.authorizeUrl} target="_blank" rel="noopener noreferrer">Open sign-in ↗</a>
          <label>
            {flow.provider === 'codex' ? 'Localhost callback URL' : 'Claude authorization code'}
            <input type="password" value={callback} autoComplete="off" spellCheck={false} disabled={busy} onChange={(event) => setCallback(event.target.value)} />
          </label>
          <div className="tab-subscriptions-flow-actions">
            <button type="button" disabled={busy || !callback.trim()} onClick={() => void complete()}>{busy ? 'Connecting…' : 'Finish connection'}</button>
            <button type="button" disabled={busy} onClick={() => { setFlow(undefined); setCallback('') }}>Cancel</button>
          </div>
        </div>
      ) : null}
      {message ? <p className="tab-subscriptions-message" role="status">{message}</p> : null}
    </section>
  )
}
