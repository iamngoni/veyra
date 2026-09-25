//! Closed-trade reporting for `GET /trades`: the venue's realized fills
//! joined with *why* each one closed.
//!
//! This module is pure aggregation over already-fetched history: the route
//! queues the same read-only `order_history` command `/performance` uses
//! (see [`crate::control::performance`]) and hands the validated
//! [`OrderHistoryPayload`] here. Classifying a close reuses the assistant's
//! ticket-linking query ([`crate::trade_journal`]) to find, for each ticket,
//! the entry decision that opened it and any recorded stop moves or closes —
//! nothing here enqueues a second command or touches an order path.
//!
//! Limitation, stated rather than hidden: the audit trail records that a
//! stop moved and which policy moved it, but never the resulting numeric
//! level (the terminal only ever reports a modify's retcode and ticket, not
//! its new price — see [`crate::broker::command::CommandPayload::ModifyOrder`]).
//! So the reported `stopLoss` is the entry level, updated to the entry price
//! only when the last recorded move was a break-even (the one case whose
//! target price is derivable without the missing data); a later trailing or
//! profit-harvest move is still used to classify the close, just not to
//! move the displayed stop.

use serde::Serialize;
use serde_json::Value;

use crate::AppState;
use crate::audit::{AuditRow, AuditRuntime};
use crate::broker::{ClosedTradePayload, OrderHistoryPayload, PositionKind};
use crate::broker_clock::BrokerClock;

/// Largest trade list one response returns.
const MAX_TRADES: usize = 256;

/// Longest `entryRationale` / `closeDetail` string returned.
const MAX_TEXT_CHARS: usize = 600;

/// Why one Veyra-owned trade closed, decided in a fixed priority order (see
/// [`classify`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The autopilot's reviewer queued the close (`proposal_evaluated`,
    /// outcome `close_queued`).
    AgentClose,
    /// The profit-harvest policy banked a retracing high-water mark
    /// (`proposal_evaluated`, outcome `profit_harvest_close`).
    HarvestClose,
    /// An operator queued the close through `POST /intents/close`.
    ManualClose,
    /// The close price is within tolerance of the last known target.
    TakeProfit,
    /// Within tolerance of the stop, last moved to break-even.
    BreakEvenStop,
    /// Within tolerance of the stop, last moved by the trailing policy.
    TrailingStop,
    /// Within tolerance of the stop, last moved by the profit-harvest policy.
    HarvestStop,
    /// Within tolerance of the stop, never moved.
    StopLoss,
    /// None of the above matched; closed at the venue outside Veyra's view.
    Unknown,
}

/// One closed trade as the console renders it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeRecord {
    /// Venue ticket.
    pub ticket: i64,
    /// Instrument.
    pub symbol: String,
    /// `"long"` or `"short"`.
    pub side: &'static str,
    /// Volume in lots.
    pub lots: f64,
    /// Open time, UTC milliseconds.
    pub opened_at_ms: i64,
    /// Close time, UTC milliseconds.
    pub closed_at_ms: i64,
    /// Entry fill.
    pub open_price: f64,
    /// Exit fill.
    pub close_price: f64,
    /// Last known stop loss (entry value, updated to the entry price by a
    /// recorded break-even move), or `None` when never recorded.
    pub stop_loss: Option<f64>,
    /// Last known take profit (the entry value; autopilot moves never change
    /// it), or `None` when never recorded.
    pub take_profit: Option<f64>,
    /// `profit + swap + commission`, rounded to cents.
    pub net: f64,
    /// Realized gross profit, rounded to cents.
    pub profit: f64,
    /// Swap charged or credited, rounded to cents.
    pub swap: f64,
    /// Commission charged, rounded to cents.
    pub commission: f64,
    /// Favourable move divided by the original entry-to-stop distance,
    /// rounded to 2 decimals, or `None` without an original stop.
    pub r_multiple: Option<f64>,
    /// Why the trade closed.
    pub close_reason: CloseReason,
    /// Short supporting detail for [`TradeRecord::close_reason`], when one
    /// was recorded.
    pub close_detail: Option<String>,
    /// The entry decision's recorded rationale, clipped, when one was found.
    pub entry_rationale: Option<String>,
}

