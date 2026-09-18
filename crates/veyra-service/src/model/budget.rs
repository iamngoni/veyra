//! Model call budget as a [`DecisionEngine`] decorator.
//!
//! A 24/7 loop asks a provider for one decision per tick; a misconfiguration
//! (an interval typo, a runaway retry) would silently multiply that cost. The
//! budget wraps any engine without touching its implementation: calls beyond
//! a fixed hourly or daily window are refused with a clear error, so the
//! autopilot records `unavailable: ... budget ...` instead of spending.
//!
//! Zero limits mean unlimited, which is also the default: the guard exists to
//! bound accidents, not to ration normal operation.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use async_trait::async_trait;

use crate::model::{DecisionAnswer, DecisionEngine, DecisionRequest, ModelError, ModelProvider};

const HOUR: Duration = Duration::from_secs(3_600);
const DAY: Duration = Duration::from_secs(86_400);

/// Call limits for one provider runtime. Zero means unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetPolicy {
    hourly: u32,
    daily: u32,
}

impl BudgetPolicy {
    /// Builds a policy from already validated limits.
    pub fn new(hourly: u32, daily: u32) -> Self {
        Self { hourly, daily }
    }

    /// Hourly call limit; zero means unlimited.
    pub fn hourly(&self) -> u32 {
        self.hourly
    }

    /// Daily call limit; zero means unlimited.
    pub fn daily(&self) -> u32 {
        self.daily
    }
}

/// Why a call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetRefusal {
    /// The hourly window is exhausted.
    Hourly {
        /// Configured hourly limit.
        limit: u32,
    },
    /// The daily window is exhausted.
    Daily {
        /// Configured daily limit.
        limit: u32,
    },
}

impl fmt::Display for BudgetRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hourly { limit } => write!(formatter, "hourly call budget of {limit} exhausted"),
            Self::Daily { limit } => write!(formatter, "daily call budget of {limit} exhausted"),
        }
    }
}

/// Fixed-window counters; windows restart from the first admitted call after
/// their length has passed.
#[derive(Debug, Clone, Copy)]
struct WindowState {
    hour_start: Instant,
    hour_start_epoch: u64,
    hour_calls: u32,
    day_start: Instant,
    day_start_epoch: u64,
    day_calls: u32,
}

impl WindowState {
    fn new(now: Instant) -> Self {
        let epoch = epoch_secs();
        Self {
            hour_start: now,
            hour_start_epoch: epoch,
            hour_calls: 0,
            day_start: now,
            day_start_epoch: epoch,
            day_calls: 0,
        }
    }

    fn roll(&mut self, now: Instant) {
        if now.duration_since(self.hour_start) >= HOUR {
            self.hour_start = now;
            self.hour_start_epoch = epoch_secs();
            self.hour_calls = 0;
        }
        if now.duration_since(self.day_start) >= DAY {
            self.day_start = now;
            self.day_start_epoch = epoch_secs();
            self.day_calls = 0;
        }
    }
}

/// Wall-clock seconds since the Unix epoch, or zero for clocks before it.
fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Admits or refuses one call against the policy at instant `now`.
///
/// # Errors
/// Returns [`BudgetRefusal`] when the hourly or daily window is exhausted.
fn admit(
    state: &mut WindowState,
    policy: &BudgetPolicy,
    now: Instant,
) -> Result<(), BudgetRefusal> {
    state.roll(now);
    if policy.hourly > 0 && state.hour_calls >= policy.hourly {
        return Err(BudgetRefusal::Hourly {
            limit: policy.hourly,
        });
    }
    if policy.daily > 0 && state.day_calls >= policy.daily {
        return Err(BudgetRefusal::Daily {
            limit: policy.daily,
        });
    }
    state.hour_calls += 1;
    state.day_calls += 1;
    Ok(())
}

/// Non-sensitive view of one tracker for status output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    /// Configured hourly limit; zero means unlimited.
    pub hour_limit: u32,
    /// Calls admitted in the current hour window.
    pub hour_calls: u32,
    /// Configured daily limit; zero means unlimited.
    pub day_limit: u32,
    /// Calls admitted in the current day window.
    pub day_calls: u32,
}

/// Shared call counter for one engine.
#[derive(Debug)]
pub struct BudgetTracker {
    policy: BudgetPolicy,
    state: Mutex<WindowState>,
}

