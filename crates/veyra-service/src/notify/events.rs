//! Turns service activity into notifications.
//!
//! Two read-only watchers feed [`crate::notify::Notifier`]:
//!
//! * [`watch_audit`] follows the in-process audit feed (the same one the
//!   console's `/events` reads) and maps journal entries — fills, closes,
//!   failed orders, reconciliation drift — through [`AuditMapper`].
//! * [`watch_health`] samples state every [`HEALTH_PERIOD`] and reports
//!   *transitions* through [`HealthTracker`]: a breaker starting to block, the
//!   kill switch or execution switch halting trading, the broker link going
//!   stale or recovering, and the autopilot failing to get decisions. It also
//!   sends the daily summary.
//!
//! Neither watcher can change anything: they only read state and queue
//! messages, and [`crate::notify::Notifier::notify`] never blocks. Transition
//! detection keeps a flapping condition from repeating a message every tick.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;

use super::{Notification, NotifyEvent, Severity};
use crate::AppState;
use crate::audit::{AuditKind, AuditQuery, FeedEvent};

/// How often [`watch_health`] samples state.
pub const HEALTH_PERIOD: Duration = Duration::from_secs(30);

/// Consecutive stale samples before the broker link is reported stale.
const STALE_SAMPLES: u32 = 2;

/// Consecutive failed decisions before model trouble is reported.
const MODEL_FAILURES: u32 = 3;

/// Identical drift is repeated at most this often.
const DRIFT_REPEAT: Duration = Duration::from_secs(6 * 3_600);

/// Remembered entry decisions and close intents.
const REMEMBERED: usize = 64;

/// Order command kinds whose failure the operator must hear about.
const ORDER_KINDS: [&str; 3] = ["open_order", "close_order", "modify_order"];