/// Trade counts and net result for the window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TradeSummary {
    /// Closed trades in the window.
    pub count: u32,
    /// Trades with net profit above zero.
    pub wins: u32,
    /// Trades with net profit below zero.
    pub losses: u32,
    /// Trades whose net profit is exactly zero.
    pub breakeven: u32,
    /// Net realized profit across every trade.
    pub net: f64,
}

/// Assembled report body, minus the request echo (`days`, `total`,
/// `truncated`) the route already holds.
#[derive(Debug, Clone, PartialEq)]
pub struct TradesReport {
    /// Estimated broker-clock offset from UTC, in seconds; `None` without a
    /// retained account snapshot.
    pub broker_offset_secs: Option<i64>,
    /// Counts and net result across every returned trade.
    pub summary: TradeSummary,
    /// Closed trades, newest first, at most [`MAX_TRADES`].
    pub trades: Vec<TradeRecord>,
}

/// Rounds to two decimals for stable, readable JSON.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// `"long"` / `"short"` label for a venue order kind.
fn side_label(kind: PositionKind) -> &'static str {
    match kind {
        PositionKind::Buy
        | PositionKind::BuyLimit
        | PositionKind::BuyStop
        | PositionKind::BuyStopLimit => "long",
        PositionKind::Sell
        | PositionKind::SellLimit
        | PositionKind::SellStop
        | PositionKind::SellStopLimit => "short",
    }
}

/// The three deterministic stop-move policies, in the order their audited
/// outcome strings are recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMoveKind {
    BreakEven,
    Trailing,
    Harvest,
}

/// Evidence gathered from one ticket's linked audit rows.
#[derive(Debug, Clone, Default)]
struct TicketEvidence {
    entry_rationale: Option<String>,
    entry_stop_loss: Option<f64>,
    entry_take_profit: Option<f64>,
    agent_close: bool,
    agent_close_detail: Option<String>,
    harvest_close: bool,
    harvest_close_detail: Option<String>,
    manual_close: bool,
    last_stop_move: Option<StopMoveKind>,
}

/// A short "gave back X of Y peak" detail from a harvest-close event's
/// high-water and then-current net profit, when both are recorded and finite.
fn harvest_detail(payload: &Value) -> Option<String> {
    let high = payload
        .get("high_net_profit")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite());
    let net = payload
        .get("net_profit")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite());
    let (high, net) = (high?, net?);
    let given_back = (high - net).max(0.0);
    Some(crate::text::clip(
        &format!("gave back {given_back:.2} of {high:.2} peak"),
        MAX_TEXT_CHARS,
    ))
}

