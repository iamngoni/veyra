/**
 * Judge selection: TypeSafe Jev by default, or OpenAI Decisions with Jev as
 * its automatic fallback.
 *
 * OpenAI is offered once a key is saved and can be switched on once a
 * connection test has passed; the service enforces both and says why when it
 * refuses. The key is write-only: the console shows its last four characters
 * and never reads it back. The operator token lives only in this view.
 */

import { useEffect, useId, useRef, useState } from 'react'

import { judge as judgeApi, type JudgeSettings, type JudgeTest } from '../lib/api'
import { Button, Control, ControlHead } from './form'
import { Dot, Hint, Toggle, type Tone } from './ui'
import '../styles/judge.css'

const SWITCH_LABEL = 'Use OpenAI Decisions instead of Jev'

const errorText = (error: unknown) => (error instanceof Error ? error.message : 'The service refused the change.')

/** The latest test as one line: what happened, and how long it took. */
function testLine(test: JudgeTest | null): { text: string; tone: Tone } {
  if (!test) return { text: 'Not tested', tone: 'idle' }
  return test.ok ? { text: `Passed · ${test.latencyMs} ms`, tone: 'ok' } : { text: `Failed · ${test.detail}`, tone: 'bad' }
}

/**
 * The judge section of the live settings.
 *
 * `settings` is the latest poll; a change's response stands in for it until
 * the next poll lands.
 */
