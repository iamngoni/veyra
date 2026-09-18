import { useState } from 'react'

import { createFileRoute } from '@tanstack/react-router'

import {
  AccountPanel,
  ActivityFeed,
  AutopilotPanel,
  CommandsPanel,
  LogsPanel,
  MarketPanel,
  MetricsPanel,
  PositionsPanel,
  RiskPanel,
  StatusPills,
} from '../components/veyra'
import { api, type LogLevel } from '../lib/api'
import { useEventFeed, useLogFeed, usePoll } from '../lib/hooks'

export const Route = createFileRoute('/')({ component: Dashboard })

export function Dashboard() {
  const { data: status } = usePoll(api.status, 5000)
  const { data: account, error: accountError } = usePoll(api.account, 5000)
  const { data: commands } = usePoll(() => api.commands(25), 10000)
  const { data: series, error: marketError } = usePoll(() => api.candles(48), 60000)
  const { data: metrics, error: metricsError } = usePoll(api.metrics, 10000)
  const { events, connected } = useEventFeed(80)
  const [focus, setFocus] = useState(true)
  const [logLevel, setLogLevel] = useState<LogLevel>('info')
  const { logs, error: logsError } = useLogFeed(logLevel)

  return (
    <div className="mx-auto flex min-h-screen w-full max-w-[1500px] flex-col gap-3 p-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-baseline gap-3">
          <span className="text-lg font-semibold tracking-tight text-slate-100">VEYRA</span>
          <span className="font-mono text-[11px] text-slate-500">
            v{status?.version ?? '…'}
            {status?.model_provider ? ` · ${status.model_provider}` : ''}
            {status?.jev_provider ? ` + ${status.jev_provider}` : ''}
          </span>
        </div>
        <StatusPills status={status} />
      </header>

      <div className="grid gap-3 lg:grid-cols-3">
        <div className="min-w-0">
          <AccountPanel account={account} error={accountError} />
        </div>
        <div className="min-w-0">
          <MarketPanel series={series} error={marketError} />
        </div>
        <div className="min-w-0">
          <AutopilotPanel status={status?.autopilot} budget={status?.model_budget} jevUsage={status?.jev_usage} />
        </div>
      </div>

      <div className="grid flex-1 items-start gap-3 lg:grid-cols-2">
        <div className="min-w-0">
          <ActivityFeed events={events} connected={connected} focus={focus} onFocusChange={setFocus} />
        </div>
        <div className="flex min-w-0 flex-col gap-3">
          <PositionsPanel account={account} />
          <CommandsPanel commands={commands?.commands} />
        </div>
      </div>

      <div className="grid gap-3 lg:grid-cols-2">
        <RiskPanel
          policy={status?.risk_policy}
          status={status}
          onApply={async (patch) => {
            try {
              await api.updatePolicy(patch)
              return undefined
            } catch (error) {
              return error instanceof Error ? error.message : String(error)
            }
          }}
        />
        <MetricsPanel metrics={metrics} error={metricsError} />
      </div>

      <LogsPanel logs={logs} error={logsError} level={logLevel} onLevelChange={setLogLevel} />

      <footer className="pb-1 text-center font-mono text-[10px] text-slate-600">
        loopback console · {status?.broker_provider ?? '—'} broker · {status?.market_provider ?? '—'} market ·{' '}
        {status?.persistence ?? '—'} audit
      </footer>
    </div>
  )
}
