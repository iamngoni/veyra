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

pub use gate::{AccountFacts, RiskCode, RiskDecision, RiskGate, RiskRejection};

use std::time::Duration;

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

const SYMBOLS_RULE: &str = "must list 1-64 comma-separated instrument symbols";
const VOLUME_RULE: &str = "must be a finite number greater than 0 and at most 100";
const OPEN_ORDERS_RULE: &str = "must be an integer from 0 through 1000";
const DUPLICATE_WINDOW_RULE: &str = "must be an integer number of seconds from 0 through 86400";
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
        }
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

        Ok(Self::new(
            kill_switch,
            symbols,
            max_volume_per_order,
            max_total_lots,
            max_open_orders,
            duplicate_window,
            session,
        ))
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
                .map(|session| format!("{}-{}", session.start_hour(), session.end_hour()))
        })
    }

    /// Whether `symbol` is on the allowlist.
    pub fn allows_symbol(&self, symbol: &Symbol) -> bool {
        self.symbols.iter().any(|allowed| allowed == symbol)
    }
}

impl Default for RiskPolicy {
    /// The restrictive baseline: no instrument allowed, smallest volume, one
    /// open order, duplicate suppression on, no session restriction.
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

        let default = RiskPolicy::default().summary();
        assert_eq!(
            default["symbols"],
            serde_json::json!([]),
            "default denies all"
        );
        assert_eq!(default["sessionUtc"], serde_json::Value::Null);
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
        assert!(!policy.allows_symbol(&parse_instrument("EURUSD").expect("symbol")));
        assert_eq!(RiskPolicy::default(), policy);
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
    }

    #[test]
    fn malformed_settings_are_rejected_by_name() {
        let cases: [(&'static str, &'static str); 10] = [
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
