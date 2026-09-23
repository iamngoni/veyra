//! Audit-trail boundary.
//!
//! Durable history (commands, acknowledgements, broker snapshots,
//! reconciliation results) lives behind [`AuditTrail`], so storage swaps
//! without touching callers: the service ships a PostgreSQL implementation and
//! tests use an in-memory one. Writes are best-effort by design —
//! [`AuditRuntime::try_record`] logs failures instead of blocking a command —
//! while a configured-but-unusable database fails startup, so a missing trail
//! is never mistaken for an empty one.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;

use crate::balance::BalancePoint;

/// Event categories written to the trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditKind {
    /// A command was queued for the terminal.
    CommandQueued,
    /// A command completed with a validated payload.
    CommandCompleted,
    /// A command failed, timed out, or was rejected by the terminal.
    CommandFailed,
    /// A validated broker snapshot was retained.
    BrokerSnapshot,
    /// A validated, broker-reported account balance was observed.
    BalanceObserved,
    /// The service process started with auditing enabled.
    ServiceStarted,
    /// Reconciliation found orders Veyra does not own.
    ReconciliationDrift,
    /// The autonomous loop evaluated one proposal.
    ProposalEvaluated,
    /// A Veyra-managed position disappeared from the book (closed at the venue).
    PositionClosed,
    /// The decision loop executed one read-only tool for the model.
    AgentToolCalled,
    /// One model turn: exactly what it was shown, and what it answered.
    AgentTurn,
    /// A failure worth surviving a restart — a panic, or an error that would
    /// otherwise exist only in the in-memory log ring.
    Failure,
    /// The live risk policy was replaced from the control surface.
    RiskPolicyUpdated,
    /// One or more live settings were changed from the control surface.
    RuntimeConfigUpdated,
}

impl AuditKind {
    /// Stable wire and column name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CommandQueued => "command_queued",
            Self::CommandCompleted => "command_completed",
            Self::CommandFailed => "command_failed",
            Self::BrokerSnapshot => "broker_snapshot",
            Self::BalanceObserved => "balance_observed",
            Self::ServiceStarted => "service_started",
            Self::ReconciliationDrift => "reconciliation_drift",
            Self::ProposalEvaluated => "proposal_evaluated",
            Self::PositionClosed => "position_closed",
            Self::AgentToolCalled => "agent_tool_called",
            Self::AgentTurn => "agent_turn",
            Self::Failure => "failure",
            Self::RiskPolicyUpdated => "risk_policy_updated",
            Self::RuntimeConfigUpdated => "runtime_config_updated",
        }
    }
}

/// One append-only audit entry.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    kind: AuditKind,
    payload: Value,
}

impl AuditEvent {
    /// Builds an event from a category and a bounded JSON payload.
    pub fn new(kind: AuditKind, payload: Value) -> Self {
        Self { kind, payload }
    }

    /// Event category.
    pub fn kind(&self) -> AuditKind {
        self.kind
    }

    /// Event payload.
    pub fn payload(&self) -> &Value {
        &self.payload
    }
}

/// One stored audit row as returned to operators.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    /// Row identity.
    pub id: String,
    /// Database timestamp, formatted by PostgreSQL.
    pub at: String,
    /// Event category.
    pub kind: String,
    /// Event payload.
    pub payload: Value,
}

/// Supported audit storage implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditProvider {
    /// PostgreSQL via SQLx.
    Postgres,
}

impl AuditProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
        }
    }
}

impl fmt::Display for AuditProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Errors raised while reading or writing the audit trail.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// The store could not be built or reached.
    #[error("audit storage failed: {reason}")]
    Storage {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Narrow contract every audit storage implements.
#[async_trait]
pub trait AuditTrail: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output.
    fn provider(&self) -> AuditProvider;

    /// Appends one event.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn record(&self, event: AuditEvent) -> Result<(), AuditError>;

    /// Returns the newest events, newest first.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn recent(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError>;

    /// Reads actual balance observations for exactly one account, oldest first.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn balance_history(
        &self,
        _login: u64,
        _server: &str,
        _since_ms: u64,
    ) -> Result<Vec<BalancePoint>, AuditError> {
        Err(AuditError::Storage {
            reason: "balance history is unavailable".to_owned(),
        })
    }

    /// Deletes events older than `keep_days`, returning how many rows went.
    /// Zero keeps everything.
    ///
    /// # Errors
    /// Returns [`AuditError`] when the store is unavailable.
    async fn prune(&self, keep_days: u32) -> Result<u64, AuditError>;
}

