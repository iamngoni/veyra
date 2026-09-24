/**
 * Live settings: every setting the service lets an operator change without
 * editing its environment, grouped the way an operator thinks about them.
 */

import { useId, useState } from 'react'
import type { ChangeEvent, ReactNode } from 'react'

import { changeModelCredential, type LiveSetting, type RuntimeConfigPatch, type SecretStatus } from '../lib/api'
import { Button, Control, ControlHead, SkeletonRows, TextControl } from './form'
import { SubscriptionConnections } from './subscriptions'
import { Icon, Panel, Toggle } from './ui'
import '../styles/provider.css'

/**
 * Settings grouped the way an operator thinks about them, not the way the
 * environment file happens to be ordered. `prefixes` are dropped from labels
 * inside the group, so "Autopilot" does not repeat on every field under it.
 */
const SETTING_GROUPS: Array<{ title: string; prefixes: string[]; names: string[] }> = [
  {
    title: 'Execution',
    prefixes: [],
    names: ['VEYRA_TRADING_ENABLED'],
  },
  {
    title: 'Autopilot',
    prefixes: ['AUTOPILOT_'],
    names: [
      'VEYRA_AUTOPILOT_ENABLED',
      'VEYRA_AUTOPILOT_SYMBOL',
      'VEYRA_AUTOPILOT_SYMBOLS',
      'VEYRA_AUTOPILOT_TIMEFRAME',
      'VEYRA_AUTOPILOT_BARS',
      'VEYRA_AUTOPILOT_TIER',
      'VEYRA_AUTOPILOT_INTERVAL_SECS',
      'VEYRA_AUTOPILOT_JEV',
      'VEYRA_AUTOPILOT_MIN_HOLD_SECS',
      'VEYRA_AUTOPILOT_ENTRY_MOVE_ATR',
      'VEYRA_AUTOPILOT_BREAKEVEN_R',
      'VEYRA_AUTOPILOT_TRAIL_R',
    ],
  },
  {
    title: 'Profit harvesting',
    prefixes: ['AUTOPILOT_HARVEST_', 'AUTOPILOT_'],
    names: [
      'VEYRA_AUTOPILOT_PROFIT_HARVEST',
      'VEYRA_AUTOPILOT_HARVEST_ARM_R',
      'VEYRA_AUTOPILOT_HARVEST_TRAIL_R',
      'VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT',
      'VEYRA_AUTOPILOT_HARVEST_GIVEBACK',
      'VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS',
      'VEYRA_AUTOPILOT_HARVEST_REENTRY_COOLDOWN_SECS',
    ],
  },
  {
    title: 'Model',
    prefixes: ['MODEL_'],
    names: [
      'VEYRA_MODEL_PROVIDER',
      'VEYRA_MODEL_FAST',
      'VEYRA_MODEL_BALANCED',
      'VEYRA_MODEL_REASONING',
      'VEYRA_MODEL_FALLBACKS',
      'VEYRA_MODEL_FAST_FALLBACKS',
      'VEYRA_MODEL_BALANCED_FALLBACKS',
      'VEYRA_MODEL_REASONING_FALLBACKS',
      'VEYRA_MODEL_MAX_CALLS_PER_HOUR',
      'VEYRA_MODEL_MAX_CALLS_PER_DAY',
      'VEYRA_MODEL_COMPEL_STRUCTURED',
      'VEYRA_MODEL_BASE_URL',
      'VEYRA_MODEL_HTTP_REFERER',
      'VEYRA_MODEL_APP_TITLE',
      'VEYRA_MODEL_APP_HIDDEN',
    ],
  },
  {
    title: 'Judgement and market data',
    prefixes: [],
    names: [
      'VEYRA_JEV_PROVIDER',
      'VEYRA_JEV_BASE_URL',
      'VEYRA_JEV_MODEL',
      'VEYRA_MARKET_PROVIDER',
      'VEYRA_MARKET_EA_AWAIT_SECS',
    ],
  },
  {
    title: 'Housekeeping',
    prefixes: [],
    names: ['VEYRA_RECONCILE_SECS', 'VEYRA_AUDIT_RETENTION_DAYS', 'VEYRA_ALERT_WEBHOOK'],
  },
]

