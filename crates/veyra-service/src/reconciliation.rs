//! Broker-state reconciliation and the periodic refresh that feeds it.
//!
//! Assessment is a pure function of the retained `account_snapshot` payload:
//! every open order is classified as Veyra-managed (it carries the Veyra magic
//! number) or unknown, so an operator can see drift without trusting the model
//! or the risk gate. The refresh helper only queues work while the EA channel
//! is fresh and nothing of that kind is already pending, so a terminal that is
//! down cannot accumulate stale commands.

use crate::AppState;
use crate::audit::{AuditEvent, AuditKind};
use crate::broker::ea::{AccountSnapshotPayload, CommandKind, ORDER_MAGIC, PositionPayload};

/// One open order classified by ownership.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionAssessment {
    /// Venue ticket.
    pub ticket: i64,
    /// Instrument.
    pub symbol: String,
    /// Magic number reported by the terminal.
    pub magic: u32,
    /// Whether the order carries the Veyra magic number.
    pub managed: bool,
    /// Volume in lots.
    pub lots: f64,
}

/// Result of assessing one broker snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconciliationReport {
    /// Every reported open order, classified.
    pub positions: Vec<PositionAssessment>,
    /// Tickets Veyra does not own.
    pub unknown_tickets: Vec<i64>,
    /// Total open volume reported by the terminal.
    pub lots: f64,
    /// Whether the terminal omitted orders beyond its own cap.
    pub positions_truncated: bool,
}

impl ReconciliationReport {
    /// Whether every reported order is Veyra-managed and none was omitted.
    pub fn is_reconciled(&self) -> bool {
        self.unknown_tickets.is_empty() && !self.positions_truncated
    }
}

/// Classifies every open order in a snapshot.
pub fn assess(snapshot: &AccountSnapshotPayload) -> ReconciliationReport {
    let mut positions = Vec::with_capacity(snapshot.positions.len());
    let mut unknown_tickets = Vec::new();
    for position in &snapshot.positions {
        let managed = position.magic == ORDER_MAGIC;
        if !managed {
            unknown_tickets.push(position.ticket);
        }
        positions.push(classify(position, managed));
    }
    ReconciliationReport {
        positions,
        unknown_tickets,
        lots: snapshot.lots,
        positions_truncated: snapshot.positions_truncated,
    }
}

/// Drift summary for auditing, when a snapshot shows orders Veyra does not
/// own or a truncated list. `None` means the book reconciles cleanly.
pub fn drift_summary(snapshot: &AccountSnapshotPayload) -> Option<serde_json::Value> {
    let report = assess(snapshot);
    if report.is_reconciled() {
        return None;
    }
    Some(serde_json::json!({
        "orders": snapshot.orders,
        "unknownTickets": report.unknown_tickets,
        "positionsTruncated": report.positions_truncated
    }))
}

fn classify(position: &PositionPayload, managed: bool) -> PositionAssessment {
    PositionAssessment {
        ticket: position.ticket,
        symbol: position.symbol.clone(),
        magic: position.magic,
        managed,
        lots: position.lots,
    }
}

/// Pure decision for the periodic refresh: the channel must be fresh and no
/// snapshot of the same kind may already be waiting.
pub fn should_refresh(fresh: bool, has_pending: bool) -> bool {
    fresh && !has_pending
}

