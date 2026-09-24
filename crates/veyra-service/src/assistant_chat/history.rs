//! Realized-trade observations for the assistant: closed trades and
//! performance, both from the terminal's account history.
//!
//! Read-only boundary: the only broker interaction is the existing
//! `order_history` command for the Veyra magic number — the same path the
//! `/performance` route uses, with its 1–365 day window and 256-order
//! terminal cap. It never reaches an order path. Broker-clock times are
//! converted to UTC with [`BrokerClock`] before any time filter applies.

use serde_json::{Map, Value, json};

use super::args::Args;
use super::bounded::{cents, fit_list, put};
use super::clock::{
    BrokerClock, DAY_MS, Edge, OperatorOffset, broker_text, duration_text, unix_ms, unknown_clock,
    utc_text,
};
use crate::AppState;
use crate::broker::{
    ClosedTradePayload, CommandPayload, CommandState, ORDER_MAGIC, OrderHistoryPayload,
    OrderHistoryRequest, PositionKind,
};

/// How long one history request may wait for the terminal.
const HISTORY_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Scope statement attached to every history result.
const HISTORY_SCOPE: &str =
    "Veyra-owned closed orders (magic 77041) from the terminal account history";

/// Queues the read-only `order_history` command and awaits its result.
///
/// # Errors
/// Returns a bounded reason when no broker is configured, the window is
/// invalid, or the terminal fails or does not answer in time.
pub(super) async fn fetch_history(
    state: &AppState,
    days: u32,
) -> Result<OrderHistoryPayload, String> {
    let broker = state
        .broker()
        .ok_or_else(|| "broker_unavailable".to_owned())?;
    let request = OrderHistoryRequest::new(days, ORDER_MAGIC)
        .map_err(|_| "invalid_history_window: days must be from 1 through 365".to_owned())?;
    let link = broker.link();
    let id = link.enqueue_order_history(request);
    match link.await_command(id, HISTORY_WAIT).await {
        CommandState::Completed {
            payload: CommandPayload::OrderHistory(history),
        } => Ok(history),
        CommandState::Completed { .. } => {
            Err("order_history_failed: the terminal answered with a different payload".to_owned())
        }
        CommandState::Failed { reason } => Err(format!(
            "order_history_failed: {}",
            super::bounded::clip(&reason, 200)
        )),
        CommandState::Pending => {
            Err("order_history_timeout: the terminal did not answer within 20 s".to_owned())
        }
    }
}

/// Human side label for a venue order kind.
pub(super) fn side(kind: PositionKind) -> &'static str {
    match kind {
        PositionKind::Buy
        | PositionKind::BuyLimit
        | PositionKind::BuyStop
        | PositionKind::BuyStopLimit => "Long",
        PositionKind::Sell
        | PositionKind::SellLimit
        | PositionKind::SellStop
        | PositionKind::SellStopLimit => "Short",
    }
}

/// One closed trade as the assistant sees it. With a known broker clock the
/// times are UTC (and local when the operator stated an offset); without one
/// they are labelled as broker-clock readings.
pub(super) fn trade_row(
    trade: &ClosedTradePayload,
    clock: Option<BrokerClock>,
    operator: OperatorOffset,
) -> Value {
    let mut row = Map::new();
    row.insert("ticket".to_owned(), json!(trade.ticket));
    row.insert("symbol".to_owned(), json!(trade.symbol));
    row.insert("side".to_owned(), json!(side(trade.kind)));
    row.insert("lots".to_owned(), json!(trade.lots));
    row.insert("open_price".to_owned(), json!(trade.open_price));
    row.insert("close_price".to_owned(), json!(trade.close_price));
    match clock {
        Some(clock) => {
            let opened = clock.to_utc_secs(trade.open_time).saturating_mul(1_000);
            let closed = clock.to_utc_secs(trade.close_time).saturating_mul(1_000);
            put(&mut row, "opened_utc", json!(utc_text(opened)));
            put(&mut row, "closed_utc", json!(utc_text(closed)));
            put(&mut row, "opened_local", json!(operator.local_text(opened)));
            put(&mut row, "closed_local", json!(operator.local_text(closed)));
        }
        None => {
            put(
                &mut row,
                "opened_broker",
                json!(broker_text(trade.open_time)),
            );
            put(
                &mut row,
                "closed_broker",
                json!(broker_text(trade.close_time)),
            );
        }
    }
    let held = trade.close_time.saturating_sub(trade.open_time);
    row.insert("held".to_owned(), json!(duration_text(held)));
    row.insert("held_secs".to_owned(), json!(held));
    row.insert("net".to_owned(), json!(cents(trade.net_profit())));
    row.insert("profit".to_owned(), json!(cents(trade.profit)));
    row.insert("swap".to_owned(), json!(cents(trade.swap)));
    row.insert("commission".to_owned(), json!(cents(trade.commission)));
    Value::Object(row)
}

