/**
 * The posture ranking: one verdict, most restrictive true statement first.
 */

import { describe, expect, it } from 'vitest'

import type { Status } from './api'
import { systemPosture } from './posture'

const status: Status = {
  service: 'veyra',
  version: '0.1.0',
  environment: 'development',
  broker_provider: 'ea',
  market_provider: 'ea',
  model_provider: 'openrouter',
  jev_provider: 'typesafe',
  persistence: 'postgres',
  broker_connected: true,
  trading_enabled: true,
  ea_live_orders: true,
  autopilot: null,
  model_budget: null,
  jev_usage: null,
  decisions: { consecutiveFailures: 0, lastFailure: null, lastFailureAt: null },
  risk_policy: {
    killSwitch: false,
    allowTradingWithoutJev: false,
    symbols: ['EURUSD'],
    maxVolumePerOrder: 0.01,
    maxTotalLots: 0.01,
    maxOpenOrders: 1,
    duplicateWindowSecs: 60,
    sessionUtc: null,
    maxRiskPercent: 12,
    maxDailyLossPercent: 10,
    maxPeakDrawdownPercent: 25,
    maxNetFactorLots: 0.01,
    calendarBlackoutMinutes: 30,
    minStopAtrFraction: 0.25,
    weekendPositions: 'agent',
  },
}

describe('systemPosture', () => {
  it('reports the most restrictive true statement first', () => {
    expect(systemPosture(undefined)).toEqual({ label: 'CONNECTING', detail: 'reaching the service', tone: 'off' })
    // A halted gate outranks everything else that looks healthy.
    expect(systemPosture({ ...status, risk_policy: { ...status.risk_policy, killSwitch: true } }).label).toBe('HALTED')
    expect(systemPosture({ ...status, broker_connected: false }).label).toBe('NO LINK')
    expect(systemPosture({ ...status, trading_enabled: false }).label).toBe('STANDBY')
    // Service armed but the terminal still validating only.
    expect(systemPosture({ ...status, ea_live_orders: false })).toMatchObject({ label: 'DRY RUN', tone: 'warn' })
    expect(systemPosture(status)).toEqual({ label: 'LIVE', detail: 'orders reach the market', tone: 'ok' })
  })

  it('names a decision outage that every other signal reports as healthy', () => {
    // The exact shape of a provider refusing every request: service up,
    // terminal live, trading armed — and nothing decided.
    const refusing: Status = {
      ...status,
      decisions: {
        consecutiveFailures: 4,
        lastFailure: 'openrouter call returned 400: Thinking mode does not support this tool_choice',
        lastFailureAt: 1_700_000_000,
        lastModel: 'openai/gpt-5.6-mini',
      },
    }
    const posture = systemPosture(refusing)
    expect(posture.label).toBe('NOT DECIDING')
    expect(posture.tone).toBe('bad')
    expect(posture.detail).toContain('4 decisions in a row failed')
    expect(posture.detail).toContain('last LLM openai/gpt-5.6-mini')
    expect(posture.detail).toContain('Thinking mode')

    // Neither the model nor the reason is guessed when missing.
    const bare = systemPosture({ ...status, decisions: { consecutiveFailures: 2, lastFailure: null, lastFailureAt: 0 } })
    expect(bare.detail).toBe('2 decisions in a row failed — no reason reported')

    // One failure is a hiccup, not an outage.
    expect(
      systemPosture({ ...status, decisions: { consecutiveFailures: 1, lastFailure: 'x', lastFailureAt: 1 } }).label,
    ).toBe('LIVE')

    // A halted gate still outranks it: the owner stopped this deliberately.
    expect(systemPosture({ ...refusing, risk_policy: { ...status.risk_policy, killSwitch: true } }).label).toBe('HALTED')
  })

  it('treats absent decision telemetry as no failures', () => {
    expect(systemPosture({ ...status, decisions: null }).label).toBe('LIVE')
  })
})
