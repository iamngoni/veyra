/**
 * Notification settings: which events are sent, where they go, and what was
 * delivered recently.
 *
 * Every change is an authenticated write to the service, which validates it
 * and stores channel secrets encrypted; the console never reads a secret back
 * and never keeps the operator token beyond this view's lifetime. A test
 * message uses the channel's *saved* settings, so testing waits until a
 * channel's edits are saved.
 */

import { useEffect, useId, useRef, useState } from 'react'
import type { ChangeEvent, ReactNode, RefObject } from 'react'

import {
  NOTIFICATION_EVENTS,
  NOTIFICATION_PROVIDERS,
  NotificationError,
  testNotification,
  updateNotifications,
  type DeliveryRecord,
  type NotificationEvent,
  type NotificationPatch,
  type NotificationProvider,
  type NotificationProviderId,
  type NotificationSettings,
  type Rejection,
} from '../lib/api'
import { Button, Control, ControlHead, SkeletonRows, TextControl } from './form'
import { Dot, Hint, Icon, Panel, Toggle } from './ui'
import '../styles/provider.css'
import '../styles/notifications.css'

/** Short label and what each event means, as the service defines it. */
const EVENTS: Record<NotificationEvent, { label: string; help: string }> = {
  breaker_tripped: { label: 'Breaker tripped', help: 'A daily-loss or peak-drawdown breaker started blocking new entries.' },
  trading_halted: { label: 'Trading halted', help: 'The kill switch engaged or execution was switched off.' },
  broker_link: { label: 'Broker link', help: 'The broker link went stale, or recovered after going stale.' },
  reconciliation_drift: { label: 'Reconciliation drift', help: 'Positions at the broker diverged from what Veyra manages.' },
  order_failed: { label: 'Order failed', help: 'The terminal rejected or failed an order.' },
  model_trouble: { label: 'Model trouble', help: 'The autopilot could not get a decision several times in a row.' },
  service_down: { label: 'Service down', help: 'The external watchdog could not reach a ready service.' },
  trade_opened: { label: 'Trade opened', help: 'A position was opened.' },
  trade_closed: { label: 'Trade closed', help: 'A position was closed.' },
  daily_summary: { label: 'Daily summary', help: 'One account summary a day, at the time beside it.' },
}

type FieldSpec = {
  key: string
  label: string
  help: string
  secret?: boolean
  required?: boolean
  placeholder?: string
  /** A closed set, edited with a menu. */
  options?: ReadonlyArray<{ value: string; label: string }>
  /** What the service uses when the field is empty. */
  fallback?: string
}

type ProviderSpec = { id: NotificationProviderId; name: string; fields: FieldSpec[]; guide: ReactNode[] }

