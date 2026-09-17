import { useEffect, useRef, useState } from 'react'

import { api, type FeedEvent } from './api'

/** Polls an async source on an interval, keeping the last good value on error. */
export function usePoll<T>(load: () => Promise<T>, intervalMs: number) {
  const [data, setData] = useState<T>()
  const [error, setError] = useState<string>()
  const loadRef = useRef(load)
  loadRef.current = load

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
    void tick()
    const timer = setInterval(tick, intervalMs)
    return () => {
      alive = false
      clearInterval(timer)
    }
  }, [intervalMs])

  return { data, error }
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
