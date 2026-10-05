/**
 * Instrument picker for the autopilot: a multi-select over every instrument
 * the connected broker offers, with a search box and one filter per kind of
 * market.
 *
 * The list comes from the service, which pulls it from the terminal once per
 * connection (see `useSymbolCatalog`), so the picker always has data to show.
 * Picking an instrument here only names it for the autopilot. What may
 * actually trade stays with the risk gate's allowlist: instruments it would
 * refuse are marked, and allowing them is a separate, explicit button that
 * lists exactly what it will add.
 */

import { useEffect, useId, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'

import { api, type CatalogSymbol, type SymbolCategoryId } from '../lib/api'
import { useSymbolCatalog } from '../lib/hooks'
import { Button, ControlHead } from './form'
import { Icon } from './ui'

/** The service rotates through at most this many instruments. */
export const MAX_AUTOPILOT_SYMBOLS = 16
/** Rows per page; a broker can list thousands, so the list is paged rather than cut off. */
export const PAGE_ROWS = 50
/** The shape the service accepts for an instrument name. */
const SYMBOL_NAME = /^(?!.* {2})[A-Za-z0-9._#+() -]{1,32}$/

type CategoryFilter = SymbolCategoryId | 'all'

/** Splits a comma-separated list, dropping blanks and case-insensitive repeats. */
export function parseSymbolList(value: string): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const part of value.split(',')) {
    const name = part.trim()
    if (name === '' || seen.has(name.toLowerCase())) continue
    seen.add(name.toLowerCase())
    out.push(name)
  }
  return out
}

/** The list as the service reads it. */
export function joinSymbolList(names: readonly string[]): string {
  return names.join(',')
}

function matches(symbol: CatalogSymbol, needle: string): boolean {
  return (
    symbol.name.toLowerCase().includes(needle) ||
    symbol.description.toLowerCase().includes(needle) ||
    symbol.path.toLowerCase().includes(needle)
  )
}

/** "3 min ago" style age for the loaded-from line. */
function ageOf(fetchedAt: number): string {
  const minutes = Math.max(0, Math.round((Date.now() / 1000 - fetchedAt) / 60))
  if (minutes < 1) return 'just now'
  if (minutes < 60) return `${minutes} min ago`
  const hours = Math.round(minutes / 60)
  return hours < 48 ? `${hours} h ago` : `${Math.round(hours / 24)} d ago`
}

