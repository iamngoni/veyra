/**
 * Form and list primitives shared by the working tabs and the settings view:
 * the button, a control's labelled head, the labelled text input and the
 * placeholder rows shown while a list's first poll is in flight. Styled in
 * the forms section of styles.css.
 */

import { useId } from 'react'
import type { ChangeEvent, ReactNode } from 'react'

import { Hint, Skeleton } from './ui'

/** Placeholder rows while a list's first poll is in flight. */
export function SkeletonRows() {
  return (
    <ul className="tab-list" aria-hidden="true">
      {[0, 1, 2].map((row) => (
        <li key={row} className="tab-row tab-skeleton-row">
          <Skeleton width={56} />
          <Skeleton width="60%" />
        </li>
      ))}
    </ul>
  )
}

/** Secondary button; `tone="ok"` marks the one primary action that commits a change. */
export function Button({
  children,
  tone,
  disabled = false,
  onClick,
}: {
  children: ReactNode
  tone?: 'ok'
  disabled?: boolean
  onClick: () => void
}) {
  return (
    <button type="button" className={`tab-button${tone ? ` is-${tone}` : ''}`} disabled={disabled} onClick={onClick}>
      {children}
    </button>
  )
}

/**
 * A control's head: the label, an info mark after it when there is help to
 * give, and an aside held to the right. `htmlFor` makes the label a real
 * `<label>` for a native input; switches and fixed values name themselves.
 */
export function ControlHead({
  label,
  htmlFor,
  help,
  aside,
}: {
  label: string
  htmlFor?: string
  /** What the setting does, shown from the info mark. */
  help?: string
  aside?: ReactNode
}) {
  return (
    <div className="tab-control-head">
      <span className="tab-control-label">
        {htmlFor ? (
          <label htmlFor={htmlFor} className="tab-control-name">
            {label}
          </label>
        ) : (
          <span className="tab-control-name">{label}</span>
        )}
        {help ? <Hint text={help} label={label} /> : null}
      </span>
      {aside ? <span className="tab-control-aside">{aside}</span> : null}
    </div>
  )
}

/** Label above, optional marker beside it, control below. */
export function Control({
  id,
  label,
  help,
  span = false,
  aside,
  children,
}: {
  id: string
  label: string
  help?: string
  /** Take two columns (long lists). */
  span?: boolean
  /** Right-aligned beside the label, e.g. an override marker. */
  aside?: ReactNode
  children: ReactNode
}) {
  return (
    <div className={`tab-control${span ? ' is-span' : ''}`}>
      <ControlHead label={label} htmlFor={id} help={help} aside={aside} />
      {children}
    </div>
  )
}

/** A labelled text input for the editors. */
export function TextControl({
  label,
  field,
  value,
  onChange,
  placeholder,
  span,
  disabled = false,
  dirty = false,
  aside,
  help,
}: {
  label: string
  /** Rides on the element as `data-field`, so one handler serves every input. */
  field: string
  value: string
  onChange: (event: ChangeEvent<HTMLInputElement>) => void
  placeholder?: string
  span?: boolean
  disabled?: boolean
  /** Changed but not yet applied. */
  dirty?: boolean
  aside?: ReactNode
  help?: string
}) {
  const id = useId()
  return (
    <Control id={id} label={label} help={help} span={span} aside={aside}>
      <input
        id={id}
        className={`tab-input${dirty ? ' is-dirty' : ''}`}
        data-field={field}
        value={value}
        placeholder={placeholder}
        disabled={disabled}
        onChange={onChange}
        spellCheck={false}
        autoComplete="off"
      />
    </Control>
  )
}
