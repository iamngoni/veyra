/**
 * Toasts: short notices about failures the operator should not have to find
 * in the network tab.
 *
 * A toast with a `key` is unique: raising it again refreshes the one shown
 * (and counts the repeat) instead of stacking a copy, so a poll that keeps
 * failing stays one toast. `resolveToast(key)` removes it once the cause has
 * cleared, for example when the same request succeeds again.
 */

export type ToastTone = 'bad' | 'warn' | 'ok'

export type Toast = {
  id: number
  key: string
  tone: ToastTone
  title: string
  detail?: string
  /** How many times this notice was raised while shown. */
  count: number
}

/** Errors stay long enough to read; a hovered toast is kept by the view. */
export const TOAST_MS: Record<ToastTone, number> = { bad: 10_000, warn: 8_000, ok: 4_000 }
/** Older toasts give way beyond this many. */
export const MAX_TOASTS = 4

let toasts: Toast[] = []
let nextId = 1
const listeners = new Set<(current: Toast[]) => void>()

function publish(next: Toast[]) {
  toasts = next
  for (const listener of listeners) listener(toasts)
}

/** Shows a toast, or refreshes the one already shown under the same key. */
export function showToast(notice: { key?: string; tone?: ToastTone; title: string; detail?: string }): number {
  const key = notice.key ?? `${notice.title}\n${notice.detail ?? ''}`
  const tone = notice.tone ?? 'bad'
  const existing = toasts.find((toast) => toast.key === key)
  // A refreshed toast gets a new id so its dismissal timer starts again.
  const toast: Toast = { id: nextId++, key, tone, title: notice.title, detail: notice.detail, count: existing ? existing.count + 1 : 1 }
  publish([...toasts.filter((shown) => shown.key !== key), toast].slice(-MAX_TOASTS))
  return toast.id
}

/** Removes one toast. */
export function dismissToast(id: number) {
  if (toasts.some((toast) => toast.id === id)) publish(toasts.filter((toast) => toast.id !== id))
}

/** Removes the toast raised under `key`, if it is still shown. */
export function resolveToast(key: string) {
  if (toasts.some((toast) => toast.key === key)) publish(toasts.filter((toast) => toast.key !== key))
}

/** The toasts shown now, oldest first. */
export function currentToasts(): Toast[] {
  return toasts
}

/** Follows the shown toasts; returns the unsubscribe. */
export function subscribeToasts(listener: (current: Toast[]) => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

/** Clears every toast; for tests. */
export function clearToasts() {
  publish([])
}