/// Count, wins, losses, flat trades, and net across `trades`.
fn totals(trades: &[ClosedTradePayload]) -> Value {
    let report = crate::performance::summarize(trades);
    json!({
        "count": report.trades,
        "wins": report.wins,
        "losses": report.losses,
        "breakeven": report.breakeven,
        "net": report.net_profit
    })
}

/// `closed_trades`: realized trades whose close falls inside a UTC window.
///
/// # Errors
/// Returns a bounded reason for invalid arguments, an unknown broker clock,
/// or an unavailable history.
pub(super) async fn closed_trades(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let args = Args::new(
        "closed_trades",
        arguments,
        &["since", "until", "symbol", "days", "utc_offset_minutes"],
    )?;
    let operator = args.operator_offset()?;
    let symbol = args.symbol()?;
    let days = args.days()?;
    let now_ms = unix_ms(state.now())?;
    let since_arg = args.instant("since", Edge::Start, now_ms, operator)?;
    let until_ms = args
        .instant("until", Edge::End, now_ms, operator)?
        .unwrap_or(now_ms);
    let since_ms = since_arg.unwrap_or_else(|| {
        now_ms.saturating_sub(i64::from(days.unwrap_or(OrderHistoryRequest::DEFAULT_DAYS)) * DAY_MS)
    });
    if since_ms >= until_ms {
        return Err("since must be before until".to_owned());
    }
    // One spare day covers the terminal's own day arithmetic on its clock.
    let needed_days = now_ms
        .saturating_sub(since_ms)
        .max(0)
        .div_euclid(DAY_MS)
        .saturating_add(2);
    if since_arg.is_some() && needed_days > i64::from(OrderHistoryRequest::MAX_DAYS) + 1 {
        return Err("since must fall within the last 365 days (terminal history limit)".to_owned());
    }
    let history_days = u32::try_from(needed_days)
        .unwrap_or(OrderHistoryRequest::MAX_DAYS)
        .max(days.unwrap_or(1))
        .clamp(1, OrderHistoryRequest::MAX_DAYS);
    let clock = BrokerClock::from_state(state)?;
    let history = fetch_history(state, history_days).await?;

    let mut trades: Vec<ClosedTradePayload> = history
        .orders
        .iter()
        .filter(|trade| {
            let closed_ms = clock.to_utc_secs(trade.close_time).saturating_mul(1_000);
            closed_ms >= since_ms && closed_ms < until_ms
        })
        .filter(|trade| {
            symbol
                .as_ref()
                .is_none_or(|symbol| trade.symbol.eq_ignore_ascii_case(symbol.as_str()))
        })
        .cloned()
        .collect();
    trades.sort_by(|left, right| {
        right
            .close_time
            .cmp(&left.close_time)
            .then(right.ticket.cmp(&left.ticket))
    });

    let mut window = Map::new();
    put(&mut window, "since_utc", json!(utc_text(since_ms)));
    put(&mut window, "until_utc", json!(utc_text(until_ms)));
    put(
        &mut window,
        "since_local",
        json!(operator.local_text(since_ms)),
    );
    put(
        &mut window,
        "until_local",
        json!(operator.local_text(until_ms)),
    );
    window.insert(
        "operator_utc_offset_minutes".to_owned(),
        json!(operator.minutes()),
    );
    window.insert("history_days".to_owned(), json!(history_days));

    let mut envelope = Map::new();
    envelope.insert("scope".to_owned(), json!(HISTORY_SCOPE));
    envelope.insert("window".to_owned(), Value::Object(window));
    envelope.insert(
        "symbol".to_owned(),
        json!(symbol.as_ref().map(|symbol| symbol.as_str())),
    );
    envelope.insert("broker_clock".to_owned(), clock.describe());
    envelope.insert("totals".to_owned(), totals(&trades));
    envelope.insert("history_orders".to_owned(), json!(history.orders.len()));
    envelope.insert("history_total".to_owned(), json!(history.total));
    envelope.insert("history_truncated".to_owned(), json!(history.truncated));
    if history.truncated {
        envelope.insert(
            "note".to_owned(),
            json!("The terminal capped its history; older trades in the window may be missing. Narrow the window."),
        );
    }
    let rows = trades
        .iter()
        .map(|trade| trade_row(trade, Some(clock), operator))
        .collect();
    Ok(fit_list(envelope, "trades", rows))
}