/** Sections the service applies immediately; the rest wait for a restart. */
const LIVE_GROUPS = new Set(['Execution', 'Autopilot', 'Profit harvesting', 'Model'])

/**
 * What each setting controls, shown on its label's info mark.
 *
 * Every sentence restates the service's own parser and `.env.example`: the
 * unit, the accepted range where there is one, and what an empty value falls
 * back to. "Risk units" are multiples of a trade's entry risk, its distance
 * from entry to the original stop.
 */
const SETTING_HELP: Record<string, string> = {
  VEYRA_TRADING_ENABLED:
    'Lets the service send orders to the terminal, closes and stop moves included; off, none are sent. The terminal must also allow live orders.',
  VEYRA_AUTOPILOT_ENABLED:
    'Runs the autonomous loop: each cycle it reviews open positions and may propose one trade, which the risk policy must approve.',
  VEYRA_AUTOPILOT_SYMBOL:
    "The one instrument the autopilot trades; empty uses the terminal's chart symbol. Leave empty when Symbols is set.",
  VEYRA_AUTOPILOT_SYMBOLS:
    'Comma-separated instruments, up to 16, the autopilot chooses from, opening at most one per cycle. Use this or Symbol, not both.',
  VEYRA_AUTOPILOT_TIMEFRAME:
    'Candle size the autopilot reads market data and judgements on. Defaults to H4, four-hour candles.',
  VEYRA_AUTOPILOT_BARS: 'Closed candles the autopilot reads per instrument each cycle, 10–240; empty means 48.',
  VEYRA_AUTOPILOT_TIER:
    'Model tier that proposes and reviews trades: Fast is the cheapest, Reasoning the strongest. Defaults to Balanced.',
  VEYRA_AUTOPILOT_INTERVAL_SECS:
    'Seconds between autopilot cycles, 30–86400; empty means 300. Stop and profit checks run on the same cadence.',
  VEYRA_AUTOPILOT_JEV:
    'Auto asks the Jev judgement service for direction, trend and momentum reads when it is set up; Off never asks.',
  VEYRA_AUTOPILOT_MIN_HOLD_SECS:
    'Minimum age, in seconds, before the autopilot may close a position; empty means 300, 0 turns the guard off.',
  VEYRA_AUTOPILOT_ENTRY_MOVE_ATR:
    'Mid-candle move, as a fraction of the average range (ATR), that prompts a fresh entry check; empty means 0.25, 0 waits for new candles.',
  VEYRA_AUTOPILOT_BREAKEVEN_R:
    'Moves the stop to the entry price once a trade is this many risk units in profit (1 = the stop distance); empty or 0 is off.',
  VEYRA_AUTOPILOT_TRAIL_R:
    'Once a trade is this many risk units in profit, keeps the stop that far behind the best price; empty or 0 is off.',
  VEYRA_AUTOPILOT_PROFIT_HARVEST:
    'Protects profit before take profit: once armed it trails the stop, and closes a trade still in profit that gives back too much of its peak.',
  VEYRA_AUTOPILOT_HARVEST_ARM_R:
    'Move in favour, in risk units (1 = the stop distance), needed before harvesting arms; empty means 0.2.',
  VEYRA_AUTOPILOT_HARVEST_TRAIL_R:
    'How far the stop is kept behind the best price once armed, in risk units, no more than Arm R; empty means 0.2.',
  VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT:
    'Net open profit in account currency, after spread, swap and commission, needed before harvesting arms; empty means 0.50.',
  VEYRA_AUTOPILOT_HARVEST_GIVEBACK:
    'Share of its best profit a trade may give back before it is closed, 0.05–0.95; empty means 0.35.',
  VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS:
    'Minimum age, in seconds, before harvesting may act on a position; empty means 300.',
  VEYRA_AUTOPILOT_HARVEST_REENTRY_COOLDOWN_SECS:
    'Seconds after a close before the same symbol may be entered again; empty means 900. Re-entry also needs fresh price movement.',
  VEYRA_MODEL_FAST:
    'Model for the Fast tier, meant to be the cheapest. Use a model ID from the selected provider; OpenRouter IDs use vendor/model.',
  VEYRA_MODEL_BALANCED: "Model for the Balanced tier, the autopilot's default. Use a model ID from the selected provider.",
  VEYRA_MODEL_REASONING: 'Model for the Reasoning tier, meant to be the strongest. Use a model ID from the selected provider.',
  VEYRA_MODEL_FALLBACKS:
    "Models tried in order when a tier's own model cannot answer (out of credits, rate limited, rejected). Comma-separated, up to 4.",
  VEYRA_MODEL_FAST_FALLBACKS:
    'Fallback models for the Fast tier only, replacing the shared list there; empty uses the shared list.',
  VEYRA_MODEL_BALANCED_FALLBACKS:
    'Fallback models for the Balanced tier only, replacing the shared list there; empty uses the shared list.',
  VEYRA_MODEL_REASONING_FALLBACKS:
    'Fallback models for the Reasoning tier only, replacing the shared list there; empty uses the shared list.',
  VEYRA_MODEL_MAX_CALLS_PER_HOUR:
    'Most model calls allowed per hour; beyond it, calls are refused until the window resets. Empty or 0 means unlimited.',
  VEYRA_MODEL_MAX_CALLS_PER_DAY:
    'Most model calls allowed per day; beyond it, calls are refused until the window resets. Empty or 0 means unlimited.',
  VEYRA_MODEL_COMPEL_STRUCTURED:
    'Requires the model to answer in the structured format instead of merely offering it. Turn off for reasoning models, which refuse it.',
  VEYRA_MODEL_PROVIDER:
    'Choose the model service for decisions and chat. Connect ChatGPT or Claude above first; API providers use developer keys.',
  VEYRA_MODEL_BASE_URL: 'Address of the model API. Custom requires one; named providers use their standard endpoint when empty.',
  VEYRA_MODEL_HTTP_REFERER:
    "Your app's URL, sent to OpenRouter for attribution. App title and App hidden only take effect when it is set.",
  VEYRA_MODEL_APP_TITLE: 'Name shown on OpenRouter beside the attribution URL. Needs HTTP referer to be set.',
  VEYRA_MODEL_APP_HIDDEN:
    "Keeps the attributed app out of OpenRouter's public rankings. OpenRouter locks this on the first request it receives.",
  VEYRA_JEV_PROVIDER: 'Service that answers the judgement questions. TypeSafe is the only one supported.',
  VEYRA_JEV_BASE_URL: 'Address of the judgement API; empty uses the TypeSafe default, https://api.typesafe.ai.',
  VEYRA_JEV_MODEL: 'Judgement model alias sent with every request; empty means jev-latest.',
  VEYRA_MARKET_PROVIDER:
    "Where candles come from: ea reads closed candles from the terminal's EA. Not set turns market data off.",
  VEYRA_MARKET_EA_AWAIT_SECS:
    'Seconds to wait for candles from the terminal before they count as unavailable, 5–120; empty means 20. Keep it above 15.',
  VEYRA_RECONCILE_SECS:
    "Seconds between automatic refreshes of the broker's account state, up to 3600; empty means 30, 0 turns them off.",
  VEYRA_AUDIT_RETENTION_DAYS:
    'Days of audit history to keep, up to 3650; older events are pruned hourly. Empty means 30, 0 keeps everything.',
  VEYRA_ALERT_WEBHOOK:
    'URL that receives stack alerts as a JSON post (Slack, Discord and ntfy work). Empty writes them to the local log instead.',
}

