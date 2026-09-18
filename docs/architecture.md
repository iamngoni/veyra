# Veyra architecture

Veyra is an autonomous MetaTrader 4 trading service: a Rust/Actix process that decides on a cadence, but can only act through a deterministic risk gate and two independent arming switches. Every external integration sits behind a narrow trait selected by configuration, and every decision, command, and acknowledgement is journaled.

This page describes the code as it is in this repository: what is live, how the pieces fit, and where to change things.

## What Veyra is

- **An autonomous MT4 trading service** (`crates/veyra-service`) behind a loopback control surface. The MT4 terminal holds the broker session; the service never sees broker credentials.
- **Two independent arming switches** in front of real money: `VEYRA_TRADING_ENABLED` in the service and the EA's `InAllowLiveOrders` input (armed at build time via `VEYRA_EA_ALLOW_LIVE` and `scripts/compile_ea.sh`). Either one alone yields dry runs or rejections, never an order.
- **A deterministic risk gate** (`risk/gate.rs`): the only authority that mints an executable intent. Model confidence cannot override limits.
- **Provider-neutral integration** — broker, market data, decision model, judgements, and audit storage are traits with configuration-selected implementations.

Status at a glance (see `README.md` and `docs/roadmap.md` for evidence):

| Area | State |
| --- | --- |
| EA control channel | Live-proven: heartbeat, ping/pong, `account_snapshot`, `rates`, and `symbol_spec` round trips through the tunnel |
| Venue contracts (`symbol_spec`) | Live-proven against IFC Markets: spread, stop level, lot band, margin per lot, and swap rates for every menu symbol; pre-queue lot/margin/stop checks and a console-editable ATR(14) noise floor |
| Decision model | Live-proven structured answers over the OpenRouter preset |
| Jev judgements | Live-proven (`jev-1.13.0`, ~1.3 s per request) |
| Economic calendar | Live-proven (ForexFactory weekly export; high-impact blackout and per-asset event context) |
| Risk gate + `order_check` | Live-proven (retcode 0 and 129 for validation, without sending an order) |
| Autonomous entry | Live-proven first autonomous order (EURUSD 0.01 sell, ticket 10650805, retcode 0) after explicit owner approval of both switches |
| Audit trail, console, alerting, launchd supervision, backups | Running under supervision on this machine |
| Durable always-on host, managed secrets, remote monitoring, versioned deploys | Open roadmap items (`docs/deployment.md` prepares the move) |

## Runtime topology

| Piece | Role | Where |
| --- | --- | --- |
| Service (`veyra-service`) | configuration, risk gate, command queue, autopilot, HTTP surface | Rust 2024 + Actix Web + Tokio, `crates/veyra-service` |
| Diagnostics/control listener | `/health`, `/ready`, `/status`, `/metrics`, `/intents/*`, `/commands*`, `/events`, `/logs`, `/audit`, `/account`, `/market/candles`, `/market/spec`, `/calendar`, `/reconciliation` | `127.0.0.1:8080` (`VEYRA_BIND_HOST`/`VEYRA_BIND_PORT`) |
| EA channel listener | token-authenticated `POST /ea/poll` carrying heartbeats and the command queue | `127.0.0.1:7801` (`VEYRA_EA_BIND_*`); non-loopback binds are rejected at startup |
| MT4 terminal + `VeyraProbe` EA | holds the broker session, polls the channel, executes acknowledged commands, reports dry runs while disarmed | `ea/VeyraProbe.mq4` inside MetaTrader 4 (Wine) |
| Cloudflare tunnel | `veyra.antonlabs.cc` → `127.0.0.1:7801` — the EA channel only | launchd agent, `KeepAlive` |
| PostgreSQL | append-only `audit_events` via SQLx (`migrations/0001_audit_events.sql`) | `VEYRA_DATABASE_URL`; unreachable configured database fails startup |
| Console | operations UI reading the loopback service through its own `/api` proxy | TanStack Start + React + Tailwind, `127.0.0.1:3000` |
| launchd agents | terminal, tunnel, service, console, alert probe, log rotation, audit backups | `scripts/launchd/`, `scripts/install-launchd.sh` |