const PROVIDERS: Record<NotificationProviderId, ProviderSpec> = {
  email: {
    id: 'email',
    name: 'Email',
    fields: [
      { key: 'host', label: 'SMTP host', help: "Your mail provider's SMTP server.", required: true, placeholder: 'smtp.gmail.com' },
      { key: 'port', label: 'Port', help: 'Usually 587 with STARTTLS or 465 with TLS; empty means 587.', placeholder: '587', fallback: '587' },
      {
        key: 'security',
        label: 'Security',
        help: 'STARTTLS upgrades the connection (port 587); TLS encrypts from the start (port 465).',
        fallback: 'starttls',
        options: [
          { value: 'starttls', label: 'STARTTLS' },
          { value: 'tls', label: 'TLS' },
        ],
      },
      { key: 'username', label: 'Username', help: 'SMTP login, usually your address. Set together with the password.', placeholder: 'Not set' },
      { key: 'password', label: 'Password', help: 'SMTP password or app password. Set together with the username.', secret: true },
      { key: 'from', label: 'From', help: 'Sender address.', required: true, placeholder: 'you@example.com' },
      { key: 'to', label: 'To', help: 'Recipient addresses, comma-separated.', required: true, placeholder: 'a@example.com, b@example.com' },
    ],
    guide: [
      'Any SMTP server works; use the host, port and login your mail provider gives you.',
      <>
        Gmail: turn on 2-Step Verification, then create an app password at{' '}
        <a href="https://myaccount.google.com/apppasswords" target="_blank" rel="noopener noreferrer">
          myaccount.google.com/apppasswords
        </a>
        .
      </>,
      'Enter host smtp.gmail.com, port 587, security STARTTLS, your Gmail address as username and From, and the app password as password.',
      'Port 465 needs security TLS.',
    ],
  },
  telegram: {
    id: 'telegram',
    name: 'Telegram',
    fields: [
      {
        key: 'chatId',
        label: 'Chat ID',
        help: 'A user or group id (groups and channels are negative), or @name for a public channel.',
        required: true,
        placeholder: '-1001234567890',
      },
      { key: 'botToken', label: 'Bot token', help: 'The token @BotFather gave you for the bot.', secret: true, required: true },
    ],
    guide: [
      'In Telegram, open @BotFather, send /newbot and copy the bot token it gives you.',
      'Send your bot any message, or add it to a group or channel.',
      <>
        Open <code>https://api.telegram.org/bot&lt;TOKEN&gt;/getUpdates</code> and copy <code>message.chat.id</code>.
        Groups and channels are negative, e.g. -100….
      </>,
      'For a public channel where the bot is an admin, @channel_name works as the chat ID.',
    ],
  },
  discord: {
    id: 'discord',
    name: 'Discord',
    fields: [
      {
        key: 'webhookUrl',
        label: 'Webhook URL',
        help: 'A channel webhook: https://discord.com/api/webhooks/…',
        secret: true,
        required: true,
      },
    ],
    guide: [
      'Open Server Settings → Integrations → Webhooks.',
      'Choose New Webhook and pick the channel.',
      'Copy Webhook URL and paste it here.',
    ],
  },
  slack: {
    id: 'slack',
    name: 'Slack',
    fields: [
      {
        key: 'webhookUrl',
        label: 'Webhook URL',
        help: 'An incoming webhook: https://hooks.slack.com/services/…',
        secret: true,
        required: true,
      },
    ],
    guide: [
      <>
        At{' '}
        <a href="https://api.slack.com/apps" target="_blank" rel="noopener noreferrer">
          api.slack.com/apps
        </a>
        , choose Create New App → From scratch.
      </>,
      'Open Incoming Webhooks and turn them on.',
      'Choose Add New Webhook to Workspace and pick the channel.',
      'Copy the webhook URL and paste it here.',
    ],
  },
  ntfy: {
    id: 'ntfy',
    name: 'ntfy',
    fields: [
      { key: 'server', label: 'Server', help: 'Empty means https://ntfy.sh; set it for a self-hosted server.', placeholder: 'https://ntfy.sh', fallback: 'https://ntfy.sh' },
      {
        key: 'topic',
        label: 'Topic',
        help: 'Letters, digits, - and _. Anyone who knows it can read it, so make it hard to guess.',
        secret: true,
        required: true,
      },
      { key: 'accessToken', label: 'Access token', help: 'Only for protected topics.', secret: true },
    ],
    guide: [
      'Install the ntfy app (iOS or Android), or open ntfy.sh in a browser.',
      'Pick a hard-to-guess topic name; anyone who knows it can read it.',
      'Subscribe to the topic in the app, then paste the topic here.',
      'Self-hosted: set Server. Add an access token only for a protected topic.',
    ],
  },
  pushover: {
    id: 'pushover',
    name: 'Pushover',
    fields: [
      { key: 'appToken', label: 'API token', help: 'From an application you create on pushover.net.', secret: true, required: true },
      { key: 'userKey', label: 'User key', help: 'Shown on your pushover.net dashboard.', secret: true, required: true },
    ],
    guide: [
      <>
        Create an account at{' '}
        <a href="https://pushover.net" target="_blank" rel="noopener noreferrer">
          pushover.net
        </a>{' '}
        and install the app (paid after a 30-day trial).
      </>,
      'Copy your User Key from the dashboard.',
      'Create an Application and copy its API token.',
    ],
  },
  webhook: {
    id: 'webhook',
    name: 'Webhook',
    fields: [
      { key: 'url', label: 'URL', help: 'An HTTPS endpoint that receives a JSON POST per notification.', secret: true, required: true },
      { key: 'bearerToken', label: 'Bearer token', help: 'Optional; sent as Authorization: Bearer.', secret: true },
    ],
    guide: [
      'Any HTTPS endpoint that accepts a JSON POST.',
      <>
        Each notification sends{' '}
        <code>{'{service: "veyra", event, severity, title, body, at}'}</code>, with <code>at</code> in unix seconds.
      </>,
      'A bearer token, if set, is sent as Authorization: Bearer <token>.',
    ],
  },
}