/**
 * How a setting is edited when its accepted values are a closed set.
 *
 * The service's parser stays the only judge of a value; these only choose a
 * control that cannot express anything outside what that parser accepts.
 * `fallback` is what the service does with an empty value, so an unset field
 * shows the behaviour in force rather than a blank. Every other setting is
 * free text.
 */
type SettingKind =
  | { kind: 'switch'; fallback: boolean }
  | { kind: 'choice'; fallback: string; options: ReadonlyArray<{ value: string; label: string }> }
  /** Exactly one supported value: shown, not edited. */
  | { kind: 'fixed'; fallback: string }

const TIMEFRAMES = ['M1', 'M5', 'M15', 'M30', 'H1', 'H4', 'D1', 'W1', 'MN1'].map((value) => ({ value, label: value }))

const SETTING_KINDS: Record<string, SettingKind> = {
  VEYRA_TRADING_ENABLED: { kind: 'switch', fallback: false },
  VEYRA_AUTOPILOT_ENABLED: { kind: 'switch', fallback: false },
  VEYRA_AUTOPILOT_PROFIT_HARVEST: { kind: 'switch', fallback: false },
  VEYRA_MODEL_COMPEL_STRUCTURED: { kind: 'switch', fallback: true },
  VEYRA_MODEL_APP_HIDDEN: { kind: 'switch', fallback: false },
  VEYRA_AUTOPILOT_TIMEFRAME: { kind: 'choice', fallback: 'H4', options: TIMEFRAMES },
  VEYRA_AUTOPILOT_TIER: {
    kind: 'choice',
    fallback: 'balanced',
    options: [
      { value: 'fast', label: 'Fast' },
      { value: 'balanced', label: 'Balanced' },
      { value: 'reasoning', label: 'Reasoning' },
    ],
  },
  VEYRA_AUTOPILOT_JEV: {
    kind: 'choice',
    fallback: 'auto',
    options: [
      { value: 'auto', label: 'Auto' },
      { value: 'off', label: 'Off' },
    ],
  },
  VEYRA_MODEL_PROVIDER: {
    kind: 'choice',
    fallback: 'openrouter',
    options: [
      { value: 'openrouter', label: 'OpenRouter' },
      { value: 'codex', label: 'ChatGPT subscription · Codex' },
      { value: 'claude_code', label: 'Claude subscription · Claude Code' },
      { value: 'openai', label: 'OpenAI API key' },
      { value: 'anthropic', label: 'Anthropic API key' },
      { value: 'groq', label: 'Groq' },
      { value: 'deepseek', label: 'DeepSeek' },
      { value: 'xai', label: 'xAI' },
      { value: 'mistral', label: 'Mistral' },
      { value: 'kimi', label: 'Moonshot / Kimi' },
      { value: 'ollama', label: 'Ollama' },
      { value: 'custom', label: 'Custom OpenAI-compatible' },
    ],
  },
  VEYRA_JEV_PROVIDER: { kind: 'fixed', fallback: 'typesafe' },
  VEYRA_MARKET_PROVIDER: { kind: 'fixed', fallback: '' },
}

