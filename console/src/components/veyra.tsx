import { useState } from 'react'
import type { ChangeEvent } from 'react'
import type { ReactNode } from 'react'

import type {
  Account,
  AuditPage,
  Candle,
  CandleSeries,
  CommandRecord,
  FeedEvent,
  LogLevel,
  LogRecord,
  MarketSessions,
  Metrics,
  Performance,
  Position,
  RiskPolicy,
  Status,
  WeekendPositions,
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
import { auditTimeMs, clockTime, money, relativeTime, usePaged } from '../lib/hooks'

/* ---------- primitives ---------- */

type Tone = 'ok' | 'warn' | 'bad' | 'off' | 'info'

const toneText: Record<Tone, string> = {
  ok: 'text-[var(--color-ok)]',
  warn: 'text-[var(--color-warn)]',
  bad: 'text-[var(--color-bad)]',
  off: 'text-[var(--color-ink-faint)]',
  info: 'text-[var(--color-info)]',
}

const toneDot: Record<Tone, string> = {
  ok: 'bg-[var(--color-ok)]',
  warn: 'bg-[var(--color-warn)]',
  bad: 'bg-[var(--color-bad)]',
  off: 'bg-[var(--color-line-strong)]',
  info: 'bg-[var(--color-info)]',
}

export function Pill({ tone, label, value }: { tone: Tone; label: string; value?: string }) {
  return (
    <div className="flex items-center gap-2 rounded-md border border-[var(--color-line)] bg-[var(--color-surface-2)] px-2.5 py-1.5">
      <span className={`size-1.5 shrink-0 rounded-full ${toneDot[tone]} ${tone === 'ok' ? 'live-dot' : ''}`} />
      <span className="label">{label}</span>
      {value ? <span className={`readout text-xs font-medium ${toneText[tone]}`}>{value}</span> : null}
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
      className={`flex min-h-0 min-w-0 flex-col overflow-hidden rounded-xl border border-[var(--color-line)] bg-[var(--color-surface-1)] ${className}`}
    >
      <header className="flex items-center justify-between gap-3 border-b border-[var(--color-line)] bg-[var(--color-surface-2)]/40 px-3.5 py-2.5">
        <h2 className="label text-[var(--color-ink-muted)]">{title}</h2>
        {detail ? <div className="text-[11px] text-[var(--color-ink-faint)]">{detail}</div> : null}
      </header>
      <div className="min-h-0 flex-1 overflow-auto">{children}</div>
    </section>
  )
}

function Field({ label, value, tone }: { label: string; value: ReactNode; tone?: string }) {
  return (
    <div className="min-w-0">
      <div className="label">{label}</div>
      <div className={`readout mt-0.5 truncate text-[13px] ${tone ?? 'text-[var(--color-ink)]'}`}>{value}</div>
    </div>
  )
}

/** Placeholder with the same footprint as the value it stands in for. */
export function Skeleton({ className = 'h-4 w-16' }: { className?: string }) {
  return <div className={`skeleton ${className}`} aria-hidden="true" />
}

/**
 * Page control for the long lists. It states the visible range rather than
 * only the page number, because "showing 26–50 of 312" answers the question an
 * operator actually has when scanning a feed.
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
  if (total === 0) return null
  const button =
    'rounded border border-[var(--color-line-strong)] px-2 py-0.5 text-[11px] text-[var(--color-ink-muted)] enabled:hover:border-[var(--color-ink-faint)] disabled:opacity-35'
  return (
    <div className="flex items-center justify-between gap-3 border-t border-[var(--color-line)] px-3 py-1.5">
      <span className="readout text-[11px] text-[var(--color-ink-faint)]">
        {start + 1}–{start + count} of {total}
      </span>
      <span className="flex items-center gap-1.5">
        <button type="button" className={button} onClick={onPrevious} disabled={page === 0} aria-label="Previous page">
          ←
        </button>
        <span className="readout text-[11px] text-[var(--color-ink-faint)]">
          {page + 1}/{pages}
        </span>
        <button
          type="button"
          className={button}
          onClick={onNext}
          disabled={page >= pages - 1}
          aria-label="Next page"
        >
          →
        </button>
      </span>
    </div>
  )
}

/** Sections the dashboard so each view is scannable without scrolling past it. */
export function Tabs({
  tabs,
  active,
  onSelect,
}: {
  tabs: ReadonlyArray<{ id: string; label: string; badge?: number }>
  active: string
  onSelect: (id: string) => void
}) {
  return (
    <div role="tablist" className="flex flex-wrap gap-1 rounded-lg border border-[var(--color-line)] bg-[var(--color-surface-1)] p-1">
      {tabs.map((tab) => {
        const selected = tab.id === active
        return (
          <button
            key={tab.id}
            type="button"
            role="tab"
            aria-selected={selected}
            onClick={() => onSelect(tab.id)}
            className={`flex items-center gap-1.5 rounded-md px-3 py-1.5 text-[12px] font-medium transition-colors ${
              selected
                ? 'bg-[var(--color-surface-3)] text-[var(--color-ink)]'
                : 'text-[var(--color-ink-faint)] hover:text-[var(--color-ink-muted)]'
            }`}
          >
            {tab.label}
            {tab.badge ? (
              <span className="readout rounded bg-[var(--color-surface-2)] px-1 text-[10px] text-[var(--color-ink-faint)]">
                {tab.badge}
              </span>
            ) : null}
          </button>
        )
      })}
    </div>
  )
}

export function ThemeToggle({ theme, onToggle }: { theme: 'dark' | 'light'; onToggle: () => void }) {
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-label={`Switch to ${theme === 'dark' ? 'light' : 'dark'} theme`}
      title={`Switch to ${theme === 'dark' ? 'light' : 'dark'} theme`}
      className="rounded-md border border-[var(--color-line)] bg-[var(--color-surface-2)] px-2 py-1.5 text-[12px] text-[var(--color-ink-muted)] hover:border-[var(--color-line-strong)] hover:text-[var(--color-ink)]"
    >
      {theme === 'dark' ? '☾' : '☀'}
    </button>
  )
}

/* ---------- posture ---------- */

