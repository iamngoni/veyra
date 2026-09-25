/**
 * Shared presentation primitives for the console.
 *
 * Every area (shell, chart, overview, rail, tabs) builds on these so the
 * surfaces read as one system: one panel frame, one state dot, one toggle, one
 * segmented control and one icon set. Styling lives in `styles.css` under the
 * matching class names; nothing here branches on theme.
 */

import { useId, useState } from 'react'
import type { CSSProperties, ReactNode, SyntheticEvent } from 'react'

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
          {count !== undefined ? (
            // Drawn as a bare number in a chip (see .panel-count); the text
            // keeps the parentheses so the heading still reads "Open
            // positions (3)" to assistive technology and in plain text.
            <span className="panel-count" data-count={count}>
              {' '}({count})
            </span>
          ) : null}
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
 * Two-state switch: the track fills with the tone while on and the knob sits
 * on that side. It only *requests* a change; callers own confirmation,
 * because a mis-click here can change what the service is allowed to do with
 * real money.
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
    </button>
  )
}

/** Widest a tip grows (see .hint-tip) and the gap it keeps from the window's edges. */
const TIP_WIDTH = 280
const TIP_EDGE = 16
/** Where a tip starts, relative to its mark, when nothing is in the way. */
const TIP_START = -10
/** Room a tip needs above its mark: a few lines, plus the view's own header. */
const TIP_ROOM = 150

/**
 * An info mark whose explanation appears on hover or keyboard focus, styled
 * with the console's tokens instead of the browser's delayed native tooltip.
 * The text is also the mark's accessible description.
 */
export function Hint({ text, label, size = 14 }: { text: string; label: string; size?: number }) {
  const id = useId()
  // A tip opens where it has room: it starts just left of its mark but slides
  // back inside the window near either edge, and near the top it opens below
  // the mark instead of under the view's header. Decided as it opens, so it
  // follows scrolling and resizing.
  const [place, setPlace] = useState<{ x: number; below: boolean }>()
  const open = (event: SyntheticEvent<HTMLElement>) => {
    const hint = event.currentTarget
    const mark = hint.getBoundingClientRect()
    // Measured when the tip is already showing; otherwise its widest.
    const shown = (hint.lastElementChild as HTMLElement).offsetWidth
    const width = shown || Math.min(TIP_WIDTH, window.innerWidth - 3 * TIP_EDGE)
    const x = Math.max(TIP_EDGE - mark.left, Math.min(TIP_START, window.innerWidth - TIP_EDGE - width - mark.left))
    setPlace({ x: Math.round(x), below: mark.top < TIP_ROOM })
  }
  return (
    <span
      className={`hint${place?.below ? ' is-below' : ''}`}
      style={place ? ({ '--tip-x': `${place.x}px` } as CSSProperties) : undefined}
      onMouseEnter={open}
      onFocus={open}
    >
      <button type="button" className="hint-mark" aria-label={`About ${label}`} aria-describedby={id}>
        <Icon name="info" size={size} />
      </button>
      <span role="tooltip" id={id} className="hint-tip">
        {text}
      </span>
    </span>
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
  | 'trades'
  | 'risk'
  | 'trace'
  | 'diagnostics'
  | 'notifications'
  | 'settings'
  | 'gear'
  | 'sun'
  | 'moon'
  | 'chevron-down'
  | 'more'
  | 'info'
  | 'check'
  | 'close'

/** Outline icons drawn on a 24 grid (16px by default), stroked with the current colour. */
const ICONS: Record<IconName, ReactNode> = {
  // The six views share one family: outlines only, inside a 16–18 unit box.
  overview: (
    <path d="M4 10.2 12 3.8l8 6.4v9.3a1 1 0 0 1-1 1h-3.75v-5.25a1 1 0 0 0-1-1h-2.5a1 1 0 0 0-1 1v5.25H5a1 1 0 0 1-1-1z" />
  ),
  activity: (
    <>
      <path d="M4 4v15a1 1 0 0 0 1 1h15" />
      <path d="M8.5 16.5v-4M13 16.5v-8M17.5 16.5v-6" />
    </>
  ),
  trades: (
    <>
      <path d="M5 8h13m-4.5-4L18 8l-4.5 4" />
      <path d="M19 16H6m4.5 4L6 16l4.5-4" />
    </>
  ),
  risk: <path d="M12 3.5 19 6.25v5.25c0 4.35-2.9 7.6-7 9-4.1-1.4-7-4.65-7-9V6.25z" />,
  trace: (
    <>
      <rect x="5" y="3.5" width="14" height="17" rx="2" />
      <path d="M9 8.5h6M9 12h6M9 15.5h3.5" />
    </>
  ),
  diagnostics: <path d="M3.5 12h3L9 6l6 12 2.5-6h3" />,
  notifications: (
    <>
      <path d="M6 16.5V11a6 6 0 0 1 12 0v5.5l1.5 2h-15z" />
      <path d="M10 20.5a2 2 0 0 0 4 0" />
    </>
  ),
  settings: (
    <>
      <path d="M4 8h3M11 8h9M4 16h9M17 16h3" />
      <circle cx="9" cy="8" r="2" />
      <circle cx="15" cy="16" r="2" />
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
  close: <path d="M6.5 6.5l11 11M17.5 6.5l-11 11" />,
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