```mermaid
flowchart LR
    subgraph mac["Supervised Mac (launchd)"]
        Browser["Browser<br/>http://127.0.0.1:3000"]
        Console["Console — TanStack Start<br/>Vite preview · no auth"]
        Service["veyra-service (Rust · Actix Web · Tokio)<br/>diagnostics 127.0.0.1:8080<br/>EA channel 127.0.0.1:7801"]
        EA["VeyraProbe EA (MQL4)"]
        MT4["MetaTrader 4 terminal (Wine)"]
        PG[("PostgreSQL 17<br/>audit_events")]
        Tunnel["cloudflared tunnel<br/>veyra.antonlabs.cc"]
    end

    Model["OpenRouter — DecisionEngine"]
    Jev["TypeSafe Jev — SemanticJudge"]
    Broker[("Broker — IFC Markets")]

    Browser -->|"loads UI"| Console
    Console -->|"proxies /api to 127.0.0.1:8080"| Service

    EA -->|"HTTPS POST /ea/poll (WebRequest + token)"| Tunnel
    Tunnel -->|"127.0.0.1:7801"| Service
    Service -->|"poll reply: cmd / ping / none"| EA
    MT4 -.->|"hosts"| EA
    MT4 -->|"orders and prices"| Broker

    Service -->|"structured proposals (HTTPS)"| Model
    Service -->|"judgement requests (HTTPS)"| Jev
    Service -->|"append-only audit events"| PG
```

Operational notes:

- Everything is deployed loopback-only except the tunnel: the EA channel rejects non-loopback binds at startup, and the diagnostics/control bind is configured to `127.0.0.1` and must not be exposed directly. The tunnel carries the EA channel only; diagnostics are never exposed publicly. Exposing `/account` or the console beyond loopback requires authentication first.
- MQL4 has no sockets and `WebRequest` only supports the scheme-default port, so the channel runs over HTTPS (443) to the tunnel; see ADR 0002.
- The EA polls about once per second; state older than 10 s is stale. Commands are typed, delivered on a poll, redelivered until acknowledged, and fail after 15 s.
- The service runs both listeners from one process: the main Actix app on 8080 and a one-route Actix app on 7801; the companion listener is stopped with the main server.
- The service tees its structured tracing events into a bounded in-process ring (2,048 records) served at `GET /logs`; like the event feed it is process-lifetime and carries no request bodies, credentials, or account data.

## Provider abstraction model

Every integration follows the same five-part shape: **a narrow trait + a provider enum with `parse()` + a validated settings parser + a runtime factory chosen by an environment variable + shared contract tests**. Construction happens once at startup; callers hold `Arc<dyn Trait>` and never see vendor types.

| Integration | Env var | Value | Contract | Selector | Settings parser | Runtime factory | Implementation |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Broker | `VEYRA_BROKER_PROVIDER` | `ea` | `BrokerLink` (`broker/mod.rs`) with the neutral command/report types in `broker/command.rs` | `BrokerProvider` | `broker/settings.rs` (`BrokerSettings::from_source`) | `BrokerRuntime::from_settings` (`broker/mod.rs`) | `broker/ea.rs` (`EaLink`, implementing `BrokerLink`) |
| Market data | `VEYRA_MARKET_PROVIDER` | `ea` | `MarketFeed` (`market/mod.rs`) | `MarketProvider` | `market/settings.rs` (`MarketSettings::from_source`) | `MarketRuntime::from_settings` (`market/mod.rs`) | `market/ea.rs` (`EaMarketFeed`) |
| Decision model | `VEYRA_MODEL_PROVIDER` | `openrouter` | `DecisionEngine` (`model/mod.rs`) | `ModelProvider` | `model/settings.rs` (`ModelSettings::from_source`) | `ModelRuntime::from_settings` (`model/mod.rs`) | `model/agent_runtime_engine.rs` (`AgentRuntimeEngine`), wrapped in `BudgetedEngine` (`model/budget.rs`) |
| Judgements | `VEYRA_JEV_PROVIDER` | `typesafe` | `SemanticJudge` (`jev/mod.rs`) | `JevProvider` | `jev/settings.rs` (`JevSettings::from_source`) | `JevRuntime::from_settings` (`jev/mod.rs`) | `jev/http.rs` (`HttpJev`); contract types in `jev/contract.rs` |
| Economic calendar | `VEYRA_CALENDAR_PROVIDER` | `forexfactory` | `EventCalendar` (`calendar/mod.rs`) | `CalendarProvider` | `calendar/settings.rs` (`CalendarSettings::from_source`) | `CalendarRuntime::from_settings` (`calendar/mod.rs`) | `calendar/forexfactory.rs` (`ForexfactoryCalendar`, weekly JSON export, cached) |
| Audit trail | `VEYRA_DATABASE_URL` enables it | `postgres` | `AuditTrail` (`audit.rs`) | `AuditProvider` | — (URL is the switch) | `main.rs`: `Store::connect` + embedded migrations | `store.rs` (`Store`) |