/** Whether a channel has every value it needs to be switched on or tested. */
function configured(spec: ProviderSpec, provider: NotificationProvider): boolean {
  return spec.fields
    .filter((field) => field.required)
    .every((field) => (field.secret ? provider.secrets[field.key]?.set : Boolean(provider.fields[field.key]?.trim())))
}

const pad = (value: number) => String(value).padStart(2, '0')

/**
 * The operator's local clock time for a whole UTC hour on `on`'s date, e.g.
 * `20:00` for 18 UTC at UTC+2 or `23:30` at UTC+5:30. Daylight saving follows
 * that date.
 */
export function localTimeOfUtcHour(utcHour: number, on: Date = new Date()): string {
  const at = new Date(Date.UTC(on.getUTCFullYear(), on.getUTCMonth(), on.getUTCDate(), utcHour))
  return `${pad(at.getHours())}:${pad(at.getMinutes())}`
}

/** Every UTC hour, labelled and ordered by the operator's local time. */
export function summaryHourOptions(on: Date = new Date()): Array<{ utc: number; label: string }> {
  return Array.from({ length: 24 }, (_, utc) => ({ utc, label: localTimeOfUtcHour(utc, on) })).sort((a, b) =>
    a.label.localeCompare(b.label),
  )
}

/** A delivery's time: the clock time today, with the date before today. */
function deliveryTime(atMs: number, now: Date = new Date()): string {
  const at = new Date(atMs)
  const time = `${pad(at.getHours())}:${pad(at.getMinutes())}`
  return at.toDateString() === now.toDateString()
    ? time
    : `${at.toLocaleDateString(undefined, { day: 'numeric', month: 'short' })} ${time}`
}

const errorText = (error: unknown, fallback: string) => (error instanceof Error ? error.message : fallback)

/** Saved credential: shown by its last characters, replaced or cleared, never read. */
function SecretControl({
  spec,
  saved,
  draft,
  disabled,
  rejected,
  onDraft,
}: {
  spec: FieldSpec
  saved: { set: boolean; hint: string | null } | undefined
  /** Undefined keeps the saved value, null clears it, a string replaces it. */
  draft: string | null | undefined
  disabled: boolean
  rejected?: string
  onDraft: (value: string | null | undefined) => void
}) {
  const id = useId()
  const aside = rejected ? <span className="tone-bad">{rejected}</span> : undefined
  if (saved?.set && draft === undefined) {
    return (
      <div className="tab-control">
        <ControlHead label={spec.label} help={spec.help} aside={aside} />
        <div className="notify-secret">
          <span className="notify-secret-state">Saved ····{saved.hint ?? ''}</span>
          <button type="button" className="tab-link" disabled={disabled} aria-label={`Replace ${spec.label}`} onClick={() => onDraft('')}>
            Replace
          </button>
          <button type="button" className="tab-link" disabled={disabled} aria-label={`Clear ${spec.label}`} onClick={() => onDraft(null)}>
            Clear
          </button>
        </div>
      </div>
    )
  }
  if (draft === null) {
    return (
      <div className="tab-control">
        <ControlHead label={spec.label} help={spec.help} aside={aside} />
        <div className="notify-secret">
          <span className="notify-secret-state tone-warn">Cleared on save</span>
          <button type="button" className="tab-link" disabled={disabled} aria-label={`Keep ${spec.label}`} onClick={() => onDraft(undefined)}>
            Undo
          </button>
        </div>
      </div>
    )
  }
  return (
    <Control
      id={id}
      label={spec.label}
      help={spec.help}
      aside={
        aside ??
        (saved?.set ? (
          <button type="button" className="tab-link" disabled={disabled} onClick={() => onDraft(undefined)}>
            Keep saved
          </button>
        ) : undefined)
      }
    >
      <input
        id={id}
        type="password"
        className={`tab-input${draft ? ' is-dirty' : ''}`}
        value={draft ?? ''}
        placeholder={saved?.set ? 'New value' : spec.required ? 'Required' : 'Not set'}
        disabled={disabled}
        autoComplete="off"
        spellCheck={false}
        aria-invalid={rejected ? true : undefined}
        onChange={(event) => onDraft(event.target.value)}
      />
    </Control>
  )
}