/** A value as the service reads it: empty means the setting's fallback. */
function settingValue(name: string, raw: string): string {
  const kind = SETTING_KINDS[name]
  if (!kind || raw.trim() !== '') return raw.trim()
  return String(kind.fallback)
}

const LABEL_WORDS: Record<string, string> = {
  r: 'R',
  atr: 'ATR',
  url: 'URL',
  http: 'HTTP',
  jev: 'JEV',
  ea: 'EA',
}

/**
 * A setting's name as a label: `VEYRA_AUTOPILOT_TRAIL_R` under Autopilot reads
 * `Trail R`, and a trailing `SECS` becomes a unit. What the setting does is
 * the label's help (`SETTING_HELP`), not its raw name.
 */
function settingLabel(name: string, prefixes: readonly string[]): string {
  let key = name.replace(/^VEYRA_/, '')
  const prefix = prefixes.find((candidate) => key.startsWith(candidate) && key.length > candidate.length)
  if (prefix) key = key.slice(prefix.length)
  const words = key
    .toLowerCase()
    .split('_')
    .map((word) => LABEL_WORDS[word] ?? word)
  const seconds = words.at(-1) === 'secs'
  if (seconds) words.pop()
  const label = words.join(' ')
  return `${label.charAt(0).toUpperCase()}${label.slice(1)}${seconds ? ' (s)' : ''}`
}