Rules that hold across all of them:

- An absent provider with no related variables disables the integration; a **partially configured section fails startup**. Malformed values name the setting and never echo its raw value; secrets are redacted from `Debug`.
- The `ea` market provider refuses to build without an active broker command channel, because it reads candles through it.
- The broker contract covers reporting *and* the command channel: `BrokerLink` exposes `enqueue_order_check` / `enqueue_open_order` / `enqueue_close_order` / `enqueue_modify_order` / `enqueue_rates` / `enqueue_symbol_spec`, `command` / `await_command` / `recent_commands`, and the retained account snapshot. `control.rs`, `reconciliation.rs`, `market/`, and `trading/autopilot.rs` depend on the trait alone, so a new venue is one implementation module plus selector arms — no caller edits. A test-only second implementation in `broker/mod.rs` holds that seam in place.
- `/status` reports the active provider identifiers (including the calendar), the broker link state, both switches, the autopilot settings, the model budget, and the effective risk policy. `GET /calendar?hours=1-168` lists the upcoming events the entry path sees.

### Adding a provider

1. **Implement the trait** in a new module (for example `broker/myvenue.rs`), owning every wire type so nothing vendor-specific leaks into domain code.
2. **Add the enum variant** plus its `parse()`/`as_str()` arms to the provider enum.
3. **Add the settings variant and parse arm**, validating strictly and failing closed on partial input.
4. **Add the runtime factory arm** constructing the implementation once at startup (a broker provider may also expose its own listener, as the EA does).
5. **Reuse the contract tests.** `tests/ea_contract.rs`, `tests/risk_contract.rs`, and the module-level tests exercise the contracts in-process; a new implementation should satisfy the same tests through the trait, not just its own happy path.

## One autopilot tick

`trading/autopilot.rs::tick` runs once per `VEYRA_AUTOPILOT_INTERVAL_SECS` (default 300 s, disabled by default; the first tick lands one interval after startup). Every missing input skips the tick; provider failures are audited as `unavailable`; nothing can be queued without a gate approval.