/** One channel's editor: its fields, Save, a test message and the setup steps. */
function ProviderEditor({
  spec,
  provider,
  disabled,
  requireToken,
  onSave,
  onTest,
}: {
  spec: ProviderSpec
  provider: NotificationProvider
  disabled: boolean
  /** Returns the operator token, or undefined after asking for it. */
  requireToken: () => string | undefined
  onSave: (token: string, patch: NotificationPatch) => Promise<void>
  onTest: (token: string) => Promise<void>
}) {
  const [fields, setFields] = useState<Record<string, string>>({})
  const [secrets, setSecrets] = useState<Record<string, string | null>>({})
  const [rejected, setRejected] = useState<Record<string, string>>({})
  const [busy, setBusy] = useState<'save' | 'test'>()
  const [message, setMessage] = useState<{ text: string; tone: 'ok' | 'bad' }>()

  const saved = (key: string) => provider.fields[key] ?? ''
  const changedFields = Object.keys(fields).filter((key) => fields[key].trim() !== saved(key))
  const changedSecrets = Object.keys(secrets).filter((key) => secrets[key] === null || secrets[key]?.trim())
  const dirty = changedFields.length + changedSecrets.length > 0
  const locked = disabled || busy !== undefined

  const edit = (event: ChangeEvent<HTMLInputElement | HTMLSelectElement>) => {
    const key = event.target.dataset.field as string
    const value = event.target.value
    setFields((current) => ({ ...current, [key]: value }))
  }
  const draftSecret = (key: string, value: string | null | undefined) =>
    setSecrets((current) => {
      const next = { ...current }
      if (value === undefined) delete next[key]
      else next[key] = value
      return next
    })

  const save = async () => {
    const token = requireToken()
    if (!token || !dirty) return
    const patch: NotificationPatch = { providers: { [spec.id]: {} } }
    const entry = patch.providers![spec.id]!
    if (changedFields.length) {
      entry.fields = Object.fromEntries(changedFields.map((key) => [key, fields[key].trim() || null]))
    }
    if (changedSecrets.length) {
      entry.secrets = Object.fromEntries(changedSecrets.map((key) => [key, secrets[key]?.trim() || null]))
    }
    setBusy('save')
    setMessage(undefined)
    setRejected({})
    try {
      await onSave(token, patch)
      setFields({})
      setSecrets({})
      setMessage({ text: 'Saved.', tone: 'ok' })
    } catch (error) {
      const prefix = `providers.${spec.id}.`
      const mine = (error instanceof NotificationError ? error.rejected : []).filter((edit: Rejection) =>
        edit.field.startsWith(prefix),
      )
      const labelled = mine.map((edit) => {
        const key = edit.field.slice(prefix.length)
        const label = spec.fields.find((field) => field.key === key)?.label ?? key
        return { key, label, reason: edit.reason.replaceAll('_', ' ') }
      })
      setRejected(Object.fromEntries(labelled.map((edit) => [edit.key, edit.reason])))
      setMessage({
        text: labelled.length
          ? `Not saved. ${labelled.map((edit) => `${edit.label}: ${edit.reason}`).join('; ')}.`
          : `Not saved. ${errorText(error, 'The service refused the change.')}`,
        tone: 'bad',
      })
    } finally {
      setBusy(undefined)
    }
  }

  const test = async () => {
    const token = requireToken()
    if (!token) return
    setBusy('test')
    setMessage(undefined)
    try {
      await onTest(token)
      setMessage({ text: `Test sent. Check ${spec.name}.`, tone: 'ok' })
    } catch (error) {
      setMessage({ text: `Test failed. ${errorText(error, 'No reason given.')}`, tone: 'bad' })
    } finally {
      setBusy(undefined)
    }
  }

  const ready = configured(spec, provider)
  const testBlocked = dirty ? 'Save first; a test uses the saved settings.' : ready ? undefined : 'Save the required values first.'

  return (
    <div className="notify-editor" id={`notify-${spec.id}`}>
      <div className="tab-group-fields notify-fields">
        {spec.fields.map((field) => {
          const aside = rejected[field.key] ? <span className="tone-bad">{rejected[field.key]}</span> : undefined
          if (field.secret) {
            return (
              <SecretControl
                key={field.key}
                spec={field}
                saved={provider.secrets[field.key]}
                draft={secrets[field.key]}
                disabled={locked}
                rejected={rejected[field.key]}
                onDraft={(value) => draftSecret(field.key, value)}
              />
            )
          }
          const value = fields[field.key] ?? saved(field.key)
          const isDirty = field.key in fields && fields[field.key].trim() !== saved(field.key)
          if (field.options) {
            return (
              <ChoiceField
                key={field.key}
                spec={field}
                value={value || (field.fallback as string)}
                dirty={isDirty}
                disabled={locked}
                aside={aside}
                onChange={edit}
              />
            )
          }
          return (
            <TextControl
              key={field.key}
              label={field.label}
              help={field.help}
              field={field.key}
              value={value}
              placeholder={field.placeholder}
              disabled={locked}
              dirty={isDirty}
              aside={aside}
              onChange={edit}
            />
          )
        })}
      </div>
      <div className="notify-editor-foot">
        <span className="tab-settings-actions">
          <Button tone="ok" disabled={locked || !dirty} onClick={() => void save()}>
            {busy === 'save' ? 'Saving…' : 'Save'}
          </Button>
          {dirty ? (
            <Button
              disabled={locked}
              onClick={() => {
                setFields({})
                setSecrets({})
                setRejected({})
              }}
            >
              Discard
            </Button>
          ) : null}
          <span title={testBlocked}>
            <Button disabled={locked || testBlocked !== undefined} onClick={() => void test()}>
              {busy === 'test' ? 'Sending…' : 'Send test'}
            </Button>
          </span>
        </span>
        <p role="status" className={`notify-message${message ? ` tone-${message.tone}` : ''}`}>
          {message?.text}
        </p>
      </div>
      <details className="tab-raw notify-guide">
        <summary>How to set up</summary>
        <ol>
          {spec.guide.map((step, index) => (
            <li key={index}>{step}</li>
          ))}
        </ol>
      </details>
    </div>
  )
}

