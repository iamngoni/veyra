/**
 * Typed client for the Veyra control surface.
 *
 * Requests go through the dev-server proxy at /api, so the browser stays
 * same-origin while the service keeps listening on loopback only.
 */

export type AutopilotStatus = {
  enabled: boolean
  interval_secs: number
  timeframe: string
  tier: string
  bars: number
  /** First configured instrument, or null when the chart symbol is used. */
  symbol: string | null
  /** Instruments the loop rotates through; empty means the chart symbol. */
  symbols: string[]
  jev: string
  /** Break-even multiple of the entry risk; zero when disabled. */
  breakeven_r: number
  /** Trailing distance in multiples of the entry risk; zero when disabled. */
  trail_r: number
  /** Ordered fallback models for the tier in use; empty when none are set. */
  model_fallbacks?: string[]
  /** Deterministic early-profit ratchet; null when disabled. */
  profit_harvest?: {
    arm_r: number
    trail_r: number
    min_profit: number
    giveback_fraction: number
    min_hold_secs: number
    reentry_cooldown_secs: number
  } | null
}

export type Status = {
  service: string
  version: string
  environment: string
  broker_provider: string | null
  market_provider: string | null
  model_provider: string | null
  jev_provider: string | null
  persistence: string | null
  broker_connected: boolean
  trading_enabled: boolean
  ea_live_orders: boolean
  autopilot: AutopilotStatus | null
  /** Model call usage against the configured caps; null without a model. */
  model_budget: { hourLimit: number; hourCalls: number; dayLimit: number; dayCalls: number } | null
  /** Judge usage as this service observed it; null without a judge. */
  jev_usage: { calls: number; failures: number; inputTokens: number; outputTokens: number } | null
  /** Effective risk gate policy; always present. */
  risk_policy: RiskPolicy
  /**
   * Whether decisions are completing. Every other field can read healthy while
   * a provider refuses every request, so this is the only signal separating
   * "nothing worth trading" from "nothing can be decided".
   */
  decisions: {
    consecutiveFailures: number
    lastFailure: string | null
    lastFailureAt: number | null
  } | null
}

export type RiskPolicy = {
  killSwitch: boolean
  symbols: string[]
  /** Allowed symbols whose venue trades through the standard FX weekend. */
  weekendSymbols?: string[]
  maxVolumePerOrder: number
  maxTotalLots: number
  maxOpenOrders: number
  duplicateWindowSecs: number
  sessionUtc: string | null
  /** Per-trade risk cap as a percentage of equity (0 disables). */
  maxRiskPercent: number
  /** Daily-loss breaker, percent below the day's opening equity. */
  maxDailyLossPercent: number
  /** Peak-drawdown breaker, percent below the lifetime peak. */
  maxPeakDrawdownPercent: number
  /** Cap on net USD-directional exposure in lots (0 disables). */
  maxNetFactorLots: number
  /** News blackout either side of a high-impact event, in minutes (0 disables). */
  calendarBlackoutMinutes: number
  /** Minimum stop distance as a fraction of ATR(14) (0 disables). */
  minStopAtrFraction: number
  /**
   * Whether a tick may continue when the semantic judge is unavailable.
   * False — the default — pauses new decisions until the judge answers again.
   */
  allowTradingWithoutJev: boolean
  /** What happens to open positions in the final hours before Friday's close. */
  weekendPositions: WeekendPositions
}

export type Metrics = {
  service: string
  version: string
  counters: Record<string, number>
  feedLatest: number
}

export type Position = {
  ticket: number
  symbol: string
  kind: 'buy' | 'sell'
  lots: number
  price: number
  profit: number
  /** Stop loss as an absolute price, zero when the position carries none. */
  sl: number
  /** Take profit as an absolute price, zero when the position carries none. */
  tp: number
  /** Swap charged or credited so far, in account currency (absent on older terminals). */
  swap?: number
  /** Commission charged or credited so far, in account currency (absent on older terminals). */
  commission?: number
  /**
   * Price the position would close at now. Zero or absent when the terminal
   * does not report it, which is also when the break-even policy is skipped.
   */
  current?: number
  magic: number
}

