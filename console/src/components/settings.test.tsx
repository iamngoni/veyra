/**
 * Render tests for the live settings view: plain labels with explanatory
 * hints, the right control per setting, dirty tracking against the service's
 * reading of a value, apply, revert and discard.
 */

import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { subscriptions, type LiveSetting } from '../lib/api'
import { LiveSettingsPanel } from './settings'

beforeEach(() => {
  vi.spyOn(subscriptions, 'status').mockResolvedValue({ subscriptions: { codex: { connected: false }, claude_code: { connected: false } } })
})
afterEach(() => { cleanup(); vi.restoreAllMocks() })

/** A promise the test resolves by hand, to observe in-flight states. */
function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((settle) => {
    resolve = settle
  })
  return { promise, resolve }
}

describe('LiveSettingsPanel', () => {
  const settings = {
    VEYRA_TRADING_ENABLED: { value: 'false', overridden: false },
    VEYRA_AUTOPILOT_PROFIT_HARVEST: { value: 'true', overridden: true },
    VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS: { value: '300', overridden: false },
    VEYRA_AUTOPILOT_TRAIL_R: { value: '', overridden: false },
    VEYRA_MODEL_FALLBACKS: { value: 'z-ai/glm-5.3-flash', overridden: false },
    VEYRA_MODEL_PROVIDER: { value: 'openrouter', overridden: false },
    VEYRA_MARKET_EA_AWAIT_SECS: { value: '5', overridden: false },
  }

  const field = (name: string) => document.querySelector<HTMLInputElement>(`[data-field="${name}"]`)!

  it('labels settings plainly and marks the ones that wait for a restart', () => {
    render(<LiveSettingsPanel settings={settings} onApply={vi.fn()} />)
    expect(screen.getByRole('switch', { name: 'Trading enabled' }).getAttribute('aria-checked')).toBe('false')
    expect(screen.getByLabelText('Trail R')).toBe(field('VEYRA_AUTOPILOT_TRAIL_R'))
    expect(screen.getByRole('switch', { name: 'Profit harvest' }).getAttribute('aria-checked')).toBe('true')
    expect(screen.getByLabelText('Min hold (s)')).toBe(field('VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS'))
    expect(screen.getByLabelText('Fallbacks')).toBe(field('VEYRA_MODEL_FALLBACKS'))
    expect(screen.getByLabelText('Market EA await (s)')).toBe(field('VEYRA_MARKET_EA_AWAIT_SECS'))
    expect(field('VEYRA_AUTOPILOT_TRAIL_R').placeholder).toBe('Not set')
    expect(screen.getAllByText('Applies on restart')).toHaveLength(1)
    // Groups with nothing to show are left out entirely.
    expect(screen.queryByText('Housekeeping')).toBeNull()
  })

  it('shows ChatGPT and Claude subscription connections beside the provider', () => {
    render(<LiveSettingsPanel settings={settings} onApply={vi.fn()} />)
    expect(
      screen.getByText('Connected subscriptions use the linked ChatGPT or Claude account. API providers use developer keys.'),
    ).toBeTruthy()
    expect(screen.getAllByRole('button', { name: /^Connect (ChatGPT|Claude)$/ })).toHaveLength(2)
    const fields = [...document.querySelectorAll('[data-field]')].map((element) => element.getAttribute('data-field'))
    expect(fields.indexOf('VEYRA_MODEL_PROVIDER')).toBeLessThan(fields.indexOf('VEYRA_MODEL_FALLBACKS'))
  })

  it('keeps subscription selection tied to the saved provider and explains model migration', async () => {
    vi.mocked(subscriptions.status).mockResolvedValue({ subscriptions: { codex: { connected: true }, claude_code: { connected: true } } })
    render(<LiveSettingsPanel settings={settings} onApply={vi.fn()} secretStore />)
    await screen.findByRole('button', { name: 'Reconnect ChatGPT' })
    fireEvent.change(field('VEYRA_MODEL_PROVIDER'), { target: { value: 'codex' } })
    expect(screen.queryByText('Selected')).toBeNull()
    expect(screen.getByText(/Replace model IDs and clear or replace fallback IDs/)).toBeTruthy()
    expect(field('VEYRA_MODEL_FALLBACKS').value).toBe('z-ai/glm-5.3-flash')
  })

  it('requires applying a provider draft before saving an API key for it', () => {
    render(<LiveSettingsPanel settings={settings} onApply={vi.fn()} secretStore />)
    expect(screen.getByRole('button', { name: 'Save key' })).toBeTruthy()
    fireEvent.change(field('VEYRA_MODEL_PROVIDER'), { target: { value: 'openai' } })
    expect(screen.queryByRole('button', { name: 'Save key' })).toBeNull()
    expect(screen.getByText('Apply or discard the provider change before editing its API key.')).toBeTruthy()
  })

  it('explains what each setting does from a mark beside its label, not its variable name', () => {
    const { container } = render(<LiveSettingsPanel settings={settings} onApply={vi.fn()} />)
    const help = (label: string) => {
      const mark = screen.getByRole('button', { name: `About ${label}` })
      return document.getElementById(mark.getAttribute('aria-describedby')!)?.textContent
    }
    expect(help('Trading enabled')).toBe(
      'Lets the service send orders to the terminal, closes and stop moves included; off, none are sent. The terminal must also allow live orders.',
    )
    expect(help('Profit harvest')).toBe(
      'Protects profit before take profit: once armed it trails the stop, and closes a trade still in profit that gives back too much of its peak.',
    )
    expect(help('Min hold (s)')).toBe('Minimum age, in seconds, before harvesting may act on a position; empty means 300.')
    expect(help('Trail R')).toBe(
      'Once a trade is this many risk units in profit, keeps the stop that far behind the best price; empty or 0 is off.',
    )
    expect(help('Fallbacks')).toBe(
      "Models tried in order when a tier's own model cannot answer (out of credits, rate limited, rejected). Comma-separated, up to 4.",
    )
    expect(help('Market EA await (s)')).toBe(
      'Seconds to wait for candles from the terminal before they count as unavailable, 5–120; empty means 20. Keep it above 15.',
    )

    // One mark per setting, straight after its label; the aside stays apart.
    const heads = [...container.querySelectorAll('.tab-control-head')]
    expect(heads).toHaveLength(Object.keys(settings).length)
    for (const head of heads) {
      expect(head.firstElementChild?.className).toBe('tab-control-label')
      expect([...head.firstElementChild!.children].map((child) => child.className)).toEqual(['tab-control-name', 'hint'])
    }
    expect(screen.getByText('Overridden').closest('.tab-control-aside')?.previousElementSibling?.className).toBe(
      'tab-control-label',
    )
    // The variable name is no longer anyone's tooltip.
    const titles = [...container.querySelectorAll('[title]')].map((element) => element.getAttribute('title'))
    expect(titles.filter((title) => title?.includes('VEYRA_'))).toEqual([])
  })

  it('has help for every setting it groups', () => {
    // A settings map that answers for any name renders every grouped setting,
    // so a setting added to a group without help fails here.
    const grouped = new Set<string>()
    const everything = new Proxy({} as Record<string, LiveSetting>, {
      get: (_, name) => {
        if (typeof name !== 'string' || !name.startsWith('VEYRA_')) return undefined
        grouped.add(name)
        return { value: '', overridden: false }
      },
    })
    const { container } = render(<LiveSettingsPanel settings={everything} onApply={vi.fn()} />)
    const controls = [...container.querySelectorAll('.tab-control')]
    expect(controls.length).toBe(grouped.size)
    expect(controls.length).toBeGreaterThan(40)

    const name = (control: Element) => control.querySelector('.tab-control-name')?.textContent
    expect(controls.filter((control) => !control.querySelector('.hint')).map(name)).toEqual([])
    const texts = controls.map((control) => control.querySelector('.hint-tip')?.textContent ?? '')
    // Short plain sentences, each its own, and never a variable name.
    expect(texts.filter((text) => text.length > 140 || !text.endsWith('.') || text.includes('VEYRA_'))).toEqual([])
    expect(new Set(texts).size).toBe(texts.length)
  })

  it('sends only the fields that actually changed', async () => {
    const pending = deferred<string | undefined>()
    const onApply = vi.fn().mockReturnValue(pending.promise)
    const onRefresh = vi.fn()
    render(<LiveSettingsPanel settings={settings} onApply={onApply} onRefresh={onRefresh} />)

    fireEvent.change(field('VEYRA_AUTOPILOT_TRAIL_R'), { target: { value: '1.5' } })
    // Flipped and flipped back: not a change.
    const trading = screen.getByRole('switch', { name: 'Trading enabled' })
    fireEvent.click(trading)
    fireEvent.click(trading)
    expect(screen.getByText('1 unsaved')).toBeTruthy()
    expect(field('VEYRA_AUTOPILOT_TRAIL_R').className).toBe('tab-input is-dirty')
    expect(trading.closest('.tab-setting-switch')?.className).toBe('tab-setting-switch')

    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    expect(onApply).toHaveBeenCalledWith({ VEYRA_AUTOPILOT_TRAIL_R: '1.5' })
    expect(screen.getByRole('button', { name: 'Applying…' })).toBeTruthy()
    expect(field('VEYRA_AUTOPILOT_TRAIL_R').disabled).toBe(true)

    await act(async () => pending.resolve(undefined))
    expect(screen.getByText('Applied')).toBeTruthy()
    expect(onRefresh).toHaveBeenCalledOnce()
  })

  it('holds skeleton rows before the config response arrives', () => {
    const { container } = render(<LiveSettingsPanel />)
    expect(screen.getByText('Live settings')).toBeTruthy()
    expect(container.querySelectorAll('.tab-skeleton-row').length).toBe(3)
  })

  it('keeps the draft on screen when the service refuses it', async () => {
    const onApply = vi.fn().mockResolvedValue('VEYRA_AUTOPILOT_TRAIL_R: must be a finite positive number')
    render(<LiveSettingsPanel settings={settings} onApply={onApply} />)

    fireEvent.change(field('VEYRA_AUTOPILOT_TRAIL_R'), { target: { value: '99' } })
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    const alert = await screen.findByRole('alert')
    expect(alert.textContent).toContain('must be a finite positive number')
    expect(field('VEYRA_AUTOPILOT_TRAIL_R').value).toBe('99')
  })

  it('clears an override with null rather than an empty string', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    const onRefresh = vi.fn()
    render(<LiveSettingsPanel settings={settings} onApply={onApply} onRefresh={onRefresh} />)

    const harvest = screen.getByRole('switch', { name: 'Profit harvest' })
    fireEvent.click(harvest)
    expect(harvest.getAttribute('aria-checked')).toBe('false')
    expect(screen.getByText('Overridden')).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Revert' }))

    expect(onApply).toHaveBeenCalledWith({ VEYRA_AUTOPILOT_PROFIT_HARVEST: null })
    await vi.waitFor(() => expect(onRefresh).toHaveBeenCalledOnce())
    // The reverted field drops its draft and shows the stored value again.
    expect(screen.getByRole('switch', { name: 'Profit harvest' }).getAttribute('aria-checked')).toBe('true')
  })

  it('leaves a failed override revert visible', async () => {
    const onApply = vi.fn().mockResolvedValue('cannot revert')
    render(<LiveSettingsPanel settings={settings} onApply={onApply} />)
    fireEvent.click(screen.getByRole('button', { name: 'Revert' }))
    expect((await screen.findByRole('alert')).textContent).toContain('cannot revert')
    expect(onApply).toHaveBeenCalledWith({ VEYRA_AUTOPILOT_PROFIT_HARVEST: null })
  })

  it('discards a changed draft without sending it', () => {
    const onApply = vi.fn()
    render(<LiveSettingsPanel settings={settings} onApply={onApply} />)
    expect(screen.queryByRole('button', { name: 'Discard' })).toBeNull()
    fireEvent.change(field('VEYRA_AUTOPILOT_TRAIL_R'), { target: { value: '1.5' } })
    fireEvent.click(screen.getByRole('button', { name: 'Discard' }))
    expect(onApply).not.toHaveBeenCalled()
    expect(field('VEYRA_AUTOPILOT_TRAIL_R').value).toBe('')
  })

  it('keeps apply and revert inert without an apply handler', () => {
    render(<LiveSettingsPanel settings={settings} />)
    fireEvent.change(field('VEYRA_AUTOPILOT_TRAIL_R'), { target: { value: '1.5' } })
    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    fireEvent.click(screen.getByRole('button', { name: 'Revert' }))
    expect(screen.queryByRole('alert')).toBeNull()
    expect(screen.getByText('1 unsaved')).toBeTruthy()
  })

  it('flips a flag in the draft and applies it only with the rest', async () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(<LiveSettingsPanel settings={settings} onApply={onApply} />)
    const trading = screen.getByRole('switch', { name: 'Trading enabled' })

    fireEvent.click(trading)
    expect(trading.getAttribute('aria-checked')).toBe('true')
    expect(trading.closest('.tab-setting-switch')?.className).toBe('tab-setting-switch is-dirty')
    // A switch never reaches the service by itself.
    expect(onApply).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole('button', { name: 'Apply' }))
    expect(onApply).toHaveBeenCalledWith({ VEYRA_TRADING_ENABLED: 'true' })
    await screen.findByText('Applied')
  })

  it('shows what an unset setting does, and offers only accepted values', () => {
    const onApply = vi.fn().mockResolvedValue(undefined)
    render(
      <LiveSettingsPanel
        settings={{
          VEYRA_MODEL_COMPEL_STRUCTURED: { value: '', overridden: false },
          VEYRA_AUTOPILOT_TIER: { value: '', overridden: false },
          VEYRA_AUTOPILOT_TIMEFRAME: { value: '240', overridden: false },
          VEYRA_AUTOPILOT_JEV: { value: 'auto', overridden: false },
        }}
        onApply={onApply}
      />,
    )
    // Unset compels a structured answer, so the switch reads on.
    expect(screen.getByRole('switch', { name: 'Compel structured' }).getAttribute('aria-checked')).toBe('true')

    const tier = screen.getByLabelText('Tier') as HTMLSelectElement
    expect(tier.value).toBe('balanced')
    expect([...tier.options].map((option) => option.value)).toEqual(['fast', 'balanced', 'reasoning'])
    // Choosing the behaviour already in force is not a change.
    fireEvent.change(tier, { target: { value: 'balanced' } })
    expect(screen.queryByText(/unsaved/)).toBeNull()
    fireEvent.change(tier, { target: { value: 'reasoning' } })
    expect(tier.className).toBe('tab-input is-dirty')
    expect(screen.getByText('1 unsaved')).toBeTruthy()

    // A timeframe given in minutes is kept, not replaced by the menu.
    const timeframe = screen.getByLabelText('Timeframe') as HTMLSelectElement
    expect(timeframe.value).toBe('240')
    expect([...timeframe.options].map((option) => option.value)).toContain('H4')
    expect([...(screen.getByLabelText('JEV') as HTMLSelectElement).options].map((option) => option.label)).toEqual([
      'Auto',
      'Off',
    ])
  })

  it('offers model providers while keeping single-value integrations fixed', () => {
    render(
      <LiveSettingsPanel
        settings={{
          VEYRA_MODEL_PROVIDER: { value: '', overridden: false },
          VEYRA_JEV_PROVIDER: { value: 'typesafe', overridden: true },
          VEYRA_MARKET_PROVIDER: { value: '', overridden: false },
        }}
        onApply={vi.fn()}
      />,
    )
    const provider = document.querySelector<HTMLSelectElement>('[data-field="VEYRA_MODEL_PROVIDER"]')
    expect(provider?.value).toBe('openrouter')
    expect(provider?.querySelector('option[value="custom"]')).toBeTruthy()
    expect(screen.getByText('typesafe')).toBeTruthy()
    // A market provider left empty means none runs.
    expect(screen.getByText('Not set')).toBeTruthy()
    // An override can still be cleared.
    expect(screen.getByRole('button', { name: 'Revert' })).toBeTruthy()
  })

  it('marks only the values an operator has moved', () => {
    render(<LiveSettingsPanel settings={settings} onApply={vi.fn()} />)
    expect(screen.getAllByRole('button', { name: 'Revert' })).toHaveLength(1)
    expect(screen.getByRole('button', { name: 'Revert' }).getAttribute('title')).toBe('Return to the deployed value')
  })
})
