/**
 * The single verdict an operator looks for first: can this thing trade?
 *
 * Pure so the header, tests and any future surface agree on one ranking of
 * what is true right now, most restrictive first.
 */

import type { Status } from './api'

export type PostureTone = 'ok' | 'warn' | 'bad' | 'off'

export type Posture = { label: string; detail: string; tone: PostureTone }

export function systemPosture(status?: Status): Posture {
  if (!status) {
    return { label: 'CONNECTING', detail: 'reaching the service', tone: 'off' }
  }
  if (status.risk_policy?.killSwitch) {
    return { label: 'HALTED', detail: 'kill switch engaged — every intent refused', tone: 'bad' }
  }
  // Ranked above the link and the switches because it is the failure that
  // looks like health: the service answers, the terminal is live, and nothing
  // is ever decided. Two in a row rules out one transient provider hiccup.
  const failures = status.decisions?.consecutiveFailures ?? 0
  if (failures >= 2) {
    const lastModel = status.decisions?.lastModel ? ` · last LLM ${status.decisions.lastModel}` : ''
    return {
      label: 'NOT DECIDING',
      detail: `${failures} decisions in a row failed${lastModel} — ${status.decisions?.lastFailure ?? 'no reason reported'}`,
      tone: 'bad',
    }
  }
  if (!status.broker_connected) {
    return { label: 'NO LINK', detail: 'terminal is not reporting', tone: 'bad' }
  }
  if (!status.trading_enabled) {
    return { label: 'STANDBY', detail: 'deciding only — nothing will execute', tone: 'off' }
  }
  if (!status.ea_live_orders) {
    return { label: 'DRY RUN', detail: 'service armed, terminal still validating only', tone: 'warn' }
  }
  return { label: 'LIVE', detail: 'orders reach the market', tone: 'ok' }
}