export type Account = {
  fresh: boolean
  connected: boolean
  tradeAllowed: boolean
  liveOrders: boolean
  login?: number
  server?: string
  symbol?: string
  ageSecs: number
  balance?: number
  equity?: number
  freeMargin?: number
  /** Margin level percentage (equity / used margin x 100); 0 when unused. */
  marginLevel?: number
  /** Account leverage (for example 100 for 1:100); 0 when unreported. */
  leverage?: number
  orders?: number
  lots?: number
  positions?: Position[]
  positionsTruncated?: boolean
  serverTime?: number
}

export type FeedEvent = {
  seq: number
  at_ms: number
  kind: string
  payload: Record<string, unknown>
}

export type Feed = { events: FeedEvent[]; latest: number; next: number }

/** Per-symbol slice of the realized-performance window. */
export type SymbolPerformance = {
  symbol: string
  trades: number
  wins: number
  net_profit: number
}

/** Aggregated realized performance over closed Veyra trades. */
export type PerformanceReport = {
  trades: number
  wins: number
  losses: number
  breakeven: number
  win_rate_percent: number
  net_profit: number
  gross_profit: number
  gross_loss: number
  profit_factor: number | null
  average_win: number | null
  average_loss: number | null
  expectancy: number | null
  best_trade: number | null
  worst_trade: number | null
  by_symbol: SymbolPerformance[]
}

/** One closed order from the venue's account history. */
export type ClosedTrade = {
  ticket: number
  symbol: string
  kind: 'buy' | 'sell'
  lots: number
  openPrice: number
  closePrice: number
  openTime: number
  closeTime: number
  profit: number
  swap: number
  commission: number
  magic: number
}

/** The standard trading week and our entry policy, from /market/sessions. */
/** How open positions are treated as the week closes. */
export type WeekendPositions = 'agent' | 'hold' | 'flatten'

export type MarketSessions = {
  now: number
  market: {
    state: 'open' | 'rollover' | 'closed'
    nextEvent: 'opens' | 'closes' | 'pauses' | 'resumes'
    nextAt: number
  }
  entries: {
    open: boolean
    blockedBy: string | null
    detail: string | null
  }
  policy: {
    rolloverBlackout: { startMinute: number; endMinute: number }
    fridayEntryCutoffMinute: number
    sundayEntryOpenMinute: number
  }
  weekend: {
    policy: WeekendPositions
    /** Seconds until Friday's close while the checkpoint window is open; null outside it. */
    closesInSecs: number | null
  }
}

/** Realized performance response for one lookback window. */
export type Performance = {
  days: number
  report: PerformanceReport
  trades: ClosedTrade[]
  total: number
  truncated: boolean
}

/** Levels the service log tail accepts, most severe first. */
export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'trace'

export const LOG_LEVELS: LogLevel[] = ['error', 'warn', 'info', 'debug', 'trace']

export type LogRecord = {
  seq: number
  atMs: number
  level: string
  target: string
  message: string
  fields: Record<string, unknown>
}

export type LogTail = { logs: LogRecord[]; latest: number }

/** One durable audit row, as the trail stored it. */
export type AuditRecord = {
  /** Row identity, a UUID rather than a sequence number. */
  id: string
  /** Postgres timestamp text, for example `2026-09-18 17:47:46.844116+00`. */
  at: string
  kind: string
  payload: Record<string, unknown> | null
}

export type AuditPage = {
  status: 'ok' | 'disabled' | 'unavailable'
  provider?: string
  events: AuditRecord[]
  error?: string
}

/** Partial update to the live risk policy; omitted fields keep their value. */
export type RiskPolicyPatch = {
  killSwitch?: boolean
  symbols?: string[]
  weekendSymbols?: string[]
  maxVolumePerOrder?: number
  maxTotalLots?: number
  maxOpenOrders?: number
  duplicateWindowSecs?: number
  sessionUtc?: string
  maxRiskPercent?: number
  maxDailyLossPercent?: number
  maxPeakDrawdownPercent?: number
  maxNetFactorLots?: number
  calendarBlackoutMinutes?: number
  minStopAtrFraction?: number
  allowTradingWithoutJev?: boolean
  weekendPositions?: WeekendPositions
}

