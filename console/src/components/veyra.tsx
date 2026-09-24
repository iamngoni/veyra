/**
 * The console's working tabs: activity and commands, the risk policy with the
 * account and market session beside it, the durable trace, diagnostics and
 * live settings.
 *
 * Every panel builds on the shared primitives in `ui.tsx` and reads as the
 * overview does: a quiet frame, plain labels, hairline-ruled rows and colour
 * only where it carries state. Styling lives in `styles/tabs.css` under `tab-`
 * class names.
 */

import { useId, useState } from 'react'
import type { ChangeEvent, ReactNode } from 'react'

import type {
  Account,
  AuditPage,
  CommandRecord,
  FeedEvent,
  LiveSetting,
  LogLevel,
  LogRecord,
  MarketSessions,
  Metrics,
  RiskPolicy,
  RiskPolicyPatch,
  RuntimeConfigPatch,
  Status,
  WeekendPositions,
} from '../lib/api'
import { LOG_LEVELS } from '../lib/api'
import {
  activityDetail,
  activityTitle,
  activityTone,
  amount,
  detailRows,
  isRoutine,
  payloadSummary,
  percent,
  signedAmount,
} from '../lib/format'
import { auditTimeMs, clockTime, relativeTime, usePaged } from '../lib/hooks'
import { Dot, Hint, Icon, Panel, Segmented, Skeleton, Toggle, signTone, type Tone } from './ui'

/* ---------- local primitives ---------- */

/** Sentence case for an identifier: `agent_tool_called` → `Agent tool called`. */
function sentence(text: string): string {
  const spaced = text.replaceAll('_', ' ')
  return spaced.charAt(0).toUpperCase() + spaced.slice(1)
}

/** A header state: a dot and a word, coloured by what it names. */
function State({ tone, title, children }: { tone: Tone; title?: string; children: ReactNode }) {
  return (
    <span className={`tab-state is-${tone}`} title={title}>
      <Dot tone={tone} />
      {children}
    </span>
  )
}

/** The terse stand-in for a poll that failed before it ever answered. */
function Unavailable({ error }: { error: string }) {
  return (
    <State tone="bad" title={error}>
      Unavailable
    </State>
  )
}

/**
 * One label/value row. An undefined value has not arrived yet and holds its
 * place with a skeleton; a known absence is passed explicitly as `—`.
 */
function Field({
  label,
  value,
  tone,
  wide = false,
}: {
  label: string
  value: ReactNode
  /** Extra class for the value, e.g. `tone-warn`. */
  tone?: string
  /** Span every column (long values such as a model chain). */
  wide?: boolean
}) {
  return (
    <div className={`tab-field${wide ? ' is-wide' : ''}`}>
      <dt>{label}</dt>
      <dd className={tone}>{value === undefined ? <Skeleton width={72} /> : value}</dd>
    </div>
  )
}

function Fields({ columns = 2, children }: { columns?: 1 | 2 | 3; children: ReactNode }) {
  return <dl className={`tab-fields is-cols-${columns}`}>{children}</dl>
}

function Empty({ children }: { children: ReactNode }) {
  return <div className="panel-empty">{children}</div>
}

/** Placeholder rows while a list's first poll is in flight. */
function SkeletonRows() {
  return (
    <ul className="tab-list" aria-hidden="true">
      {[0, 1, 2].map((row) => (
        <li key={row} className="tab-row tab-skeleton-row">
          <Skeleton width={56} />
          <Skeleton width="60%" />
        </li>
      ))}
    </ul>
  )
}

/** Quiet text button; `ok` marks the one action that commits a change. */
function Button({
  children,
  tone,
  disabled = false,
  onClick,
}: {
  children: ReactNode
  tone?: 'ok'
  disabled?: boolean
  onClick: () => void
}) {
  return (
    <button type="button" className={`tab-button${tone ? ` is-${tone}` : ''}`} disabled={disabled} onClick={onClick}>
      {children}
    </button>
  )
}

/**
 * Page control for the long lists. It states the visible range rather than
 * only the page number, because "showing 26–50 of 312" answers the question an
 * operator actually has when scanning a feed. A list that fits on one page
 * needs no control at all.
 */
export function Pager({
  page,
  pages,
  start,
  count,
  total,
  onPrevious,
  onNext,
}: {
  page: number
  pages: number
  start: number
  count: number
  total: number
  onPrevious: () => void
  onNext: () => void
}) {
  if (pages <= 1) return null
  return (
    <div className="tab-pager">
      <span className="readout">
        {start + 1}–{start + count} of {total}
      </span>
      <span className="tab-pager-controls">
        <button
          type="button"
          className="tab-pager-button is-previous"
          onClick={onPrevious}
          disabled={page === 0}
          aria-label="Previous page"
        >
          <Icon name="chevron-down" size={16} />
        </button>
        <button
          type="button"
          className="tab-pager-button is-next"
          onClick={onNext}
          disabled={page >= pages - 1}
          aria-label="Next page"
        >
          <Icon name="chevron-down" size={16} />
        </button>
      </span>
    </div>
  )
}

type Paged = {
  page: number
  pages: number
  start: number
  total: number
  items: readonly unknown[]
  next: () => void
  previous: () => void
}

function ListPager({ paged }: { paged: Paged }) {
  return (
    <Pager
      page={paged.page}
      pages={paged.pages}
      start={paged.start}
      count={paged.items.length}
      total={paged.total}
      onPrevious={paged.previous}
      onNext={paged.next}
    />
  )
}

/** Pretty JSON for a drill-down; strings are shown whole rather than quoted. */
function Raw({ value }: { value: unknown }) {
  return <pre className="tab-pre">{typeof value === 'string' ? value : JSON.stringify(value, null, 2)}</pre>
}

/** ISO instant for a `<time>` element, or undefined for an unreadable one. */
function isoTime(ms: number): string | undefined {
  return Number.isFinite(ms) ? new Date(ms).toISOString() : undefined
}

