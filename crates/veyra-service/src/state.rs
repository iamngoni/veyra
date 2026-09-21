//! Durable runtime state boundary.
//!
//! Counters and baselines that must survive restarts — judge usage, the model
//! call budget, drawdown baselines, stop/profit-policy memory, and the live
//! risk policy — are stored as one JSON row per key. The service snapshots them on
//! a cadence (and the policy immediately on every accepted edit), then loads
//! them before it starts serving, so a restart resumes rather than resets.
//!
//! Writes are best-effort by design: storage trouble logs a warning and the
//! trading path continues, because a lost counter must never stop the bot.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

/// Stable keys under which runtime state is stored. Additions are backwards
/// compatible; unknown rows are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKey {
    /// Cumulative judge calls, failures, and provider-reported tokens.
    JevUsage,
    /// Model call-budget window anchors and counts.
    ModelBudget,
    /// Equity drawdown baselines (day open and peak).
    EquityBaselines,
    /// Stop-policy entry-risk memory, keyed by ticket.
    StopBasis,
    /// Profit high-water marks plus post-close symbol cooldowns.
    ProfitHarvest,
    /// The effective risk policy as an apply-able snapshot patch.
    RiskPolicy,
}

impl StateKey {
    /// Returns the stable storage key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JevUsage => "jev_usage",
            Self::ModelBudget => "model_budget",
            Self::EquityBaselines => "equity_baselines",
            Self::StopBasis => "stop_basis",
            Self::ProfitHarvest => "profit_harvest",
            Self::RiskPolicy => "risk_policy",
        }
    }
}

/// Errors raised by a raw state store.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The store could not read or write the value.
    #[error("runtime state storage failed: {reason}")]
    Storage {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Narrow contract every runtime-state implementation implements.
#[async_trait]
pub trait StateStore: Send + Sync + fmt::Debug + 'static {
    /// Reads one key, if present.
    ///
    /// # Errors
    /// Returns [`StateError::Storage`] when the store is unavailable.
    async fn load(&self, key: &str) -> Result<Option<Value>, StateError>;

    /// Writes one key, replacing any previous value.
    ///
    /// # Errors
    /// Returns [`StateError::Storage`] when the store is unavailable.
    async fn save(&self, key: &str, value: &Value) -> Result<(), StateError>;
}

/// Runtime-state facade handed to the runtimes that snapshot themselves.
///
/// A disabled facade (no database configured) turns every read into `None`
/// and every write into a no-op, so the same code paths run in tests and in
/// database-less deployments.
#[derive(Debug, Clone)]
pub struct RuntimeState {
    store: Option<Arc<dyn StateStore>>,
    /// Last value successfully written or read per key; identical snapshots
    /// are skipped so a quiet minute costs no database write.
    last_seen: Arc<Mutex<HashMap<&'static str, Value>>>,
}

