//! Audit-trail boundary.
//!
//! Durable history (commands, acknowledgements, broker snapshots,
//! reconciliation results) lives behind [`AuditTrail`], so storage swaps
//! without touching callers: the service ships a PostgreSQL implementation and
//! tests use an in-memory one. Writes are best-effort by design —
//! [`AuditRuntime::try_record`] logs failures instead of blocking a command —
//! while a configured-but-unusable database fails startup, so a missing trail
//! is never mistaken for an empty one.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

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
    /// The service process started with auditing enabled.
    ServiceStarted,
}

impl AuditKind {
    /// Stable wire and column name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CommandQueued => "command_queued",
            Self::CommandCompleted => "command_completed",
            Self::CommandFailed => "command_failed",
            Self::BrokerSnapshot => "broker_snapshot",
            Self::ServiceStarted => "service_started",
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
}

/// Active audit integration.
#[derive(Debug, Clone)]
pub struct AuditRuntime {
    trail: Arc<dyn AuditTrail>,
}

impl AuditRuntime {
    /// Wraps a storage implementation.
    pub fn new(trail: Arc<dyn AuditTrail>) -> Self {
        Self { trail }
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
    /// an audit hiccup can not block or fail a command.
    pub async fn try_record(&self, event: AuditEvent) {
        if let Err(error) = self.trail.record(event).await {
            tracing::warn!(%error, "audit write failed");
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    }

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(AuditKind::CommandQueued.as_str(), "command_queued");
        assert_eq!(AuditKind::CommandCompleted.as_str(), "command_completed");
        assert_eq!(AuditKind::CommandFailed.as_str(), "command_failed");
        assert_eq!(AuditKind::BrokerSnapshot.as_str(), "broker_snapshot");
        assert_eq!(AuditKind::ServiceStarted.as_str(), "service_started");
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
    }
}