fn signed(value: f64) -> String {
    // An empty sum is -0.0, which would otherwise print as "+-0.00".
    let value = if value == 0.0 { 0.0 } else { value };
    if value >= 0.0 {
        format!("+{value:.2}")
    } else {
        format!("−{:.2}", value.abs())
    }
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn number(payload: &Value, key: &str) -> Option<f64> {
    payload
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn side_label(raw: &str) -> &'static str {
    match raw {
        "buy" | "long" => "long",
        "sell" | "short" => "short",
        _ => "",
    }
}

/// What the autopilot decided when it queued an entry, kept until the fill.
#[derive(Debug, Clone)]
struct EntryDecision {
    command_id: String,
    symbol: String,
    side: String,
    volume: Option<f64>,
    stop: Option<f64>,
    target: Option<f64>,
    rationale: Option<String>,
}

/// Maps audit feed events to notifications. Holds just enough memory to
/// connect a fill to its decision and a close to its cause.
#[derive(Debug, Default)]
pub struct AuditMapper {
    entries: VecDeque<EntryDecision>,
    /// Ticket → why Veyra asked to close it.
    close_intents: VecDeque<(i64, &'static str)>,
    last_drift: Option<(String, u64)>,
}

impl AuditMapper {
    /// A mapper with no memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// The notification for one feed event, if it warrants one.
    pub fn map(&mut self, event: &FeedEvent) -> Option<Notification> {
        let payload = &event.payload;
        match event.kind {
            AuditKind::ProposalEvaluated => {
                self.remember_decision(payload);
                None
            }
            AuditKind::CommandCompleted => self.completed(payload),
            AuditKind::CommandFailed => {
                let kind = text(payload, "kind")?;
                ORDER_KINDS.contains(&kind).then(|| {
                    Notification::new(
                        NotifyEvent::OrderFailed,
                        Severity::Warning,
                        format!("Order failed: {}", kind.replace('_', " ")),
                        text(payload, "error").unwrap_or("The terminal reported a failure."),
                    )
                })
            }
            AuditKind::PositionClosed => Some(self.closed(payload)),
            AuditKind::ReconciliationDrift => self.drift(payload, event.at_ms),
            _ => None,
        }
    }

    fn remember_decision(&mut self, payload: &Value) {
        match text(payload, "outcome") {
            Some("queued") => {
                let (Some(command_id), Some(symbol)) =
                    (text(payload, "command_id"), text(payload, "symbol"))
                else {
                    return;
                };
                if self.entries.len() == REMEMBERED {
                    self.entries.pop_front();
                }
                self.entries.push_back(EntryDecision {
                    command_id: command_id.to_owned(),
                    symbol: symbol.to_owned(),
                    side: side_label(text(payload, "side").unwrap_or("")).to_owned(),
                    volume: number(payload, "volume"),
                    stop: number(payload, "stop_loss"),
                    target: number(payload, "take_profit"),
                    rationale: text(payload, "rationale").map(str::to_owned),
                });
            }
            Some(outcome @ ("close_queued" | "profit_harvest_close")) => {
                let Some(ticket) = payload.get("ticket").and_then(Value::as_i64) else {
                    return;
                };
                let cause = if outcome == "close_queued" {
                    "Closed by agent"
                } else {
                    "Harvest close"
                };
                if self.close_intents.len() == REMEMBERED {
                    self.close_intents.pop_front();
                }
                self.close_intents.push_back((ticket, cause));
            }
            _ => {}
        }
    }

    fn completed(&mut self, payload: &Value) -> Option<Notification> {
        let kind = text(payload, "kind")?;
        if !ORDER_KINDS.contains(&kind) {
            return None;
        }
        let result = payload.get("result")?;
        let executed = result.get("executed").and_then(Value::as_bool)?;
        if !executed {
            let retcode = result.get("retcode").and_then(Value::as_i64);
            return Some(Notification::new(
                NotifyEvent::OrderFailed,
                Severity::Warning,
                format!("Order rejected: {}", kind.replace('_', " ")),
                match retcode {
                    Some(code) => format!("The broker did not execute it (retcode {code})."),
                    None => "The broker did not execute it.".to_owned(),
                },
            ));
        }
        if kind != "open_order" {
            return None;
        }
        let ticket = result.get("ticket").and_then(Value::as_i64);
        let command_id = text(payload, "command_id").unwrap_or("");
        let entry = self
            .entries
            .iter()
            .position(|entry| entry.command_id == command_id)
            .and_then(|index| self.entries.remove(index));
        let notification = match entry {
            Some(entry) => {
                let mut title = format!("Opened {} {}", entry.symbol, entry.side);
                if let Some(volume) = entry.volume {
                    title.push_str(&format!(" {volume:.2}"));
                }
                let mut lines = Vec::new();
                match (entry.stop, entry.target) {
                    (Some(stop), Some(target)) => {
                        lines.push(format!("Stop {stop} · target {target}"))
                    }
                    (Some(stop), None) => lines.push(format!("Stop {stop}")),
                    (None, Some(target)) => lines.push(format!("Target {target}")),
                    (None, None) => {}
                }
                if let Some(rationale) = entry.rationale {
                    lines.push(rationale);
                }
                if let Some(ticket) = ticket {
                    lines.push(format!("Ticket {ticket}"));
                }
                Notification::new(
                    NotifyEvent::TradeOpened,
                    Severity::Info,
                    title.trim_end(),
                    lines.join("\n"),
                )
            }
            None => Notification::new(
                NotifyEvent::TradeOpened,
                Severity::Info,
                "Position opened",
                ticket.map_or_else(String::new, |ticket| format!("Ticket {ticket}")),
            ),
        };
        Some(notification)
    }

    fn closed(&mut self, payload: &Value) -> Notification {
        let ticket = payload.get("ticket").and_then(Value::as_i64);
        let symbol = text(payload, "symbol").unwrap_or("Position");
        let side = side_label(text(payload, "kind").unwrap_or(""));
        let profit = number(payload, "profit");
        let cause = ticket
            .and_then(|ticket| {
                self.close_intents
                    .iter()
                    .position(|(known, _)| *known == ticket)
            })
            .and_then(|index| self.close_intents.remove(index))
            .map_or("Closed at broker (stop, target or manual)", |(_, cause)| {
                cause
            });
        let mut title = format!("Closed {symbol} {side}");
        if let Some(profit) = profit {
            title = format!("{} · {}", title.trim_end(), signed(profit));
        }
        let mut lines = vec![cause.to_owned()];
        if let Some(lots) = number(payload, "lots") {
            lines.push(format!("{lots:.2} lots"));
        }
        if let Some(ticket) = ticket {
            lines.push(format!("Ticket {ticket}"));
        }
        let severity = if profit.is_some_and(|profit| profit < 0.0) {
            Severity::Warning
        } else {
            Severity::Info
        };
        Notification::new(
            NotifyEvent::TradeClosed,
            severity,
            title.trim_end(),
            lines.join(" · "),
        )
    }

    fn drift(&mut self, payload: &Value, at_ms: u64) -> Option<Notification> {
        let key = payload.to_string();
        if let Some((last, when)) = &self.last_drift
            && *last == key
            && at_ms.saturating_sub(*when) < DRIFT_REPEAT.as_millis() as u64
        {
            return None;
        }
        self.last_drift = Some((key, at_ms));
        let unknown: Vec<String> = payload
            .get("unknownTickets")
            .and_then(Value::as_array)
            .map(|tickets| tickets.iter().map(ToString::to_string).collect())
            .unwrap_or_default();
        let body = if unknown.is_empty() {
            "The broker's book no longer matches what Veyra manages.".to_owned()
        } else {
            format!(
                "Positions Veyra does not manage are open: {}.",
                unknown.join(", ")
            )
        };
        Some(Notification::new(
            NotifyEvent::ReconciliationDrift,
            Severity::Warning,
            "Reconciliation drift",
            body,
        ))
    }
}

/// One sample of the state [`HealthTracker`] watches. `None` means unknown
/// (no broker, no account yet), which never triggers a transition.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HealthSample {
    /// Whether the broker link reported within its freshness window.
    pub broker_fresh: Option<bool>,
    /// Day and peak drawdown percent, when equity is known.
    pub drawdowns: Option<(f64, f64)>,
    /// Daily-loss and peak-drawdown limits in percent (0 disables one).
    pub limits: (f64, f64),
    /// Kill switch engaged.
    pub kill_switch: bool,
    /// Service execution switch armed.
    pub trading_enabled: bool,
    /// Consecutive failed autopilot decisions and the latest reason.
    pub model_failures: (u32, Option<String>),
}

/// Remembers the last reported state of each watched condition so only
/// transitions produce notifications.
#[derive(Debug, Default)]
pub struct HealthTracker {
    started: bool,
    stale_samples: u32,
    broker_stale: bool,
    breaker: Option<&'static str>,
    halted: Option<&'static str>,
    model_trouble: bool,
}

impl HealthTracker {
    /// A tracker that treats the first sample as the baseline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Notifications for what changed since the previous sample.
    pub fn observe(&mut self, sample: &HealthSample) -> Vec<Notification> {
        let mut out = Vec::new();
        let first = !self.started;
        self.started = true;

        // Broker link: stale only after consecutive stale samples.
        match sample.broker_fresh {
            Some(false) => {
                self.stale_samples = self.stale_samples.saturating_add(1);
                if self.stale_samples >= STALE_SAMPLES && !self.broker_stale {
                    self.broker_stale = true;
                    out.push(Notification::new(
                        NotifyEvent::BrokerLink,
                        Severity::Critical,
                        "Broker link stale",
                        "The terminal has stopped reporting. Veyra cannot trade or manage positions until it reconnects.",
                    ));
                }
            }
            Some(true) => {
                self.stale_samples = 0;
                if self.broker_stale {
                    self.broker_stale = false;
                    out.push(Notification::new(
                        NotifyEvent::BrokerLink,
                        Severity::Info,
                        "Broker link recovered",
                        "The terminal is reporting again.",
                    ));
                }
            }
            None => {}
        }

        // Breakers: the same comparison the risk gate makes.
        if let Some((day, peak)) = sample.drawdowns {
            let (day_limit, peak_limit) = sample.limits;
            let tripped = if day_limit > 0.0 && day >= day_limit {
                Some("Daily loss limit reached")
            } else if peak_limit > 0.0 && peak >= peak_limit {
                Some("Peak drawdown limit reached")
            } else {
                None
            };
            match (self.breaker, tripped) {
                (None, Some(title)) => out.push(Notification::new(
                    NotifyEvent::BreakerTripped,
                    Severity::Critical,
                    title,
                    format!(
                        "Day drawdown {day:.2}% (limit {day_limit:.2}%), peak drawdown {peak:.2}% (limit {peak_limit:.2}%). New entries are blocked; open positions are still managed."
                    ),
                )),
                (Some(_), None) => out.push(Notification::new(
                    NotifyEvent::BreakerTripped,
                    Severity::Info,
                    "Breaker cleared",
                    "Drawdown is back within limits. New entries are allowed again.",
                )),
                _ => {}
            }
            self.breaker = tripped;
        }

        // Halts: the kill switch or the service execution switch.
        let halted = if sample.kill_switch {
            Some("Kill switch engaged")
        } else if !sample.trading_enabled {
            Some("Execution switched off")
        } else {
            None
        };
        if !first && halted != self.halted {
            out.push(match halted {
                Some(title) => Notification::new(
                    NotifyEvent::TradingHalted,
                    Severity::Critical,
                    title,
                    "Veyra will not open or change positions until it is re-armed.",
                ),
                None => Notification::new(
                    NotifyEvent::TradingHalted,
                    Severity::Info,
                    "Trading re-armed",
                    "Veyra may open and manage positions again.",
                ),
            });
        }
        self.halted = halted;

        // Model trouble: consecutive failed decisions.
        let (failures, reason) = &sample.model_failures;
        if *failures >= MODEL_FAILURES && !self.model_trouble {
            self.model_trouble = true;
            out.push(Notification::new(
                NotifyEvent::ModelTrouble,
                Severity::Critical,
                format!("Autopilot cannot get decisions ({failures} in a row)"),
                reason.as_deref().unwrap_or("Every model call is failing."),
            ));
        } else if *failures == 0 && self.model_trouble {
            self.model_trouble = false;
            out.push(Notification::new(
                NotifyEvent::ModelTrouble,
                Severity::Info,
                "Autopilot decisions recovered",
                "The model is answering again.",
            ));
        }
        out
    }
}

/// Decides when the daily summary is due: once per UTC date, at the first
/// sample at or after the configured hour.
#[derive(Debug, Default)]
pub struct SummaryClock {
    last_sent: Option<time::Date>,
}

impl SummaryClock {
    /// A clock that has sent nothing. The first summary goes out at the next
    /// configured hour, not immediately on startup after that hour.
    pub fn starting_at(now: OffsetDateTime, hour_utc: u8) -> Self {
        Self {
            last_sent: (now.hour() >= hour_utc).then_some(now.date()),
        }
    }

    /// Whether the summary should go out now; marks it sent when it should.
    pub fn due(&mut self, now: OffsetDateTime, hour_utc: u8) -> bool {
        if now.hour() < hour_utc || self.last_sent == Some(now.date()) {
            return false;
        }
        self.last_sent = Some(now.date());
        true
    }
}

/// The daily summary from closed-position journal rows of the last 24 hours
/// and the latest account figures.
pub fn daily_summary(profits: &[f64], account: Option<(f64, f64)>) -> Notification {
    let wins = profits.iter().filter(|profit| **profit > 0.0).count();
    let losses = profits.iter().filter(|profit| **profit < 0.0).count();
    let net: f64 = profits.iter().sum();
    let mut lines = vec![format!(
        "{} closed · {wins} won · {losses} lost · net {}",
        profits.len(),
        signed(net)
    )];
    if let Some((balance, equity)) = account {
        lines.push(format!("Balance {balance:.2} · equity {equity:.2}"));
    }
    Notification::new(
        NotifyEvent::DailySummary,
        Severity::Info,
        "Daily summary",
        lines.join("\n"),
    )
}

/// Follows the audit feed for the life of the process, queueing a
/// notification for each event [`AuditMapper`] maps.
pub async fn watch_audit(state: AppState) {
    let Some(audit) = state.audit().cloned() else {
        return;
    };
    let mut mapper = AuditMapper::new();
    // Start from now: a restart must not replay the ring as fresh news.
    let mut cursor = audit.feed_latest();
    loop {
        let events = audit.feed_after(cursor, 200, Duration::from_secs(15)).await;
        for event in &events {
            cursor = cursor.max(event.seq);
            if let Some(notification) = mapper.map(event) {
                state.notifier().notify(notification);
            }
        }
    }
}

pub(crate) async fn sample(state: &AppState) -> HealthSample {
    let policy = state.risk().policy();
    let mut sample = HealthSample {
        limits: (
            policy.max_daily_loss_percent(),
            policy.max_peak_drawdown_percent(),
        ),
        kill_switch: policy.kill_switch(),
        trading_enabled: state.trading_enabled(),
        model_failures: (
            state.decision_health().consecutive_failures(),
            state
                .decision_health()
                .last_failure()
                .map(|(reason, _)| reason),
        ),
        ..HealthSample::default()
    };
    if let Some(broker) = state.broker() {
        let report = broker.link().report().await;
        sample.broker_fresh = Some(
            report.fresh
                && report
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.connected()),
        );
        if report.fresh
            && let Some(account) = broker.link().last_account()
        {
            let drawdowns = state
                .equity_guard()
                .observe(account.equity, std::time::SystemTime::now());
            sample.drawdowns = Some((drawdowns.day_percent, drawdowns.peak_percent));
        }
    }
    sample
}