1. **Preconditions.** Autopilot enabled, model + market + broker configured, link fresh, account facts available, and a candidate menu: the configured `VEYRA_AUTOPILOT_SYMBOLS` list (or a single `VEYRA_AUTOPILOT_SYMBOL`; setting both is a configuration error), plus the symbols of any open Veyra positions, de-duplicated and capped at eight. An empty list falls back to the terminal's chart symbol. Any failure → `Skipped`, no cost.
2. **Market candles.** For every candidate, `MarketFeed` queues a read-only `rates` command and awaits its acknowledgement; the EA returns closed bars oldest-first (forming bar excluded) from `iOpen/iHigh/iLow/iClose/iVolume`, and the service re-validates OHLC sanity, ordering, symbol, and timeframe into a `CandleSeries`. A candidate whose data is unavailable is dropped for this tick instead of failing the rest; only an empty menu aborts.
3. **Contracts and scheduled news.** Before the entry decision, `MarketFeed::symbol_spec` queues a read-only `symbol_spec` command per candidate so the model sees the venue's own contract, and a configured `EventCalendar` supplies the next 24 hours of scheduled events for the menu. A candidate whose contract is unavailable is still reviewed but its entries are rejected as unverifiable rather than queued blind; a configured calendar that cannot answer aborts the entry sweep (`unavailable`), because trading blind through a data outage is what the blackout exists to prevent.
4. **Jev judgements (when configured, `VEYRA_AUTOPILOT_JEV=auto`).** For every candidate with data, three typed questions over a compact market narrative — direction (`choice`), trending (`noul`), momentum (`score`) — are validated against the request that produced them and reduced to a per-asset JSON summary. Judgements are inputs code may consult; they grant no execution authority, and a configured judge that fails aborts the tick.
5. **Deterministic stops first.** Stop policies run across **all** managed positions and a due move ends the tick, so capital preservation never waits on a model call.
6. **Position review.** One managed position per tick is reviewed, rotating through the open book; the model answers through the decision loop (below) with `hold` or `close` for its ticket plus a short `rationale`, and a close goes through the shared staged close with a minimum-hold and age check. A `hold` falls through to the entry decision. The rationale and the reviewed asset's judgements are journaled with the decision.
7. **Entry path — the AI picks from the menu.** Through the decision loop (below), the model receives every candidate's recent candles, its judgements, its venue contract (spread, stop level, lot band and step, margin per lot, swap rates), ATR(14), the upcoming events for its currencies, any position already open on that asset, and the account facts including free margin and margin level. The prompt asks it to decide per instrument whether conditions justify a trade; it may answer `none` (skipping is normal and expected — every asset is reconsidered next tick) or open exactly one instrument from the menu as a bracketed market order. The prompt requires `stop_loss` and `take_profit`, and always asks for a short `rationale` explaining the choice (or the skip). `normalize_proposal` drops exactly three execution-neutral phrasings (an echoed `price` on a market order, an over-long `comment`, a stray `intent` beside `action: "none"`); everything else must pass strict parsing into a `TradeIntentDraft`. The rationale is sanitised (`parse_rationale`: trimmed, control characters stripped, 280-character bound) and journaled with the chosen asset's judgements — it never influences execution.
8. **Deterministic risk gate.** The draft is evaluated in fixed order (see Safety model). Only approval mints a `TradeIntent` with identity.
9. **News and venue contract checks (pre-queue).** An approved draft is first refused when a high-impact event for its currencies sits inside the configured blackout window (`news_blackout`), then measured against the instrument contract: volume lands on the venue's lot band and step, estimated margin (`marginRequired x lots`) fits the reported free margin, and the stop sits beyond the current spread, the broker's minimum stop level, and the configured ATR(14) noise floor. Violations are audited as `rejected` with stable codes and nothing is queued.
10. **Command queue.** `queue_staged_order` stamps the Veyra magic, re-checks the service switch, queues an `open_order` command, and audits `command_queued` with the intent id (the terminal applies its own arming when the command arrives). Entries without both `stop_loss` and `take_profit` are rejected before this step.
11. **EA poll → MT4.** On the next poll the EA receives `cmd`, executes (or dry-runs) in the terminal, and acknowledges by id. Delivery is at-least-once; acks are validated against the command's typed payload before being recorded.
12. **Audit and console.** The tick records `proposal_evaluated` carrying the model's `rationale` and, when a judge is configured, the chosen asset's `judgements`, plus `intent_id`/`command_id` where they exist; the command lifecycle and broker snapshots follow; the console's `/events` long-poll surfaces them within about 250 ms.

### Instrument coverage

Candidates are whatever the configured menu and the open book contain. The valuation model understands USD-quoted and USD-based currency pairs (100,000 units per lot) and the metals contracts: 100 troy ounces per lot for `XAU*`, 5,000 for `XAG*`, priced only when quoted in USD. Anything else (crosses, synthetic symbols like `XAUOIL`) stays deliberately unpriceable and the gate fails closed on it when a price is known. On a small account, metals are *available as options* but rarely tradeable: one 0.01-lot gold position is ~1 oz (~4× the notional of a 0.01-lot EURUSD), a normal gold stop exceeds the per-trade risk cap, and the venue margin per lot is large relative to this account's free margin — so the loop will usually consider gold and skip it, and the contract check would refuse it as `insufficient_margin` if it did not. That is the intended behaviour, not a failure.

### The decision loop