/** The single verdict an operator looks for first: can this thing trade? */
export function systemPosture(status?: Status): {
  label: string
  detail: string
  tone: Tone
} {
  if (!status) {
    return { label: 'CONNECTING', detail: 'reaching the service', tone: 'off' }
  }
  if (status.risk_policy?.killSwitch) {
    return { label: 'HALTED', detail: 'kill switch engaged — every intent refused', tone: 'bad' }
  }
  // Ranked above the link and the switches because it is the failure that
  // looks like health: the service answers, the terminal is live, and nothing
  // is ever decided. Two in a row rules out one transient provider hiccup.
  const failures = status.decisions?.consecutiveFailures ?? 0
  if (failures >= 2) {
    return {
      label: 'NOT DECIDING',
      detail: `${failures} decisions in a row failed — ${status.decisions?.lastFailure ?? 'no reason reported'}`,
      tone: 'bad',
    }
  }
  if (!status.broker_connected) {
    return { label: 'NO LINK', detail: 'terminal is not reporting', tone: 'bad' }
  }
  if (!status.trading_enabled) {
    return { label: 'STANDBY', detail: 'deciding only — nothing will execute', tone: 'off' }
  }
  if (!status.ea_live_orders) {
    return { label: 'DRY RUN', detail: 'service armed, terminal still validating only', tone: 'warn' }
  }
  return { label: 'LIVE', detail: 'orders reach the market', tone: 'ok' }
}

const postureFrame: Record<Tone, string> = {
  ok: 'border-[var(--color-ok)]/40 bg-[var(--color-ok-dim)]',
  warn: 'border-[var(--color-warn)]/40 bg-[var(--color-warn-dim)]',
  bad: 'border-[var(--color-bad)]/50 bg-[var(--color-bad-dim)]',
  off: 'border-[var(--color-line-strong)] bg-[var(--color-surface-2)]',
  info: 'border-[var(--color-info)]/40 bg-[var(--color-info-dim)]',
}

export function PostureBanner({ status }: { status?: Status }) {
  const posture = systemPosture(status)
  return (
    <div
      className={`flex items-center gap-3.5 rounded-xl border px-4 py-3 ${postureFrame[posture.tone]}`}
      role="status"
    >
      <span className={`size-2.5 shrink-0 rounded-full ${toneDot[posture.tone]} ${posture.tone === 'ok' ? 'live-dot' : ''}`} />
      <div className="min-w-0">
        <div className={`text-base font-semibold leading-none tracking-tight ${toneText[posture.tone]}`}>
          {posture.label}
        </div>
        <div className="mt-1 truncate text-[11px] text-[var(--color-ink-muted)]">{posture.detail}</div>
      </div>
    </div>
  )
}

/* ---------- headline numbers ---------- */

/** The four figures that decide whether anything else needs attention. */
export function HeroMetrics({ account, error }: { account?: Account; error?: string }) {
  const positions = account?.positions ?? []
  const open = positions.reduce((sum, position) => sum + (position.profit ?? 0), 0)
  const openTone = open > 0 ? 'text-[var(--color-ok)]' : open < 0 ? 'text-[var(--color-bad)]' : 'text-[var(--color-ink)]'
  const cells: Array<{ label: string; value: ReactNode; tone?: string; hint?: string }> = [
    {
      label: 'Equity',
      value: account?.equity === undefined ? null : money(account.equity),
      hint: account?.balance === undefined ? undefined : `balance ${money(account.balance)}`,
    },
    {
      label: 'Open P/L',
      value: account ? (positions.length > 0 ? `${open >= 0 ? '+' : ''}${open.toFixed(2)}` : '—') : null,
      tone: openTone,
      hint: `${positions.length} position${positions.length === 1 ? '' : 's'}`,
    },
    {
      // A snapshot can arrive before its money fields do, so an absent value
      // reads as pending rather than rendering the word "undefined".
      label: 'Exposure',
      value: account?.lots === undefined ? null : `${account.lots} lots`,
      hint:
        account?.orders === undefined
          ? undefined
          : `${account.orders} order${account.orders === 1 ? '' : 's'}`,
    },
    {
      label: 'Free margin',
      value: account?.freeMargin === undefined ? null : money(account.freeMargin),
      hint: account?.marginLevel != null ? `level ${account.marginLevel.toFixed(0)}%` : undefined,
    },
  ]
  return (
    <div className="grid grid-cols-2 gap-px overflow-hidden rounded-xl border border-[var(--color-line)] bg-[var(--color-line)] lg:grid-cols-4">
      {cells.map((cell) => (
        <div key={cell.label} className="bg-[var(--color-surface-1)] px-4 py-3">
          <div className="label">{cell.label}</div>
          <div className={`readout mt-1.5 text-xl leading-none font-medium ${cell.tone ?? 'text-[var(--color-ink)]'}`}>
            {cell.value ?? (error ? <span className="text-sm text-[var(--color-bad)]">unavailable</span> : <Skeleton className="h-5 w-24" />)}
          </div>
          <div className="mt-1.5 h-3 text-[11px] text-[var(--color-ink-faint)]">{cell.value ? (cell.hint ?? '') : ''}</div>
        </div>
      ))}
    </div>
  )
}

/* ---------- safety controls ---------- */

/**
 * The two switches an operator reaches for in a hurry, promoted out of the
 * policy editor. Both are one click plus a confirm, because a mis-click on
 * either one changes what the bot is allowed to do with real money.
 */