/// Builds [`TicketEvidence`] from one ticket's linked rows (see
/// [`crate::trade_journal::ticket_episode`]). Rows are oldest first, so a
/// later stop-move outcome always overwrites an earlier one, leaving the
/// last recorded move.
fn evidence_from_rows(rows: &[AuditRow], open_commands: &[String]) -> TicketEvidence {
    let mut evidence = TicketEvidence::default();
    if let Some(entry) = crate::trade_journal::entry_decision(rows, open_commands) {
        evidence.entry_rationale = entry
            .payload
            .get("rationale")
            .and_then(Value::as_str)
            .map(|text| crate::text::clip(text, MAX_TEXT_CHARS));
        evidence.entry_stop_loss = entry.payload.get("stop_loss").and_then(Value::as_f64);
        evidence.entry_take_profit = entry.payload.get("take_profit").and_then(Value::as_f64);
    }
    for row in rows {
        if row.kind == "command_queued" {
            if row.payload.get("kind").and_then(Value::as_str) == Some("close_order") {
                evidence.manual_close = true;
            }
            continue;
        }
        if row.kind != "proposal_evaluated" {
            continue;
        }
        match row.payload.get("outcome").and_then(Value::as_str) {
            Some("close_queued") => {
                evidence.agent_close = true;
                evidence.agent_close_detail = row
                    .payload
                    .get("rationale")
                    .and_then(Value::as_str)
                    .or_else(|| row.payload.get("reason").and_then(Value::as_str))
                    .map(|text| crate::text::clip(text, MAX_TEXT_CHARS));
            }
            Some("profit_harvest_close") => {
                evidence.harvest_close = true;
                evidence.harvest_close_detail = harvest_detail(&row.payload);
            }
            Some("break_even") => evidence.last_stop_move = Some(StopMoveKind::BreakEven),
            Some("trailing_stop") => evidence.last_stop_move = Some(StopMoveKind::Trailing),
            Some("profit_harvest_stop") => evidence.last_stop_move = Some(StopMoveKind::Harvest),
            _ => {}
        }
    }
    evidence
}

/// Favourable move ÷ |entry − original stop|, rounded to 2 decimals, or
/// `None` without a usable original stop.
fn r_multiple(trade: &ClosedTradePayload, original_stop: Option<f64>) -> Option<f64> {
    let stop = original_stop?;
    let risk = (trade.open_price - stop).abs();
    if risk <= 0.0 {
        return None;
    }
    let favourable_move = match trade.kind {
        PositionKind::Buy
        | PositionKind::BuyLimit
        | PositionKind::BuyStop
        | PositionKind::BuyStopLimit => trade.close_price - trade.open_price,
        PositionKind::Sell
        | PositionKind::SellLimit
        | PositionKind::SellStop
        | PositionKind::SellStopLimit => trade.open_price - trade.close_price,
    };
    Some(round2(favourable_move / risk))
}

/// `max(5% of |entry − original stop| when known, 0.0002 × closePrice)`.
///
/// The venue's spread is deliberately left out: today's spread is not a
/// trustworthy stand-in for the spread at a past close, and the module has
/// no other source for it (see the module-level limitation note).
fn tolerance(open_price: f64, close_price: f64, original_stop: Option<f64>) -> f64 {
    let mut bound = 0.0002 * close_price;
    if let Some(stop) = original_stop {
        bound = bound.max(0.05 * (open_price - stop).abs());
    }
    bound
}

/// Decides why one trade closed, in the fixed priority order the contract
/// specifies: an autopilot-queued close, a profit-harvest close, an
/// operator-queued close, then a tolerance comparison against the last known
/// target and stop, falling back to `unknown`.
fn classify(
    trade: &ClosedTradePayload,
    evidence: &TicketEvidence,
) -> (CloseReason, Option<String>) {
    if evidence.agent_close {
        return (CloseReason::AgentClose, evidence.agent_close_detail.clone());
    }
    if evidence.harvest_close {
        return (
            CloseReason::HarvestClose,
            evidence.harvest_close_detail.clone(),
        );
    }
    if evidence.manual_close {
        return (CloseReason::ManualClose, None);
    }
    let last_stop = last_known_stop_loss(trade, evidence);
    let last_target = evidence.entry_take_profit;
    let bound = tolerance(
        trade.open_price,
        trade.close_price,
        evidence.entry_stop_loss,
    );
    if let Some(target) = last_target.filter(|target| *target > 0.0)
        && (trade.close_price - target).abs() <= bound
    {
        return (CloseReason::TakeProfit, None);
    }
    if let Some(stop) = last_stop.filter(|stop| *stop > 0.0)
        && (trade.close_price - stop).abs() <= bound
    {
        let reason = match evidence.last_stop_move {
            Some(StopMoveKind::BreakEven) => CloseReason::BreakEvenStop,
            Some(StopMoveKind::Trailing) => CloseReason::TrailingStop,
            Some(StopMoveKind::Harvest) => CloseReason::HarvestStop,
            None => CloseReason::StopLoss,
        };
        return (reason, None);
    }
    (CloseReason::Unknown, None)
}