/* ---------- account ---------- */

export function AccountPanel({ account, error }: { account?: Account; error?: string }) {
  // Nothing on screen is invented: before the first poll every value is a
  // skeleton, and after a failed one it is a dash.
  const pending = !account && !error
  const show = (present: boolean, text: () => string) => (account && present ? text() : pending ? undefined : '—')
  const positions = account?.positions ?? []
  const open = positions.reduce((sum, position) => sum + (position.profit ?? 0), 0)
  return (
    <Panel
      title="Account"
      className="tab-panel"
      actions={
        account ? (
          <span className={account.fresh ? undefined : 'tone-warn'}>
            Updated {relativeTime(Date.now() - account.ageSecs * 1000)}
          </span>
        ) : error ? (
          <Unavailable error={error} />
        ) : null
      }
    >
      <Fields>
        <Field label="Balance" value={show(account?.balance !== undefined, () => amount(account?.balance))} />
        <Field label="Equity" value={show(account?.equity !== undefined, () => amount(account?.equity))} />
        <Field label="Free margin" value={show(account?.freeMargin !== undefined, () => amount(account?.freeMargin))} />
        {/* Zero means no margin is in use, so there is no level to report. */}
        <Field label="Margin level" value={show(Boolean(account?.marginLevel), () => percent(account?.marginLevel, 1))} />
        <Field label="Leverage" value={show(Boolean(account?.leverage), () => `1:${account?.leverage}`)} />
        <Field label="Open orders" value={show(account?.orders !== undefined, () => String(account?.orders))} />
        <Field label="Open lots" value={show(account?.lots !== undefined, () => String(account?.lots))} />
        <Field
          label="Open P/L"
          value={show(positions.length > 0, () => signedAmount(open))}
          tone={positions.length > 0 ? signTone(open) : undefined}
        />
        <Field label="Server" value={show(Boolean(account?.server), () => String(account?.server))} />
        <Field label="Login" value={show(account?.login != null, () => String(account?.login))} />
      </Fields>
    </Panel>
  )
}

/* ---------- market session ---------- */

/** UTC clock label for an instant, e.g. `Fri 21:00 UTC`. */
function utcClock(unix: number): string {
  const date = new Date(unix * 1000)
  const weekday = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'][date.getUTCDay()]
  const hh = String(date.getUTCHours()).padStart(2, '0')
  const mm = String(date.getUTCMinutes()).padStart(2, '0')
  return `${weekday} ${hh}:${mm} UTC`
}

/** Minute of the UTC day as a clock, e.g. `1245` → `20:45`. */
function utcMinute(minute: number): string {
  const hh = String(Math.floor(minute / 60) % 24).padStart(2, '0')
  const mm = String(minute % 60).padStart(2, '0')
  return `${hh}:${mm}`
}

/** Compact time-until label, e.g. `in 3h 12m`. */
function untilLabel(unix: number, now: number): string {
  const seconds = Math.max(0, unix - now)
  const days = Math.floor(seconds / 86_400)
  const hours = Math.floor((seconds % 86_400) / 3_600)
  const minutes = Math.floor((seconds % 3_600) / 60)
  if (days > 0) return `in ${days}d ${hours}h`
  if (hours > 0) return `in ${hours}h ${minutes}m`
  return `in ${minutes}m`
}

const MARKET_STATE: Record<MarketSessions['market']['state'], { tone: Tone; label: string }> = {
  open: { tone: 'ok', label: 'Open' },
  rollover: { tone: 'warn', label: 'Rollover' },
  closed: { tone: 'idle', label: 'Closed' },
}

const SESSION_EVENT_LABELS: Record<MarketSessions['market']['nextEvent'], string> = {
  opens: 'Opens',
  closes: 'Closes',
  pauses: 'Pauses',
  resumes: 'Resumes',
}

const ENTRY_BLOCK_LABELS: Record<string, string> = {
  rollover_blackout: 'rollover blackout',
  weekend_approach: 'weekend cutoff',
  weekend_open: 'weekend',
  session_closed: 'session window',
}

/** How the weekend preference reads, shared by the policy and the session. */
const WEEKEND_LABELS: Record<WeekendPositions, string> = {
  agent: 'Analyst decides',
  hold: 'Held through',
  flatten: 'Flattened before close',
}

/**
 * Where the trading week stands: the market state, when it next changes,
 * whether new entries are allowed and what is exposed meanwhile.
 */
export function SessionPanel({
  sessions,
  account,
}: {
  sessions?: MarketSessions
  /** Positions are named here so the panel says what is exposed while the market is shut. */
  account?: Account
}) {
  const state = sessions ? MARKET_STATE[sessions.market.state] : undefined
  const held = account ? (account.positions ?? []).map((position) => position.symbol) : undefined
  const checkpoint = sessions?.weekend.closesInSecs ?? null
  const blockedBy = sessions?.entries.blockedBy
  const blockReason = blockedBy ? (ENTRY_BLOCK_LABELS[blockedBy] ?? blockedBy) : undefined
  return (
    <Panel
      title="Market session"
      className="tab-panel"
      actions={state ? <State tone={state.tone}>{state.label}</State> : null}
    >
      <Fields columns={1}>
        <Field
          label={sessions ? SESSION_EVENT_LABELS[sessions.market.nextEvent] : 'Next'}
          value={
            sessions
              ? `${utcClock(sessions.market.nextAt)} · ${untilLabel(sessions.market.nextAt, sessions.now)}`
              : undefined
          }
        />
        <Field
          label="Entries"
          value={
            sessions
              ? sessions.entries.open
                ? 'Open'
                : blockReason
                  ? `Blocked · ${blockReason}`
                  : 'Blocked'
              : undefined
          }
          tone={sessions ? (sessions.entries.open ? 'tone-ok' : 'tone-warn') : undefined}
        />
        <Field label="Holding" value={held ? (held.length > 0 ? held.join(' · ') : 'None') : undefined} />
        <Field
          label="Rollover"
          value={
            sessions
              ? `${utcMinute(sessions.policy.rolloverBlackout.startMinute)}–${utcMinute(sessions.policy.rolloverBlackout.endMinute)} UTC`
              : undefined
          }
        />
        <Field
          label="Entry window"
          value={
            sessions
              ? `Sun ${utcMinute(sessions.policy.sundayEntryOpenMinute)} – Fri ${utcMinute(sessions.policy.fridayEntryCutoffMinute)} UTC`
              : undefined
          }
        />
        {sessions && checkpoint !== null ? (
          <>
            <Field
              label="Weekend close"
              value={`${utcClock(sessions.now + checkpoint)} · ${untilLabel(sessions.now + checkpoint, sessions.now)}`}
              tone="tone-warn"
            />
            <Field label="Weekend positions" value={WEEKEND_LABELS[sessions.weekend.policy]} />
          </>
        ) : null}
      </Fields>
    </Panel>
  )
}

