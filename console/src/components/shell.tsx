/**
 * Console chrome: the top bar (wordmark, live status strip, theme and settings)
 * and the left navigation rail.
 */

import { useEffect, useRef } from 'react'

import type { Status } from '../lib/api'
import type { Theme } from '../lib/hooks'
import { systemPosture, type PostureTone } from '../lib/posture'
import { Dot, Icon, type IconName, type Tone } from './ui'

export type NavTab = { id: string; label: string; icon: IconName }

type Indicator = { id: string; label: string; tone: Tone; title?: string }

const POSTURE_TONE: Record<PostureTone, Tone> = { ok: 'ok', warn: 'warn', bad: 'bad', off: 'idle' }

/** Loop interval in the largest whole unit: 30s, 5m, 1h. */
function cadence(secs: number): string {
  if (secs >= 3600 && secs % 3600 === 0) return `${secs / 3600}h`
  if (secs >= 60 && secs % 60 === 0) return `${secs / 60}m`
  return `${secs}s`
}

/**
 * The status strip, verdict first. Only the dot carries state; the words stay
 * plain so a row of colours never has to be decoded.
 */
function indicators(status?: Status): Indicator[] {
  if (!status) return [{ id: 'posture', label: 'Connecting', tone: 'idle' }]
  const posture = systemPosture(status)
  const autopilot = status.autopilot
  return [
    {
      id: 'posture',
      label: posture.label,
      tone: POSTURE_TONE[posture.tone],
      title: posture.tone === 'ok' ? undefined : posture.detail,
    },
    status.broker_connected
      ? { id: 'terminal', label: 'Terminal connected', tone: 'ok' }
      : { id: 'terminal', label: 'Terminal stale', tone: 'bad' },
    status.ea_live_orders
      ? { id: 'ea', label: 'EA armed', tone: 'ok' }
      : { id: 'ea', label: 'EA disarmed', tone: 'idle' },
    status.trading_enabled
      ? { id: 'trading', label: 'Trading enabled', tone: 'ok' }
      : { id: 'trading', label: 'Trading disabled', tone: 'idle' },
    autopilot?.enabled
      ? { id: 'autopilot', label: `Autopilot ${cadence(autopilot.interval_secs)} · ${autopilot.timeframe}`, tone: 'ok' }
      : { id: 'autopilot', label: 'Autopilot off', tone: 'idle' },
  ]
}

/** Full-width header: wordmark, the live status strip, theme and settings. */
export function Topbar({
  status,
  theme,
  onToggleTheme,
  onOpenSettings,
}: {
  status?: Status
  theme: Theme
  onToggleTheme: () => void
  onOpenSettings: () => void
}) {
  const themeLabel = theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme'
  return (
    <header className="shell-topbar">
      <div className="shell-brand">
        <svg className="shell-brand-mark" viewBox="0 0 24 24" aria-hidden="true">
          <path d="m3 5 9 15L21 5h-5l-4 7-4-7Z" />
        </svg>
        <span className="shell-wordmark">Veyra</span>
      </div>
      <span className="shell-workspace">Trading workspace</span>
      <ul className="shell-status" aria-label="System status">
        {indicators(status).map((item) => (
          <li key={item.id} className="shell-status-item" title={item.title}>
            <Dot tone={item.tone} />
            <span>{item.label}</span>
          </li>
        ))}
      </ul>
      <div className="shell-actions">
        <button type="button" className="icon-button" aria-label={themeLabel} title={themeLabel} onClick={onToggleTheme}>
          <Icon name={theme === 'dark' ? 'sun' : 'moon'} />
        </button>
        <button type="button" className="icon-button" aria-label="Settings" title="Settings" onClick={onOpenSettings}>
          <Icon name="gear" />
        </button>
      </div>
    </header>
  )
}

/** Left navigation between the console views. */
export function Sidebar({
  tabs,
  active,
  onSelect,
}: {
  tabs: ReadonlyArray<NavTab>
  active: string
  onSelect: (id: string) => void
}) {
  const navRef = useRef<HTMLElement>(null)
  // On a narrow screen the rail is a sideways strip, and a view can open from
  // elsewhere (the settings gear, "View all") with its tab scrolled out of
  // sight. Bring it back so the strip always shows where you are.
  useEffect(() => {
    const nav = navRef.current as HTMLElement
    if (nav.scrollWidth <= nav.clientWidth) return
    nav.querySelector('[aria-current="page"]')?.scrollIntoView({ block: 'nearest', inline: 'nearest' })
  }, [active])
  return (
    <div className="shell-sidebar">
      <nav ref={navRef} className="shell-nav" aria-label="Main">
        {tabs.map((tab) => (
          <button
            key={tab.id}
            type="button"
            className="shell-nav-item"
            aria-current={tab.id === active ? 'page' : undefined}
            onClick={() => onSelect(tab.id)}
          >
            <Icon name={tab.icon} />
            <span>{tab.label}</span>
          </button>
        ))}
      </nav>
    </div>
  )
}
