/**
 * Render tests for the shared primitives: the panel frame and its count chip,
 * the switch, the segmented control and the info mark's tooltip placement.
 */

import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

import { Dot, Hint, Icon, Panel, Segmented, Skeleton, Toggle, signTone } from './ui'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('Panel', () => {
  it('draws the count as a chip while the heading still reads it in parentheses', () => {
    render(
      <Panel title="Open positions" count={3}>
        body
      </Panel>,
    )
    const heading = screen.getByRole('heading', { level: 2 })
    expect(heading.textContent).toBe('Open positions (3)')
    const chip = heading.querySelector('.panel-count') as HTMLElement
    // The chip paints the bare number from the attribute.
    expect(chip.dataset.count).toBe('3')
  })

  it('leaves the chip out without a count and wires actions, rule and level', () => {
    const { container } = render(
      <Panel title="Risk" actions={<button type="button">Edit</button>} divided headingLevel={3} label="Risk card" className="rc-panel">
        body
      </Panel>,
    )
    expect(container.querySelector('.panel-count')).toBeNull()
    expect(screen.getByRole('heading', { level: 3 }).textContent).toBe('Risk')
    expect(container.querySelector('.panel-head')?.className).toBe('panel-head is-divided')
    expect(screen.getByRole('region', { name: 'Risk card' }).className).toBe('panel rc-panel')
    expect(screen.getByRole('button', { name: 'Edit' }).closest('.panel-actions')).not.toBeNull()
  })
})

describe('Toggle', () => {
  it('is a switch whose state lives in aria-checked, not in a printed word', () => {
    const onClick = vi.fn()
    render(<Toggle checked label="Kill switch" onClick={onClick} />)
    const toggle = screen.getByRole('switch', { name: 'Kill switch' })
    expect(toggle.getAttribute('aria-checked')).toBe('true')
    expect(toggle.className).toBe('toggle is-bad')
    expect(toggle.textContent).toBe('')
    fireEvent.click(toggle)
    expect(onClick).toHaveBeenCalledTimes(1)
  })

  it('carries its tone and can be disabled', () => {
    render(<Toggle checked={false} label="Trading" tone="ok" disabled />)
    const toggle = screen.getByRole('switch', { name: 'Trading' }) as HTMLButtonElement
    expect(toggle.getAttribute('aria-checked')).toBe('false')
    expect(toggle.className).toBe('toggle is-ok')
    expect(toggle.disabled).toBe(true)
  })
})

describe('Segmented', () => {
  it('presses only the chosen option and reports a new choice', () => {
    const onChange = vi.fn()
    render(
      <Segmented
        label="Range"
        value={30}
        onChange={onChange}
        options={[
          { value: 7, label: '7D' },
          { value: 30, label: '30D' },
        ]}
      />,
    )
    expect(screen.getByRole('group', { name: 'Range' })).toBeTruthy()
    expect(screen.getByRole('button', { name: '30D' }).getAttribute('aria-pressed')).toBe('true')
    expect(screen.getByRole('button', { name: '7D' }).getAttribute('aria-pressed')).toBe('false')
    fireEvent.click(screen.getByRole('button', { name: '7D' }))
    expect(onChange).toHaveBeenCalledWith(7)
  })
})

describe('Hint', () => {
  /** Puts the hint's box at (left, top) and returns its wrapper. */
  function renderAt(left: number, top: number, tipWidth = 0) {
    render(<Hint text="Blocks new orders." label="Kill switch" />)
    const mark = screen.getByRole('button', { name: 'About Kill switch' })
    const hint = mark.parentElement as HTMLElement
    hint.getBoundingClientRect = () => ({ left, top, right: left + 14, bottom: top + 14, width: 14, height: 14, x: left, y: top, toJSON: () => ({}) })
    Object.defineProperty(screen.getByRole('tooltip', { hidden: true }), 'offsetWidth', { configurable: true, value: tipWidth })
    return { hint, mark }
  }

  it('describes its mark and opens just left of it when there is room', () => {
    vi.stubGlobal('innerWidth', 1512)
    const { hint, mark } = renderAt(400, 500)
    expect(mark.getAttribute('aria-describedby')).toBe(screen.getByRole('tooltip', { hidden: true }).id)
    expect(hint.getAttribute('style')).toBeNull()
    fireEvent.mouseEnter(hint)
    expect(hint.style.getPropertyValue('--tip-x')).toBe('-10px')
    expect(hint.className).toBe('hint')
  })

  it('slides back inside the window near the right edge, by its measured width', () => {
    vi.stubGlobal('innerWidth', 1512)
    const { hint } = renderAt(1400, 500, 200)
    fireEvent.mouseEnter(hint)
    // 1512 − 16 − 200 − 1400
    expect(hint.style.getPropertyValue('--tip-x')).toBe('-104px')
  })

  it('assumes its widest before it has been measured', () => {
    vi.stubGlobal('innerWidth', 1512)
    const { hint } = renderAt(1400, 500)
    fireEvent.mouseEnter(hint)
    // 1512 − 16 − 280 − 1400
    expect(hint.style.getPropertyValue('--tip-x')).toBe('-184px')
  })

  it('never starts past the left edge on a narrow screen', () => {
    vi.stubGlobal('innerWidth', 390)
    const { hint } = renderAt(4, 500)
    act(() => screen.getByRole('button', { name: 'About Kill switch' }).focus())
    expect(hint.style.getPropertyValue('--tip-x')).toBe('12px')
  })

  it('opens below its mark near the top of the window', () => {
    vi.stubGlobal('innerWidth', 1512)
    const { hint } = renderAt(400, 120)
    fireEvent.mouseEnter(hint)
    expect(hint.className).toBe('hint is-below')
  })
})

describe('small primitives', () => {
  it('labels a dot only when asked to', () => {
    const { container } = render(
      <>
        <Dot tone="ok" label="Healthy" />
        <Dot tone="off" />
      </>,
    )
    expect(screen.getByRole('img', { name: 'Healthy' }).className).toBe('dot is-ok')
    expect(container.querySelector('.dot.is-off')?.getAttribute('aria-hidden')).toBe('true')
  })

  it('sizes icons and skeletons as asked', () => {
    const { container } = render(
      <>
        <Icon name="check" size={12} className="extra" />
        <Icon name="info" />
        <Skeleton width={40} height={10} />
      </>,
    )
    const [sized, plain] = container.querySelectorAll('svg')
    expect(sized.getAttribute('class')).toBe('icon extra')
    expect(sized.getAttribute('width')).toBe('12')
    expect(plain.getAttribute('style')).toBeNull()
    const skeleton = container.querySelector('.skeleton') as HTMLElement
    expect(skeleton.style.width).toBe('40px')
    expect(skeleton.style.height).toBe('10px')
  })

  it('tones a signed amount', () => {
    expect(signTone(1)).toBe('tone-ok')
    expect(signTone(-1)).toBe('tone-bad')
    expect(signTone(0)).toBe('')
    expect(signTone(null)).toBe('')
    expect(signTone(Number.NaN)).toBe('')
  })
})
