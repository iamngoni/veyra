//! Deterministic, fail-closed risk policy.
//!
//! [`RiskPolicy`] is parsed once at startup; [`RiskGate`] evaluates drafts
//! against it and against explicit account facts supplied by the caller. The
//! gate is the only authority that turns a draft into an executable intent,
//! and it rejects whenever required state is unavailable, so a lost or stale
//! broker link can only reduce activity, never increase it.
//!
//! Defaults are deliberately restrictive: no instrument is allowed until it
//! is configured.

pub mod gate;
pub mod guard;
pub mod valuation;
pub mod window;

pub use gate::{AccountFacts, RiskCode, RiskDecision, RiskGate, RiskRejection};

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::broker::Symbol;
use crate::trading::intent::{Volume, parse_instrument};

/// Default cap on open venue orders once every other control passes.
const DEFAULT_MAX_OPEN_ORDERS: u32 = 1;
/// Default duplicate-suppression window in seconds.
const DEFAULT_DUPLICATE_WINDOW_SECS: u64 = 60;
/// Largest allowlist the parser accepts.
const MAX_SYMBOLS: usize = 64;
/// Absolute ceiling for the open-order cap, above which configuration is a bug.
const MAX_OPEN_ORDERS_CEILING: u32 = 1_000;
/// Absolute ceiling for the duplicate-suppression window.
const MAX_DUPLICATE_WINDOW_SECS: u64 = 86_400;
/// Largest representable UTC hour.
const MAX_SESSION_HOUR: u8 = 23;
/// Default per-trade risk cap as a percentage of equity (0 disables).
const DEFAULT_MAX_RISK_PERCENT: f64 = 12.0;
/// Default daily-loss breaker as a percentage below the UTC day's opening
/// equity (0 disables).
const DEFAULT_MAX_DAILY_LOSS_PERCENT: f64 = 10.0;
/// Default peak-drawdown breaker as a percentage below the highest equity
/// since startup (0 disables).
const DEFAULT_MAX_PEAK_DRAWDOWN_PERCENT: f64 = 25.0;
/// Default cap on net USD-directional exposure in lots (0 disables).
const DEFAULT_MAX_NET_FACTOR_LOTS: f64 = 0.01;
/// Default news blackout either side of a high-impact event, in minutes.
const DEFAULT_CALENDAR_BLACKOUT_MINUTES: u64 = 30;
/// Absolute ceiling for the news blackout window (24 hours either side).
const MAX_CALENDAR_BLACKOUT_MINUTES: u64 = 1_440;
/// Default minimum stop distance as a fraction of ATR(14) (0 disables).
const DEFAULT_MIN_STOP_ATR_FRACTION: f64 = 0.25;
/// Absolute ceiling for the ATR stop floor.
const MAX_MIN_STOP_ATR_FRACTION: f64 = 2.0;

const SYMBOLS_RULE: &str = "must list 1-64 comma-separated instrument symbols";
const VOLUME_RULE: &str = "must be a finite number greater than 0 and at most 100";
const OPEN_ORDERS_RULE: &str = "must be an integer from 0 through 1000";
const DUPLICATE_WINDOW_RULE: &str = "must be an integer number of seconds from 0 through 86400";
const PERCENT_RULE: &str = "must be a number from 0 through 100 (0 disables the check)";
const FACTOR_LOTS_RULE: &str = "must be a number from 0 through 100 lots (0 disables the check)";
const CALENDAR_BLACKOUT_RULE: &str =
    "must be an integer number of minutes from 0 through 1440 (0 disables the check)";
const ATR_FRACTION_RULE: &str =
    "must be a number from 0 through 2 (fraction of ATR; 0 disables the check)";
const SESSION_RULE: &str = "must be `HH-HH` with UTC hours 0-23 and different bounds";
const KILL_SWITCH_RULE: &str = "must be `true` or `false`";

/// Errors raised while parsing risk policy settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid risk setting `{name}`: {reason}")]
pub struct RiskError {
    /// Setting that must be corrected.
    pub name: &'static str,
    /// Non-sensitive acceptance rule.
    pub reason: &'static str,
}

/// Partial update to the live policy, as the control surface submits it.
/// Every field is optional; omitted fields keep their current value.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RiskPolicyPatch {
    /// Engage or release the kill switch.
    pub kill_switch: Option<bool>,
    /// Replacement instrument allowlist (1-64 symbols; empty is rejected —
    /// stop trading with the kill switch instead).
    pub symbols: Option<Vec<String>>,
    /// Largest lot volume a single intent may request.
    pub max_volume_per_order: Option<f64>,
    /// Largest total open volume.
    pub max_total_lots: Option<f64>,
    /// Largest number of open venue orders.
    pub max_open_orders: Option<u32>,
    /// Duplicate-suppression window in seconds.
    pub duplicate_window_secs: Option<u64>,
    /// Session window (`8-17`, may wrap); an empty string clears it.
    pub session_utc: Option<String>,
    /// Per-trade risk cap, percent of equity (0 disables).
    pub max_risk_percent: Option<f64>,
    /// Daily-loss breaker, percent (0 disables).
    pub max_daily_loss_percent: Option<f64>,
    /// Peak-drawdown breaker, percent (0 disables).
    pub max_peak_drawdown_percent: Option<f64>,
    /// Net USD-direction cap in lots (0 disables).
    pub max_net_factor_lots: Option<f64>,
    /// News blackout either side of a high-impact event, in minutes
    /// (0 disables the check).
    pub calendar_blackout_minutes: Option<u64>,
    /// Minimum stop distance as a fraction of ATR(14) (0 disables).
    pub min_stop_atr_fraction: Option<f64>,
}