/// Queues one `account_snapshot` when the channel is fresh and no snapshot is
/// already pending. Returns whether a command was queued.
pub async fn refresh_once(state: &AppState) -> bool {
    let Some(runtime) = state.broker() else {
        return false;
    };
    let Some(link) = runtime.ea_link() else {
        return false;
    };
    let fresh = runtime.link().report().await.fresh;
    if !should_refresh(fresh, link.has_pending(CommandKind::AccountSnapshot)) {
        return false;
    }
    let command = link.enqueue(CommandKind::AccountSnapshot);
    if let Some(audit) = state.audit() {
        audit
            .try_record(AuditEvent::new(
                AuditKind::CommandQueued,
                serde_json::json!({
                    "command_id": command.to_string(),
                    "kind": "account_snapshot",
                    "origin": "periodic_refresh"
                }),
            ))
            .await;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::ea::{PositionKind, PositionPayload};

    fn position(ticket: i64, magic: u32) -> PositionPayload {
        PositionPayload {
            ticket,
            symbol: "EURUSD".to_owned(),
            kind: PositionKind::Buy,
            lots: 0.01,
            price: 1.095,
            profit: -0.25,
            stop_loss: 1.085,
            take_profit: 1.105,
            magic,
        }
    }

    fn snapshot(positions: Vec<PositionPayload>, truncated: bool) -> AccountSnapshotPayload {
        AccountSnapshotPayload {
            balance: 20.57,
            equity: 20.57,
            free_margin: 20.57,
            orders: positions.len() as u32,
            lots: positions.iter().map(|position| position.lots).sum(),
            positions,
            positions_truncated: truncated,
            server_time: 1_758_000_000,
        }
    }

    #[test]
    fn an_empty_book_is_reconciled() {
        let report = assess(&snapshot(Vec::new(), false));
        assert!(report.is_reconciled());
        assert!(report.positions.is_empty());
        assert!(report.unknown_tickets.is_empty());
        assert_eq!(report.lots, 0.0);
    }

    #[test]
    fn veyra_orders_are_managed_and_foreign_orders_are_flagged() {
        let report = assess(&snapshot(
            vec![
                position(123, ORDER_MAGIC),
                position(456, 0),
                position(789, ORDER_MAGIC),
            ],
            false,
        ));
        assert!(!report.is_reconciled());
        assert_eq!(report.unknown_tickets, vec![456]);
        assert_eq!(report.positions.len(), 3);
        assert!(report.positions[0].managed);
        assert!(!report.positions[1].managed);
        assert_eq!(report.positions[1].magic, 0);
        assert!(report.positions[2].managed);
        assert_eq!(report.lots, 0.03);
    }

    #[test]
    fn truncation_is_drift_even_without_unknown_tickets() {
        let report = assess(&snapshot(vec![position(123, ORDER_MAGIC)], true));
        assert!(report.unknown_tickets.is_empty());
        assert!(!report.is_reconciled());
        assert!(report.positions_truncated);
    }

    #[test]
    fn drift_summaries_are_audited_only_when_the_book_disagrees() {
        assert!(drift_summary(&snapshot(Vec::new(), false)).is_none());
        assert!(drift_summary(&snapshot(vec![position(1, ORDER_MAGIC)], false)).is_none());

        let foreign = drift_summary(&snapshot(vec![position(456, 0)], false)).expect("drift");
        assert_eq!(foreign["unknownTickets"], serde_json::json!([456]));

        let truncated =
            drift_summary(&snapshot(vec![position(1, ORDER_MAGIC)], true)).expect("drift");
        assert_eq!(truncated["positionsTruncated"], true);
    }

    #[test]
    fn the_refresh_gate_requires_a_fresh_idle_channel() {
        assert!(should_refresh(true, false));
        assert!(!should_refresh(true, true), "no duplicate snapshots");
        assert!(!should_refresh(false, false), "stale channels are skipped");
        assert!(!should_refresh(false, true));
    }

    #[actix_web::test]
    async fn refresh_queues_only_when_fresh_and_idle() {
        use crate::broker::{
            AccountLogin, AccountSnapshot, BrokerRuntime, BrokerSettings, ServerName, Symbol,
        };
        use crate::config::{ConfigError, ServiceConfig};
        use crate::risk::{RiskGate, RiskPolicy};

        let gate = || RiskGate::new(RiskPolicy::default());
        let config = || {
            ServiceConfig::from_source(|name| match name {
                "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
                "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
                "VEYRA_ENV" => Ok("development".to_owned()),
                _ => Err(ConfigError::MissingEnvironmentVariable { name }),
            })
            .expect("config must parse")
        };

        assert!(
            !refresh_once(&AppState::new(config(), None, None, gate())).await,
            "no broker means no refresh"
        );

        let settings = BrokerSettings::from_source(|name| match name {
            "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
            "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings must parse")
        .expect("broker must be configured");
        let runtime = BrokerRuntime::from_settings(settings).expect("runtime builds");
        let link = runtime.ea_link().expect("ea link");
        link.record(AccountSnapshot::new(
            AccountLogin::parse(94168).expect("login"),
            ServerName::parse("IFCMarkets-Real").expect("server"),
            Symbol::parse("EURUSD").expect("symbol"),
            true,
            true,
            0,
            0.0,
        ));

        let state = AppState::new(config(), Some(runtime), None, gate());
        assert!(
            refresh_once(&state).await,
            "fresh channel queues a snapshot"
        );
        assert!(
            !refresh_once(&state).await,
            "a pending snapshot is not duplicated"
        );

        // With a trail attached, the periodic refresh records its own queueing.
        use std::sync::Arc;

        use crate::audit::{AuditKind, AuditRuntime, MemoryTrail};

        let trail = Arc::new(MemoryTrail::default());
        let settings = BrokerSettings::from_source(|name| match name {
            "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
            "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings must parse")
        .expect("broker must be configured");
        let runtime = BrokerRuntime::from_settings(settings).expect("runtime builds");
        runtime
            .ea_link()
            .expect("ea link")
            .record(AccountSnapshot::new(
                AccountLogin::parse(94168).expect("login"),
                ServerName::parse("IFCMarkets-Real").expect("server"),
                Symbol::parse("EURUSD").expect("symbol"),
                true,
                true,
                0,
                0.0,
            ));
        let audited = AppState::new(config(), Some(runtime), None, gate())
            .with_audit(Some(AuditRuntime::new(trail.clone())));
        assert!(refresh_once(&audited).await);
        let events = trail.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind(), AuditKind::CommandQueued);
        assert_eq!(events[0].payload()["origin"], "periodic_refresh");
    }
}
