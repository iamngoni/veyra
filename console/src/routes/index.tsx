import { useEffect, useRef, useState } from 'react'

import { createFileRoute } from '@tanstack/react-router'

import {
  AccountPanel,
  ActivityFeed,
  AutopilotPanel,
  BalanceHistoryPanel,
  CommandsPanel,
  HeroMetrics,
  LiveSettingsPanel,
  LogsPanel,
  MarketPanel,
  MetricsPanel,
  PerformancePanel,
  PositionsPanel,
  RecentActivityPreview,
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
 * Operator views retain the existing controls while the account-balance
 * history leads the overview.
 */
const TABS = [
  { id: 'overview', label: 'Overview' },
  { id: 'market', label: 'Market' },
  { id: 'activity', label: 'Activity' },
  { id: 'risk', label: 'Risk' },
  { id: 'trace', label: 'Trace' },
  { id: 'diagnostics', label: 'Diagnostics' },
  { id: 'settings', label: 'Settings' },
] as const

type TabId = (typeof TABS)[number]['id']

export function Dashboard() {
  const { data: status, refetch: refetchStatus } = usePoll(api.status, 5000)
  const { data: account, error: accountError } = usePoll(api.account, 5000)
  const { data: commands } = usePoll(() => api.commands(25), 10000)
  const { data: performance, error: performanceError } = usePoll(api.performance, 30000)
  const { data: sessions } = usePoll(api.sessions, 30000)
  const { data: series, error: marketError } = usePoll(() => api.candles(48), 60000)
  const { data: balanceHistory, error: balanceHistoryError } = usePoll(() => api.balanceHistory(30), 30000)
  const { data: metrics, error: metricsError } = usePoll(api.metrics, 10000)
  const { events, connected } = useEventFeed(200)
  const [focus, setFocus] = useState(true)
  const [logLevel, setLogLevel] = useState<LogLevel>('info')
  const { logs, error: logsError } = useLogFeed(logLevel)
  const { data: audit, error: auditError } = usePoll(() => api.audit(200), 15000)
  // Settings change only when someone changes them, so this polls slowly and
  // is refetched immediately after an edit.
  const { data: liveConfig, refetch: refetchConfig } = usePoll(api.config, 60000)
  const [traceKind, setTraceKind] = useState('all')
  const { theme, toggle } = useTheme()
  const [tab, setTab] = useState<TabId>('overview')
  const lastDecisionAt = events.find((event) => event.kind === 'proposal_evaluated')?.at_ms

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

  const applyConfig = async (patch: Parameters<typeof api.updateConfig>[0]) => {
    try {
      await api.updateConfig(patch)
      // A settings edit can move the autopilot and the execution switch, both
      // of which the header reads, so refresh status alongside the settings.
      void refetchStatus()
      return undefined
    } catch (error) {
      return error instanceof Error ? error.message : String(error)
    }
  }

  return (
    <div className="console-shell">
      <header className="console-topbar">
        <div className="console-brand">
          <span className="console-wordmark">VEYRA</span>
        </div>
        <div className="console-topbar-meta">
          <StatusPills status={status} />
        </div>
        <ThemeToggle theme={theme} onToggle={toggle} />
      </header>

      <div className="console-body">
        <aside className="console-sidebar" aria-label="Console navigation">
          <Tabs tabs={TABS} active={tab} onSelect={(id) => setTab(id as TabId)} />
        </aside>

        <main className="console-main">
          <div className="console-page-heading">
            <h1>{TABS.find((item) => item.id === tab)?.label ?? 'Overview'}</h1>
          </div>

          {tab === 'overview' ? (
            <div className="overview-layout">
              <div className="overview-primary">
                <HeroMetrics account={account} error={accountError} />
                <BalanceHistoryPanel history={balanceHistory} account={account} error={balanceHistoryError} />
                <PositionsPanel account={account} />
                <PerformancePanel performance={performance} error={performanceError} />
              </div>
              <aside className="overview-rail">
                <AutopilotPanel
                  status={status?.autopilot}
                  budget={status?.model_budget}
                  jevUsage={status?.jev_usage}
                  decisions={status?.decisions}
                  compact
                  lastDecisionAt={lastDecisionAt}
                />
                <RecentActivityPreview events={events} connected={connected} onViewAll={() => setTab('activity')} />
                <SafetyControls policy={status?.risk_policy} jevHealthy={jevHealthy} onApply={applyPatch} />
              </aside>
            </div>
          ) : null}

          {tab === 'market' ? (
            <div className="single-tab-layout">
              <MarketPanel series={series} sessions={sessions} account={account} error={marketError} />
            </div>
          ) : null}

          {tab === 'activity' ? (
            <div className="console-tab-grid">
              <ActivityFeed events={events} connected={connected} focus={focus} onFocusChange={setFocus} />
              <CommandsPanel commands={commands?.commands} />
            </div>
          ) : null}

          {tab === 'settings' ? (
            <LiveSettingsPanel
              settings={liveConfig?.settings}
              onApply={applyConfig}
              onRefresh={() => void refetchConfig()}
            />
          ) : null}

          {tab === 'risk' ? (
            <div className="console-tab-grid">
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
              <MetricsPanel metrics={metrics} status={status} error={metricsError} />
              <LogsPanel logs={logs} error={logsError} level={logLevel} onLevelChange={setLogLevel} />
            </div>
          ) : null}

        </main>
      </div>
    </div>
  )
}