/// Validates one symbol list from the control surface.
fn parse_symbol_entries(entries: &[String]) -> Result<Vec<Symbol>, RiskError> {
    let invalid = |reason: &'static str| RiskError {
        name: "symbols",
        reason,
    };
    let mut symbols: Vec<Symbol> = Vec::new();
    for entry in entries.iter().map(|entry| entry.trim()) {
        if entry.is_empty() {
            return Err(invalid(SYMBOLS_RULE));
        }
        let symbol = parse_instrument(entry).map_err(|error| invalid(error.reason))?;
        if !symbols.contains(&symbol) {
            symbols.push(symbol);
        }
    }
    if symbols.is_empty() || symbols.len() > MAX_SYMBOLS {
        return Err(invalid(SYMBOLS_RULE));
    }
    Ok(symbols)
}

/// Validates one control-surface percentage.
fn percent_value(name: &'static str, value: f64) -> Result<f64, RiskError> {
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(value)
    } else {
        Err(RiskError {
            name,
            reason: PERCENT_RULE,
        })
    }
}

/// A UTC hour window; `22-6` wraps midnight and the bounds must differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionWindow {
    start_hour: u8,
    end_hour: u8,
}

impl SessionWindow {
    /// Parses `<start>-<end>` in UTC hours.
    ///
    /// # Errors
    /// Returns [`RiskError`] when the value is not two different hours 0-23.
    pub fn parse(value: &str) -> Result<Self, RiskError> {
        let invalid = RiskError {
            name: "VEYRA_RISK_SESSION_HOURS_UTC",
            reason: SESSION_RULE,
        };
        let (start, end) = value.split_once('-').ok_or(invalid)?;
        let start: u8 = start.trim().parse().map_err(|_| invalid)?;
        let end: u8 = end.trim().parse().map_err(|_| invalid)?;
        if start > MAX_SESSION_HOUR || end > MAX_SESSION_HOUR || start == end {
            return Err(invalid);
        }
        Ok(Self {
            start_hour: start,
            end_hour: end,
        })
    }

    /// First allowed UTC hour.
    pub fn start_hour(self) -> u8 {
        self.start_hour
    }

    /// First excluded UTC hour.
    pub fn end_hour(self) -> u8 {
        self.end_hour
    }

    /// Whether `hour` lies inside the window; wrapping windows cover both
    /// ranges (for example `22-6` allows 22, 23, 0-5).
    pub fn contains(self, hour: u8) -> bool {
        if self.start_hour < self.end_hour {
            hour >= self.start_hour && hour < self.end_hour
        } else {
            hour >= self.start_hour || hour < self.end_hour
        }
    }
}

/// Deterministic limits every intent must satisfy before it can execute.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskPolicy {
    kill_switch: bool,
    symbols: Vec<Symbol>,
    max_volume_per_order: Volume,
    max_total_lots: Volume,
    max_open_orders: u32,
    duplicate_window: Duration,
    session: Option<SessionWindow>,
    max_risk_percent: f64,
    max_daily_loss_percent: f64,
    max_peak_drawdown_percent: f64,
    max_net_factor_lots: f64,
    calendar_blackout_minutes: u64,
    min_stop_atr_fraction: f64,
}

impl RiskPolicy {
    /// Builds a policy from validated components.
    pub fn new(
        kill_switch: bool,
        symbols: Vec<Symbol>,
        max_volume_per_order: Volume,
        max_total_lots: Volume,
        max_open_orders: u32,
        duplicate_window: Duration,
        session: Option<SessionWindow>,
    ) -> Self {
        Self {
            kill_switch,
            symbols,
            max_volume_per_order,
            max_total_lots,
            max_open_orders,
            duplicate_window,
            session,
            // The constructor is the test and embedding baseline: valuation
            // rules are opt-in. Production configuration (`from_source`)
            // applies the operational defaults below.
            max_risk_percent: 0.0,
            max_daily_loss_percent: 0.0,
            max_peak_drawdown_percent: 0.0,
            max_net_factor_lots: 0.0,
            calendar_blackout_minutes: 0,
            min_stop_atr_fraction: 0.0,
        }
    }

    /// Sets the valuation and breaker limits:
    /// per-trade risk percent, daily-loss percent, peak-drawdown percent, and
    /// the net USD-directional lot cap. Zero disables each check.
    pub fn with_limits(
        mut self,
        max_risk_percent: f64,
        max_daily_loss_percent: f64,
        max_peak_drawdown_percent: f64,
        max_net_factor_lots: f64,
    ) -> Self {
        self.max_risk_percent = max_risk_percent;
        self.max_daily_loss_percent = max_daily_loss_percent;
        self.max_peak_drawdown_percent = max_peak_drawdown_percent;
        self.max_net_factor_lots = max_net_factor_lots;
        self
    }

    /// Sets the news blackout window in minutes either side of a high-impact
    /// event. Zero disables the check; the calendar must also be configured
    /// for it to apply.
    pub fn with_calendar_blackout(mut self, minutes: u64) -> Self {
        self.calendar_blackout_minutes = minutes;
        self
    }

    /// Sets the minimum stop distance as a fraction of ATR(14). Zero
    /// disables the floor; the ATR window comes from the candles the tick
    /// already fetched.
    pub fn with_min_stop_atr_fraction(mut self, fraction: f64) -> Self {
        self.min_stop_atr_fraction = fraction;
        self
    }

    /// Reads optional `VEYRA_RISK_*` variables, applying restrictive defaults.
    ///
    /// # Errors
    /// Returns [`RiskError`] for a malformed value; startup then fails closed.
    pub fn from_env() -> Result<Self, RiskError> {
        Self::from_source(|name| std::env::var(name).ok())
    }