Entry decisions and position reviews both answer through a loop (`trading/agent.rs`), not a single model call. Each iteration is one structured answer over the `veyra_agent_step` schema: the model either asks for a read-only tool or gives its final answer (`none`/`open` for entries, `hold`/`close` for reviews).

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_judgements` | `{symbol}` | Jev's calibrated direction/trending/momentum summary (served from the tick's cache when already computed) |
| `get_market` | `{symbol, timeframe?, bars?}` | A compact candle window for any allowlisted symbol (1-240 bars) |
| `get_account` | `{}` | Gate facts: open orders/lots/symbols, trade-allowed, equity |
| `get_positions` | `{}` | Venue positions with entry/current/profit/brackets and the Veyra magic flag |
| `get_market_window` | `{}` | UTC session, rollover, and weekend state, and whether entries are open |
| `check_risk` | `{intent}` | A dry run of a draft through the deterministic gate; never records an approval |

Every tool result — or its error — is appended to an *Agent transcript* that is fed back as input on the next call, so the model can iterate: ask Jev for a second opinion, pull another timeframe, dry-run a draft, then decide. Tools are strictly read-only; none can queue, modify, or close anything. The loop only produces a decision, which the existing staged execution path then gates and executes. Bounds: at most 8 model calls per decision (tool calls included), and every call counts against the model budget; exhausting the bound fails closed as `unavailable`. Each execution is journaled as an `agent_tool_called` event and the final decision carries `agent_tool_calls`/`agent_tools` alongside the rationale.

### Stops and the `StopBasis` memory

While a managed position is open, deterministic policies feed one planned stop move:

- **Break-even** (`VEYRA_AUTOPILOT_BREAKEVEN_R`): once the trade has travelled that many multiples of its entry risk in favour, the stop moves to the entry price.
- **Trailing** (`VEYRA_AUTOPILOT_TRAIL_R`): once that many risk units in favour, the stop stays that far behind the best favourable price.

The most protective candidate wins, stops only ever move in the favourable direction, an improvement must beat the current stop by at least a tenth of the entry risk, and both policies are opt-in (0 = off). Moves go through the same staged `modify_order` path as the control surface and are audited with the policy name (`break_even` / `trailing_stop`).

The terminal reports only a position's **current** stop, so the service cannot derive the original risk from any one payload. `StopBasis` (process lifetime, in `AppState`) remembers each ticket's first observed entry-to-stop distance while the stop still sits behind the entry; a position first seen after a stop move has no basis and is left alone until it closes. Tickets no longer open are dropped.

## Audit trail as source of truth

One append-only PostgreSQL table (`audit_events`: id, timestamp, kind, JSONB payload) with embedded migrations. A configured but unreachable database fails startup; individual writes are best-effort so storage can never block or fail a command. Retention defaults to `VEYRA_AUDIT_RETENTION_DAYS=30`, pruned hourly; launchd backs the database up daily (local generations plus an off-machine R2 upload).

| Event kind | Written when |
| --- | --- |
| `service_started` | Process start; carries the effective risk policy so every later decision can be read against the rules in force |
| `command_queued` | A command enters the queue (`kind`, `command_id`, plus `intent_id` for orders) |
| `command_completed` | A validated ack completes (`kind`, bounded result summary) |
| `command_failed` | A failed acknowledgement is processed, or an acknowledgement arrives for a command the queue already marked failed (for example a timeout) |
| `broker_snapshot` | A validated `account_snapshot` ack is retained |
| `agent_tool_called` | The decision loop executed one read-only tool (tool, bounded arguments and result, step, rationale) |
| `risk_policy_updated` | The live risk policy was replaced from the control surface (full resulting policy attached) |
| `proposal_evaluated` | Every autopilot decision attempt (`outcome`: `no_trade`, `rejected`, `approved_dry_run`, `queued`, `unavailable`, `held`, `close_queued`, `close_rejected`, `stop_rejected`, `break_even`, `trailing_stop`, …). Entry and review decisions carry the model's `rationale` and, when a judge is configured, the chosen asset's `judgements`, so the why is queryable next to the what |
| `reconciliation_drift` | A snapshot shows orders Veyra does not own, or a truncated position list |
| `position_closed` | A managed ticket disappears from the book (last observed values, including P/L) |

Read routes on the loopback surface:

- **`GET /events`** — the live feed: an in-memory ring of the 512 most recent events with a monotonic sequence cursor. No cursor returns the buffered tail; a cursor long-polls up to `wait_ms` (max 25 s), checking every 250 ms. The durable trail remains the source of truth; the ring is just a fast reader.
- **`GET /metrics`** — process-lifetime counters derived from the same stream: `event.<kind>`, `proposal.<outcome>`, and `command.<event>.<kind>`, plus `feedLatest`. Cheap for dashboards and the alert probe; resets with the process.
- **`GET /audit?limit=`** — newest rows straight from PostgreSQL, newest first.
- **`GET /reconciliation`** — every order in the retained snapshot classified as Veyra-managed or unknown, with the snapshot age and a `reconciled`/`drift` verdict (or `unavailable`, `stale`, or `no_snapshot` when the channel or a snapshot is missing).

Traceability: a `proposal_evaluated` event carries the `intent_id` and `command_id` it produced, command events carry the `command_id` and kind, and review events carry the ticket — so a venue ticket can be traced back through its command and ack to the decision that opened it.

## Safety model

Four independent controls, each of which can only reduce activity:

| Control | Can do | Cannot do |
| --- | --- | --- |
| Model proposal (`DecisionEngine`) | Propose one schema-constrained bracketed trade, hold, or close | Approve, queue, or execute anything; rejections are normal outcomes |
| Jev judgement (`SemanticJudge`) | Supply calibrated, validated inputs to the proposal | Grant execution authority; contradictory answers fail closed |
| Risk gate (deterministic code) | Mint the only executable `TradeIntent` | Be influenced by model confidence; it never fetches state itself |
| Arming switches (`VEYRA_TRADING_ENABLED`, EA `InAllowLiveOrders`) | Authorise real money | Trade alone: with either off, the command is refused or dry-runs |

The gate evaluates one draft in a fixed order: **kill switch → instrument allowlist → built-in entry window (rollover/weekend) → configured session window → per-order volume cap → account facts available and connected → trading permission → daily-loss breaker → peak-drawdown breaker → one position per asset → open-order cap → total-exposure cap → per-trade risk cap → net directional exposure cap → duplicate suppression**. Rejections carry stable codes (`kill_switch`, `symbol_not_allowed`, `market_window_closed`, `session_closed`, `volume_above_limit`, `account_state_unavailable`, `trading_not_allowed`, `daily_loss_limit`, `peak_drawdown_limit`, `symbol_already_open`, `order_limit_reached`, `exposure_above_limit`, `risk_above_limit`, `risk_unverifiable`, `factor_exposure_above_limit`, `duplicate_intent`).

The surrounding guards:

- **Kill switch** — `VEYRA_RISK_KILL_SWITCH=true` rejects every intent.
- **Symbol allowlist** — `VEYRA_RISK_SYMBOLS`; the default is empty, which approves nothing, so a missing setting cannot widen behaviour.
- **One position per asset.** The gate rejects an open intent for any symbol that already appears in the latest validated snapshot's position list (manual or Veyra-owned), so the book cannot stack two positions on one instrument.
- **Per-trade risk cap** — `VEYRA_RISK_MAX_RISK_PERCENT` (default 12): a draft's stop distance is converted to money through pip value (currency pairs and USD-quoted metals) and the caller's reference prices, and anything risking more than that share of equity is rejected (`risk_above_limit`). When a price is known but the instrument cannot be valued (a cross, a non-USD metal, a synthetic), the gate fails closed (`risk_unverifiable`); without any price the check cannot run and the exposure caps stay binding.
- **Drawdown breakers** — `VEYRA_RISK_MAX_DAILY_LOSS_PERCENT` (default 10, below the UTC day's opening equity) and `VEYRA_RISK_MAX_PEAK_DRAWDOWN_PERCENT` (default 25, below the highest equity since startup). Both refuse *new* risk (`daily_loss_limit`, `peak_drawdown_limit`) while stops and reviews keep running. Baselines live in memory, so a restart re-baselines.
- **Net directional cap** — `VEYRA_RISK_MAX_NET_FACTOR_LOTS` (default 0.01): the gate sums signed USD exposure across open positions and the draft, so long EURUSD plus long USDJPY is one bet, not two (`factor_exposure_above_limit`). Opposing directions offset.
- **Venue contract check (pre-queue)** — after gate approval, the draft is measured against the instrument report from the terminal: volume below/above the lot band (`volume_below_min`, `volume_above_max`) or off the lot step (`volume_not_on_step`), estimated margin above the snapshot's free margin (`insufficient_margin`), a stop inside the spread (`stop_inside_spread`) or inside the broker's minimum distance (`stop_below_level`), and a missing contract (`spec_unavailable`). The margin estimate is skipped only until the first snapshot payload reports free margin; the terminal still re-validates margin when the order is sent.
- **News blackout** — when a calendar provider is configured, an approved entry is refused with `news_blackout` while a high-impact event for the instrument's currencies sits inside `VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES` (default 30, console-editable, 0 disables). The model sees the same events in `upcoming_events`, so it can plan around a print instead of fighting it.
- **ATR noise floor** — `VEYRA_RISK_MIN_STOP_ATR_FRACTION` (default 0.25, console-editable, 0 disables) refuses a stop closer to the entry than that fraction of ATR(14) (`stop_inside_noise`). ATR is measured from the same closed candles the tick already fetched; when the window is too short to measure, the floor is skipped rather than failing entries the tick cannot assess.
- **Bounded execution deviation** — the EA caps `OrderSend` slippage at twice the live spread, floored at 10 points and capped at 30, so a spread blowout cannot become a blank cheque.
- **Bounded model budget** — `BudgetedEngine` wraps whatever engine a provider builds; `VEYRA_MODEL_MAX_CALLS_PER_HOUR` / `_PER_DAY` (0 = unlimited, the default) refuse calls past a fixed window, and `/status` reports usage against the caps.
- **Judge usage counters** — the Jev runtime counts calls, failures, and the provider-reported input/output tokens; `/status` carries the process-lifetime totals and the console shows them in the Autopilot panel, so the service's actual usage can always be checked against the provider's dashboard (which can lag, aggregate differently, or belong to another project).
- **Duplicate window** — `VEYRA_RISK_DUPLICATE_WINDOW_SECS` (default 60) suppresses an identical approved draft.
- **Missing or stale state rejects.** No fresh link report or no connected terminal means `account_state_unavailable`, not an assumption.
- `POST /intents/check` performs a broker-side `order_check` without the trading switch because it never sends an order; `POST /intents/execute`, `/intents/close`, and `/intents/modify` all refuse with `403 trading_disabled` unless the service switch is on.

## Live policy control

Environment variables are **startup defaults and secrets** (bind addresses, tokens, provider selection, database URL) — nothing a bad edit over a UI should be able to break. Everything an operator tunes day to day lives in the **risk policy** and can be changed from the console while the service runs:

- `GET /risk/policy` returns the effective policy; `POST /risk/policy` applies a partial patch (omitted fields keep their value).
- The patch is validated by exactly the same rules as the environment parser — caps, booleans, symbol list, session window, breaks — so a console edit can never widen behaviour beyond what a restart would accept. Unknown fields are rejected, and failures name the field and reason.
- Every accepted change is journaled as `risk_policy_updated` (with the resulting policy) and takes effect atomically for all decisions; the kill switch is just one field of the patch.
- Changes are runtime-only: a restart restores the `.env` values. The console's editor panel exposes the fields directly, and the control surface is loopback-only like everything else.

## Console

`console/` is a TanStack Start application served by a supervised Vite preview on `http://127.0.0.1:3000`. It reads only the loopback control surface, proxying `/api` so the browser never needs cross-origin access. There is no authentication: keep it on loopback.