export function SafetyControls({
  policy,
  jevHealthy,
  onApply,
}: {
  policy?: RiskPolicy
  /** Whether the judge answered recently; drives the degraded warning. */
  jevHealthy?: boolean
  onApply?: (patch: RiskPolicyPatch) => Promise<string | undefined>
}) {
  const [pending, setPending] = useState<keyof RiskPolicyPatch>()
  const [error, setError] = useState<string>()
  const [confirming, setConfirming] = useState<keyof RiskPolicyPatch>()

  const submit = async (key: 'killSwitch' | 'allowTradingWithoutJev', next: boolean) => {
    if (!onApply) return
    setPending(key)
    setError(undefined)
    const failure = await onApply({ [key]: next })
    setPending(undefined)
    setConfirming(undefined)
    if (failure) setError(failure)
  }

  const halted = policy?.killSwitch ?? false
  const withoutJev = policy?.allowTradingWithoutJev ?? false
  const degraded = jevHealthy === false

  return (
    <Panel
      title="Controls"
      detail={error ? <span className="text-[var(--color-bad)]">{error}</span> : undefined}
    >
      <div className="flex flex-col gap-px bg-[var(--color-line)]">
        <Switch
          label="Kill switch"
          description={
            halted
              ? 'Engaged. Every new intent is refused; open positions are untouched.'
              : 'Refuse every new intent immediately. Open positions are left alone.'
          }
          checked={halted}
          tone="bad"
          busy={pending === 'killSwitch'}
          disabled={!policy || !onApply}
          confirming={confirming === 'killSwitch'}
          onRequest={() => setConfirming(confirming === 'killSwitch' ? undefined : 'killSwitch')}
          onConfirm={() => void submit('killSwitch', !halted)}
          confirmLabel={halted ? 'Release the kill switch' : 'Halt all new intents'}
        />
        <Switch
          label="Trade without the judge"
          description={
            withoutJev
              ? 'Override active. A judge outage no longer pauses new decisions — the model decides alone.'
              : 'Default: if the judge cannot answer, the tick is abandoned and nothing is proposed.'
          }
          checked={withoutJev}
          tone="warn"
          busy={pending === 'allowTradingWithoutJev'}
          disabled={!policy || !onApply}
          confirming={confirming === 'allowTradingWithoutJev'}
          onRequest={() =>
            setConfirming(confirming === 'allowTradingWithoutJev' ? undefined : 'allowTradingWithoutJev')
          }
          onConfirm={() => void submit('allowTradingWithoutJev', !withoutJev)}
          confirmLabel={withoutJev ? 'Require the judge again' : 'Allow trading without the judge'}
          warning={
            degraded && !withoutJev
              ? 'The judge is not answering — new decisions are paused right now.'
              : degraded && withoutJev
                ? 'The judge is not answering and the override is on: the model is deciding alone.'
                : undefined
          }
        />
      </div>
    </Panel>
  )
}

function Switch({
  label,
  description,
  checked,
  tone,
  busy,
  disabled,
  confirming,
  onRequest,
  onConfirm,
  confirmLabel,
  warning,
}: {
  label: string
  description: string
  checked: boolean
  tone: Tone
  busy: boolean
  disabled: boolean
  confirming: boolean
  onRequest: () => void
  onConfirm: () => void
  confirmLabel: string
  warning?: string
}) {
  return (
    <div className="bg-[var(--color-surface-1)] px-3.5 py-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="text-[13px] font-medium text-[var(--color-ink)]">{label}</span>
            {checked ? (
              <span className={`rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wider ${toneText[tone]} bg-[var(--color-surface-3)]`}>
                on
              </span>
            ) : null}
          </div>
          <p className="mt-1 text-[11px] leading-relaxed text-[var(--color-ink-faint)]">{description}</p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={checked}
          aria-label={label}
          disabled={disabled || busy}
          onClick={onRequest}
          className={`relative mt-0.5 h-5 w-9 shrink-0 rounded-full border transition-colors disabled:opacity-40 ${
            checked
              ? `${toneDot[tone]} border-transparent`
              : 'border-[var(--color-line-strong)] bg-[var(--color-surface-3)]'
          }`}
        >
          <span
            className={`absolute top-0.5 size-3.5 rounded-full bg-[var(--color-surface-0)] transition-all ${
              checked ? 'left-[1.15rem]' : 'left-0.5'
            }`}
          />
        </button>
      </div>
      {warning ? (
        <p className="mt-2 rounded border border-[var(--color-warn)]/30 bg-[var(--color-warn-dim)] px-2 py-1.5 text-[11px] text-[var(--color-warn)]">
          {warning}
        </p>
      ) : null}
      {confirming ? (
        <div className="mt-2 flex items-center justify-between gap-2 rounded border border-[var(--color-line-strong)] bg-[var(--color-surface-2)] px-2 py-1.5">
          <span className="text-[11px] text-[var(--color-ink-muted)]">{confirmLabel}?</span>
          <span className="flex gap-1.5">
            <button
              type="button"
              onClick={onRequest}
              className="rounded border border-[var(--color-line-strong)] px-2 py-0.5 text-[11px] text-[var(--color-ink-muted)] hover:border-[var(--color-ink-faint)]"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={onConfirm}
              disabled={busy}
              className={`rounded px-2 py-0.5 text-[11px] font-medium text-[var(--color-surface-0)] disabled:opacity-50 ${toneDot[tone]}`}
            >
              {busy ? 'Applying…' : 'Confirm'}
            </button>
          </span>
        </div>
      ) : null}
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
            ? <span className="text-[var(--color-bad)]">{error}</span>
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
          tone={totalProfit >= 0 ? 'text-[var(--color-ok)]' : 'text-[var(--color-bad)]'}
        />
      </div>
      <div className="border-t border-[var(--color-line)]/80 px-3 py-2 text-[11px] text-[var(--color-ink-faint)]">
        {account?.server ? `${account.server} · #${account.login} · ` : ''}
        {account?.symbol ?? ''}
      </div>
    </Panel>
  )
}

/**
 * How far price has moved in the position's favour. A sell profits as price
 * falls, so the raw difference is signed against the side rather than reported
 * as a bare price change that would read backwards on half the book.
 */
function favourableMove(position: Position): number {
  if (!position.current) return 0
  return position.kind === 'buy'
    ? position.current - position.price
    : position.price - position.current
}

/**
 * The move as a signed price delta at the venue's own quote precision.
 *
 * Deliberately not pips: pip size differs per instrument class (and this book
 * mixes FX with metals), so a converted figure would be wrong somewhere and
 * silently so. The delta is always true.
 *
 * Precision is the widest seen across the row's prices, because any single one
 * can be short a digit — an entry that happens to land on 1.3 says nothing
 * about how finely the instrument is quoted, and rounding to it would report a
 * real move as no move at all.
 */