impl BudgetTracker {
    /// Builds a tracker starting now.
    pub fn new(policy: BudgetPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(WindowState::new(Instant::now())),
        }
    }

    /// Admits or refuses one call.
    ///
    /// # Errors
    /// Returns [`BudgetRefusal`] once the configured window is exhausted.
    pub fn admit(&self) -> Result<(), BudgetRefusal> {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        admit(&mut state, &self.policy, Instant::now())
    }

    /// Serializable snapshot of the window anchors and counts, so a restart
    /// resumes the same windows instead of resetting the budget.
    pub fn state_snapshot(&self) -> Value {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.roll(Instant::now());
        serde_json::json!({
            "hourStart": state.hour_start_epoch,
            "hourCalls": state.hour_calls,
            "dayStart": state.day_start_epoch,
            "dayCalls": state.day_calls
        })
    }

    /// Restores window anchors and counts from a stored snapshot. Windows
    /// that already expired relative to the wall clock stay empty, so a
    /// long-dormant snapshot cannot ration a fresh hour.
    ///
    /// # Errors
    /// Returns a description when the value is not a budget snapshot.
    pub fn restore_state(&self, value: &Value) -> Result<(), String> {
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("budget snapshot is missing `{name}`"))
        };
        let hour_start = field("hourStart")?;
        let hour_calls = u32::try_from(field("hourCalls")?)
            .map_err(|_| "budget hourCalls is out of range".to_owned())?;
        let day_start = field("dayStart")?;
        let day_calls = u32::try_from(field("dayCalls")?)
            .map_err(|_| "budget dayCalls is out of range".to_owned())?;

        let now = Instant::now();
        let epoch = epoch_secs();
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if epoch.saturating_sub(hour_start) < HOUR.as_secs() {
            let elapsed = Duration::from_secs(epoch - hour_start);
            state.hour_start = now.checked_sub(elapsed).unwrap_or(now);
            state.hour_start_epoch = hour_start;
            state.hour_calls = hour_calls;
        }
        if epoch.saturating_sub(day_start) < DAY.as_secs() {
            let elapsed = Duration::from_secs(epoch - day_start);
            state.day_start = now.checked_sub(elapsed).unwrap_or(now);
            state.day_start_epoch = day_start;
            state.day_calls = day_calls;
        }
        Ok(())
    }

    /// Current usage against the configured limits.
    pub fn snapshot(&self) -> BudgetSnapshot {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.roll(Instant::now());
        BudgetSnapshot {
            hour_limit: self.policy.hourly,
            hour_calls: state.hour_calls,
            day_limit: self.policy.daily,
            day_calls: state.day_calls,
        }
    }
}

/// Whatever engine it wraps, behind the same budget.
#[derive(Debug)]
pub struct BudgetedEngine {
    inner: Arc<dyn DecisionEngine>,
    tracker: Arc<BudgetTracker>,
}

impl BudgetedEngine {
    /// Wraps an engine with a shared tracker.
    pub fn new(inner: Arc<dyn DecisionEngine>, tracker: Arc<BudgetTracker>) -> Self {
        Self { inner, tracker }
    }
}