/* ---------- autopilot ---------- */

/** Compact token count: 1234 → 1.2k, 2164642 → 2.2M. */
function compactTokens(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`
  if (value >= 1000) return `${(value / 1000).toFixed(1)}k`
  return String(value)
}

/** A call cap, where zero means unbounded. */
function cap(limit: number): string {
  return limit ? amount(limit, 0) : '∞'
}

export function AutopilotPanel({
  status,
  budget,
  jevUsage,
  decisions,
}: {
  status?: Status['autopilot']
  budget?: Status['model_budget']
  jevUsage?: Status['jev_usage']
  decisions?: Status['decisions']
}) {
  const on = status?.enabled === true
  // A null autopilot is a known answer (none configured); undefined is a poll
  // that has not landed yet.
  const known = status !== undefined
  const stops = status
    ? sentence(
        [status.breakeven_r > 0 ? `break-even ${status.breakeven_r}R` : null, status.trail_r > 0 ? `trail ${status.trail_r}R` : null]
          .filter(Boolean)
          .join(' · ') || 'bracket only',
      )
    : undefined
  const chain = status?.model_chain?.length
    ? status.model_chain.join(' → ')
    : status?.model_fallbacks?.length
      ? `Fallbacks: ${status.model_fallbacks.join(' → ')}`
      : undefined
  const failing = decisions && decisions.consecutiveFailures > 0 && decisions.lastFailure
  return (
    <Panel
      title="Autopilot"
      className="tab-panel"
      actions={known ? <State tone={on ? 'ok' : 'idle'}>{on ? 'Running' : 'Off'}</State> : null}
    >
      <Fields>
        <Field label="Cadence" value={known ? (on && status ? `${status.interval_secs} seconds` : '—') : undefined} />
        <Field label="Timeframe" value={known ? (status?.timeframe ?? '—') : undefined} />
        <Field label="Window" value={known ? (on && status ? `${status.bars} bars` : '—') : undefined} />
        <Field label="Tier" value={known ? (status?.tier ?? '—') : undefined} />
        <Field label="Judgements" value={known ? (status?.jev ?? '—') : undefined} />
        <Field label="Stops" value={known ? (stops ?? '—') : undefined} />
        <Field
          label="Symbols"
          wide
          value={known ? (status ? (status.symbols.length > 0 ? status.symbols.join(' · ') : 'Chart symbol') : '—') : undefined}
        />
        <Field
          label="Profit harvest"
          wide
          value={
            known
              ? status?.profit_harvest
                ? `${status.profit_harvest.arm_r}R arm · ${status.profit_harvest.trail_r}R trail · ${amount(status.profit_harvest.min_profit)} floor`
                : 'Off'
              : undefined
          }
        />
        <Field
          label="Model chain"
          wide
          value={known ? (status ? (chain ?? 'No fallbacks') : '—') : undefined}
          tone={status && !chain ? 'tone-warn' : undefined}
        />
        <Field
          label="Last LLM"
          wide
          value={decisions === undefined ? undefined : (decisions?.lastModel ?? 'Not called yet')}
          tone={decisions?.lastModel ? undefined : 'tone-faint'}
        />
        <Field
          label="Last answer"
          wide
          value={decisions === undefined ? undefined : (decisions?.lastSuccessfulModel ?? 'None yet')}
          tone={decisions?.lastSuccessfulModel ? undefined : 'tone-faint'}
        />
        <Field
          label="Calls per hour"
          value={budget === undefined ? undefined : budget ? `${budget.hourCalls} / ${cap(budget.hourLimit)}` : '—'}
        />
        <Field
          label="Calls per day"
          value={budget === undefined ? undefined : budget ? `${amount(budget.dayCalls, 0)} / ${cap(budget.dayLimit)}` : '—'}
        />
        <Field
          label="Judge calls"
          value={
            jevUsage === undefined
              ? undefined
              : jevUsage && jevUsage.calls > 0
                ? `${amount(jevUsage.calls, 0)}${jevUsage.failures > 0 ? ` · ${jevUsage.failures} failed` : ''}`
                : '—'
          }
          tone={jevUsage && jevUsage.failures > 0 ? 'tone-warn' : undefined}
        />
        <Field
          label="Judge tokens"
          value={
            jevUsage === undefined
              ? undefined
              : jevUsage && jevUsage.calls > 0
                ? compactTokens(jevUsage.inputTokens + jevUsage.outputTokens)
                : '—'
          }
        />
        {failing ? (
          <Field
            label="Last failure"
            wide
            value={`${decisions.consecutiveFailures} in a row · ${decisions.lastFailure}`}
            tone="tone-bad"
          />
        ) : null}
      </Fields>
    </Panel>
  )
}

/* ---------- risk ---------- */

/** Editable mirror of the live policy; numbers stay strings until save. */
type PolicyDraft = {
  killSwitch: boolean
  allowTradingWithoutJev: boolean
  weekendPositions: WeekendPositions
  symbols: string
  weekendSymbols: string
  maxVolumePerOrder: string
  maxTotalLots: string
  maxOpenOrders: string
  duplicateWindowSecs: string
  sessionUtc: string
  maxRiskPercent: string
  maxDailyLossPercent: string
  maxPeakDrawdownPercent: string
  maxNetFactorLots: string
  calendarBlackoutMinutes: string
  minStopAtrFraction: string
}

type NumericDraftKey =
  | 'maxVolumePerOrder'
  | 'maxTotalLots'
  | 'maxOpenOrders'
  | 'duplicateWindowSecs'
  | 'maxRiskPercent'
  | 'maxDailyLossPercent'
  | 'maxPeakDrawdownPercent'
  | 'maxNetFactorLots'
  | 'calendarBlackoutMinutes'
  | 'minStopAtrFraction'

const POLICY_NUMBER_FIELDS: Array<{ key: NumericDraftKey; label: string; integer: boolean }> = [
  { key: 'maxVolumePerOrder', label: 'Max / order (lots)', integer: false },
  { key: 'maxTotalLots', label: 'Max total (lots)', integer: false },
  { key: 'maxOpenOrders', label: 'Max open orders', integer: true },
  { key: 'duplicateWindowSecs', label: 'Duplicate window (s)', integer: true },
  { key: 'maxRiskPercent', label: 'Max risk (% / trade)', integer: false },
  { key: 'maxDailyLossPercent', label: 'Daily brake (%)', integer: false },
  { key: 'maxPeakDrawdownPercent', label: 'Peak brake (%)', integer: false },
  { key: 'maxNetFactorLots', label: 'Net USD cap (lots)', integer: false },
  { key: 'calendarBlackoutMinutes', label: 'News blackout (minutes)', integer: true },
  { key: 'minStopAtrFraction', label: 'Min stop (× ATR)', integer: false },
]

function draftFromPolicy(policy: RiskPolicy): PolicyDraft {
  return {
    killSwitch: policy.killSwitch,
    allowTradingWithoutJev: policy.allowTradingWithoutJev,
    weekendPositions: policy.weekendPositions,
    symbols: policy.symbols.join(', '),
    weekendSymbols: (policy.weekendSymbols ?? []).join(', '),
    maxVolumePerOrder: String(policy.maxVolumePerOrder),
    maxTotalLots: String(policy.maxTotalLots),
    maxOpenOrders: String(policy.maxOpenOrders),
    duplicateWindowSecs: String(policy.duplicateWindowSecs),
    sessionUtc: policy.sessionUtc ?? '',
    maxRiskPercent: String(policy.maxRiskPercent),
    maxDailyLossPercent: String(policy.maxDailyLossPercent),
    maxPeakDrawdownPercent: String(policy.maxPeakDrawdownPercent),
    maxNetFactorLots: String(policy.maxNetFactorLots),
    calendarBlackoutMinutes: String(policy.calendarBlackoutMinutes),
    minStopAtrFraction: String(policy.minStopAtrFraction),
  }
}

/** Parses the draft into a patch, or returns the first input error. */
function patchFromDraft(draft: PolicyDraft): { patch: RiskPolicyPatch } | { error: string } {
  const patch: RiskPolicyPatch = {
    killSwitch: draft.killSwitch,
    allowTradingWithoutJev: draft.allowTradingWithoutJev,
    weekendPositions: draft.weekendPositions,
    symbols: draft.symbols
      .split(',')
      .map((symbol) => symbol.trim())
      .filter(Boolean),
    weekendSymbols: draft.weekendSymbols
      .split(',')
      .map((symbol) => symbol.trim())
      .filter(Boolean),
    sessionUtc: draft.sessionUtc.trim(),
  }
  for (const field of POLICY_NUMBER_FIELDS) {
    const raw = draft[field.key].trim()
    const value = Number(raw)
    if (raw === '' || Number.isNaN(value)) {
      return { error: `${field.label}: must be a number` }
    }
    if (field.integer && !Number.isInteger(value)) {
      return { error: `${field.label}: must be a whole number` }
    }
    patch[field.key] = value
  }
  return { patch }
}

/**
 * A control's head: the label, an info mark after it when there is help to
 * give, and an aside held to the right. `htmlFor` makes the label a real
 * `<label>` for a native input; switches and fixed values name themselves.
 */
function ControlHead({
  label,
  htmlFor,
  help,
  aside,
}: {
  label: string
  htmlFor?: string
  /** What the setting does, shown from the info mark. */
  help?: string
  aside?: ReactNode
}) {
  return (
    <div className="tab-control-head">
      <span className="tab-control-label">
        {htmlFor ? (
          <label htmlFor={htmlFor} className="tab-control-name">
            {label}
          </label>
        ) : (
          <span className="tab-control-name">{label}</span>
        )}
        {help ? <Hint text={help} label={label} /> : null}
      </span>
      {aside ? <span className="tab-control-aside">{aside}</span> : null}
    </div>
  )
}

/** Label above, optional marker beside it, control below. */
function Control({
  id,
  label,
  help,
  span = false,
  aside,
  children,
}: {
  id: string
  label: string
  help?: string
  /** Take two columns (long lists). */
  span?: boolean
  /** Right-aligned beside the label, e.g. an override marker. */
  aside?: ReactNode
  children: ReactNode
}) {
  return (
    <div className={`tab-control${span ? ' is-span' : ''}`}>
      <ControlHead label={label} htmlFor={id} help={help} aside={aside} />
      {children}
    </div>
  )
}

/** A labelled text input for the editors. */
function TextControl({
  label,
  field,
  value,
  onChange,
  placeholder,
  span,
  disabled = false,
  dirty = false,
  aside,
  help,
}: {
  label: string
  /** Rides on the element as `data-field`, so one handler serves every input. */
  field: string
  value: string
  onChange: (event: ChangeEvent<HTMLInputElement>) => void
  placeholder?: string
  span?: boolean
  disabled?: boolean
  /** Changed but not yet applied. */
  dirty?: boolean
  aside?: ReactNode
  help?: string
}) {
  const id = useId()
  return (
    <Control id={id} label={label} help={help} span={span} aside={aside}>
      <input
        id={id}
        className={`tab-input${dirty ? ' is-dirty' : ''}`}
        data-field={field}
        value={value}
        placeholder={placeholder}
        disabled={disabled}
        onChange={onChange}
        spellCheck={false}
        autoComplete="off"
      />
    </Control>
  )
}

/** The weekend preference is a choice, not free text. */
function WeekendControl({
  value,
  onChange,
}: {
  value: WeekendPositions
  onChange: (value: WeekendPositions) => void
}) {
  const id = useId()
  return (
    <Control id={id} label="Weekend positions">
      <span className="tab-select">
        <select
          id={id}
          className="tab-input"
          value={value}
          onChange={(event) => onChange(event.target.value as WeekendPositions)}
        >
          <option value="agent">Agent decides per position</option>
          <option value="hold">Hold through the weekend</option>
          <option value="flatten">Flatten before the close</option>
        </select>
        <Icon name="chevron-down" size={16} />
      </span>
    </Control>
  )
}

export function RiskPanel({
  policy,
  status,
  onApply,
}: {
  policy?: RiskPolicy
  status?: Status
  /** Applies a patch; resolves to an error message or undefined on success. */
  onApply?: (patch: RiskPolicyPatch) => Promise<string | undefined>
}) {
  // An open draft is the editor; there is no separate editing flag to drift.
  const [draft, setDraft] = useState<PolicyDraft>()
  const [error, setError] = useState<string>()
  const [saving, setSaving] = useState(false)

  const startEditing = (current: RiskPolicy) => {
    setDraft(draftFromPolicy(current))
    setError(undefined)
  }
  const cancelEditing = () => {
    setDraft(undefined)
    setError(undefined)
  }
  /** One handler for every text input; the field name rides on the element. */
  const handleField = (event: ChangeEvent<HTMLInputElement>) => {
    const key = event.target.dataset.field as keyof PolicyDraft
    const value = event.target.value
    setDraft((current) => current && { ...current, [key]: value })
  }
  const flip = (key: 'killSwitch' | 'allowTradingWithoutJev') =>
    setDraft((current) => current && { ...current, [key]: !current[key] })
  const setWeekend = (value: WeekendPositions) =>
    setDraft((current) => current && { ...current, weekendPositions: value })
  const save = async (apply: (patch: RiskPolicyPatch) => Promise<string | undefined>, current: PolicyDraft) => {
    const parsed = patchFromDraft(current)
    if ('error' in parsed) {
      setError(parsed.error)
      return
    }
    setSaving(true)
    setError(undefined)
    const failure = await apply(parsed.patch)
    setSaving(false)
    if (failure) {
      setError(failure)
    } else {
      setDraft(undefined)
    }
  }

  if (draft && onApply) {
    return (
      <Panel
        title="Risk policy"
        className="tab-panel"
        actions={
          <>
            <Button onClick={cancelEditing}>Cancel</Button>
            <Button tone="ok" onClick={() => void save(onApply, draft)} disabled={saving}>
              {saving ? 'Saving…' : 'Save'}
            </Button>
          </>
        }
      >
        {error ? (
          <p className="tab-error" role="alert">
            {error}
          </p>
        ) : null}
        <div className="tab-form">
          <div className="tab-switch is-span">
            <span>Kill switch</span>
            <Toggle checked={draft.killSwitch} label="Kill switch" tone="bad" onClick={() => flip('killSwitch')} />
          </div>
          <div className="tab-switch is-span">
            <span>Judge bypass</span>
            <Toggle
              checked={draft.allowTradingWithoutJev}
              label="Judge bypass"
              tone="warn"
              onClick={() => flip('allowTradingWithoutJev')}
            />
          </div>
          <TextControl label="Symbols" field="symbols" value={draft.symbols} onChange={handleField} span />
          <TextControl
            label="Weekend symbols"
            field="weekendSymbols"
            value={draft.weekendSymbols}
            onChange={handleField}
            span
          />
          <TextControl
            label="Session UTC"
            field="sessionUtc"
            value={draft.sessionUtc}
            onChange={handleField}
            placeholder="Always open"
          />
          <WeekendControl value={draft.weekendPositions} onChange={setWeekend} />
          {POLICY_NUMBER_FIELDS.map((field) => (
            <TextControl
              key={field.key}
              label={field.label}
              field={field.key}
              value={draft[field.key]}
              onChange={handleField}
            />
          ))}
        </div>
      </Panel>
    )
  }

  const known = policy !== undefined
  const view = (text: (current: RiskPolicy) => string) => (policy ? text(policy) : undefined)
  const offOr = (active: boolean, text: string) => (active ? text : 'Off')
  return (
    <Panel
      title="Risk policy"
      className="tab-panel"
      actions={
        known ? (
          <>
            {policy.killSwitch ? <State tone="bad">Kill switch on</State> : <State tone="ok">Gate active</State>}
            {onApply ? <Button onClick={() => startEditing(policy)}>Edit</Button> : null}
          </>
        ) : null
      }
    >
      <Fields columns={3}>
        <Field
          label="Symbols"
          wide
          value={view((current) => (current.symbols.length > 0 ? current.symbols.join(' · ') : 'None allowed'))}
        />
        <Field label="Weekend markets" value={view((current) => (current.weekendSymbols ?? []).join(' · ') || 'None')} />
        <Field label="Session UTC" value={view((current) => current.sessionUtc ?? 'Always open')} />
        <Field label="Weekend positions" value={view((current) => WEEKEND_LABELS[current.weekendPositions])} />
        <Field label="Max per order" value={view((current) => `${current.maxVolumePerOrder} lots`)} />
        <Field label="Max total" value={view((current) => `${current.maxTotalLots} lots`)} />
        <Field label="Max open orders" value={view((current) => String(current.maxOpenOrders))} />
        <Field label="Duplicate window" value={view((current) => `${current.duplicateWindowSecs}s`)} />
        <Field
          label="Max risk"
          value={view((current) => offOr(current.maxRiskPercent > 0, `${current.maxRiskPercent}% per trade`))}
        />
        <Field
          label="Loss brakes"
          value={view((current) =>
            sentence(
              [
                current.maxDailyLossPercent > 0 ? `day ${current.maxDailyLossPercent}%` : null,
                current.maxPeakDrawdownPercent > 0 ? `peak ${current.maxPeakDrawdownPercent}%` : null,
              ]
                .filter(Boolean)
                .join(' · ') || 'off',
            ),
          )}
        />
        <Field
          label="Net USD cap"
          value={view((current) => offOr(current.maxNetFactorLots > 0, `${current.maxNetFactorLots} lots`))}
        />
        <Field
          label="News blackout"
          value={view((current) => offOr(current.calendarBlackoutMinutes > 0, `${current.calendarBlackoutMinutes}m`))}
        />
        <Field
          label="Stop floor"
          value={view((current) => offOr(current.minStopAtrFraction > 0, `${current.minStopAtrFraction}× ATR`))}
        />
        <Field
          label="Judge outage"
          value={view((current) => (current.allowTradingWithoutJev ? 'Keeps trading' : 'Pauses decisions'))}
          tone={policy?.allowTradingWithoutJev ? 'tone-warn' : undefined}
        />
        <Field
          label="Execution"
          value={status ? (status.trading_enabled ? 'Enabled' : 'Disabled') : undefined}
          tone={status?.trading_enabled ? 'tone-warn' : undefined}
        />
        <Field
          label="Terminal"
          value={status ? (status.ea_live_orders ? 'Armed' : 'Disarmed') : undefined}
          tone={status?.ea_live_orders ? 'tone-ok' : undefined}
        />
      </Fields>
    </Panel>
  )
}

/* ---------- metrics ---------- */

export function MetricsPanel({ metrics, error, status }: { metrics?: Metrics; error?: string; status?: Status }) {
  const counters = metrics
    ? Object.entries(metrics.counters)
        .sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]))
        .slice(0, 12)
    : []
  return (
    <Panel
      title="Metrics"
      className="tab-panel"
      actions={error && !metrics ? <Unavailable error={error} /> : null}
    >
      <Fields>
        <Field label="Version" value={status?.version} />
        <Field label="Environment" value={status?.environment} />
        <Field label="Audit" value={status ? (status.persistence ?? 'Off') : undefined} />
        <Field label="Feed" value={metrics ? `#${metrics.feedLatest}` : error ? '—' : undefined} />
      </Fields>
      {counters.length > 0 ? (
        <div className="tab-table tab-counters">
          <div className="tab-table-head" aria-hidden="true">
            <span>Counter</span>
            <span>Count</span>
          </div>
          <ul className="tab-list">
            {counters.map(([key, value]) => (
              <li key={key} className="tab-row tab-counter">
                <span className="mono" title={key}>
                  {key}
                </span>
                <span className="readout">{amount(value, 0)}</span>
              </li>
            ))}
          </ul>
        </div>
      ) : metrics ? (
        <Empty>No counters yet</Empty>
      ) : error ? null : (
        <SkeletonRows />
      )}
    </Panel>
  )
}

