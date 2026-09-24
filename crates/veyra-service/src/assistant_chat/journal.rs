//! Decision and command observations for the assistant, from the durable
//! audit trail joined with retained broker state.
//!
//! Read-only boundary: these tools read the audit trail through
//! [`AuditTrail::query`](crate::audit::AuditTrail::query), the retained
//! account snapshot, and the in-memory command history. `position_story`
//! may additionally queue the same read-only `order_history` command as
//! `closed_trades` to find a closed ticket's fill. Nothing here enqueues an
//! order, close, or modification. Recorded text (reasons, rationales,
//! errors) is clipped and every list is fitted to the output ceiling.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::args::Args;
use super::bounded::{MAX_TEXT_CHARS, clip, fit_list, fit_text_rows, put};
use super::clock::{
    BrokerClock, DAY_MS, Edge, OperatorOffset, broker_text, duration_text, unix_ms, unknown_clock,
    utc_text,
};
use super::history::{fetch_history, side, trade_row};
use crate::AppState;
use crate::audit::{AuditKind, AuditQuery, AuditRow, parse_trail_time};
use crate::broker::{
    CommandId, CommandKind, CommandPayload, CommandState, ORDER_MAGIC, OrderHistoryRequest,
    PositionPayload,
};

/// Kinds `decision_history` searches when the model names none.
const DECISION_KINDS: [AuditKind; 3] = [
    AuditKind::ProposalEvaluated,
    AuditKind::PositionClosed,
    AuditKind::CommandFailed,
];

/// Kinds that can mention one position or one of its commands.
const STORY_KINDS: [AuditKind; 5] = [
    AuditKind::ProposalEvaluated,
    AuditKind::PositionClosed,
    AuditKind::CommandQueued,
    AuditKind::CommandCompleted,
    AuditKind::CommandFailed,
];

/// Default and largest `decision_history` page.
const DEFAULT_DECISION_ROWS: i64 = 20;
const MAX_DECISION_ROWS: i64 = 50;

/// Rows each `position_story` query may read.
const STORY_ROWS: u32 = 120;

/// Latest hold reviews kept verbatim in a story; older ones are counted.
const STORY_HELD_REVIEWS: usize = 3;

/// Largest in-memory command window and default page.
const COMMAND_WINDOW: usize = 64;
const DEFAULT_COMMAND_ROWS: i64 = 15;
const MAX_COMMAND_ROWS: i64 = 25;

/// Decision rows `activity` reads.
const ACTIVITY_ROWS: u32 = 35;

/// Every command kind, for validating the `kind` filter.
const COMMAND_KINDS: [CommandKind; 9] = [
    CommandKind::Ping,
    CommandKind::AccountSnapshot,
    CommandKind::OrderCheck,
    CommandKind::OpenOrder,
    CommandKind::CloseOrder,
    CommandKind::ModifyOrder,
    CommandKind::Rates,
    CommandKind::SymbolSpec,
    CommandKind::OrderHistory,
];

/// Refreshes and market reads that would drown the decisions out.
const ROUTINE_COMMANDS: [CommandKind; 4] = [
    CommandKind::Ping,
    CommandKind::AccountSnapshot,
    CommandKind::Rates,
    CommandKind::SymbolSpec,
];

fn audit_error(_: crate::audit::AuditError) -> String {
    "audit_unavailable: the durable audit trail could not be read".to_owned()
}

fn query_error(error: crate::audit::AuditQueryError) -> String {
    error.to_string()
}

fn text_field(payload: &Value, key: &str, bound: usize) -> Value {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map_or(Value::Null, |text| json!(clip(text, bound)))
}

fn scalar_field(payload: &Value, key: &str) -> Value {
    match payload.get(key) {
        Some(value @ (Value::Number(_) | Value::Bool(_))) => value.clone(),
        Some(Value::String(text)) => json!(clip(text, 64)),
        _ => Value::Null,
    }
}