/** A flag: label above, the shared ON/OFF switch below. */
function SwitchSetting({
  label,
  help,
  checked,
  disabled,
  dirty,
  aside,
  onFlip,
}: {
  label: string
  help?: string
  checked: boolean
  disabled: boolean
  dirty: boolean
  aside?: ReactNode
  onFlip: () => void
}) {
  return (
    <div className="tab-control">
      <ControlHead label={label} help={help} aside={aside} />
      <div className={`tab-setting-switch${dirty ? ' is-dirty' : ''}`}>
        <Toggle checked={checked} label={label} tone="ok" disabled={disabled} onClick={onFlip} />
      </div>
    </div>
  )
}

/** A closed set: a menu that holds only values the service accepts. */
function ChoiceSetting({
  label,
  help,
  field,
  value,
  options,
  disabled,
  dirty,
  aside,
  note,
  onChange,
}: {
  label: string
  help?: string
  field: string
  value: string
  options: ReadonlyArray<{ value: string; label: string }>
  disabled: boolean
  dirty: boolean
  aside?: ReactNode
  note?: string
  onChange: (event: ChangeEvent<HTMLSelectElement>) => void
}) {
  const id = useId()
  // A value the service accepts outside the menu (a timeframe in minutes)
  // is kept as its own entry rather than silently replaced.
  const choices = options.some((option) => option.value === value) ? options : [...options, { value, label: value }]
  return (
    <Control id={id} label={label} help={help} aside={aside}>
      <span className="tab-select">
        <select
          id={id}
          className={`tab-input${dirty ? ' is-dirty' : ''}`}
          data-field={field}
          value={value}
          disabled={disabled}
          aria-describedby={note ? `${id}-note` : undefined}
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
      {note ? <p className="tab-group-note" id={`${id}-note`}>{note}</p> : null}
    </Control>
  )
}

/** A setting with a single supported value: stated, not offered as a choice. */
function FixedSetting({ label, help, value, aside }: { label: string; help?: string; value: string; aside?: ReactNode }) {
  return (
    <div className="tab-control">
      <ControlHead label={label} help={help} aside={aside} />
      <div className="tab-setting-fixed">{value || 'Not set'}</div>
    </div>
  )
}

