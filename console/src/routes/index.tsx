import { useEffect, useRef, useState } from 'react'

import { createFileRoute } from '@tanstack/react-router'

import {
  AccountPanel,
  ActivityFeed,
  AutopilotPanel,
  CommandsPanel,
  HeroMetrics,
  LogsPanel,
  MarketPanel,
  MetricsPanel,
  PerformancePanel,
  PositionsPanel,
  PostureBanner,
  RiskPanel,
  SafetyControls,
  StatusPills,
  TracePanel,
  Tabs,
  ThemeToggle,
} from '../components/veyra'
import { api, type LogLevel } from '../lib/api'
import { useEventFeed, useLogFeed, usePoll, useTheme } from '../lib/hooks'

export const Route = createFileRoute('/')({ component: Dashboard })

/**
 * Sections, not one scroll. Posture, money and the safety switches stay pinned
 * above the tabs because they are the answer to "is anything wrong"; anything
 * that is read deliberately rather than at a glance lives behind a tab.
 */
const TABS = [
  { id: 'overview', label: 'Overview' },
  { id: 'activity', label: 'Activity' },
  { id: 'risk', label: 'Risk' },
  { id: 'trace', label: 'Trace' },
  { id: 'diagnostics', label: 'Diagnostics' },
] as const

type TabId = (typeof TABS)[number]['id']

export function Dashboard() {
  const { data: status, refetch: refetchStatus } = usePoll(api.status, 5000)
  const { data: account, error: accountError } = usePoll(api.account, 5000)
  const { data: commands } = usePoll(() => api.commands(25), 10000)
  const { data: performance, error: performanceError } = usePoll(api.performance, 30000)
  const { data: sessions } = usePoll(api.sessions, 30000)
  const { data: series, error: marketError } = usePoll(() => api.candles(48), 60000)
  const { data: metrics, error: metricsError } = usePoll(api.metrics, 10000)
  const { events, connected } = useEventFeed(200)
  const [focus, setFocus] = useState(true)
  const [logLevel, setLogLevel] = useState<LogLevel>('info')
  const { logs, error: logsError } = useLogFeed(logLevel)
  const { data: audit, error: auditError } = usePoll(() => api.audit(200), 15000)
  const [traceKind, setTraceKind] = useState('all')
  const { theme, toggle } = useTheme()
  const [tab, setTab] = useState<TabId>('overview')

  // Judge health is not reported directly, so it is inferred from the failure
  // counter moving between polls. Cumulative totals alone cannot say whether
  // the judge is failing *now*, which is the only thing the warning means.
  const lastJev = useRef<{ failures: number; calls: number } | undefined>(undefined)
  const [jevHealthy, setJevHealthy] = useState<boolean>()
  useEffect(() => {
    const usage = status?.jev_usage
    if (!usage) {
      setJevHealthy(undefined)
      lastJev.current = undefined
      return
    }
    const previous = lastJev.current
    if (previous) {
      const failed = usage.failures > previous.failures
      const answered = usage.calls - usage.failures > previous.calls - previous.failures
      if (failed || answered) setJevHealthy(!failed)
    }
    lastJev.current = { failures: usage.failures, calls: usage.calls }
  }, [status?.jev_usage])

  const applyPatch = async (patch: Parameters<typeof api.updatePolicy>[0]) => {
    try {
      await api.updatePolicy(patch)
      // Reflect the new policy at once instead of leaving a stale reading on
      // screen until the next poll.
      void refetchStatus()
      return undefined
    } catch (error) {
      return error instanceof Error ? error.message : String(error)
    }
  }

  return (
    <div className="mx-auto flex min-h-screen w-full max-w-[1560px] flex-col gap-4 p-4 lg:p-6">
      <header className="flex flex-wrap items-center justify-between gap-x-6 gap-y-3">
        <div className="flex items-baseline gap-3">
          <span className="text-[15px] font-semibold tracking-[0.16em] text-[var(--color-ink)]">VEYRA</span>
          <span className="readout text-[11px] text-[var(--color-ink-faint)]">
            v{status?.version ?? '…'}
            {status?.model_provider ? ` · ${status.model_provider}` : ''}
            {status?.jev_provider ? ` + ${status.jev_provider}` : ''}
          </span>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <StatusPills status={status} />
          <ThemeToggle theme={theme} onToggle={toggle} />
        </div>
      </header>

      <div className="grid gap-4 lg:grid-cols-[minmax(260px,1fr)_3fr]">
        <PostureBanner status={status} />
        <HeroMetrics account={account} error={accountError} />
      </div>

      <SafetyControls policy={status?.risk_policy} jevHealthy={jevHealthy} onApply={applyPatch} />

      <Tabs tabs={TABS} active={tab} onSelect={(id) => setTab(id as TabId)} />

      {tab === 'overview' ? (
        <div className="grid items-start gap-4 lg:grid-cols-2">
          <div className="flex min-w-0 flex-col gap-4">
            <PositionsPanel account={account} />
            <AutopilotPanel
              status={status?.autopilot}
              budget={status?.model_budget}
              jevUsage={status?.jev_usage}
            />
          </div>
          <div className="flex min-w-0 flex-col gap-4">
            <MarketPanel series={series} sessions={sessions} account={account} error={marketError} />
            <PerformancePanel performance={performance} error={performanceError} />
          </div>
        </div>
      ) : null}

      {tab === 'activity' ? (
        <div className="grid flex-1 items-start gap-4 lg:grid-cols-2">
          <ActivityFeed events={events} connected={connected} focus={focus} onFocusChange={setFocus} />
          <CommandsPanel commands={commands?.commands} />
        </div>
      ) : null}

      {tab === 'risk' ? (
        <div className="grid items-start gap-4 lg:grid-cols-2">
          <RiskPanel policy={status?.risk_policy} status={status} onApply={applyPatch} />
          <AccountPanel account={account} error={accountError} />
        </div>
      ) : null}

      {tab === 'trace' ? (
        <TracePanel
          page={audit}
          error={auditError}
          kind={traceKind}
          onKindChange={setTraceKind}
        />
      ) : null}

      {tab === 'diagnostics' ? (
        <div className="flex flex-col gap-4">
          <MetricsPanel metrics={metrics} error={metricsError} />
          <LogsPanel logs={logs} error={logsError} level={logLevel} onLevelChange={setLogLevel} />
        </div>
      ) : null}

      <footer className="pb-1 text-center text-[11px] text-[var(--color-ink-faint)]">
        loopback console · {status?.broker_provider ?? '—'} broker · {status?.market_provider ?? '—'} market ·{' '}
        {status?.persistence ?? '—'} audit
      </footer>
    </div>
  )
}
