/**
 * Shared presentation primitives for the console.
 *
 * Every area (shell, chart, overview, rail, tabs) builds on these so the
 * surfaces read as one system: one panel frame, one state dot, one toggle, one
 * segmented control and one icon set. Styling lives in `styles.css` under the
 * matching class names; nothing here branches on theme.
 */

import type { ReactNode } from 'react'

export type Tone = 'ok' | 'warn' | 'bad' | 'idle' | 'off'

/** Text tone class for a signed amount: gains green, losses red, flat plain. */
export function signTone(value: number | null | undefined): string {
  if (value == null || value === 0 || Number.isNaN(value)) return ''
  return value > 0 ? 'tone-ok' : 'tone-bad'
}

export function Panel({
  title,
  count,
  actions,
  divided = false,
  children,
  className = '',
  headingLevel = 2,
  label,
}: {
  /** Heading text, or a control (for example a view menu) standing in for it. */
  title: ReactNode
  /** Rendered muted after the title, e.g. `(2)`. */
  count?: number
  /** Right-aligned header content: links, segmented controls, state. */
  actions?: ReactNode
  /** Draw a rule under the header (rail cards) instead of flowing into the body. */
  divided?: boolean
  children: ReactNode
  className?: string
  headingLevel?: 2 | 3
  /** Accessible name when the title is not plain text. */
  label?: string
}) {
  const Heading = headingLevel === 2 ? 'h2' : 'h3'
  return (
    <section className={`panel ${className}`} aria-label={label}>
      <header className={`panel-head${divided ? ' is-divided' : ''}`}>
        <Heading className="panel-title">
          {title}
          {count !== undefined ? <span className="panel-count"> ({count})</span> : null}
        </Heading>
        {actions ? <div className="panel-actions">{actions}</div> : null}
      </header>
      <div className="panel-body">{children}</div>
    </section>
  )
}

export function Dot({ tone, label }: { tone: Tone; label?: string }) {
  return <span className={`dot is-${tone}`} role={label ? 'img' : undefined} aria-label={label} aria-hidden={label ? undefined : true} />
}

/** Placeholder with the same footprint as the value it stands in for. */
export function Skeleton({ width = 64, height = 16 }: { width?: number | string; height?: number | string }) {
  return <span className="skeleton" style={{ width, height }} aria-hidden="true" />
}

/**
 * Two-state switch with the state spelled out. It only *requests* a change;
 * callers own confirmation, because a mis-click here can change what the
 * service is allowed to do with real money.
 */
export function Toggle({
  checked,
  label,
  tone = 'bad',
  disabled = false,
  onClick,
}: {
  checked: boolean
  /** Accessible name, e.g. "Kill switch". */
  label: string
  /** Colour used while on. */
  tone?: 'ok' | 'warn' | 'bad'
  disabled?: boolean
  onClick?: () => void
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className={`toggle is-${tone}`}
    >
      <span className="toggle-knob" aria-hidden="true" />
      <span className="toggle-word" aria-hidden="true">
        {checked ? 'ON' : 'OFF'}
      </span>
    </button>
  )
}

/** Row of mutually exclusive buttons (ranges, timeframes). */
export function Segmented<T extends string | number>({
  options,
  value,
  onChange,
  label,
}: {
  options: ReadonlyArray<{ value: T; label: string }>
  value: T
  onChange: (value: T) => void
  /** Accessible group name, e.g. "Chart range". */
  label: string
}) {
  return (
    <div className="segmented" role="group" aria-label={label}>
      {options.map((option) => (
        <button
          key={String(option.value)}
          type="button"
          aria-pressed={option.value === value}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  )
}

export type IconName =
  | 'overview'
  | 'activity'
  | 'risk'
  | 'trace'
  | 'diagnostics'
  | 'settings'
  | 'gear'
  | 'sun'
  | 'moon'
  | 'chevron-down'
  | 'more'
  | 'info'
  | 'check'

/** 20×20 outline icons drawn on a 24 grid, stroked with the current colour. */
const ICONS: Record<IconName, ReactNode> = {
  overview: <path d="M12 3.5 21.5 12h-2v8.5h-15V12h-2z" />,
  activity: (
    <>
      <rect x="3.5" y="14" width="4" height="6" rx=".5" fill="currentColor" stroke="none" />
      <rect x="10" y="9" width="4" height="11" rx=".5" fill="currentColor" stroke="none" />
      <rect x="16.5" y="4" width="4" height="16" rx=".5" fill="currentColor" stroke="none" />
    </>
  ),
  risk: <path d="M12 3 19.5 5.8v5.4c0 4.6-3.1 8.2-7.5 9.8-4.4-1.6-7.5-5.2-7.5-9.8V5.8z" />,
  trace: (
    <>
      <rect x="5" y="3.5" width="14" height="17" rx="1.5" />
      <path d="M8.5 8.5h7" />
      <path d="M8.5 12h7" />
      <path d="M8.5 15.5h2.5M15 15.5h.5" />
    </>
  ),
  diagnostics: <path d="M2.5 12h4l2.5-6 5 12 2.5-6h5" />,
  settings: (
    <>
      <path d="M4 8h16" />
      <path d="M4 16h16" />
      <circle cx="9" cy="8" r="2.2" />
      <circle cx="15" cy="16" r="2.2" />
    </>
  ),
  gear: (
    <>
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z" />
    </>
  ),
  sun: (
    <>
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M5.3 18.7l1.4-1.4M17.3 6.7l1.4-1.4" />
    </>
  ),
  moon: <path d="M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z" />,
  'chevron-down': <path d="m6 9 6 6 6-6" />,
  more: (
    <>
      <circle cx="12" cy="5.5" r="1.1" fill="currentColor" />
      <circle cx="12" cy="12" r="1.1" fill="currentColor" />
      <circle cx="12" cy="18.5" r="1.1" fill="currentColor" />
    </>
  ),
  info: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 11v5.5" />
      <circle cx="12" cy="7.8" r="0.6" fill="currentColor" />
    </>
  ),
  check: <path d="m5 12.5 4.5 4.5L19 7.5" />,
}

export function Icon({ name, className = '', size }: { name: IconName; className?: string; size?: number }) {
  return (
    <svg
      className={`icon ${className}`}
      viewBox="0 0 24 24"
      width={size}
      height={size}
      style={size ? { width: size, height: size } : undefined}
      aria-hidden="true"
      focusable="false"
    >
      {ICONS[name]}
    </svg>
  )
}