/// `performance`: the realized report plus bounded per-trade detail.
///
/// # Errors
/// Returns a bounded reason for invalid arguments or an unavailable history.
pub(super) async fn performance(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let args = Args::new("performance", arguments, &["days", "utc_offset_minutes"])?;
    let days = args.days()?.unwrap_or(OrderHistoryRequest::DEFAULT_DAYS);
    let operator = args.operator_offset()?;
    let history = fetch_history(state, days).await?;
    let clock = BrokerClock::from_state(state);
    let mut envelope = Map::new();
    envelope.insert("scope".to_owned(), json!(HISTORY_SCOPE));
    envelope.insert("days".to_owned(), json!(days));
    envelope.insert(
        "report".to_owned(),
        json!(crate::performance::summarize(&history.orders)),
    );
    envelope.insert(
        "broker_clock".to_owned(),
        match &clock {
            Ok(clock) => clock.describe(),
            Err(reason) => unknown_clock(reason),
        },
    );
    envelope.insert("total".to_owned(), json!(history.total));
    envelope.insert("truncated".to_owned(), json!(history.truncated));
    let clock = clock.ok();
    let mut trades = history.orders.clone();
    trades.sort_by(|left, right| {
        right
            .close_time
            .cmp(&left.close_time)
            .then(right.ticket.cmp(&left.ticket))
    });
    let rows = trades
        .iter()
        .map(|trade| trade_row(trade, clock, operator))
        .collect();
    Ok(fit_list(envelope, "trades", rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(
        ticket: i64,
        kind: PositionKind,
        open_time: i64,
        close_time: i64,
    ) -> ClosedTradePayload {
        ClosedTradePayload {
            ticket,
            symbol: "USDJPY".to_owned(),
            kind,
            lots: 0.01,
            open_price: 156.198,
            close_price: 156.41,
            open_time,
            close_time,
            profit: 1.364,
            swap: -0.1,
            commission: -0.05,
            magic: ORDER_MAGIC,
        }
    }

    #[test]
    fn trade_rows_convert_broker_times_and_net_the_costs() {
        let clock = BrokerClock::estimate(1_790_239_566, 1_790_232_369).expect("offset");
        let row = trade_row(
            &trade(10_654_130, PositionKind::Buy, 1_790_231_406, 1_790_239_566),
            Some(clock),
            OperatorOffset::new(120).expect("offset"),
        );
        assert_eq!(row["side"], "Long");
        assert_eq!(row["closed_utc"], "2026-09-24T06:46:06Z");
        assert_eq!(row["closed_local"], "2026-09-24T08:46:06+02:00");
        assert_eq!(row["opened_utc"], "2026-09-24T04:30:06Z");
        assert_eq!(row["held"], "2h 16m");
        assert_eq!(row["held_secs"], 8_160);
        assert_eq!(row["net"], 1.21);
        assert!(row.get("closed_broker").is_none());

        let unaligned = trade_row(
            &trade(7, PositionKind::SellStop, 1_790_231_406, 1_790_239_566),
            None,
            OperatorOffset::default(),
        );
        assert_eq!(unaligned["side"], "Short");
        assert_eq!(unaligned["closed_broker"], "2026-09-24T08:46:06");
        assert!(unaligned.get("closed_utc").is_none());
        assert!(unaligned.get("closed_local").is_none());
    }

    #[test]
    fn totals_count_wins_losses_and_net() {
        let mut losing = trade(2, PositionKind::Sell, 10, 20);
        losing.profit = -3.0;
        let summary = totals(&[trade(1, PositionKind::Buy, 10, 20), losing]);
        assert_eq!(summary["count"], 2);
        assert_eq!(summary["wins"], 1);
        assert_eq!(summary["losses"], 1);
        assert_eq!(summary["net"], -1.94);
        assert_eq!(totals(&[])["count"], 0);
    }
}