export type CommandRecord = {
  id: string
  kind: string
  status: 'pending' | 'completed' | 'failed'
  summary: Record<string, unknown> | null
  reason: string | null
}

export type Candle = {
  time: number
  open: number
  high: number
  low: number
  close: number
  volume: number
}

export type CandleSeries = { symbol: string; timeframe: string; candles: Candle[] }

export type Reconciliation = {
  status: string
  accountAgeSecs?: number
  lots?: number
  orders?: number
  positionsTruncated?: boolean
  unknownTickets?: number[]
  positions?: Array<Record<string, unknown>>
}

/**
 * One live setting as the service reports it.
 *
 * `overridden` separates a value an operator chose from one still coming from
 * the deployment's environment, so the console can show what has drifted from
 * the baseline rather than presenting every field as a decision someone made.
 */
export type LiveSetting = { value: string; overridden: boolean }

export type RuntimeConfig = {
  settings: Record<string, LiveSetting>
  /** Sections whose edits take effect without a restart. */
  live_sections: string[]
}

/**
 * Partial update to the live settings, keyed by environment-variable name.
 *
 * `null` clears an override, returning that setting to whatever the
 * environment says — the only way back to the startup baseline.
 */
export type RuntimeConfigPatch = Record<string, string | number | boolean | null>

const BASE = '/api'

async function get<T>(path: string, signal?: AbortSignal): Promise<T> {
  const response = await fetch(`${BASE}${path}`, {
    signal,
    headers: { accept: 'application/json' },
  })
  if (!response.ok) {
    throw new Error(`${path} → ${response.status}`)
  }
  return (await response.json()) as T
}

async function post<T>(path: string, body: unknown): Promise<T> {
  const response = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify(body),
  })
  if (!response.ok) {
    let detail = `${path} → ${response.status}`
    try {
      const payload = (await response.json()) as {
        field?: string
        reason?: string
        rejected?: Array<{ field: string; reason: string }>
      }
      if (payload.field && payload.reason) detail = `${payload.field}: ${payload.reason}`
      // A settings patch reports every bad field at once, so the message names
      // all of them rather than only the first.
      else if (payload.rejected?.length) {
        detail = payload.rejected.map((edit) => `${edit.field}: ${edit.reason}`).join('; ')
      }
    } catch {
      // Keep the status-only detail when the body is not JSON.
    }
    throw new Error(detail)
  }
  return (await response.json()) as T
}

export const api = {
  status: () => get<Status>('/status'),
  account: () => get<Account>('/account'),
  reconciliation: () => get<Reconciliation>('/reconciliation'),
  metrics: () => get<Metrics>('/metrics'),
  commands: (limit = 25) => get<{ commands: CommandRecord[] }>(`/commands?limit=${limit}`),
  candles: (bars = 48) => get<CandleSeries>(`/market/candles?timeframe=H4&bars=${bars}`),
  performance: (days = 30) => get<Performance>(`/performance?days=${days}`),
  sessions: () => get<MarketSessions>('/market/sessions'),
  events: (after: number | undefined, waitMs = 15000) =>
    get<Feed>(after === undefined ? '/events' : `/events?after=${after}&wait_ms=${waitMs}`),
  logs: (after: number | undefined, level: LogLevel, limit = 300) =>
    get<LogTail>(`/logs?limit=${limit}&level=${level}${after === undefined ? '' : `&after=${after}`}`),
  /** Durable trail, newest first. Survives restarts, unlike the log ring. */
  audit: (limit = 200) => get<AuditPage>(`/audit?limit=${limit}`),
  updatePolicy: (patch: RiskPolicyPatch) => post<RiskPolicy>('/risk/policy', patch),
  config: () => get<RuntimeConfig>('/config'),
  updateConfig: (patch: RuntimeConfigPatch) =>
    post<{ changed: string[]; settings: Record<string, LiveSetting> }>('/config', patch),
}

export const VEYRA_MAGIC = 77041