    /// Parses an injected settings source; absent names fall back to defaults.
    ///
    /// # Errors
    /// Returns [`RiskError`] when a supplied value is malformed.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Option<String>,
    ) -> Result<Self, RiskError> {
        let kill_switch = match trimmed(&mut source, "VEYRA_RISK_KILL_SWITCH").as_deref() {
            None => false,
            Some("true") => true,
            Some("false") => false,
            Some(_) => {
                return Err(RiskError {
                    name: "VEYRA_RISK_KILL_SWITCH",
                    reason: KILL_SWITCH_RULE,
                });
            }
        };

        let symbols = match trimmed(&mut source, "VEYRA_RISK_SYMBOLS") {
            None => Vec::new(),
            Some(raw) => parse_symbols(&raw)?,
        };

        let max_volume_per_order = match trimmed(&mut source, "VEYRA_RISK_MAX_VOLUME_PER_ORDER") {
            None => Volume::MINIMUM,
            Some(raw) => {
                let value = raw.parse::<f64>().map_err(|_| RiskError {
                    name: "VEYRA_RISK_MAX_VOLUME_PER_ORDER",
                    reason: VOLUME_RULE,
                })?;
                Volume::parse(value).map_err(|error| RiskError {
                    name: "VEYRA_RISK_MAX_VOLUME_PER_ORDER",
                    reason: error.reason,
                })?
            }
        };

        let max_total_lots = match trimmed(&mut source, "VEYRA_RISK_MAX_TOTAL_LOTS") {
            None => Volume::MINIMUM,
            Some(raw) => {
                let value = raw.parse::<f64>().map_err(|_| RiskError {
                    name: "VEYRA_RISK_MAX_TOTAL_LOTS",
                    reason: VOLUME_RULE,
                })?;
                Volume::parse(value).map_err(|error| RiskError {
                    name: "VEYRA_RISK_MAX_TOTAL_LOTS",
                    reason: error.reason,
                })?
            }
        };

        let max_open_orders = match trimmed(&mut source, "VEYRA_RISK_MAX_OPEN_ORDERS") {
            None => DEFAULT_MAX_OPEN_ORDERS,
            Some(raw) => {
                let value = raw.parse::<u32>().map_err(|_| RiskError {
                    name: "VEYRA_RISK_MAX_OPEN_ORDERS",
                    reason: OPEN_ORDERS_RULE,
                })?;
                if value > MAX_OPEN_ORDERS_CEILING {
                    return Err(RiskError {
                        name: "VEYRA_RISK_MAX_OPEN_ORDERS",
                        reason: OPEN_ORDERS_RULE,
                    });
                }
                value
            }
        };

        let duplicate_window = match trimmed(&mut source, "VEYRA_RISK_DUPLICATE_WINDOW_SECS") {
            None => Duration::from_secs(DEFAULT_DUPLICATE_WINDOW_SECS),
            Some(raw) => {
                let seconds = raw.parse::<u64>().map_err(|_| RiskError {
                    name: "VEYRA_RISK_DUPLICATE_WINDOW_SECS",
                    reason: DUPLICATE_WINDOW_RULE,
                })?;
                if seconds > MAX_DUPLICATE_WINDOW_SECS {
                    return Err(RiskError {
                        name: "VEYRA_RISK_DUPLICATE_WINDOW_SECS",
                        reason: DUPLICATE_WINDOW_RULE,
                    });
                }
                Duration::from_secs(seconds)
            }
        };

        let session = match trimmed(&mut source, "VEYRA_RISK_SESSION_HOURS_UTC") {
            None => None,
            Some(raw) => Some(SessionWindow::parse(&raw)?),
        };

        let max_risk_percent = percent(
            &mut source,
            "VEYRA_RISK_MAX_RISK_PERCENT",
            DEFAULT_MAX_RISK_PERCENT,
        )?;
        let max_daily_loss_percent = percent(
            &mut source,
            "VEYRA_RISK_MAX_DAILY_LOSS_PERCENT",
            DEFAULT_MAX_DAILY_LOSS_PERCENT,
        )?;
        let max_peak_drawdown_percent = percent(
            &mut source,
            "VEYRA_RISK_MAX_PEAK_DRAWDOWN_PERCENT",
            DEFAULT_MAX_PEAK_DRAWDOWN_PERCENT,
        )?;
        let max_net_factor_lots = match trimmed(&mut source, "VEYRA_RISK_MAX_NET_FACTOR_LOTS") {
            None => DEFAULT_MAX_NET_FACTOR_LOTS,
            Some(raw) => {
                let value = raw.parse::<f64>().map_err(|_| RiskError {
                    name: "VEYRA_RISK_MAX_NET_FACTOR_LOTS",
                    reason: FACTOR_LOTS_RULE,
                })?;
                if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                    return Err(RiskError {
                        name: "VEYRA_RISK_MAX_NET_FACTOR_LOTS",
                        reason: FACTOR_LOTS_RULE,
                    });
                }
                value
            }
        };

        let calendar_blackout_minutes =
            match trimmed(&mut source, "VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES") {
                None => DEFAULT_CALENDAR_BLACKOUT_MINUTES,
                Some(raw) => {
                    let minutes = raw.parse::<u64>().map_err(|_| RiskError {
                        name: "VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES",
                        reason: CALENDAR_BLACKOUT_RULE,
                    })?;
                    if minutes > MAX_CALENDAR_BLACKOUT_MINUTES {
                        return Err(RiskError {
                            name: "VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES",
                            reason: CALENDAR_BLACKOUT_RULE,
                        });
                    }
                    minutes
                }
            };

        let min_stop_atr_fraction = match trimmed(&mut source, "VEYRA_RISK_MIN_STOP_ATR_FRACTION") {
            None => DEFAULT_MIN_STOP_ATR_FRACTION,
            Some(raw) => {
                let value = raw.parse::<f64>().map_err(|_| RiskError {
                    name: "VEYRA_RISK_MIN_STOP_ATR_FRACTION",
                    reason: ATR_FRACTION_RULE,
                })?;
                if !value.is_finite() || !(0.0..=MAX_MIN_STOP_ATR_FRACTION).contains(&value) {
                    return Err(RiskError {
                        name: "VEYRA_RISK_MIN_STOP_ATR_FRACTION",
                        reason: ATR_FRACTION_RULE,
                    });
                }
                value
            }
        };

        Ok(Self::new(
            kill_switch,
            symbols,
            max_volume_per_order,
            max_total_lots,
            max_open_orders,
            duplicate_window,
            session,
        )
        .with_limits(
            max_risk_percent,
            max_daily_loss_percent,
            max_peak_drawdown_percent,
            max_net_factor_lots,
        )
        .with_calendar_blackout(calendar_blackout_minutes)
        .with_min_stop_atr_fraction(min_stop_atr_fraction))
    }

    /// Applies a partial update from the control surface, keeping every field
    /// the patch omits. Values are validated with the same rules as the
    /// environment parser, so a console edit can never widen behaviour beyond
    /// what a restart would accept.
    ///
    /// # Errors
    /// Returns [`RiskError`] naming the field that failed validation.
    pub fn apply_patch(&self, patch: &RiskPolicyPatch) -> Result<Self, RiskError> {
        let kill_switch = patch.kill_switch.unwrap_or(self.kill_switch);

        let symbols = match &patch.symbols {
            None => self.symbols.clone(),
            Some(entries) => parse_symbol_entries(entries)?,
        };

        let max_volume_per_order = match patch.max_volume_per_order {
            None => self.max_volume_per_order,
            Some(value) => Volume::parse(value).map_err(|error| RiskError {
                name: "maxVolumePerOrder",
                reason: error.reason,
            })?,
        };
        let max_total_lots = match patch.max_total_lots {
            None => self.max_total_lots,
            Some(value) => Volume::parse(value).map_err(|error| RiskError {
                name: "maxTotalLots",
                reason: error.reason,
            })?,
        };
        let max_open_orders = match patch.max_open_orders {
            None => self.max_open_orders,
            Some(value) if value <= MAX_OPEN_ORDERS_CEILING => value,
            Some(_) => {
                return Err(RiskError {
                    name: "maxOpenOrders",
                    reason: OPEN_ORDERS_RULE,
                });
            }
        };
        let duplicate_window = match patch.duplicate_window_secs {
            None => self.duplicate_window,
            Some(seconds) if seconds <= MAX_DUPLICATE_WINDOW_SECS => Duration::from_secs(seconds),
            Some(_) => {
                return Err(RiskError {
                    name: "duplicateWindowSecs",
                    reason: DUPLICATE_WINDOW_RULE,
                });
            }
        };
        let session = match &patch.session_utc {
            None => self.session,
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => Some(SessionWindow::parse(raw.trim()).map_err(|error| RiskError {
                name: "sessionUtc",
                reason: error.reason,
            })?),
        };
        let max_risk_percent = match patch.max_risk_percent {
            None => self.max_risk_percent,
            Some(value) => percent_value("maxRiskPercent", value)?,
        };
        let max_daily_loss_percent = match patch.max_daily_loss_percent {
            None => self.max_daily_loss_percent,
            Some(value) => percent_value("maxDailyLossPercent", value)?,
        };
        let max_peak_drawdown_percent = match patch.max_peak_drawdown_percent {
            None => self.max_peak_drawdown_percent,
            Some(value) => percent_value("maxPeakDrawdownPercent", value)?,
        };
        let max_net_factor_lots = match patch.max_net_factor_lots {
            None => self.max_net_factor_lots,
            Some(value) if value.is_finite() && (0.0..=100.0).contains(&value) => value,
            Some(_) => {
                return Err(RiskError {
                    name: "maxNetFactorLots",
                    reason: FACTOR_LOTS_RULE,
                });
            }
        };
        let calendar_blackout_minutes = match patch.calendar_blackout_minutes {
            None => self.calendar_blackout_minutes,
            Some(minutes) if minutes <= MAX_CALENDAR_BLACKOUT_MINUTES => minutes,
            Some(_) => {
                return Err(RiskError {
                    name: "calendarBlackoutMinutes",
                    reason: CALENDAR_BLACKOUT_RULE,
                });
            }
        };
        let min_stop_atr_fraction = match patch.min_stop_atr_fraction {
            None => self.min_stop_atr_fraction,
            Some(value)
                if value.is_finite() && (0.0..=MAX_MIN_STOP_ATR_FRACTION).contains(&value) =>
            {
                value
            }
            Some(_) => {
                return Err(RiskError {
                    name: "minStopAtrFraction",
                    reason: ATR_FRACTION_RULE,
                });
            }
        };

        Ok(Self::new(
            kill_switch,
            symbols,
            max_volume_per_order,
            max_total_lots,
            max_open_orders,
            duplicate_window,
            session,
        )
        .with_limits(
            max_risk_percent,
            max_daily_loss_percent,
            max_peak_drawdown_percent,
            max_net_factor_lots,
        )
        .with_calendar_blackout(calendar_blackout_minutes)
        .with_min_stop_atr_fraction(min_stop_atr_fraction))
    }

    /// Whether the kill switch is engaged; engaged means every intent fails.
    pub fn kill_switch(&self) -> bool {
        self.kill_switch
    }

    /// Configured instrument allowlist; empty means nothing is allowed.
    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    /// Largest lot volume a single intent may request.
    pub fn max_volume_per_order(&self) -> Volume {
        self.max_volume_per_order
    }

    /// Largest total open volume, across existing orders plus a new intent.
    pub fn max_total_lots(&self) -> Volume {
        self.max_total_lots
    }

    /// Largest number of open venue orders the gate tolerates.
    pub fn max_open_orders(&self) -> u32 {
        self.max_open_orders
    }

    /// Window inside which an identical approved draft is suppressed.
    pub fn duplicate_window(&self) -> Duration {
        self.duplicate_window
    }

    /// Optional UTC session window; absent means no session restriction.
    pub fn session(&self) -> Option<SessionWindow> {
        self.session
    }

    /// Per-trade risk cap as a percentage of equity; zero disables.
    pub fn max_risk_percent(&self) -> f64 {
        self.max_risk_percent
    }

    /// Daily-loss breaker as a percentage below the day's opening equity.
    pub fn max_daily_loss_percent(&self) -> f64 {
        self.max_daily_loss_percent
    }

    /// Peak-drawdown breaker as a percentage below the lifetime peak.
    pub fn max_peak_drawdown_percent(&self) -> f64 {
        self.max_peak_drawdown_percent
    }

    /// Cap on net USD-directional exposure in lots; zero disables.
    pub fn max_net_factor_lots(&self) -> f64 {
        self.max_net_factor_lots
    }

    /// Snapshot of the effective policy as an apply-able patch: every field is
    /// present, so applying it over any baseline reproduces this policy
    /// exactly. Used to persist operator edits across restarts.
    pub fn snapshot_patch(&self) -> RiskPolicyPatch {
        RiskPolicyPatch {
            kill_switch: Some(self.kill_switch),
            symbols: (!self.symbols.is_empty())
                .then(|| self.symbols.iter().map(|s| s.as_str().to_owned()).collect()),
            max_volume_per_order: Some(self.max_volume_per_order.value()),
            max_total_lots: Some(self.max_total_lots.value()),
            max_open_orders: Some(self.max_open_orders),
            duplicate_window_secs: Some(self.duplicate_window.as_secs()),
            session_utc: Some(
                self.session
                    .map(|session| format!("{}-{}", session.start_hour(), session.end_hour()))
                    .unwrap_or_default(),
            ),
            max_risk_percent: Some(self.max_risk_percent),
            max_daily_loss_percent: Some(self.max_daily_loss_percent),
            max_peak_drawdown_percent: Some(self.max_peak_drawdown_percent),
            max_net_factor_lots: Some(self.max_net_factor_lots),
            calendar_blackout_minutes: Some(self.calendar_blackout_minutes),
            min_stop_atr_fraction: Some(self.min_stop_atr_fraction),
        }
    }

    /// News blackout either side of a high-impact event, in minutes; zero
    /// disables the check.
    pub fn calendar_blackout_minutes(&self) -> u64 {
        self.calendar_blackout_minutes
    }

    /// Minimum stop distance as a fraction of ATR(14); zero disables the
    /// floor.
    pub fn min_stop_atr_fraction(&self) -> f64 {
        self.min_stop_atr_fraction
    }

    /// Bounded, non-sensitive snapshot of the effective policy.
    ///
    /// Recorded with the audit trail at startup so a decision can always be
    /// read against the exact rules that were in force when it was made.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "killSwitch": self.kill_switch,
            "symbols": self
                .symbols
                .iter()
                .map(|symbol| symbol.as_str())
                .collect::<Vec<_>>(),
            "maxVolumePerOrder": self.max_volume_per_order.value(),
            "maxTotalLots": self.max_total_lots.value(),
            "maxOpenOrders": self.max_open_orders,
            "duplicateWindowSecs": self.duplicate_window.as_secs(),
            "sessionUtc": self
                .session
                .map(|session| format!("{}-{}", session.start_hour(), session.end_hour())),
            "maxRiskPercent": self.max_risk_percent,
            "maxDailyLossPercent": self.max_daily_loss_percent,
            "maxPeakDrawdownPercent": self.max_peak_drawdown_percent,
            "maxNetFactorLots": self.max_net_factor_lots,
            "calendarBlackoutMinutes": self.calendar_blackout_minutes,
            "minStopAtrFraction": self.min_stop_atr_fraction
        })
    }

    /// Whether `symbol` is on the allowlist.
    pub fn allows_symbol(&self, symbol: &Symbol) -> bool {
        self.symbols.iter().any(|allowed| allowed == symbol)
    }
}