/// One audit row as bounded evidence: its time, kind, and the decision
/// fields that answer "what and why". Text fields are clipped to `bound`.
pub(super) fn event_row(row: &AuditRow, bound: usize, operator: OperatorOffset) -> Value {
    let payload = &row.payload;
    let at_ms = parse_trail_time(&row.at);
    let mut object = Map::new();
    object.insert(
        "at".to_owned(),
        json!(
            at_ms
                .and_then(utc_text)
                .unwrap_or_else(|| clip(&row.at, 40))
        ),
    );
    put(
        &mut object,
        "at_local",
        json!(at_ms.and_then(|ms| operator.local_text(ms))),
    );
    object.insert("kind".to_owned(), json!(row.kind));
    for key in ["outcome", "origin", "symbol", "ticket"] {
        put(&mut object, key, scalar_field(payload, key));
    }
    match row.kind.as_str() {
        "command_queued" | "command_completed" | "command_failed" => {
            put(&mut object, "command_kind", scalar_field(payload, "kind"));
        }
        "position_closed" => {
            if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
                object.insert(
                    "side".to_owned(),
                    json!(if kind.starts_with("buy") {
                        "Long"
                    } else {
                        "Short"
                    }),
                );
            }
            for key in ["lots", "price", "profit"] {
                put(&mut object, key, scalar_field(payload, key));
            }
        }
        _ => {
            put(&mut object, "side", scalar_field(payload, "side"));
        }
    }
    for key in [
        "volume",
        "order_type",
        "stop_loss",
        "take_profit",
        "high_net_profit",
        "net_profit",
        "command_id",
        "intent_id",
    ] {
        put(&mut object, key, scalar_field(payload, key));
    }
    for key in ["reason", "rationale", "error"] {
        put(&mut object, key, text_field(payload, key, bound));
    }
    if let Some(result) = payload.get("result").filter(|result| result.is_object()) {
        let text = result.to_string();
        put(
            &mut object,
            "result",
            if text.chars().count() <= 300 {
                result.clone()
            } else {
                json!(clip(&text, 300))
            },
        );
    }
    if let Some(tools) = payload.get("agent_tools").and_then(Value::as_array) {
        let names: Vec<Value> = tools
            .iter()
            .filter_map(Value::as_str)
            .take(12)
            .map(|name| json!(clip(name, 40)))
            .collect();
        object.insert("agent_tools".to_owned(), Value::Array(names));
    }
    Value::Object(object)
}

/// `activity`: the newest decision-related rows, bounded.
///
/// # Errors
/// Returns a bounded reason when the trail is missing or unreadable.
pub(super) async fn activity(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let args = Args::new("activity", arguments, &["utc_offset_minutes"])?;
    let operator = args.operator_offset()?;
    let audit = state
        .audit()
        .ok_or_else(|| "audit_unavailable".to_owned())?;
    let rows = audit
        .trail()
        .recent_decisions(ACTIVITY_ROWS)
        .await
        .map_err(audit_error)?;
    let mut envelope = Map::new();
    envelope.insert("limited_to".to_owned(), json!(ACTIVITY_ROWS));
    Ok(fit_text_rows(envelope, "events", |bound| {
        rows.iter()
            .map(|row| event_row(row, bound, operator))
            .collect()
    }))
}

