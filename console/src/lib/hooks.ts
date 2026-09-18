import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'

import { api, type FeedEvent, type LogLevel, type LogRecord } from './api'

/** Polls an async source on an interval, keeping the last good value on error. */
export function usePoll<T>(load: () => Promise<T>, intervalMs: number) {
  const [data, setData] = useState<T>()
  const [error, setError] = useState<string>()
  const loadRef = useRef(load)
  loadRef.current = load
  // Lets a control that just changed server state pull the new value at once
  // rather than leaving a stale reading on screen until the next interval.
  const refetchRef = useRef<(() => Promise<void>) | undefined>(undefined)

  useEffect(() => {
    let alive = true
    const tick = async () => {
      try {
        const value = await loadRef.current()
        if (alive) {
          setData(value)
          setError(undefined)
        }
      } catch (cause) {
        if (alive) setError(cause instanceof Error ? cause.message : String(cause))
      }
    }
    refetchRef.current = tick
    void tick()
    const timer = setInterval(tick, intervalMs)
    return () => {
      alive = false
      refetchRef.current = undefined
      clearInterval(timer)
    }
  }, [intervalMs])

  const refetch = useCallback(async () => {
    await refetchRef.current?.()
  }, [])

  return { data, error, refetch }
}

/** Follows /events with a cursor; reconnects on any transport failure. */
export function useEventFeed(capacity = 80) {
  const [events, setEvents] = useState<FeedEvent[]>([])
  const [connected, setConnected] = useState(false)
  const cursor = useRef<number | undefined>(undefined)

  useEffect(() => {
    let alive = true
    const run = async () => {
      while (alive) {
        try {
          const feed = await api.events(cursor.current)
          if (!alive) return
          cursor.current = feed.next
          if (feed.events.length > 0) {
            setEvents((previous) => [...feed.events].reverse().concat(previous).slice(0, capacity))
          }
          setConnected(true)
        } catch {
          if (!alive) return
          setConnected(false)
          await new Promise((resolve) => setTimeout(resolve, 2000))
        }
      }
    }
    void run()
    return () => {
      alive = false
    }
  }, [capacity])

  return { events, connected }
}

/**
 * Tails `/logs` on a short interval, resetting the cursor whenever the level
 * filter changes so the list always reflects the selected severity.
 */
export function useLogFeed(level: LogLevel, capacity = 300, intervalMs = 2000) {
  const [logs, setLogs] = useState<LogRecord[]>([])
  const [error, setError] = useState<string>()
  const cursor = useRef<number | undefined>(undefined)

  useEffect(() => {
    let alive = true
    cursor.current = undefined
    setLogs([])
    const tick = async () => {
      try {
        const tail = await api.logs(cursor.current, level)
        if (!alive) return
        cursor.current = tail.latest
        if (tail.logs.length > 0) {
          setLogs((previous) => [...previous, ...tail.logs].slice(-capacity))
        }
        setError(undefined)
      } catch (cause) {
        if (alive) setError(cause instanceof Error ? cause.message : String(cause))
      }
    }
    void tick()
    const timer = setInterval(tick, intervalMs)
    return () => {
      alive = false
      clearInterval(timer)
    }
  }, [level, capacity, intervalMs])

  return { logs, error }
}

export type Theme = 'dark' | 'light'

const THEME_KEY = 'veyra.theme'

/** The stored choice, else the operating system preference. Client only. */
function preferredTheme(): Theme {
  const stored = window.localStorage?.getItem(THEME_KEY)
  if (stored === 'dark' || stored === 'light') return stored
  return window.matchMedia?.('(prefers-color-scheme: light)').matches ? 'light' : 'dark'
}

/**
 * Theme choice, applied to the document root and remembered per machine.
 *
 * Every token is defined for both themes, so switching is a single attribute
 * flip rather than a re-render of themed values.
 *
 * The first render is always the server's value: reading storage or the media
 * query during render makes the client disagree with the markup it is
 * hydrating, which React rejects. The real preference is adopted immediately
 * afterwards, before paint.
 */
export function useTheme() {
  const [theme, setTheme] = useState<Theme>('dark')
  const resolved = useRef(false)

  useLayoutEffect(() => {
    if (resolved.current) return
    resolved.current = true
    setTheme(preferredTheme())
  }, [])

  useEffect(() => {
    document.documentElement.dataset.theme = theme
    // Only persist a deliberate choice, never the value hydration started from.
    if (resolved.current) window.localStorage?.setItem(THEME_KEY, theme)
  }, [theme])

  const toggle = useCallback(() => {
    setTheme((current) => (current === 'dark' ? 'light' : 'dark'))
  }, [])

  return { theme, toggle }
}

/**
 * Windows a list into pages, keeping the page in range as the list grows or
 * shrinks underneath it — a feed that gains rows must not strand the reader on
 * a page that no longer exists.
 */
export function usePaged<T>(items: readonly T[], perPage: number) {
  const [page, setPage] = useState(0)
  const pages = Math.max(1, Math.ceil(items.length / perPage))
  const current = Math.min(page, pages - 1)
  const start = current * perPage
  return {
    page: current,
    pages,
    total: items.length,
    start,
    items: items.slice(start, start + perPage),
    setPage,
    next: () => setPage((value) => Math.min(value + 1, pages - 1)),
    previous: () => setPage((value) => Math.max(value - 1, 0)),
  }
}

export function relativeTime(ms: number): string {
  const seconds = Math.max(0, Math.round((Date.now() - ms) / 1000))
  if (seconds < 60) return `${seconds}s ago`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h ago`
  return `${Math.floor(seconds / 86_400)}d ago`
}

export function clockTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour12: false })
}

export function money(value: number | undefined): string {
  return value === undefined ? '—' : value.toFixed(2)
}
