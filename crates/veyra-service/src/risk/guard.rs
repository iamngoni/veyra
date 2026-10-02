//! Equity drawdown guard.
//!
//! Two baselines the gate uses as breakers, both measured in the account
//! currency and reported as percentages below the baseline:
//!
//! * **Daily**: the account at the start of the current day. [`LossRules`]
//!   choose when the day starts (UTC midnight, or the broker server's
//!   midnight, which is when most prop firms reset) and what it is measured
//!   from (equity, balance, or the higher of the two).
//! * **Maximum**: the highest equity reached, or a fixed reference balance
//!   when one is set (a prop firm's static maximum loss from the initial
//!   balance).
//!
//! The defaults reproduce the original guard: UTC days, equity, highest
//! equity. Baselines are snapshotted to the durable runtime state, so a
//! restart mid-day resumes the same baselines instead of handing the bot a
//! fresh loss budget.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Drawdowns at one observation, in percent (0 = at or above the baseline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drawdowns {
    /// Percentage below the current day's opening baseline.
    pub day_percent: f64,
    /// Percentage below the highest equity reached, or below the fixed
    /// drawdown reference when one is set.
    pub peak_percent: f64,
}

/// When the daily-loss day starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DailyReset {
    /// UTC midnight.
    #[default]
    Utc,
    /// The broker server's midnight (most prop firms); UTC until the broker
    /// clock is known.
    Broker,
}

impl DailyReset {
    /// Stable identifier for status output, storage, and the console.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Utc => "utc",
            Self::Broker => "broker",
        }
    }

    /// Parses the identifier accepted by the environment and the console.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "utc" => Some(Self::Utc),
            "broker" => Some(Self::Broker),
            _ => None,
        }
    }
}

/// What the daily loss is measured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DailyBasis {
    /// Equity at the start of the day, floating profit included.
    #[default]
    Equity,
    /// Balance at the start of the day (FTMO, FundedNext).
    Balance,
    /// The higher of the two at the start of the day (The5ers).
    Higher,
}

impl DailyBasis {
    /// Stable identifier for status output, storage, and the console.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Equity => "equity",
            Self::Balance => "balance",
            Self::Higher => "higher",
        }
    }

    /// Parses the identifier accepted by the environment and the console.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "equity" => Some(Self::Equity),
            "balance" => Some(Self::Balance),
            "higher" => Some(Self::Higher),
            _ => None,
        }
    }
}

/// How the account's losses are measured; see the module notes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LossRules {
    /// When the day starts.
    pub reset: DailyReset,
    /// What the daily loss is measured from.
    pub basis: DailyBasis,
    /// Fixed balance the maximum loss is measured from; 0 measures from the
    /// highest equity reached.
    pub drawdown_reference: f64,
}

/// One account reading for [`EquityGuard::observe_with`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// Account equity.
    pub equity: f64,
    /// Account balance, when known.
    pub balance: Option<f64>,
    /// When it was read.
    pub now: SystemTime,
    /// Broker clock offset from UTC in seconds, when known.
    pub broker_offset_secs: Option<i64>,
}

#[derive(Debug, Default, Clone, Copy)]
struct GuardState {
    day: Option<u64>,
    day_start_equity: Option<f64>,
    day_start_balance: Option<f64>,
    peak_equity: Option<f64>,
}

/// Equity tracker shared by the service.
#[derive(Debug, Default)]
pub struct EquityGuard {
    state: Mutex<GuardState>,
}

impl EquityGuard {
    /// Builds an unobserved guard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serializable snapshot of the baselines.
    pub fn state_snapshot(&self) -> serde_json::Value {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        serde_json::json!({
            "day": state.day,
            "dayStartEquity": state.day_start_equity,
            "dayStartBalance": state.day_start_balance,
            "peakEquity": state.peak_equity
        })
    }