/// `decision_history`: a filtered, newest-first search of the audit trail.
///
/// # Errors
/// Returns a bounded reason for invalid filters or an unreadable trail.
pub(super) async fn decision_history(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let args = Args::new(
        "decision_history",
        arguments,
        &[
            "symbol",
            "ticket",
            "kinds",
            "outcome",
            "since",
            "until",
            "limit",
            "utc_offset_minutes",
        ],
    )?;
    let operator = args.operator_offset()?;
    let now_ms = unix_ms(state.now())?;
    let limit = args
        .integer("limit", 1, MAX_DECISION_ROWS)?
        .unwrap_or(DEFAULT_DECISION_ROWS);
    let kinds = match args.text_list("kinds", AuditKind::ALL.len())? {
        None => DECISION_KINDS.to_vec(),
        Some(names) => names
            .iter()
            .map(|name| {
                AuditKind::parse(name).ok_or_else(|| {
                    format!(
                        "unknown kind `{}`; accepted: {}",
                        clip(name, 40),
                        AuditKind::ALL.map(AuditKind::as_str).join(", ")
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let since_ms = args.instant("since", Edge::Start, now_ms, operator)?;
    let until_ms = args.instant("until", Edge::End, now_ms, operator)?;
    let mut query = AuditQuery::new(&kinds, u32::try_from(limit).unwrap_or(20))
        .map_err(query_error)?
        .with_window(since_ms, until_ms)
        .map_err(query_error)?;
    let symbol = args.symbol()?;
    if let Some(symbol) = &symbol {
        query = query.with_symbol(symbol.as_str()).map_err(query_error)?;
    }
    let ticket = args.ticket()?;
    if let Some(ticket) = ticket {
        query = query.with_ticket(ticket).map_err(query_error)?;
    }
    let outcome = args.text("outcome")?;
    if let Some(outcome) = outcome {
        query = query.with_outcome(outcome).map_err(query_error)?;
    }
    let audit = state
        .audit()
        .ok_or_else(|| "audit_unavailable: the durable audit trail is not configured".to_owned())?;
    let rows = audit.trail().query(&query).await.map_err(audit_error)?;

    let mut filters = Map::new();
    filters.insert(
        "kinds".to_owned(),
        json!(
            query
                .kinds()
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
        ),
    );
    put(&mut filters, "symbol", json!(query.symbol()));
    put(&mut filters, "ticket", json!(ticket));
    put(&mut filters, "outcome", json!(query.outcome()));
    put(
        &mut filters,
        "since_utc",
        json!(since_ms.and_then(utc_text)),
    );
    put(
        &mut filters,
        "until_utc",
        json!(until_ms.and_then(utc_text)),
    );
    filters.insert("limit".to_owned(), json!(limit));
    let mut envelope = Map::new();
    envelope.insert("filters".to_owned(), Value::Object(filters));
    envelope.insert("order".to_owned(), json!("newest first"));
    envelope.insert("returned".to_owned(), json!(rows.len()));
    envelope.insert(
        "retention_days".to_owned(),
        json!(state.config().audit_retention_days()),
    );
    Ok(fit_text_rows(envelope, "events", |bound| {
        rows.iter()
            .map(|row| event_row(row, bound, operator))
            .collect()
    }))
}

/// One open position as bounded evidence, with UTC open time when the broker
/// clock is known.
pub(super) fn position_row(
    position: &PositionPayload,
    clock: Option<BrokerClock>,
    server_time: i64,
    operator: OperatorOffset,
) -> Value {
    let mut row = Map::new();
    row.insert("ticket".to_owned(), json!(position.ticket));
    row.insert("symbol".to_owned(), json!(position.symbol));
    row.insert("side".to_owned(), json!(side(position.kind)));
    row.insert("kind".to_owned(), json!(position.kind));
    row.insert("lots".to_owned(), json!(position.lots));
    row.insert("open_price".to_owned(), json!(position.price));
    row.insert("current_price".to_owned(), json!(position.current));
    row.insert("stop_loss".to_owned(), json!(position.stop_loss));
    row.insert("take_profit".to_owned(), json!(position.take_profit));
    row.insert(
        "net".to_owned(),
        json!(super::bounded::cents(
            position.profit + position.swap + position.commission
        )),
    );
    row.insert("profit".to_owned(), json!(position.profit));
    row.insert("swap".to_owned(), json!(position.swap));
    row.insert("commission".to_owned(), json!(position.commission));
    row.insert("magic".to_owned(), json!(position.magic));
    row.insert("veyra".to_owned(), json!(position.magic == ORDER_MAGIC));
    if position.opened_at > 0 {
        match clock {
            Some(clock) => {
                let opened = clock.to_utc_secs(position.opened_at).saturating_mul(1_000);
                put(&mut row, "opened_utc", json!(utc_text(opened)));
                put(&mut row, "opened_local", json!(operator.local_text(opened)));
            }
            None => put(
                &mut row,
                "opened_broker",
                json!(broker_text(position.opened_at)),
            ),
        }
        if server_time >= position.opened_at {
            let held = server_time - position.opened_at;
            row.insert("held".to_owned(), json!(duration_text(held)));
            row.insert("held_secs".to_owned(), json!(held));
        }
    }
    Value::Object(row)
}

/// `positions`: the retained open book with UTC open times, bounded.
///
/// # Errors
/// Returns a bounded reason for invalid arguments or a missing broker.
pub(super) async fn positions(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let args = Args::new("positions", arguments, &["utc_offset_minutes"])?;
    let operator = args.operator_offset()?;
    let broker = state
        .broker()
        .ok_or_else(|| "broker_unavailable".to_owned())?;
    let link = broker.link();
    let report = link.report().await;
    let snapshot = link.last_account();
    let age_secs = link.last_account_age(state.now()).map(|age| age.as_secs());
    let clock = BrokerClock::from_state(state);
    let mut envelope = Map::new();
    envelope.insert("fresh".to_owned(), json!(report.fresh));
    envelope.insert("age_secs".to_owned(), json!(age_secs));
    envelope.insert(
        "positions_truncated".to_owned(),
        json!(
            snapshot
                .as_ref()
                .is_some_and(|account| account.positions_truncated)
        ),
    );
    if snapshot.is_some() {
        envelope.insert(
            "broker_clock".to_owned(),
            match &clock {
                Ok(clock) => clock.describe(),
                Err(reason) => unknown_clock(reason),
            },
        );
    }
    let Some(snapshot) = snapshot else {
        envelope.insert("positions".to_owned(), Value::Null);
        envelope.insert(
            "note".to_owned(),
            json!("No account snapshot has been received yet."),
        );
        return Ok(Value::Object(envelope));
    };
    let clock = clock.ok();
    let rows = snapshot
        .positions
        .iter()
        .map(|position| position_row(position, clock, snapshot.server_time, operator))
        .collect();
    Ok(fit_list(envelope, "positions", rows))
}

/// Command ids named by the rows, in first-seen order.
fn command_ids(rows: &[AuditRow]) -> Vec<String> {
    let mut ids = Vec::new();
    for row in rows {
        if let Some(id) = row.payload.get("command_id").and_then(Value::as_str)
            && CommandId::parse(id).is_some()
            && !ids.iter().any(|known: &String| known == id)
        {
            ids.push(id.to_owned());
        }
    }
    ids
}

/// Whether a completed-command row reports `ticket` as its fill.
fn is_open_fill(row: &AuditRow, ticket: i64) -> bool {
    row.kind == "command_completed"
        && row.payload.get("kind").and_then(Value::as_str) == Some("open_order")
        && row
            .payload
            .get("result")
            .and_then(|result| result.get("ticket"))
            .and_then(Value::as_i64)
            == Some(ticket)
}

/// Sort key: parsed trail time, then the raw text for unparseable rows.
fn chronological(rows: &mut [AuditRow]) {
    rows.sort_by(|left, right| {
        parse_trail_time(&left.at)
            .cmp(&parse_trail_time(&right.at))
            .then_with(|| left.at.cmp(&right.at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// `position_story`: one ticket's entry decision, adjustments, command
/// outcomes, and close, oldest first.
///
/// # Errors
/// Returns a bounded reason for invalid arguments or an unreadable trail.
/// A missing broker or history is reported inside the story instead.
pub(super) async fn position_story(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let args = Args::new(
        "position_story",
        arguments,
        &["ticket", "days", "utc_offset_minutes"],
    )?;
    let ticket = args
        .ticket()?
        .ok_or_else(|| "ticket is required: a positive venue ticket".to_owned())?;
    let days = args.days()?;
    let operator = args.operator_offset()?;
    let audit = state
        .audit()
        .ok_or_else(|| "audit_unavailable: the durable audit trail is not configured".to_owned())?;
    let trail = audit.trail();

    // Everything that names the ticket, then everything that names one of
    // its commands: the entry decision carries only the open command's id.
    let mut rows = trail
        .query(
            &AuditQuery::new(&STORY_KINDS, STORY_ROWS)
                .and_then(|query| query.with_ticket(ticket))
                .map_err(query_error)?,
        )
        .await
        .map_err(audit_error)?;
    let open_commands: Vec<String> = command_ids(
        &rows
            .iter()
            .filter(|row| is_open_fill(row, ticket))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let linked_ids = command_ids(&rows);
    if !linked_ids.is_empty() {
        let ids: Vec<String> = linked_ids
            .into_iter()
            .take(crate::audit::MAX_QUERY_COMMAND_IDS)
            .collect();
        let linked = trail
            .query(
                &AuditQuery::new(&STORY_KINDS, STORY_ROWS)
                    .and_then(|query| query.with_command_ids(&ids))
                    .map_err(query_error)?,
            )
            .await
            .map_err(audit_error)?;
        rows.extend(linked);
    }
    let mut seen = BTreeSet::new();
    rows.retain(|row| seen.insert(row.id.clone()));
    chronological(&mut rows);

    let entry = rows.iter().find(|row| {
        row.kind == "proposal_evaluated"
            && row
                .payload
                .get("command_id")
                .and_then(Value::as_str)
                .is_some_and(|id| open_commands.iter().any(|open| open == id))
    });
    let open_queued = rows.iter().find(|row| {
        row.kind == "command_queued"
            && row
                .payload
                .get("command_id")
                .and_then(Value::as_str)
                .is_some_and(|id| open_commands.iter().any(|open| open == id))
    });

    let mut gaps = Vec::new();
    let now_ms = unix_ms(state.now())?;
    let retention = state.config().audit_retention_days();

    // Current book first; only a ticket that is not open needs the history.
    let clock = BrokerClock::from_state(state);
    let snapshot = state
        .broker()
        .and_then(|broker| broker.link().last_account());
    let open_position = snapshot.as_ref().and_then(|account| {
        account
            .positions
            .iter()
            .find(|position| position.ticket == ticket)
            .map(|position| {
                position_row(
                    position,
                    clock.as_ref().ok().copied(),
                    account.server_time,
                    operator,
                )
            })
    });
    let mut closed_trade = None;
    let mut history_error = None;
    if open_position.is_none() {
        let earliest_ms = rows.first().and_then(|row| parse_trail_time(&row.at));
        let history_days = days.unwrap_or_else(|| {
            earliest_ms.map_or(OrderHistoryRequest::DEFAULT_DAYS, |earliest| {
                u32::try_from(now_ms.saturating_sub(earliest).div_euclid(DAY_MS) + 2)
                    .unwrap_or(OrderHistoryRequest::MAX_DAYS)
                    .clamp(1, OrderHistoryRequest::MAX_DAYS)
            })
        });
        match fetch_history(state, history_days).await {
            Ok(history) => {
                closed_trade = history
                    .orders
                    .iter()
                    .find(|trade| trade.ticket == ticket)
                    .map(|trade| trade_row(trade, clock.as_ref().ok().copied(), operator));
                if closed_trade.is_none() {
                    gaps.push(format!(
                        "Ticket {ticket} is neither in the latest open book nor in the last {history_days} days of Veyra-owned closed orders{}.",
                        if history.truncated { " (the terminal capped that history)" } else { "" }
                    ));
                }
            }
            Err(reason) => history_error = Some(reason),
        }
    }
    let status = match (&open_position, &closed_trade, &history_error) {
        (Some(_), _, _) => "open",
        (None, Some(_), _) => "closed",
        (None, None, None) => "not_found",
        (None, None, Some(_)) => "unknown",
    };

    if entry.is_none() {
        gaps.push(match (open_commands.is_empty(), open_queued) {
            (true, _) => format!(
                "No open_order completion reporting ticket {ticket} is in the audit trail (it keeps {retention} days), so no entry decision can be linked; the position may predate retention or have been opened outside Veyra's command channel."
            ),
            (false, Some(queued)) => format!(
                "The open command was queued{} but no autopilot proposal_evaluated event carries its command id, so no model rationale was recorded for this entry.",
                queued
                    .payload
                    .get("intent_id")
                    .and_then(Value::as_str)
                    .map(|intent| format!(" for intent {}", clip(intent, 40)))
                    .unwrap_or_default()
            ),
            (false, None) => "The open fill is recorded, but neither its queued command nor an entry decision is in the trail.".to_owned(),
        });
    }

    // Hold reviews repeat every tick; keep the latest few verbatim.
    let held_total = rows
        .iter()
        .filter(|row| row.payload.get("outcome").and_then(Value::as_str) == Some("held"))
        .count();
    let mut held_skip = held_total.saturating_sub(STORY_HELD_REVIEWS);
    let timeline_rows: Vec<&AuditRow> = rows
        .iter()
        .filter(|row| {
            if row.payload.get("outcome").and_then(Value::as_str) == Some("held") && held_skip > 0 {
                held_skip -= 1;
                return false;
            }
            true
        })
        .collect();

    let symbol = open_position
        .as_ref()
        .and_then(|row| row.get("symbol").cloned())
        .or_else(|| {
            closed_trade
                .as_ref()
                .and_then(|row| row.get("symbol").cloned())
        })
        .or_else(|| {
            rows.iter()
                .find_map(|row| row.payload.get("symbol").and_then(Value::as_str))
                .map(|symbol| json!(symbol))
        });

    let mut envelope = Map::new();
    envelope.insert("ticket".to_owned(), json!(ticket));
    envelope.insert("status".to_owned(), json!(status));
    put(&mut envelope, "symbol", symbol.unwrap_or(Value::Null));
    if open_position.is_some()
        && let Some(broker) = state.broker()
    {
        // "Open" is only as current as the retained book.
        let link = broker.link();
        envelope.insert("book_fresh".to_owned(), json!(link.report().await.fresh));
        envelope.insert(
            "book_age_secs".to_owned(),
            json!(link.last_account_age(state.now()).map(|age| age.as_secs())),
        );
    }
    put(
        &mut envelope,
        "open_position",
        open_position.unwrap_or(Value::Null),
    );
    put(
        &mut envelope,
        "closed_trade",
        closed_trade.unwrap_or(Value::Null),
    );
    envelope.insert(
        "entry_decision".to_owned(),
        entry.map_or(Value::Null, |row| event_row(row, MAX_TEXT_CHARS, operator)),
    );
    envelope.insert(
        "broker_clock".to_owned(),
        match &clock {
            Ok(clock) => clock.describe(),
            Err(reason) => unknown_clock(reason),
        },
    );
    envelope.insert(
        "held_reviews".to_owned(),
        json!({"total": held_total, "shown": held_total.min(STORY_HELD_REVIEWS)}),
    );
    put(&mut envelope, "history_error", json!(history_error));
    envelope.insert("gaps".to_owned(), json!(gaps));
    envelope.insert("order".to_owned(), json!("oldest first"));
    let fitted = fit_text_rows(envelope, "timeline", |bound| {
        timeline_rows
            .iter()
            .map(|row| event_row(row, bound, operator))
            .collect()
    });
    Ok(fitted)
}

/// Bounded, balance-free detail of one completed command payload.
pub(super) fn command_detail(payload: &CommandPayload) -> Value {
    match payload {
        CommandPayload::Ping => json!({}),
        CommandPayload::AccountSnapshot(snapshot) => json!({
            "orders": snapshot.orders,
            "lots": snapshot.lots,
            "tickets": snapshot.positions.iter().take(10).map(|position| position.ticket).collect::<Vec<_>>(),
            "positions_truncated": snapshot.positions_truncated
        }),
        CommandPayload::OrderCheck(check) => json!({
            "passed": check.passed,
            "retcode": check.retcode,
            "comment": clip(&check.comment, 160),
            "margin": check.margin
        }),
        CommandPayload::OpenOrder(execution)
        | CommandPayload::CloseOrder(execution)
        | CommandPayload::ModifyOrder(execution) => json!({
            "executed": execution.executed,
            "retcode": execution.retcode,
            "comment": clip(&execution.comment, 160),
            "ticket": execution.ticket,
            "price": execution.price
        }),
        CommandPayload::Rates(rates) => json!({
            "symbol": rates.symbol,
            "timeframe_minutes": rates.timeframe_minutes,
            "candles": rates.candles.len()
        }),
        CommandPayload::SymbolSpec(spec) => json!({
            "symbol": spec.symbol,
            "spread_points": spec.spread_points,
            "stop_level_points": spec.stop_level_points,
            "lot_min": spec.lot_min,
            "lot_step": spec.lot_step,
            "margin_required": spec.margin_required,
            "trade_allowed": spec.trade_allowed
        }),
        CommandPayload::OrderHistory(history) => json!({
            "orders": history.orders.len(),
            "total": history.total,
            "truncated": history.truncated,
            "tickets": history.orders.iter().take(10).map(|trade| trade.ticket).collect::<Vec<_>>()
        }),
    }
}

/// `recent_commands`: the in-memory command window with outcome detail and
/// the durable queueing context (who queued it, for which decision).
///
/// # Errors
/// Returns a bounded reason for invalid arguments or a missing broker.
pub(super) async fn recent_commands(state: &AppState, arguments: &Value) -> Result<Value, String> {
    let args = Args::new(
        "recent_commands",
        arguments,
        &["limit", "kind", "include_routine", "utc_offset_minutes"],
    )?;
    let limit = args
        .integer("limit", 1, MAX_COMMAND_ROWS)?
        .unwrap_or(DEFAULT_COMMAND_ROWS);
    let kind = args
        .text("kind")?
        .map(|name| {
            COMMAND_KINDS
                .into_iter()
                .find(|kind| kind.as_str() == name)
                .ok_or_else(|| {
                    format!(
                        "unknown command kind `{}`; accepted: {}",
                        clip(name, 40),
                        COMMAND_KINDS.map(CommandKind::as_str).join(", ")
                    )
                })
        })
        .transpose()?;
    let include_routine = args.flag("include_routine")?.unwrap_or(false);
    let operator = args.operator_offset()?;
    let broker = state
        .broker()
        .ok_or_else(|| "broker_unavailable".to_owned())?;
    let link = broker.link();
    let listed = link.recent_commands(COMMAND_WINDOW);
    let routine_omitted = if kind.is_none() && !include_routine {
        listed
            .iter()
            .filter(|command| ROUTINE_COMMANDS.contains(&command.kind))
            .count()
    } else {
        0
    };
    let selected: Vec<_> = listed
        .into_iter()
        .filter(|command| match kind {
            Some(kind) => command.kind == kind,
            None => include_routine || !ROUTINE_COMMANDS.contains(&command.kind),
        })
        .take(usize::try_from(limit).unwrap_or(15))
        .collect();

    // Durable context: the queue event and any decision naming each command.
    let ids: Vec<String> = selected
        .iter()
        .map(|command| command.id.to_string())
        .collect();
    let mut context: Vec<AuditRow> = Vec::new();
    let mut audit_context = "unavailable";
    if let Some(audit) = state.audit()
        && !ids.is_empty()
    {
        let query = AuditQuery::new(
            &[AuditKind::CommandQueued, AuditKind::ProposalEvaluated],
            crate::audit::MAX_QUERY_ROWS,
        )
        .and_then(|query| query.with_command_ids(&ids))
        .map_err(query_error)?;
        if let Ok(rows) = audit.trail().query(&query).await {
            context = rows;
            audit_context = "joined";
        }
    }
    let for_command = |id: &str, kind: &str| {
        context.iter().find(|row| {
            row.kind == kind && row.payload.get("command_id").and_then(Value::as_str) == Some(id)
        })
    };
    let rows = |bound: usize| {
        selected
            .iter()
            .map(|command| {
                let id = command.id.to_string();
                let mut row = Map::new();
                row.insert("id".to_owned(), json!(id));
                row.insert("kind".to_owned(), json!(command.kind.as_str()));
                row.insert("status".to_owned(), json!(command.status));
                if let Some(queued) = for_command(&id, "command_queued") {
                    let at = parse_trail_time(&queued.at);
                    put(&mut row, "queued_at", json!(at.and_then(utc_text)));
                    put(
                        &mut row,
                        "queued_local",
                        json!(at.and_then(|ms| operator.local_text(ms))),
                    );
                    for key in ["origin", "intent_id", "ticket"] {
                        put(&mut row, key, scalar_field(&queued.payload, key));
                    }
                }
                match link.command(command.id).map(|record| record.state) {
                    Some(CommandState::Completed { payload }) => {
                        row.insert("detail".to_owned(), command_detail(&payload));
                    }
                    Some(CommandState::Failed { reason }) => {
                        row.insert("reason".to_owned(), json!(clip(&reason, bound)));
                    }
                    Some(CommandState::Pending) | None => {
                        put(
                            &mut row,
                            "reason",
                            json!(command.reason.as_deref().map(|reason| clip(reason, bound))),
                        );
                    }
                }
                if let Some(decision) = for_command(&id, "proposal_evaluated") {
                    row.insert(
                        "decision".to_owned(),
                        event_row(decision, bound.min(400), operator),
                    );
                }
                Value::Object(row)
            })
            .collect()
    };
    let mut envelope = Map::new();
    envelope.insert(
        "window".to_owned(),
        json!("in-memory command history since the service started (at most the last 64 commands); use decision_history with command kinds for older, durable records"),
    );
    envelope.insert("limited_to".to_owned(), json!(limit));
    envelope.insert("audit_context".to_owned(), json!(audit_context));
    if routine_omitted > 0 {
        envelope.insert("routine_omitted".to_owned(), json!(routine_omitted));
    }
    if let Some(kind) = kind {
        envelope.insert("kind".to_owned(), json!(kind.as_str()));
    }
    Ok(fit_text_rows(envelope, "commands", rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(at: &str, kind: &str, payload: Value) -> AuditRow {
        AuditRow {
            id: format!("{kind}-{at}"),
            at: at.to_owned(),
            kind: kind.to_owned(),
            payload,
        }
    }

    #[test]
    fn event_rows_keep_decision_fields_and_clip_text() {
        let decision = row(
            "2026-09-24T06:40:00.000Z",
            "proposal_evaluated",
            json!({
                "outcome": "queued",
                "origin": "autopilot",
                "symbol": "USDJPY",
                "side": "buy",
                "volume": 0.01,
                "stop_loss": 155.9,
                "take_profit": 156.8,
                "rationale": "r".repeat(2_000),
                "command_id": "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10",
                "judgements": {"direction": "up"},
                "agent_tools": ["candles", "calendar"]
            }),
        );
        let event = event_row(&decision, 100, OperatorOffset::new(120).expect("offset"));
        assert_eq!(event["at"], "2026-09-24T06:40:00Z");
        assert_eq!(event["at_local"], "2026-09-24T08:40:00+02:00");
        assert_eq!(event["side"], "buy");
        assert_eq!(
            event["rationale"].as_str().map(|text| text.chars().count()),
            Some(101)
        );
        assert!(event.get("judgements").is_none(), "bulky context stays out");
        assert_eq!(event["agent_tools"], json!(["candles", "calendar"]));

        let closed = row(
            "2026-09-24 06:46:09.5+00",
            "position_closed",
            json!({"ticket": 10654130, "symbol": "USDJPY", "kind": "sell", "lots": 0.01, "price": 156.41, "profit": 1.36}),
        );
        let event = event_row(&closed, 100, OperatorOffset::default());
        assert_eq!(event["at"], "2026-09-24T06:46:09Z");
        assert_eq!(event["side"], "Short");
        assert!(event.get("at_local").is_none());
        let bought = row(
            "2026-09-24T06:46:09Z",
            "position_closed",
            json!({"ticket": 1, "kind": "buy"}),
        );
        assert_eq!(
            event_row(&bought, 100, OperatorOffset::default())["side"],
            "Long"
        );

        let completed = row(
            "not a time",
            "command_completed",
            json!({"kind": "open_order", "command_id": "x", "result": {"executed": true, "ticket": 5, "note": "n".repeat(400)}}),
        );
        let event = event_row(&completed, 100, OperatorOffset::default());
        assert_eq!(event["at"], "not a time");
        assert_eq!(event["command_kind"], "open_order");
        assert!(
            event["result"].is_string(),
            "oversized results are clipped text"
        );
        assert!(is_open_fill(
            &row(
                "t",
                "command_completed",
                json!({"kind": "open_order", "result": {"ticket": 5}})
            ),
            5
        ));
        assert!(!is_open_fill(&completed, 6));
    }

    #[test]
    fn command_ids_are_valid_unique_and_ordered() {
        let rows = vec![
            row(
                "a",
                "command_queued",
                json!({"command_id": "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10"}),
            ),
            row("b", "command_queued", json!({"command_id": "not-a-uuid"})),
            row(
                "c",
                "command_completed",
                json!({"command_id": "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10"}),
            ),
            row("d", "command_failed", json!({})),
        ];
        assert_eq!(
            command_ids(&rows),
            vec!["5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10".to_owned()]
        );
        let mut unordered = vec![
            row("2026-09-24T06:00:00.000Z", "b", json!({})),
            row("2026-09-24 05:00:00+00", "a", json!({})),
            row("garbage", "c", json!({})),
        ];
        chronological(&mut unordered);
        assert_eq!(
            unordered
                .iter()
                .map(|row| row.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "a", "b"],
            "unparseable times sort first, the rest chronologically"
        );
    }
}
