//! Equity drawdown guard.
//!
//! Two process-lifetime baselines the gate uses as breakers: the equity at the
//! start of the current UTC day, and the highest equity observed since startup.
//! Drawdowns are reported as percentages below those baselines.
//!
//! The baselines live in memory: a restart re-baselines at the current equity,
//! which is documented behaviour, not an accident.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Drawdowns at one observation, in percent (0 = at or above the baseline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drawdowns {
    /// Percentage below the current UTC day's opening equity.
    pub day_percent: f64,
    /// Percentage below the highest equity seen since startup.
    pub peak_percent: f64,
}

#[derive(Debug, Default, Clone, Copy)]
struct GuardState {
    day: Option<u64>,
    day_start_equity: Option<f64>,
    peak_equity: Option<f64>,
}

/// Process-lifetime equity tracker shared by the service.
#[derive(Debug, Default)]
pub struct EquityGuard {
    state: Mutex<GuardState>,
}

impl EquityGuard {
    /// Builds an unobserved guard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one equity observation and returns the drawdowns it implies.
    ///
    /// Unusable values (non-finite or non-positive) are ignored so a broken
    /// snapshot cannot move the baselines.
    pub fn observe(&self, equity: f64, now: SystemTime) -> Drawdowns {
        if !equity.is_finite() || equity <= 0.0 {
            return Drawdowns {
                day_percent: 0.0,
                peak_percent: 0.0,
            };
        }
        let day = now
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs() / 86_400);
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.peak_equity.is_none_or(|peak| equity > peak) {
            state.peak_equity = Some(equity);
        }
        if state.day != day || state.day_start_equity.is_none() {
            state.day = day;
            state.day_start_equity = Some(equity);
        }
        Drawdowns {
            day_percent: percent_below(state.day_start_equity.unwrap_or(equity), equity),
            peak_percent: percent_below(state.peak_equity.unwrap_or(equity), equity),
        }
    }
}

/// How far `equity` sits below `baseline`, in percent.
fn percent_below(baseline: f64, equity: f64) -> f64 {
    if baseline.is_finite() && baseline > 0.0 && equity < baseline {
        (baseline - equity) / baseline * 100.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn day(offset: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_767_787_200 + offset * 86_400)
    }

    #[test]
    fn tracks_daily_and_peak_drawdowns_separately() {
        let guard = EquityGuard::new();
        assert_eq!(guard.observe(100.0, day(0)).day_percent, 0.0);

        let drawdown = guard.observe(90.0, day(0));
        assert!((drawdown.day_percent - 10.0).abs() < 1e-9);
        assert!((drawdown.peak_percent - 10.0).abs() < 1e-9);

        // A new day re-baselines the daily figure but not the peak.
        let drawdown = guard.observe(95.0, day(1));
        assert_eq!(drawdown.day_percent, 0.0, "new day opens at 95");
        assert!(
            (drawdown.peak_percent - 5.0).abs() < 1e-9,
            "peak remains 100"
        );

        // A fresh peak resets the peak drawdown.
        let drawdown = guard.observe(110.0, day(1));
        assert_eq!(drawdown.peak_percent, 0.0);
        let drawdown = guard.observe(107.0, day(1));
        assert!((drawdown.peak_percent - (3.0 / 110.0 * 100.0)).abs() < 1e-9);
    }

    #[test]
    fn ignores_unusable_values() {
        let guard = EquityGuard::new();
        guard.observe(100.0, day(0));
        assert_eq!(guard.observe(f64::NAN, day(0)).day_percent, 0.0);
        assert_eq!(guard.observe(0.0, day(0)).peak_percent, 0.0);
        // The baseline survived: 50 is 50% below 100.
        assert!((guard.observe(50.0, day(0)).day_percent - 50.0).abs() < 1e-9);
    }
}
