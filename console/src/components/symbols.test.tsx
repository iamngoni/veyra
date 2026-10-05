/**
 * Render tests for the instrument picker: the chosen instruments as chips, the
 * broker's list with a filter per market and a search, the sixteen-instrument
 * cap, the risk-gate marker and its explicit allow button, and the fallbacks
 * for when the terminal has not yet listed its instruments.
 */

import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { useState } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { api, type CatalogSymbol, type SymbolCatalog } from '../lib/api'
import { forgetSymbolCatalog } from '../lib/hooks'
import { joinSymbolList, MAX_AUTOPILOT_SYMBOLS, parseSymbolList, SymbolPicker } from './symbols'

function entry(name: string, category: CatalogSymbol['category'], riskAllowed = true, description = ''): CatalogSymbol {
  return { name, description, path: `${category}\\${name}`, category, riskAllowed }
}

function readyCatalog(symbols: CatalogSymbol[], fetchedAgoSecs = 120): SymbolCatalog {
  const counts = new Map<CatalogSymbol['category'], number>()
  for (const symbol of symbols) counts.set(symbol.category, (counts.get(symbol.category) ?? 0) + 1)
  return {
    ready: true,
    server: 'Demo-Server',
    fetchedAt: Math.floor(Date.now() / 1000) - fetchedAgoSecs,
    total: symbols.length,
    skipped: 0,
    categories: [...counts].map(([id, count]) => ({ id, label: id.charAt(0).toUpperCase() + id.slice(1), count })),
    symbols,
  }
}

const MARKETS = [
  entry('EURUSD', 'forex', true, 'Euro vs US Dollar'),
  entry('GBPUSD', 'forex', false, 'Pound vs US Dollar'),
  entry('US30', 'indices', false, 'Wall Street 30'),
  entry('XAUUSD', 'metals', true, 'Gold'),
  entry('BTCUSD', 'crypto', false, 'Bitcoin'),
]

beforeEach(() => {
  forgetSymbolCatalog()
})
afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
})

/** A controlled host, as the settings panel is. */
function Harness({ initial = '', onValue }: { initial?: string; onValue?: (value: string) => void }) {
  const [value, setValue] = useState(initial)
  return (
    <SymbolPicker
      label="Instruments"
      value={value}
      onChange={(next) => {
        setValue(next)
        onValue?.(next)
      }}
    />
  )
}

async function openPicker() {
  fireEvent.click(screen.getByRole('button', { name: /Choose instruments|Add or remove/ }))
  return screen.findByRole('dialog', { name: 'Choose instruments' })
}

describe('list helpers', () => {
  it('parses and joins lists, dropping blanks and case-insensitive repeats', () => {
    expect(parseSymbolList(' eurusd, GBPUSD ,,EURUSD, XAUUSD ')).toEqual(['eurusd', 'GBPUSD', 'XAUUSD'])
    expect(parseSymbolList('')).toEqual([])
    expect(joinSymbolList(['EURUSD', 'GBPUSD'])).toBe('EURUSD,GBPUSD')
    expect(MAX_AUTOPILOT_SYMBOLS).toBe(16)
  })
})

