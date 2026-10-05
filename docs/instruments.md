# Choosing the autopilot's instruments

Settings -> Autopilot -> **Instruments** is a multi-select over every
instrument the connected broker offers, with a search box and one filter per
kind of market (Forex, Indices, Metals, Energies, Commodities, Crypto, Stocks,
Bonds, Other). Up to 16 can be chosen; the autopilot picks at most one per
cycle from them.

## Where the list comes from

The list is the broker's own. The terminal knows what the account's server
offers, so the service asks it once and keeps the answer:

1. The EA answers the `list_symbols` command with one page (up to 200) of
   `SymbolsTotal(false)` entries: name, description, and the broker's folder
   path (for example `Forex\Majors\EURUSD`).
2. The service pulls every page when a terminal first connects, again if the
   account moves to another broker server, when the list is a day old, or when
   an operator presses **Reload from broker**. All other reads come from
   memory, so the picker always has data and never waits on MetaTrader.
3. The console keeps its own copy for the page (and in session storage across
   reloads). A later "not ready" answer never replaces a list it already holds.

Market type comes from the broker's folder path first (`Indices`, `Metals`,
`Crypto`, ...); the symbol's own name decides only when the path says nothing.
Names Veyra cannot trade (anything outside letters, digits and `. _ # + -`, or
longer than 24 characters) and duplicates are left out and counted.

## What choosing does and does not do

Choosing an instrument only names it for the autopilot. **What may actually
trade is still the risk gate's allowlist.** Instruments the gate would refuse
are marked "not in risk gate" in the list and flagged under the field, with a
button that adds exactly the listed instruments to the allowlist. Nothing is
added without that click.

The service accepts either one `Symbol` or a `Symbols` list, never both. The
picker owns both settings: choosing instruments clears the single `Symbol` in
the same Apply, and choosing back what is already deployed discards the draft.

## API

| Route | Does |
| --- | --- |
| `GET /symbols` | The catalogue: `ready`, `server`, `fetchedAt`, `categories` with counts, and every instrument with its category and whether the risk gate accepts it. Always answers; `ready: false` carries a `reason`. |
| `POST /symbols/refresh` | Pulls the list from the terminal now. 503 with a reason when no terminal is connected. |

## Needs EA 1.27 or newer

An older EA answers `list_symbols` with "unsupported command" and the picker
shows its reason. Recompile (`scripts/compile_ea.sh`) and reload the EA on
every machine that runs MetaTrader 4, and check that **InAllowLiveOrders**
still shows what you intend after the reload.