/// Bounded number of recent events retained in memory for live feeds.
const FEED_CAPACITY: usize = 512;

/// How often a long-poll feed checks the buffer for new events.
const FEED_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One event as seen by live feeds: a monotonic sequence number plus the
/// event contents. Sequence numbers start at 1 and survive buffer eviction as
/// a cursor; a client older than the buffer is simply handed the tail.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedEvent {
    /// Monotonic sequence number; `after` cursors compare against this.
    pub seq: u64,
    /// Unix milliseconds at record time.
    pub at_ms: u64,
    /// Event category.
    pub kind: AuditKind,
    /// Bounded JSON payload.
    pub payload: Value,
}

/// In-memory ring of the most recent events, shared by every feed reader.
#[derive(Debug)]
struct EventFeed {
    events: Mutex<VecDeque<FeedEvent>>,
    next_seq: AtomicU64,
}

impl EventFeed {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::with_capacity(FEED_CAPACITY)),
            next_seq: AtomicU64::new(1),
        }
    }

    fn publish(&self, kind: AuditKind, payload: &Value) {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        if let Ok(mut events) = self.events.lock() {
            events.push_back(FeedEvent {
                seq,
                at_ms,
                kind,
                payload: payload.clone(),
            });
            while events.len() > FEED_CAPACITY {
                events.pop_front();
            }
        }
    }

    /// Events strictly after `after`, oldest first, at most `limit`.
    fn after(&self, after: u64, limit: usize) -> Vec<FeedEvent> {
        self.events
            .lock()
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.seq > after)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Sequence number of the most recent event (zero before any).
    fn latest(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst).saturating_sub(1)
    }
}

/// Process-lifetime counters derived from the audit stream.
///
/// Cheap tallies for the operations surface: every recorded event bumps one
/// total, proposals also bump their outcome, and command events bump their
/// command kind. Counters reset with the process; the durable trail remains
/// the source of truth.
#[derive(Debug, Default)]
struct EventCounters {
    counts: Mutex<std::collections::BTreeMap<String, u64>>,
}

impl EventCounters {
    fn increment(&self, key: &str) {
        let mut counts = match self.counts.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *counts.entry(key.to_owned()).or_insert(0) += 1;
    }

    fn snapshot(&self) -> std::collections::BTreeMap<String, u64> {
        let counts = match self.counts.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        counts.clone()
    }

    fn tally(&self, kind: AuditKind, payload: &Value) {
        self.increment(&format!("event.{}", kind.as_str()));
        if kind == AuditKind::ProposalEvaluated
            && let Some(outcome) = payload.get("outcome").and_then(Value::as_str)
        {
            self.increment(&format!("proposal.{outcome}"));
        }
        if matches!(
            kind,
            AuditKind::CommandQueued | AuditKind::CommandCompleted | AuditKind::CommandFailed
        ) && let Some(command) = payload.get("kind").and_then(Value::as_str)
        {
            self.increment(&format!("command.{}.{}", kind.as_str(), command));
        }
    }
}

/// Active audit integration.
#[derive(Debug, Clone)]
pub struct AuditRuntime {
    trail: Arc<dyn AuditTrail>,
    feed: Arc<EventFeed>,
    counters: Arc<EventCounters>,
}

impl AuditRuntime {
    /// Wraps a storage implementation.
    pub fn new(trail: Arc<dyn AuditTrail>) -> Self {
        Self {
            trail,
            feed: Arc::new(EventFeed::new()),
            counters: Arc::new(EventCounters::default()),
        }
    }

    /// Counters since process start, keyed by `event.*`, `proposal.*`, and
    /// `command.*`.
    pub fn counters(&self) -> std::collections::BTreeMap<String, u64> {
        self.counters.snapshot()
    }

    /// Sequence number of the most recent recorded event (zero before any).
    pub fn feed_latest(&self) -> u64 {
        self.feed.latest()
    }