pub(crate) async fn send_summary(state: &AppState) {
    let since_ms = (OffsetDateTime::now_utc().unix_timestamp() - 86_400) * 1_000;
    let mut profits = Vec::new();
    if let Some(audit) = state.audit()
        && let Ok(query) = AuditQuery::new(&[AuditKind::PositionClosed], 200)
            .and_then(|query| query.with_window(Some(since_ms), None))
    {
        match audit.trail().query(&query).await {
            Ok(rows) => {
                profits = rows
                    .iter()
                    .filter_map(|row| number(&row.payload, "profit"))
                    .collect()
            }
            Err(error) => {
                tracing::warn!(%error, "daily summary could not read closed positions");
                return;
            }
        }
    }
    let account = state
        .broker()
        .and_then(|broker| broker.link().last_account())
        .map(|account| (account.balance, account.equity));
    state.notifier().notify(daily_summary(&profits, account));
}

/// Samples state every [`HEALTH_PERIOD`] for the life of the process,
/// queueing a notification for each transition and the daily summary.
pub async fn watch_health(state: AppState) {
    let mut tracker = HealthTracker::new();
    let mut clock = SummaryClock::starting_at(
        OffsetDateTime::now_utc(),
        state.notifier().config().prefs.summary_hour_utc,
    );
    let mut cadence = actix_web::rt::time::interval(HEALTH_PERIOD);
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        cadence.tick().await;
        for notification in tracker.observe(&sample(&state).await) {
            state.notifier().notify(notification);
        }
        let hour = state.notifier().config().prefs.summary_hour_utc;
        if clock.due(OffsetDateTime::now_utc(), hour)
            && state.notifier().wants(NotifyEvent::DailySummary)
        {
            send_summary(&state).await;
        }
    }
}