| Panel | Shows |
| --- | --- |
| Status pills | Terminal live/stale, EA armed/disarmed, trading enabled/disabled, autopilot cadence, audit provider, environment |
| Account | Balance, equity, free margin, open orders, open lots, open P/L, server/login/symbol, freshness |
| Market | 48 closed H4 candles via `/market/candles`: sparkline, last close, window change, last high/low |
| Autopilot | Enabled, cadence, timeframe, window, model tier, Jev mode, symbol menu, stop policies, model-budget usage, and the service's own Jev call/token counters |
| Activity | `/events` cursor feed (streaming indicator); "focus" mode hides routine snapshots and read-only commands |
| Positions | Ticket, side, lots, entry, SL, TP, P/L, and owner (Veyra by magic 77041 vs manual); truncation flag |
| Commands | Recent command lifecycle (`pending`/`completed`/`failed`) with bounded summaries |
| Risk | The effective gate policy plus both switch states |
| Risk | Effective gate policy: symbols, caps, risk/drawdown brakes, net-exposure cap, execution/terminal state — with an inline editor for live changes |
| Metrics | Top counters from `/metrics` and the feed sequence |
| Agent log | `/logs` tail with a level filter (`error`…`trace`), polled every 2 s; shows the tracing target, message, and structured fields |