/* ---------- commands ---------- */

const COMMAND_STATUS: Record<CommandRecord['status'], { tone: Tone; label: string }> = {
  pending: { tone: 'idle', label: 'Pending' },
  completed: { tone: 'ok', label: 'Completed' },
  failed: { tone: 'bad', label: 'Failed' },
}

export function CommandsPanel({ commands }: { commands?: CommandRecord[] }) {
  const [expandedId, setExpandedId] = useState<string | undefined>(undefined)
  const paged = usePaged(commands ?? [], 12)
  return (
    <Panel title="Commands" count={commands?.length} className="tab-panel">
      {!commands ? (
        <SkeletonRows />
      ) : commands.length === 0 ? (
        <Empty>No commands yet</Empty>
      ) : (
        <div className="tab-table">
          <div className="tab-table-head tab-command" aria-hidden="true">
            <span>Command</span>
            <span className="tab-command-id">Id</span>
            <span className="tab-command-result">Result</span>
            <span>Status</span>
          </div>
          <ul className="tab-list">
            {paged.items.map((command) => {
              const expanded = command.id === expandedId
              const state = COMMAND_STATUS[command.status]
              return (
                <li key={command.id} className="tab-row">
                  <button
                    type="button"
                    onClick={() => setExpandedId(expanded ? undefined : command.id)}
                    aria-expanded={expanded}
                    className="tab-row-button tab-command"
                  >
                    <span className="tab-command-kind">{sentence(command.kind)}</span>
                    <span className="tab-command-id mono">{command.id.slice(0, 8)}</span>
                    <span className="tab-command-result">
                      {command.reason ? (
                        <span className="tone-bad">{command.reason}</span>
                      ) : command.summary ? (
                        <span className="mono">{JSON.stringify(command.summary)}</span>
                      ) : null}
                    </span>
                    <span className="tab-command-status">
                      <Dot tone={state.tone} />
                      {state.label}
                    </span>
                  </button>
                  {expanded ? (
                    <div className="tab-detail">
                      <dl className="tab-detail-rows">
                        <div>
                          <dt>Id</dt>
                          <dd className="mono">{command.id}</dd>
                        </div>
                      </dl>
                      <Raw value={{ summary: command.summary ?? null, reason: command.reason ?? null }} />
                    </div>
                  ) : null}
                </li>
              )
            })}
          </ul>
        </div>
      )}
      <ListPager paged={paged} />
    </Panel>
  )
}