export function SymbolPicker({
  label,
  help,
  value,
  dirty = false,
  disabled = false,
  aside,
  onChange,
}: {
  label: string
  help?: string
  /** The chosen instruments, comma separated. */
  value: string
  dirty?: boolean
  disabled?: boolean
  aside?: ReactNode
  onChange: (value: string) => void
}) {
  const { catalog, error, refreshing, reload, refresh } = useSymbolCatalog()
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [category, setCategory] = useState<CategoryFilter>('all')
  const [page, setPage] = useState(0)
  const [note, setNote] = useState<string>()
  const [allowing, setAllowing] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const searchId = useId()
  const dialogId = useId()

  const selected = useMemo(() => parseSymbolList(value), [value])
  const chosen = useMemo(() => new Set(selected.map((name) => name.toLowerCase())), [selected])
  const known = useMemo(
    () => new Map((catalog?.ready ? catalog.symbols : []).map((symbol) => [symbol.name.toLowerCase(), symbol])),
    [catalog],
  )

  // Escape and a click outside close the list; focus returns to its opener.
  useEffect(() => {
    if (!open) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false)
    }
    const onPointer = (event: MouseEvent) => {
      if (root.current && !root.current.contains(event.target as Node)) setOpen(false)
    }
    document.addEventListener('keydown', onKey)
    document.addEventListener('mousedown', onPointer)
    return () => {
      document.removeEventListener('keydown', onKey)
      document.removeEventListener('mousedown', onPointer)
    }
  }, [open])

  const needle = query.trim().toLowerCase()
  const rows = useMemo(() => {
    if (!catalog?.ready) return []
    return catalog.symbols.filter((symbol) => (category === 'all' || symbol.category === category) && (needle === '' || matches(symbol, needle)))
  }, [catalog, category, needle])

  const pageCount = Math.max(1, Math.ceil(rows.length / PAGE_ROWS))
  const current = Math.min(page, pageCount - 1)
  const first = current * PAGE_ROWS
  const shown = rows.slice(first, first + PAGE_ROWS)

  const full = selected.length >= MAX_AUTOPILOT_SYMBOLS
  const refused = selected.filter((name) => known.get(name.toLowerCase())?.riskAllowed === false)

  const toggle = (name: string) => {
    setNote(undefined)
    if (chosen.has(name.toLowerCase())) {
      onChange(joinSymbolList(selected.filter((existing) => existing.toLowerCase() !== name.toLowerCase())))
    } else if (!full) {
      onChange(joinSymbolList([...selected, name]))
    }
  }

  const typed = query.trim()
  const canAddTyped =
    SYMBOL_NAME.test(typed) && !chosen.has(typed.toLowerCase()) && !known.has(typed.toLowerCase()) && !full

  const allowInRiskGate = async () => {
    setAllowing(true)
    setNote(undefined)
    try {
      const current = await api.riskPolicy()
      const have = new Set(current.symbols.map((name) => name.toLowerCase()))
      const add = refused.filter((name) => !have.has(name.toLowerCase()))
      await api.updatePolicy({ symbols: [...current.symbols, ...add] })
      await reload()
      setNote(`Added ${add.join(', ')} to the risk gate's allowed instruments.`)
    } catch (cause) {
      setNote(cause instanceof Error ? cause.message : 'The risk gate was not changed.')
    } finally {
      setAllowing(false)
    }
  }

  // An EA built before the instrument list existed answers with this exact reason.
  const outdated = !catalog?.ready && catalog?.reason.includes('unsupported command')
  const status = catalog?.ready
    ? `${catalog.total.toLocaleString()} instruments from ${catalog.server}, loaded ${ageOf(catalog.fetchedAt)}`
    : outdated
      ? 'The MetaTrader EA is too old to list instruments. Recompile VeyraProbe (version 1.27 or newer) and reload it on the chart.'
      : catalog
        ? `Waiting for the terminal to list its instruments (${catalog.reason}).`
        : error
          ? `Instrument list unavailable: ${error}`
          : 'Loading the instrument list…'

  return (
    <div className="tab-control is-span sym-picker" ref={root}>
      <ControlHead label={label} help={help} aside={aside} />
      <div className={`tab-input sym-field${dirty ? ' is-dirty' : ''}`}>
        {selected.length === 0 ? <span className="sym-empty">No instruments chosen</span> : null}
        {selected.map((name) => {
          const refusedByGate = known.get(name.toLowerCase())?.riskAllowed === false
          return (
            <span
              key={name}
              className={`sym-chip${refusedByGate ? ' is-warn' : ''}`}
              title={refusedByGate ? "Not on the risk gate's allowed list" : undefined}
            >
              {name}
              <button type="button" aria-label={`Remove ${name}`} disabled={disabled} onClick={() => toggle(name)}>
                <Icon name="close" size={12} />
              </button>
            </span>
          )
        })}
        <button
          type="button"
          className="sym-open"
          aria-haspopup="dialog"
          aria-expanded={open}
          aria-controls={open ? dialogId : undefined}
          disabled={disabled}
          onClick={() => setOpen((current) => !current)}
        >
          {selected.length === 0 ? 'Choose instruments' : 'Add or remove'}
        </button>
      </div>

      {refused.length > 0 ? (
        <p className="tab-group-note sym-note is-warn">
          {refused.join(', ')} {refused.length === 1 ? 'is' : 'are'} not on the risk gate&apos;s allowed list, so the
          autopilot will skip {refused.length === 1 ? 'it' : 'them'}.{' '}
          <button type="button" className="tab-link" disabled={allowing || disabled} onClick={() => void allowInRiskGate()}>
            {allowing ? 'Adding…' : `Allow ${refused.join(', ')} in the risk gate`}
          </button>
        </p>
      ) : null}
      {note ? (
        <p className="tab-group-note sym-note" role="status">
          {note}
        </p>
      ) : null}

      {open ? (
        <div className="sym-pop" role="dialog" aria-label="Choose instruments" id={dialogId}>
          <input
            id={searchId}
            type="search"
            className="tab-input"
            aria-label="Search instruments"
            placeholder="Search by name, description or folder"
            value={query}
            onChange={(event) => {
              setQuery(event.target.value)
              setPage(0)
            }}
            autoFocus
          />
          {catalog?.ready ? (
            <div className="sym-cats" role="group" aria-label="Filter by market">
              <button
                type="button"
                className={`sym-cat${category === 'all' ? ' is-on' : ''}`}
                aria-pressed={category === 'all'}
                onClick={() => {
                  setCategory('all')
                  setPage(0)
                }}
              >
                All <span>{catalog.total.toLocaleString()}</span>
              </button>
              {catalog.categories.map((entry) => (
                <button
                  key={entry.id}
                  type="button"
                  className={`sym-cat${category === entry.id ? ' is-on' : ''}`}
                  aria-pressed={category === entry.id}
                  onClick={() => {
                    setCategory(entry.id)
                    setPage(0)
                  }}
                >
                  {entry.label} <span>{entry.count.toLocaleString()}</span>
                </button>
              ))}
            </div>
          ) : null}

          <p className="sym-status">{status}</p>
          {error && catalog?.ready ? (
            <p className="sym-status is-warn" role="alert">
              Could not reload the list: {error}
            </p>
          ) : null}

          {catalog?.ready ? (
            rows.length > 0 ? (
              <ul className="sym-list" aria-label="Instruments">
                {shown.map((symbol) => {
                  const checked = chosen.has(symbol.name.toLowerCase())
                  return (
                    <li key={symbol.name}>
                      <label className="sym-row">
                        <input
                          type="checkbox"
                          checked={checked}
                          disabled={disabled || (!checked && full)}
                          onChange={() => toggle(symbol.name)}
                        />
                        <span className="sym-name">{symbol.name}</span>
                        <span className="sym-desc">{symbol.description || symbol.path}</span>
                        <span className="sym-tags">
                          {!symbol.riskAllowed ? <span className="sym-tag is-warn">not in risk gate</span> : null}
                          <span className="sym-tag">{symbol.category}</span>
                        </span>
                      </label>
                    </li>
                  )
                })}
              </ul>
            ) : (
              <p className="sym-status">Nothing matches that search.</p>
            )
          ) : null}
          {catalog?.ready && rows.length > PAGE_ROWS ? (
            <nav className="sym-pager" aria-label="Instrument pages">
              <Button onClick={() => setPage(current - 1)} disabled={current === 0}>
                Previous
              </Button>
              <span className="sym-status" aria-live="polite">
                {first + 1}–{first + shown.length} of {rows.length.toLocaleString()}
              </span>
              <Button onClick={() => setPage(current + 1)} disabled={current >= pageCount - 1}>
                Next
              </Button>
            </nav>
          ) : null}

          {canAddTyped ? (
            <button type="button" className="sym-manual" onClick={() => { toggle(typed); setQuery('') }}>
              Add “{typed}” by name
            </button>
          ) : null}

          <div className="sym-foot">
            <span className={full ? 'tone-warn' : ''}>
              {selected.length} of {MAX_AUTOPILOT_SYMBOLS} chosen
            </span>
            <span className="sym-foot-actions">
              <Button onClick={() => void refresh()} disabled={refreshing || disabled}>
                {refreshing ? 'Reloading…' : 'Reload from broker'}
              </Button>
              <Button onClick={() => onChange('')} disabled={selected.length === 0 || disabled}>
                Clear
              </Button>
              <Button tone="ok" onClick={() => setOpen(false)}>
                Done
              </Button>
            </span>
          </div>
        </div>
      ) : null}
    </div>
  )
}