**Decision/command drill-down.** Clicking an activity row expands priority-ordered detail rows starting with the model's `rationale` and `judgements`, then `outcome`, `reason`, `origin`, `symbol`, `side`, `volume`, `ticket`, `intent_id`, `command_id`, stops, … plus the raw JSON payload, so a decision can be followed into the command and on to its ack (`GET /commands/{id}`). Agent tool calls appear as their own `agent_tool_called` rows carrying the arguments and result. Clicking a command row expands its result summary or failure reason. Expanding a position's story therefore runs: proposal outcome → queued command → terminal ack/result → later stop, close, or `position_closed` events.

## Known limits

- **One position, one action at a time.** The risk cap defaults to one open order (`VEYRA_RISK_MAX_OPEN_ORDERS=1`) and the autopilot takes at most one action (stop move, review, or entry) per tick; it rotates across up to eight configured instruments (`VEYRA_AUTOPILOT_SYMBOLS`), and what may actually trade is still bounded by `VEYRA_RISK_SYMBOLS`.
- **H4 by default.** The supervised configuration runs H4 (`VEYRA_AUTOPILOT_TIMEFRAME`, console candles fixed at H4 in `console/src/lib/api.ts`). Other timeframes exist in the contract (`M1`…`MN1`) but are not what is exercised today.
- **EA-specific wire transport.** Command channels are provider-neutral (`BrokerLink` + `broker/command.rs`), but the only implemented transport today is the EA poll loop in `broker/ea.rs`; the `ea_link()` accessor remains for its transport and tests, and no other venue implementation exists yet.
- **No console authentication.** The console and `/account` expose owner-facing money state on loopback only; exposing either beyond loopback requires authentication first.
- **Always-on deployment pending.** Supervision runs on one local Mac; a durable 24/7 host/VPS, managed secrets, remote monitoring, and a versioned deployment pipeline are open roadmap items.
- **One implementation per provider today** (`ea`, `openrouter`, `typesafe`, `postgres`); the abstraction is the extension point, not a menu of built-ins.