impl Default for RiskPolicy {
    /// The restrictive baseline: no instrument allowed, smallest volume, one
    /// open order, duplicate suppression on, no session restriction, and the
    /// operational valuation limits (per-trade risk 12%, daily loss 10%, peak
    /// drawdown 25%, 0.01-lot net USD-direction cap).
    fn default() -> Self {
        Self::new(
            false,
            Vec::new(),
            Volume::MINIMUM,
            Volume::MINIMUM,
            DEFAULT_MAX_OPEN_ORDERS,
            Duration::from_secs(DEFAULT_DUPLICATE_WINDOW_SECS),
            None,
        )
        .with_limits(
            DEFAULT_MAX_RISK_PERCENT,
            DEFAULT_MAX_DAILY_LOSS_PERCENT,
            DEFAULT_MAX_PEAK_DRAWDOWN_PERCENT,
            DEFAULT_MAX_NET_FACTOR_LOTS,
        )
        .with_calendar_blackout(DEFAULT_CALENDAR_BLACKOUT_MINUTES)
        .with_min_stop_atr_fraction(DEFAULT_MIN_STOP_ATR_FRACTION)
    }
}

/// Parses an optional percentage with a default; malformed values fail.
fn percent(
    source: &mut impl FnMut(&'static str) -> Option<String>,
    name: &'static str,
    default: f64,
) -> Result<f64, RiskError> {
    match trimmed(source, name) {
        None => Ok(default),
        Some(raw) => {
            let value = raw.parse::<f64>().map_err(|_| RiskError {
                name,
                reason: PERCENT_RULE,
            })?;
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err(RiskError {
                    name,
                    reason: PERCENT_RULE,
                });
            }
            Ok(value)
        }
    }
}