    /// Returns events after `after`, waiting up to `wait` for the first one
    /// when none is buffered yet. Long-poll friendly: the caller decides the
    /// window and always gets an answer.
    pub async fn feed_after(&self, after: u64, limit: usize, wait: Duration) -> Vec<FeedEvent> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            let events = self.feed.after(after, limit);
            if !events.is_empty() {
                return events;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Vec::new();
            }
            actix_web::rt::time::sleep(FEED_POLL_INTERVAL.min(remaining)).await;
        }
    }

    /// Provider identifier of the active implementation.
    pub fn provider(&self) -> AuditProvider {
        self.trail.provider()
    }

    /// Storage implementation behind this runtime.
    pub fn trail(&self) -> &Arc<dyn AuditTrail> {
        &self.trail
    }

    /// Appends an event best-effort: failures are logged, never propagated, so
    /// an audit hiccup can not block or fail a command. The event also enters
    /// the in-memory feed for live readers, whether or not storage accepts it.
    pub async fn try_record(&self, event: AuditEvent) {
        self.feed.publish(event.kind(), event.payload());
        self.counters.tally(event.kind(), event.payload());
        if let Err(error) = self.trail.record(event).await {
            tracing::warn!(%error, "audit write failed");
        }
    }

    /// Prunes the trail best-effort: failures are logged and reported as zero
    /// rows removed, so a storage hiccup never fails the maintenance loop.
    pub async fn try_prune(&self, keep_days: u32) -> u64 {
        match self.trail.prune(keep_days).await {
            Ok(deleted) => deleted,
            Err(error) => {
                tracing::warn!(%error, "audit prune failed");
                0
            }
        }
    }
}

/// In-memory audit trail.
///
/// Useful for tests and local experiments only: nothing in the production path
/// selects it, so a missing database is never mistaken for a durable one.
#[derive(Debug, Default)]
pub struct MemoryTrail {
    events: std::sync::Mutex<Vec<AuditEvent>>,
}

impl MemoryTrail {
    /// Number of retained events.
    pub fn len(&self) -> usize {
        self.events.lock().map(|events| events.len()).unwrap_or(0)
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns every recorded event, oldest first.
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl AuditTrail for MemoryTrail {
    fn provider(&self) -> AuditProvider {
        AuditProvider::Postgres
    }

    async fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        let mut events = match self.events.lock() {
            Ok(events) => events,
            Err(poisoned) => poisoned.into_inner(),
        };
        events.push(event);
        Ok(())
    }

    async fn prune(&self, _keep_days: u32) -> Result<u64, AuditError> {
        // The in-memory trail never prunes: tests assert on what was written.
        Ok(0)
    }

    async fn recent(&self, limit: u32) -> Result<Vec<AuditRow>, AuditError> {
        let events = match self.events.lock() {
            Ok(events) => events,
            Err(poisoned) => poisoned.into_inner(),
        };
        Ok(events
            .iter()
            .rev()
            .take(limit as usize)
            .map(|event| AuditRow {
                id: "row".to_owned(),
                at: "now".to_owned(),
                kind: event.kind().as_str().to_owned(),
                payload: event.payload().clone(),
            })
            .collect())
    }

    async fn balance_history(
        &self,
        login: u64,
        server: &str,
        since_ms: u64,
    ) -> Result<Vec<BalancePoint>, AuditError> {
        let events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut points: Vec<_> = events
            .iter()
            .filter(|event| event.kind() == AuditKind::BalanceObserved)
            .filter(|event| event.payload()["login"].as_u64() == Some(login))
            .filter(|event| event.payload()["server"].as_str() == Some(server))
            .filter_map(|event| {
                let at_ms = event.payload()["atMs"].as_u64()?;
                let balance = event.payload()["balance"].as_f64()?;
                (at_ms >= since_ms && balance.is_finite())
                    .then_some(BalancePoint { at_ms, balance })
            })
            .collect();
        points.sort_by_key(|point| point.at_ms);
        Ok(points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[actix_web::test]
    async fn balance_history_is_account_scoped_chronological_and_preserves_deficits() {
        let trail = MemoryTrail::default();
        for (login, at_ms, balance) in [
            (10, 300, -2.5),
            (11, 200, 500.0),
            (10, 100, 4.0),
            (10, 200, 0.0),
        ] {
            trail
                .record(AuditEvent::new(
                    AuditKind::BalanceObserved,
                    json!({"login": login, "server": "Broker-Real", "atMs": at_ms, "balance": balance}),
                ))
                .await
                .expect("record");
        }
        let points = trail
            .balance_history(10, "Broker-Real", 0)
            .await
            .expect("history");
        assert_eq!(
            points.iter().map(|point| point.at_ms).collect::<Vec<_>>(),
            vec![100, 200, 300]
        );
        assert_eq!(
            points.iter().map(|point| point.balance).collect::<Vec<_>>(),
            vec![4.0, 0.0, -2.5]
        );
        assert_eq!(
            trail.balance_history(10, "Other", 0).await.expect("other"),
            Vec::new()
        );
        assert_eq!(
            trail
                .balance_history(10, "Broker-Real", 201)
                .await
                .expect("window")
                .len(),
            1
        );
    }

    /// Storage that always fails, to prove writes never propagate.
    #[derive(Debug)]
    struct BrokenTrail;

    #[async_trait]
    impl AuditTrail for BrokenTrail {
        fn provider(&self) -> AuditProvider {
            AuditProvider::Postgres
        }

        async fn record(&self, _event: AuditEvent) -> Result<(), AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }

        async fn recent(&self, _limit: u32) -> Result<Vec<AuditRow>, AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }

        async fn prune(&self, _keep_days: u32) -> Result<u64, AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }
    }

