import { useState } from 'react'
import type { ChangeEvent } from 'react'
import type { ReactNode } from 'react'

import type {
  Account,
  Candle,
  CandleSeries,
  CommandRecord,
  FeedEvent,
  LogLevel,
  LogRecord,
  Metrics,
  Position,
  RiskPolicy,
  Status,
} from '../lib/api'
import { LOG_LEVELS, VEYRA_MAGIC } from '../lib/api'
import type { RiskPolicyPatch } from '../lib/api'
import {
  commandTone,
  detailRows,
  isRoutine,
  kindTone,
  outcomeTone,
  payloadSummary,
} from '../lib/format'
import { clockTime, money, relativeTime } from '../lib/hooks'

/* ---------- primitives ---------- */

type Tone = 'ok' | 'warn' | 'bad' | 'off' | 'info'

const toneText: Record<Tone, string> = {
  ok: 'text-emerald-400',
  warn: 'text-amber-400',
  bad: 'text-rose-400',
  off: 'text-slate-500',
  info: 'text-sky-400',
}

const toneDot: Record<Tone, string> = {
  ok: 'bg-emerald-400',
  warn: 'bg-amber-400',
  bad: 'bg-rose-400',
  off: 'bg-slate-600',
  info: 'bg-sky-400',
}

export function Pill({ tone, label, value }: { tone: Tone; label: string; value?: string }) {
  return (
    <div className="flex items-center gap-2 rounded-md border border-slate-800 bg-slate-900/70 px-2.5 py-1.5">
      <span className={`size-1.5 rounded-full ${toneDot[tone]} ${tone === 'ok' ? 'animate-pulse' : ''}`} />
      <span className="text-[11px] uppercase tracking-wider text-slate-500">{label}</span>
      {value ? <span className={`text-xs font-medium ${toneText[tone]}`}>{value}</span> : null}
    </div>
  )
}

export function Panel({
  title,
  detail,
  children,
  className = '',
}: {
  title: string
  detail?: ReactNode
  children: ReactNode
  className?: string
}) {
  return (
    <section
      className={`flex min-h-0 min-w-0 flex-col overflow-hidden rounded-lg border border-slate-800 bg-[#0c1017] ${className}`}
    >
      <header className="flex items-center justify-between border-b border-slate-800/80 px-3 py-2">
        <h2 className="text-[11px] font-semibold uppercase tracking-widest text-slate-400">{title}</h2>
        {detail ? <div className="text-[11px] text-slate-500">{detail}</div> : null}
      </header>
      <div className="min-h-0 flex-1 overflow-auto">{children}</div>
    </section>
  )
}

function Field({ label, value, tone }: { label: string; value: ReactNode; tone?: string }) {
  return (
    <div>
      <div className="text-[10px] uppercase tracking-wider text-slate-500">{label}</div>
      <div className={`font-mono text-sm tabular-nums ${tone ?? 'text-slate-200'}`}>{value}</div>
    </div>
  )
}

/* ---------- header ---------- */

export function StatusPills({ status }: { status?: Status }) {
  if (!status) {
    return <Pill tone="off" label="service" value="connecting…" />
  }
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Pill tone={status.broker_connected ? 'ok' : 'bad'} label="terminal" value={status.broker_connected ? 'live' : 'stale'} />
      <Pill tone={status.ea_live_orders ? 'ok' : 'off'} label="EA" value={status.ea_live_orders ? 'armed' : 'disarmed'} />
      <Pill tone={status.trading_enabled ? 'warn' : 'off'} label="trading" value={status.trading_enabled ? 'enabled' : 'disabled'} />
      <Pill
        tone={status.autopilot?.enabled ? 'info' : 'off'}
        label="autopilot"
        value={status.autopilot?.enabled ? `${status.autopilot.interval_secs}s · ${status.autopilot.timeframe}` : 'off'}
      />
      <Pill tone={status.persistence ? 'ok' : 'off'} label="audit" value={status.persistence ?? 'off'} />
      <Pill tone="off" label="env" value={status.environment} />
    </div>
  )
}

/* ---------- account ---------- */

