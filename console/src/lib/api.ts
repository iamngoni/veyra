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
}

export type RiskPolicy = {
  killSwitch: boolean
  symbols: string[]
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

/** Partial update to the live risk policy; omitted fields keep their value. */
export type RiskPolicyPatch = {
  killSwitch?: boolean
  symbols?: string[]
  maxVolumePerOrder?: number
  maxTotalLots?: number
  maxOpenOrders?: number
  duplicateWindowSecs?: number
  sessionUtc?: string
  maxRiskPercent?: number
  maxDailyLossPercent?: number
  maxPeakDrawdownPercent?: number
  maxNetFactorLots?: number
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
      const payload = (await response.json()) as { field?: string; reason?: string }
      if (payload.field && payload.reason) detail = `${payload.field}: ${payload.reason}`
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
  events: (after: number | undefined, waitMs = 15000) =>
    get<Feed>(after === undefined ? '/events' : `/events?after=${after}&wait_ms=${waitMs}`),
  logs: (after: number | undefined, level: LogLevel, limit = 300) =>
    get<LogTail>(`/logs?limit=${limit}&level=${level}${after === undefined ? '' : `&after=${after}`}`),
  updatePolicy: (patch: RiskPolicyPatch) => post<RiskPolicy>('/risk/policy', patch),
}

export const VEYRA_MAGIC = 77041