    /// Restores baselines from a stored snapshot; absent or unusable fields
    /// stay unset and the next observation re-baselines them.
    ///
    /// # Errors
    /// Returns a description when a present value is not usable.
    pub fn restore_state(&self, value: &serde_json::Value) -> Result<(), String> {
        let day = match value.get("day") {
            None | Some(serde_json::Value::Null) => None,
            Some(day) => Some(
                day.as_u64()
                    .ok_or_else(|| "equity snapshot `day` must be an integer".to_owned())?,
            ),
        };
        let equity = |name: &str| match value.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(raw) => {
                let parsed = raw
                    .as_f64()
                    .ok_or_else(|| format!("equity snapshot `{name}` must be a number"))?;
                if parsed.is_finite() && parsed > 0.0 {
                    Ok(Some(parsed))
                } else {
                    Err(format!("equity snapshot `{name}` must be positive"))
                }
            }
        };
        let day_start_equity = equity("dayStartEquity")?;
        let day_start_balance = equity("dayStartBalance")?;
        let peak_equity = equity("peakEquity")?;

        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.day = day;
        state.day_start_equity = day_start_equity;
        state.day_start_balance = day_start_balance;
        state.peak_equity = peak_equity;
        Ok(())
    }

    /// Records one equity observation under the default rules (UTC days,
    /// equity, highest equity).
    pub fn observe(&self, equity: f64, now: SystemTime) -> Drawdowns {
        self.observe_with(
            Reading {
                equity,
                balance: None,
                now,
                broker_offset_secs: None,
            },
            LossRules::default(),
        )
    }

    /// Records one account reading and returns the drawdowns it implies under
    /// `rules`.
    ///
    /// Unusable values (non-finite or non-positive) are ignored so a broken
    /// snapshot cannot move the baselines.
    pub fn observe_with(&self, reading: Reading, rules: LossRules) -> Drawdowns {
        let equity = reading.equity;
        if !equity.is_finite() || equity <= 0.0 {
            return Drawdowns {
                day_percent: 0.0,
                peak_percent: 0.0,
            };
        }
        let balance = reading
            .balance
            .filter(|balance| balance.is_finite() && *balance > 0.0);
        let day = reading
            .now
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
            .map(|utc| match (rules.reset, reading.broker_offset_secs) {
                (DailyReset::Broker, Some(offset)) => utc.saturating_add(offset),
                _ => utc,
            })
            .and_then(|clock| u64::try_from(clock.div_euclid(86_400)).ok());
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
            state.day_start_balance = balance;
        } else if state.day_start_balance.is_none() {
            // A baseline restored from before balances were kept, or set by
            // an equity-only reading, learns the balance at the next one.
            state.day_start_balance = balance;
        }
        let day_equity = state.day_start_equity.unwrap_or(equity);
        let day_baseline = match (rules.basis, state.day_start_balance) {
            (DailyBasis::Equity, _) | (_, None) => day_equity,
            (DailyBasis::Balance, Some(balance)) => balance,
            (DailyBasis::Higher, Some(balance)) => balance.max(day_equity),
        };
        let max_baseline = if rules.drawdown_reference.is_finite() && rules.drawdown_reference > 0.0
        {
            rules.drawdown_reference
        } else {
            state.peak_equity.unwrap_or(equity)
        };
        Drawdowns {
            day_percent: percent_below(day_baseline, equity),
            peak_percent: percent_below(max_baseline, equity),
        }
    }
}

