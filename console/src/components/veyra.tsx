import type { ReactNode } from 'react'

import type { Account, Candle, CandleSeries, CommandRecord, FeedEvent, Position, Status } from '../lib/api'
import { VEYRA_MAGIC } from '../lib/api'
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

export function AutopilotPanel({ status }: { status?: Status['autopilot'] }) {
  const on = status?.enabled === true
  return (
    <Panel title="Autopilot" detail={on ? 'deciding on cadence' : 'disabled'}>
      <div className="grid grid-cols-2 gap-x-4 gap-y-3 p-3">
        <Field label="Cadence" value={on && status ? `${status.interval_secs}s` : '—'} />
        <Field label="Timeframe" value={status?.timeframe ?? '—'} />
        <Field label="Window" value={on && status ? `${status.bars} bars` : '—'} />
        <Field label="Model tier" value={status?.tier ?? '—'} />
        <Field label="Judgements" value={status?.jev ?? '—'} />
        <Field label="Symbol" value={status?.symbol ?? 'chart symbol'} />
      </div>
      <div className="border-t border-slate-800/80 px-3 py-2 text-[11px] leading-relaxed text-slate-500">
        Every entry carries both stops, passes the deterministic risk gate, and still needs both armed switches
        before the terminal can place it.
      </div>
    </Panel>
  )
}

/* ---------- commands ---------- */

const commandTone: Record<CommandRecord['status'], string> = {
  pending: 'text-sky-400',
  completed: 'text-emerald-400',
  failed: 'text-rose-400',
}

export function CommandsPanel({ commands }: { commands?: CommandRecord[] }) {
  return (
    <Panel title="Commands" detail={`${commands?.length ?? 0} recent`}>
      {!commands || commands.length === 0 ? (
        <div className="p-3 text-xs text-slate-500">No commands yet.</div>
      ) : (
        <ul className="divide-y divide-slate-800/60">
          {commands.map((command) => (
            <li key={command.id} className="flex items-center justify-between gap-3 px-3 py-1.5 text-xs">
              <div className="flex min-w-0 items-center gap-2">
                <span className="rounded bg-slate-800/80 px-1.5 py-0.5 font-mono text-[10px] text-slate-300">
                  {command.kind}
                </span>
                <span className="truncate font-mono text-[10px] text-slate-500">{command.id.slice(0, 8)}</span>
                {command.summary ? (
                  <span className="truncate font-mono text-[10px] text-slate-400">{JSON.stringify(command.summary)}</span>
                ) : null}
                {command.reason ? <span className="truncate text-[10px] text-rose-300">{command.reason}</span> : null}
              </div>
              <span className={`shrink-0 text-[10px] uppercase tracking-wider ${commandTone[command.status]}`}>
                {command.status}
              </span>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  )
}

/* ---------- activity feed ---------- */

const kindTone: Record<string, string> = {
  proposal_evaluated: 'text-violet-300 bg-violet-500/10',
  command_queued: 'text-sky-300 bg-sky-500/10',
  command_completed: 'text-emerald-300 bg-emerald-500/10',
  command_failed: 'text-rose-300 bg-rose-500/10',
  broker_snapshot: 'text-slate-400 bg-slate-700/30',
  position_closed: 'text-cyan-300 bg-cyan-500/10',
  service_started: 'text-amber-300 bg-amber-500/10',
  reconciliation_drift: 'text-rose-300 bg-rose-500/15',
}

const outcomeTone: Record<string, string> = {
  queued: 'text-emerald-300',
  approved_dry_run: 'text-cyan-300',
  no_trade: 'text-slate-400',
  rejected: 'text-amber-300',
  unavailable: 'text-rose-300',
}

function payloadSummary(event: FeedEvent): string {
  const payload = event.payload ?? {}
  if (event.kind === 'proposal_evaluated') {
    const parts = [payload.outcome, payload.side, payload.volume, payload.reason].filter(Boolean)
    return parts.join(' · ')
  }
  if (event.kind === 'broker_snapshot') {
    return `orders=${payload.orders} lots=${payload.lots}`
  }
  if (event.kind === 'position_closed') {
    const profit = Number(payload.profit ?? 0)
    return `ticket ${payload.ticket} ${payload.symbol} ${payload.kind} · P/L ${profit >= 0 ? '+' : ''}${profit.toFixed(2)}`
  }
  return JSON.stringify(payload)
}

export function ActivityFeed({ events, connected }: { events: FeedEvent[]; connected: boolean }) {
  return (
    <Panel
      title="Activity"
      className="min-h-[320px]"
      detail={
        <span className="flex items-center gap-1.5">
          <span className={`size-1.5 rounded-full ${connected ? 'animate-pulse bg-emerald-400' : 'bg-rose-400'}`} />
          {connected ? 'streaming' : 'reconnecting…'}
        </span>
      }
    >
      {events.length === 0 ? (
        <div className="p-3 text-xs text-slate-500">Waiting for events…</div>
      ) : (
        <ul className="divide-y divide-slate-800/40">
          {events.map((event) => {
            const outcome = typeof event.payload?.outcome === 'string' ? event.payload.outcome : undefined
            return (
              <li key={event.seq} className="flex items-start gap-2 px-3 py-1.5 text-xs">
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
                  title={JSON.stringify(event.payload)}
                >
                  {payloadSummary(event)}
                </span>
              </li>
            )
          })}
        </ul>
      )}
    </Panel>
  )
}