export function JudgeSection({
  settings,
  error,
  onRefresh,
}: {
  settings?: JudgeSettings
  error?: string
  onRefresh?: () => void
}) {
  const [fresh, setFresh] = useState<JudgeSettings>()
  const [token, setToken] = useState('')
  const [key, setKey] = useState('')
  const [replacing, setReplacing] = useState(false)
  const [busy, setBusy] = useState<'save' | 'remove' | 'test' | 'switch'>()
  const [notice, setNotice] = useState<{ text: string; tone: 'ok' | 'bad' | 'warn' }>()
  const tokenInput = useRef<HTMLInputElement>(null)
  const headId = useId()
  const keyId = useId()
  const tokenId = useId()

  // A newer poll supersedes the copy a change returned.
  useEffect(() => setFresh(undefined), [settings])

  const view = fresh ?? settings
  const openai = view?.provider === 'openai'

  const head = (
    <div className="tab-group-head">
      <h3 id={headId}>Judge</h3>
      <span className="tab-group-note judge-note">
        {view ? (
          <span className="judge-active">
            <Dot tone={openai ? 'ok' : 'idle'} />
            {openai ? 'OpenAI, Jev fallback' : 'TypeSafe Jev'}
          </span>
        ) : null}
        <Hint
          label="Judge"
          text="Answers the direction, trend and momentum questions each cycle. With OpenAI on, Jev answers whenever OpenAI fails."
        />
      </span>
    </div>
  )

  if (!view || !view.available) {
    return (
      <section className="tab-group judge" aria-labelledby={headId}>
        {head}
        <p className="tab-group-note judge-body">
          {view
            ? 'Needs encrypted credential storage on the service.'
            : error
              ? `Unavailable: ${error}`
              : 'Loading…'}
        </p>
      </section>
    )
  }

  const locked = busy !== undefined

  const requireToken = () => {
    if (token.trim()) return token
    setNotice({ text: 'Enter the operator token first.', tone: 'warn' })
    tokenInput.current?.focus()
    return undefined
  }

  /** Runs one change with the token; a refusal is reported under `failed`. */
  const run = async (kind: NonNullable<typeof busy>, failed: string, change: (secret: string) => Promise<void>) => {
    const secret = requireToken()
    if (!secret) return
    setBusy(kind)
    setNotice(undefined)
    try {
      await change(secret)
      onRefresh?.()
    } catch (cause) {
      setNotice({ text: `${failed} ${errorText(cause)}`, tone: 'bad' })
    } finally {
      setBusy(undefined)
    }
  }

  const saveKey = () =>
    run('save', 'Not saved.', async (secret) => {
      setFresh(await judgeApi.saveKey(secret, key.trim()))
      setKey('')
      setReplacing(false)
      setNotice({ text: 'Key saved. Test it before switching.', tone: 'ok' })
    })

  const removeKey = () =>
    run('remove', 'Not removed.', async (secret) => {
      setFresh(await judgeApi.removeKey(secret))
      setNotice({ text: 'Key removed. Jev answers.', tone: 'ok' })
    })

  const test = () =>
    run('test', 'Test not run.', async (secret) => {
      const result = await judgeApi.test(secret)
      // Shown at once; the refresh that follows brings the saved copy. A
      // failed test also switches OpenAI off, as the service does.
      setFresh({
        ...view,
        provider: result.ok ? view.provider : 'typesafe',
        openai: {
          ...view.openai,
          test: { ...result, atMs: Date.now(), model: view.openai.model },
        },
      })
    })

  const flip = () =>
    run('switch', 'Not switched.', async (secret) => {
      setFresh(await judgeApi.select(secret, openai ? 'typesafe' : 'openai'))
      setNotice({ text: openai ? 'Jev answers.' : 'OpenAI answers first; Jev is the fallback.', tone: 'ok' })
    })

  const saved = view.openai.key
  const line = testLine(view.openai.test)
  // Switching back to Jev is always allowed.
  const switchBlocked = openai
    ? undefined
    : !view.fallbackAvailable
      ? 'Needs TypeSafe Jev configured as the fallback.'
      : view.openai.test?.ok
        ? undefined
        : 'Run a passing test first.'

  return (
    <section className="tab-group judge" aria-labelledby={headId}>
      {head}
      <div className="judge-body">
        <div className="tab-group-fields">
          {saved.set && !replacing ? (
            <div className="tab-control">
              <ControlHead label="OpenAI API key" help="Stored encrypted and never shown again. Replacing or removing it switches back to Jev." />
              <div className="judge-secret">
                <span className="judge-secret-state">Saved ····{saved.hint}</span>
                <button type="button" className="tab-link" disabled={locked} onClick={() => setReplacing(true)}>
                  Replace
                </button>
                <button type="button" className="tab-link" disabled={locked} onClick={() => void removeKey()}>
                  {busy === 'remove' ? 'Removing…' : 'Remove'}
                </button>
              </div>
            </div>
          ) : (
            <Control
              id={keyId}
              label="OpenAI API key"
              help="Stored encrypted and never shown again. Replacing or removing it switches back to Jev."
              aside={
                replacing ? (
                  <button
                    type="button"
                    className="tab-link"
                    disabled={locked}
                    onClick={() => {
                      setReplacing(false)
                      setKey('')
                    }}
                  >
                    Keep saved
                  </button>
                ) : undefined
              }
            >
              <div className="judge-key">
                <input
                  id={keyId}
                  type="password"
                  className="tab-input"
                  placeholder="sk-…"
                  value={key}
                  disabled={locked}
                  autoComplete="off"
                  spellCheck={false}
                  onChange={(event) => setKey(event.target.value)}
                />
                <Button tone="ok" disabled={locked || !key.trim()} onClick={() => void saveKey()}>
                  {busy === 'save' ? 'Saving…' : 'Save'}
                </Button>
              </div>
            </Control>
          )}
          <Control id={tokenId} label="Operator token" help="Required to change the judge or run a test. Kept only until you leave this tab.">
            <input
              id={tokenId}
              ref={tokenInput}
              type="password"
              className="tab-input"
              autoComplete="off"
              spellCheck={false}
              value={token}
              onChange={(event) => setToken(event.target.value)}
            />
          </Control>
        </div>

        {saved.set ? (
          <div className="judge-rows">
            <div className="judge-row">
              <Button disabled={locked} onClick={() => void test()}>
                {busy === 'test' ? 'Testing…' : 'Test'}
              </Button>
              <span
                className={`judge-result tone-${line.tone}`}
                title={view.openai.test ? `${view.openai.test.model} · ${new Date(view.openai.test.atMs).toLocaleString()}` : undefined}
              >
                <Dot tone={line.tone} />
                {line.text}
              </span>
            </div>
            <div className="judge-row">
              <ControlHead
                label={SWITCH_LABEL}
                help={`OpenAI (${view.openai.model}) answers first; Jev answers any question OpenAI fails.`}
                aside={
                  view.openai.fallbacks > 0 ? (
                    <span className="judge-fallbacks">
                      {view.openai.fallbacks} answered by Jev
                      <Hint label="Jev fallbacks" text="Judgements Jev answered because OpenAI failed." />
                    </span>
                  ) : undefined
                }
              />
              <span className="judge-toggle" title={switchBlocked}>
                <Toggle checked={openai} label={SWITCH_LABEL} tone="ok" disabled={locked || switchBlocked !== undefined} onClick={() => void flip()} />
              </span>
            </div>
          </div>
        ) : null}

        <p role="status" className={`judge-message${notice ? ` tone-${notice.tone}` : ''}`}>
          {notice?.text}
        </p>
      </div>
    </section>
  )
}