fn trimmed(
    source: &mut impl FnMut(&'static str) -> Option<String>,
    name: &'static str,
) -> Option<String> {
    source(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_symbols(raw: &str) -> Result<Vec<Symbol>, RiskError> {
    let invalid = |reason: &'static str| RiskError {
        name: "VEYRA_RISK_SYMBOLS",
        reason,
    };
    let mut symbols: Vec<Symbol> = Vec::new();
    for entry in raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let symbol = parse_instrument(entry).map_err(|error| invalid(error.reason))?;
        if !symbols.contains(&symbol) {
            symbols.push(symbol);
        }
    }
    if symbols.is_empty() || symbols.len() > MAX_SYMBOLS {
        return Err(invalid(SYMBOLS_RULE));
    }
    Ok(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::intent::Volume;

    #[test]
    fn policy_summaries_capture_the_effective_rules() {
        let summary = RiskPolicy::new(
            false,
            vec![Symbol::parse("EURUSD").expect("symbol")],
            Volume::parse(0.01).expect("volume"),
            Volume::parse(0.05).expect("volume"),
            1,
            Duration::from_secs(60),
            Some(SessionWindow::parse("7-21").expect("session")),
        )
        .summary();
        assert_eq!(summary["killSwitch"], false);
        assert_eq!(summary["symbols"], serde_json::json!(["EURUSD"]));
        assert_eq!(summary["maxVolumePerOrder"], 0.01);
        assert_eq!(summary["maxTotalLots"], 0.05);
        assert_eq!(summary["maxOpenOrders"], 1);
        assert_eq!(summary["duplicateWindowSecs"], 60);
        assert_eq!(summary["sessionUtc"], "7-21");
        assert_eq!(
            summary["calendarBlackoutMinutes"], 0,
            "the embedding baseline disables the news blackout"
        );
        assert_eq!(
            summary["minStopAtrFraction"], 0.0,
            "the embedding baseline disables the ATR floor"
        );

        let default = RiskPolicy::default().summary();
        assert_eq!(
            default["symbols"],
            serde_json::json!([]),
            "default denies all"
        );
        assert_eq!(default["sessionUtc"], serde_json::Value::Null);
        assert_eq!(
            default["calendarBlackoutMinutes"],
            DEFAULT_CALENDAR_BLACKOUT_MINUTES
        );
        assert_eq!(default["minStopAtrFraction"], DEFAULT_MIN_STOP_ATR_FRACTION);
    }

    fn source<'a>(
        pairs: &'a [(&'static str, &'a str)],
    ) -> impl FnMut(&'static str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn defaults_are_restrictive() {
        let policy = RiskPolicy::from_source(source(&[])).expect("defaults parse");
        assert!(!policy.kill_switch());
        assert!(policy.symbols().is_empty());
        assert_eq!(policy.max_volume_per_order().value(), 0.01);
        assert_eq!(policy.max_total_lots().value(), 0.01);
        assert_eq!(policy.max_open_orders(), 1);
        assert_eq!(policy.duplicate_window(), Duration::from_secs(60));
        assert_eq!(policy.session(), None);
        assert_eq!(policy.max_risk_percent(), DEFAULT_MAX_RISK_PERCENT);
        assert_eq!(
            policy.max_daily_loss_percent(),
            DEFAULT_MAX_DAILY_LOSS_PERCENT
        );
        assert_eq!(
            policy.max_peak_drawdown_percent(),
            DEFAULT_MAX_PEAK_DRAWDOWN_PERCENT
        );
        assert_eq!(policy.max_net_factor_lots(), DEFAULT_MAX_NET_FACTOR_LOTS);
        assert_eq!(
            policy.calendar_blackout_minutes(),
            DEFAULT_CALENDAR_BLACKOUT_MINUTES
        );
        assert_eq!(
            policy.min_stop_atr_fraction(),
            DEFAULT_MIN_STOP_ATR_FRACTION
        );
        assert!(!policy.allows_symbol(&parse_instrument("EURUSD").expect("symbol")));
        assert_eq!(RiskPolicy::default(), policy);
    }

    #[test]
    fn control_surface_patches_validate_with_env_rules() {
        let base = RiskPolicy::new(
            false,
            vec![Symbol::parse("EURUSD").expect("symbol")],
            Volume::parse(0.01).expect("volume"),
            Volume::parse(0.02).expect("volume"),
            5,
            Duration::from_secs(60),
            None,
        )
        .with_limits(12.0, 10.0, 25.0, 0.01);

        // A partial patch changes only what it names.
        let patched = base
            .apply_patch(&RiskPolicyPatch {
                kill_switch: Some(true),
                max_open_orders: Some(3),
                ..Default::default()
            })
            .expect("patch applies");
        assert!(patched.kill_switch());
        assert_eq!(patched.max_open_orders(), 3);
        assert_eq!(patched.symbols().len(), 1);
        assert_eq!(patched.max_risk_percent(), 12.0, "omitted fields survive");

        // Every field is settable, including clearing the session.
        let full = base
            .apply_patch(&RiskPolicyPatch {
                symbols: Some(vec!["eurusd".to_owned(), "GBPUSD".to_owned()]),
                max_volume_per_order: Some(0.05),
                max_total_lots: Some(0.1),
                duplicate_window_secs: Some(5),
                session_utc: Some("8-17".to_owned()),
                max_risk_percent: Some(3.0),
                max_daily_loss_percent: Some(0.0),
                max_peak_drawdown_percent: Some(40.0),
                max_net_factor_lots: Some(0.05),
                calendar_blackout_minutes: Some(15),
                min_stop_atr_fraction: Some(0.5),
                ..Default::default()
            })
            .expect("patch applies");
        assert_eq!(full.symbols().len(), 2);
        assert_eq!(full.max_volume_per_order().value(), 0.05);
        assert_eq!(full.max_total_lots().value(), 0.1);
        assert_eq!(full.duplicate_window(), Duration::from_secs(5));
        assert_eq!(
            full.session(),
            Some(SessionWindow::parse("8-17").expect("window"))
        );
        assert_eq!(full.max_risk_percent(), 3.0);
        assert_eq!(full.max_daily_loss_percent(), 0.0);
        assert_eq!(full.max_peak_drawdown_percent(), 40.0);
        assert_eq!(full.max_net_factor_lots(), 0.05);
        assert_eq!(full.calendar_blackout_minutes(), 15);
        assert_eq!(full.min_stop_atr_fraction(), 0.5);

        let cleared = full
            .apply_patch(&RiskPolicyPatch {
                session_utc: Some("   ".to_owned()),
                ..Default::default()
            })
            .expect("blank clears the session");
        assert_eq!(cleared.session(), None);

        // Rejections name the control-surface field.
        let cases: [(RiskPolicyPatch, &str); 10] = [
            (
                RiskPolicyPatch {
                    symbols: Some(Vec::new()),
                    ..Default::default()
                },
                "symbols",
            ),
            (
                RiskPolicyPatch {
                    symbols: Some(vec![String::new()]),
                    ..Default::default()
                },
                "symbols",
            ),
            (
                RiskPolicyPatch {
                    max_volume_per_order: Some(0.0),
                    ..Default::default()
                },
                "maxVolumePerOrder",
            ),
            (
                RiskPolicyPatch {
                    max_open_orders: Some(1_001),
                    ..Default::default()
                },
                "maxOpenOrders",
            ),
            (
                RiskPolicyPatch {
                    duplicate_window_secs: Some(86_401),
                    ..Default::default()
                },
                "duplicateWindowSecs",
            ),
            (
                RiskPolicyPatch {
                    max_risk_percent: Some(101.0),
                    ..Default::default()
                },
                "maxRiskPercent",
            ),
            (
                RiskPolicyPatch {
                    max_net_factor_lots: Some(-1.0),
                    ..Default::default()
                },
                "maxNetFactorLots",
            ),
            (
                RiskPolicyPatch {
                    session_utc: Some("24-3".to_owned()),
                    ..Default::default()
                },
                "sessionUtc",
            ),
            (
                RiskPolicyPatch {
                    calendar_blackout_minutes: Some(1_441),
                    ..Default::default()
                },
                "calendarBlackoutMinutes",
            ),
            (
                RiskPolicyPatch {
                    min_stop_atr_fraction: Some(2.5),
                    ..Default::default()
                },
                "minStopAtrFraction",
            ),
        ];
        for (patch, field) in cases {
            let error = base.apply_patch(&patch).expect_err(field);
            assert_eq!(error.name, field);
        }
    }

    #[test]
    fn every_setting_is_parsed_and_lowercase_symbols_are_normalised() {
        let policy = RiskPolicy::from_source(source(&[
            ("VEYRA_RISK_KILL_SWITCH", "true"),
            ("VEYRA_RISK_SYMBOLS", " eurusd , EURUSD,gbpusd ,, "),
            ("VEYRA_RISK_MAX_VOLUME_PER_ORDER", "0.25"),
            ("VEYRA_RISK_MAX_TOTAL_LOTS", "0.05"),
            ("VEYRA_RISK_MAX_OPEN_ORDERS", "7"),
            ("VEYRA_RISK_DUPLICATE_WINDOW_SECS", "5"),
            ("VEYRA_RISK_SESSION_HOURS_UTC", "8-17"),
            ("VEYRA_RISK_MAX_RISK_PERCENT", "3.5"),
            ("VEYRA_RISK_MAX_DAILY_LOSS_PERCENT", "0"),
            ("VEYRA_RISK_MAX_PEAK_DRAWDOWN_PERCENT", "40"),
            ("VEYRA_RISK_MAX_NET_FACTOR_LOTS", "0.05"),
            ("VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES", "45"),
            ("VEYRA_RISK_MIN_STOP_ATR_FRACTION", "0.75"),
        ]))
        .expect("valid settings");

        assert!(policy.kill_switch());
        assert_eq!(policy.symbols().len(), 2);
        assert!(policy.allows_symbol(&parse_instrument("gbpusd").expect("symbol")));
        assert_eq!(policy.max_volume_per_order().value(), 0.25);
        assert_eq!(policy.max_total_lots().value(), 0.05);
        assert_eq!(policy.max_open_orders(), 7);
        assert_eq!(policy.duplicate_window(), Duration::from_secs(5));
        assert_eq!(
            policy.session(),
            Some(SessionWindow::parse("8-17").expect("window"))
        );
        assert_eq!(policy.max_risk_percent(), 3.5);
        assert_eq!(policy.max_daily_loss_percent(), 0.0, "zero disables");
        assert_eq!(policy.max_peak_drawdown_percent(), 40.0);
        assert_eq!(policy.max_net_factor_lots(), 0.05);
        assert_eq!(policy.calendar_blackout_minutes(), 45);
        assert_eq!(policy.min_stop_atr_fraction(), 0.75);
    }

    #[test]
    fn malformed_settings_are_rejected_by_name() {
        let cases: [(&'static str, &'static str); 18] = [
            ("VEYRA_RISK_KILL_SWITCH", "yes"),
            ("VEYRA_RISK_SYMBOLS", "not a symbol!,EURUSD"),
            ("VEYRA_RISK_MAX_VOLUME_PER_ORDER", "0"),
            ("VEYRA_RISK_MAX_VOLUME_PER_ORDER", "101"),
            ("VEYRA_RISK_MAX_VOLUME_PER_ORDER", "lots"),
            ("VEYRA_RISK_MAX_TOTAL_LOTS", "0"),
            ("VEYRA_RISK_MAX_TOTAL_LOTS", "lots"),
            ("VEYRA_RISK_MAX_OPEN_ORDERS", "1001"),
            ("VEYRA_RISK_DUPLICATE_WINDOW_SECS", "86401"),
            ("VEYRA_RISK_SESSION_HOURS_UTC", "24-3"),
            ("VEYRA_RISK_MAX_RISK_PERCENT", "101"),
            ("VEYRA_RISK_MAX_DAILY_LOSS_PERCENT", "soon"),
            ("VEYRA_RISK_MAX_PEAK_DRAWDOWN_PERCENT", "-1"),
            ("VEYRA_RISK_MAX_NET_FACTOR_LOTS", "lots"),
            ("VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES", "1441"),
            ("VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES", "soon"),
            ("VEYRA_RISK_MIN_STOP_ATR_FRACTION", "2.5"),
            ("VEYRA_RISK_MIN_STOP_ATR_FRACTION", "tight"),
        ];
        for (name, value) in cases {
            let error =
                RiskPolicy::from_source(source(&[(name, value)])).expect_err("must be rejected");
            assert_eq!(error.name, name, "{name}={value}");
        }

        let empty_window =
            RiskPolicy::from_source(source(&[("VEYRA_RISK_SESSION_HOURS_UTC", "5-5")]))
                .expect_err("identical bounds must fail");
        assert_eq!(empty_window.reason, SESSION_RULE);

        let too_many = (0..65)
            .map(|index| format!("S{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let error = RiskPolicy::from_source(source(&[("VEYRA_RISK_SYMBOLS", &too_many)]))
            .expect_err("allowlist cap must fail");
        assert_eq!(error.name, "VEYRA_RISK_SYMBOLS");
    }

    #[test]
    fn snapshot_patches_reproduce_a_policy_after_a_restart() {
        let policy = RiskPolicy::from_source(source(&[
            ("VEYRA_RISK_KILL_SWITCH", "true"),
            ("VEYRA_RISK_SYMBOLS", "EURUSD,AUDUSD"),
            ("VEYRA_RISK_MAX_VOLUME_PER_ORDER", "0.03"),
            ("VEYRA_RISK_MAX_TOTAL_LOTS", "0.05"),
            ("VEYRA_RISK_MAX_OPEN_ORDERS", "5"),
            ("VEYRA_RISK_DUPLICATE_WINDOW_SECS", "5"),
            ("VEYRA_RISK_SESSION_HOURS_UTC", "8-17"),
            ("VEYRA_RISK_MAX_RISK_PERCENT", "3.5"),
            ("VEYRA_RISK_MAX_DAILY_LOSS_PERCENT", "0"),
            ("VEYRA_RISK_MAX_PEAK_DRAWDOWN_PERCENT", "40"),
            ("VEYRA_RISK_MAX_NET_FACTOR_LOTS", "0.05"),
            ("VEYRA_RISK_CALENDAR_BLACKOUT_MINUTES", "45"),
            ("VEYRA_RISK_MIN_STOP_ATR_FRACTION", "0.75"),
        ]))
        .expect("valid settings");

        // The snapshot serializes with the control-surface field names and
        // applies over a fresh baseline to the same effective policy.
        let value = serde_json::to_value(policy.snapshot_patch()).expect("snapshot serializes");
        let patch: RiskPolicyPatch = serde_json::from_value(value).expect("snapshot deserializes");
        let rebuilt = RiskPolicy::default()
            .apply_patch(&patch)
            .expect("snapshot applies");
        assert_eq!(rebuilt.summary(), policy.summary());
    }

    #[test]
    fn empty_values_fall_back_to_defaults() {
        let policy = RiskPolicy::from_source(source(&[
            ("VEYRA_RISK_SYMBOLS", "  "),
            ("VEYRA_RISK_KILL_SWITCH", ""),
        ]))
        .expect("blank values behave as unset");
        assert!(policy.symbols().is_empty());
        assert!(!policy.kill_switch());
    }

    #[test]
    fn session_windows_include_start_and_exclude_end() {
        let day = SessionWindow::parse("7-21").expect("window");
        assert!(day.contains(7));
        assert!(day.contains(20));
        assert!(!day.contains(21));
        assert!(!day.contains(6));
        assert_eq!(day.start_hour(), 7);
        assert_eq!(day.end_hour(), 21);

        let night = SessionWindow::parse("22-6").expect("window");
        assert!(night.contains(22));
        assert!(night.contains(23));
        assert!(night.contains(0));
        assert!(night.contains(5));
        assert!(!night.contains(6));
        assert!(!night.contains(12));

        assert!(SessionWindow::parse("7").is_err());
        assert!(SessionWindow::parse("a-b").is_err());
        assert!(SessionWindow::parse("25-2").is_err());
    }
}