/* ---------- activity feed ---------- */

/**
 * The line under an activity title: the decision's reason where there is one,
 * else the model's stated rationale, else a digest of the payload. `raw` marks
 * a digest that is still JSON, which is set in the mono face.
 */
function eventLine(event: FeedEvent): { text: string; raw: boolean } | undefined {
  const detail = activityDetail(event)
  if (detail) return { text: detail, raw: false }
  const payload = event.payload ?? {}
  const answer = payload.answer as { rationale?: unknown } | null | undefined
  if (typeof answer?.rationale === 'string') return { text: answer.rationale, raw: false }
  if (event.kind.startsWith('command_') && typeof payload.kind === 'string') {
    return { text: sentence(payload.kind), raw: false }
  }
  if (event.kind === 'broker_snapshot' && typeof payload.orders === 'number') {
    return { text: `${payload.orders} order${payload.orders === 1 ? '' : 's'} · ${payload.lots ?? 0} lots`, raw: false }
  }
  if (event.kind === 'balance_observed' && typeof payload.balance === 'number') {
    return { text: `Balance ${amount(payload.balance)}`, raw: false }
  }
  const summary = payloadSummary(event)
  if (summary === '{}') return undefined
  return { text: summary, raw: summary.startsWith('{') }
}

function EventDetail({ event }: { event: FeedEvent }) {
  const payload = event.payload ?? {}
  return (
    <div className="tab-detail">
      <div className="tab-detail-meta">
        <span>
          <span className="mono">{event.kind}</span> · #{event.seq}
        </span>
        <span>
          <span className="mono">{isoTime(event.at_ms)}</span> · {relativeTime(event.at_ms)}
        </span>
      </div>
      <dl className="tab-detail-rows">
        {detailRows(payload).map((row) => (
          <div key={row.label}>
            <dt>{row.label}</dt>
            <dd className={/^[[{]/.test(row.value) ? 'mono' : undefined}>{row.value}</dd>
          </div>
        ))}
      </dl>
      <details className="tab-raw">
        <summary>Raw payload</summary>
        <Raw value={payload} />
      </details>
    </div>
  )
}

export function ActivityFeed({
  events,
  connected,
  focus,
  onFocusChange,
}: {
  events: FeedEvent[]
  connected: boolean
  focus: boolean
  onFocusChange: (focus: boolean) => void
}) {
  const [selectedSeq, setSelectedSeq] = useState<number | undefined>(undefined)
  const visible = focus ? events.filter((event) => !isRoutine(event)) : events
  const paged = usePaged(visible, 14)

  return (
    <Panel
      title="Activity"
      className="tab-panel"
      actions={
        <>
          {connected ? (
            <State tone="ok">Streaming</State>
          ) : events.length > 0 ? (
            <State tone="bad">Reconnecting</State>
          ) : (
            <State tone="idle">Connecting</State>
          )}
          <Segmented
            label="Activity filter"
            options={[
              { value: 'focus', label: 'Focus' },
              { value: 'all', label: 'All' },
            ]}
            value={focus ? 'focus' : 'all'}
            onChange={(value) => onFocusChange(value === 'focus')}
          />
        </>
      }
    >
      {events.length === 0 && !connected ? (
        <SkeletonRows />
      ) : visible.length === 0 ? (
        <Empty>{events.length === 0 ? 'No events yet' : 'No decisions yet'}</Empty>
      ) : (
        <ul className="tab-list">
          {paged.items.map((event) => {
            const expanded = event.seq === selectedSeq
            const line = eventLine(event)
            return (
              <li key={event.seq} className="tab-row">
                <button
                  type="button"
                  onClick={() => setSelectedSeq(expanded ? undefined : event.seq)}
                  aria-expanded={expanded}
                  className="tab-row-button tab-event"
                >
                  <time className="tab-time readout" dateTime={isoTime(event.at_ms)}>
                    {clockTime(event.at_ms)}
                  </time>
                  {/* Routine plumbing stays grey so the eye lands on decisions. */}
                  <Dot tone={isRoutine(event) ? 'idle' : activityTone(event)} />
                  <span className="tab-event-text">
                    <span className="tab-event-title">{activityTitle(event)}</span>
                    {line ? <span className={`tab-event-line${line.raw ? ' mono' : ''}`}>{line.text}</span> : null}
                  </span>
                </button>
                {expanded ? <EventDetail event={event} /> : null}
              </li>
            )
          })}
        </ul>
      )}
      <ListPager paged={paged} />
    </Panel>
  )
}

/* ---------- durable trace ---------- */

/**
 * What identifies a trail row at a glance, from the fields most rows carry.
 * A part that only repeats the row's own kind says nothing and is dropped.
 */
function traceSummary(kind: string, payload: Record<string, unknown>): string {
  const parts = [payload.outcome, payload.kind, payload.tool, payload.symbol].filter(
    (part): part is string => typeof part === 'string' && part !== '' && part !== kind,
  )
  return parts.length > 0 ? sentence(parts.join(' · ')) : `${Object.keys(payload).length} fields`
}

/** Short single-line scalars read as a row; anything larger keeps a block. */
function isInline(value: unknown): boolean {
  if (value === null || typeof value === 'number' || typeof value === 'boolean') return true
  return typeof value === 'string' && value.length <= 120 && !value.includes('\n')
}

/**
 * The durable trail, not the in-memory ring: what survives a restart.
 *
 * Every row is expandable to its whole payload — a model turn shows the exact
 * prompt it was given and the answer it returned, a tool call shows its
 * arguments and result. Nothing is summarised away, because the point of this
 * view is to answer "what actually happened" without reading the database.
 */
export function TracePanel({
  page,
  error,
  kind,
  onKindChange,
}: {
  page?: AuditPage
  error?: string
  kind: string
  onKindChange: (kind: string) => void
}) {
  const [openId, setOpenId] = useState<string | undefined>(undefined)
  const rows = page?.events ?? []
  const kinds = ['all', ...Array.from(new Set(rows.map((row) => row.kind))).sort()]
  const visible = kind === 'all' ? rows : rows.filter((row) => row.kind === kind)
  const paged = usePaged(visible, 12)
  // A disabled or unreachable trail must not pass for an empty one.
  const problem = error ? 'Unavailable' : page && page.status !== 'ok' ? sentence(page.status) : undefined

  return (
    <Panel
      title="Audit trail"
      count={page ? rows.length : undefined}
      className="tab-panel"
      actions={
        problem && rows.length > 0 ? (
          <State tone={error ? 'bad' : 'warn'} title={error ?? page?.error}>
            {problem}
          </State>
        ) : null
      }
    >
      {rows.length > 0 ? (
        <div className="tab-toolbar">
          <Segmented
            label="Trail kind"
            options={kinds.slice(0, 8).map((candidate) => ({ value: candidate, label: sentence(candidate) }))}
            value={kind}
            onChange={onKindChange}
          />
        </div>
      ) : null}
      {visible.length === 0 ? (
        !page && !error ? (
          <SkeletonRows />
        ) : (
          <Empty>
            <span title={error ?? page?.error}>{problem ?? 'No events yet'}</span>
          </Empty>
        )
      ) : (
        <ul className="tab-list">
          {paged.items.map((row) => {
            const open = row.id === openId
            const payload = row.payload ?? {}
            const at = auditTimeMs(row.at)
            return (
              <li key={row.id} className="tab-row">
                <button
                  type="button"
                  onClick={() => setOpenId(open ? undefined : row.id)}
                  aria-expanded={open}
                  className="tab-row-button tab-trace"
                >
                  <time className="tab-time readout" dateTime={isoTime(at)}>
                    {Number.isNaN(at) ? '—' : clockTime(at)}
                  </time>
                  <span className="tab-trace-kind">{sentence(row.kind)}</span>
                  <span className="tab-trace-summary">{traceSummary(row.kind, payload)}</span>
                  <span className="tab-trace-id mono" title={row.id}>
                    {row.id.slice(0, 8)}
                  </span>
                </button>
                {open ? (
                  <div className="tab-detail">
                    <div className="tab-detail-meta">
                      <span className="mono">{row.kind}</span>
                      <span className="mono">{row.id}</span>
                    </div>
                    <dl className="tab-detail-rows is-raw">
                      {Object.entries(payload)
                        .filter(([, value]) => isInline(value))
                        .map(([field, value]) => (
                          <div key={field}>
                            <dt>{field}</dt>
                            <dd className="mono">{String(value)}</dd>
                          </div>
                        ))}
                    </dl>
                    {Object.entries(payload)
                      .filter(([, value]) => !isInline(value))
                      .map(([field, value]) => (
                        <div key={field} className="tab-trace-field">
                          <div className="tab-trace-label mono">{field}</div>
                          <Raw value={value} />
                        </div>
                      ))}
                  </div>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
      <ListPager paged={paged} />
    </Panel>
  )
}

/* ---------- agent log ---------- */

export function LogsPanel({
  logs,
  error,
  level,
  onLevelChange,
}: {
  logs: LogRecord[]
  error?: string
  level: LogLevel
  onLevelChange: (level: LogLevel) => void
}) {
  const ordered = [...logs].reverse()
  const paged = usePaged(ordered, 20)
  return (
    <Panel
      title="Agent log"
      className="tab-panel"
      actions={
        <Segmented
          label="Log level"
          options={LOG_LEVELS.map((candidate) => ({ value: candidate, label: sentence(candidate) }))}
          value={level}
          onChange={onLevelChange}
        />
      }
    >
      {ordered.length === 0 ? (
        <Empty>{error ? <span title={error}>Unavailable</span> : 'No log lines yet'}</Empty>
      ) : (
        <ul className="tab-list">
          {paged.items.map((record) => (
            <li key={record.seq} className="tab-row tab-log">
              <time className="tab-time readout" dateTime={isoTime(record.atMs)}>
                {clockTime(record.atMs)}
              </time>
              <span className={`tab-level is-${record.level}`}>{sentence(record.level)}</span>
              <span className="tab-log-line mono">
                <span className="tab-log-target">{record.target}</span> {record.message}
                {Object.keys(record.fields).length > 0 ? (
                  <span className="tab-log-fields"> {JSON.stringify(record.fields)}</span>
                ) : null}
              </span>
            </li>
          ))}
        </ul>
      )}
      <ListPager paged={paged} />
    </Panel>
  )
}

/* ---------- live settings ---------- */

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
      'VEYRA_MODEL_PROVIDER',
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
    'Model for the Fast tier, meant to be the cheapest, as an OpenRouter id such as openai/gpt-4.1-mini.',
  VEYRA_MODEL_BALANCED: "Model for the Balanced tier, the autopilot's default, as an OpenRouter id (vendor/model).",
  VEYRA_MODEL_REASONING: 'Model for the Reasoning tier, meant to be the strongest, as an OpenRouter id (vendor/model).',
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
  VEYRA_MODEL_PROVIDER: 'Service that runs the model tiers. OpenRouter is the only one supported.',
  VEYRA_MODEL_BASE_URL: "Address of the model API; empty uses OpenRouter's standard endpoint.",
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
  VEYRA_MODEL_PROVIDER: { kind: 'fixed', fallback: 'openrouter' },
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

/** A setting with a single supported value: stated, not offered as a choice. */
function FixedSetting({ label, help, value, aside }: { label: string; help?: string; value: string; aside?: ReactNode }) {
  return (
    <div className="tab-control">
      <ControlHead label={label} help={help} aside={aside} />
      <div className="tab-setting-fixed">{value || 'Not set'}</div>
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
  onApply,
  onRefresh,
}: {
  settings?: Record<string, LiveSetting>
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
                    placeholder="Not set"
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