/// Starts both watchers when notifications can be delivered.
pub fn spawn(state: &AppState) {
    if !state.notifier().available() {
        return;
    }
    actix_web::rt::spawn(watch_audit(state.clone()));
    actix_web::rt::spawn(watch_health(state.clone()));
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(kind: AuditKind, payload: Value) -> FeedEvent {
        FeedEvent {
            seq: 1,
            at_ms: 1_000,
            kind,
            payload,
        }
    }

    const OPEN_ID: &str = "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10";

    #[test]
    fn a_fill_is_reported_with_the_decision_that_queued_it() {
        let mut mapper = AuditMapper::new();
        assert!(
            mapper
                .map(&event(
                    AuditKind::ProposalEvaluated,
                    json!({
                        "outcome": "queued", "origin": "autopilot", "symbol": "GBPUSD",
                        "side": "sell", "volume": 0.01, "stop_loss": 1.3265, "take_profit": 1.3162,
                        "rationale": "Aligned bearish evidence", "command_id": OPEN_ID
                    }),
                ))
                .is_none()
        );
        let opened = mapper
            .map(&event(
                AuditKind::CommandCompleted,
                json!({"kind": "open_order", "command_id": OPEN_ID, "result": {"executed": true, "retcode": 0, "ticket": 42}}),
            ))
            .expect("fill notified");
        assert_eq!(opened.event, Some(NotifyEvent::TradeOpened));
        assert_eq!(opened.title, "Opened GBPUSD short 0.01");
        assert_eq!(
            opened.body,
            "Stop 1.3265 · target 1.3162\nAligned bearish evidence\nTicket 42"
        );

        let unknown = mapper
            .map(&event(
                AuditKind::CommandCompleted,
                json!({"kind": "open_order", "command_id": OPEN_ID, "result": {"executed": true, "ticket": 43}}),
            ))
            .expect("fill without a remembered decision");
        assert_eq!(unknown.title, "Position opened");
        assert_eq!(unknown.body, "Ticket 43");
    }

    #[test]
    fn closes_carry_their_cause_and_result() {
        let mut mapper = AuditMapper::new();
        mapper.map(&event(
            AuditKind::ProposalEvaluated,
            json!({"outcome": "close_queued", "ticket": 7, "symbol": "USDJPY"}),
        ));
        let agent = mapper
            .map(&event(
                AuditKind::PositionClosed,
                json!({"ticket": 7, "symbol": "USDJPY", "kind": "buy", "lots": 0.01, "profit": -2.63}),
            ))
            .expect("close");
        assert_eq!(agent.title, "Closed USDJPY long · −2.63");
        assert_eq!(agent.body, "Closed by agent · 0.01 lots · Ticket 7");
        assert_eq!(agent.severity, Severity::Warning);

        mapper.map(&event(
            AuditKind::ProposalEvaluated,
            json!({"outcome": "profit_harvest_close", "ticket": 8}),
        ));
        let harvest = mapper
            .map(&event(
                AuditKind::PositionClosed,
                json!({"ticket": 8, "symbol": "EURUSD", "kind": "sell", "profit": 1.2}),
            ))
            .expect("close");
        assert_eq!(harvest.title, "Closed EURUSD short · +1.20");
        assert!(harvest.body.starts_with("Harvest close"));
        assert_eq!(harvest.severity, Severity::Info);

        let broker = mapper
            .map(&event(
                AuditKind::PositionClosed,
                json!({"ticket": 9, "symbol": "AUDUSD"}),
            ))
            .expect("close");
        assert_eq!(broker.title, "Closed AUDUSD");
        assert!(broker.body.starts_with("Closed at broker"));
    }

    #[test]
    fn only_order_commands_report_failures() {
        let mut mapper = AuditMapper::new();
        let failed = mapper
            .map(&event(
                AuditKind::CommandFailed,
                json!({"kind": "close_order", "error": "trade context busy"}),
            ))
            .expect("order failure");
        assert_eq!(failed.event, Some(NotifyEvent::OrderFailed));
        assert_eq!(failed.title, "Order failed: close order");
        assert_eq!(failed.body, "trade context busy");

        assert!(
            mapper
                .map(&event(
                    AuditKind::CommandFailed,
                    json!({"kind": "rates", "error": "x"})
                ))
                .is_none(),
            "data commands are not orders"
        );
        let rejected = mapper
            .map(&event(
                AuditKind::CommandCompleted,
                json!({"kind": "modify_order", "result": {"executed": false, "retcode": 130}}),
            ))
            .expect("rejection");
        assert_eq!(rejected.title, "Order rejected: modify order");
        assert!(rejected.body.contains("retcode 130"));
        assert!(
            mapper
                .map(&event(
                    AuditKind::CommandCompleted,
                    json!({"kind": "close_order", "result": {"executed": true}}),
                ))
                .is_none(),
            "an executed close is reported by position_closed"
        );
        assert!(
            mapper
                .map(&event(
                    AuditKind::CommandCompleted,
                    json!({"kind": "account_snapshot", "result": {}})
                ))
                .is_none()
        );
    }

    #[test]
    fn identical_drift_is_not_repeated_within_the_window() {
        let mut mapper = AuditMapper::new();
        let drift = json!({"orders": 3, "unknownTickets": [11, 12], "positionsTruncated": false});
        let first = mapper
            .map(&event(AuditKind::ReconciliationDrift, drift.clone()))
            .expect("first drift");
        assert_eq!(
            first.body,
            "Positions Veyra does not manage are open: 11, 12."
        );
        assert!(
            mapper
                .map(&event(AuditKind::ReconciliationDrift, drift.clone()))
                .is_none()
        );
        let mut later = event(AuditKind::ReconciliationDrift, drift);
        later.at_ms += DRIFT_REPEAT.as_millis() as u64;
        assert!(mapper.map(&later).is_some(), "repeated after the window");
        let changed = event(
            AuditKind::ReconciliationDrift,
            json!({"orders": 1, "unknownTickets": [], "positionsTruncated": true}),
        );
        let changed = mapper.map(&changed).expect("changed drift");
        assert!(changed.body.starts_with("The broker's book"));
    }

    fn healthy() -> HealthSample {
        HealthSample {
            broker_fresh: Some(true),
            drawdowns: Some((0.5, 1.0)),
            limits: (3.0, 10.0),
            kill_switch: false,
            trading_enabled: true,
            model_failures: (0, None),
        }
    }

    fn titles(notifications: &[Notification]) -> Vec<&str> {
        notifications
            .iter()
            .map(|notification| notification.title.as_str())
            .collect()
    }

    #[test]
    fn the_broker_link_is_stale_only_after_consecutive_samples() {
        let mut tracker = HealthTracker::new();
        assert!(tracker.observe(&healthy()).is_empty());
        let stale = HealthSample {
            broker_fresh: Some(false),
            ..healthy()
        };
        assert!(tracker.observe(&stale).is_empty(), "one blip is ignored");
        assert_eq!(titles(&tracker.observe(&stale)), vec!["Broker link stale"]);
        assert!(tracker.observe(&stale).is_empty(), "reported once");
        assert_eq!(
            titles(&tracker.observe(&healthy())),
            vec!["Broker link recovered"]
        );
        let unknown = HealthSample {
            broker_fresh: None,
            ..healthy()
        };
        assert!(tracker.observe(&unknown).is_empty());
    }

    #[test]
    fn breakers_report_when_they_start_and_stop_blocking() {
        let mut tracker = HealthTracker::new();
        let day = HealthSample {
            drawdowns: Some((3.2, 4.0)),
            ..healthy()
        };
        let tripped = tracker.observe(&day);
        assert_eq!(titles(&tripped), vec!["Daily loss limit reached"]);
        assert!(tripped[0].body.contains("Day drawdown 3.20% (limit 3.00%)"));
        assert!(tracker.observe(&day).is_empty());
        assert_eq!(
            titles(&tracker.observe(&healthy())),
            vec!["Breaker cleared"]
        );

        let peak = HealthSample {
            drawdowns: Some((0.0, 12.0)),
            ..healthy()
        };
        assert_eq!(
            titles(&tracker.observe(&peak)),
            vec!["Peak drawdown limit reached"]
        );
        let disabled = HealthSample {
            drawdowns: Some((50.0, 50.0)),
            limits: (0.0, 0.0),
            ..healthy()
        };
        assert_eq!(
            titles(&tracker.observe(&disabled)),
            vec!["Breaker cleared"],
            "a limit of 0 never trips"
        );
    }

    #[test]
    fn halts_report_changes_but_not_the_startup_state() {
        let mut tracker = HealthTracker::new();
        let killed = HealthSample {
            kill_switch: true,
            ..healthy()
        };
        assert!(
            tracker.observe(&killed).is_empty(),
            "startup is the baseline"
        );
        assert_eq!(
            titles(&tracker.observe(&healthy())),
            vec!["Trading re-armed"]
        );
        let off = HealthSample {
            trading_enabled: false,
            ..healthy()
        };
        assert_eq!(
            titles(&tracker.observe(&off)),
            vec!["Execution switched off"]
        );
        assert_eq!(
            titles(&tracker.observe(&killed)),
            vec!["Kill switch engaged"]
        );
    }

    #[test]
    fn model_trouble_reports_after_repeated_failures_and_recovery() {
        let mut tracker = HealthTracker::new();
        let failing = |count| HealthSample {
            model_failures: (count, Some("model unavailable: 404".to_owned())),
            ..healthy()
        };
        assert!(tracker.observe(&failing(2)).is_empty());
        let trouble = tracker.observe(&failing(3));
        assert_eq!(
            titles(&trouble),
            vec!["Autopilot cannot get decisions (3 in a row)"]
        );
        assert_eq!(trouble[0].body, "model unavailable: 404");
        assert!(tracker.observe(&failing(4)).is_empty());
        assert_eq!(
            titles(&tracker.observe(&healthy())),
            vec!["Autopilot decisions recovered"]
        );
    }

    #[test]
    fn the_summary_goes_out_once_a_day_at_the_hour() {
        let at = |text: &str| {
            OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                .expect("time")
        };
        let mut late_start = SummaryClock::starting_at(at("2026-09-25T19:00:00Z"), 18);
        assert!(
            !late_start.due(at("2026-09-25T19:30:00Z"), 18),
            "not on a late startup"
        );
        assert!(!late_start.due(at("2026-09-26T17:59:00Z"), 18));
        assert!(late_start.due(at("2026-09-26T18:00:30Z"), 18));
        assert!(
            !late_start.due(at("2026-09-26T21:00:00Z"), 18),
            "once per day"
        );

        let mut early_start = SummaryClock::starting_at(at("2026-09-25T06:00:00Z"), 18);
        assert!(early_start.due(at("2026-09-25T18:00:00Z"), 18));
    }

    #[test]
    fn the_summary_counts_results_and_quotes_the_account() {
        let summary = daily_summary(&[1.2, -2.63, 0.5, 0.0], Some((1234.5, 1230.25)));
        assert_eq!(summary.event, Some(NotifyEvent::DailySummary));
        assert_eq!(
            summary.body,
            "4 closed · 2 won · 1 lost · net −0.93\nBalance 1234.50 · equity 1230.25"
        );
        assert_eq!(
            daily_summary(&[], None).body,
            "0 closed · 0 won · 0 lost · net +0.00"
        );
    }
}