function quotedDecimals(position: Position): number {
  const prices = [position.price, position.current, position.sl, position.tp]
  const widest = prices.reduce<number>((most, price) => {
    if (!price) return most
    return Math.max(most, (String(price).split('.')[1] ?? '').length)
  }, 0)
  return widest || 2
}

function formatMove(position: Position): string {
  const move = favourableMove(position)
  return `${move >= 0 ? '+' : ''}${move.toFixed(quotedDecimals(position))}`
}

export function PositionsPanel({ account }: { account?: Account }) {
  const positions = account?.positions ?? []
  return (
    <Panel
      title="Positions"
      detail={account?.positionsTruncated ? <span className="text-[var(--color-warn)]">truncated</span> : `${positions.length}`}
    >
      {positions.length === 0 ? (
        <div className="p-3 text-xs text-[var(--color-ink-faint)]">Flat — no open orders.</div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-left text-xs">
            <thead className="text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)]">
              <tr className="border-b border-[var(--color-line)]/80">
                <th className="px-3 py-1.5 font-medium">Ticket</th>
                <th className="px-2 py-1.5 font-medium">Symbol</th>
                <th className="px-2 py-1.5 font-medium">Side</th>
                <th className="px-2 py-1.5 font-medium">Lots</th>
                <th className="px-2 py-1.5 font-medium">Entry</th>
                <th className="px-2 py-1.5 font-medium">Current</th>
                <th className="px-2 py-1.5 font-medium">SL</th>
                <th className="px-2 py-1.5 font-medium">TP</th>
                <th className="px-2 py-1.5 font-medium">Swap</th>
                <th className="px-2 py-1.5 font-medium">P/L</th>
                <th className="px-3 py-1.5 font-medium">Owner</th>
              </tr>
            </thead>
            <tbody className="font-mono tabular-nums">
              {positions.map((position: Position) => (
                <tr key={position.ticket} className="border-b border-[var(--color-line)]/40">
                  <td className="px-3 py-1.5 text-[var(--color-ink)]">{position.ticket}</td>
                  <td className="px-2 py-1.5 font-semibold text-[var(--color-ink)]">{position.symbol}</td>
                  <td className={`px-2 py-1.5 ${position.kind === 'buy' ? 'text-[var(--color-ok)]' : 'text-[var(--color-bad)]'}`}>
                    {position.kind}
                  </td>
                  <td className="px-2 py-1.5 text-[var(--color-ink)]">{position.lots}</td>
                  <td className="px-2 py-1.5 text-[var(--color-ink)]">{position.price}</td>
                  <td className="px-2 py-1.5 text-[var(--color-ink)]">
                    {position.current ? (
                      <>
                        {position.current}
                        {/* The move only means anything with a direction, so it
                            is signed against the side rather than the price. */}
                        <span
                          className={`ml-1.5 text-[10px] ${
                            favourableMove(position) >= 0
                              ? 'text-[var(--color-ok)]'
                              : 'text-[var(--color-bad)]'
                          }`}
                        >
                          {formatMove(position)}
                        </span>
                      </>
                    ) : (
                      '—'
                    )}
                  </td>
                  <td className="px-2 py-1.5 text-[var(--color-bad)]/80">{position.sl > 0 ? position.sl : '—'}</td>
                  <td className="px-2 py-1.5 text-[var(--color-ok)]/80">{position.tp > 0 ? position.tp : '—'}</td>
                  <td className={`px-2 py-1.5 ${position.swap != null && position.swap < 0 ? 'text-[var(--color-bad)]/80' : 'text-[var(--color-ok)]/80'}`}>
                    {position.swap == null
                      ? '—'
                      : `${position.swap >= 0 ? '+' : ''}${position.swap.toFixed(2)}`}
                  </td>
                  <td className={`px-2 py-1.5 ${position.profit >= 0 ? 'text-[var(--color-ok)]' : 'text-[var(--color-bad)]'}`}>
                    {position.profit >= 0 ? '+' : ''}
                    {position.profit.toFixed(2)}
                  </td>
                  <td className="px-3 py-1.5">
                    {position.magic === VEYRA_MAGIC ? (
                      <span className="rounded bg-violet-500/15 px-1.5 py-0.5 text-[10px] text-violet-300">veyra</span>
                    ) : (
                      <span className="rounded bg-[var(--color-surface-3)]/40 px-1.5 py-0.5 text-[10px] text-[var(--color-ink-muted)]">manual</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  )
}

/* ---------- performance ---------- */

export function PerformancePanel({
  performance,
  error,
}: {
  performance?: Performance
  error?: string
}) {
  const report = performance?.report
  const money = (value: number | null | undefined) =>
    value == null ? '—' : `${value >= 0 ? '+' : ''}${value.toFixed(2)}`
  return (
    <Panel
      title="Performance"
      detail={
        performance
          ? `last ${performance.days}d · ${performance.total} closed${performance.truncated ? ' · truncated' : ''}`
          : error
            ? <span className="text-[var(--color-bad)]">{error}</span>
            : 'waiting…'
      }
    >
      <div className="grid grid-cols-2 gap-x-4 gap-y-3 p-3 sm:grid-cols-3">
        <Field
          label="Win rate"
          value={report && report.trades > 0 ? `${report.win_rate_percent.toFixed(1)}%` : '—'}
          tone={
            report && report.trades > 0 && report.win_rate_percent >= 50
              ? 'text-[var(--color-ok)]'
              : undefined
          }
        />
        <Field
          label="Record"
          value={
            report && report.trades > 0
              ? `${report.wins}W · ${report.losses}L${report.breakeven > 0 ? ` · ${report.breakeven}F` : ''}`
              : '—'
          }
        />
        <Field
          label="Net P/L"
          value={performance ? money(report?.net_profit) : '—'}
          tone={report && report.net_profit >= 0 ? 'text-[var(--color-ok)]' : report ? 'text-[var(--color-bad)]' : undefined}
        />
        <Field
          label="Profit factor"
          value={report?.profit_factor != null ? report.profit_factor.toFixed(2) : '—'}
        />
        <Field label="Avg win" value={money(report?.average_win)} />
        <Field
          label="Avg loss"
          value={report?.average_loss != null ? `-${report.average_loss.toFixed(2)}` : '—'}
        />
      </div>
      {report && report.by_symbol.length > 0 ? (
        <div className="flex flex-wrap gap-x-4 gap-y-1 border-t border-[var(--color-line)]/80 px-3 py-2 font-mono text-[11px] text-[var(--color-ink-muted)]">
          {report.by_symbol.map((entry) => (
            <span key={entry.symbol}>
              {entry.symbol} {entry.wins}/{entry.trades}{' '}
              <span className={entry.net_profit >= 0 ? 'text-[var(--color-ok)]' : 'text-[var(--color-bad)]'}>
                {money(entry.net_profit)}
              </span>
            </span>
          ))}
        </div>
      ) : null}
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

/** UTC clock label for an instant, e.g. `Fri 21:00 UTC`. */
function utcClock(unix: number): string {
  const date = new Date(unix * 1000)
  const weekday = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'][date.getUTCDay()]
  const hh = String(date.getUTCHours()).padStart(2, '0')
  const mm = String(date.getUTCMinutes()).padStart(2, '0')
  return `${weekday} ${hh}:${mm} UTC`
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

const SESSION_EVENT_LABELS: Record<MarketSessions['market']['nextEvent'], string> = {
  opens: 'opens',
  closes: 'closes',
  pauses: 'rollover pause',
  resumes: 'resumes',
}

const ENTRY_BLOCK_LABELS: Record<string, string> = {
  rollover_blackout: 'rollover blackout',
  weekend_approach: 'weekend cutoff',
  weekend_open: 'weekend',
  session_closed: 'session window',
}

/** How the active weekend preference reads on the market line. */
const WEEKEND_POLICY_LABELS: Record<WeekendPositions, string> = {
  agent: 'the analyst settles each open position',
  hold: 'positions stay through the weekend',
  flatten: 'every open position is flattened',
}

export function MarketPanel({
  series,
  sessions,
  account,
  error,
}: {
  series?: CandleSeries
  sessions?: MarketSessions
  /** Positions are surfaced here so the panel names what is exposed when the market is closed. */
  account?: Account
  error?: string
}) {
  const last = series?.candles.at(-1)
  const first = series?.candles.at(0)
  const change = last && first ? ((last.close - first.close) / first.close) * 100 : undefined
  const held = (account?.positions ?? []).map((position) => position.symbol)
  const state = sessions?.market.state
  const stateTone =
    state === 'open' ? 'text-[var(--color-ok)]' : state === 'rollover' ? 'text-amber-400' : 'text-[var(--color-bad)]'
  return (
    <Panel
      title="Market"
      detail={series ? `${series.symbol} ${series.timeframe}` : error ? <span className="text-[var(--color-bad)]">{error}</span> : '…'}
    >
      <div className="p-3">
        {series ? <Sparkline candles={series.candles} /> : <div className="h-16" />}
        <div className="mt-2 grid grid-cols-3 gap-3">
          <Field label="Last close" value={last ? last.close.toFixed(5) : '—'} />
          <Field
            label={`Change ${series?.candles.length ?? 0}b`}
            value={change === undefined ? '—' : `${change >= 0 ? '+' : ''}${change.toFixed(2)}%`}
            tone={change !== undefined && change >= 0 ? 'text-[var(--color-ok)]' : 'text-[var(--color-bad)]'}
          />
          <Field label="H / L" value={last ? `${last.high.toFixed(5)} / ${last.low.toFixed(5)}` : '—'} />
        </div>
      </div>
      {sessions ? (
        <div className="border-t border-[var(--color-line)] px-3 py-2 text-[11px] leading-relaxed text-[var(--color-muted)]">
          <div>
            <span className={`font-semibold uppercase ${stateTone}`}>{state}</span>
            {' · '}
            {SESSION_EVENT_LABELS[sessions.market.nextEvent]} {utcClock(sessions.market.nextAt)} (
            {untilLabel(sessions.market.nextAt, sessions.now)})
          </div>
          <div>
            Entries{' '}
            {sessions.entries.open ? (
              <span className="text-[var(--color-ok)]">open</span>
            ) : (
              <span className="text-amber-400">
                blocked — {ENTRY_BLOCK_LABELS[sessions.entries.blockedBy ?? ''] ?? sessions.entries.blockedBy}
              </span>
            )}
          </div>
          {held.length > 0 ? (
            <div>
              Holding {held.join(' · ')}
              {state !== 'open' ? ' — market closed; stops rest at the broker' : ''}
            </div>
          ) : null}
          {sessions.weekend.closesInSecs !== null ? (
            <div>
              <span className="text-[var(--color-warn)]">Weekend checkpoint</span>
              {' · closes '}
              {utcClock(sessions.now + sessions.weekend.closesInSecs)} (
              {untilLabel(sessions.now + sessions.weekend.closesInSecs, sessions.now)}) {' — '}
              {WEEKEND_POLICY_LABELS[sessions.weekend.policy]}
            </div>
          ) : null}
        </div>
      ) : null}
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
          label="Profit harvest"
          value={
            status?.profit_harvest
              ? `${status.profit_harvest.arm_r}R arm · ${status.profit_harvest.trail_r}R trail · ${status.profit_harvest.min_profit.toFixed(2)} floor`
              : 'off'
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
      <div className="border-t border-[var(--color-line)]/80 px-3 py-2 text-[11px] leading-relaxed text-[var(--color-ink-faint)]">
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
function patchFromDraft(draft: PolicyDraft): { patch?: RiskPolicyPatch; error?: string } {
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
  /** The weekend preference is a choice, not free text. */
  const handleSelect = (event: ChangeEvent<HTMLSelectElement>) => {
    const value = event.target.value as WeekendPositions
    setDraft((current) => (current ? { ...current, weekendPositions: value } : current))
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
      {error ? <span className="text-[var(--color-bad)]">{error}</span> : null}
      <button
        type="button"
        onClick={cancelEditing}
        className="rounded border border-[var(--color-line-strong)] px-1.5 py-0.5 text-[10px] uppercase tracking-wider text-[var(--color-ink-muted)] hover:border-[var(--color-ink-faint)]"
      >
        cancel
      </button>
      <button
        type="button"
        onClick={() => void save()}
        disabled={saving}
        className="rounded border border-[var(--color-ok)]/60 px-1.5 py-0.5 text-[10px] uppercase tracking-wider text-[var(--color-ok)] hover:border-[var(--color-ok)] disabled:opacity-50"
      >
        {saving ? 'saving…' : 'save'}
      </button>
    </span>
  ) : (
    <span className="flex items-center gap-2">
      {policy?.killSwitch ? <span className="text-[var(--color-bad)]">kill switch on</span> : 'gate active'}
      {onApply && policy ? (
        <button
          type="button"
          onClick={() => startEditing(policy)}
          className="rounded border border-[var(--color-line-strong)] px-1.5 py-0.5 text-[10px] uppercase tracking-wider text-[var(--color-ink-muted)] hover:border-[var(--color-ink-faint)]"
        >
          edit
        </button>
      ) : null}
    </span>
  )

  if (editing && draft) {
    const inputClass =
      'w-full rounded border border-[var(--color-line-strong)] bg-[var(--color-surface-2)] px-1.5 py-1 font-mono text-[11px] text-[var(--color-ink)]'
    const labelClass = 'flex flex-col gap-1'
    return (
      <Panel
        title="Risk"
        detail={detail}
        className="min-h-[320px]"
      >
        <div className="grid grid-cols-1 gap-x-4 gap-y-3 p-3 sm:grid-cols-2">
          <label className={labelClass}>
            <span className="text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)]">Symbols (comma separated)</span>
            <input
              className={inputClass}
              data-field="symbols"
              value={draft.symbols}
              onChange={handleField}
            />
          </label>
          <label className={labelClass}>
            <span className="text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)]">
              Weekend-traded symbols (must be allowed above)
            </span>
            <input
              className={inputClass}
              data-field="weekendSymbols"
              value={draft.weekendSymbols}
              onChange={handleField}
            />
          </label>
          <label className={labelClass}>
            <span className="text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)]">Session UTC (8-17, empty = always open)</span>
            <input
              className={inputClass}
              data-field="sessionUtc"
              value={draft.sessionUtc}
              onChange={handleField}
            />
          </label>
          <label className={labelClass}>
            <span className="text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)]">
              Weekend positions (final hours before Friday's close)
            </span>
            <select
              className={inputClass}
              data-field="weekendPositions"
              value={draft.weekendPositions}
              onChange={handleSelect}
            >
              <option value="agent">agent decides per position</option>
              <option value="hold">hold through the weekend</option>
              <option value="flatten">flatten before the close</option>
            </select>
          </label>
          {POLICY_NUMBER_FIELDS.map((field) => (
            <label key={field.key} className={labelClass}>
              <span className="text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)]">{field.label}</span>
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
            <span className="text-[10px] uppercase tracking-wider text-[var(--color-ink-muted)]">
              Kill switch (refuses every new intent)
            </span>
          </label>
          <label className="flex items-center gap-2 pt-1">
            <input
              type="checkbox"
              data-field="allowTradingWithoutJev"
              checked={draft.allowTradingWithoutJev}
              onChange={handleField}
            />
            <span className="text-[10px] uppercase tracking-wider text-[var(--color-ink-muted)]">
              Trade without the judge (otherwise a judge outage pauses decisions)
            </span>
          </label>
        </div>
        <div className="border-t border-[var(--color-line)]/80 px-3 py-2 text-[11px] leading-relaxed text-[var(--color-ink-faint)]">
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
        <Field
          label="Weekend markets"
          value={policy ? ((policy.weekendSymbols ?? []).join(' · ') || 'none') : '—'}
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
        <Field
          label="Stop floor"
          value={
            policy
              ? policy.minStopAtrFraction > 0
                ? `${policy.minStopAtrFraction}\u00d7 ATR`
                : 'off'
              : '—'
          }
        />
        <Field label="Session UTC" value={policy?.sessionUtc ?? 'always open'} />
        <Field
          label="Weekend"
          value={
            policy
              ? {
                  agent: 'analyst decides',
                  hold: 'held through',
                  flatten: 'flattened before close',
                }[policy.weekendPositions]
              : '—'
          }
        />
        <Field
          label="Judge outage"
          value={policy ? (policy.allowTradingWithoutJev ? 'keeps trading' : 'pauses decisions') : '—'}
          tone={policy?.allowTradingWithoutJev ? 'text-[var(--color-warn)]' : undefined}
        />
        <Field
          label="Execution"
          value={status?.trading_enabled ? 'switch on' : 'switch off'}
          tone={status?.trading_enabled ? 'text-[var(--color-warn)]' : undefined}
        />
        <Field
          label="Terminal"
          value={status?.ea_live_orders ? 'armed' : 'disarmed'}
          tone={status?.ea_live_orders ? 'text-[var(--color-ok)]' : undefined}
        />
      </div>
      <div className="border-t border-[var(--color-line)]/80 px-3 py-2 text-[11px] leading-relaxed text-[var(--color-ink-faint)]">
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
      detail={metrics ? `feed #${metrics.feedLatest}` : error ? <span className="text-[var(--color-bad)]">{error}</span> : '…'}
    >
      {counters.length === 0 ? (
        <div className="p-3 text-xs text-[var(--color-ink-faint)]">No counters yet.</div>
      ) : (
        <ul className="divide-y divide-[var(--color-line)]/40">
          {counters.map(([key, value]) => (
            <li key={key} className="flex items-center justify-between gap-3 px-3 py-1.5 text-xs">
              <span className="truncate font-mono text-[10px] text-[var(--color-ink-muted)]" title={key}>
                {key}
              </span>
              <span className="shrink-0 font-mono tabular-nums text-[var(--color-ink)]">{value}</span>
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
  const paged = usePaged(commands ?? [], 8)
  return (
    <Panel title="Commands" detail={`${commands?.length ?? 0} recent`}>
      {!commands || commands.length === 0 ? (
        <div className="p-3 text-xs text-[var(--color-ink-faint)]">No commands yet.</div>
      ) : (
        <ul className="divide-y divide-[var(--color-line)]">
          {paged.items.map((command) => {
            const expanded = command.id === expandedId
            return (
              <li key={command.id}>
                <button
                  type="button"
                  onClick={() => setExpandedId(expanded ? undefined : command.id)}
                  aria-expanded={expanded}
                  className={`flex w-full items-center justify-between gap-3 px-3 py-1.5 text-left text-xs transition-colors ${
                    expanded ? 'bg-[var(--color-surface-3)]/50' : 'hover:bg-[var(--color-surface-3)]/20'
                  }`}
                >
                  <span className="flex min-w-0 items-center gap-2">
                    <span className="rounded bg-[var(--color-surface-3)]/80 px-1.5 py-0.5 font-mono text-[10px] text-[var(--color-ink)]">
                      {command.kind}
                    </span>
                    <span className="truncate font-mono text-[10px] text-[var(--color-ink-faint)]">{command.id.slice(0, 8)}</span>
                    {command.summary ? (
                      <span className="truncate font-mono text-[10px] text-[var(--color-ink-muted)]">
                        {JSON.stringify(command.summary)}
                      </span>
                    ) : null}
                    {command.reason ? <span className="truncate text-[10px] text-[var(--color-bad)]">{command.reason}</span> : null}
                  </span>
                  <span className={`shrink-0 text-[10px] uppercase tracking-wider ${commandTone[command.status]}`}>
                    {command.status}
                  </span>
                </button>
                {expanded ? (
                  <div className="space-y-1 border-t border-[var(--color-line)]/60 bg-[var(--color-surface-2)]/50 px-3 py-2">
                    <div className="font-mono text-[10px] text-[var(--color-ink-faint)]">id {command.id}</div>
                    <pre className="max-h-40 overflow-auto rounded bg-[var(--color-surface-0)] p-2 font-mono text-[10px] leading-relaxed text-[var(--color-ink-muted)]">
                      {JSON.stringify({ summary: command.summary ?? null, reason: command.reason ?? null }, null, 2)}
                    </pre>
                  </div>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
      <Pager
        page={paged.page}
        pages={paged.pages}
        start={paged.start}
        count={paged.items.length}
        total={paged.total}
        onPrevious={paged.previous}
        onNext={paged.next}
      />
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
  const paged = usePaged(visible, 14)

  return (
    <Panel
      title="Activity"
      className="min-h-[320px]"
      detail={
        <span className="flex items-center gap-3">
          <span className="hidden text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)] sm:inline">
            click a row for detail
          </span>
          <button
            type="button"
            onClick={() => onFocusChange(!focus)}
            className={`rounded border px-1.5 py-0.5 text-[10px] uppercase tracking-wider transition-colors ${
              focus
                ? 'border-[var(--color-line-strong)] text-[var(--color-ink)] hover:border-[var(--color-ink-faint)]'
                : 'border-[var(--color-warn)]/40 text-[var(--color-warn)]'
            }`}
            title={focus ? 'Show routine snapshots and reads' : 'Hide routine snapshots and reads'}
          >
            {focus ? 'focus' : 'all'}
          </button>
          <span className="flex items-center gap-1.5">
            <span className={`size-1.5 rounded-full ${connected ? 'animate-pulse bg-[var(--color-ok)]' : 'bg-[var(--color-bad)]'}`} />
            {connected ? 'streaming' : 'reconnecting…'}
          </span>
        </span>
      }
    >
      {visible.length === 0 ? (
        <div className="p-3 text-xs text-[var(--color-ink-faint)]">
          {events.length === 0 ? 'Waiting for events…' : 'No decisions yet — routine activity hidden.'}
        </div>
      ) : (
        <ul className="divide-y divide-[var(--color-line)]">
          {paged.items.map((event) => {
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
                    expanded ? 'bg-[var(--color-surface-3)]/50' : 'hover:bg-[var(--color-surface-3)]/20'
                  }`}
                >
                  <span className="w-14 shrink-0 pt-0.5 font-mono text-[10px] text-[var(--color-ink-faint)]">
                    {clockTime(event.at_ms)}
                  </span>
                  <span
                    className={`shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] ${
                      kindTone[event.kind] ?? 'bg-[var(--color-surface-3)]/30 text-[var(--color-ink)]'
                    }`}
                  >
                    {event.kind}
                  </span>
                  <span
                    className={`min-w-0 flex-1 truncate font-mono text-[11px] ${
                      outcome ? (outcomeTone[outcome] ?? 'text-[var(--color-ink)]') : 'text-[var(--color-ink)]'
                    }`}
                  >
                    {payloadSummary(event)}
                  </span>
                </button>
                {expanded ? (
                  <div className="space-y-1.5 border-t border-[var(--color-line)]/60 bg-[var(--color-surface-2)]/50 px-3 py-2">
                    <div className="flex items-center justify-between font-mono text-[10px] text-[var(--color-ink-faint)]">
                      <span>
                        #{event.seq} · {new Date(event.at_ms).toISOString()}
                      </span>
                      <span>{relativeTime(event.at_ms)}</span>
                    </div>
                    <div className="space-y-1">
                      {detailRows(event.payload ?? {}).map((row) => (
                        <div key={row.label} className="grid grid-cols-[8rem_1fr] gap-x-3">
                          <span className="font-mono text-[10px] uppercase tracking-wider text-[var(--color-ink-faint)]">
                            {row.label}
                          </span>
                          <span className="min-w-0 break-words font-mono text-[11px] text-[var(--color-ink)]">{row.value}</span>
                        </div>
                      ))}
                    </div>
                    <pre className="max-h-40 overflow-auto rounded bg-[var(--color-surface-0)] p-2 font-mono text-[10px] leading-relaxed text-[var(--color-ink-muted)]">
                      {JSON.stringify(event.payload ?? {}, null, 2)}
                    </pre>
                  </div>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
      <Pager
        page={paged.page}
        pages={paged.pages}
        start={paged.start}
        count={paged.items.length}
        total={paged.total}
        onPrevious={paged.previous}
        onNext={paged.next}
      />
    </Panel>
  )
}

/* ---------- durable trace ---------- */

/** Long values are shown whole on demand, not silently clipped in the row. */
function TraceValue({ value }: { value: unknown }) {
  const rendered = typeof value === 'string' ? value : JSON.stringify(value, null, 2)
  return (
    <pre className="max-h-[28rem] overflow-auto rounded bg-[var(--color-surface-0)] p-2 font-mono text-[11px] leading-relaxed whitespace-pre-wrap text-[var(--color-ink-muted)]">
      {rendered}
    </pre>
  )
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

  return (
    <Panel
      title="Trace"
      className="min-h-[320px]"
      detail={
        <span className="flex flex-wrap items-center gap-1">
          {page?.status && page.status !== 'ok' ? (
            <span className="text-[var(--color-warn)]">{page.status}</span>
          ) : null}
          {kinds.slice(0, 8).map((candidate) => (
            <button
              key={candidate}
              type="button"
              onClick={() => onKindChange(candidate)}
              className={`rounded border px-1.5 py-0.5 text-[10px] transition-colors ${
                candidate === kind
                  ? 'border-[var(--color-ink-faint)] text-[var(--color-ink)]'
                  : 'border-[var(--color-line)] text-[var(--color-ink-faint)] hover:border-[var(--color-line-strong)]'
              }`}
            >
              {candidate}
            </button>
          ))}
        </span>
      }
    >
      {error ? <div className="px-3 py-2 text-[11px] text-[var(--color-bad)]">{error}</div> : null}
      {visible.length === 0 ? (
        <div className="p-3 text-xs text-[var(--color-ink-faint)]">
          {error ? 'Trail unavailable.' : 'No durable events recorded yet.'}
        </div>
      ) : (
        <ul className="divide-y divide-[var(--color-line)]">
          {paged.items.map((row) => {
            const open = row.id === openId
            const payload = row.payload ?? {}
            const outcome = typeof payload.outcome === 'string' ? payload.outcome : undefined
            return (
              <li key={row.id}>
                <button
                  type="button"
                  onClick={() => setOpenId(open ? undefined : row.id)}
                  aria-expanded={open}
                  className={`flex w-full items-start gap-2 px-3 py-1.5 text-left text-xs transition-colors ${
                    open ? 'bg-[var(--color-surface-3)]/50' : 'hover:bg-[var(--color-surface-3)]/20'
                  }`}
                >
                  <span className="readout w-14 shrink-0 pt-0.5 text-[10px] text-[var(--color-ink-faint)]">
                    {Number.isNaN(auditTimeMs(row.at)) ? '—' : clockTime(auditTimeMs(row.at))}
                  </span>
                  <span
                    className={`readout shrink-0 rounded px-1.5 py-0.5 text-[10px] ${
                      kindTone[row.kind] ?? 'bg-[var(--color-surface-3)]/30 text-[var(--color-ink)]'
                    }`}
                  >
                    {row.kind}
                  </span>
                  <span className="readout min-w-0 flex-1 truncate text-[11px] text-[var(--color-ink-muted)]">
                    {outcome ? `${outcome} · ` : ''}
                    {Object.keys(payload).length} fields
                  </span>
                  <span
                    className="readout shrink-0 text-[10px] text-[var(--color-ink-faint)]"
                    title={row.id}
                  >
                    {row.id.slice(0, 8)}
                  </span>
                </button>
                {open ? (
                  <div className="space-y-2 border-t border-[var(--color-line)] bg-[var(--color-surface-2)]/40 px-3 py-2">
                    {Object.entries(payload).map(([field, value]) => (
                      <div key={field}>
                        <div className="label mb-0.5">{field}</div>
                        <TraceValue value={value} />
                      </div>
                    ))}
                  </div>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
      <Pager
        page={paged.page}
        pages={paged.pages}
        start={paged.start}
        count={paged.items.length}
        total={paged.total}
        onPrevious={paged.previous}
        onNext={paged.next}
      />
    </Panel>
  )
}

/* ---------- agent log ---------- */

const logLevelTone: Record<string, string> = {
  error: 'text-[var(--color-bad)] bg-[var(--color-bad)]/15',
  warn: 'text-[var(--color-warn)] bg-[var(--color-warn)]/15',
  info: 'text-[var(--color-info)] bg-[var(--color-info)]/10',
  debug: 'text-[var(--color-ink-muted)] bg-[var(--color-surface-3)]/30',
  trace: 'text-[var(--color-ink-faint)] bg-[var(--color-surface-3)]/20',
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
  const paged = usePaged(ordered, 20)
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
                  ? 'border-[var(--color-ink-faint)] text-[var(--color-ink)]'
                  : 'border-[var(--color-line)] text-[var(--color-ink-faint)] hover:border-[var(--color-line-strong)]'
              }`}
            >
              {candidate}
            </button>
          ))}
        </span>
      }
    >
      {error ? <div className="px-3 py-2 text-[11px] text-[var(--color-bad)]">{error}</div> : null}
      {ordered.length === 0 ? (
        <div className="p-3 text-xs text-[var(--color-ink-faint)]">
          {error ? 'Log tail unavailable.' : 'No log records yet.'}
        </div>
      ) : (
        <ul className="divide-y divide-[var(--color-line)]">
          {paged.items.map((record) => (
            <li key={record.seq} className="flex items-start gap-2 px-3 py-1 text-xs">
              <span className="w-14 shrink-0 pt-0.5 font-mono text-[10px] text-[var(--color-ink-faint)]">
                {clockTime(record.atMs)}
              </span>
              <span
                className={`shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] uppercase ${
                  logLevelTone[record.level] ?? 'bg-[var(--color-surface-3)]/30 text-[var(--color-ink)]'
                }`}
              >
                {record.level}
              </span>
              <span className="min-w-0 flex-1 font-mono text-[11px] leading-relaxed text-[var(--color-ink)]">
                <span className="text-[var(--color-ink-faint)]">{record.target}</span>{' '}
                <span>{record.message}</span>
                {Object.keys(record.fields).length > 0 ? (
                  <span className="text-[var(--color-ink-faint)]"> {JSON.stringify(record.fields)}</span>
                ) : null}
              </span>
            </li>
          ))}
        </ul>
      )}
      <Pager
        page={paged.page}
        pages={paged.pages}
        start={paged.start}
        count={paged.items.length}
        total={paged.total}
        onPrevious={paged.previous}
        onNext={paged.next}
      />
    </Panel>
  )
}