/// Last known stop level: the entry price once a break-even move is
/// recorded (the only move whose resulting price is derivable — see the
/// module-level limitation note), else the entry's original stop.
fn last_known_stop_loss(trade: &ClosedTradePayload, evidence: &TicketEvidence) -> Option<f64> {
    match evidence.last_stop_move {
        Some(StopMoveKind::BreakEven) => Some(trade.open_price),
        _ => evidence.entry_stop_loss,
    }
}

/// Looks up one ticket's linked audit rows and turns them into evidence,
/// degrading to no evidence (never failing the whole report) when the trail
/// is unavailable or a single lookup fails.
async fn ticket_evidence(audit: Option<&AuditRuntime>, ticket: i64) -> TicketEvidence {
    let Some(audit) = audit else {
        return TicketEvidence::default();
    };
    match crate::trade_journal::ticket_episode(
        audit.trail(),
        &crate::trade_journal::STORY_KINDS,
        crate::trade_journal::STORY_ROWS,
        ticket,
    )
    .await
    {
        Ok(episode) => evidence_from_rows(&episode.rows, &episode.open_commands),
        Err(error) => {
            tracing::warn!(%error, ticket, "trade episode lookup failed");
            TicketEvidence::default()
        }
    }
}

/// Builds one [`TradeRecord`] from a validated closed trade and its
/// evidence, converting broker-clock times to UTC when `clock` is known.
fn build_record(
    trade: &ClosedTradePayload,
    clock: Option<BrokerClock>,
    evidence: TicketEvidence,
) -> TradeRecord {
    let (close_reason, close_detail) = classify(trade, &evidence);
    let (opened_at_ms, closed_at_ms) = match clock {
        Some(clock) => (
            clock.to_utc_secs(trade.open_time).saturating_mul(1_000),
            clock.to_utc_secs(trade.close_time).saturating_mul(1_000),
        ),
        None => (
            trade.open_time.saturating_mul(1_000),
            trade.close_time.saturating_mul(1_000),
        ),
    };
    TradeRecord {
        ticket: trade.ticket,
        symbol: trade.symbol.clone(),
        side: side_label(trade.kind),
        lots: trade.lots,
        opened_at_ms,
        closed_at_ms,
        open_price: trade.open_price,
        close_price: trade.close_price,
        stop_loss: last_known_stop_loss(trade, &evidence),
        take_profit: evidence.entry_take_profit,
        net: round2(trade.net_profit()),
        profit: round2(trade.profit),
        swap: round2(trade.swap),
        commission: round2(trade.commission),
        r_multiple: r_multiple(trade, evidence.entry_stop_loss),
        close_reason,
        close_detail,
        entry_rationale: evidence.entry_rationale,
    }
}

/// Builds the full report from validated history and already-resolved
/// evidence (the broker clock and, optionally, the audit trail). Kept apart
/// from [`build`] so classification is testable without an [`AppState`].
async fn build_with(
    clock: Option<BrokerClock>,
    audit: Option<&AuditRuntime>,
    history: &OrderHistoryPayload,
) -> TradesReport {
    let mut trades = history.orders.clone();
    trades.sort_by(|left, right| {
        right
            .close_time
            .cmp(&left.close_time)
            .then(right.ticket.cmp(&left.ticket))
    });
    trades.truncate(MAX_TRADES);

    let mut records = Vec::with_capacity(trades.len());
    for trade in &trades {
        let evidence = ticket_evidence(audit, trade.ticket).await;
        records.push(build_record(trade, clock, evidence));
    }

    let report = crate::performance::summarize(&history.orders);
    TradesReport {
        broker_offset_secs: clock.map(BrokerClock::offset_secs),
        summary: TradeSummary {
            count: report.trades,
            wins: report.wins,
            losses: report.losses,
            breakeven: report.breakeven,
            net: report.net_profit,
        },
        trades: records,
    }
}