## Where to change things

| Change | Files / settings |
| --- | --- |
| New broker/venue | Implement `BrokerLink` (report + enqueue/await command surface) in a new `broker/<provider>.rs`, add the variant to `BrokerProvider` + `broker/settings.rs` + `BrokerRuntime::from_settings`, and mirror `tests/ea_contract.rs`; `control.rs`, `reconciliation.rs`, `market/`, and `autopilot.rs` need no changes |
| New market-data provider | `market/mod.rs` (trait + enum + factory arm), `market/settings.rs`, new `market/<provider>.rs` (use `market/ea.rs` as the template) |
| New model provider | `model/mod.rs` (enum + parse + factory arm), `model/settings.rs` arms, new engine module (or extend `agent_runtime_engine.rs`); `BudgetedEngine` wraps it automatically |
| New judgement provider | `jev/mod.rs` (enum + factory arm), `jev/settings.rs` arms, new transport module alongside `jev/http.rs`; contract types live in `jev/contract.rs` |
| Different audit storage | Implement `AuditTrail` (see `audit.rs` and `store.rs`) and swap the construction in `main.rs` |
| New symbols | `VEYRA_RISK_SYMBOLS` + `VEYRA_AUTOPILOT_SYMBOLS` (up to 8, comma-separated; `VEYRA_AUTOPILOT_SYMBOL` remains the single-symbol form); no code change |
| New timeframe | `VEYRA_AUTOPILOT_TIMEFRAME`; update the console's fixed H4 call in `console/src/lib/api.ts` if the UI should follow |
| More positions | `VEYRA_RISK_MAX_OPEN_ORDERS` and `VEYRA_RISK_MAX_TOTAL_LOTS`, plus the candidate menu in `VEYRA_AUTOPILOT_SYMBOLS`/`VEYRA_RISK_SYMBOLS`; the AI chooses at most one instrument per tick and skips unsuitable ones, one position per asset is enforced by the gate |
| New risk limit | `risk/mod.rs` (policy parse + `summary`) and `risk/gate.rs` (fixed check order), plus gate tests; valuation maths lives in `risk/valuation.rs` and the equity baselines in `risk/guard.rs` |
| New terminal command | `broker/command.rs` (`CommandKind`, request/payload types, validation), `broker/ea.rs` (wire mapping + ack handling), `ea/VeyraProbe.mq4`, and the caller in `control.rs`/`autopilot.rs` |
| Console behaviour | `console/src/components/veyra.tsx`, typed client `console/src/lib/api.ts`, feed hooks `console/src/lib/hooks.ts`, formatting rules `console/src/lib/format.ts` |
| Log capture and tail | Buffer and level parsing in `logs.rs`, tracing tee in `observability.rs`, route contract in `routes.rs` (`GET /logs`), console panel in `console/src/components/veyra.tsx` |
| Deployment / supervision | `docs/deployment.md`, `scripts/launchd/*`, `scripts/install-launchd.sh` |

Related documents: [roadmap](roadmap.md), [deployment](deployment.md), and the decision records in [`docs/decisions/`](decisions/).