    #[actix_web::test]
    async fn counters_tally_events_outcomes_and_commands() {
        use serde_json::json;

        let runtime = AuditRuntime::new(Arc::new(MemoryTrail::default()));
        assert!(runtime.counters().is_empty(), "nothing recorded yet");

        runtime
            .try_record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "held"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "held"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::ProposalEvaluated,
                json!({"outcome": "queued"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::CommandQueued,
                json!({"kind": "open_order"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::CommandCompleted,
                json!({"kind": "open_order"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(AuditKind::BrokerSnapshot, json!({})))
            .await;

        let counters = runtime.counters();
        assert_eq!(counters["event.proposal_evaluated"], 3);
        assert_eq!(counters["proposal.held"], 2);
        assert_eq!(counters["proposal.queued"], 1);
        assert_eq!(counters["event.command_queued"], 1);
        assert_eq!(counters["command.command_queued.open_order"], 1);
        assert_eq!(counters["command.command_completed.open_order"], 1);
        assert_eq!(counters["event.broker_snapshot"], 1);
    }

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(AuditKind::CommandQueued.as_str(), "command_queued");
        assert_eq!(AuditKind::CommandCompleted.as_str(), "command_completed");
        assert_eq!(AuditKind::CommandFailed.as_str(), "command_failed");
        assert_eq!(AuditKind::BrokerSnapshot.as_str(), "broker_snapshot");
        assert_eq!(AuditKind::ServiceStarted.as_str(), "service_started");
        assert_eq!(
            AuditKind::ReconciliationDrift.as_str(),
            "reconciliation_drift"
        );
        assert_eq!(AuditKind::ProposalEvaluated.as_str(), "proposal_evaluated");
        assert_eq!(AuditKind::PositionClosed.as_str(), "position_closed");
        assert_eq!(AuditProvider::Postgres.as_str(), "postgres");
        assert_eq!(AuditProvider::Postgres.to_string(), "postgres");
    }

    #[actix_web::test]
    async fn runtime_records_and_reads_through_the_trail() {
        let trail = Arc::new(MemoryTrail::default());
        let runtime = AuditRuntime::new(trail.clone());
        assert_eq!(runtime.provider(), AuditProvider::Postgres);

        runtime
            .try_record(AuditEvent::new(
                AuditKind::CommandQueued,
                json!({"command_id": "abc"}),
            ))
            .await;
        runtime
            .try_record(AuditEvent::new(
                AuditKind::BrokerSnapshot,
                json!({"orders": 0}),
            ))
            .await;

        assert_eq!(trail.len(), 2);
        assert_eq!(trail.events().len(), 2);
        assert!(!trail.is_empty());

        let rows = runtime.trail().recent(10).await.expect("readable");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "broker_snapshot");
        assert_eq!(rows[1].payload["command_id"], "abc");
    }

    #[actix_web::test]
    async fn failing_storage_never_propagates() {
        let runtime = AuditRuntime::new(Arc::new(BrokenTrail));
        runtime
            .try_record(AuditEvent::new(AuditKind::CommandFailed, json!({})))
            .await;
        assert!(runtime.trail().recent(1).await.is_err());
        assert_eq!(runtime.try_prune(30).await, 0, "failures report zero rows");
    }

    #[actix_web::test]
    async fn pruning_is_best_effort_and_keeps_memory_trails_intact() {
        let trail = Arc::new(MemoryTrail::default());
        let runtime = AuditRuntime::new(trail.clone());
        runtime
            .try_record(AuditEvent::new(AuditKind::ServiceStarted, json!({})))
            .await;
        assert_eq!(runtime.try_prune(30).await, 0);
        assert_eq!(trail.len(), 1);
    }
}
