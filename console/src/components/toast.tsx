import { useEffect, useState } from 'react'

import { TOAST_MS, currentToasts, dismissToast, subscribeToasts, type Toast } from '../lib/toast'
import { Icon } from './ui'
import '../styles/toast.css'

/** One toast: dismissed by its button or after its tone's time, kept while hovered or focused. */
function ToastItem({ toast }: { toast: Toast }) {
  const [held, setHeld] = useState(false)

  useEffect(() => {
    if (held) return
    const timer = setTimeout(() => dismissToast(toast.id), TOAST_MS[toast.tone])
    return () => clearTimeout(timer)
  }, [toast.id, toast.tone, held])

  return (
    <li
      className={`toast tone-${toast.tone}`}
      role={toast.tone === 'bad' ? 'alert' : 'status'}
      onMouseEnter={() => setHeld(true)}
      onMouseLeave={() => setHeld(false)}
      onFocus={() => setHeld(true)}
      onBlur={() => setHeld(false)}
    >
      <span className="toast-dot" aria-hidden="true" />
      <div className="toast-body">
        <p className="toast-title">
          {toast.title}
          {toast.count > 1 ? <span className="toast-count"> ×{toast.count}</span> : null}
        </p>
        {toast.detail ? <p className="toast-detail">{toast.detail}</p> : null}
      </div>
      <button type="button" className="toast-close" onClick={() => dismissToast(toast.id)} title="Dismiss" aria-label="Dismiss">
        <Icon name="close" size={14} />
      </button>
    </li>
  )
}

/** Every toast raised anywhere in the console, bottom right, newest last. */
export function Toaster() {
  const [toasts, setToasts] = useState<Toast[]>(currentToasts)

  useEffect(() => subscribeToasts(setToasts), [])

  return (
    <ol className="toaster" aria-label="Notifications" aria-live="polite">
      {toasts.map((toast) => (
        <ToastItem key={toast.id} toast={toast} />
      ))}
    </ol>
  )
}