/** A closed set of values for one channel field. */
function ChoiceField({
  spec,
  value,
  dirty,
  disabled,
  aside,
  onChange,
}: {
  spec: FieldSpec
  value: string
  dirty: boolean
  disabled: boolean
  aside?: ReactNode
  onChange: (event: ChangeEvent<HTMLSelectElement>) => void
}) {
  const id = useId()
  const options = spec.options as ReadonlyArray<{ value: string; label: string }>
  const choices = options.some((option) => option.value === value) ? options : [...options, { value, label: value }]
  return (
    <Control id={id} label={spec.label} help={spec.help} aside={aside}>
      <span className="tab-select">
        <select
          id={id}
          className={`tab-input${dirty ? ' is-dirty' : ''}`}
          data-field={spec.key}
          value={value}
          disabled={disabled}
          onChange={onChange}
        >
          {choices.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </select>
        <Icon name="chevron-down" size={16} />
      </span>
    </Control>
  )
}

/** The operator token, shared by every change in this section and kept only in memory. */
function TokenField({ value, inputRef, onChange }: { value: string; inputRef: RefObject<HTMLInputElement | null>; onChange: (value: string) => void }) {
  const id = useId()
  return (
    <div className="notify-token">
      <label htmlFor={id}>Operator token</label>
      <Hint label="Operator token" text="Required to change notifications or send a test. Kept only until you leave Settings." />
      <input
        id={id}
        ref={inputRef}
        type="password"
        className="tab-input"
        autoComplete="off"
        spellCheck={false}
        value={value}
        onChange={(event) => onChange(event.target.value)}
      />
    </div>
  )
}

function DeliveryRow({ record, now }: { record: DeliveryRecord; now: Date }) {
  const name = PROVIDERS[record.provider as NotificationProviderId]?.name ?? record.provider
  return (
    <li className="tab-row notify-delivery">
      <span className="tab-time">{deliveryTime(record.atMs, now)}</span>
      <span className="notify-delivery-provider">{name}</span>
      <span className="notify-delivery-title" title={record.title}>
        {record.title}
      </span>
      <span className={`notify-delivery-state ${record.ok ? 'tone-ok' : 'tone-bad'}`}>
        <Dot tone={record.ok ? 'ok' : 'bad'} />
        {record.ok ? 'Sent' : 'Failed'}
        {!record.ok && record.detail ? (
          <Hint label={`${name} failure`} text={`${record.detail} (${record.attempts} ${record.attempts === 1 ? 'attempt' : 'attempts'})`} />
        ) : null}
      </span>
    </li>
  )
}

/** How many recent deliveries are listed. */
const RECENT_SHOWN = 10

/**
 * Notification settings, applied as they are changed.
 *
 * Event switches and a channel's on/off switch save at once; a channel's
 * fields save with its own Save. `settings` is the latest poll, replaced by a
 * save's response until the next poll arrives.
 */
export function NotificationsPanel({
  settings,
  error,
  onRefresh,
}: {
  settings?: NotificationSettings
  error?: string
  onRefresh?: () => void
}) {
  const [fresh, setFresh] = useState<NotificationSettings>()
  const [token, setToken] = useState('')
  const [open, setOpen] = useState<NotificationProviderId>()
  const [busy, setBusy] = useState(false)
  const [notice, setNotice] = useState<{ scope: 'events' | 'channels'; text: string; tone: 'ok' | 'bad' | 'warn' }>()
  const tokenInput = useRef<HTMLInputElement>(null)

  // A newer poll supersedes the copy a save returned.
  useEffect(() => setFresh(undefined), [settings])

  const view = fresh ?? settings
  if (!view) {
    return (
      <Panel title="Notifications" className="tab-panel tab-notify">
        {error ? <p className="tab-error" role="alert">Notifications unavailable: {error}</p> : <SkeletonRows />}
      </Panel>
    )
  }

  const available = view.available
  const now = new Date()

  const requireToken = (scope: 'events' | 'channels') => () => {
    if (token.trim()) return token
    setNotice({ scope, text: 'Enter the operator token first.', tone: 'warn' })
    tokenInput.current?.focus()
    return undefined
  }

  const persist = async (secret: string, patch: NotificationPatch) => {
    setFresh(await updateNotifications(secret, patch))
    onRefresh?.()
  }

  /** Saves a patch from a switch or menu, reporting under `scope`. */
  const apply = async (scope: 'events' | 'channels', patch: NotificationPatch, done: string) => {
    const secret = requireToken(scope)()
    if (!secret) return
    setBusy(true)
    setNotice(undefined)
    try {
      await persist(secret, patch)
      setNotice({ scope, text: done, tone: 'ok' })
    } catch (cause) {
      setNotice({ scope, text: `Not saved. ${errorText(cause, 'The service refused the change.')}`, tone: 'bad' })
    } finally {
      setBusy(false)
    }
  }

  const locked = !available || busy
  const summary = view.summaryHourUtc
  const hourOptions = summaryHourOptions(now)
  const noticeFor = (scope: 'events' | 'channels') => (
    <p role="status" className={`notify-message${notice?.scope === scope ? ` tone-${notice.tone}` : ''}`}>
      {notice?.scope === scope ? notice.text : null}
    </p>
  )

  return (
    <Panel
      title="Notifications"
      className="tab-panel tab-notify"
      actions={available ? <TokenField value={token} inputRef={tokenInput} onChange={setToken} /> : undefined}
    >
      {available ? null : (
        <p className="tab-group-note notify-unavailable">
          Notifications need encrypted credential storage and a database on the service.
        </p>
      )}

      <section className="tab-group" aria-labelledby="notify-events">
        <div className="tab-group-head">
          <h3 id="notify-events">Events</h3>
          <span className="tab-group-note">Sent to every channel that is on</span>
        </div>
        <div className="notify-body">
          <ul className="notify-events">
            {NOTIFICATION_EVENTS.map((event) => {
              const { label, help } = EVENTS[event]
              const on = view.events[event]
              return (
                <li key={event} className="notify-event">
                  <span className="tab-control-label">
                    <span className="tab-control-name">{label}</span>
                    <Hint
                      label={label}
                      text={event === 'daily_summary' ? `${help} Your local time; ${pad(summary)}:00 UTC.` : help}
                    />
                  </span>
                  {event === 'daily_summary' ? (
                    <span className="tab-select notify-hour">
                      <select
                        className="tab-input"
                        aria-label="Daily summary time"
                        value={summary}
                        disabled={locked}
                        onChange={(change) => {
                          const hour = Number(change.target.value)
                          void apply('events', { summaryHourUtc: hour }, `Daily summary at ${localTimeOfUtcHour(hour, now)}.`)
                        }}
                      >
                        {hourOptions.map((option) => (
                          <option key={option.utc} value={option.utc}>
                            {option.label}
                          </option>
                        ))}
                      </select>
                      <Icon name="chevron-down" size={16} />
                    </span>
                  ) : null}
                  <Toggle
                    checked={on}
                    label={label}
                    tone="ok"
                    disabled={locked}
                    onClick={() => void apply('events', { events: { [event]: !on } }, `${label} ${on ? 'off' : 'on'}.`)}
                  />
                </li>
              )
            })}
          </ul>
          {noticeFor('events')}
        </div>
      </section>

      <section className="tab-group" aria-labelledby="notify-channels">
        <div className="tab-group-head">
          <h3 id="notify-channels">Channels</h3>
          <span className="tab-group-note">Where notifications go</span>
        </div>
        <div className="notify-body">
          <ul className="tab-list notify-channels">
            {NOTIFICATION_PROVIDERS.map((id) => {
              const spec = PROVIDERS[id]
              const provider = view.providers[id]
              const ready = configured(spec, provider)
              const expanded = open === id
              return (
                <li key={id} className="tab-row notify-channel">
                  <div className="notify-channel-row">
                    <strong>{spec.name}</strong>
                    <span className={`tab-subscription-state${provider.enabled ? ' is-connected' : ''}`}>
                      <Dot tone={provider.enabled ? 'ok' : 'idle'} />
                      {provider.enabled ? 'On' : ready ? 'Off' : 'Not set up'}
                    </span>
                    <button
                      type="button"
                      className="tab-link"
                      aria-expanded={expanded}
                      aria-controls={expanded ? `notify-${id}` : undefined}
                      aria-label={`${expanded ? 'Close' : ready ? 'Edit' : 'Set up'} ${spec.name}`}
                      onClick={() => setOpen(expanded ? undefined : id)}
                    >
                      {expanded ? 'Close' : ready ? 'Edit' : 'Set up'}
                    </button>
                    <Toggle
                      checked={provider.enabled}
                      label={spec.name}
                      tone="ok"
                      disabled={locked || (!provider.enabled && !ready)}
                      onClick={() =>
                        void apply(
                          'channels',
                          { providers: { [id]: { enabled: !provider.enabled } } },
                          `${spec.name} ${provider.enabled ? 'off' : 'on'}.`,
                        )
                      }
                    />
                  </div>
                  {expanded ? (
                    <ProviderEditor
                      spec={spec}
                      provider={provider}
                      disabled={!available}
                      requireToken={requireToken('channels')}
                      onSave={persist}
                      onTest={async (secret) => {
                        await testNotification(secret, id)
                        onRefresh?.()
                      }}
                    />
                  ) : null}
                </li>
              )
            })}
          </ul>
          {noticeFor('channels')}
        </div>
      </section>

      <section className="tab-group" aria-labelledby="notify-deliveries">
        <div className="tab-group-head">
          <h3 id="notify-deliveries">Recent deliveries</h3>
          <span className="tab-group-note notify-counters">
            {view.status.delivered} sent · {view.status.failed} failed · {view.status.dropped} dropped
            <Hint
              label="Delivery counts"
              text="Since the service started. Dropped notifications were discarded because the queue was full."
            />
          </span>
        </div>
        <div className="notify-body">
          {view.recent.length ? (
            <ol className="tab-list notify-deliveries">
              {view.recent.slice(0, RECENT_SHOWN).map((record, index) => (
                <DeliveryRow key={`${record.atMs}-${record.provider}-${index}`} record={record} now={now} />
              ))}
            </ol>
          ) : (
            <p className="tab-group-note">Nothing sent yet.</p>
          )}
        </div>
      </section>
    </Panel>
  )
}