/** Credentials are submitted separately from the audited live-settings form. */
function ProviderCredentialPanel({
  status,
  enabled,
  providerChanged = false,
  onRefresh,
}: {
  status?: SecretStatus
  enabled: boolean
  providerChanged?: boolean
  onRefresh?: () => void
}) {
  const [key, setKey] = useState('')
  const [token, setToken] = useState('')
  const [busy, setBusy] = useState(false)
  const [message, setMessage] = useState('')
  const save = async (remove: boolean) => {
    if (!token.trim() || (!remove && !key.trim())) return
    setBusy(true)
    setMessage('')
    try {
      const result = await changeModelCredential(token, remove ? undefined : key)
      setKey('')
      setToken('')
      setMessage(remove ? 'Saved key removed.' : result.active ? 'Credential saved and model active.' : result.reason ? `Saved. ${result.reason.replaceAll('_', ' ')}.` : 'Credential saved; model is not configured yet.')
      onRefresh?.()
    } catch (error) {
      setMessage(error instanceof Error ? error.message : 'Credential update failed.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="tab-provider-credential">
      <div className="tab-provider-credential-head">
        <strong>API credential</strong>
        <span>{status === undefined ? 'Credential status unavailable' : status.set ? `${status.source === 'console' ? 'Saved key' : 'Environment key'} ····${status.hint ?? ''}` : 'No key configured'}</span>
      </div>
      <p>API keys use encrypted storage and are never shown again. Ollama needs no key.</p>
      {enabled && !providerChanged ? (
        <div className="tab-provider-credential-fields">
          <label>
            API key
            <input type="password" autoComplete="off" value={key} disabled={busy} onChange={(event) => setKey(event.target.value)} placeholder="Paste a key for the selected provider" />
          </label>
          <label>
            Operator token
            <input type="password" autoComplete="off" value={token} disabled={busy} onChange={(event) => setToken(event.target.value)} placeholder="Required to change saved credentials" />
          </label>
          <div className="tab-provider-credential-actions">
            <Button disabled={busy || !key.trim() || !token.trim()} onClick={() => void save(false)}>Save key</Button>
            {status?.source === 'console' ? <Button disabled={busy || !token.trim()} onClick={() => void save(true)}>Remove saved key</Button> : null}
          </div>
        </div>
      ) : (
        <p className="tab-group-note">{providerChanged ? 'Apply or discard the provider change before editing its API key.' : 'Encrypted key entry requires an updated service with VEYRA_CONSOLE_SECRET_KEY and VEYRA_CONSOLE_ADMIN_TOKEN configured.'}</p>
      )}
      {message ? <p role="status" className="tab-provider-credential-message">{message}</p> : null}
    </div>
  )
}

/**
 * Live settings, editable without a restart.
 *
 * Flags are switches and closed sets are menus (see `SETTING_KINDS`); the rest
 * are text boxes. Either way the service validates an edit with the same
 * parser that validates `.env`, so the console restates no acceptance rule
 * beyond those closed sets. Nothing applies until Apply, so a switch cannot
 * change the live service by itself, and a rejected patch leaves the draft on
 * screen to be corrected.
 */
export function LiveSettingsPanel({
  settings,
  secretStatus,
  secretStore = false,
  onApply,
  onRefresh,
}: {
  settings?: Record<string, LiveSetting>
  secretStatus?: SecretStatus
  secretStore?: boolean
  /** Applies a patch; resolves to an error message or undefined on success. */
  onApply?: (patch: RuntimeConfigPatch) => Promise<string | undefined>
  onRefresh?: () => void
}) {
  const [draft, setDraft] = useState<Record<string, string>>({})
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const [saved, setSaved] = useState(false)

  if (!settings) {
    return (
      <Panel title="Live settings" className="tab-panel">
        <SkeletonRows />
      </Panel>
    )
  }

  // Only rendered names ever reach the draft, and those exist in `settings`.
  const effective = (name: string) => draft[name] ?? settings[name].value
  const isDirty = (name: string) =>
    name in draft && settingValue(name, draft[name]) !== settingValue(name, settings[name].value)
  const dirty = Object.keys(draft).filter(isDirty)

  const submit = async () => {
    if (!onApply || dirty.length === 0) return
    setBusy(true)
    setError(undefined)
    setSaved(false)
    const patch: RuntimeConfigPatch = {}
    for (const name of dirty) patch[name] = draft[name]
    const failure = await onApply(patch)
    setBusy(false)
    if (failure) {
      setError(failure)
      return
    }
    setDraft({})
    setSaved(true)
    onRefresh?.()
  }

  const revert = async (name: string) => {
    if (!onApply) return
    setBusy(true)
    setError(undefined)
    const failure = await onApply({ [name]: null })
    setBusy(false)
    if (failure) {
      setError(failure)
      return
    }
    setDraft((current) => {
      const next = { ...current }
      delete next[name]
      return next
    })
    onRefresh?.()
  }

  const handleChange = (event: ChangeEvent<HTMLInputElement | HTMLSelectElement>) => {
    const name = event.target.dataset.field as string
    const value = event.target.value
    setDraft((current) => ({ ...current, [name]: value }))
  }
  const flip = (name: string) =>
    setDraft((current) => ({ ...current, [name]: settingValue(name, effective(name)) === 'true' ? 'false' : 'true' }))

  return (
    <Panel
      title="Live settings"
      className="tab-panel tab-settings"
      actions={
        dirty.length > 0 ? (
          <span className="tone-warn">{dirty.length} unsaved</span>
        ) : saved ? (
          <span className="tone-ok">Applied</span>
        ) : null
      }
    >
      {SETTING_GROUPS.map((group) => {
        const names = group.names.filter((name) => settings[name] !== undefined)
        if (names.length === 0) return null
        return (
          <section key={group.title} className="tab-group">
            <div className="tab-group-head">
              <h3>{group.title}</h3>
              {LIVE_GROUPS.has(group.title) ? null : <span className="tab-group-note">Applies on restart</span>}
            </div>
            {group.title === 'Model' ? (
              <>
                <SubscriptionConnections enabled={secretStore} selectedProvider={settingValue('VEYRA_MODEL_PROVIDER', settings.VEYRA_MODEL_PROVIDER?.value ?? '')} onRefresh={onRefresh} />
                {settings.VEYRA_MODEL_PROVIDER && ['codex', 'claude_code'].includes(effective('VEYRA_MODEL_PROVIDER')) ? null : (
                  <ProviderCredentialPanel status={secretStatus} enabled={secretStore} providerChanged={isDirty('VEYRA_MODEL_PROVIDER')} onRefresh={onRefresh} />
                )}
              </>
            ) : null}
            <div className="tab-group-fields">
              {names.map((name) => {
                const label = settingLabel(name, group.prefixes)
                const help = SETTING_HELP[name]
                const kind = SETTING_KINDS[name]
                const aside = settings[name].overridden ? (
                  <>
                    <span className="tone-warn">Overridden</span>
                    <button
                      type="button"
                      className="tab-link"
                      onClick={() => void revert(name)}
                      disabled={busy}
                      title="Return to the deployed value"
                    >
                      Revert
                    </button>
                  </>
                ) : undefined
                if (kind?.kind === 'switch') {
                  return (
                    <SwitchSetting
                      key={name}
                      label={label}
                      help={help}
                      checked={settingValue(name, effective(name)) === 'true'}
                      disabled={busy}
                      dirty={isDirty(name)}
                      aside={aside}
                      onFlip={() => flip(name)}
                    />
                  )
                }
                if (kind?.kind === 'choice') {
                  return (
                    <ChoiceSetting
                      key={name}
                      label={label}
                      help={help}
                      field={name}
                      value={settingValue(name, effective(name))}
                      options={kind.options}
                      disabled={busy}
                      dirty={isDirty(name)}
                      aside={aside}
                      note={
                        name === 'VEYRA_MODEL_PROVIDER'
                          ? isDirty(name)
                            ? 'Provider changed. Replace model IDs and clear or replace fallback IDs with ones this provider supports before Apply.'
                            : 'Connected subscriptions use the linked ChatGPT or Claude account. API providers use developer keys.'
                          : undefined
                      }
                      onChange={handleChange}
                    />
                  )
                }
                if (kind?.kind === 'fixed') {
                  return (
                    <FixedSetting key={name} label={label} help={help} value={settingValue(name, effective(name))} aside={aside} />
                  )
                }
                return (
                  <TextControl
                    key={name}
                    label={label}
                    help={help}
                    field={name}
                    value={effective(name)}
                    disabled={busy}
                    dirty={isDirty(name)}
                    placeholder={['VEYRA_MODEL_FAST', 'VEYRA_MODEL_BALANCED', 'VEYRA_MODEL_REASONING'].includes(name) ? 'Model ID from this provider' : 'Not set'}
                    onChange={handleChange}
                    aside={aside}
                  />
                )
              })}
            </div>
          </section>
        )
      })}

      <div className="tab-settings-foot">
        {error ? (
          <p className="tab-error" role="alert">
            {error}
          </p>
        ) : (
          <span />
        )}
        <span className="tab-settings-actions">
          {dirty.length > 0 ? (
            <Button onClick={() => setDraft({})} disabled={busy}>
              Discard
            </Button>
          ) : null}
          <Button tone="ok" onClick={() => void submit()} disabled={busy || dirty.length === 0}>
            {busy ? 'Applying…' : 'Apply'}
          </Button>
        </span>
      </div>
    </Panel>
  )
}