#[async_trait]
impl DecisionEngine for BudgetedEngine {
    fn provider(&self) -> ModelProvider {
        self.inner.provider()
    }

    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
        self.tracker
            .admit()
            .map_err(|refusal| ModelError::Request {
                reason: refusal.to_string(),
            })?;
        self.inner.answer(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AnswerFormat;
    use serde_json::json;

    fn policy(hourly: u32, daily: u32) -> BudgetPolicy {
        BudgetPolicy::new(hourly, daily)
    }

    #[test]
    fn fixed_windows_admit_refuse_and_roll_over() {
        let start = Instant::now();
        let mut state = WindowState::new(start);
        let hourly = policy(2, 3);

        assert!(admit(&mut state, &hourly, start).is_ok());
        assert!(admit(&mut state, &hourly, start + Duration::from_secs(10)).is_ok());
        assert_eq!(
            admit(&mut state, &hourly, start + Duration::from_secs(20)),
            Err(BudgetRefusal::Hourly { limit: 2 })
        );
        // A new hour resets the hourly window but not the daily counter.
        let next_hour = start + HOUR + Duration::from_secs(1);
        assert!(
            admit(&mut state, &hourly, next_hour).is_ok(),
            "third call of the day"
        );
        assert_eq!(
            admit(&mut state, &hourly, next_hour + Duration::from_secs(5)),
            Err(BudgetRefusal::Daily { limit: 3 })
        );
        // A new day resets everything.
        let next_day = start + DAY + Duration::from_secs(1);
        assert!(admit(&mut state, &hourly, next_day).is_ok());
    }

    #[test]
    fn budget_windows_survive_a_restart_through_a_snapshot() {
        let tracker = BudgetTracker::new(policy(2, 100));
        assert!(tracker.admit().is_ok());
        assert!(tracker.admit().is_ok());
        assert_eq!(
            tracker.admit(),
            Err(BudgetRefusal::Hourly { limit: 2 }),
            "the live window is exhausted"
        );

        let snapshot = tracker.state_snapshot();
        let restored = BudgetTracker::new(policy(2, 100));
        restored
            .restore_state(&snapshot)
            .expect("snapshot restores");
        assert_eq!(
            restored.admit(),
            Err(BudgetRefusal::Hourly { limit: 2 }),
            "the restored window still refuses calls"
        );

        // Windows that expired while the process was down start empty.
        let stale = serde_json::json!({
            "hourStart": 1_u64,
            "hourCalls": 5_u64,
            "dayStart": 1_u64,
            "dayCalls": 5_u64
        });
        let fresh = BudgetTracker::new(policy(2, 100));
        fresh
            .restore_state(&stale)
            .expect("stale snapshot restores");
        assert!(fresh.admit().is_ok(), "expired windows are not restored");

        // Malformed snapshots name what is missing or out of range.
        for broken in [
            serde_json::json!({ "hourStart": 1_u64 }),
            serde_json::json!({
                "hourStart": u64::MAX,
                "hourCalls": u64::from(u32::MAX) + 1,
                "dayStart": 1_u64,
                "dayCalls": 0_u64
            }),
        ] {
            assert!(fresh.restore_state(&broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn zero_limits_mean_unlimited() {
        let start = Instant::now();
        let mut state = WindowState::new(start);
        let unlimited = policy(0, 0);
        for index in 0..1_000u32 {
            assert!(
                admit(
                    &mut state,
                    &unlimited,
                    start + Duration::from_secs(u64::from(index))
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn refusals_describe_the_exhausted_window() {
        assert_eq!(
            BudgetRefusal::Hourly { limit: 5 }.to_string(),
            "hourly call budget of 5 exhausted"
        );
        assert_eq!(
            BudgetRefusal::Daily { limit: 40 }.to_string(),
            "daily call budget of 40 exhausted"
        );
    }

    #[test]
    fn snapshots_reflect_usage_and_limits() {
        let tracker = BudgetTracker::new(policy(10, 100));
        tracker.admit().expect("first call");
        tracker.admit().expect("second call");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.hour_limit, 10);
        assert_eq!(snapshot.hour_calls, 2);
        assert_eq!(snapshot.day_limit, 100);
        assert_eq!(snapshot.day_calls, 2);
    }

    #[derive(Debug)]
    struct CountingEngine;

    #[async_trait]
    impl DecisionEngine for CountingEngine {
        fn provider(&self) -> ModelProvider {
            ModelProvider::OpenRouter
        }

        async fn answer(&self, _request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
            Ok(DecisionAnswer {
                value: json!({"action": "none"}),
            })
        }
    }

    fn request() -> DecisionRequest {
        DecisionRequest {
            instructions: "test".to_owned(),
            input: "{}".to_owned(),
            format: AnswerFormat {
                name: "test".to_owned(),
                schema: json!({"type": "object"}),
            },
            tier: crate::model::ModelTier::Balanced,
        }
    }

    #[actix_web::test]
    async fn the_decorator_refuses_beyond_the_budget() {
        let tracker = Arc::new(BudgetTracker::new(policy(1, 0)));
        let engine = BudgetedEngine::new(Arc::new(CountingEngine), tracker.clone());
        assert_eq!(engine.provider(), ModelProvider::OpenRouter);
        engine.answer(request()).await.expect("first call passes");
        let error = engine
            .answer(request())
            .await
            .expect_err("second call is refused");
        assert!(
            error.to_string().contains("hourly call budget of 1"),
            "unexpected error: {error}"
        );
        assert_eq!(tracker.snapshot().hour_calls, 1, "refusals are not counted");
    }
}