/// Builds the `/trades` report for validated history: the broker-clock
/// offset from the retained account snapshot (`None` without one), the
/// realized-performance summary, and each trade with its close reason.
///
/// Never fails: an unavailable broker clock or audit trail degrades the
/// affected fields to `None` / `"unknown"` rather than rejecting the
/// request, matching `/performance`'s own tolerance for a missing trail.
pub async fn build(state: &AppState, history: &OrderHistoryPayload) -> TradesReport {
    let clock = BrokerClock::from_state(state).ok();
    build_with(clock, state.audit(), history).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::audit::{AuditEvent, AuditKind, AuditRuntime, MemoryTrail};
    use serde_json::json;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    fn instant(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).expect("fixture time")
    }

    fn trade(ticket: i64, kind: PositionKind, open: f64, close: f64) -> ClosedTradePayload {
        ClosedTradePayload {
            ticket,
            symbol: "GBPUSD".to_owned(),
            kind,
            lots: 0.01,
            open_price: open,
            close_price: close,
            open_time: 1_790_271_000,
            close_time: 1_790_341_680,
            profit: -5.31,
            swap: 0.0,
            commission: 0.0,
            magic: crate::broker::ORDER_MAGIC,
        }
    }

    fn history(orders: Vec<ClosedTradePayload>) -> OrderHistoryPayload {
        let total = orders.len() as u32;
        OrderHistoryPayload {
            orders,
            total,
            truncated: false,
        }
    }

    #[test]
    fn side_labels_are_lowercase() {
        assert_eq!(side_label(PositionKind::Buy), "long");
        assert_eq!(side_label(PositionKind::SellStop), "short");
    }

    #[test]
    fn r_multiple_matches_the_contract_example() {
        // GBPUSD short, entry 1.32123, stop 1.3265, close at the stop.
        let trade = trade(1, PositionKind::Sell, 1.32123, 1.3265);
        assert_eq!(r_multiple(&trade, Some(1.3265)), Some(-1.0));
        assert_eq!(r_multiple(&trade, None), None, "no original stop");
        assert_eq!(
            r_multiple(&trade, Some(1.32123)),
            None,
            "zero-distance stop is not a usable basis"
        );
        let mut winner = trade;
        // Exactly one risk-unit (0.00527) in its favour from the entry.
        winner.close_price = 1.31596;
        assert_eq!(r_multiple(&winner, Some(1.3265)), Some(1.0));
    }

    #[test]
    fn tolerance_uses_the_wider_of_the_stop_fraction_and_the_price_floor() {
        // A wide stop distance makes the 5% term dominate the price floor.
        assert_eq!(
            tolerance(1.3212, 1.3265, Some(1.30)),
            0.05 * (1.3212_f64 - 1.30).abs()
        );
        // A tight stop distance leaves the price floor as the wider bound.
        assert_eq!(tolerance(1.3212, 1.3265, Some(1.3265)), 0.0002 * 1.3265);
        // With no original stop, only the price floor applies.
        assert_eq!(tolerance(1.3212, 100.0, None), 0.0002 * 100.0);
    }

    #[test]
    fn agent_close_takes_priority_and_reports_its_rationale() {
        let evidence = TicketEvidence {
            agent_close: true,
            agent_close_detail: Some("thesis invalidated".to_owned()),
            manual_close: true, // would otherwise win; agent_close must come first
            ..TicketEvidence::default()
        };
        let trade = trade(1, PositionKind::Buy, 1.10, 1.101);
        let (reason, detail) = classify(&trade, &evidence);
        assert_eq!(reason, CloseReason::AgentClose);
        assert_eq!(detail.as_deref(), Some("thesis invalidated"));
    }

    #[test]
    fn harvest_close_outranks_manual_and_reports_the_giveback() {
        let evidence = TicketEvidence {
            harvest_close: true,
            harvest_close_detail: harvest_detail(
                &json!({"high_net_profit": 12.5, "net_profit": 9.0}),
            ),
            manual_close: true,
            ..TicketEvidence::default()
        };
        let trade = trade(1, PositionKind::Buy, 1.10, 1.101);
        let (reason, detail) = classify(&trade, &evidence);
        assert_eq!(reason, CloseReason::HarvestClose);
        assert_eq!(detail.as_deref(), Some("gave back 3.50 of 12.50 peak"));
    }

    #[test]
    fn manual_close_wins_over_the_tolerance_fallback() {
        let evidence = TicketEvidence {
            manual_close: true,
            entry_stop_loss: Some(1.30),
            ..TicketEvidence::default()
        };
        // Close price sits exactly on the stop, which would otherwise read as
        // stop_loss; the recorded manual close must still win.
        let trade = trade(1, PositionKind::Sell, 1.32123, 1.30);
        let (reason, detail) = classify(&trade, &evidence);
        assert_eq!(reason, CloseReason::ManualClose);
        assert_eq!(detail, None);
    }

    #[test]
    fn take_profit_is_checked_before_the_stop() {
        let evidence = TicketEvidence {
            entry_stop_loss: Some(1.3265),
            entry_take_profit: Some(1.3162),
            ..TicketEvidence::default()
        };
        let trade = trade(1, PositionKind::Sell, 1.32123, 1.3162);
        assert_eq!(classify(&trade, &evidence).0, CloseReason::TakeProfit);
    }

    #[test]
    fn stop_reasons_follow_the_last_recorded_move() {
        let base = TicketEvidence {
            entry_stop_loss: Some(1.3265),
            ..TicketEvidence::default()
        };
        let trade = trade(1, PositionKind::Sell, 1.32123, 1.3265);

        assert_eq!(classify(&trade, &base).0, CloseReason::StopLoss);

        let break_even = TicketEvidence {
            last_stop_move: Some(StopMoveKind::BreakEven),
            ..base.clone()
        };
        // Break-even moves the stop to the entry price, so the trade must
        // close near *that* level, not the original stop, to classify.
        let mut break_even_trade = trade.clone();
        break_even_trade.close_price = break_even_trade.open_price;
        assert_eq!(
            classify(&break_even_trade, &break_even).0,
            CloseReason::BreakEvenStop
        );

        let trailing = TicketEvidence {
            last_stop_move: Some(StopMoveKind::Trailing),
            ..base.clone()
        };
        assert_eq!(classify(&trade, &trailing).0, CloseReason::TrailingStop);

        let harvest = TicketEvidence {
            last_stop_move: Some(StopMoveKind::Harvest),
            ..base
        };
        assert_eq!(classify(&trade, &harvest).0, CloseReason::HarvestStop);
    }

    #[test]
    fn unclassified_closes_report_unknown() {
        let evidence = TicketEvidence {
            entry_stop_loss: Some(1.3265),
            entry_take_profit: Some(1.3162),
            ..TicketEvidence::default()
        };
        // Closed well away from both the stop and the target.
        let trade = trade(1, PositionKind::Sell, 1.32123, 1.3200);
        assert_eq!(classify(&trade, &evidence).0, CloseReason::Unknown);
    }

    #[test]
    fn entry_rationale_and_close_detail_are_clipped() {
        let long_text = "r".repeat(2_000);
        let rows = vec![AuditRow {
            id: "1".to_owned(),
            at: "2026-01-01T00:00:00Z".to_owned(),
            kind: "proposal_evaluated".to_owned(),
            payload: json!({
                "outcome": "queued",
                "command_id": "open-1",
                "rationale": long_text,
                "stop_loss": 1.30,
                "take_profit": 1.32
            }),
        }];
        let evidence = evidence_from_rows(&rows, &["open-1".to_owned()]);
        let rationale = evidence.entry_rationale.expect("rationale");
        assert_eq!(
            rationale.chars().count(),
            MAX_TEXT_CHARS + 1,
            "clip marker included"
        );
        assert!(rationale.ends_with('…'));
        assert_eq!(evidence.entry_stop_loss, Some(1.30));
        assert_eq!(evidence.entry_take_profit, Some(1.32));
    }

    #[actix_web::test]
    async fn build_with_reports_the_broker_offset_and_bounded_evidence() {
        let clock = BrokerClock::estimate(1_790_239_566, 1_790_232_369).expect("offset");
        let trail = MemoryTrail::default();
        let open_id = "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10".to_owned();
        trail.record_at(
            instant("2026-01-01T00:00:00Z"),
            AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({
                    "outcome": "queued",
                    "command_id": open_id,
                    "rationale": "GBPUSD has the strongest aligned bearish evidence",
                    "stop_loss": 1.3265,
                    "take_profit": 1.3162
                }),
            ),
        );
        trail.record_at(
            instant("2026-01-01T00:00:05Z"),
            AuditEvent::new(
                AuditKind::CommandCompleted,
                json!({"kind": "open_order", "command_id": open_id, "result": {"ticket": 10_655_087}}),
            ),
        );
        let audit = AuditRuntime::new(Arc::new(trail));

        let trade = ClosedTradePayload {
            ticket: 10_655_087,
            symbol: "GBPUSD".to_owned(),
            kind: PositionKind::Sell,
            lots: 0.01,
            open_price: 1.32123,
            close_price: 1.3265,
            open_time: 1_790_271_000,
            close_time: 1_790_341_680,
            profit: -5.31,
            swap: 0.0,
            commission: 0.0,
            magic: crate::broker::ORDER_MAGIC,
        };
        let report = build_with(Some(clock), Some(&audit), &history(vec![trade])).await;
        assert_eq!(report.broker_offset_secs, Some(7_200));
        assert_eq!(report.summary.count, 1);
        assert_eq!(report.summary.losses, 1);
        assert_eq!(report.summary.net, -5.31);
        let record = &report.trades[0];
        assert_eq!(record.side, "short");
        assert_eq!(record.close_reason, CloseReason::StopLoss);
        assert_eq!(record.stop_loss, Some(1.3265));
        assert_eq!(record.take_profit, Some(1.3162));
        assert_eq!(record.r_multiple, Some(-1.0));
        assert_eq!(
            record.entry_rationale.as_deref(),
            Some("GBPUSD has the strongest aligned bearish evidence")
        );
        // opened/closed times are shifted by the +02:00 broker offset.
        assert_eq!(record.opened_at_ms, (1_790_271_000 - 7_200) * 1_000);
        assert_eq!(record.closed_at_ms, (1_790_341_680 - 7_200) * 1_000);
    }

    #[actix_web::test]
    async fn build_with_is_bounded_and_ordered_without_an_audit_trail() {
        let mut older = trade(1, PositionKind::Buy, 1.0, 1.01);
        older.close_time = 100;
        let mut newer = trade(2, PositionKind::Buy, 1.0, 1.01);
        newer.close_time = 200;
        let report = build_with(None, None, &history(vec![older, newer])).await;
        assert_eq!(report.broker_offset_secs, None);
        assert_eq!(
            report
                .trades
                .iter()
                .map(|trade| trade.ticket)
                .collect::<Vec<_>>(),
            vec![2, 1],
            "newest first"
        );
        for record in &report.trades {
            assert_eq!(record.close_reason, CloseReason::Unknown);
            assert_eq!(record.entry_rationale, None);
            assert_eq!(record.stop_loss, None);
            assert_eq!(record.r_multiple, None);
        }
    }
}
