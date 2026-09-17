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
}

export type Position = {
  ticket: number
  symbol: string
  kind: 'buy' | 'sell'
  lots: number
  price: number
  profit: number
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
  commands: (limit = 25) => get<{ commands: CommandRecord[] }>(`/commands?limit=${limit}`),
  candles: (bars = 48) => get<CandleSeries>(`/market/candles?timeframe=H4&bars=${bars}`),
  events: (after: number | undefined, waitMs = 15000) =>
    get<Feed>(after === undefined ? '/events' : `/events?after=${after}&wait_ms=${waitMs}`),
}

export const VEYRA_MAGIC = 77041