describe('SymbolPicker', () => {
  it('shows the chosen instruments as removable chips, or says none are chosen', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    const values: string[] = []
    const { unmount } = render(<Harness initial="" onValue={(value) => values.push(value)} />)
    expect(screen.getByText('No instruments chosen')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Choose instruments' })).toBeTruthy()
    unmount()

    render(<Harness initial="EURUSD,XAUUSD" onValue={(value) => values.push(value)} />)
    expect(screen.getByText('EURUSD')).toBeTruthy()
    expect(screen.getByText('XAUUSD')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Add or remove' })).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Remove EURUSD' }))
    expect(values).toEqual(['XAUUSD'])
    await waitFor(() => expect(api.symbols).toHaveBeenCalled())
  })

  it('opens a list of the broker instruments with a count for each market', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    render(<Harness />)
    const dialog = await openPicker()
    expect(await within(dialog).findByText('5 instruments from Demo-Server, loaded 2 min ago')).toBeTruthy()

    const filters = within(dialog).getByRole('group', { name: 'Filter by market' })
    expect(within(filters).getByRole('button', { name: 'All 5' }).getAttribute('aria-pressed')).toBe('true')
    expect(within(filters).getByRole('button', { name: 'Forex 2' })).toBeTruthy()
    expect(within(filters).getByRole('button', { name: 'Indices 1' })).toBeTruthy()
    expect(within(filters).getByRole('button', { name: 'Crypto 1' })).toBeTruthy()
    expect(within(dialog).getAllByRole('checkbox')).toHaveLength(5)
    expect(within(dialog).getByText('Euro vs US Dollar')).toBeTruthy()
  })

  it('narrows the list by market and by search', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    render(<Harness />)
    const dialog = await openPicker()
    await within(dialog).findByText(/5 instruments/)

    fireEvent.click(within(dialog).getByRole('button', { name: 'Forex 2' }))
    expect(within(dialog).getAllByRole('checkbox')).toHaveLength(2)
    expect(within(dialog).getByRole('button', { name: 'Forex 2' }).getAttribute('aria-pressed')).toBe('true')

    fireEvent.click(within(dialog).getByRole('button', { name: 'All 5' }))
    fireEvent.change(within(dialog).getByLabelText('Search instruments'), { target: { value: 'gold' } })
    expect(within(dialog).getAllByRole('checkbox')).toHaveLength(1)
    expect(within(dialog).getByText('XAUUSD')).toBeTruthy()

    // The folder path is searched too.
    fireEvent.change(within(dialog).getByLabelText('Search instruments'), { target: { value: 'indices\\' } })
    expect(within(dialog).getByText('US30')).toBeTruthy()

    fireEvent.change(within(dialog).getByLabelText('Search instruments'), { target: { value: 'zzz' } })
    expect(within(dialog).getByText('Nothing matches that search.')).toBeTruthy()
  })

  it('picks and unpicks instruments from the list', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    const values: string[] = []
    render(<Harness initial="EURUSD" onValue={(value) => values.push(value)} />)
    const dialog = await openPicker()
    await within(dialog).findByText(/5 instruments/)

    fireEvent.click(within(dialog).getByRole('checkbox', { name: /XAUUSD/ }))
    fireEvent.click(within(dialog).getByRole('checkbox', { name: /EURUSD/ }))
    expect(values).toEqual(['EURUSD,XAUUSD', 'XAUUSD'])
    expect(within(dialog).getByText('1 of 16 chosen')).toBeTruthy()

    fireEvent.click(within(dialog).getByRole('button', { name: 'Clear' }))
    expect(values.at(-1)).toBe('')
    expect(within(dialog).getByText('0 of 16 chosen')).toBeTruthy()
  })

  it('stops at sixteen instruments', async () => {
    const many = Array.from({ length: 20 }, (_, index) => entry(`PAIR${String(index + 1).padStart(2, '0')}`, 'forex'))
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(many))
    const initial = many.slice(0, 16).map((symbol) => symbol.name).join(',')
    render(<Harness initial={initial} />)
    const dialog = await openPicker()
    await within(dialog).findByText(/20 instruments/)

    expect(within(dialog).getByText('16 of 16 chosen')).toBeTruthy()
    const boxes = within(dialog).getAllByRole('checkbox') as HTMLInputElement[]
    expect(boxes.filter((box) => !box.checked).every((box) => box.disabled)).toBe(true)
    expect(boxes.filter((box) => box.checked).every((box) => !box.disabled)).toBe(true)
  })

  it('pages a very long list so every instrument can be reached', async () => {
    const huge = Array.from({ length: 120 }, (_, index) => entry(`SYM${String(index).padStart(3, '0')}`, 'stocks'))
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(huge))
    render(<Harness />)
    const dialog = await openPicker()
    expect(await within(dialog).findByText('1–50 of 120')).toBeTruthy()
    expect(within(dialog).getAllByRole('checkbox')).toHaveLength(50)
    expect((within(dialog).getByRole('button', { name: 'Previous' }) as HTMLButtonElement).disabled).toBe(true)

    fireEvent.click(within(dialog).getByRole('button', { name: 'Next' }))
    expect(within(dialog).getByText('51–100 of 120')).toBeTruthy()
    fireEvent.click(within(dialog).getByRole('button', { name: 'Next' }))
    expect(within(dialog).getByText('101–120 of 120')).toBeTruthy()
    expect(within(dialog).getAllByRole('checkbox')).toHaveLength(20)
    expect((within(dialog).getByRole('button', { name: 'Next' }) as HTMLButtonElement).disabled).toBe(true)

    fireEvent.change(within(dialog).getByLabelText('Search instruments'), { target: { value: 'SYM11' } })
    expect(within(dialog).queryByRole('navigation', { name: 'Instrument pages' })).toBeNull()
    expect(within(dialog).getAllByRole('checkbox')).toHaveLength(10)
  })

  it('marks instruments the risk gate would refuse and allows them only on request', async () => {
    const before = readyCatalog(MARKETS)
    const after = readyCatalog(MARKETS.map((symbol) => (symbol.name === 'GBPUSD' ? { ...symbol, riskAllowed: true } : symbol)))
    const symbols = vi.spyOn(api, 'symbols').mockResolvedValueOnce(before).mockResolvedValue(after)
    vi.spyOn(api, 'riskPolicy').mockResolvedValue({ symbols: ['EURUSD'] } as Awaited<ReturnType<typeof api.riskPolicy>>)
    const update = vi.spyOn(api, 'updatePolicy').mockResolvedValue({} as Awaited<ReturnType<typeof api.updatePolicy>>)
    render(<Harness initial="EURUSD,GBPUSD" />)

    expect(await screen.findByText(/GBPUSD is not on the risk gate's allowed list/)).toBeTruthy()
    expect(update).not.toHaveBeenCalled()
    fireEvent.click(screen.getByRole('button', { name: 'Allow GBPUSD in the risk gate' }))

    await waitFor(() => expect(update).toHaveBeenCalledWith({ symbols: ['EURUSD', 'GBPUSD'] }))
    expect(await screen.findByText("Added GBPUSD to the risk gate's allowed instruments.")).toBeTruthy()
    await waitFor(() => expect(screen.queryByText(/is not on the risk gate/)).toBeNull())
    expect(symbols).toHaveBeenCalledTimes(2)
  })

  it('reports a failure to change the risk gate instead of pretending', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    vi.spyOn(api, 'riskPolicy').mockRejectedValue(new Error('/risk/policy → 502'))
    const update = vi.spyOn(api, 'updatePolicy')
    render(<Harness initial="GBPUSD,US30" />)

    expect(await screen.findByText(/GBPUSD, US30 are not on the risk gate's allowed list/)).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Allow GBPUSD, US30 in the risk gate' }))
    expect(await screen.findByText('/risk/policy → 502')).toBeTruthy()
    expect(update).not.toHaveBeenCalled()
  })

  it('lets an instrument be typed in while the terminal has not listed any', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue({ ready: false, reason: 'waiting for the terminal', categories: [], symbols: [] })
    const values: string[] = []
    render(<Harness onValue={(value) => values.push(value)} />)
    const dialog = await openPicker()
    expect(await within(dialog).findByText('Waiting for the terminal to list its instruments (waiting for the terminal).')).toBeTruthy()
    expect(within(dialog).queryByRole('group', { name: 'Filter by market' })).toBeNull()

    const search = within(dialog).getByLabelText('Search instruments')
    fireEvent.change(search, { target: { value: 'bad$name' } })
    expect(within(dialog).queryByRole('button', { name: /by name/ })).toBeNull()

    fireEvent.change(search, { target: { value: 'US500.cash' } })
    fireEvent.click(within(dialog).getByRole('button', { name: 'Add “US500.cash” by name' }))
    expect(values).toEqual(['US500.cash'])
    expect((search as HTMLInputElement).value).toBe('')
  })

  it('tells the operator when the MetaTrader EA is too old to list instruments', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue({ ready: false, reason: 'unsupported command', categories: [], symbols: [] })
    render(<Harness />)
    const dialog = await openPicker()
    expect(
      await within(dialog).findByText(
        'The MetaTrader EA is too old to list instruments. Recompile VeyraProbe (version 1.27 or newer) and reload it on the chart.',
      ),
    ).toBeTruthy()
    // Typing an instrument in still works meanwhile.
    fireEvent.change(within(dialog).getByLabelText('Search instruments'), { target: { value: 'EURUSD' } })
    expect(within(dialog).getByRole('button', { name: 'Add “EURUSD” by name' })).toBeTruthy()
  })

  it('says when the list cannot be reached at all', async () => {
    vi.spyOn(api, 'symbols').mockRejectedValue(new Error('/symbols → 503'))
    render(<Harness />)
    const dialog = await openPicker()
    expect(await within(dialog).findByText('Instrument list unavailable: /symbols → 503')).toBeTruthy()
  })

  it('reloads the list from the broker on request', async () => {
    const symbols = vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    const refresh = vi.spyOn(api, 'refreshSymbols').mockResolvedValue({ status: 'refreshed', count: 5 })
    render(<Harness />)
    const dialog = await openPicker()
    await within(dialog).findByText(/5 instruments/)

    fireEvent.click(within(dialog).getByRole('button', { name: 'Reload from broker' }))
    await waitFor(() => expect(refresh).toHaveBeenCalledTimes(1))
    await waitFor(() => expect(symbols.mock.calls.length).toBeGreaterThanOrEqual(2))
    await within(dialog).findByRole('button', { name: 'Reload from broker' })

    // A refused reload is reported, not swallowed.
    refresh.mockRejectedValueOnce(new Error('the terminal is not connected'))
    fireEvent.click(within(dialog).getByRole('button', { name: 'Reload from broker' }))
    expect(await within(dialog).findByRole('alert')).toHaveProperty(
      'textContent',
      'Could not reload the list: the terminal is not connected',
    )
    // The list already held stays on screen while the reload is refused.
    expect(within(dialog).getAllByRole('checkbox')).toHaveLength(5)
  })

  it('closes on Escape, on a click outside, and on Done', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    render(<Harness />)
    let dialog = await openPicker()
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(screen.queryByRole('dialog')).toBeNull()

    dialog = await openPicker()
    fireEvent.mouseDown(document.body)
    expect(screen.queryByRole('dialog')).toBeNull()

    dialog = await openPicker()
    fireEvent.click(within(dialog).getByRole('button', { name: 'Done' }))
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('does nothing while disabled', async () => {
    vi.spyOn(api, 'symbols').mockResolvedValue(readyCatalog(MARKETS))
    render(
      <SymbolPicker label="Instruments" value="EURUSD" disabled onChange={() => undefined} />,
    )
    expect((screen.getByRole('button', { name: 'Add or remove' }) as HTMLButtonElement).disabled).toBe(true)
    expect((screen.getByRole('button', { name: 'Remove EURUSD' }) as HTMLButtonElement).disabled).toBe(true)
    await act(async () => undefined)
  })
})