export function AccountPanel({ account, error }: { account?: Account; error?: string }) {
  const positions = account?.positions ?? []
  const totalProfit = positions.reduce((sum, position) => sum + (position.profit ?? 0), 0)
  return (
    <Panel
      title="Account"
      detail={
        account
          ? `updated ${relativeTime(Date.now() - account.ageSecs * 1000)}`
          : error
            ? <span className="text-rose-400">{error}</span>
            : 'waiting…'
      }
    >
      <div className="grid grid-cols-2 gap-x-4 gap-y-3 p-3 sm:grid-cols-3">
        <Field label="Balance" value={money(account?.balance)} />
        <Field label="Equity" value={money(account?.equity)} />
        <Field label="Free margin" value={money(account?.freeMargin)} />
        <Field
          label="Margin level"
          value={account?.marginLevel != null ? `${account.marginLevel.toFixed(1)}%` : '—'}
        />
        <Field label="Leverage" value={account?.leverage != null ? `1:${account.leverage}` : '—'} />
        <Field label="Open orders" value={account?.orders ?? '—'} />
        <Field label="Open lots" value={account?.lots ?? '—'} />
        <Field
          label="Open P/L"
          value={positions.length > 0 ? `${totalProfit >= 0 ? '+' : ''}${totalProfit.toFixed(2)}` : '—'}
          tone={totalProfit >= 0 ? 'text-emerald-400' : 'text-rose-400'}
        />
      </div>
      <div className="border-t border-slate-800/80 px-3 py-2 text-[11px] text-slate-500">
        {account?.server ? `${account.server} · #${account.login} · ` : ''}
        {account?.symbol ?? ''}
      </div>
    </Panel>
  )
}