impl RuntimeState {
    /// Builds a facade over an optional store.
    pub fn new(store: Option<Arc<dyn StateStore>>) -> Self {
        Self {
            store,
            last_seen: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Builds a facade that persists nothing.
    pub fn disabled() -> Self {
        Self::new(None)
    }

    fn with_last_seen<T>(&self, apply: impl FnOnce(&mut HashMap<&'static str, Value>) -> T) -> T {
        let mut guard = match self.last_seen.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply(&mut guard)
    }

    /// Whether a store is attached.
    pub fn enabled(&self) -> bool {
        self.store.is_some()
    }

    /// Loads one key; failures log and read as absent so startup continues on
    /// the in-memory baseline instead of refusing to serve.
    pub async fn load(&self, key: StateKey) -> Option<Value> {
        let store = self.store.as_ref()?;
        match store.load(key.as_str()).await {
            Ok(value) => {
                if let Some(value) = &value {
                    self.with_last_seen(|last_seen| {
                        last_seen.insert(key.as_str(), value.clone());
                    });
                }
                value
            }
            Err(error) => {
                tracing::warn!(key = key.as_str(), %error, "runtime state load failed");
                None
            }
        }
    }

    /// Best-effort save; failures log a warning and are otherwise ignored.
    /// Values identical to the last successful read or write are skipped.
    pub async fn save(&self, key: StateKey, value: &Value) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let unchanged = self.with_last_seen(|last_seen| last_seen.get(key.as_str()) == Some(value));
        if unchanged {
            return;
        }
        match store.save(key.as_str(), value).await {
            Ok(()) => {
                self.with_last_seen(|last_seen| {
                    last_seen.insert(key.as_str(), value.clone());
                });
            }
            Err(error) => {
                tracing::warn!(key = key.as_str(), %error, "runtime state save failed");
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// In-memory store whose contents and write count tests can inspect.
    #[derive(Debug, Default)]
    pub(crate) struct MemoryState {
        values: Mutex<HashMap<String, Value>>,
        writes: std::sync::atomic::AtomicUsize,
    }

    impl MemoryState {
        /// Reads what a key currently holds.
        pub(crate) fn saved(&self, key: StateKey) -> Option<Value> {
            let values = match self.values.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            values.get(key.as_str()).cloned()
        }

        /// Number of writes the store has accepted.
        pub(crate) fn writes(&self) -> usize {
            self.writes.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Seeds a value without going through [`RuntimeState`], as another
        /// process would have left it.
        pub(crate) async fn seed(&self, key: StateKey, value: Value) {
            self.save(key.as_str(), &value)
                .await
                .expect("seed write must succeed");
        }
    }

    #[async_trait]
    impl StateStore for MemoryState {
        async fn load(&self, key: &str) -> Result<Option<Value>, StateError> {
            let values = match self.values.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            Ok(values.get(key).cloned())
        }

        async fn save(&self, key: &str, value: &Value) -> Result<(), StateError> {
            let mut values = match self.values.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            values.insert(key.to_owned(), value.clone());
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Store that always fails, for the best-effort paths.
    #[derive(Debug)]
    pub(crate) struct FailingState;

    #[async_trait]
    impl StateStore for FailingState {
        async fn load(&self, _key: &str) -> Result<Option<Value>, StateError> {
            Err(StateError::Storage {
                reason: "down".to_owned(),
            })
        }

        async fn save(&self, _key: &str, _value: &Value) -> Result<(), StateError> {
            Err(StateError::Storage {
                reason: "down".to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FailingState, MemoryState};
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_stable() {
        assert_eq!(StateKey::JevUsage.as_str(), "jev_usage");
        assert_eq!(StateKey::ModelBudget.as_str(), "model_budget");
        assert_eq!(StateKey::EquityBaselines.as_str(), "equity_baselines");
        assert_eq!(StateKey::StopBasis.as_str(), "stop_basis");
        assert_eq!(StateKey::RiskPolicy.as_str(), "risk_policy");
    }

    #[actix_web::test]
    async fn values_round_trip_through_a_store() {
        let store = Arc::new(MemoryState::default());
        let state = RuntimeState::new(Some(store.clone()));
        assert!(state.enabled());

        assert_eq!(state.load(StateKey::JevUsage).await, None);
        state.save(StateKey::JevUsage, &json!({ "calls": 7 })).await;
        assert_eq!(store.saved(StateKey::JevUsage), Some(json!({ "calls": 7 })));
        assert_eq!(
            state.load(StateKey::JevUsage).await,
            Some(json!({ "calls": 7 }))
        );
    }

    #[actix_web::test]
    async fn a_disabled_facade_is_inert() {
        let state = RuntimeState::disabled();
        assert!(!state.enabled());
        state.save(StateKey::JevUsage, &json!({ "calls": 1 })).await;
        assert_eq!(state.load(StateKey::JevUsage).await, None);
    }

    #[actix_web::test]
    async fn unchanged_snapshots_are_not_rewritten() {
        let store = Arc::new(MemoryState::default());
        let state = RuntimeState::new(Some(store.clone()));

        state.save(StateKey::JevUsage, &json!({ "calls": 1 })).await;
        assert_eq!(store.writes(), 1);
        state.save(StateKey::JevUsage, &json!({ "calls": 1 })).await;
        assert_eq!(store.writes(), 1, "identical snapshots are skipped");
        state.save(StateKey::JevUsage, &json!({ "calls": 2 })).await;
        assert_eq!(store.writes(), 2, "changed snapshots are written");

        // A load seeds the skip list, so a restart does not immediately
        // rewrite what it just read.
        store
            .seed(StateKey::ModelBudget, json!({ "hourCalls": 4 }))
            .await;
        assert!(state.load(StateKey::ModelBudget).await.is_some());
        state
            .save(StateKey::ModelBudget, &json!({ "hourCalls": 4 }))
            .await;
        assert_eq!(store.writes(), 3, "the re-read value is not rewritten");
    }

    #[actix_web::test]
    async fn storage_failures_are_swallowed() {
        let state = RuntimeState::new(Some(Arc::new(FailingState)));
        assert_eq!(state.load(StateKey::RiskPolicy).await, None);
        state.save(StateKey::RiskPolicy, &json!({})).await;
    }
}
