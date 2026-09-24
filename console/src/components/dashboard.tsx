/**
 * The console dashboard: polls the control surface, owns view state (tab,
 * chart mode and range, theme) and lays out the areas. Presentation lives in
 * the area modules; this file only wires data to them.
 */

import { useEffect, useRef, useState } from 'react'

import { ChartPanel, MARKET_BARS, type ChartMode, type MarketTimeframe, type PerformanceRange } from './chart'
import { KpiRow, OpenPositions, PerformanceSummary } from './overview'
import { AutopilotCard, RecentActivity, RiskControls } from './rail'
import { Sidebar, Topbar, type NavTab } from './shell'
import {
  AccountPanel,
  ActivityFeed,
  AutopilotPanel,
  CommandsPanel,
  LiveSettingsPanel,
  LogsPanel,
  MetricsPanel,
  RiskPanel,
  SessionPanel,
  TracePanel,
} from './veyra'
import { api, type LogLevel } from '../lib/api'
import { useEventFeed, useLogFeed, usePoll, useTheme } from '../lib/hooks'

const TABS = [
  { id: 'overview', label: 'Overview', icon: 'overview' },
  { id: 'activity', label: 'Activity', icon: 'activity' },
  { id: 'risk', label: 'Risk', icon: 'risk' },
  { id: 'trace', label: 'Trace', icon: 'trace' },
  { id: 'diagnostics', label: 'Diagnostics', icon: 'diagnostics' },
  { id: 'settings', label: 'Settings', icon: 'settings' },
] as const satisfies ReadonlyArray<NavTab>

type TabId = (typeof TABS)[number]['id']

export function Dashboard() {
  const { data: status, refetch: refetchStatus } = usePoll(api.status, 5000)
  const { data: account, error: accountError } = usePoll(api.account, 5000)
  const { data: commands } = usePoll(() => api.commands(25), 10000)
  const { data: performance, error: performanceError } = usePoll(api.performance, 30000)
  // The growth chart and today's realized change read the longest window once
  // rather than one venue history request per selected range.
  const { data: history, error: historyError } = usePoll(() => api.performance(365), 60000)
  const { data: sessions } = usePoll(api.sessions, 30000)
  const { data: metrics, error: metricsError } = usePoll(api.metrics, 10000)
  const { events, notable, connected, settled } = useEventFeed(200)
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

  const [chartMode, setChartMode] = useState<ChartMode>('performance')
  const [chartRange, setChartRange] = useState<PerformanceRange>(30)
  const [timeframe, setTimeframe] = useState<MarketTimeframe>('H4')
  const [symbol, setSymbol] = useState<string>()
  const {
    data: series,
    error: marketError,
    refetch: refetchSeries,
  } = usePoll(() => api.candles(MARKET_BARS, timeframe, symbol), 60000)
  // A new instrument or timeframe must not wait out the poll interval. The
  // first render is skipped: the poll's own first tick already covers it.
  const seriesKey = useRef(`${symbol ?? ''}|${timeframe}`)
  useEffect(() => {
    const key = `${symbol ?? ''}|${timeframe}`
    if (seriesKey.current === key) return
    seriesKey.current = key
    void refetchSeries()
  }, [symbol, timeframe, refetchSeries])

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

  // The year-long window is a slow venue request; ranges it does not need are
  // drawn from the 30-day window as soon as that one answers.
  const chartTrades = history ?? (chartRange <= 30 ? performance : undefined)

  const symbols = status?.autopilot?.symbols.length
    ? status.autopilot.symbols
    : status?.autopilot?.symbol
      ? [status.autopilot.symbol]
      : []

  return (
    <div className="app">
      <Topbar status={status} theme={theme} onToggleTheme={toggle} onOpenSettings={() => setTab('settings')} />

      <div className="app-body">
        <Sidebar tabs={TABS} active={tab} onSelect={(id) => setTab(id as TabId)} />

        <main className="app-main">
          <h1 className="page-title">{TABS.find((item) => item.id === tab)?.label ?? 'Overview'}</h1>

          {tab === 'overview' ? (
            <div className="overview">
              <div className="overview-main">
                <KpiRow account={account} error={accountError} trades={performance?.trades ?? history?.trades} />
                <ChartPanel
                  mode={chartMode}
                  onModeChange={setChartMode}
                  range={chartRange}
                  onRangeChange={setChartRange}
                  timeframe={timeframe}
                  onTimeframeChange={setTimeframe}
                  symbol={symbol}
                  symbols={symbols}
                  onSymbolChange={setSymbol}
                  trades={chartTrades?.trades}
                  tradesTruncated={chartTrades?.truncated}
                  tradesError={historyError}
                  balance={account?.balance}
                  series={series}
                  seriesError={marketError}
                />
                <OpenPositions account={account} harvestEnabled={Boolean(status?.autopilot?.profit_harvest)} />
                <PerformanceSummary performance={performance} error={performanceError} balance={account?.balance} />
              </div>
              <aside className="overview-rail" aria-label="Autopilot and controls">
                <AutopilotCard status={status} events={notable} loading={!settled} />
                {/* Before the first answer there is nothing to reconnect to. */}
                <RecentActivity
                  events={notable}
                  trades={performance?.trades ?? history?.trades}
                  connected={connected || !settled}
                  loading={!settled && !(performance ?? history)}
                  onViewAll={() => setTab('activity')}
                />
                <RiskControls policy={status?.risk_policy} jevHealthy={jevHealthy} onApply={applyPatch} />
              </aside>
            </div>
          ) : null}

          {tab === 'activity' ? (
            <div className="tab-grid">
              <ActivityFeed events={events} connected={connected} focus={focus} onFocusChange={setFocus} />
              <CommandsPanel commands={commands?.commands} />
            </div>
          ) : null}

          {tab === 'risk' ? (
            <div className="tab-stack">
              <RiskPanel policy={status?.risk_policy} status={status} onApply={applyPatch} />
              <div className="tab-grid">
                <AccountPanel account={account} error={accountError} />
                <SessionPanel sessions={sessions} account={account} />
              </div>
            </div>
          ) : null}

          {tab === 'trace' ? (
            <TracePanel page={audit} error={auditError} kind={traceKind} onKindChange={setTraceKind} />
          ) : null}

          {tab === 'diagnostics' ? (
            <div className="tab-stack">
              <div className="tab-grid">
                <AutopilotPanel
                  status={status?.autopilot}
                  budget={status?.model_budget}
                  jevUsage={status?.jev_usage}
                  decisions={status?.decisions}
                />
                <MetricsPanel metrics={metrics} status={status} error={metricsError} />
              </div>
              <LogsPanel logs={logs} error={logsError} level={logLevel} onLevelChange={setLogLevel} />
            </div>
          ) : null}

          {tab === 'settings' ? (
            <LiveSettingsPanel
              settings={liveConfig?.settings}
              onApply={applyConfig}
              onRefresh={() => void refetchConfig()}
            />
          ) : null}
        </main>
      </div>
    </div>
  )
}