export function PositionsPanel({ account }: { account?: Account }) {
  const positions = account?.positions ?? []
  return (
    <Panel
      title="Positions"
      detail={account?.positionsTruncated ? <span className="text-amber-400">truncated</span> : `${positions.length}`}
    >
      {positions.length === 0 ? (
        <div className="p-3 text-xs text-slate-500">Flat — no open orders.</div>
      ) : (
        <table className="w-full text-left text-xs">
          <thead className="text-[10px] uppercase tracking-wider text-slate-500">
            <tr className="border-b border-slate-800/80">
              <th className="px-3 py-1.5 font-medium">Ticket</th>
              <th className="px-2 py-1.5 font-medium">Side</th>
              <th className="px-2 py-1.5 font-medium">Lots</th>
              <th className="px-2 py-1.5 font-medium">Entry</th>
              <th className="px-2 py-1.5 font-medium">SL</th>
              <th className="px-2 py-1.5 font-medium">TP</th>
              <th className="px-2 py-1.5 font-medium">P/L</th>
              <th className="px-3 py-1.5 font-medium">Owner</th>
            </tr>
          </thead>
          <tbody className="font-mono tabular-nums">
            {positions.map((position: Position) => (
              <tr key={position.ticket} className="border-b border-slate-800/40">
                <td className="px-3 py-1.5 text-slate-300">{position.ticket}</td>
                <td className={`px-2 py-1.5 ${position.kind === 'buy' ? 'text-emerald-400' : 'text-rose-400'}`}>
                  {position.kind}
                </td>
                <td className="px-2 py-1.5 text-slate-300">{position.lots}</td>
                <td className="px-2 py-1.5 text-slate-300">{position.price}</td>
                <td className="px-2 py-1.5 text-rose-300/80">{position.sl > 0 ? position.sl : '—'}</td>
                <td className="px-2 py-1.5 text-emerald-300/80">{position.tp > 0 ? position.tp : '—'}</td>
                <td className={`px-2 py-1.5 ${position.profit >= 0 ? 'text-emerald-400' : 'text-rose-400'}`}>
                  {position.profit >= 0 ? '+' : ''}
                  {position.profit.toFixed(2)}
                </td>
                <td className="px-3 py-1.5">
                  {position.magic === VEYRA_MAGIC ? (
                    <span className="rounded bg-violet-500/15 px-1.5 py-0.5 text-[10px] text-violet-300">veyra</span>
                  ) : (
                    <span className="rounded bg-slate-700/40 px-1.5 py-0.5 text-[10px] text-slate-400">manual</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  )
}

/* ---------- market ---------- */

function Sparkline({ candles }: { candles: Candle[] }) {
  if (candles.length < 2) return null
  const closes = candles.map((candle) => candle.close)
  const min = Math.min(...closes)
  const max = Math.max(...closes)
  const span = max - min || 1
  const width = 100
  const height = 30
  const points = closes
    .map((close, index) => {
      const x = (index / (closes.length - 1)) * width
      const y = height - ((close - min) / span) * height
      return `${x.toFixed(2)},${y.toFixed(2)}`
    })
    .join(' ')
  const rising = closes[closes.length - 1] >= closes[0]
  const stroke = rising ? '#34d399' : '#fb7185'
  return (
    <svg viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" className="h-16 w-full">
      <polygon points={`0,${height} ${points} ${width},${height}`} fill={stroke} opacity={0.08} />
      <polyline points={points} fill="none" stroke={stroke} strokeWidth={1.2} vectorEffect="non-scaling-stroke" />
    </svg>
  )
}

export function MarketPanel({ series, error }: { series?: CandleSeries; error?: string }) {
  const last = series?.candles.at(-1)
  const first = series?.candles.at(0)
  const change = last && first ? ((last.close - first.close) / first.close) * 100 : undefined
  return (
    <Panel
      title="Market"
      detail={series ? `${series.symbol} ${series.timeframe}` : error ? <span className="text-rose-400">{error}</span> : '…'}
    >
      <div className="p-3">
        {series ? <Sparkline candles={series.candles} /> : <div className="h-16" />}
        <div className="mt-2 grid grid-cols-3 gap-3">
          <Field label="Last close" value={last ? last.close.toFixed(5) : '—'} />
          <Field
            label={`Change ${series?.candles.length ?? 0}b`}
            value={change === undefined ? '—' : `${change >= 0 ? '+' : ''}${change.toFixed(2)}%`}
            tone={change !== undefined && change >= 0 ? 'text-emerald-400' : 'text-rose-400'}
          />
          <Field label="H / L" value={last ? `${last.high.toFixed(5)} / ${last.low.toFixed(5)}` : '—'} />
        </div>
      </div>
    </Panel>
  )
}

/* ---------- autopilot ---------- */

/** Compact token count for the panel (1234 -> 1.2k). */
function compactTokens(value: number): string {
  return value >= 1000 ? `${(value / 1000).toFixed(1)}k` : String(value)
}

export function AutopilotPanel({
  status,
  budget,
  jevUsage,
}: {
  status?: Status['autopilot']
  budget?: Status['model_budget']
  jevUsage?: Status['jev_usage']
}) {
  const on = status?.enabled === true
  return (
    <Panel title="Autopilot" detail={on ? 'deciding on cadence' : 'disabled'}>
      <div className="grid grid-cols-2 gap-x-4 gap-y-3 p-3">
        <Field label="Cadence" value={on && status ? `${status.interval_secs}s` : '—'} />
        <Field label="Timeframe" value={status?.timeframe ?? '—'} />
        <Field label="Window" value={on && status ? `${status.bars} bars` : '—'} />
        <Field label="Model tier" value={status?.tier ?? '—'} />
        <Field label="Judgements" value={status?.jev ?? '—'} />
        <Field
          label="Symbols"
          value={status && status.symbols.length > 0 ? status.symbols.join(' · ') : 'chart symbol'}
        />
        <Field
          label="Stops"
          value={
            status && (status.breakeven_r > 0 || status.trail_r > 0)
              ? [status.breakeven_r > 0 ? `BE ${status.breakeven_r}R` : null, status.trail_r > 0 ? `trail ${status.trail_r}R` : null]
                  .filter(Boolean)
                  .join(' · ')
              : 'bracket only'
          }
        />
        <Field
          label="Model calls"
          value={
            budget
              ? `${budget.hourCalls}/${budget.hourLimit || '∞'} h · ${budget.dayCalls}/${budget.dayLimit || '∞'} d`
              : '—'
          }
        />
        <Field
          label="Jev usage"
          value={
            jevUsage && jevUsage.calls > 0
              ? `${jevUsage.calls} calls · ${compactTokens(
                  jevUsage.inputTokens + jevUsage.outputTokens,
                )} tok${jevUsage.failures > 0 ? ` · ${jevUsage.failures} failed` : ''}`
              : '—'
          }
        />
      </div>
      <div className="border-t border-slate-800/80 px-3 py-2 text-[11px] leading-relaxed text-slate-500">
        Every entry carries both stops, passes the deterministic risk gate, and still needs both armed switches
        before the terminal can place it.
      </div>
    </Panel>
  )
}

/* ---------- risk ---------- */

/** Editable mirror of the live policy; numbers stay strings until save. */
type PolicyDraft = {
  killSwitch: boolean
  symbols: string
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
}

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
]

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

function draftFromPolicy(policy: RiskPolicy): PolicyDraft {
  return {
    killSwitch: policy.killSwitch,
    symbols: policy.symbols.join(', '),
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
  }
}

/** Parses the draft into a patch, or returns the first input error. */
function patchFromDraft(draft: PolicyDraft): { patch?: RiskPolicyPatch; error?: string } {
  const patch: RiskPolicyPatch = {
    killSwitch: draft.killSwitch,
    symbols: draft.symbols
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
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState<PolicyDraft>()
  const [error, setError] = useState<string>()
  const [saving, setSaving] = useState(false)

  const startEditing = (current: RiskPolicy) => {
    setDraft(draftFromPolicy(current))
    setError(undefined)
    setEditing(true)
  }
  const cancelEditing = () => {
    setEditing(false)
    setError(undefined)
  }
  /** One handler for every editor input; the field name rides on the element. */
  const handleField = (event: ChangeEvent<HTMLInputElement>) => {
    const key = event.target.dataset.field as keyof PolicyDraft
    const value =
      event.target.type === 'checkbox' ? event.target.checked : event.target.value
    setDraft((current) => (current ? { ...current, [key]: value } : current))
  }
  const save = async () => {
    if (!draft || !onApply) return
    const { patch, error: inputError } = patchFromDraft(draft)
    if (!patch) {
      setError(inputError ?? 'invalid policy')
      return
    }
    setSaving(true)
    setError(undefined)
    const failure = await onApply(patch)
    setSaving(false)
    if (failure) {
      setError(failure)
    } else {
      setEditing(false)
    }
  }

  const detail = editing ? (
    <span className="flex items-center gap-2">
      {error ? <span className="text-rose-400">{error}</span> : null}
      <button
        type="button"
        onClick={cancelEditing}
        className="rounded border border-slate-700 px-1.5 py-0.5 text-[10px] uppercase tracking-wider text-slate-400 hover:border-slate-500"
      >
        cancel
      </button>
      <button
        type="button"
        onClick={() => void save()}
        disabled={saving}
        className="rounded border border-emerald-600/60 px-1.5 py-0.5 text-[10px] uppercase tracking-wider text-emerald-300 hover:border-emerald-400 disabled:opacity-50"
      >
        {saving ? 'saving…' : 'save'}
      </button>
    </span>
  ) : (
    <span className="flex items-center gap-2">
      {policy?.killSwitch ? <span className="text-rose-400">kill switch on</span> : 'gate active'}
      {onApply && policy ? (
        <button
          type="button"
          onClick={() => startEditing(policy)}
          className="rounded border border-slate-700 px-1.5 py-0.5 text-[10px] uppercase tracking-wider text-slate-400 hover:border-slate-500"
        >
          edit
        </button>
      ) : null}
    </span>
  )

  if (editing && draft) {
    const inputClass =
      'w-full rounded border border-slate-700 bg-slate-900 px-1.5 py-1 font-mono text-[11px] text-slate-200'
    const labelClass = 'flex flex-col gap-1'
    return (
      <Panel
        title="Risk"
        detail={detail}
        className="min-h-[320px]"
      >
        <div className="grid grid-cols-1 gap-x-4 gap-y-3 p-3 sm:grid-cols-2">
          <label className={labelClass}>
            <span className="text-[10px] uppercase tracking-wider text-slate-500">Symbols (comma separated)</span>
            <input
              className={inputClass}
              data-field="symbols"
              value={draft.symbols}
              onChange={handleField}
            />
          </label>
          <label className={labelClass}>
            <span className="text-[10px] uppercase tracking-wider text-slate-500">Session UTC (8-17, empty = always open)</span>
            <input
              className={inputClass}
              data-field="sessionUtc"
              value={draft.sessionUtc}
              onChange={handleField}
            />
          </label>
          {POLICY_NUMBER_FIELDS.map((field) => (
            <label key={field.key} className={labelClass}>
              <span className="text-[10px] uppercase tracking-wider text-slate-500">{field.label}</span>
              <input
                className={inputClass}
                data-field={field.key}
                value={draft[field.key]}
                onChange={handleField}
              />
            </label>
          ))}
          <label className="flex items-center gap-2 pt-1">
            <input
              type="checkbox"
              data-field="killSwitch"
              checked={draft.killSwitch}
              onChange={handleField}
            />
            <span className="text-[10px] uppercase tracking-wider text-slate-400">
              Kill switch (refuses every new intent)
            </span>
          </label>
        </div>
        <div className="border-t border-slate-800/80 px-3 py-2 text-[11px] leading-relaxed text-slate-500">
          Changes apply to the live gate immediately and are journaled with the resulting policy. Restarting the
          service restores the environment defaults.
        </div>
      </Panel>
    )
  }

  return (
    <Panel
      title="Risk"
      detail={detail}
    >
      <div className="grid grid-cols-2 gap-x-4 gap-y-3 p-3 sm:grid-cols-4">
        <Field
          label="Symbols"
          value={policy ? (policy.symbols.length > 0 ? policy.symbols.join(' · ') : 'none allowed') : '—'}
        />
        <Field label="Max / order" value={policy ? `${policy.maxVolumePerOrder} lots` : '—'} />
        <Field label="Max total" value={policy ? `${policy.maxTotalLots} lots` : '—'} />
        <Field label="Max open" value={policy?.maxOpenOrders ?? '—'} />
        <Field label="Duplicates" value={policy ? `${policy.duplicateWindowSecs}s window` : '—'} />
        <Field
          label="Max risk"
          value={policy ? (policy.maxRiskPercent > 0 ? `${policy.maxRiskPercent}% / trade` : 'off') : '—'}
        />
        <Field
          label="Brakes"
          value={
            policy
              ? [
                  policy.maxDailyLossPercent > 0 ? `day ${policy.maxDailyLossPercent}%` : null,
                  policy.maxPeakDrawdownPercent > 0 ? `peak ${policy.maxPeakDrawdownPercent}%` : null,
                ]
                  .filter(Boolean)
                  .join(' · ') || 'off'
              : '—'
          }
        />
        <Field
          label="Net USD"
          value={policy ? (policy.maxNetFactorLots > 0 ? `${policy.maxNetFactorLots} lots` : 'off') : '—'}
        />
        <Field
          label="News"
          value={
            policy
              ? policy.calendarBlackoutMinutes > 0
                ? `${policy.calendarBlackoutMinutes}m blackout`
                : 'off'
              : '—'
          }
        />
        <Field label="Session UTC" value={policy?.sessionUtc ?? 'always open'} />
        <Field
          label="Execution"
          value={status?.trading_enabled ? 'switch on' : 'switch off'}
          tone={status?.trading_enabled ? 'text-amber-300' : undefined}
        />
        <Field
          label="Terminal"
          value={status?.ea_live_orders ? 'armed' : 'disarmed'}
          tone={status?.ea_live_orders ? 'text-emerald-300' : undefined}
        />
      </div>
      <div className="border-t border-slate-800/80 px-3 py-2 text-[11px] leading-relaxed text-slate-500">
        Every intent passes the gate in order: kill switch, allowlist, entry window, session, per-order cap,
        account facts, permission, drawdown brakes, one-per-asset, order cap, exposure, per-trade risk, net
        exposure, duplicates. Approved entries then pass the venue contract check and the news blackout.
      </div>
    </Panel>
  )
}

/* ---------- metrics ---------- */

export function MetricsPanel({ metrics, error }: { metrics?: Metrics; error?: string }) {
  const counters = metrics
    ? Object.entries(metrics.counters)
        .sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]))
        .slice(0, 12)
    : []
  return (
    <Panel
      title="Metrics"
      detail={metrics ? `feed #${metrics.feedLatest}` : error ? <span className="text-rose-400">{error}</span> : '…'}
    >
      {counters.length === 0 ? (
        <div className="p-3 text-xs text-slate-500">No counters yet.</div>
      ) : (
        <ul className="divide-y divide-slate-800/40">
          {counters.map(([key, value]) => (
            <li key={key} className="flex items-center justify-between gap-3 px-3 py-1.5 text-xs">
              <span className="truncate font-mono text-[10px] text-slate-400" title={key}>
                {key}
              </span>
              <span className="shrink-0 font-mono tabular-nums text-slate-200">{value}</span>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  )
}

/* ---------- commands ---------- */

export function CommandsPanel({ commands }: { commands?: CommandRecord[] }) {
  const [expandedId, setExpandedId] = useState<string | undefined>(undefined)
  return (
    <Panel title="Commands" detail={`${commands?.length ?? 0} recent`}>
      {!commands || commands.length === 0 ? (
        <div className="p-3 text-xs text-slate-500">No commands yet.</div>
      ) : (
        <ul className="divide-y divide-slate-800/60">
          {commands.map((command) => {
            const expanded = command.id === expandedId
            return (
              <li key={command.id}>
                <button
                  type="button"
                  onClick={() => setExpandedId(expanded ? undefined : command.id)}
                  aria-expanded={expanded}
                  className={`flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left text-xs transition-colors ${
                    expanded ? 'bg-slate-800/50' : 'hover:bg-slate-800/20'
                  }`}
                >
                  <span className="flex min-w-0 items-center gap-2">
                    <span className="rounded bg-slate-800/80 px-1.5 py-0.5 font-mono text-[10px] text-slate-300">
                      {command.kind}
                    </span>
                    <span className="truncate font-mono text-[10px] text-slate-500">{command.id.slice(0, 8)}</span>
                    {command.summary ? (
                      <span className="truncate font-mono text-[10px] text-slate-400">
                        {JSON.stringify(command.summary)}
                      </span>
                    ) : null}
                    {command.reason ? <span className="truncate text-[10px] text-rose-300">{command.reason}</span> : null}
                  </span>
                  <span className={`shrink-0 text-[10px] uppercase tracking-wider ${commandTone[command.status]}`}>
                    {command.status}
                  </span>
                </button>
                {expanded ? (
                  <div className="space-y-1 border-t border-slate-800/60 bg-slate-900/50 px-3 py-2">
                    <div className="font-mono text-[10px] text-slate-500">id {command.id}</div>
                    <pre className="max-h-40 overflow-auto rounded bg-black/30 p-2 font-mono text-[10px] leading-relaxed text-slate-400">
                      {JSON.stringify({ summary: command.summary ?? null, reason: command.reason ?? null }, null, 2)}
                    </pre>
                  </div>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
    </Panel>
  )
}

/* ---------- activity feed ---------- */

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

  return (
    <Panel
      title="Activity"
      className="min-h-[320px]"
      detail={
        <span className="flex items-center gap-3">
          <span className="hidden text-[10px] uppercase tracking-wider text-slate-600 sm:inline">
            click a row for detail
          </span>
          <button
            type="button"
            onClick={() => onFocusChange(!focus)}
            className={`rounded border px-1.5 py-0.5 text-[10px] uppercase tracking-wider transition-colors ${
              focus
                ? 'border-slate-700 text-slate-300 hover:border-slate-500'
                : 'border-amber-500/40 text-amber-300'
            }`}
            title={focus ? 'Show routine snapshots and reads' : 'Hide routine snapshots and reads'}
          >
            {focus ? 'focus' : 'all'}
          </button>
          <span className="flex items-center gap-1.5">
            <span className={`size-1.5 rounded-full ${connected ? 'animate-pulse bg-emerald-400' : 'bg-rose-400'}`} />
            {connected ? 'streaming' : 'reconnecting…'}
          </span>
        </span>
      }
    >
      {visible.length === 0 ? (
        <div className="p-3 text-xs text-slate-500">
          {events.length === 0 ? 'Waiting for events…' : 'No decisions yet — routine activity hidden.'}
        </div>
      ) : (
        <ul className="divide-y divide-slate-800/40">
          {visible.map((event) => {
            const outcome = typeof event.payload?.outcome === 'string' ? event.payload.outcome : undefined
            const expanded = event.seq === selectedSeq
            return (
              <li key={event.seq}>
                <button
                  type="button"
                  onClick={() => setSelectedSeq(expanded ? undefined : event.seq)}
                  aria-expanded={expanded}
                  title={JSON.stringify(event.payload)}
                  className={`flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs transition-colors ${
                    expanded ? 'bg-slate-800/50' : 'hover:bg-slate-800/20'
                  }`}
                >
                  <span className="w-14 shrink-0 pt-0.5 font-mono text-[10px] text-slate-500">
                    {clockTime(event.at_ms)}
                  </span>
                  <span
                    className={`shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] ${
                      kindTone[event.kind] ?? 'bg-slate-700/30 text-slate-300'
                    }`}
                  >
                    {event.kind}
                  </span>
                  <span
                    className={`min-w-0 flex-1 truncate font-mono text-[11px] ${
                      outcome ? (outcomeTone[outcome] ?? 'text-slate-300') : 'text-slate-300'
                    }`}
                  >
                    {payloadSummary(event)}
                  </span>
                </button>
                {expanded ? (
                  <div className="space-y-1.5 border-t border-slate-800/60 bg-slate-900/50 px-3 py-2">
                    <div className="flex items-center justify-between font-mono text-[10px] text-slate-500">
                      <span>
                        #{event.seq} · {new Date(event.at_ms).toISOString()}
                      </span>
                      <span>{relativeTime(event.at_ms)}</span>
                    </div>
                    <div className="space-y-1">
                      {detailRows(event.payload ?? {}).map((row) => (
                        <div key={row.label} className="grid grid-cols-[8rem_1fr] gap-x-3">
                          <span className="font-mono text-[10px] uppercase tracking-wider text-slate-500">
                            {row.label}
                          </span>
                          <span className="min-w-0 break-words font-mono text-[11px] text-slate-300">{row.value}</span>
                        </div>
                      ))}
                    </div>
                    <pre className="max-h-40 overflow-auto rounded bg-black/30 p-2 font-mono text-[10px] leading-relaxed text-slate-400">
                      {JSON.stringify(event.payload ?? {}, null, 2)}
                    </pre>
                  </div>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
    </Panel>
  )
}

/* ---------- agent log ---------- */

const logLevelTone: Record<string, string> = {
  error: 'text-rose-300 bg-rose-500/15',
  warn: 'text-amber-300 bg-amber-500/15',
  info: 'text-sky-300 bg-sky-500/10',
  debug: 'text-slate-400 bg-slate-700/30',
  trace: 'text-slate-500 bg-slate-700/20',
}

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
  return (
    <Panel
      title="Agent log"
      className="min-h-[280px]"
      detail={
        <span className="flex items-center gap-1">
          {LOG_LEVELS.map((candidate) => (
            <button
              key={candidate}
              type="button"
              onClick={() => onLevelChange(candidate)}
              className={`rounded border px-1.5 py-0.5 text-[10px] uppercase tracking-wider transition-colors ${
                candidate === level
                  ? 'border-slate-500 text-slate-200'
                  : 'border-slate-800 text-slate-500 hover:border-slate-600'
              }`}
            >
              {candidate}
            </button>
          ))}
        </span>
      }
    >
      {error ? <div className="px-3 py-2 text-[11px] text-rose-400">{error}</div> : null}
      {ordered.length === 0 ? (
        <div className="p-3 text-xs text-slate-500">{error ? 'Log tail unavailable.' : 'No log records yet.'}</div>
      ) : (
        <ul className="divide-y divide-slate-800/40">
          {ordered.map((record) => (
            <li key={record.seq} className="flex items-start gap-2 px-3 py-1 text-xs">
              <span className="w-14 shrink-0 pt-0.5 font-mono text-[10px] text-slate-500">
                {clockTime(record.atMs)}
              </span>
              <span
                className={`shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] uppercase ${
                  logLevelTone[record.level] ?? 'bg-slate-700/30 text-slate-300'
                }`}
              >
                {record.level}
              </span>
              <span className="min-w-0 flex-1 font-mono text-[11px] leading-relaxed text-slate-300">
                <span className="text-slate-500">{record.target}</span>{' '}
                <span>{record.message}</span>
                {Object.keys(record.fields).length > 0 ? (
                  <span className="text-slate-500"> {JSON.stringify(record.fields)}</span>
                ) : null}
              </span>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  )
}