/// Observes the latest account snapshot under the live policy's loss rules,
/// with the broker clock when it is known. Reads state only.
pub fn observe_account(
    state: &crate::AppState,
    account: &crate::broker::AccountSnapshotPayload,
    now: SystemTime,
) -> Drawdowns {
    let rules = state.risk().policy().loss_rules();
    let broker_offset_secs = match rules.reset {
        DailyReset::Broker => crate::broker_clock::BrokerClock::from_state(state)
            .ok()
            .map(crate::broker_clock::BrokerClock::offset_secs),
        DailyReset::Utc => None,
    };
    state.equity_guard().observe_with(
        Reading {
            equity: account.equity,
            balance: Some(account.balance),
            now,
            broker_offset_secs,
        },
        rules,
    )
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
    fn baselines_survive_a_restart_through_a_snapshot() {
        let guard = EquityGuard::new();
        guard.observe(100.0, day(0));
        guard.observe(90.0, day(0));
        let snapshot = guard.state_snapshot();

        // A restarted guard resumes at the same baselines: 90 is 10% below
        // the restored 100 baseline in both windows.
        let restarted = EquityGuard::new();
        restarted
            .restore_state(&snapshot)
            .expect("snapshot restores");
        let drawdown = restarted.observe(90.0, day(0));
        assert!((drawdown.day_percent - 10.0).abs() < 1e-9);
        assert!((drawdown.peak_percent - 10.0).abs() < 1e-9);

        // A new day re-baselines the daily figure after a restore too.
        let drawdown = restarted.observe(95.0, day(1));
        assert_eq!(drawdown.day_percent, 0.0);

        // Malformed snapshots fail closed instead of adopting junk.
        for broken in [
            serde_json::json!({ "day": "today" }),
            serde_json::json!({ "peakEquity": -5.0 }),
            serde_json::json!({ "dayStartEquity": "lots" }),
        ] {
            assert!(restarted.restore_state(&broken).is_err(), "{broken}");
        }
        // An empty snapshot simply leaves the guard unobserved.
        let empty = EquityGuard::new();
        empty
            .restore_state(&serde_json::json!({}))
            .expect("empty ok");
        assert_eq!(empty.observe(50.0, day(0)).day_percent, 0.0);
    }

    fn reading(equity: f64, balance: f64, now: SystemTime, offset: Option<i64>) -> Reading {
        Reading {
            equity,
            balance: Some(balance),
            now,
            broker_offset_secs: offset,
        }
    }

    #[test]
    fn the_daily_loss_follows_the_chosen_basis() {
        // Day opens with a floating profit: balance 100, equity 104.
        let rules = |basis| LossRules {
            basis,
            ..LossRules::default()
        };
        for (basis, expected) in [
            (DailyBasis::Equity, (104.0 - 98.0) / 104.0 * 100.0),
            (DailyBasis::Balance, 2.0),
            (DailyBasis::Higher, (104.0 - 98.0) / 104.0 * 100.0),
        ] {
            let guard = EquityGuard::new();
            guard.observe_with(reading(104.0, 100.0, day(0), None), rules(basis));
            let drawdown = guard.observe_with(reading(98.0, 100.0, day(0), None), rules(basis));
            assert!((drawdown.day_percent - expected).abs() < 1e-9, "{basis:?}");
        }
        // Higher picks the balance when the day opens on a floating loss.
        let guard = EquityGuard::new();
        let higher = rules(DailyBasis::Higher);
        guard.observe_with(reading(96.0, 100.0, day(0), None), higher);
        let drawdown = guard.observe_with(reading(95.0, 100.0, day(0), None), higher);
        assert!((drawdown.day_percent - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_broker_day_resets_at_the_broker_midnight() {
        let broker = LossRules {
            reset: DailyReset::Broker,
            ..LossRules::default()
        };
        let guard = EquityGuard::new();
        // 22:30 UTC is 00:30 on a broker clock two hours ahead: a new broker
        // day, though the UTC day continues.
        let midnight = UNIX_EPOCH + std::time::Duration::from_secs(20_461 * 86_400);
        let late = midnight + std::time::Duration::from_secs(22 * 3_600 + 1_800);
        let evening = midnight + std::time::Duration::from_secs(21 * 3_600);
        guard.observe_with(reading(100.0, 100.0, evening, Some(7_200)), broker);
        let drawdown = guard.observe_with(reading(90.0, 100.0, late, Some(7_200)), broker);
        assert_eq!(
            drawdown.day_percent, 0.0,
            "the broker day re-baselined at 90"
        );
        // The same readings on UTC days keep one day.
        let utc = EquityGuard::new();
        utc.observe_with(
            reading(100.0, 100.0, evening, Some(7_200)),
            LossRules::default(),
        );
        let drawdown = utc.observe_with(
            reading(90.0, 100.0, late, Some(7_200)),
            LossRules::default(),
        );
        assert!((drawdown.day_percent - 10.0).abs() < 1e-9);
        // An unknown broker clock falls back to UTC days.
        let unknown = EquityGuard::new();
        unknown.observe_with(reading(100.0, 100.0, evening, None), broker);
        let drawdown = unknown.observe_with(reading(90.0, 100.0, late, None), broker);
        assert!((drawdown.day_percent - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_reference_balance_fixes_the_maximum_loss() {
        let fixed = LossRules {
            drawdown_reference: 1_000.0,
            ..LossRules::default()
        };
        let guard = EquityGuard::new();
        // A run-up to 1,200 does not move a static maximum loss.
        guard.observe_with(reading(1_200.0, 1_200.0, day(0), None), fixed);
        let drawdown = guard.observe_with(reading(950.0, 1_200.0, day(1), None), fixed);
        assert!((drawdown.peak_percent - 5.0).abs() < 1e-9);
        // Above the reference there is no drawdown at all.
        let drawdown = guard.observe_with(reading(1_100.0, 1_200.0, day(1), None), fixed);
        assert_eq!(drawdown.peak_percent, 0.0);
        // Without a reference the highest equity applies again.
        let drawdown = guard.observe_with(
            reading(1_100.0, 1_200.0, day(1), None),
            LossRules::default(),
        );
        assert!((drawdown.peak_percent - (100.0 / 1_200.0 * 100.0)).abs() < 1e-9);
    }

    #[test]
    fn a_restored_baseline_without_a_balance_learns_it() {
        let guard = EquityGuard::new();
        guard
            .restore_state(
                &serde_json::json!({ "day": 20_460, "dayStartEquity": 100.0, "peakEquity": 100.0 }),
            )
            .expect("restore");
        let balance = LossRules {
            basis: DailyBasis::Balance,
            ..LossRules::default()
        };
        let now = UNIX_EPOCH + std::time::Duration::from_secs(20_460 * 86_400 + 3_600);
        // The first reading supplies the balance baseline (98), so a fall to
        // 97 is measured from it.
        guard.observe_with(reading(99.0, 98.0, now, None), balance);
        let drawdown = guard.observe_with(reading(97.0, 98.0, now, None), balance);
        assert!((drawdown.day_percent - (1.0 / 98.0 * 100.0)).abs() < 1e-9);
        assert_eq!(guard.state_snapshot()["dayStartBalance"], 98.0);
        assert!(
            guard
                .restore_state(&serde_json::json!({ "dayStartBalance": 0 }))
                .is_err()
        );
    }

    #[test]
    fn rule_names_round_trip() {
        for reset in [DailyReset::Utc, DailyReset::Broker] {
            assert_eq!(DailyReset::parse(reset.as_str()), Some(reset));
        }
        for basis in [DailyBasis::Equity, DailyBasis::Balance, DailyBasis::Higher] {
            assert_eq!(DailyBasis::parse(basis.as_str()), Some(basis));
        }
        assert_eq!(DailyReset::parse(" broker "), Some(DailyReset::Broker));
        assert_eq!(DailyReset::parse("prague"), None);
        assert_eq!(DailyBasis::parse("lowest"), None);
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
