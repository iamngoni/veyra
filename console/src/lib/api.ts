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
  symbol: string | null
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

export const api = {
  status: () => get<Status>('/status'),
  account: () => get<Account>('/account'),
  reconciliation: () => get<Reconciliation>('/reconciliation'),
  metrics: () => get<Metrics>('/metrics'),
  commands: (limit = 25) => get<{ commands: CommandRecord[] }>(`/commands?limit=${limit}`),
  candles: (bars = 48) => get<CandleSeries>(`/market/candles?timeframe=H4&bars=${bars}`),
  events: (after: number | undefined, waitMs = 15000) =>
    get<Feed>(after === undefined ? '/events' : `/events?after=${after}&wait_ms=${waitMs}`),
}

export const VEYRA_MAGIC = 77041
