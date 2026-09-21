//! Autonomous trader loop.
//!
//! One tick gathers validated market data, optionally asks Jev for calibrated
//! judgements, asks the configured decision engine for a structured proposal,
//! and routes it through the deterministic risk gate and the same staged
//! execution path the control surface uses. The loop never widens behavior:
//! every missing input skips the tick, the gate owns approval, and execution
//! still requires the operator switch plus the terminal's own arming.
//!
//! Disabled by default (`VEYRA_AUTOPILOT_ENABLED`). The supervised process
//! runs the first tick one interval after startup, so a restart never fires
//! immediately. Every autonomous entry must carry both a stop loss and a take
//! profit; an unbracketed proposal is rejected before the command layer sees
//! it.
//!
//! While a Veyra-managed position is open the tick reviews it instead of
//! looking for entries: the model may `hold` (the bracket stands) or `close`
//! (flatten). Autonomous closes are risk-reducing but never instant: the
//! ticket must match a reviewed managed position, the shared staged close
//! re-validates it against the latest snapshot, and a position younger than
//! `VEYRA_AUTOPILOT_MIN_HOLD_SECS` — or one whose age cannot be verified — is
//! refused so the loop cannot churn in and out of the same trade.
//!
//! Before any review, deterministic stop policies run: break-even
//! (`VEYRA_AUTOPILOT_BREAKEVEN_R`) and trailing (`VEYRA_AUTOPILOT_TRAIL_R`),
//! expressed as multiples of the entry risk. The most protective candidate
//! wins, stops only ever move in the favourable direction, and improvements
//! smaller than a tenth of the entry risk are suppressed to bound churn.
//! Optional profit harvesting adds a spread-and-cost-aware high-water mark:
//! it ratchets the broker stop before TP, banks a still-positive retracement,
//! and requires both a cooldown and fresh market movement before re-entry.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::AppState;
use crate::audit::{AuditEvent, AuditKind};
use crate::broker::Symbol;
use crate::broker::{CommandKind, ORDER_MAGIC, PositionPayload, SymbolSpecPayload};
use crate::calendar::{self, CalendarEvent};
use crate::config::ConfigError;
use crate::control::{
    StagedClose, StagedExecution, StagedModify, queue_staged_close, queue_staged_modify,
    queue_staged_order,
};
use crate::jev::{
    Answer, ChoiceOptions, Instructions, JevRequest, JevRuntime, NoulCriteria, Question,
    ScoreLevels, State as JevState,
};
use crate::market::{Candle, CandleRequest, CandleSeries, Timeframe};
use crate::model::ModelTier;
use crate::risk::window::WeekendPrep;
use crate::risk::{AccountFacts, RiskPolicy, WeekendPositions};
use crate::trading::agent::{self, AgentDecision, AgentMode, AgentSession};
use crate::trading::contract;
use crate::trading::intent::TradeIntentDraft;
use crate::trading::pipeline::{PipelineError, PipelineOutcome};

/// Default proposal cadence in seconds.
const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Largest number of instruments one autopilot rotation accepts.
const MAX_SYMBOLS: usize = 16;
/// Smallest accepted cadence: frequent enough to act, slow enough to be sane.
const MIN_INTERVAL_SECS: u64 = 30;
/// Largest accepted cadence.
const MAX_INTERVAL_SECS: u64 = 86_400;
/// Default closed-candle window handed to the providers.
const DEFAULT_BARS: u16 = 48;
/// Smallest candle window that carries any context.
const MIN_BARS: u16 = 10;
/// How many recent candles are embedded in the model input.
const RECENT_CANDLES: usize = 12;
/// True-range window used for the ATR context handed to the model.
const ATR_PERIOD: usize = 14;
/// How far ahead the model sees scheduled events, in seconds.
const CALENDAR_HORIZON_SECS: i64 = 86_400;
/// How far back events are fetched so a late tick still sees a recent print.
const CALENDAR_LOOKBACK_SECS: i64 = 86_400;
/// Largest number of scheduled events listed per asset.
const MAX_LISTED_EVENTS: usize = 6;
/// Default minimum position age before an autonomous close is allowed.
const DEFAULT_MIN_HOLD_SECS: u64 = 300;
/// Default mid-candle trigger: a quarter of the instrument's own ATR.
const DEFAULT_ENTRY_MOVE_ATR_FRACTION: f64 = 0.25;
/// Earliest favourable move that arms profit harvesting, in entry-risk units.
const DEFAULT_HARVEST_ARM_R: f64 = 0.2;
/// Distance kept behind favourable price after harvesting arms.
const DEFAULT_HARVEST_TRAIL_R: f64 = 0.2;
/// Smallest spread-adjusted floating profit that can arm harvesting.
const DEFAULT_HARVEST_MIN_PROFIT: f64 = 0.5;
/// Fraction of the best floating profit surrendered before a direct close.
const DEFAULT_HARVEST_GIVEBACK: f64 = 0.35;
/// Minimum position age before profit harvesting may act.
const DEFAULT_HARVEST_MIN_HOLD_SECS: u64 = 300;
/// Quiet period after a position disappears before its symbol may re-enter.
const DEFAULT_HARVEST_REENTRY_COOLDOWN_SECS: u64 = 900;
/// Largest minimum-hold window the parser accepts.
const MAX_MIN_HOLD_SECS: u64 = 86_400;

/// Validated deterministic policy for banking profit before the original take
/// profit while preventing immediate same-signal re-entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfitHarvestPolicy {
    arm_r: f64,
    trail_r: f64,
    min_profit: f64,
    giveback_fraction: f64,
    min_hold: Duration,
    reentry_cooldown: Duration,
}

impl ProfitHarvestPolicy {
    /// Favourable move, in original entry-risk units, required to arm.
    pub fn arm_r(&self) -> f64 {
        self.arm_r
    }

    /// Entry-risk distance kept behind the best favourable price.
    pub fn trail_r(&self) -> f64 {
        self.trail_r
    }

    /// Minimum live net profit in account currency required to arm.
    pub fn min_profit(&self) -> f64 {
        self.min_profit
    }

    /// Fraction of the high-water profit whose surrender queues a close.
    pub fn giveback_fraction(&self) -> f64 {
        self.giveback_fraction
    }

    /// Minimum verified broker age before harvesting may act.
    pub fn min_hold(&self) -> Duration {
        self.min_hold
    }

    /// Minimum quiet period before the same symbol can be proposed again.
    pub fn reentry_cooldown(&self) -> Duration {
        self.reentry_cooldown
    }
}

/// Whether the loop consults the configured judgement engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevPreference {
    /// Consult Jev when it is configured; skip silently when it is not.
    Auto,
    /// Never consult Jev, even when configured.
    Off,
}

impl JevPreference {
    /// Stable name used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }

    /// Parses a configuration value; unknown values are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "true" => Some(Self::Auto),
            "off" | "false" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Validated autopilot configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct AutopilotSettings {
    enabled: bool,
    symbols: Vec<Symbol>,
    timeframe: Timeframe,
    bars: u16,
    tier: ModelTier,
    interval: Duration,
    jev: JevPreference,
    min_hold: Duration,
    breakeven_r: f64,
    trail_r: f64,
    entry_move_atr_fraction: f64,
    profit_harvest: Option<ProfitHarvestPolicy>,
}

impl AutopilotSettings {
    /// Reads autopilot settings from the process environment.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for malformed settings.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_source(|name| {
            std::env::var(name).map_err(|_| ConfigError::MissingEnvironmentVariable { name })
        })
    }

    /// Parses an injected settings source. Absent variables default and
    /// disable the loop; any present variable is validated strictly.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when a present value is malformed.
    pub fn from_source(
        mut source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> Result<Option<Self>, ConfigError> {
        let enabled_raw = optional(&mut source, "VEYRA_AUTOPILOT_ENABLED");
        let symbol_raw = optional(&mut source, "VEYRA_AUTOPILOT_SYMBOL");
        let symbols_raw = optional(&mut source, "VEYRA_AUTOPILOT_SYMBOLS");
        let timeframe_raw = optional(&mut source, "VEYRA_AUTOPILOT_TIMEFRAME");
        let bars_raw = optional(&mut source, "VEYRA_AUTOPILOT_BARS");
        let tier_raw = optional(&mut source, "VEYRA_AUTOPILOT_TIER");
        let interval_raw = optional(&mut source, "VEYRA_AUTOPILOT_INTERVAL_SECS");
        let jev_raw = optional(&mut source, "VEYRA_AUTOPILOT_JEV");
        let min_hold_raw = optional(&mut source, "VEYRA_AUTOPILOT_MIN_HOLD_SECS");
        let breakeven_raw = optional(&mut source, "VEYRA_AUTOPILOT_BREAKEVEN_R");
        let trail_raw = optional(&mut source, "VEYRA_AUTOPILOT_TRAIL_R");
        let entry_move_raw = optional(&mut source, "VEYRA_AUTOPILOT_ENTRY_MOVE_ATR");
        let harvest_enabled_raw = optional(&mut source, "VEYRA_AUTOPILOT_PROFIT_HARVEST");
        let harvest_arm_raw = optional(&mut source, "VEYRA_AUTOPILOT_HARVEST_ARM_R");
        let harvest_trail_raw = optional(&mut source, "VEYRA_AUTOPILOT_HARVEST_TRAIL_R");
        let harvest_min_profit_raw = optional(&mut source, "VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT");
        let harvest_giveback_raw = optional(&mut source, "VEYRA_AUTOPILOT_HARVEST_GIVEBACK");
        let harvest_min_hold_raw = optional(&mut source, "VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS");
        let harvest_reentry_raw =
            optional(&mut source, "VEYRA_AUTOPILOT_HARVEST_REENTRY_COOLDOWN_SECS");

        if enabled_raw.is_empty()
            && symbol_raw.is_empty()
            && symbols_raw.is_empty()
            && timeframe_raw.is_empty()
            && bars_raw.is_empty()
            && tier_raw.is_empty()
            && interval_raw.is_empty()
            && jev_raw.is_empty()
            && min_hold_raw.is_empty()
            && breakeven_raw.is_empty()
            && trail_raw.is_empty()
            && entry_move_raw.is_empty()
            && harvest_enabled_raw.is_empty()
            && harvest_arm_raw.is_empty()
            && harvest_trail_raw.is_empty()
            && harvest_min_profit_raw.is_empty()
            && harvest_giveback_raw.is_empty()
            && harvest_min_hold_raw.is_empty()
            && harvest_reentry_raw.is_empty()
        {
            return Ok(None);
        }

        let enabled = match enabled_raw.as_str() {
            "" | "false" => false,
            "true" => true,
            _ => {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_ENABLED",
                    reason: "must be `true` or `false`",
                });
            }
        };
        if !symbol_raw.is_empty() && !symbols_raw.is_empty() {
            return Err(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_AUTOPILOT_SYMBOLS",
                reason: "set either VEYRA_AUTOPILOT_SYMBOL or VEYRA_AUTOPILOT_SYMBOLS, not both",
            });
        }
        let symbols = if !symbols_raw.is_empty() {
            parse_symbol_list(&symbols_raw)?
        } else if !symbol_raw.is_empty() {
            vec![Symbol::parse(&symbol_raw).map_err(|_| {
                ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_SYMBOL",
                    reason: "must be 1-24 characters of letters, digits, '.', '_', '#', '+' or '-'",
                }
            })?]
        } else {
            Vec::new()
        };
        let timeframe = match timeframe_raw.as_str() {
            "" => Timeframe::H4,
            other => Timeframe::parse(other).ok_or(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_AUTOPILOT_TIMEFRAME",
                reason: "must be a timeframe name (M1, M5, M15, M30, H1, H4, D1, W1, MN1) or minutes",
            })?,
        };
        let bars = match bars_raw.as_str() {
            "" => DEFAULT_BARS,
            other => {
                let invalid = || ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_BARS",
                    reason: "must be an integer from 10 through 240",
                };
                let bars = other.parse::<u16>().map_err(|_| invalid())?;
                if !(MIN_BARS..=240).contains(&bars) {
                    return Err(invalid());
                }
                bars
            }
        };
        let tier = match tier_raw.as_str() {
            "" => ModelTier::Balanced,
            other => ModelTier::parse(other).ok_or(ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_AUTOPILOT_TIER",
                reason: "must be fast, balanced, or reasoning",
            })?,
        };
        let interval = match interval_raw.as_str() {
            "" => Duration::from_secs(DEFAULT_INTERVAL_SECS),
            other => {
                let invalid = || ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_INTERVAL_SECS",
                    reason: "must be an integer number of seconds from 30 through 86400",
                };
                let secs = other.parse::<u64>().map_err(|_| invalid())?;
                if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs) {
                    return Err(invalid());
                }
                Duration::from_secs(secs)
            }
        };
        let jev = match jev_raw.as_str() {
            "" => JevPreference::Auto,
            other => {
                JevPreference::parse(other).ok_or(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_JEV",
                    reason: "must be `auto` or `off`",
                })?
            }
        };
        let min_hold = match min_hold_raw.as_str() {
            "" => Duration::from_secs(DEFAULT_MIN_HOLD_SECS),
            other => {
                let invalid = || ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_MIN_HOLD_SECS",
                    reason: "must be an integer number of seconds from 0 through 86400",
                };
                let secs = other.parse::<u64>().map_err(|_| invalid())?;
                if secs > MAX_MIN_HOLD_SECS {
                    return Err(invalid());
                }
                Duration::from_secs(secs)
            }
        };

        let multiple = |name: &'static str, raw: &str| -> Result<f64, ConfigError> {
            match raw {
                "" => Ok(0.0),
                other => {
                    let invalid = || ConfigError::InvalidEnvironmentVariable {
                        name,
                        reason: "must be a number from 0 through 10 (0 disables the policy)",
                    };
                    let value = other.parse::<f64>().map_err(|_| invalid())?;
                    if !value.is_finite() || !(0.0..=10.0).contains(&value) {
                        return Err(invalid());
                    }
                    Ok(value)
                }
            }
        };
        let breakeven_r = multiple("VEYRA_AUTOPILOT_BREAKEVEN_R", &breakeven_raw)?;
        let trail_r = multiple("VEYRA_AUTOPILOT_TRAIL_R", &trail_raw)?;
        // Unset keeps the same floor the stop checks already use, so a
        // mid-candle move has to be meaningful by the instrument's own measure
        // before it buys another opinion.
        let entry_move_atr_fraction = match entry_move_raw.as_str() {
            "" => DEFAULT_ENTRY_MOVE_ATR_FRACTION,
            other => multiple("VEYRA_AUTOPILOT_ENTRY_MOVE_ATR", other)?,
        };
        let harvest_values_present = [
            &harvest_arm_raw,
            &harvest_trail_raw,
            &harvest_min_profit_raw,
            &harvest_giveback_raw,
            &harvest_min_hold_raw,
            &harvest_reentry_raw,
        ]
        .iter()
        .any(|value| !value.is_empty());
        let harvest_enabled = match harvest_enabled_raw.as_str() {
            "" | "false" if !harvest_values_present => false,
            "true" => true,
            "" | "false" => {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_PROFIT_HARVEST",
                    reason: "must be `true` when profit-harvest settings are present",
                });
            }
            _ => {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_PROFIT_HARVEST",
                    reason: "must be `true` or `false`",
                });
            }
        };
        let profit_harvest = if harvest_enabled {
            let positive = |name: &'static str,
                            raw: &str,
                            default: f64,
                            maximum: f64|
             -> Result<f64, ConfigError> {
                let value = if raw.is_empty() {
                    default
                } else {
                    raw.parse::<f64>()
                        .map_err(|_| ConfigError::InvalidEnvironmentVariable {
                            name,
                            reason: "must be a finite positive number within the documented bound",
                        })?
                };
                if !value.is_finite() || value <= 0.0 || value > maximum {
                    return Err(ConfigError::InvalidEnvironmentVariable {
                        name,
                        reason: "must be a finite positive number within the documented bound",
                    });
                }
                Ok(value)
            };
            let seconds =
                |name: &'static str, raw: &str, default: u64| -> Result<Duration, ConfigError> {
                    let value = if raw.is_empty() {
                        default
                    } else {
                        raw.parse::<u64>()
                            .map_err(|_| ConfigError::InvalidEnvironmentVariable {
                                name,
                                reason: "must be an integer number of seconds from 0 through 86400",
                            })?
                    };
                    if value > MAX_MIN_HOLD_SECS {
                        return Err(ConfigError::InvalidEnvironmentVariable {
                            name,
                            reason: "must be an integer number of seconds from 0 through 86400",
                        });
                    }
                    Ok(Duration::from_secs(value))
                };
            let arm_r = positive(
                "VEYRA_AUTOPILOT_HARVEST_ARM_R",
                &harvest_arm_raw,
                DEFAULT_HARVEST_ARM_R,
                10.0,
            )?;
            let trail_r = positive(
                "VEYRA_AUTOPILOT_HARVEST_TRAIL_R",
                &harvest_trail_raw,
                DEFAULT_HARVEST_TRAIL_R,
                10.0,
            )?;
            if trail_r > arm_r {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_HARVEST_TRAIL_R",
                    reason: "must be less than or equal to VEYRA_AUTOPILOT_HARVEST_ARM_R",
                });
            }
            let min_profit = positive(
                "VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT",
                &harvest_min_profit_raw,
                DEFAULT_HARVEST_MIN_PROFIT,
                1_000_000.0,
            )?;
            let giveback_fraction = positive(
                "VEYRA_AUTOPILOT_HARVEST_GIVEBACK",
                &harvest_giveback_raw,
                DEFAULT_HARVEST_GIVEBACK,
                0.95,
            )?;
            if giveback_fraction < 0.05 {
                return Err(ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_HARVEST_GIVEBACK",
                    reason: "must be a number from 0.05 through 0.95",
                });
            }
            Some(ProfitHarvestPolicy {
                arm_r,
                trail_r,
                min_profit,
                giveback_fraction,
                min_hold: seconds(
                    "VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS",
                    &harvest_min_hold_raw,
                    DEFAULT_HARVEST_MIN_HOLD_SECS,
                )?,
                reentry_cooldown: seconds(
                    "VEYRA_AUTOPILOT_HARVEST_REENTRY_COOLDOWN_SECS",
                    &harvest_reentry_raw,
                    DEFAULT_HARVEST_REENTRY_COOLDOWN_SECS,
                )?,
            })
        } else {
            None
        };

        Ok(Some(Self {
            enabled,
            symbols,
            timeframe,
            bars,
            tier,
            interval,
            jev,
            min_hold,
            breakeven_r,
            trail_r,
            entry_move_atr_fraction,
            profit_harvest,
        }))
    }

    /// Whether the loop runs at all.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Configured instruments, in rotation order; empty means the terminal's
    /// chart symbol. Open Veyra positions are appended at tick time so their
    /// lifecycle is managed even when they are outside the configured list.
    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    /// Timeframe for market data and judgements.
    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Closed candles per tick.
    pub fn bars(&self) -> u16 {
        self.bars
    }

    /// Capability tier used for proposals.
    pub fn tier(&self) -> ModelTier {
        self.tier
    }

    /// Delay between ticks.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether Jev is consulted when configured.
    pub fn jev(&self) -> JevPreference {
        self.jev
    }

    /// Minimum position age before the loop may close it; zero disables the
    /// guard, which is only appropriate in tests.
    pub fn min_hold(&self) -> Duration {
        self.min_hold
    }

    /// Multiple of the entry risk at which the stop moves to break-even;
    /// zero disables the policy.
    pub fn breakeven_r(&self) -> f64 {
        self.breakeven_r
    }

    /// Fraction of ATR a candidate must move inside a candle before the entry
    /// sweep asks the model again; zero leaves new candles as the only trigger.
    pub fn entry_move_atr_fraction(&self) -> f64 {
        self.entry_move_atr_fraction
    }

    /// Distance kept behind the best favourable price once trailing starts,
    /// as a multiple of the entry risk; zero disables the policy.
    pub fn trail_r(&self) -> f64 {
        self.trail_r
    }

    /// Deterministic early-profit policy, when explicitly enabled.
    pub fn profit_harvest(&self) -> Option<&ProfitHarvestPolicy> {
        self.profit_harvest.as_ref()
    }
}

fn optional(
    source: &mut impl FnMut(&'static str) -> Result<String, ConfigError>,
    name: &'static str,
) -> String {
    source(name)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// One decision cycle's outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum TickOutcome {
    /// The tick did not reach a decision (disabled, missing integration,
    /// stale link, unavailable account facts).
    Skipped {
        /// Stable reason for logs and tests.
        reason: &'static str,
    },
    /// A provider failed; nothing was decided.
    Unavailable {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// The model proposed no trade.
    NoTrade,
    /// No candidate moved since the last proposal, so none was requested.
    /// Stops and reviews still ran; only the entry question was skipped.
    Unchanged,
    /// The reviewer chose to keep the open position and its bracket.
    Held,
    /// The reviewer asked to close; the close command was queued.
    CloseQueued {
        /// Identifier of the queued close command.
        command: String,
    },
    /// The stop moved to break-even; the modify command was queued.
    StopMoved {
        /// Identifier of the queued modify command.
        command: String,
    },
    /// The gate or the stop policy rejected the proposal.
    Rejected {
        /// Stable rejection code.
        code: &'static str,
    },
    /// Approved while the service switch is off: recorded, nothing queued.
    ApprovedDryRun,
    /// Approved and queued for the terminal.
    Queued {
        /// Identifier of the queued command.
        command: String,
    },
}

/// Runs one decision cycle. Never panics on provider failure: every external
/// call degrades to an audited outcome or a skip.
#[tracing::instrument(skip_all, name = "autopilot.tick", fields(symbols = tracing::field::Empty))]
pub async fn tick(state: &AppState) -> TickOutcome {
    tick_inner(state, true).await
}

/// Runs the model/market decision cycle while leaving deterministic position
/// management to its independent cadence. Production uses this so slow market
/// providers cannot delay profit protection on the already-open book.
#[tracing::instrument(
    skip_all,
    name = "autopilot.decision",
    fields(symbols = tracing::field::Empty)
)]
pub async fn decision_tick(state: &AppState) -> TickOutcome {
    tick_inner(state, false).await
}

async fn tick_inner(state: &AppState, manage_positions: bool) -> TickOutcome {
    let Some(settings) = state.autopilot() else {
        return TickOutcome::Skipped {
            reason: "not_configured",
        };
    };
    if !settings.enabled() {
        return TickOutcome::Skipped { reason: "disabled" };
    }
    let Some(broker) = state.broker() else {
        return TickOutcome::Skipped {
            reason: "no_broker",
        };
    };

    let report = broker.link().report().await;
    if !report.fresh {
        return TickOutcome::Skipped {
            reason: "stale_link",
        };
    }
    if manage_positions
        && let Some(outcome) = manage_open_positions_after_link(state, settings).await
    {
        return outcome;
    }

    let managed = managed_positions(state);
    let snapshot = broker.link().last_account();
    let server_time = snapshot
        .as_ref()
        .map(|snapshot| snapshot.server_time)
        .unwrap_or(0);

    let Some(model) = state.model() else {
        return TickOutcome::Skipped { reason: "no_model" };
    };
    let Some(market) = state.market() else {
        return TickOutcome::Skipped {
            reason: "no_market",
        };
    };
    // Candidate menu: configured symbols first, then any symbol carrying an
    // open Veyra position (so every managed position stays managed), capped by
    // the settings parser. With nothing configured, the chart symbol is the
    // whole menu.
    let mut candidates = candidate_symbols(settings.symbols(), &managed);
    if candidates.is_empty()
        && let Some(snapshot) = report.snapshot.as_ref()
    {
        candidates.push(snapshot.symbol().clone());
    }
    if candidates.is_empty() {
        return TickOutcome::Skipped {
            reason: "symbol_unavailable",
        };
    }
    let menu = candidates
        .iter()
        .map(|symbol| symbol.as_str())
        .collect::<Vec<_>>()
        .join(",");
    tracing::Span::current().record("symbols", menu.as_str());

    let Some(account) = crate::routes::account_facts(state).await else {
        return TickOutcome::Skipped {
            reason: "account_unavailable",
        };
    };

    // Closed candles for every candidate; a candidate whose data is
    // unavailable is dropped from this tick instead of failing the rest.
    let mut markets: Vec<(Symbol, CandleSeries)> = Vec::new();
    let mut market_error: Option<String> = None;
    for symbol in &candidates {
        let request =
            match CandleRequest::new(symbol.clone(), settings.timeframe(), settings.bars()) {
                Ok(request) => request,
                Err(error) => {
                    let reason = format!("market request: {error}");
                    record(state, "unavailable", Some(symbol), None, Some(&reason)).await;
                    return TickOutcome::Unavailable { reason };
                }
            };
        match market.feed().candles(request).await {
            Ok(series) if !series.candles().is_empty() => markets.push((symbol.clone(), series)),
            Ok(_) => {
                tracing::warn!(
                    symbol = symbol.as_str(),
                    "candidate returned no candles; skipping"
                );
            }
            Err(error) => {
                tracing::warn!(symbol = symbol.as_str(), %error, "candidate market data unavailable; skipping");
                market_error = Some(error.to_string());
            }
        }
    }
    if markets.is_empty() {
        let reason = match market_error {
            Some(error) => format!("market unavailable: {error}"),
            None => "market returned no candles for any candidate".to_owned(),
        };
        record(state, "unavailable", None, None, Some(&reason)).await;
        return TickOutcome::Unavailable { reason };
    }
    // The tick's own last closes value stop distances for candidates that have
    // no open position to quote from.
    let mut account = with_reference_prices(account, &markets);

    // Calibrated judgements per candidate. They are advisory inputs, but a
    // configured judge that fails normally aborts the tick: no model call runs
    // on partial inputs. The owner can override that from the console, which
    // trades on the model alone rather than pausing through a judge outage.
    // The override drops the whole set instead of keeping the symbols that did
    // answer, so the model never compares a judged instrument against an
    // unjudged one and reads the silence as an absence of evidence.
    let mut judgements: Vec<(Symbol, Value)> = Vec::new();
    if settings.jev() != JevPreference::Off
        && let Some(jev) = state.jev()
    {
        for (symbol, series) in &markets {
            let candle_time = series.last().map(|candle| candle.time()).unwrap_or(0);
            // The judge sees only this candle series, so an answer already held
            // for the same newest candle is the answer it would give again.
            if let Some(held) = state.judgements().get(symbol.as_str(), candle_time) {
                judgements.push((symbol.clone(), held));
                continue;
            }
            match judgements_for(jev, series).await {
                Ok(summary) => {
                    state
                        .judgements()
                        .put(symbol.as_str(), candle_time, summary.clone());
                    judgements.push((symbol.clone(), summary));
                }
                Err(error) => {
                    let reason = format!("judgement unavailable: {error}");
                    if !state.risk().policy().allow_trading_without_jev() {
                        record(state, "unavailable", Some(symbol), None, Some(&reason)).await;
                        return TickOutcome::Unavailable { reason };
                    }
                    tracing::warn!(
                        symbol = symbol.as_str(),
                        %error,
                        "judge unavailable; continuing without judgements by owner override"
                    );
                    record(state, "jev_degraded", Some(symbol), None, Some(&reason)).await;
                    judgements.clear();
                    break;
                }
            }
        }
    }

    // One position review per tick, rotating through the open book; a close
    // ends the tick, a hold falls through so entries can still be considered.
    //
    // A position the minimum hold still protects cannot be closed whatever the
    // answer, so asking costs a model call to produce a verdict the close path
    // would only reject as `position_too_young`. The same applies when the
    // venue reports no verifiable age: that close is refused too. Stop moves
    // above are deliberately not gated — they guard money already at risk.
    let mut reviewed_hold = false;
    state
        .review_watch()
        .retain(&managed.iter().map(|p| p.ticket).collect::<Vec<_>>());
    state
        .weekend_watch()
        .retain(&managed.iter().map(|p| p.ticket).collect::<Vec<_>>());

    // Weekend posture: the last hours before Friday's close already refuse
    // entries, so the whole window belongs to the book. Every open position
    // gets one verdict there — a staged flatten for the `flatten` preference,
    // one weekend review for `agent`, nothing for `hold` — and it is asked
    // before the candle review so the weekend question owns the tick while a
    // staged close can still execute.
    let policy = state.risk().policy();
    let weekend = crate::risk::window::weekend_prep(state.now());
    // `hold` is the operator taking the weekend: neither preference below
    // runs, and the candle reviews stay as they were — without the weekend
    // framing, so the model cannot close for a risk the operator accepted.
    let weekend_special = weekend.filter(|_| policy.weekend_positions() != WeekendPositions::Hold);
    let weekend_due: Vec<ManagedPosition> = weekend_special
        .map(|prep| {
            managed
                .iter()
                .filter(|position| {
                    !policy.allows_weekend_name(&position.symbol)
                        && state
                            .weekend_watch()
                            .should_review(position.ticket, prep.closes_at)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(prep) = weekend_special
        && policy.weekend_positions() == WeekendPositions::Flatten
        && !weekend_due.is_empty()
    {
        return flatten_before_weekend(state, &weekend_due, prep).await;
    }
    // Only the agent preference turns the weekend into a question for the
    // model; under `hold` the candle review is left exactly as it was.
    let weekend_context = weekend.filter(|_| policy.weekend_positions() == WeekendPositions::Agent);
    let mut reviewed_weekend = false;
    if let Some(prep) = weekend_context
        && let Some(position) = weekend_target(&weekend_due, state.rotation())
        && matches!(
            position_age_secs(server_time, position.opened_at),
            Some(age) if age >= settings.min_hold().as_secs()
        )
        && let Some(series) = series_for_symbol(&markets, &position.symbol)
    {
        let candle_time = series.last().map(|candle| candle.time()).unwrap_or(0);
        // Both watches are marked before the verdict: the weekend question is
        // settled for this close, and the candle it was answered on is not
        // asked again from the same bars. An error on the way to a verdict
        // must not repeat either question every tick.
        state
            .weekend_watch()
            .record(position.ticket, prep.closes_at);
        state.review_watch().record(position.ticket, candle_time);
        let engine = model.engine();
        let session = AgentSession {
            state,
            engine: engine.as_ref(),
            mode: AgentMode::Review,
            markets: &markets,
            account: &account,
            judgements: &judgements,
            tier: settings.tier(),
            now: state.now(),
        };
        let outcome = review_positions(
            state,
            settings,
            &session,
            series,
            std::slice::from_ref(&position),
            Some(prep),
            "autopilot_weekend",
        )
        .await;
        if !matches!(outcome, TickOutcome::Held) {
            return outcome;
        }
        reviewed_weekend = true;
        reviewed_hold = true;
    }

    if !reviewed_weekend
        && let Some(position) = next_review_position(&managed, state.rotation())
        && matches!(
            position_age_secs(server_time, position.opened_at),
            Some(age) if age >= settings.min_hold().as_secs()
        )
        && let Some(series) = series_for_symbol(&markets, &position.symbol)
        && let candle_time = series.last().map(|candle| candle.time()).unwrap_or(0)
        && state
            .review_watch()
            .should_review(position.ticket, candle_time)
    {
        // Marked before the verdict, so an error on the way to one cannot make
        // the same question repeat every tick for the rest of the candle.
        state.review_watch().record(position.ticket, candle_time);
        // A candle review inside the weekend window carries the same context,
        // so it settles the weekend question for this close too.
        let position_weekend_context =
            weekend_context.filter(|_| !policy.allows_weekend_name(&position.symbol));
        if let Some(prep) = position_weekend_context {
            state
                .weekend_watch()
                .record(position.ticket, prep.closes_at);
        }
        let engine = model.engine();
        let session = AgentSession {
            state,
            engine: engine.as_ref(),
            mode: AgentMode::Review,
            markets: &markets,
            account: &account,
            judgements: &judgements,
            tier: settings.tier(),
            now: state.now(),
        };
        let outcome = review_positions(
            state,
            settings,
            &session,
            series,
            std::slice::from_ref(&position),
            position_weekend_context,
            "autopilot_review",
        )
        .await;
        if !matches!(outcome, TickOutcome::Held) {
            return outcome;
        }
        reviewed_hold = true;
    }

    // Entry: the deterministic caps decide whether another position is even
    // possible; only then is the model asked to pick from the menu, and it may
    // still answer `none` when no instrument is suitable.
    // What the model would be shown, as the gate reads it. A candidate's live
    // price is only known when the venue reports one for it; without that the
    // gate falls back to new closed candles alone.
    // Venue contracts are part of the gate facts, not just prompt context.
    // They let the same deterministic stop-risk formula price FX, crypto,
    // indices, and any future CFD in the account currency the broker reports.
    let specs = symbol_specs(state, &markets).await;
    account.symbol_specs = specs.iter().map(|(_, spec)| spec.clone()).collect();
    let mut entry_markets = eligible_entry_markets(&markets, &specs, &policy, state.now());
    let eligible_before_cooldown = !entry_markets.is_empty();
    let observations: Vec<EntryObservation<'_>> = markets
        .iter()
        .map(|(symbol, series)| EntryObservation {
            symbol: symbol.as_str(),
            candle_time: series.last().map(|candle| candle.time()).unwrap_or(0),
            price: account
                .prices
                .iter()
                .find(|(known, _)| known == symbol)
                .map(|(_, price)| *price),
            atr: average_true_range(series, ATR_PERIOD),
        })
        .collect();
    if settings.profit_harvest().is_some() {
        let pending = state.profit_harvest_book().pending_fresh_baselines();
        if !pending.is_empty() {
            let captured = state.entry_watch().record_symbols(&observations, &pending);
            state.profit_harvest_book().mark_fresh_baselines(&captured);
        }
        let awaiting_fresh = state.profit_harvest_book().fresh_market_required();
        let newly_fresh = state.entry_watch().changed_symbols(
            &observations,
            &awaiting_fresh,
            settings.entry_move_atr_fraction(),
        );
        state.profit_harvest_book().mark_fresh_market(&newly_fresh);
        let now = unix_secs(state.now());
        entry_markets.retain(|(symbol, _)| {
            !state.profit_harvest_book().cooling(symbol.as_str(), now)
                && !state
                    .profit_harvest_book()
                    .requires_fresh_market(symbol.as_str())
        });
    }
    let entry_outcome = if account.open_orders >= policy.max_open_orders() {
        record(state, "no_trade", None, None, Some("open_order_cap")).await;
        TickOutcome::NoTrade
    } else if policy.max_total_lots().value() - account.open_lots <= 0.0 {
        record(state, "no_trade", None, None, Some("exposure_cap")).await;
        TickOutcome::NoTrade
    } else if !state.entry_watch().should_evaluate(
        &observations,
        settings.entry_move_atr_fraction(),
        unix_secs(state.now()),
    ) {
        // Nothing moved since the last proposal, so the model would be asked
        // an identical question. Stops and reviews already ran above.
        TickOutcome::Unchanged
    } else if entry_markets.is_empty() {
        let reason = if eligible_before_cooldown {
            "reentry_cooldown"
        } else {
            empty_entry_reason(&markets, &policy, state.now())
        };
        state.entry_watch().record(&observations);
        record(state, "no_trade", None, None, Some(reason)).await;
        TickOutcome::NoTrade
    } else {
        // Marked before the proposal, not after: whatever this sweep decides,
        // the question has now been asked about this market, and an error on
        // the way to the answer must not leave it asking again every tick.
        // A sweep that dies before a verdict arms a retry instead (see
        // `mark_failed`), so the mark costs minutes rather than the candle.
        state.entry_watch().record(&observations);
        // Scheduled news: a configured calendar that cannot answer aborts the
        // entry sweep; trading blind through a data outage is exactly what the
        // blackout exists to prevent.
        let events = match calendar_events(state).await {
            Ok(events) => events,
            Err(reason) => {
                state.entry_watch().mark_failed(unix_secs(state.now()));
                record(state, "unavailable", None, None, Some(&reason)).await;
                return TickOutcome::Unavailable { reason };
            }
        };
        let input = proposal_input(
            &entry_markets,
            &judgements,
            &account,
            &managed,
            &specs,
            &events,
            unix_secs(state.now()),
        );
        let instructions = proposal_instructions(state, &entry_markets, &account);
        let engine = model.engine();
        let session = AgentSession {
            state,
            engine: engine.as_ref(),
            mode: AgentMode::Proposal,
            markets: &entry_markets,
            account: &account,
            judgements: &judgements,
            tier: settings.tier(),
            now: state.now(),
        };
        match agent::run(&session, &instructions, &input).await {
            Err(error) => {
                let reason = match error {
                    PipelineError::AgentLoopLimit { reason } => format!("agent loop: {reason}"),
                    other => format!("model unavailable: {other}"),
                };
                // A refused request fails this tick and nothing else, so the
                // run has to be counted somewhere a surface can read it.
                let now = unix_secs(state.now());
                state.decision_health().failed(&reason, now);
                // The gate was marked for a market this sweep never judged.
                state.entry_watch().mark_failed(now);
                record(state, "unavailable", None, None, Some(&reason)).await;
                TickOutcome::Unavailable { reason }
            }
            Ok(outcome) => {
                state.decision_health().succeeded();
                state.entry_watch().mark_settled();
                let AgentDecision::Proposal(evaluation) = outcome.decision else {
                    let reason = "agent returned a review for an entry decision".to_owned();
                    record(state, "unavailable", None, None, Some(&reason)).await;
                    return TickOutcome::Unavailable { reason };
                };
                let tool_names: Vec<String> = outcome
                    .tool_calls
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect();
                let rationale = evaluation.rationale.as_deref();
                match evaluation.outcome {
                    PipelineOutcome::NoTrade => {
                        record_event_context(
                            state,
                            "no_trade",
                            None,
                            None,
                            None,
                            None,
                            None,
                            DecisionContext {
                                rationale,
                                judgements: None,
                                tool_names: Some(&tool_names),
                            },
                        )
                        .await;
                        TickOutcome::NoTrade
                    }
                    PipelineOutcome::Rejected { rejection, draft } => {
                        let symbol = draft.symbol().clone();
                        record_event_context(
                            state,
                            "rejected",
                            Some(&symbol),
                            Some(&draft),
                            Some(rejection.code().as_str()),
                            None,
                            None,
                            DecisionContext {
                                rationale,
                                judgements: judgement_for_symbol(&judgements, symbol.as_str()),
                                tool_names: Some(&tool_names),
                            },
                        )
                        .await;
                        TickOutcome::Rejected {
                            code: rejection.code().as_str(),
                        }
                    }
                    PipelineOutcome::Approved(intent) => {
                        let draft = intent.draft();
                        let symbol = draft.symbol().clone();
                        let context = DecisionContext {
                            rationale,
                            judgements: judgement_for_symbol(&judgements, symbol.as_str()),
                            tool_names: Some(&tool_names),
                        };
                        if draft.stop_loss().is_none() || draft.take_profit().is_none() {
                            record_event_context(
                                state,
                                "rejected",
                                Some(&symbol),
                                Some(draft),
                                Some("missing_stops"),
                                None,
                                None,
                                context,
                            )
                            .await;
                            return TickOutcome::Rejected {
                                code: "missing_stops",
                            };
                        }
                        // Scheduled news decides before the venue contract:
                        // an entry inside a high-impact window is refused for
                        // the instrument's own currencies.
                        if let Some(event) = calendar::blackout(
                            &events,
                            symbol.as_str(),
                            unix_secs(state.now()),
                            policy.calendar_blackout_minutes(),
                        ) {
                            tracing::debug!(
                                symbol = symbol.as_str(),
                                event = event.title(),
                                "entry refused inside the news blackout"
                            );
                            record_event_context(
                                state,
                                "rejected",
                                Some(&symbol),
                                Some(draft),
                                Some("news_blackout"),
                                None,
                                None,
                                context,
                            )
                            .await;
                            return TickOutcome::Rejected {
                                code: "news_blackout",
                            };
                        }
                        // The venue contract decides last: volume on the lot
                        // grid, margin the account can cover, and a stop far
                        // enough out to survive the spread and stop level.
                        if let Err(violation) = contract::validate_entry(
                            draft,
                            symbol_spec_for(&specs, symbol.as_str()),
                            account.free_margin,
                            series_for_symbol(&entry_markets, symbol.as_str())
                                .and_then(|series| series.last())
                                .map(|candle| candle.close()),
                            series_for_symbol(&entry_markets, symbol.as_str())
                                .and_then(|series| average_true_range(series, ATR_PERIOD)),
                            policy.min_stop_atr_fraction(),
                        ) {
                            record_event_context(
                                state,
                                "rejected",
                                Some(&symbol),
                                Some(draft),
                                Some(violation.as_str()),
                                None,
                                None,
                                context,
                            )
                            .await;
                            return TickOutcome::Rejected {
                                code: violation.as_str(),
                            };
                        }
                        match queue_staged_order(state, &intent).await {
                            StagedExecution::Queued { command, intent_id } => {
                                record_event_context(
                                    state,
                                    "queued",
                                    Some(&symbol),
                                    Some(draft),
                                    None,
                                    Some(&intent_id),
                                    Some(&command.to_string()),
                                    context,
                                )
                                .await;
                                TickOutcome::Queued {
                                    command: command.to_string(),
                                }
                            }
                            StagedExecution::TradingDisabled => {
                                let intent_id = intent.id().to_string();
                                record_event_context(
                                    state,
                                    "approved_dry_run",
                                    Some(&symbol),
                                    Some(draft),
                                    None,
                                    Some(&intent_id),
                                    None,
                                    context,
                                )
                                .await;
                                TickOutcome::ApprovedDryRun
                            }
                            StagedExecution::ChannelUnavailable => {
                                let reason = "command channel unavailable".to_owned();
                                let intent_id = intent.id().to_string();
                                record_event_context(
                                    state,
                                    "unavailable",
                                    Some(&symbol),
                                    Some(draft),
                                    Some(&reason),
                                    Some(&intent_id),
                                    None,
                                    context,
                                )
                                .await;
                                TickOutcome::Unavailable { reason }
                            }
                        }
                    }
                }
            }
        }
    };
    // A hold review is the tick's primary outcome when the entry sweep also
    // found nothing to do; both events are already recorded.
    if reviewed_hold && matches!(entry_outcome, TickOutcome::NoTrade) {
        TickOutcome::Held
    } else {
        entry_outcome
    }
}

/// Runs only deterministic management of the open book. It has no market-data,
/// judge, or model dependency and is scheduled separately in production so a
/// slow decision sweep cannot delay a profitable close or protective stop.
#[tracing::instrument(skip_all, name = "autopilot.positions")]
pub async fn manage_open_positions(state: &AppState) -> TickOutcome {
    let Some(settings) = state.autopilot() else {
        return TickOutcome::Skipped {
            reason: "not_configured",
        };
    };
    if !settings.enabled() {
        return TickOutcome::Skipped { reason: "disabled" };
    }
    let Some(broker) = state.broker() else {
        return TickOutcome::Skipped {
            reason: "no_broker",
        };
    };
    if !broker.link().report().await.fresh {
        return TickOutcome::Skipped {
            reason: "stale_link",
        };
    }
    manage_open_positions_after_link(state, settings)
        .await
        .unwrap_or(TickOutcome::Unchanged)
}

async fn manage_open_positions_after_link(
    state: &AppState,
    settings: &AutopilotSettings,
) -> Option<TickOutcome> {
    let broker = state.broker()?;
    let managed = managed_positions(state);
    let snapshot = broker.link().last_account();
    let server_time = snapshot
        .as_ref()
        .map(|snapshot| snapshot.server_time)
        .unwrap_or(0);
    let max_snapshot_age = Duration::from_secs(
        settings
            .interval()
            .as_secs()
            .saturating_mul(2)
            .max(MIN_INTERVAL_SECS),
    );
    let snapshot_is_complete_and_fresh = snapshot
        .as_ref()
        .is_some_and(|snapshot| !snapshot.positions_truncated)
        && broker
            .link()
            .last_account_age(state.now())
            .is_some_and(|age| age <= max_snapshot_age);
    if !snapshot_is_complete_and_fresh {
        return None;
    }

    // Profit harvesting only learns from a fresh, complete venue book: an old
    // or truncated snapshot can never fabricate a close or a disappeared
    // ticket. One staged close/modify at a time prevents command duplication.
    state.stop_basis().observe(&managed);
    let execution_pending = broker.link().has_pending(CommandKind::CloseOrder)
        || broker.link().has_pending(CommandKind::ModifyOrder);
    if let Some(policy) = settings.profit_harvest() {
        state.profit_harvest_book().observe(
            &managed,
            unix_secs(state.now()),
            policy.reentry_cooldown(),
        );
        if state.config().trading_enabled() && !execution_pending {
            if let Some(plan) = harvest_close_plan(
                &managed,
                policy,
                state.stop_basis(),
                state.profit_harvest_book(),
                server_time,
                unix_secs(state.now()),
            ) {
                return Some(close_harvest(state, plan).await);
            }
            if let Some(plan) = harvest_stop_plan(
                &managed,
                policy,
                state.stop_basis(),
                state.profit_harvest_book(),
                server_time,
            ) {
                let Some(symbol) = managed
                    .iter()
                    .find(|position| position.ticket == plan.ticket)
                    .map(|position| position.symbol.as_str())
                else {
                    return Some(TickOutcome::Unavailable {
                        reason: "profit-harvest stop lost its position context".to_owned(),
                    });
                };
                return Some(move_stop(state, symbol, plan).await);
            }
        }
    }
    if state.config().trading_enabled()
        && !execution_pending
        && let Some(plan) = stop_plan(
            &managed,
            settings.breakeven_r(),
            settings.trail_r(),
            state.stop_basis(),
        )
    {
        let Some(symbol) = managed
            .iter()
            .find(|position| position.ticket == plan.ticket)
            .map(|position| position.symbol.as_str())
        else {
            return Some(TickOutcome::Unavailable {
                reason: "stop policy lost its position context".to_owned(),
            });
        };
        return Some(move_stop(state, symbol, plan).await);
    }
    None
}

/// Direction of a managed position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedSide {
    /// Long.
    Buy,
    /// Short.
    Sell,
}

impl ManagedSide {
    fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

/// One Veyra-managed position as the review needs it.
#[derive(Debug, Clone, PartialEq)]
struct ManagedPosition {
    ticket: i64,
    symbol: String,
    side: ManagedSide,
    lots: f64,
    entry: f64,
    profit: f64,
    stop_loss: f64,
    take_profit: f64,
    opened_at: i64,
    current: f64,
    swap: f64,
    commission: f64,
    is_market: bool,
}

impl ManagedPosition {
    /// Spread-adjusted live result including the venue costs reported so far.
    fn net_profit(&self) -> f64 {
        self.profit + self.swap + self.commission
    }
}

/// Managed positions from the latest completed snapshot.
///
/// Like the control surface, this reads the provider's retained state
/// directly; a second broker implementation replaces this one mapping.
fn managed_positions(state: &AppState) -> Vec<ManagedPosition> {
    state
        .broker()
        .and_then(|broker| broker.link().last_account())
        .map(|snapshot| {
            snapshot
                .positions
                .iter()
                .filter(|position| position.magic == ORDER_MAGIC)
                .map(managed_from_payload)
                .collect()
        })
        .unwrap_or_default()
}

fn managed_from_payload(position: &PositionPayload) -> ManagedPosition {
    use crate::broker::PositionKind;
    ManagedPosition {
        ticket: position.ticket,
        symbol: position.symbol.clone(),
        side: match position.kind {
            PositionKind::Buy
            | PositionKind::BuyLimit
            | PositionKind::BuyStop
            | PositionKind::BuyStopLimit => ManagedSide::Buy,
            PositionKind::Sell
            | PositionKind::SellLimit
            | PositionKind::SellStop
            | PositionKind::SellStopLimit => ManagedSide::Sell,
        },
        lots: position.lots,
        entry: position.price,
        profit: position.profit,
        stop_loss: position.stop_loss,
        take_profit: position.take_profit,
        opened_at: position.opened_at,
        current: position.current,
        swap: position.swap,
        commission: position.commission,
        is_market: matches!(
            position.kind,
            crate::broker::PositionKind::Buy | crate::broker::PositionKind::Sell
        ),
    }
}

/// The candidate menu: configured symbols first, then symbols of open
/// managed positions, de-duplicated in first-seen order.
fn candidate_symbols(configured: &[Symbol], managed: &[ManagedPosition]) -> Vec<Symbol> {
    let mut symbols: Vec<Symbol> = configured.to_vec();
    for position in managed {
        if let Ok(symbol) = Symbol::parse(&position.symbol)
            && !symbols
                .iter()
                .any(|known| known.as_str() == symbol.as_str())
        {
            symbols.push(symbol);
        }
    }
    symbols
}

/// Adds the tick's last closes to the facts so the gate can value drafts for
/// instruments without an open position.
fn with_reference_prices(
    mut account: AccountFacts,
    markets: &[(Symbol, CandleSeries)],
) -> AccountFacts {
    for (symbol, series) in markets {
        let Some(last) = series.last() else {
            continue;
        };
        // Only fill a gap. A symbol already carrying a price got it from the
        // account snapshot, which is the live close for an open position and
        // therefore fresher than a candle that may be hours old.
        if !account.prices.iter().any(|(known, _)| known == symbol) {
            account.prices.push((symbol.clone(), last.close()));
        }
    }
    account
}

/// Series for one symbol from this tick's fetched markets.
fn series_for_symbol<'a>(
    markets: &'a [(Symbol, CandleSeries)],
    symbol: &str,
) -> Option<&'a CandleSeries> {
    markets
        .iter()
        .find(|(candidate, _)| candidate.as_str() == symbol)
        .map(|(_, series)| series)
}

/// Fetches the venue contract for every menu instrument. A symbol whose
/// contract is unavailable is left out; the contract check then treats its
/// drafts as unverifiable and rejects them instead of queueing blind.
async fn symbol_specs(
    state: &AppState,
    markets: &[(Symbol, CandleSeries)],
) -> Vec<(Symbol, SymbolSpecPayload)> {
    let Some(market) = state.market() else {
        return Vec::new();
    };
    let feed = market.feed();
    let mut specs = Vec::new();
    for (symbol, _) in markets {
        match feed.symbol_spec(symbol).await {
            Ok(spec) => specs.push((symbol.clone(), spec)),
            Err(error) => {
                tracing::warn!(
                    symbol = symbol.as_str(),
                    %error,
                    "symbol contract unavailable; entries for this symbol will be rejected"
                );
            }
        }
    }
    specs
}

/// Entry-only market menu after deterministic schedule and venue-contract
/// checks. Managed positions remain in the full `markets` list for reviews;
/// this filtered clone is the only menu the entry model can propose from.
fn eligible_entry_markets(
    markets: &[(Symbol, CandleSeries)],
    specs: &[(Symbol, SymbolSpecPayload)],
    policy: &RiskPolicy,
    now: SystemTime,
) -> Vec<(Symbol, CandleSeries)> {
    let session_open = configured_session_open(policy, now);
    markets
        .iter()
        .filter(|(symbol, _)| {
            let schedule_open = policy.allows_weekend(symbol)
                || crate::risk::window::entry_block(now, None).is_none();
            let contract_open = match symbol_spec_for(specs, symbol.as_str()) {
                Some(spec) => {
                    spec.trade_allowed
                        && spec.tick_size.is_finite()
                        && spec.tick_size > 0.0
                        && spec.tick_value.is_finite()
                        && spec.tick_value > 0.0
                }
                None => crate::risk::valuation::supports_static_valuation(symbol),
            };
            session_open && schedule_open && contract_open
        })
        .cloned()
        .collect()
}

fn configured_session_open(policy: &RiskPolicy, now: SystemTime) -> bool {
    policy.session().is_none_or(|session| {
        crate::risk::window::utc_now_parts(now)
            .map(|(_, minute)| session.contains((minute / 60) as u8))
            .unwrap_or(false)
    })
}

/// Stable audit reason when no market can enter before a model call.
fn empty_entry_reason(
    markets: &[(Symbol, CandleSeries)],
    policy: &RiskPolicy,
    now: SystemTime,
) -> &'static str {
    if !configured_session_open(policy, now) {
        return "session_closed";
    }
    if !markets.is_empty()
        && markets
            .iter()
            .all(|(symbol, _)| !policy.allows_weekend(symbol))
        && let Some(block) = crate::risk::window::entry_block(now, None)
    {
        return block.as_str();
    }
    "no_tradeable_candidates"
}

/// Fetches the scheduled events covering the decision horizon. Without a
/// configured calendar the list is empty and both news behaviours are inert.
async fn calendar_events(state: &AppState) -> Result<Vec<CalendarEvent>, String> {
    let Some(calendar) = state.calendar() else {
        return Ok(Vec::new());
    };
    let now = unix_secs(state.now());
    calendar
        .feed()
        .events(now - CALENDAR_LOOKBACK_SECS, now + CALENDAR_HORIZON_SECS)
        .await
        .map_err(|error| format!("calendar unavailable: {error}"))
}

/// Seconds since the Unix epoch, or zero for clock values before it.
fn unix_secs(now: SystemTime) -> i64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Contract for one symbol from this tick's fetched specs.
fn symbol_spec_for<'a>(
    specs: &'a [(Symbol, SymbolSpecPayload)],
    symbol: &str,
) -> Option<&'a SymbolSpecPayload> {
    specs
        .iter()
        .find(|(candidate, _)| candidate.as_str() == symbol)
        .map(|(_, spec)| spec)
}

/// Judgement summary for one symbol, when the judge produced one.
fn judgement_for_symbol<'a>(judgements: &'a [(Symbol, Value)], symbol: &str) -> Option<&'a Value> {
    judgements
        .iter()
        .find(|(candidate, _)| candidate.as_str() == symbol)
        .map(|(_, summary)| summary)
}

/// Parses a comma-separated symbol list: 1-16 distinct validated instruments.
fn parse_symbol_list(raw: &str) -> Result<Vec<Symbol>, ConfigError> {
    let invalid = |reason: &'static str| ConfigError::InvalidEnvironmentVariable {
        name: "VEYRA_AUTOPILOT_SYMBOLS",
        reason,
    };
    let mut symbols = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err(invalid(
                "entries must be non-empty symbols separated by commas",
            ));
        }
        let symbol = Symbol::parse(trimmed).map_err(|_| {
            invalid(
                "each entry must be 1-24 characters of letters, digits, '.', '_', '#', '+' or '-'",
            )
        })?;
        if !symbols
            .iter()
            .any(|known: &Symbol| known.as_str() == symbol.as_str())
        {
            symbols.push(symbol);
        }
    }
    if symbols.is_empty() {
        return Err(invalid("at least one symbol is required"));
    }
    if symbols.len() > MAX_SYMBOLS {
        return Err(invalid("at most 16 symbols may rotate"));
    }
    Ok(symbols)
}

/// The position to review this tick: one step of a round-robin over the open
/// book, so several positions take turns instead of one starving the rest.
fn next_review_position(
    managed: &[ManagedPosition],
    counter: &AtomicUsize,
) -> Option<ManagedPosition> {
    if managed.is_empty() {
        return None;
    }
    let index = counter.fetch_add(1, Ordering::Relaxed) % managed.len();
    managed.get(index).cloned()
}

/// Next position in rotation order that still needs a weekend verdict.
///
/// The caller passes only the positions still due one, so this is the same
/// rotation the candle review uses, over a smaller book.
fn weekend_target(due: &[ManagedPosition], counter: &AtomicUsize) -> Option<ManagedPosition> {
    if due.is_empty() {
        return None;
    }
    let index = counter.fetch_add(1, Ordering::Relaxed) % due.len();
    due.get(index).cloned()
}

/// How a refused staged close reads to the caller that asked for it: the
/// journal label, the reason, and the tick outcome.
///
/// The position review and the weekend flatten ask the same question of the
/// same venue, so a refusal has to read the same either way; the mapping lives
/// here once rather than duplicated across both matches. `Queued` never
/// reaches this — both callers handle the command it carries first — so the
/// fallback reads as a stale position rather than panicking.
fn close_refusal(failed: &StagedClose) -> (&'static str, String, TickOutcome) {
    match failed {
        StagedClose::TradingDisabled => (
            "close_rejected",
            "trading_disabled".to_owned(),
            TickOutcome::Rejected {
                code: "trading_disabled",
            },
        ),
        StagedClose::ChannelUnavailable => {
            let reason = "command channel unavailable".to_owned();
            (
                "unavailable",
                reason.clone(),
                TickOutcome::Unavailable { reason },
            )
        }
        StagedClose::NoPositions | StagedClose::UnknownTicket | StagedClose::Queued { .. } => (
            "close_rejected",
            "stale_position".to_owned(),
            TickOutcome::Rejected {
                code: "stale_position",
            },
        ),
        StagedClose::NotVeyra => (
            "close_rejected",
            "not_a_veyra_position".to_owned(),
            TickOutcome::Rejected {
                code: "not_a_veyra_position",
            },
        ),
    }
}

/// Queues the weekend close for every position still due one, in this tick.
///
/// The verdict is the operator's, so there is no model call to ration and the
/// whole book can be settled at once; the earliest tick in the window also
/// leaves the most room to retry if the venue refuses. A position is marked
/// only once its close is queued, so a refused close is asked again on the next
/// tick rather than leaving the position to the gap.
async fn flatten_before_weekend(
    state: &AppState,
    due: &[ManagedPosition],
    prep: WeekendPrep,
) -> TickOutcome {
    let mut queued = None;
    for position in due {
        match queue_staged_close(state, position.ticket).await {
            StagedClose::Queued { command, ticket } => {
                let command_id = command.to_string();
                record_symbol_event(
                    state,
                    "close_queued",
                    "autopilot_weekend",
                    &position.symbol,
                    Some(ticket),
                    None,
                    Some(&command_id),
                )
                .await;
                state.weekend_watch().record(ticket, prep.closes_at);
                queued = Some(command.to_string());
            }
            failed => {
                let (label, reason, outcome) = close_refusal(&failed);
                record_symbol_event(
                    state,
                    label,
                    "autopilot_weekend",
                    &position.symbol,
                    Some(position.ticket),
                    Some(&reason),
                    None,
                )
                .await;
                return outcome;
            }
        }
    }
    match queued {
        Some(command) => TickOutcome::CloseQueued { command },
        None => TickOutcome::Unchanged,
    }
}

/// Which policy produced a stop move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMoveKind {
    /// The stop moved to the entry price.
    BreakEven,
    /// The stop trailed behind the best favourable price.
    Trail,
    /// The early-profit policy ratcheted the stop before the original target.
    ProfitHarvest,
}

impl StopMoveKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::BreakEven => "break_even",
            Self::Trail => "trailing_stop",
            Self::ProfitHarvest => "profit_harvest_stop",
        }
    }
}

/// A planned stop change for one position.
#[derive(Debug, Clone, PartialEq)]
struct StopMove {
    ticket: i64,
    stop: f64,
    kind: StopMoveKind,
}

/// A profitable position whose high-water retracement should be banked now.
#[derive(Debug, Clone, PartialEq)]
struct HarvestClosePlan {
    ticket: i64,
    symbol: String,
    high_net_profit: f64,
    net_profit: f64,
}

/// Smallest improvement worth another modify round trip, as a fraction of the
/// entry risk; keeps a trailing stop from re-submitting every tick.
const STOP_MIN_STEP_RATIO: f64 = 0.1;

/// Entry-risk memory for the stop policies.
///
/// The terminal reports a position's *current* stop, so once break-even or
/// trailing has moved it, the distance the trade originally risked is no
/// longer derivable from one payload. The first observation of each ticket —
/// while its stop still sits behind the entry — is remembered here and used
/// as the risk basis for every later decision. A ticket first seen after a
/// move (for example across a service restart) has no basis and is left
/// alone until it closes. Tickets that are no longer open are dropped.
#[derive(Debug, Default)]
pub struct StopBasis {
    risks: std::sync::Mutex<std::collections::HashMap<i64, f64>>,
}

/// Durable floating-profit memory and re-entry guard for the harvest policy.
#[derive(Debug, Default)]
pub struct ProfitHarvestBook {
    state: std::sync::Mutex<ProfitHarvestState>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfitHarvestState {
    positions: std::collections::HashMap<i64, ProfitHighWater>,
    cooldowns: std::collections::HashMap<String, i64>,
    #[serde(default)]
    pending_fresh_baselines: std::collections::BTreeSet<String>,
    #[serde(default)]
    fresh_market_required: std::collections::BTreeSet<String>,
    #[serde(default)]
    close_guards: std::collections::HashMap<i64, i64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfitHighWater {
    symbol: String,
    high_net_profit: f64,
    #[serde(default)]
    armed: bool,
}

impl ProfitHarvestBook {
    /// Observes the complete managed book, advances profit high-water marks,
    /// and starts a symbol cooldown when a previously observed ticket closes.
    fn observe(&self, positions: &[ManagedPosition], now: i64, cooldown: Duration) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let live: std::collections::HashSet<i64> =
            positions.iter().map(|position| position.ticket).collect();
        let closed: Vec<ProfitHighWater> = state
            .positions
            .iter()
            .filter(|(ticket, _)| !live.contains(ticket))
            .map(|(_, seen)| seen.clone())
            .collect();
        state.positions.retain(|ticket, _| live.contains(ticket));
        state
            .close_guards
            .retain(|ticket, until| live.contains(ticket) && *until > now);
        for seen in closed {
            let cooldown_secs = cooldown.as_secs().min(i64::MAX as u64) as i64;
            let until = now.saturating_add(cooldown_secs);
            state
                .cooldowns
                .entry(seen.symbol.clone())
                .and_modify(|current| *current = (*current).max(until))
                .or_insert(until);
            state.pending_fresh_baselines.insert(seen.symbol.clone());
            state.fresh_market_required.insert(seen.symbol);
        }
        state.cooldowns.retain(|_, until| *until > now);
        for position in positions.iter().filter(|position| position.is_market) {
            let net = position.net_profit().max(0.0);
            state
                .positions
                .entry(position.ticket)
                .and_modify(|seen| {
                    seen.high_net_profit = seen.high_net_profit.max(net);
                    seen.symbol.clone_from(&position.symbol);
                })
                .or_insert_with(|| ProfitHighWater {
                    symbol: position.symbol.clone(),
                    high_net_profit: net,
                    armed: false,
                });
        }
    }

    /// Highest spread-and-cost-adjusted floating result observed for a ticket.
    fn high_net_profit(&self, ticket: i64) -> Option<f64> {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .positions
            .get(&ticket)
            .map(|seen| seen.high_net_profit)
    }

    fn is_armed(&self, ticket: i64) -> bool {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.positions.get(&ticket).is_some_and(|seen| seen.armed)
    }

    fn mark_armed(&self, ticket: i64) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(seen) = state.positions.get_mut(&ticket) {
            seen.armed = true;
        }
    }

    fn close_guarded(&self, ticket: i64, now: i64) -> bool {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .close_guards
            .get(&ticket)
            .is_some_and(|until| *until > now)
    }

    fn guard_close(&self, ticket: i64, now: i64) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.close_guards.insert(ticket, now.saturating_add(120));
    }

    /// Whether a recently closed symbol is still inside its quiet period.
    fn cooling(&self, symbol: &str, now: i64) -> bool {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .cooldowns
            .get(symbol)
            .is_some_and(|until| *until > now)
    }

    /// Symbols whose entry baseline must be reset at the first post-close
    /// market observation, so the next entry needs genuinely fresh movement.
    fn pending_fresh_baselines(&self) -> Vec<String> {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.pending_fresh_baselines.iter().cloned().collect()
    }

    /// Marks post-close baselines as captured for the supplied symbols.
    fn mark_fresh_baselines(&self, symbols: &[String]) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for symbol in symbols {
            state.pending_fresh_baselines.remove(symbol);
        }
    }

    /// Symbols that remain ineligible until their own market changes from the
    /// post-close baseline. This is independent of movement in other symbols.
    fn fresh_market_required(&self) -> Vec<String> {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.fresh_market_required.iter().cloned().collect()
    }

    fn requires_fresh_market(&self, symbol: &str) -> bool {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.fresh_market_required.contains(symbol)
    }

    /// Releases only symbols whose own candle or ATR-scaled price move is new.
    fn mark_fresh_market(&self, symbols: &[String]) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for symbol in symbols {
            state.fresh_market_required.remove(symbol);
        }
    }

    /// Serializable snapshot of high-water marks and cooldowns.
    pub fn state_snapshot(&self) -> Value {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match serde_json::to_value(&*state) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "profit-harvest state serialization failed");
                json!({})
            }
        }
    }

    /// Restores validated high-water and cooldown state.
    ///
    /// # Errors
    /// Returns a description when stored state is malformed or unsafe.
    pub fn restore_state(&self, value: &Value) -> Result<(), String> {
        let restored: ProfitHarvestState = serde_json::from_value(value.clone())
            .map_err(|error| format!("profit-harvest state is unreadable: {error}"))?;
        for (ticket, seen) in &restored.positions {
            if *ticket <= 0 {
                return Err("profit-harvest ticket must be positive".to_owned());
            }
            Symbol::parse(&seen.symbol)
                .map_err(|error| format!("profit-harvest symbol is invalid: {error}"))?;
            if !seen.high_net_profit.is_finite() || seen.high_net_profit < 0.0 {
                return Err(format!(
                    "profit-harvest high-water mark for ticket {ticket} is unusable"
                ));
            }
        }
        for (symbol, until) in &restored.cooldowns {
            Symbol::parse(symbol)
                .map_err(|error| format!("profit-harvest cooldown symbol is invalid: {error}"))?;
            if *until < 0 {
                return Err(format!(
                    "profit-harvest cooldown for {symbol} must be non-negative"
                ));
            }
        }
        for symbol in &restored.pending_fresh_baselines {
            Symbol::parse(symbol).map_err(|error| {
                format!("profit-harvest pending-baseline symbol is invalid: {error}")
            })?;
        }
        for symbol in &restored.fresh_market_required {
            Symbol::parse(symbol).map_err(|error| {
                format!("profit-harvest fresh-market symbol is invalid: {error}")
            })?;
        }
        for (ticket, until) in &restored.close_guards {
            if *ticket <= 0 || *until < 0 {
                return Err("profit-harvest close guard is unusable".to_owned());
            }
        }
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state = restored;
        Ok(())
    }
}

/// One instrument's market as the entry sweep last saw it.
#[derive(Debug, Clone, Copy)]
struct EntrySeen {
    /// Open time of the newest closed candle that was evaluated.
    candle_time: i64,
    /// Price at evaluation, when one was known.
    price: Option<f64>,
}

/// One instrument's current market, as the sweep gate reads it.
#[derive(Debug, Clone, Copy)]
pub struct EntryObservation<'a> {
    /// Instrument being observed.
    pub symbol: &'a str,
    /// Open time of its newest closed candle.
    pub candle_time: i64,
    /// Live price, when the venue reported one for this instrument.
    pub price: Option<f64>,
    /// ATR over the same window, scaling the mid-candle threshold to the
    /// instrument's own volatility.
    pub atr: Option<f64>,
}

/// What the entry sweep last formed an opinion about, per instrument.
///
/// A proposal costs a model call, and asking the same question of an unchanged
/// chart returns the same answer. This remembers the market each instrument was
/// last judged on, so the sweep runs when something actually moved: a new closed
/// candle, or a mid-candle move past a fraction of the instrument's own ATR.
///
/// Position reviews and stop moves never consult this. They must keep running
/// every tick because they react to live price on money already at risk.
#[derive(Debug, Default)]
pub struct EntryWatch {
    seen: std::sync::Mutex<std::collections::HashMap<String, EntrySeen>>,
    /// When the last sweep died before reaching a verdict, if it did.
    ///
    /// The gate is marked before the model is asked, so a sweep that fails on
    /// the way to an answer would otherwise hold the gate shut for the rest of
    /// the candle — up to four hours of silence on H4 from one transient
    /// refusal. Arming a retry keeps the anti-hammer property (a sustained
    /// outage re-asks every [`ENTRY_RETRY_AFTER_SECS`], not every tick) while
    /// bounding what a single failure costs.
    failed_at: std::sync::Mutex<Option<i64>>,
}

/// How long a failed entry sweep holds the gate before it is asked again.
///
/// Short enough that one bad answer costs minutes rather than a whole candle,
/// long enough that a provider outage cannot drain the hourly model budget and
/// leave nothing for the recovery.
pub const ENTRY_RETRY_AFTER_SECS: i64 = 300;

/// The candle each open position was last reviewed on.
///
/// A review asks whether the reason for holding still stands, and that reason
/// is formed from closed candles. Asking again inside the same candle re-runs
/// an identical question, and a model asked repeatedly will eventually answer
/// differently for no new reason — which cuts winners short.
///
/// Stop moves are deliberately not gated by this. A position running toward its
/// stop cannot wait for a bar to close, so that path stays on every tick.
#[derive(Debug, Default)]
pub struct ReviewWatch {
    seen: std::sync::Mutex<std::collections::HashMap<i64, i64>>,
}

impl ReviewWatch {
    /// Whether this position has yet to be reviewed on this candle.
    pub fn should_review(&self, ticket: i64, candle_time: i64) -> bool {
        let Ok(seen) = self.seen.lock() else {
            // A poisoned lock must not strand an open position unreviewed.
            return true;
        };
        seen.get(&ticket).is_none_or(|last| candle_time > *last)
    }

    /// Marks this position as reviewed on this candle.
    pub fn record(&self, ticket: i64, candle_time: i64) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        seen.insert(ticket, candle_time);
    }

    /// Forgets tickets that are no longer open, so the map tracks the book
    /// rather than every position the process has ever seen.
    pub fn retain(&self, open: &[i64]) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        seen.retain(|ticket, _| open.contains(ticket));
    }
}

/// Whether decisions are actually completing.
///
/// A model that refuses every request fails the tick, not the process: the
/// service stays healthy, the broker link stays live, and the console reads
/// green while nothing is ever decided. The only symptom is an absence of
/// trades, which is indistinguishable from a market offering nothing. This
/// counts consecutive failures so that absence becomes something a surface can
/// state outright.
#[derive(Debug, Default)]
pub struct DecisionHealth {
    consecutive_failures: std::sync::atomic::AtomicU32,
    last_failure: std::sync::Mutex<Option<(String, i64)>>,
}

impl DecisionHealth {
    /// Records a decision that reached a verdict, clearing any failure run.
    pub fn succeeded(&self) {
        self.consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut last) = self.last_failure.lock() {
            *last = None;
        }
    }

    /// Records a decision that could not be reached, and why.
    pub fn failed(&self, reason: &str, at: i64) {
        self.consecutive_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut last) = self.last_failure.lock() {
            // Bounded: this travels to a status payload, and a provider error
            // body can be long enough to bury everything around it.
            *last = Some((reason.chars().take(300).collect(), at));
        }
    }

    /// Consecutive failures since the last verdict.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The most recent failure and when it happened, while a run is unbroken.
    pub fn last_failure(&self) -> Option<(String, i64)> {
        self.last_failure.lock().ok().and_then(|last| last.clone())
    }
}

/// Judge answers, keyed to the candle they were formed on.
///
/// The judge is shown nothing but the market narrative, and that narrative is
/// derived entirely from the closed candles. The same candles therefore produce
/// the same answer, so an answer already held for an instrument's newest candle
/// is reused rather than bought again. Nothing else the tick knows — price,
/// equity, open positions — reaches the judge, which is what makes the candle
/// alone a sound key.
#[derive(Debug, Default)]
pub struct JudgementCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, (i64, Value)>>,
}

impl JudgementCache {
    /// The answer already held for this instrument's candle, if any.
    pub fn get(&self, symbol: &str, candle_time: i64) -> Option<Value> {
        let entries = self.entries.lock().ok()?;
        entries
            .get(symbol)
            .filter(|(seen, _)| *seen == candle_time)
            .map(|(_, value)| value.clone())
    }

    /// Holds one answer, replacing any older candle's answer for the symbol.
    pub fn put(&self, symbol: &str, candle_time: i64, value: Value) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.insert(symbol.to_owned(), (candle_time, value));
    }
}

impl EntryWatch {
    /// Whether any instrument changed enough to be worth a fresh proposal.
    ///
    /// An instrument never seen before always qualifies, so a restart or a
    /// newly configured symbol gets one look before the gate starts applying.
    pub fn should_evaluate(
        &self,
        observations: &[EntryObservation<'_>],
        fraction: f64,
        now: i64,
    ) -> bool {
        // A sweep that never reached a verdict left the gate marked anyway, so
        // the market it recorded was never actually judged. Re-ask once the
        // retry window has passed rather than waiting out the candle.
        if let Ok(failed_at) = self.failed_at.lock()
            && let Some(at) = *failed_at
            && now.saturating_sub(at) >= ENTRY_RETRY_AFTER_SECS
        {
            return true;
        }
        let Ok(seen) = self.seen.lock() else {
            // A poisoned lock must not silently stop trading; ask instead.
            return true;
        };
        observations.iter().any(|observation| {
            let Some(previous) = seen.get(observation.symbol) else {
                return true;
            };
            if observation.candle_time > previous.candle_time {
                return true;
            }
            let (Some(now), Some(before), Some(atr)) =
                (observation.price, previous.price, observation.atr)
            else {
                return false;
            };
            fraction > 0.0 && atr > 0.0 && (now - before).abs() >= atr * fraction
        })
    }

    /// Records the market each instrument was judged on.
    ///
    /// Only called once a sweep actually runs, so the threshold measures
    /// movement since the last real look rather than since the last tick.
    pub fn record(&self, observations: &[EntryObservation<'_>]) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        for observation in observations {
            seen.insert(
                observation.symbol.to_owned(),
                EntrySeen {
                    candle_time: observation.candle_time,
                    price: observation.price,
                },
            );
        }
    }

    /// Resets selected instruments to a post-close market baseline. A later
    /// proposal therefore needs a new candle or a fresh ATR-scaled move from
    /// the exit area rather than movement accumulated during the old trade.
    fn record_symbols(
        &self,
        observations: &[EntryObservation<'_>],
        symbols: &[String],
    ) -> Vec<String> {
        let selected: Vec<EntryObservation<'_>> = observations
            .iter()
            .copied()
            .filter(|observation| symbols.iter().any(|symbol| symbol == observation.symbol))
            .collect();
        self.record(&selected);
        selected
            .iter()
            .map(|observation| observation.symbol.to_owned())
            .collect()
    }

    /// Returns guarded symbols whose own market changed from the post-close
    /// baseline. Unlike the global sweep gate, movement elsewhere cannot make
    /// a recently closed symbol eligible.
    fn changed_symbols(
        &self,
        observations: &[EntryObservation<'_>],
        symbols: &[String],
        fraction: f64,
    ) -> Vec<String> {
        let Ok(seen) = self.seen.lock() else {
            // This gate is anti-churn. A poisoned lock therefore fails closed
            // for re-entry even though the ordinary entry sweep fails open.
            return Vec::new();
        };
        observations
            .iter()
            .filter(|observation| symbols.iter().any(|symbol| symbol == observation.symbol))
            .filter(|observation| {
                let Some(previous) = seen.get(observation.symbol) else {
                    return false;
                };
                if observation.candle_time > previous.candle_time {
                    return true;
                }
                let (Some(now), Some(before), Some(atr)) =
                    (observation.price, previous.price, observation.atr)
                else {
                    return false;
                };
                fraction > 0.0 && atr > 0.0 && (now - before).abs() >= atr * fraction
            })
            .map(|observation| observation.symbol.to_owned())
            .collect()
    }

    /// Notes that this sweep died before reaching a verdict, arming a retry.
    pub fn mark_failed(&self, now: i64) {
        if let Ok(mut failed_at) = self.failed_at.lock() {
            *failed_at = Some(now);
        }
    }

    /// Clears an armed retry once a sweep reaches a verdict of any kind.
    pub fn mark_settled(&self) {
        if let Ok(mut failed_at) = self.failed_at.lock() {
            *failed_at = None;
        }
    }
}

impl StopBasis {
    /// Records the first observed risk for every live position.
    fn observe(&self, positions: &[ManagedPosition]) {
        let mut risks = match self.risks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        risks.retain(|ticket, _| positions.iter().any(|position| position.ticket == *ticket));
        for position in positions {
            if risks.contains_key(&position.ticket)
                || position.stop_loss <= 0.0
                || position.entry <= 0.0
            {
                continue;
            }
            let observed = (position.entry - position.stop_loss).abs();
            if observed > 0.0 {
                risks.insert(position.ticket, observed);
            }
        }
    }

    /// Serializable snapshot of the entry-risk memory, keyed by ticket.
    pub fn state_snapshot(&self) -> Value {
        let risks = match self.risks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut entries = serde_json::Map::new();
        for (ticket, risk) in risks.iter() {
            entries.insert(ticket.to_string(), json!(risk));
        }
        Value::Object(entries)
    }

    /// Restores the entry-risk memory from a stored snapshot, replacing any
    /// current contents.
    ///
    /// # Errors
    /// Returns a description when the value is not a ticket-to-risk object.
    pub fn restore_state(&self, value: &Value) -> Result<(), String> {
        let Some(entries) = value.as_object() else {
            return Err("stop basis must be an object keyed by ticket".to_owned());
        };
        let mut restored = std::collections::HashMap::new();
        for (ticket, risk) in entries {
            let ticket: i64 = ticket
                .parse()
                .map_err(|_| format!("stop basis ticket `{ticket}` is not an integer"))?;
            let risk = risk
                .as_f64()
                .filter(|risk| risk.is_finite() && *risk > 0.0)
                .ok_or_else(|| format!("stop basis risk for ticket {ticket} is unusable"))?;
            restored.insert(ticket, risk);
        }
        let mut risks = match self.risks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *risks = restored;
        Ok(())
    }

    /// Remembered entry risk for one ticket.
    fn risk(&self, ticket: i64) -> Option<f64> {
        let risks = match self.risks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        risks.get(&ticket).copied()
    }
}

fn harvest_is_armed(
    position: &ManagedPosition,
    policy: &ProfitHarvestPolicy,
    basis: &StopBasis,
    book: &ProfitHarvestBook,
    server_time: i64,
) -> Option<(f64, f64)> {
    if !position.is_market
        || position.current <= 0.0
        || position.entry <= 0.0
        || position_age_secs(server_time, position.opened_at)? < policy.min_hold().as_secs()
    {
        return None;
    }
    let risk = basis.risk(position.ticket)?;
    if risk <= 0.0 {
        return None;
    }
    let favourable = match position.side {
        ManagedSide::Buy => position.current - position.entry,
        ManagedSide::Sell => position.entry - position.current,
    };
    let high = book.high_net_profit(position.ticket)?;
    if book.is_armed(position.ticket) {
        return Some((risk, high));
    }
    if favourable >= policy.arm_r() * risk && high >= policy.min_profit() {
        book.mark_armed(position.ticket);
        return Some((risk, high));
    }
    None
}

/// Plans a direct profitable close once an armed position gives back the
/// configured fraction of its observed high-water result.
fn harvest_close_plan(
    positions: &[ManagedPosition],
    policy: &ProfitHarvestPolicy,
    basis: &StopBasis,
    book: &ProfitHarvestBook,
    server_time: i64,
    now: i64,
) -> Option<HarvestClosePlan> {
    for position in positions {
        if book.close_guarded(position.ticket, now) {
            continue;
        }
        let Some((_, high)) = harvest_is_armed(position, policy, basis, book, server_time) else {
            continue;
        };
        let net = position.net_profit();
        if net <= 0.0 || high <= 0.0 || net >= high {
            continue;
        }
        let giveback = (high - net) / high;
        if giveback >= policy.giveback_fraction() {
            return Some(HarvestClosePlan {
                ticket: position.ticket,
                symbol: position.symbol.clone(),
                high_net_profit: high,
                net_profit: net,
            });
        }
    }
    None
}

/// Plans a broker-side profit ratchet as soon as harvesting arms. The stop is
/// computed from live close price and original risk, then the shared modify
/// path and terminal still enforce venue stop-distance rules.
fn harvest_stop_plan(
    positions: &[ManagedPosition],
    policy: &ProfitHarvestPolicy,
    basis: &StopBasis,
    book: &ProfitHarvestBook,
    server_time: i64,
) -> Option<StopMove> {
    for position in positions {
        let Some((risk, _)) = harvest_is_armed(position, policy, basis, book, server_time) else {
            continue;
        };
        if position.stop_loss <= 0.0 {
            continue;
        }
        let candidate = match position.side {
            ManagedSide::Buy => position.current - policy.trail_r() * risk,
            ManagedSide::Sell => position.current + policy.trail_r() * risk,
        };
        let locks_non_negative = match position.side {
            ManagedSide::Buy => candidate >= position.entry,
            ManagedSide::Sell => candidate <= position.entry,
        };
        if !locks_non_negative {
            continue;
        }
        let min_step = risk * STOP_MIN_STEP_RATIO;
        let improves = match position.side {
            ManagedSide::Buy => candidate >= position.stop_loss + min_step,
            ManagedSide::Sell => candidate <= position.stop_loss - min_step,
        };
        if improves {
            return Some(StopMove {
                ticket: position.ticket,
                stop: candidate,
                kind: StopMoveKind::ProfitHarvest,
            });
        }
    }
    None
}

/// Plans the most protective stop change for the first eligible position.
///
/// Two deterministic policies feed one decision: break-even (the stop moves
/// to the entry price once the trade has travelled `breakeven_r` times its
/// entry risk in favour) and trailing (`trail_r` times the risk is kept
/// behind the best favourable price once that distance is exceeded). The
/// winner is the most protective candidate that also improves the current
/// stop by at least one step, so stops never move backwards and churn is
/// bounded. Positions without a stop, without a current price, or that have
/// not moved in favour are skipped.
fn stop_plan(
    positions: &[ManagedPosition],
    breakeven_r: f64,
    trail_r: f64,
    basis: &StopBasis,
) -> Option<StopMove> {
    let breakeven_enabled = breakeven_r.is_finite() && breakeven_r > 0.0;
    let trail_enabled = trail_r.is_finite() && trail_r > 0.0;
    if !breakeven_enabled && !trail_enabled {
        return None;
    }
    basis.observe(positions);
    for position in positions {
        if position.stop_loss <= 0.0 || position.current <= 0.0 || position.entry <= 0.0 {
            continue;
        }
        let Some(risk) = basis.risk(position.ticket) else {
            continue;
        };
        if risk <= 0.0 {
            continue;
        }
        let (favourable, long) = match position.side {
            ManagedSide::Buy => (position.current - position.entry, true),
            ManagedSide::Sell => (position.entry - position.current, false),
        };
        if favourable <= 0.0 {
            continue;
        }
        let mut candidates: Vec<(f64, StopMoveKind)> = Vec::new();
        if breakeven_enabled && favourable >= breakeven_r * risk {
            candidates.push((position.entry, StopMoveKind::BreakEven));
        }
        if trail_enabled && favourable >= trail_r * risk {
            let trailing = if long {
                position.current - trail_r * risk
            } else {
                position.current + trail_r * risk
            };
            candidates.push((trailing, StopMoveKind::Trail));
        }

        let min_step = risk * STOP_MIN_STEP_RATIO;
        let mut best: Option<(f64, StopMoveKind)> = None;
        for (candidate, kind) in candidates {
            let improves = if long {
                candidate >= position.stop_loss + min_step
            } else {
                candidate <= position.stop_loss - min_step
            };
            if !improves {
                continue;
            }
            let better = match best {
                None => true,
                Some((current, _)) => {
                    if long {
                        candidate > current
                    } else {
                        candidate < current
                    }
                }
            };
            if better {
                best = Some((candidate, kind));
            }
        }
        if let Some((stop, kind)) = best {
            return Some(StopMove {
                ticket: position.ticket,
                stop,
                kind,
            });
        }
    }
    None
}

/// Position age in seconds when both broker timestamps allow it.
fn position_age_secs(server_time: i64, opened_at: i64) -> Option<u64> {
    if server_time <= 0 || opened_at <= 0 || server_time < opened_at {
        return None;
    }
    u64::try_from(server_time - opened_at).ok()
}

/// The reviewer's parsed decision.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReviewDecision {
    /// Keep the position and its bracket.
    Hold,
    /// Flatten the given ticket.
    Close(i64),
}

/// Parses the constrained review answer.
pub(crate) fn parse_review(value: &serde_json::Value) -> Result<ReviewDecision, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        action: String,
        #[serde(default)]
        ticket: Option<i64>,
        /// Journal metadata, extracted separately by `parse_rationale`.
        #[serde(default)]
        #[allow(dead_code)]
        rationale: Option<String>,
    }
    let answer: Answer = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid review answer: {error}"))?;
    match answer.action.as_str() {
        "hold" => Ok(ReviewDecision::Hold),
        "close" => match answer.ticket {
            Some(ticket) if ticket > 0 => Ok(ReviewDecision::Close(ticket)),
            _ => Err("close requires a positive ticket".to_owned()),
        },
        other => Err(format!("unknown review action `{other}`")),
    }
}

/// Reviews the open managed position: hold, or close when the entry thesis no
/// longer holds.
///
/// `weekend` is passed when the review is taken inside the pre-close window:
/// the prompt then carries the weekend trade-off (gap risk against the cost of
/// flattening), and `origin` names which question produced the verdict so the
/// journal can tell the two apart.
async fn review_positions(
    state: &AppState,
    settings: &AutopilotSettings,
    session: &AgentSession<'_>,
    series: &CandleSeries,
    positions: &[ManagedPosition],
    weekend: Option<WeekendPrep>,
    origin: &'static str,
) -> TickOutcome {
    let judgements = judgement_for_symbol(session.judgements, series.symbol().as_str());
    let instructions = review_instructions(series, positions, weekend);
    let input = review_input(state, series, positions, judgements, weekend);
    let outcome = match agent::run(session, &instructions, &input).await {
        Ok(outcome) => {
            state.decision_health().succeeded();
            outcome
        }
        Err(error) => {
            let reason = match error {
                PipelineError::AgentLoopLimit { reason } => format!("agent loop: {reason}"),
                other => format!("model unavailable: {other}"),
            };
            state
                .decision_health()
                .failed(&reason, unix_secs(state.now()));
            record(
                state,
                "unavailable",
                Some(series.symbol()),
                None,
                Some(&reason),
            )
            .await;
            return TickOutcome::Unavailable { reason };
        }
    };

    let tool_names: Vec<String> = outcome
        .tool_calls
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    let context = DecisionContext {
        rationale: outcome.rationale.as_deref(),
        judgements,
        tool_names: Some(&tool_names),
    };
    let decision = match outcome.decision {
        AgentDecision::Review(decision) => decision,
        AgentDecision::Proposal(_) => {
            record_position_context(
                state,
                "close_rejected",
                origin,
                series,
                None,
                Some("agent returned a proposal for a review"),
                None,
                context,
            )
            .await;
            return TickOutcome::Rejected {
                code: "invalid_review",
            };
        }
    };

    match decision {
        ReviewDecision::Hold => {
            record_position_context(
                state,
                "held",
                origin,
                series,
                Some(positions[0].ticket),
                None,
                None,
                context,
            )
            .await;
            TickOutcome::Held
        }
        ReviewDecision::Close(ticket) => {
            let Some(position) = positions.iter().find(|position| position.ticket == ticket) else {
                record_position_context(
                    state,
                    "close_rejected",
                    origin,
                    series,
                    Some(ticket),
                    Some("unknown_ticket"),
                    None,
                    context,
                )
                .await;
                return TickOutcome::Rejected {
                    code: "unknown_ticket",
                };
            };
            let snapshot_server_time = state
                .broker()
                .and_then(|broker| broker.link().last_account())
                .map(|snapshot| snapshot.server_time)
                .unwrap_or(0);
            match position_age_secs(snapshot_server_time, position.opened_at) {
                Some(age) if age >= settings.min_hold().as_secs() => {}
                Some(_) => {
                    record_position_context(
                        state,
                        "close_rejected",
                        origin,
                        series,
                        Some(ticket),
                        Some("position_too_young"),
                        None,
                        context,
                    )
                    .await;
                    return TickOutcome::Rejected {
                        code: "position_too_young",
                    };
                }
                None => {
                    record_position_context(
                        state,
                        "close_rejected",
                        origin,
                        series,
                        Some(ticket),
                        Some("position_age_unknown"),
                        None,
                        context,
                    )
                    .await;
                    return TickOutcome::Rejected {
                        code: "position_age_unknown",
                    };
                }
            }
            match queue_staged_close(state, ticket).await {
                StagedClose::Queued { command, ticket } => {
                    let command_id = command.to_string();
                    record_position_context(
                        state,
                        "close_queued",
                        origin,
                        series,
                        Some(ticket),
                        None,
                        Some(&command_id),
                        context,
                    )
                    .await;
                    TickOutcome::CloseQueued {
                        command: command.to_string(),
                    }
                }
                failed => {
                    let (label, reason, outcome) = close_refusal(&failed);
                    record_position_context(
                        state,
                        label,
                        origin,
                        series,
                        Some(ticket),
                        Some(&reason),
                        None,
                        context,
                    )
                    .await;
                    outcome
                }
            }
        }
    }
}

/// Submits one planned stop change through the shared modify path.
async fn move_stop(state: &AppState, symbol: &str, plan: StopMove) -> TickOutcome {
    match queue_staged_modify(state, plan.ticket, Some(plan.stop), None).await {
        StagedModify::Queued { command, ticket } => {
            let command_id = command.to_string();
            record_symbol_event(
                state,
                plan.kind.as_str(),
                "autopilot",
                symbol,
                Some(ticket),
                None,
                Some(&command_id),
            )
            .await;
            TickOutcome::StopMoved {
                command: command.to_string(),
            }
        }
        StagedModify::TradingDisabled => {
            record_symbol_event(
                state,
                "stop_rejected",
                "autopilot",
                symbol,
                Some(plan.ticket),
                Some("trading_disabled"),
                None,
            )
            .await;
            TickOutcome::Rejected {
                code: "trading_disabled",
            }
        }
        StagedModify::ChannelUnavailable => {
            let reason = "command channel unavailable".to_owned();
            record_symbol_event(
                state,
                "stop_rejected",
                "autopilot",
                symbol,
                Some(plan.ticket),
                Some(&reason),
                None,
            )
            .await;
            TickOutcome::Unavailable { reason }
        }
        StagedModify::NoPositions | StagedModify::UnknownTicket => {
            record_symbol_event(
                state,
                "stop_rejected",
                "autopilot",
                symbol,
                Some(plan.ticket),
                Some("stale_position"),
                None,
            )
            .await;
            TickOutcome::Rejected {
                code: "stale_position",
            }
        }
        StagedModify::NotVeyra => {
            record_symbol_event(
                state,
                "stop_rejected",
                "autopilot",
                symbol,
                Some(plan.ticket),
                Some("not_a_veyra_position"),
                None,
            )
            .await;
            TickOutcome::Rejected {
                code: "not_a_veyra_position",
            }
        }
    }
}

/// Banks one armed high-water retracement through the same ownership-checked
/// staged close path used by the operator and AI reviewer.
async fn close_harvest(state: &AppState, plan: HarvestClosePlan) -> TickOutcome {
    match queue_staged_close(state, plan.ticket).await {
        StagedClose::Queued { command, ticket: _ } => {
            state
                .profit_harvest_book()
                .guard_close(plan.ticket, unix_secs(state.now()));
            let command_id = command.to_string();
            record_harvest_event(
                state,
                "profit_harvest_close",
                &plan,
                None,
                Some(&command_id),
            )
            .await;
            TickOutcome::CloseQueued {
                command: command_id,
            }
        }
        failed => {
            let (label, reason, outcome) = close_refusal(&failed);
            record_harvest_event(state, label, &plan, Some(&reason), None).await;
            outcome
        }
    }
}

/// Instructions for the hold-or-close review.
///
/// Inside the pre-close window the same review carries the weekend question:
/// the position would sit through Sunday's reopen with its stop resting at the
/// broker, so the gap enters the hold-or-close decision.
fn review_instructions(
    series: &CandleSeries,
    positions: &[ManagedPosition],
    weekend: Option<WeekendPrep>,
) -> String {
    let tickets: Vec<String> = positions
        .iter()
        .map(|position| position.ticket.to_string())
        .collect();
    let weekend_rules = match weekend {
        Some(prep) => format!(
            " This is the weekend checkpoint: the market closes at {closes} UTC, in {left}. A          position held past it sits through Sunday's reopen with its stop resting at the broker,          where a weekend gap can open beyond it; holding across the weekend also accrues the          broker's swap. Flattening costs the spread and gives up the bracket, so close only when          the gap exposure is not justified by the position's stop distance and the latest          evidence; otherwise hold.",
            closes = utc_clock(prep.closes_at),
            left = duration_phrase(prep.closes_in_secs)
        ),
        None => String::new(),
    };
    format!(
        "You are the analyst for Veyra, a single-instrument trading bot. Your open {symbol}          {timeframe} position(s) (ticket(s) {tickets}) were entered by this bot with a stop loss          and take profit attached.
         Decide for the reported position: `hold` keeps the entry bracket and lets the plan play          out; `close` flattens the ticket now because the thesis that justified the entry is no          longer supported by the latest candles and judgements.
         Closing costs the spread and abandons the bracket, so hold unless the evidence has          genuinely shifted; do not close merely because the position shows a small loss — the          attached stop defines the risk.{weekend_rules}
         Answer with the provided schema only, including the ticket when you close, and a short `rationale` (a sentence or two, at most 280 characters) explaining the decision. Before answering you may call read-only tools: get_market(symbol, timeframe?, bars?), get_judgements(symbol), get_account(), get_positions(), and get_market_window(). Call a tool only when its result would change your decision.",
        symbol = series.symbol().as_str(),
        timeframe = series.timeframe().as_str(),
        tickets = tickets.join(", "),
        weekend_rules = weekend_rules
    )
}

/// `21:00` style UTC clock for prompts and events.
fn utc_clock(unix: i64) -> String {
    let minute = unix.rem_euclid(86_400) / 60;
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// Human phrase for a stretch that remains: `1h 05m` or `45m`.
fn duration_phrase(secs: i64) -> String {
    let minutes = (secs / 60).max(0);
    if minutes >= 60 {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

/// Model input for the review: the same market block plus the position and
/// its bracket, and the weekend close when the review is taken inside the
/// pre-close window.
fn review_input(
    state: &AppState,
    series: &CandleSeries,
    positions: &[ManagedPosition],
    judgements: Option<&Value>,
    weekend: Option<WeekendPrep>,
) -> String {
    let recent: Vec<Value> = series
        .candles()
        .iter()
        .rev()
        .take(RECENT_CANDLES)
        .collect::<Vec<&Candle>>()
        .into_iter()
        .rev()
        .map(|candle| {
            json!({
                "t": candle.time(),
                "o": candle.open(),
                "h": candle.high(),
                "l": candle.low(),
                "c": candle.close()
            })
        })
        .collect();

    let snapshot = state
        .broker()
        .and_then(|broker| broker.link().last_account());
    let server_time = snapshot
        .as_ref()
        .map(|snapshot| snapshot.server_time)
        .unwrap_or(0);
    let open_positions: Vec<Value> = positions
        .iter()
        .map(|position| {
            json!({
                "ticket": position.ticket,
                "side": position.side.as_str(),
                "lots": position.lots,
                "entry": position.entry,
                "profit": position.profit,
                "swap": position.swap,
                "commission": position.commission,
                "net_profit": position.net_profit(),
                "stop_loss": position.stop_loss,
                "take_profit": position.take_profit,
                "age_secs": position_age_secs(server_time, position.opened_at)
            })
        })
        .collect();

    let mut input = json!({
        "symbol": series.symbol().as_str(),
        "timeframe": series.timeframe().as_str(),
        "market": {
            "bars": series.candles().len(),
            "last_close": series.last().map(|candle| candle.close()),
            "window_high": window_high(series),
            "window_low": window_low(series),
            "change_pct": change_pct(series),
            "atr14": average_true_range(series, ATR_PERIOD),
            "recent": recent
        },
        "open_positions": open_positions
    });
    if let Some(judgements) = judgements {
        input["judgements"] = judgements.clone();
    }
    if let Some(prep) = weekend {
        input["weekend"] = json!({
            "market_closes_at": prep.closes_at,
            "market_closes_in_secs": prep.closes_in_secs
        });
    }
    if let Some(snapshot) = &snapshot {
        input["account"] = json!({
            "free_margin": snapshot.free_margin,
            "margin_level": snapshot.margin_level,
            "leverage": snapshot.leverage
        });
    }
    input.to_string()
}

/// Optional model context journaled alongside a proposal event: the model's
/// short rationale and the judgements of the instrument it decided on.
#[derive(Debug, Default, Clone, Copy)]
struct DecisionContext<'a> {
    rationale: Option<&'a str>,
    judgements: Option<&'a Value>,
    /// Read-only agent tools used before this decision, in call order.
    tool_names: Option<&'a [String]>,
}

/// Records one review decision with the model's rationale and judgements.
/// Position records always come from a review, so the context is required.
#[allow(clippy::too_many_arguments)]
async fn record_position_context(
    state: &AppState,
    outcome: &'static str,
    origin: &'static str,
    series: &CandleSeries,
    ticket: Option<i64>,
    reason: Option<&str>,
    command_id: Option<&str>,
    context: DecisionContext<'_>,
) {
    record_symbol_event_context(
        state,
        outcome,
        origin,
        series.symbol().as_str(),
        ticket,
        reason,
        command_id,
        context,
    )
    .await;
}

/// Records one position decision against a symbol, for callers that no longer
/// hold the market series (deterministic stop moves).
async fn record_symbol_event(
    state: &AppState,
    outcome: &'static str,
    origin: &'static str,
    symbol: &str,
    ticket: Option<i64>,
    reason: Option<&str>,
    command_id: Option<&str>,
) {
    record_symbol_event_context(
        state,
        outcome,
        origin,
        symbol,
        ticket,
        reason,
        command_id,
        DecisionContext::default(),
    )
    .await;
}

/// Records the money high-water context for an automatic profitable close.
async fn record_harvest_event(
    state: &AppState,
    outcome: &'static str,
    plan: &HarvestClosePlan,
    reason: Option<&str>,
    command_id: Option<&str>,
) {
    let Some(audit) = state.audit() else {
        return;
    };
    let mut payload = json!({
        "outcome": outcome,
        "origin": "profit_harvest",
        "symbol": plan.symbol,
        "ticket": plan.ticket,
        "high_net_profit": plan.high_net_profit,
        "net_profit": plan.net_profit
    });
    if let Some(reason) = reason {
        payload["reason"] = json!(reason);
    }
    if let Some(command_id) = command_id {
        payload["command_id"] = json!(command_id);
    }
    audit
        .try_record(AuditEvent::new(AuditKind::ProposalEvaluated, payload))
        .await;
}

/// Records one position decision with model context.
#[allow(clippy::too_many_arguments)]
async fn record_symbol_event_context(
    state: &AppState,
    outcome: &'static str,
    origin: &'static str,
    symbol: &str,
    ticket: Option<i64>,
    reason: Option<&str>,
    command_id: Option<&str>,
    context: DecisionContext<'_>,
) {
    let Some(audit) = state.audit() else {
        return;
    };
    let mut payload = json!({
        "outcome": outcome,
        "origin": origin,
        "symbol": symbol
    });
    if let Some(ticket) = ticket {
        payload["ticket"] = json!(ticket);
    }
    if let Some(reason) = reason {
        payload["reason"] = json!(reason);
    }
    if let Some(command_id) = command_id {
        payload["command_id"] = json!(command_id);
    }
    if let Some(rationale) = context.rationale {
        payload["rationale"] = json!(rationale);
    }
    if let Some(judgements) = context.judgements {
        payload["judgements"] = judgements.clone();
    }
    if let Some(names) = context.tool_names
        && !names.is_empty()
    {
        payload["agent_tool_calls"] = json!(names.len());
        payload["agent_tools"] = json!(names);
    }
    audit
        .try_record(AuditEvent::new(AuditKind::ProposalEvaluated, payload))
        .await;
}

/// Records one decision attempt; best-effort and bounded in size.
async fn record(
    state: &AppState,
    outcome: &'static str,
    symbol: Option<&Symbol>,
    draft: Option<&TradeIntentDraft>,
    reason: Option<&str>,
) {
    record_event(state, outcome, symbol, draft, reason, None, None).await;
}

/// Records one decision attempt together with the identifiers that connect
/// it to the rest of the journal: the approved intent and the command it
/// produced. A position ticket can then be traced back to its decision
/// through the command.
async fn record_event(
    state: &AppState,
    outcome: &'static str,
    symbol: Option<&Symbol>,
    draft: Option<&TradeIntentDraft>,
    reason: Option<&str>,
    intent_id: Option<&str>,
    command_id: Option<&str>,
) {
    record_event_context(
        state,
        outcome,
        symbol,
        draft,
        reason,
        intent_id,
        command_id,
        DecisionContext::default(),
    )
    .await;
}

/// Records one decision attempt with the model's rationale and judgements.
#[allow(clippy::too_many_arguments)]
async fn record_event_context(
    state: &AppState,
    outcome: &'static str,
    symbol: Option<&Symbol>,
    draft: Option<&TradeIntentDraft>,
    reason: Option<&str>,
    intent_id: Option<&str>,
    command_id: Option<&str>,
    context: DecisionContext<'_>,
) {
    let Some(audit) = state.audit() else {
        return;
    };
    let mut payload = json!({ "outcome": outcome, "origin": "autopilot" });
    if let Some(symbol) = symbol {
        payload["symbol"] = json!(symbol.as_str());
    }
    if let Some(draft) = draft {
        payload["side"] = json!(draft.side().as_str());
        payload["volume"] = json!(draft.volume().value());
        payload["order_type"] = json!(draft.order().as_str());
        if let Some(stop) = draft.stop_loss() {
            payload["stop_loss"] = json!(stop.value());
        }
        if let Some(target) = draft.take_profit() {
            payload["take_profit"] = json!(target.value());
        }
    }
    if let Some(reason) = reason {
        payload["reason"] = json!(reason);
    }
    if let Some(intent_id) = intent_id {
        payload["intent_id"] = json!(intent_id);
    }
    if let Some(command_id) = command_id {
        payload["command_id"] = json!(command_id);
    }
    if let Some(rationale) = context.rationale {
        payload["rationale"] = json!(rationale);
    }
    if let Some(judgements) = context.judgements {
        payload["judgements"] = judgements.clone();
    }
    if let Some(names) = context.tool_names
        && !names.is_empty()
    {
        payload["agent_tool_calls"] = json!(names.len());
        payload["agent_tools"] = json!(names);
    }
    audit
        .try_record(AuditEvent::new(AuditKind::ProposalEvaluated, payload))
        .await;
}

/// Asks the judgement engine for calibrated answers over the same market
/// state, returning a compact JSON summary for the model input.
pub(crate) async fn judgements_for(
    jev: &JevRuntime,
    series: &CandleSeries,
) -> Result<Value, String> {
    let state = JevState::text(&market_narrative(series)).map_err(|error| error.to_string())?;
    let instructions = |text: &str| Instructions::text(text).map_err(|error| error.to_string());

    let mut questions = BTreeMap::new();
    questions.insert(
        "direction".to_owned(),
        Question::choice(
            instructions("Which direction has the strongest evidence for the next few candles?")?,
            ChoiceOptions::new([
                (
                    "long".to_owned(),
                    Some("Evidence favours buying".to_owned()),
                ),
                (
                    "short".to_owned(),
                    Some("Evidence favours selling".to_owned()),
                ),
                ("flat".to_owned(), Some("No directional edge".to_owned())),
            ])
            .map_err(|error| error.to_string())?,
        ),
    );
    questions.insert(
        "trending".to_owned(),
        Question::noul(
            instructions(
                "Does the market described look like a trending market rather than a range?",
            )?,
            NoulCriteria::default(),
        ),
    );
    questions.insert(
        "momentum".to_owned(),
        Question::score(
            instructions("How strong is the directional momentum?")?,
            ScoreLevels::new(["Weak".to_owned(), "Neutral".to_owned(), "Strong".to_owned()])
                .map_err(|error| error.to_string())?,
        ),
    );

    let request = JevRequest::new(state, questions).map_err(|error| error.to_string())?;
    let response = jev
        .evaluate(request.clone())
        .await
        .map_err(|error| error.to_string())?;
    request
        .validate_answers(&response)
        .map_err(|error| error.to_string())?;

    let mut summary = serde_json::Map::new();
    for (id, answer) in response.answers() {
        let value = match answer {
            Answer::Choice(choice) => json!({
                "choice": choice.choice(),
                "confidence": choice.confidence().value(),
                "probabilities": choice
                    .probabilities()
                    .iter()
                    .map(|(option, probability)| (option.clone(), json!(probability.value())))
                    .collect::<serde_json::Map<String, Value>>()
            }),
            Answer::Noul(noul) => json!({ "probability": noul.probability().value() }),
            Answer::Score(score) => json!({
                "score": score.score(),
                "confidence": score.confidence().value(),
                "legend": score.legend()
            }),
        };
        summary.insert(id.clone(), value);
    }
    Ok(Value::Object(summary))
}

/// Describes the series in one compact paragraph for judgement questions.
fn market_narrative(series: &CandleSeries) -> String {
    let symbol = series.symbol().as_str();
    let timeframe = series.timeframe().as_str();
    let closes: Vec<String> = series
        .candles()
        .iter()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|candle| format!("{:.5}", candle.close()))
        .collect();
    match (series.candles().first(), series.last()) {
        (Some(first), Some(last)) => format!(
            "{symbol} {timeframe}: {} closed candles, first close {:.5}, last close {:.5}, window high {:.5}, window low {:.5}, change {:.2}%. Last closes: {}.",
            series.candles().len(),
            first.close(),
            last.close(),
            window_high(series),
            window_low(series),
            change_pct(series),
            closes.join(", ")
        ),
        _ => format!("{symbol} {timeframe}: no closed candles available."),
    }
}

/// Assembles the structured model input for one proposal.
fn proposal_input(
    markets: &[(Symbol, CandleSeries)],
    judgements: &[(Symbol, Value)],
    account: &AccountFacts,
    managed: &[ManagedPosition],
    specs: &[(Symbol, SymbolSpecPayload)],
    events: &[CalendarEvent],
    now: i64,
) -> String {
    let assets: Vec<Value> = markets
        .iter()
        .map(|(symbol, series)| {
            let recent: Vec<Value> = series
                .candles()
                .iter()
                .rev()
                .take(RECENT_CANDLES)
                .collect::<Vec<&Candle>>()
                .into_iter()
                .rev()
                .map(|candle| {
                    json!({
                        "t": candle.time(),
                        "o": candle.open(),
                        "h": candle.high(),
                        "l": candle.low(),
                        "c": candle.close()
                    })
                })
                .collect();
            let mut asset = json!({
                "symbol": symbol.as_str(),
                "market": {
                    "bars": series.candles().len(),
                    "last_close": series.last().map(|candle| candle.close()),
                    "window_high": window_high(series),
                    "window_low": window_low(series),
                    "change_pct": change_pct(series),
                    "atr14": average_true_range(series, ATR_PERIOD),
                    "recent": recent
                }
            });
            if let Some(spec) = symbol_spec_for(specs, symbol.as_str()) {
                asset["contract"] = json!({
                    "digits": spec.digits,
                    "point": spec.point,
                    "spread_points": spec.spread_points,
                    "stop_level_points": spec.stop_level_points,
                    "lot_min": spec.lot_min,
                    "lot_max": spec.lot_max,
                    "lot_step": spec.lot_step,
                    "tick_value": spec.tick_value,
                    "margin_required": spec.margin_required,
                    "swap_long": spec.swap_long,
                    "swap_short": spec.swap_short,
                    "trade_allowed": spec.trade_allowed
                });
            }
            let upcoming = calendar::upcoming(events, symbol.as_str(), now, CALENDAR_HORIZON_SECS);
            if !upcoming.is_empty() {
                asset["upcoming_events"] = json!(
                    upcoming
                        .iter()
                        .take(MAX_LISTED_EVENTS)
                        .map(|event| json!({
                            "title": event.title(),
                            "currency": event.currency(),
                            "impact": event.impact().as_str(),
                            "in_minutes": (event.time() - now) / 60
                        }))
                        .collect::<Vec<_>>()
                );
            }
            if let Some(summary) = judgement_for_symbol(judgements, symbol.as_str()) {
                asset["judgements"] = summary.clone();
            }
            if let Some(position) = managed
                .iter()
                .find(|position| position.symbol == symbol.as_str())
            {
                asset["open_position"] = json!({
                    "ticket": position.ticket,
                    "side": position.side.as_str(),
                    "lots": position.lots,
                    "entry": position.entry,
                    "profit": position.profit,
                    "stop_loss": position.stop_loss,
                    "take_profit": position.take_profit
                });
            }
            asset
        })
        .collect();

    json!({
        "timeframe": markets
            .first()
            .map(|(_, series)| series.timeframe().as_str())
            .unwrap_or("H4"),
        "assets": assets,
        "account": {
            "open_orders": account.open_orders,
            "open_lots": account.open_lots,
            "free_margin": account.free_margin,
            "trade_allowed": account.trade_allowed,
            "open_symbols": account
                .open_symbols
                .iter()
                .map(|symbol| symbol.as_str())
                .collect::<Vec<_>>()
        }
    })
    .to_string()
}

/// Assembles the analyst instructions, including the enforced volume cap.
fn proposal_instructions(
    state: &AppState,
    markets: &[(Symbol, CandleSeries)],
    account: &AccountFacts,
) -> String {
    let policy = state.risk().policy();
    let menu = markets
        .iter()
        .map(|(symbol, _)| symbol.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are the analyst for Veyra, a systematic multi-asset trading bot. You are given a menu of          instruments ({menu}) with recent {timeframe} candles and optional calibrated judgements; the last          candle of each block is the most recent closed bar.\n         Decide for each instrument independently whether the evidence justifies opening a position right          now. You may open at most one instrument per answer. If none of them is suitable, answer `none`          \u{2014} skipping is normal and expected, and every instrument is reconsidered on the next tick.\n         Use the symbol exactly as written. Constraints: at most {max_orders} open orders and {max_total}          lots total exposure ({open_lots} lots currently open), one position per instrument, and volume at          most {max_volume} lots. If you open, use a market order \u{2014} omit `price` entirely \u{2014} with          both `stop_loss` and `take_profit` as absolute prices bracketing the entry, and stay within the          instrument's own price scale. Omit `comment` entirely (the bot annotates orders itself). A          deterministic risk gate re-validates everything and will reject anything outside these limits;          rejections are expected outcomes, not errors. Each asset reports its venue contract \u{2014} spread and stop level in points, lot band and step, margin per lot \u{2014} plus ATR(14); size the volume and stop distance so the order lands on the lot grid, fits the free margin shown in the account block, and keeps the stop outside the spread and the minimum stop level. High-impact events blackout entries for their currencies around the release; the `upcoming_events` list shows what is scheduled, so avoid fighting a print. Always include a short `rationale` (at most 280 characters) explaining why this instrument and direction `-` or, when answering none, why no instrument qualifies; the operator sees it in the decision journal. Before answering you may call read-only tools: get_judgements(symbol) for calibrated probabilities, get_market(symbol, timeframe?, bars?) for another window, get_account(), get_positions(), get_market_window() for session and rollover state, and check_risk(intent) to dry-run a draft through the deterministic gate. Call a tool only when its result would change your decision; otherwise answer none or open.",
        menu = menu,
        timeframe = markets
            .first()
            .map(|(_, series)| series.timeframe().as_str())
            .unwrap_or("H4"),
        max_orders = policy.max_open_orders(),
        max_total = policy.max_total_lots().value(),
        open_lots = account.open_lots,
        max_volume = policy.max_volume_per_order().value()
    )
}

/// Highest candle high across the window.
pub(crate) fn window_high(series: &CandleSeries) -> f64 {
    series
        .candles()
        .iter()
        .map(|candle| candle.high())
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Lowest candle low across the window.
pub(crate) fn window_low(series: &CandleSeries) -> f64 {
    series
        .candles()
        .iter()
        .map(|candle| candle.low())
        .fold(f64::INFINITY, f64::min)
}

/// Simple average true range over the last `period` completed candles,
/// rounded to five decimals to keep the model input compact. Returns `None`
/// when the window cannot be measured: fewer than `period + 1` candles means a
/// true range would lack its previous close.
pub(crate) fn average_true_range(series: &CandleSeries, period: usize) -> Option<f64> {
    if period == 0 {
        return None;
    }
    let candles = series.candles();
    if candles.len() < period + 1 {
        return None;
    }
    let start = candles.len() - period;
    let mut sum = 0.0;
    for index in start..candles.len() {
        let candle = &candles[index];
        let previous_close = candles[index - 1].close();
        let true_range = (candle.high() - candle.low())
            .max((candle.high() - previous_close).abs())
            .max((candle.low() - previous_close).abs());
        sum += true_range;
    }
    let atr = sum / period as f64;
    Some((atr * 100_000.0).round() / 100_000.0)
}

/// Percentage change from the first to the last close, rounded to four
/// decimals to keep the model input compact.
pub(crate) fn change_pct(series: &CandleSeries) -> f64 {
    match (series.candles().first(), series.last()) {
        (Some(first), Some(last)) if first.close() > 0.0 => {
            let change = (last.close() - first.close()) / first.close() * 100.0;
            (change * 10_000.0).round() / 10_000.0
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::audit::{AuditRuntime, MemoryTrail};
    use crate::broker::{AccountLogin, AccountSnapshot, BrokerRuntime, BrokerSettings, ServerName};
    use crate::broker::{AccountSnapshotPayload, CommandKind};
    use crate::calendar::{
        CalendarError, CalendarProvider, CalendarRuntime, EventCalendar, Impact,
    };
    use crate::config::ServiceConfig;
    use crate::jev::{
        JevError, JevProvider, JevRequest as JudgeRequest, JevResponse, SemanticJudge,
    };
    use crate::market::{MarketError, MarketFeed, MarketProvider, MarketRuntime};
    use crate::model::{
        DecisionAnswer, DecisionEngine, DecisionRequest, ModelError, ModelProvider, ModelRuntime,
    };
    use crate::risk::{RiskGate, RiskPolicy};
    use crate::trading::intent::Volume;

    /// Engine that records every request and returns one canned answer.
    #[derive(Debug)]
    struct StubEngine {
        answer: Option<Value>,
        review: Option<Value>,
        seen: Mutex<Vec<DecisionRequest>>,
    }

    impl StubEngine {
        fn answering(answer: Value) -> Arc<Self> {
            Arc::new(Self {
                answer: Some(answer),
                review: None,
                seen: Mutex::new(Vec::new()),
            })
        }

        /// Answers the position-review and entry schemas differently, as a
        /// tick that holds a position and then evaluates entries needs.
        fn answering_review(review: Value, proposal: Value) -> Arc<Self> {
            Arc::new(Self {
                answer: Some(proposal),
                review: Some(review),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                answer: None,
                review: None,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<DecisionRequest> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl DecisionEngine for StubEngine {
        fn provider(&self) -> ModelProvider {
            ModelProvider::OpenRouter
        }

        async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
            let is_review = request.format.schema["properties"]["action"]["enum"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|action| action == "hold"));
            self.seen.lock().expect("lock").push(request);
            let selected = if is_review {
                self.review.as_ref().or(self.answer.as_ref())
            } else {
                self.answer.as_ref()
            };
            match selected {
                Some(value) => Ok(DecisionAnswer {
                    value: value.clone(),
                }),
                None => Err(ModelError::Request {
                    reason: "provider down".to_owned(),
                }),
            }
        }
    }

    /// Feed that hands out a deterministic rising series.
    #[derive(Debug)]
    struct StubFeed {
        bars: u16,
        fail: bool,
        /// Contract override; `None` serves the canned default that all
        /// existing entry tests rely on.
        spec: Option<SymbolSpecPayload>,
        /// Forces contract requests to fail while candles still answer, so
        /// the unavailable-contract path is reachable.
        spec_fail: bool,
    }

    #[async_trait]
    impl MarketFeed for StubFeed {
        fn provider(&self) -> MarketProvider {
            MarketProvider::Ea
        }

        async fn candles(&self, request: CandleRequest) -> Result<CandleSeries, MarketError> {
            if self.fail {
                return Err(MarketError::Unavailable {
                    reason: "terminal down".to_owned(),
                });
            }
            let candles = (0..self.bars)
                .map(|index| {
                    let index = f64::from(index);
                    Candle::from_validated(
                        1_700_000_000 + (index as i64) * 14_400,
                        1.09,
                        1.10,
                        1.08,
                        1.09 + index * 0.0001,
                        100,
                    )
                })
                .collect();
            Ok(CandleSeries::from_validated(
                request.symbol().clone(),
                request.timeframe(),
                candles,
            ))
        }

        async fn symbol_spec(&self, symbol: &Symbol) -> Result<SymbolSpecPayload, MarketError> {
            if self.fail || self.spec_fail {
                return Err(MarketError::Unavailable {
                    reason: "terminal down".to_owned(),
                });
            }
            Ok(self
                .spec
                .clone()
                .unwrap_or_else(|| canned_spec(symbol.as_str())))
        }
    }

    /// Calendar stub answering from a fixed event list or failing outright.
    #[derive(Debug)]
    struct StubCalendar {
        events: Vec<CalendarEvent>,
        fail: bool,
    }

    #[async_trait]
    impl EventCalendar for StubCalendar {
        fn provider(&self) -> CalendarProvider {
            CalendarProvider::Forexfactory
        }

        async fn events(&self, from: i64, until: i64) -> Result<Vec<CalendarEvent>, CalendarError> {
            if self.fail {
                return Err(CalendarError::Transport {
                    reason: "feed down".to_owned(),
                });
            }
            Ok(self
                .events
                .iter()
                .filter(|event| event.time() >= from && event.time() < until)
                .cloned()
                .collect())
        }
    }

    /// Contract every stub symbol reports unless a test overrides it: the
    /// values IFC Markets publishes for EURUSD-class pairs.
    fn canned_spec(symbol: &str) -> SymbolSpecPayload {
        SymbolSpecPayload {
            symbol: symbol.to_owned(),
            digits: 5,
            point: 0.00001,
            bid: 1.1,
            ask: 1.10012,
            spread_points: 12,
            stop_level_points: 5,
            freeze_level_points: 0,
            lot_min: 0.01,
            lot_max: 100.0,
            lot_step: 0.01,
            tick_value: 0.1,
            tick_size: 0.00001,
            margin_required: 3.29,
            swap_long: -0.72,
            swap_short: -0.31,
            swap_type: 0,
            trade_allowed: true,
        }
    }

    /// Judge that records requests and returns a canned response or fails.
    #[derive(Debug)]
    struct StubJudge {
        response: Option<Value>,
        seen: Mutex<Vec<JudgeRequest>>,
    }

    impl StubJudge {
        fn responding(response: Value) -> Arc<Self> {
            Arc::new(Self {
                response: Some(response),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                response: None,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<JudgeRequest> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl SemanticJudge for StubJudge {
        fn provider(&self) -> JevProvider {
            JevProvider::TypeSafe
        }

        async fn judge(&self, request: JudgeRequest) -> Result<JevResponse, JevError> {
            self.seen.lock().expect("lock").push(request);
            match &self.response {
                Some(value) => {
                    crate::jev::contract::parse_response_body(value.to_string().as_bytes())
                }
                None => Err(JevError::Transport {
                    reason: "judge down".to_owned(),
                }),
            }
        }
    }

    fn config(trading_enabled: bool) -> ServiceConfig {
        ServiceConfig::from_source(|name| match name {
            "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
            "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
            "VEYRA_ENV" => Ok("development".to_owned()),
            "VEYRA_TRADING_ENABLED" => Ok(trading_enabled.to_string()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("config parses")
    }

    fn settings_from(
        source: impl FnMut(&'static str) -> Result<String, ConfigError>,
    ) -> AutopilotSettings {
        AutopilotSettings::from_source(source)
            .expect("settings parse")
            .expect("configured")
    }

    fn enabled_settings() -> AutopilotSettings {
        settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
    }

    fn broker_runtime(record: bool) -> BrokerRuntime {
        let settings = BrokerSettings::from_source(|name| match name {
            "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
            "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        let runtime = BrokerRuntime::from_settings(settings).expect("runtime builds");
        if record {
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
        }
        runtime
    }

    fn gate() -> RiskGate {
        RiskGate::new(RiskPolicy::new(
            false,
            vec![Symbol::parse("EURUSD").expect("symbol")],
            Volume::parse(0.05).expect("volume"),
            Volume::parse(0.05).expect("volume"),
            1,
            Duration::from_secs(60),
            None,
        ))
    }

    #[allow(dead_code)]
    struct Rig {
        state: AppState,
        trail: Arc<MemoryTrail>,
        _engine: Option<Arc<StubEngine>>,
        _judge: Option<Arc<StubJudge>>,
    }

    /// Wednesday 2026-01-07 12:00 UTC: midweek, mid-session, no window guard.
    fn test_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_787_200)
    }

    /// Friday 2026-09-18 20:00 UTC: inside the weekend window, one hour before
    /// the close.
    fn friday_run_up() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_761_600)
    }

    /// Sunday 2026-09-20 09:00 UTC: the week has been shut since Friday's
    /// close and reopens at 21:00.
    fn sunday_morning() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_894_800)
    }

    fn build_harness(
        settings: AutopilotSettings,
        engine: Option<Arc<StubEngine>>,
        feed: Option<StubFeed>,
        judge: Option<Arc<StubJudge>>,
        trading: bool,
        record_snapshot: bool,
    ) -> Rig {
        build_harness_at(
            test_now(),
            settings,
            engine,
            feed,
            judge,
            trading,
            record_snapshot,
        )
    }

    /// Harness with an explicit clock, for the tests that depend on where the
    /// trading week stands.
    fn build_harness_at(
        now: SystemTime,
        settings: AutopilotSettings,
        engine: Option<Arc<StubEngine>>,
        feed: Option<StubFeed>,
        judge: Option<Arc<StubJudge>>,
        trading: bool,
        record_snapshot: bool,
    ) -> Rig {
        let trail = Arc::new(MemoryTrail::default());
        let model = engine
            .clone()
            .map(|engine| ModelRuntime::with_engine(ModelProvider::OpenRouter, engine));
        let mut state = AppState::new(
            config(trading),
            Some(broker_runtime(record_snapshot)),
            model,
            gate(),
        )
        .with_autopilot(Some(settings));
        if let Some(feed) = feed {
            state = state.with_market(Some(MarketRuntime::from_feed(Arc::new(feed))));
        }
        if let Some(judge) = judge.clone() {
            state = state.with_jev(Some(JevRuntime::with_judge(JevProvider::TypeSafe, judge)));
        }
        state = state.with_audit(Some(AuditRuntime::new(trail.clone())));
        // Pinned so the trading week is a test input: the default is
        // Wednesday noon UTC, where no window guard applies.
        state = state.with_fixed_now(Some(now));
        Rig {
            state,
            trail,
            _engine: engine,
            _judge: judge,
        }
    }

    fn open_proposal(with_stops: bool, symbol: &str) -> Value {
        let mut rationale = json!({});
        rationale["action"] = json!("open");
        rationale["rationale"] = json!("Breakout above the window high with momentum.");
        let mut intent = json!({
            "symbol": symbol,
            "side": "buy",
            "order_type": "market",
            "volume": 0.01
        });
        if with_stops {
            intent["stop_loss"] = json!(1.0850);
            intent["take_profit"] = json!(1.1000);
        }
        rationale["intent"] = intent;
        rationale
    }

    fn judgements_response() -> Value {
        json!({
            "model": "jev-test",
            "answers": {
                "direction": {
                    "type": "choice",
                    "choice": "long",
                    "probabilities": {"long": 0.6, "short": 0.3, "flat": 0.1},
                    "confidence": 0.8
                },
                "trending": {"type": "noul", "noul": 0.7},
                "momentum": {
                    "type": "score",
                    "score": 1.5,
                    "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"},
                    "probabilities": {"0": 0.1, "1": 0.3, "2": 0.6},
                    "confidence": 0.75
                }
            },
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    }

    fn outcomes(trail: &MemoryTrail) -> Vec<String> {
        trail
            .events()
            .iter()
            .filter(|event| event.kind() == AuditKind::ProposalEvaluated)
            .map(|event| {
                event.payload()["outcome"]
                    .as_str()
                    .unwrap_or("?")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn settings_parse_defaults_bounds_and_absent_configuration() {
        let absent = AutopilotSettings::from_source(|name| {
            Err(ConfigError::MissingEnvironmentVariable { name })
        })
        .expect("absent settings parse");
        assert!(absent.is_none());

        let defaults = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("false".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        assert!(!defaults.enabled());
        assert_eq!(defaults.timeframe(), Timeframe::H4);
        assert_eq!(defaults.bars(), 48);
        assert_eq!(defaults.tier(), ModelTier::Balanced);
        assert_eq!(defaults.interval(), Duration::from_secs(300));
        assert_eq!(defaults.jev(), JevPreference::Auto);
        assert_eq!(defaults.min_hold(), Duration::from_secs(300));
        assert_eq!(defaults.breakeven_r(), 0.0, "break-even is opt-in");
        assert_eq!(defaults.trail_r(), 0.0, "trailing is opt-in");
        assert!(defaults.profit_harvest().is_none(), "harvesting is opt-in");
        assert!(defaults.symbols().is_empty());

        let custom = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_SYMBOL" => Ok("gbpusd".to_owned()),
            "VEYRA_AUTOPILOT_TIMEFRAME" => Ok("240".to_owned()),
            "VEYRA_AUTOPILOT_BARS" => Ok("96".to_owned()),
            "VEYRA_AUTOPILOT_TIER" => Ok("reasoning".to_owned()),
            "VEYRA_AUTOPILOT_INTERVAL_SECS" => Ok("60".to_owned()),
            "VEYRA_AUTOPILOT_JEV" => Ok("off".to_owned()),
            "VEYRA_AUTOPILOT_MIN_HOLD_SECS" => Ok("0".to_owned()),
            "VEYRA_AUTOPILOT_BREAKEVEN_R" => Ok("1.5".to_owned()),
            "VEYRA_AUTOPILOT_TRAIL_R" => Ok("2".to_owned()),
            "VEYRA_AUTOPILOT_PROFIT_HARVEST" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_ARM_R" => Ok("0.4".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_TRAIL_R" => Ok("0.25".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT" => Ok("0.75".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_GIVEBACK" => Ok("0.3".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS" => Ok("120".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_REENTRY_COOLDOWN_SECS" => Ok("600".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        assert!(custom.enabled());
        assert_eq!(custom.min_hold(), Duration::ZERO);
        assert_eq!(custom.breakeven_r(), 1.5);
        assert_eq!(custom.trail_r(), 2.0);
        assert_eq!(custom.symbols().first().expect("symbol").as_str(), "gbpusd");
        assert_eq!(custom.timeframe(), Timeframe::H4);
        assert_eq!(custom.bars(), 96);
        assert_eq!(custom.tier(), ModelTier::Reasoning);
        assert_eq!(custom.interval(), Duration::from_secs(60));
        assert_eq!(custom.jev(), JevPreference::Off);
        let harvest = custom.profit_harvest().expect("harvest enabled");
        assert_eq!(harvest.arm_r(), 0.4);
        assert_eq!(harvest.trail_r(), 0.25);
        assert_eq!(harvest.min_profit(), 0.75);
        assert_eq!(harvest.giveback_fraction(), 0.3);
        assert_eq!(harvest.min_hold(), Duration::from_secs(120));
        assert_eq!(harvest.reentry_cooldown(), Duration::from_secs(600));
        let multi = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_SYMBOLS" => Ok(" eurusd, GBPUSD ,eurusd, XAUUSD ".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        assert_eq!(
            multi
                .symbols()
                .iter()
                .map(|symbol| symbol.as_str())
                .collect::<Vec<_>>(),
            ["eurusd", "GBPUSD", "XAUUSD"],
            "lists trim, validate, and de-duplicate in order"
        );

        let conflict = AutopilotSettings::from_source(|requested| match requested {
            "VEYRA_AUTOPILOT_SYMBOL" => Ok("EURUSD".to_owned()),
            "VEYRA_AUTOPILOT_SYMBOLS" => Ok("GBPUSD".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name: requested }),
        })
        .expect_err("single and list forms are mutually exclusive");
        assert!(matches!(
            conflict,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_AUTOPILOT_SYMBOLS",
                ..
            }
        ));

        assert_eq!(JevPreference::parse(" TRUE "), Some(JevPreference::Auto));
        assert_eq!(JevPreference::parse("no"), None);
        assert_eq!(JevPreference::Auto.as_str(), "auto");
        assert_eq!(JevPreference::Off.as_str(), "off");

        for (name, value) in [
            ("VEYRA_AUTOPILOT_ENABLED", "sure"),
            ("VEYRA_AUTOPILOT_SYMBOL", "bad symbol"),
            ("VEYRA_AUTOPILOT_SYMBOLS", "EURUSD,,GBPUSD"),
            ("VEYRA_AUTOPILOT_SYMBOLS", "EURUSD,"),
            ("VEYRA_AUTOPILOT_SYMBOLS", "EURUSD,bad symbol"),
            (
                "VEYRA_AUTOPILOT_SYMBOLS",
                "A,B,C,D,E,F,G,H,I,J,K,L,M,N,O,P,Q",
            ),
            ("VEYRA_AUTOPILOT_TIMEFRAME", "H6"),
            ("VEYRA_AUTOPILOT_BARS", "9"),
            ("VEYRA_AUTOPILOT_BARS", "241"),
            ("VEYRA_AUTOPILOT_TIER", "genius"),
            ("VEYRA_AUTOPILOT_INTERVAL_SECS", "29"),
            ("VEYRA_AUTOPILOT_INTERVAL_SECS", "many"),
            ("VEYRA_AUTOPILOT_JEV", "always"),
            ("VEYRA_AUTOPILOT_MIN_HOLD_SECS", "86401"),
            ("VEYRA_AUTOPILOT_MIN_HOLD_SECS", "-5"),
            ("VEYRA_AUTOPILOT_BREAKEVEN_R", "-1"),
            ("VEYRA_AUTOPILOT_BREAKEVEN_R", "10.5"),
            ("VEYRA_AUTOPILOT_BREAKEVEN_R", "soon"),
            ("VEYRA_AUTOPILOT_TRAIL_R", "11"),
            ("VEYRA_AUTOPILOT_TRAIL_R", "trail"),
            ("VEYRA_AUTOPILOT_PROFIT_HARVEST", "perhaps"),
            ("VEYRA_AUTOPILOT_HARVEST_ARM_R", "0"),
            ("VEYRA_AUTOPILOT_HARVEST_TRAIL_R", "-0.1"),
            ("VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT", "none"),
            ("VEYRA_AUTOPILOT_HARVEST_GIVEBACK", "0.01"),
            ("VEYRA_AUTOPILOT_HARVEST_MIN_HOLD_SECS", "86401"),
            ("VEYRA_AUTOPILOT_HARVEST_REENTRY_COOLDOWN_SECS", "never"),
        ] {
            let error = AutopilotSettings::from_source(|requested| match requested {
                _ if requested == name => Ok(value.to_owned()),
                "VEYRA_AUTOPILOT_PROFIT_HARVEST" => Ok("true".to_owned()),
                _ => Err(ConfigError::MissingEnvironmentVariable { name: requested }),
            })
            .expect_err("malformed values must fail startup");
            assert!(
                matches!(
                    error,
                    ConfigError::InvalidEnvironmentVariable { name: rejected, .. } if rejected == name
                ),
                "unexpected error for {name}={value}: {error:?}"
            );
        }

        let contradictory = AutopilotSettings::from_source(|name| match name {
            "VEYRA_AUTOPILOT_PROFIT_HARVEST" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_ARM_R" => Ok("0.2".to_owned()),
            "VEYRA_AUTOPILOT_HARVEST_TRAIL_R" => Ok("0.3".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect_err("the trail cannot sit beyond the arming move");
        assert!(matches!(
            contradictory,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_AUTOPILOT_HARVEST_TRAIL_R",
                ..
            }
        ));

        let not_enabled = AutopilotSettings::from_source(|name| match name {
            "VEYRA_AUTOPILOT_HARVEST_MIN_PROFIT" => Ok("0.5".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect_err("harvest values require the explicit switch");
        assert!(matches!(
            not_enabled,
            ConfigError::InvalidEnvironmentVariable {
                name: "VEYRA_AUTOPILOT_PROFIT_HARVEST",
                ..
            }
        ));
    }

    #[test]
    fn weekend_entry_menu_keeps_only_explicit_live_contracts() {
        let symbols = ["BTCUSD", "EURUSD", "SP500m"];
        let markets: Vec<(Symbol, CandleSeries)> = symbols
            .iter()
            .map(|name| {
                let symbol = Symbol::parse(name).expect("symbol");
                (
                    symbol.clone(),
                    CandleSeries::from_validated(
                        symbol,
                        Timeframe::H4,
                        vec![Candle::from_validated(
                            1_700_000_000,
                            1.0,
                            1.1,
                            0.9,
                            1.05,
                            10,
                        )],
                    ),
                )
            })
            .collect();
        let mut blocked_index = canned_spec("SP500m");
        blocked_index.trade_allowed = false;
        let specs = vec![
            (
                Symbol::parse("BTCUSD").expect("symbol"),
                canned_spec("BTCUSD"),
            ),
            (
                Symbol::parse("EURUSD").expect("symbol"),
                canned_spec("EURUSD"),
            ),
            (Symbol::parse("SP500m").expect("symbol"), blocked_index),
        ];
        let policy = RiskPolicy::new(
            false,
            symbols
                .iter()
                .map(|name| Symbol::parse(name).expect("symbol"))
                .collect(),
            Volume::parse(0.05).expect("volume"),
            Volume::parse(0.05).expect("volume"),
            5,
            Duration::ZERO,
            None,
        )
        .apply_patch(&crate::risk::RiskPolicyPatch {
            weekend_symbols: Some(vec!["BTCUSD".to_owned()]),
            ..Default::default()
        })
        .expect("weekend list applies");

        let weekend = eligible_entry_markets(&markets, &specs, &policy, sunday_morning());
        assert_eq!(
            weekend
                .iter()
                .map(|(symbol, _)| symbol.as_str())
                .collect::<Vec<_>>(),
            ["BTCUSD"]
        );

        let weekday = eligible_entry_markets(&markets, &specs, &policy, test_now());
        assert_eq!(
            weekday
                .iter()
                .map(|(symbol, _)| symbol.as_str())
                .collect::<Vec<_>>(),
            ["BTCUSD", "EURUSD"],
            "the venue's tradeAllowed flag still removes a weekday index"
        );
    }

    #[actix_web::test]
    async fn tick_skips_when_the_loop_is_not_configured_at_all() {
        let state = AppState::new(config(true), Some(broker_runtime(true)), None, gate());
        assert_eq!(
            tick(&state).await,
            TickOutcome::Skipped {
                reason: "not_configured"
            }
        );
    }

    #[actix_web::test]
    async fn tick_skips_until_every_integration_is_ready() {
        let disabled = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("false".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        let harness = build_harness(
            disabled,
            Some(StubEngine::answering(json!({"action": "none"}))),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Skipped { reason: "disabled" }
        );

        let harness = build_harness(
            enabled_settings(),
            None,
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Skipped { reason: "no_model" }
        );

        let harness = build_harness(
            enabled_settings(),
            Some(StubEngine::answering(json!({"action": "none"}))),
            None,
            None,
            true,
            true,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Skipped {
                reason: "no_market"
            }
        );

        let harness = build_harness(
            enabled_settings(),
            Some(StubEngine::answering(json!({"action": "none"}))),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            false,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Skipped {
                reason: "stale_link"
            }
        );

        let harness = build_harness(
            enabled_settings(),
            Some(StubEngine::answering(json!({"action": "none"}))),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert!(
            tick(&harness.state).await
                != TickOutcome::Skipped {
                    reason: "stale_link"
                }
        );
    }

    #[test]
    fn stub_providers_report_their_names() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        assert_eq!(engine.provider(), ModelProvider::OpenRouter);
        let judge = StubJudge::failing();
        assert_eq!(judge.provider(), JevProvider::TypeSafe);
    }

    #[actix_web::test]
    async fn tick_records_no_trade_without_judgements() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert_eq!(outcomes(&harness.trail), vec!["no_trade".to_owned()]);

        let requests = engine.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].input.contains("judgements"),
            "no judgement engine is configured"
        );
        assert!(requests[0].input.contains("\"symbol\":\"EURUSD\""));
        assert!(requests[0].instructions.contains("stop_loss"));
    }

    #[actix_web::test]
    async fn tick_records_gate_rejections() {
        let engine = StubEngine::answering(open_proposal(true, "GBPUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "symbol_not_allowed"
            }
        );
        assert_eq!(outcomes(&harness.trail), vec!["rejected".to_owned()]);
        let event = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.kind() == AuditKind::ProposalEvaluated)
            .expect("decision recorded");
        assert_eq!(event.payload()["reason"], "symbol_not_allowed");
        assert_eq!(event.payload()["symbol"], "GBPUSD");
    }

    #[actix_web::test]
    async fn tick_rejects_entries_without_bracketing_stops() {
        let engine = StubEngine::answering(open_proposal(false, "EURUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "missing_stops"
            }
        );
        let event = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.kind() == AuditKind::ProposalEvaluated)
            .expect("decision recorded");
        assert_eq!(event.payload()["reason"], "missing_stops");
    }

    #[actix_web::test]
    async fn tick_records_approved_dry_run_when_execution_is_disabled() {
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            false,
            true,
        );
        assert_eq!(tick(&harness.state).await, TickOutcome::ApprovedDryRun);
        assert_eq!(
            outcomes(&harness.trail),
            vec!["approved_dry_run".to_owned()]
        );
        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        assert!(
            !link.has_pending(CommandKind::OpenOrder),
            "nothing may be queued while the switch is off"
        );
    }

    #[actix_web::test]
    async fn tick_queues_approved_entries_when_execution_is_enabled() {
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            Some(StubJudge::responding(judgements_response())),
            true,
            true,
        );
        let outcome = tick(&harness.state).await;
        let command = match outcome {
            TickOutcome::Queued { command } => command,
            other => panic!("expected a queued command, got {other:?}"),
        };
        assert!(outcomes(&harness.trail).contains(&"queued".to_owned()));
        let decision = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.kind() == AuditKind::ProposalEvaluated)
            .expect("decision recorded");
        assert_eq!(decision.payload()["stop_loss"], 1.0850);
        assert_eq!(decision.payload()["take_profit"], 1.1000);
        assert_eq!(
            decision.payload()["rationale"],
            "Breakout above the window high with momentum.",
            "the model's why reaches the journal"
        );
        assert_eq!(
            decision.payload()["judgements"]["direction"]["choice"],
            "long",
            "the judgements behind the decision travel with it"
        );
        assert!(
            decision.payload()["intent_id"]
                .as_str()
                .is_some_and(|id| id.len() == 36),
            "the decision records its intent"
        );
        assert_eq!(
            decision.payload()["command_id"],
            command,
            "the decision links the command it produced"
        );
        let kinds: Vec<&'static str> = harness
            .trail
            .events()
            .iter()
            .map(|event| event.kind().as_str())
            .collect();
        assert!(kinds.contains(&"command_queued"));

        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        assert!(link.has_pending(CommandKind::OpenOrder));
        let id = crate::broker::CommandId::parse(&command).expect("command id");
        let record = link.command(id).expect("record retained");
        assert_eq!(record.kind, CommandKind::OpenOrder);
    }

    #[actix_web::test]
    async fn tick_rejects_entries_the_free_margin_cannot_cover() {
        let mut spec = canned_spec("EURUSD");
        spec.margin_required = 5_000.0;
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: Some(spec),
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        link.retain_snapshot(cash_snapshot(20.0, 20.0));

        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "insufficient_margin"
            }
        );
        let rejection = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "rejected")
            .expect("rejection recorded");
        assert_eq!(rejection.payload()["reason"], "insufficient_margin");
        assert!(
            !link.has_pending(CommandKind::OpenOrder),
            "an unaffordable order is never queued"
        );

        let input: Value =
            serde_json::from_str(&engine.requests()[0].input).expect("input is JSON");
        assert_eq!(
            input["account"]["free_margin"], 20.0,
            "the model sees the same free margin the check used"
        );
        assert_eq!(input["assets"][0]["contract"]["margin_required"], 5_000.0);
    }

    #[actix_web::test]
    async fn tick_rejects_entries_below_the_venue_lot_minimum() {
        let mut spec = canned_spec("EURUSD");
        spec.lot_min = 0.02;
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: Some(spec),
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "volume_below_min"
            }
        );
    }

    #[actix_web::test]
    async fn tick_rejects_entries_inside_the_atr_floor() {
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        // The canned series carries a 0.02 ATR, so a 0.5 floor demands a
        // 0.01 stop distance; the draft carries 0.0069.
        let policy = harness
            .state
            .risk()
            .policy()
            .with_min_stop_atr_fraction(0.5);
        harness.state.risk().update_policy(policy);

        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "stop_inside_noise"
            }
        );
    }

    #[actix_web::test]
    async fn tick_rejects_entries_without_a_venue_contract() {
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: true,
            }),
            None,
            true,
            true,
        );
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "spec_unavailable"
            }
        );
    }

    #[actix_web::test]
    async fn tick_rejects_entries_inside_a_news_blackout() {
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let mut harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        let now = unix_secs(test_now());
        let event =
            CalendarEvent::new("Non-Farm Employment Change", "USD", Impact::High, now + 600)
                .expect("event");
        harness.state = harness
            .state
            .clone()
            .with_calendar(Some(CalendarRuntime::from_feed(Arc::new(StubCalendar {
                events: vec![event],
                fail: false,
            }))));
        let policy = harness.state.risk().policy().with_calendar_blackout(30);
        harness.state.risk().update_policy(policy);

        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "news_blackout"
            }
        );
        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        assert!(
            !link.has_pending(CommandKind::OpenOrder),
            "an entry inside the news window is never queued"
        );

        let input: Value =
            serde_json::from_str(&engine.requests()[0].input).expect("input is JSON");
        assert_eq!(
            input["assets"][0]["upcoming_events"][0]["title"], "Non-Farm Employment Change",
            "the model sees what is scheduled"
        );
        assert_eq!(input["assets"][0]["upcoming_events"][0]["impact"], "high");
        assert_eq!(input["assets"][0]["upcoming_events"][0]["in_minutes"], 10);
    }

    #[actix_web::test]
    async fn tick_allows_entries_outside_the_news_window() {
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let mut harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        let now = unix_secs(test_now());
        let event = CalendarEvent::new("ECB Press Conference", "EUR", Impact::High, now + 7_200)
            .expect("event");
        harness.state = harness
            .state
            .clone()
            .with_calendar(Some(CalendarRuntime::from_feed(Arc::new(StubCalendar {
                events: vec![event],
                fail: false,
            }))));
        let policy = harness.state.risk().policy().with_calendar_blackout(30);
        harness.state.risk().update_policy(policy);

        assert!(matches!(
            tick(&harness.state).await,
            TickOutcome::Queued { .. }
        ));
    }

    #[actix_web::test]
    async fn tick_fails_closed_when_the_configured_calendar_fails() {
        let engine = StubEngine::answering(open_proposal(true, "EURUSD"));
        let mut harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness.state = harness
            .state
            .clone()
            .with_calendar(Some(CalendarRuntime::from_feed(Arc::new(StubCalendar {
                events: Vec::new(),
                fail: true,
            }))));

        match tick(&harness.state).await {
            TickOutcome::Unavailable { reason } => {
                assert!(reason.contains("calendar unavailable"), "{reason}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[actix_web::test]
    async fn tick_feeds_judgements_into_the_model_input() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let judge = StubJudge::responding(judgements_response());
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            Some(judge.clone()),
            true,
            true,
        );
        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert_eq!(judge.requests().len(), 1);

        let requests = engine.requests();
        let input: Value = serde_json::from_str(&requests[0].input).expect("input is JSON");
        assert_eq!(input["assets"][0]["symbol"], "EURUSD");
        assert_eq!(
            input["assets"][0]["judgements"]["direction"]["choice"],
            "long"
        );
        assert_eq!(
            input["assets"][0]["judgements"]["trending"]["probability"],
            0.7
        );
        assert_eq!(input["assets"][0]["judgements"]["momentum"]["score"], 1.5);
        assert_eq!(input["assets"][0]["market"]["bars"], 20);
        assert_eq!(
            input["assets"][0]["market"]["atr14"], 0.02,
            "the model gets the true-range context it sizes stops against"
        );
        assert_eq!(
            input["assets"][0]["contract"]["spread_points"], 12,
            "the venue contract travels with the asset"
        );
        assert_eq!(input["assets"][0]["contract"]["lot_min"], 0.01);
        assert_eq!(input["assets"][0]["contract"]["stop_level_points"], 5);
    }

    #[actix_web::test]
    async fn tick_honours_the_off_judgement_preference() {
        let settings = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_JEV" => Ok("off".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        let engine = StubEngine::answering(json!({"action": "none"}));
        let judge = StubJudge::responding(judgements_response());
        let harness = build_harness(
            settings,
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            Some(judge.clone()),
            true,
            true,
        );
        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert!(judge.requests().is_empty(), "off means never consulted");
        assert!(!engine.requests()[0].input.contains("judgements"));
    }

    #[actix_web::test]
    async fn tick_fails_closed_when_judgements_fail() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            Some(StubJudge::failing()),
            true,
            true,
        );
        match tick(&harness.state).await {
            TickOutcome::Unavailable { reason } => {
                assert!(reason.contains("judgement unavailable"), "{reason}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
        assert!(
            engine.requests().is_empty(),
            "a failed judgement must not reach the model"
        );
        assert_eq!(outcomes(&harness.trail), vec!["unavailable".to_owned()]);
    }

    #[test]
    fn a_failure_run_is_counted_until_a_verdict_clears_it() {
        let health = DecisionHealth::default();
        assert_eq!(health.consecutive_failures(), 0);
        assert!(health.last_failure().is_none());

        health.failed("openrouter call returned 400: thinking mode", 1_700_000_000);
        health.failed("openrouter call returned 400: thinking mode", 1_700_000_060);
        assert_eq!(health.consecutive_failures(), 2);
        let (reason, at) = health.last_failure().expect("a run is open");
        assert!(reason.contains("thinking mode"));
        assert_eq!(at, 1_700_000_060);

        // One verdict means the provider is answering again.
        health.succeeded();
        assert_eq!(health.consecutive_failures(), 0);
        assert!(
            health.last_failure().is_none(),
            "a cleared run must not leave a stale reason on the status surface"
        );
    }

    #[test]
    fn a_recorded_failure_reason_cannot_swamp_the_status_payload() {
        let health = DecisionHealth::default();
        // Provider error bodies embed the whole upstream response.
        health.failed(&"x".repeat(5_000), 1_700_000_000);
        let (reason, _) = health.last_failure().expect("failure recorded");
        assert!(
            reason.chars().count() <= 300,
            "got {}",
            reason.chars().count()
        );
    }

    #[actix_web::test]
    async fn a_refused_model_leaves_a_failure_run_behind() {
        // The engine refuses every request, exactly as a thinking model does
        // when handed a compelled tool choice.
        let engine = StubEngine::failing();
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );

        assert!(matches!(
            tick(&harness.state).await,
            TickOutcome::Unavailable { .. }
        ));
        assert_eq!(
            harness.state.decision_health().consecutive_failures(),
            1,
            "a tick that never reached a verdict must be visible as a failure"
        );
        assert!(harness.state.decision_health().last_failure().is_some());
    }

    #[test]
    fn a_position_is_reviewed_once_per_candle() {
        let watch = ReviewWatch::default();
        // Never reviewed: the first look on any candle is always allowed.
        assert!(watch.should_review(10650805, 1_700_000_000));
        watch.record(10650805, 1_700_000_000);

        assert!(
            !watch.should_review(10650805, 1_700_000_000),
            "the same candle is the same question"
        );
        assert!(
            watch.should_review(10650805, 1_700_014_400),
            "a newly closed candle is a new question"
        );
        assert!(
            watch.should_review(7, 1_700_000_000),
            "reviews are tracked per position"
        );

        // A closed position stops being tracked, so a recycled ticket number
        // cannot inherit a review it never had.
        watch.retain(&[7]);
        assert!(watch.should_review(10650805, 1_700_000_000));
    }

    #[actix_web::test]
    async fn an_open_position_is_not_re_reviewed_inside_its_candle() {
        let engine = StubEngine::answering(json!({"action": "hold"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            // Opened long enough ago that the minimum hold no longer applies.
            .retain_snapshot(managed_snapshot(10650805, 1_757_900_000, 1_758_003_600));

        tick(&harness.state).await;
        let after_first = engine.requests().len();
        assert!(after_first >= 1, "the first tick reviews the position");

        tick(&harness.state).await;
        assert_eq!(
            engine.requests().len(),
            after_first,
            "an unchanged candle must not re-review an open position"
        );
    }

    #[actix_web::test]
    async fn a_position_inside_the_minimum_hold_is_not_reviewed() {
        // The close path refuses a position this young, so the review would buy
        // a verdict that cannot be acted on.
        let settings = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_MIN_HOLD_SECS" => Ok("86400".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            settings,
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );

        let outcome = tick(&harness.state).await;

        // The entry sweep still runs; only the review was skipped.
        assert_eq!(outcome, TickOutcome::NoTrade);
        assert!(
            !outcomes(&harness.trail).contains(&"close_rejected".to_owned()),
            "a review that cannot act must not run at all"
        );
    }

    #[actix_web::test]
    async fn the_judge_is_asked_once_per_candle() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let judge = StubJudge::responding(judgements_response());
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            Some(judge.clone()),
            true,
            true,
        );

        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert_eq!(judge.requests().len(), 1);

        // Same candles: the judge would be shown an identical narrative, so the
        // held answer stands in for it.
        assert_eq!(tick(&harness.state).await, TickOutcome::Unchanged);
        assert_eq!(
            judge.requests().len(),
            1,
            "an unchanged candle must not be judged twice"
        );
    }

    #[test]
    fn held_judgements_belong_to_one_candle_only() {
        let cache = JudgementCache::default();
        cache.put("EURUSD", 100, json!({"direction": "long"}));

        assert_eq!(
            cache.get("EURUSD", 100),
            Some(json!({"direction": "long"})),
            "the answer stands for the candle that produced it"
        );
        assert_eq!(
            cache.get("EURUSD", 101),
            None,
            "a newer candle is a different question"
        );
        assert_eq!(cache.get("GBPUSD", 100), None, "answers are per instrument");

        // A newer candle replaces the older answer rather than accumulating.
        cache.put("EURUSD", 101, json!({"direction": "short"}));
        assert_eq!(cache.get("EURUSD", 100), None);
        assert_eq!(
            cache.get("EURUSD", 101),
            Some(json!({"direction": "short"}))
        );
    }

    #[actix_web::test]
    async fn an_unchanged_market_is_not_proposed_on_twice() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );

        // First look: nothing has been judged yet, so the model is asked.
        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert_eq!(engine.requests().len(), 1);

        // Same candles, same price: the question would be identical.
        assert_eq!(tick(&harness.state).await, TickOutcome::Unchanged);
        assert_eq!(
            engine.requests().len(),
            1,
            "an unchanged chart must not buy a second opinion"
        );
    }

    #[actix_web::test]
    async fn a_new_candle_reopens_the_entry_question() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);

        // A later candle is new information, so the gate opens again.
        let observation = EntryObservation {
            symbol: "EURUSD",
            candle_time: i64::MAX,
            price: None,
            atr: None,
        };
        assert!(
            harness
                .state
                .entry_watch()
                .should_evaluate(&[observation], 0.25, GATE_NOW),
            "a newer closed candle must reopen the entry question"
        );
    }

    /// Wall clock for gate tests that are not about the retry window.
    const GATE_NOW: i64 = 1_700_000_000;

    /// A sweep that dies before a verdict must not hold the gate for the candle.
    ///
    /// The gate is marked before the model is asked, so a failed sweep leaves
    /// every instrument recorded against a market nothing actually judged. On
    /// H4 that silently withheld entries for the rest of a four-hour candle,
    /// from a single malformed answer, with the console still reading LIVE.
    #[test]
    fn a_failed_sweep_reopens_the_question_without_waiting_for_the_candle() {
        let watch = EntryWatch::default();
        let observation = || EntryObservation {
            symbol: "EURUSD",
            candle_time: GATE_NOW,
            price: Some(1.1000),
            atr: Some(0.0040),
        };
        // The sweep runs and marks the gate, then fails on the way to a verdict.
        watch.record(&[observation()]);
        watch.mark_failed(GATE_NOW);

        // Nothing moved, so the ordinary gate is shut and would stay shut until
        // the next candle.
        assert!(
            !watch.should_evaluate(&[observation()], 0.25, GATE_NOW + 1),
            "a failure must not re-ask on the very next tick"
        );
        assert!(
            !watch.should_evaluate(
                &[observation()],
                0.25,
                GATE_NOW + ENTRY_RETRY_AFTER_SECS - 1
            ),
            "the retry window must actually hold"
        );
        // Once the window passes, the question the sweep never answered is
        // asked again rather than waiting out the candle.
        assert!(
            watch.should_evaluate(&[observation()], 0.25, GATE_NOW + ENTRY_RETRY_AFTER_SECS),
            "a failed sweep must be retried within the candle"
        );

        // A sweep that reaches a verdict disarms the retry.
        watch.mark_settled();
        assert!(
            !watch.should_evaluate(
                &[observation()],
                0.25,
                GATE_NOW + ENTRY_RETRY_AFTER_SECS * 10
            ),
            "a settled sweep must fall back to the ordinary gate"
        );
    }

    #[test]
    fn a_mid_candle_move_reopens_the_question_only_past_the_threshold() {
        let watch = EntryWatch::default();
        let seen = |price: f64| EntryObservation {
            symbol: "EURUSD",
            candle_time: 1_700_000_000,
            price: Some(price),
            atr: Some(0.0040),
        };
        // Never seen before: always worth one look.
        assert!(watch.should_evaluate(&[seen(1.1000)], 0.25, GATE_NOW));
        watch.record(&[seen(1.1000)]);

        // A quarter of ATR is 0.0010, so drift below it stays quiet. The exact
        // boundary is deliberately not asserted: at these magnitudes it lands
        // inside float error, and no decision should hinge on which side of a
        // rounding step a price fell.
        assert!(!watch.should_evaluate(&[seen(1.1005)], 0.25, GATE_NOW));
        // Clearly past it, in either direction.
        assert!(watch.should_evaluate(&[seen(1.1012)], 0.25, GATE_NOW));
        assert!(watch.should_evaluate(&[seen(1.0988)], 0.25, GATE_NOW));
        // Zero disables the mid-candle trigger entirely.
        assert!(!watch.should_evaluate(&[seen(1.5000)], 0.0, GATE_NOW));
        // Without a live price there is nothing to compare, so candles rule.
        let unpriced = EntryObservation {
            symbol: "EURUSD",
            candle_time: 1_700_000_000,
            price: None,
            atr: Some(0.0040),
        };
        assert!(!watch.should_evaluate(&[unpriced], 0.25, GATE_NOW));
    }

    #[test]
    fn post_close_fresh_market_gate_is_per_symbol() {
        let watch = EntryWatch::default();
        let observation = |symbol: &'static str, candle_time: i64, price: f64| EntryObservation {
            symbol,
            candle_time,
            price: Some(price),
            atr: Some(0.0040),
        };
        watch.record(&[
            observation("EURUSD", GATE_NOW, 1.1000),
            observation("GBPUSD", GATE_NOW, 1.2500),
        ]);
        let guarded = vec!["EURUSD".to_owned()];

        assert!(
            watch
                .changed_symbols(
                    &[
                        observation("EURUSD", GATE_NOW, 1.1000),
                        observation("GBPUSD", GATE_NOW, 1.2600),
                    ],
                    &guarded,
                    0.25,
                )
                .is_empty(),
            "another symbol's move cannot release EURUSD"
        );
        assert_eq!(
            watch.changed_symbols(&[observation("EURUSD", GATE_NOW, 1.1012)], &guarded, 0.25,),
            guarded,
            "EURUSD's own ATR-scaled move releases it"
        );
        assert_eq!(
            watch.changed_symbols(
                &[observation("EURUSD", GATE_NOW + 14_400, 1.1000)],
                &["EURUSD".to_owned()],
                0.25,
            ),
            vec!["EURUSD".to_owned()],
            "EURUSD's own new candle also releases it"
        );
    }

    #[actix_web::test]
    async fn tick_continues_without_judgements_when_the_owner_allows_it() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            Some(StubJudge::failing()),
            true,
            true,
        );
        let allowed = harness.state.risk().policy().with_trading_without_jev(true);
        harness.state.risk().update_policy(allowed);

        // The same judge failure now degrades instead of stopping: the model is
        // asked, and it is asked with no judgements at all rather than a
        // partial set.
        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        let requests = engine.requests();
        let input: Value =
            serde_json::from_str(&requests.first().expect("the model is consulted").input)
                .expect("input is JSON");
        assert_eq!(input["assets"][0]["symbol"], "EURUSD");
        assert!(
            input["assets"][0].get("judgements").is_none(),
            "no judgement set should reach the model: {input}"
        );
        assert_eq!(
            outcomes(&harness.trail),
            vec!["jev_degraded".to_owned(), "no_trade".to_owned()],
            "the degraded tick is visible in the trail"
        );
    }

    #[actix_web::test]
    async fn tick_reports_empty_market_windows() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 0,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        match tick(&harness.state).await {
            TickOutcome::Unavailable { reason } => {
                assert!(reason.contains("no candles"), "{reason}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
        assert!(
            engine.requests().is_empty(),
            "no proposal on an empty window"
        );
    }

    #[actix_web::test]
    async fn tick_skips_without_a_broker_or_with_a_disconnected_terminal() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let feed = StubFeed {
            bars: 20,
            fail: false,
            spec: None,
            spec_fail: false,
        };
        let no_broker = AppState::new(
            config(true),
            None,
            Some(ModelRuntime::with_engine(
                ModelProvider::OpenRouter,
                engine.clone(),
            )),
            gate(),
        )
        .with_autopilot(Some(enabled_settings()))
        .with_market(Some(MarketRuntime::from_feed(Arc::new(feed))));
        assert_eq!(
            tick(&no_broker).await,
            TickOutcome::Skipped {
                reason: "no_broker"
            }
        );

        // A recorded but disconnected terminal yields no account facts.
        let broker = broker_runtime(false);
        broker.ea_link().expect("link").record(AccountSnapshot::new(
            AccountLogin::parse(94168).expect("login"),
            ServerName::parse("IFCMarkets-Real").expect("server"),
            Symbol::parse("EURUSD").expect("symbol"),
            false,
            false,
            0,
            0.0,
        ));
        let disconnected = AppState::new(
            config(true),
            Some(broker),
            Some(ModelRuntime::with_engine(ModelProvider::OpenRouter, engine)),
            gate(),
        )
        .with_autopilot(Some(enabled_settings()))
        .with_market(Some(MarketRuntime::from_feed(Arc::new(StubFeed {
            bars: 20,
            fail: false,
            spec: None,
            spec_fail: false,
        }))));
        assert_eq!(
            tick(&disconnected).await,
            TickOutcome::Skipped {
                reason: "account_unavailable"
            }
        );
    }

    #[actix_web::test]
    async fn tick_works_without_an_audit_trail() {
        let state = AppState::new(
            config(true),
            Some(broker_runtime(true)),
            Some(ModelRuntime::with_engine(
                ModelProvider::OpenRouter,
                StubEngine::answering(json!({"action": "none"})),
            )),
            gate(),
        )
        .with_autopilot(Some(enabled_settings()))
        .with_market(Some(MarketRuntime::from_feed(Arc::new(StubFeed {
            bars: 20,
            fail: false,
            spec: None,
            spec_fail: false,
        }))));
        assert_eq!(tick(&state).await, TickOutcome::NoTrade);
    }

    #[test]
    fn narrative_and_change_helpers_survive_empty_series() {
        let empty = CandleSeries::from_validated(
            Symbol::parse("EURUSD").expect("symbol"),
            Timeframe::H4,
            Vec::new(),
        );
        assert!(market_narrative(&empty).contains("no closed candles"));
        assert_eq!(change_pct(&empty), 0.0);
        assert_eq!(average_true_range(&empty, ATR_PERIOD), None);
    }

    #[test]
    fn average_true_range_measures_true_ranges_or_reports_none() {
        let three = CandleSeries::from_validated(
            Symbol::parse("EURUSD").expect("symbol"),
            Timeframe::H4,
            vec![
                Candle::from_validated(1, 1.0, 1.05, 0.95, 1.0, 1),
                Candle::from_validated(2, 1.0, 1.5, 0.9, 1.2, 1),
                Candle::from_validated(3, 1.2, 1.3, 1.0, 1.1, 1),
            ],
        );
        // TR2 = max(0.6, |1.5-1.0|, |0.9-1.0|) = 0.6; TR3 = max(0.3, 0.1, 0.2) = 0.3.
        assert_eq!(average_true_range(&three, 2), Some(0.45));
        assert_eq!(
            average_true_range(&three, 0),
            None,
            "a zero window is not measurable"
        );
        assert_eq!(
            average_true_range(&three, 3),
            None,
            "each true range needs a previous close"
        );
    }

    fn managed_snapshot(ticket: i64, opened_at: i64, server_time: i64) -> AccountSnapshotPayload {
        managed_snapshot_at(ticket, opened_at, server_time, 1.1477)
    }

    /// Snapshot with an explicit current price for the managed position.
    fn managed_snapshot_at(
        ticket: i64,
        opened_at: i64,
        server_time: i64,
        current: f64,
    ) -> AccountSnapshotPayload {
        AccountSnapshotPayload {
            balance: 20.57,
            equity: 20.57,
            free_margin: 20.0,
            orders: 1,
            lots: 0.01,
            positions: vec![crate::broker::PositionPayload {
                ticket,
                symbol: "EURUSD".to_owned(),
                kind: crate::broker::PositionKind::Sell,
                lots: 0.01,
                price: 1.14757,
                profit: -0.2,
                stop_loss: 1.1497,
                take_profit: 1.14554,
                opened_at,
                current,
                swap: -0.11,
                commission: 0.0,
                magic: crate::broker::ORDER_MAGIC,
            }],
            positions_truncated: false,
            server_time,
            leverage: 100,
            margin_level: 357.5,
        }
    }

    /// Snapshot of a flat account with explicit money values, used to give
    /// the pre-queue margin check a free-margin number to compare against.
    fn cash_snapshot(equity: f64, free_margin: f64) -> AccountSnapshotPayload {
        AccountSnapshotPayload {
            balance: equity,
            equity,
            free_margin,
            orders: 0,
            lots: 0.0,
            positions: Vec::new(),
            positions_truncated: false,
            server_time: 1_758_003_600,
            leverage: 100,
            margin_level: 0.0,
        }
    }

    #[test]
    fn review_answers_parse_strictly() {
        assert_eq!(
            parse_review(&json!({"action": "hold"})).expect("hold parses"),
            ReviewDecision::Hold
        );
        assert_eq!(
            parse_review(&json!({"action": "hold", "ticket": 42})).expect("stray ticket tolerated"),
            ReviewDecision::Hold
        );
        assert_eq!(
            parse_review(&json!({"action": "close", "ticket": 42})).expect("close parses"),
            ReviewDecision::Close(42)
        );
        assert!(parse_review(&json!({"action": "close"})).is_err());
        assert!(parse_review(&json!({"action": "close", "ticket": 0})).is_err());
        assert!(parse_review(&json!({"action": "flatten"})).is_err());
        assert!(parse_review(&json!({"action": "hold", "extra": true})).is_err());
    }

    #[actix_web::test]
    async fn review_holds_when_the_analyst_holds() {
        let engine = StubEngine::answering_review(
            json!({"action": "hold", "rationale": "Bracket intact; wait for break-even."}),
            json!({"action": "none"}),
        );
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(10650805, 1_758_000_000, 1_758_003_600));

        assert_eq!(tick(&harness.state).await, TickOutcome::Held);
        let held = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "held")
            .expect("held decision recorded");
        assert_eq!(
            held.payload()["rationale"],
            "Bracket intact; wait for break-even.",
            "the reviewer's why reaches the journal"
        );
        assert_eq!(
            outcomes(&harness.trail),
            vec!["held".to_owned(), "no_trade".to_owned()],
            "the hold review is recorded before the entry sweep declines"
        );
        let request = &engine.requests()[0];
        assert!(
            request.instructions.contains("10650805"),
            "the reviewer sees the ticket"
        );
        let input: Value = serde_json::from_str(&request.input).expect("input is JSON");
        assert_eq!(input["open_positions"][0]["ticket"], 10650805);
        assert_eq!(input["open_positions"][0]["age_secs"], 3600);
        assert_eq!(input["open_positions"][0]["stop_loss"], 1.1497);
        assert_eq!(
            input["open_positions"][0]["swap"], -0.11,
            "the reviewer sees the carry the position is paying"
        );
        assert_eq!(input["account"]["margin_level"], 357.5);
        assert_eq!(input["account"]["leverage"], 100);
        assert_eq!(
            input["market"]["atr14"], 0.02,
            "the reviewer sees the same volatility context as the entry prompt"
        );
    }

    #[actix_web::test]
    async fn review_refuses_young_or_unverifiable_positions() {
        // Younger than the minimum hold.
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 10650805}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(10650805, 1_758_003_540, 1_758_003_600));
        // The tick no longer reviews a position the minimum hold protects: the
        // close would be refused, so the verdict is never bought. The guard in
        // the close path below stays as the backstop for any other caller.
        assert!(
            !outcomes(&harness.trail).contains(&"close_rejected".to_owned()),
            "a position that cannot be closed must not be reviewed"
        );

        // Age cannot be verified: the close path refuses that too, so the same
        // skip applies rather than paying for an unusable verdict.
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 7}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(7, 0, 1_758_003_600));
        tick(&harness.state).await;
        assert!(
            !outcomes(&harness.trail).contains(&"close_rejected".to_owned()),
            "an unverifiable age cannot authorise a close, so it is not asked"
        );

        // An unknown ticket is refused before any command exists.
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 999}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(10650805, 1_758_000_000, 1_758_003_600));
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "unknown_ticket"
            }
        );
    }

    #[actix_web::test]
    async fn review_queues_closes_for_old_positions() {
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 10650805}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(10650805, 1_758_000_000, 1_758_003_600));

        let outcome = tick(&harness.state).await;
        match outcome {
            TickOutcome::CloseQueued { command } => {
                assert!(!command.is_empty());
            }
            other => panic!("expected a queued close, got {other:?}"),
        }
        assert!(outcomes(&harness.trail).contains(&"close_queued".to_owned()));
        let close = harness
            .trail
            .events()
            .into_iter()
            .find(|event| {
                event.kind() == AuditKind::ProposalEvaluated
                    && event.payload()["outcome"] == "close_queued"
            })
            .expect("close recorded");
        assert!(
            close.payload()["command_id"]
                .as_str()
                .is_some_and(|id| id.len() == 36),
            "the close links its command"
        );
        let kinds: Vec<&str> = harness
            .trail
            .events()
            .iter()
            .map(|event| event.kind().as_str())
            .collect();
        assert!(kinds.contains(&"command_queued"));
        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        assert!(link.has_pending(CommandKind::CloseOrder));
    }

    #[actix_web::test]
    async fn review_respects_the_service_switch_and_zero_min_hold() {
        // Switch off: the close is refused and nothing queues.
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 10650805}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            false,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(10650805, 1_758_000_000, 1_758_003_600));
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "trading_disabled"
            }
        );

        // Zero minimum hold removes the age gate (tests only).
        let settings = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_MIN_HOLD_SECS" => Ok("0".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 42}));
        let harness = build_harness(
            settings,
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(42, 1_758_003_599, 1_758_003_600));
        assert!(matches!(
            tick(&harness.state).await,
            TickOutcome::CloseQueued { .. }
        ));
    }

    /// Snapshot for the weekend tests: mid-Friday, a day-old position.
    fn friday_book(link: &std::sync::Arc<crate::broker::ea::EaLink>) {
        link.retain_snapshot(managed_snapshot(10650805, 1_789_680_000, 1_789_761_600));
    }

    #[actix_web::test]
    async fn the_weekend_checkpoint_lets_the_analyst_flatten_before_the_close() {
        let engine = StubEngine::answering_review(
            json!({
                "action": "close",
                "ticket": 10650805,
                "rationale": "Gap risk outweighs the bracket."
            }),
            json!({"action": "none"}),
        );
        let harness = build_harness_at(
            friday_run_up(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        friday_book(
            &harness
                .state
                .broker()
                .expect("broker")
                .ea_link()
                .expect("link"),
        );

        assert!(matches!(
            tick(&harness.state).await,
            TickOutcome::CloseQueued { .. }
        ));
        let request = &engine.requests()[0];
        assert!(
            request.instructions.contains("weekend checkpoint"),
            "the prompt carries the weekend question: {}",
            request.instructions
        );
        assert!(
            request.instructions.contains("21:00 UTC"),
            "the prompt names the close"
        );
        assert!(
            request.instructions.contains("1h 00m"),
            "the prompt states what remains"
        );
        let input: Value = serde_json::from_str(&request.input).expect("input is JSON");
        assert_eq!(
            input["weekend"],
            json!({"market_closes_at": 1_789_765_200_i64, "market_closes_in_secs": 3_600})
        );
        let closed = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "close_queued")
            .expect("the weekend close is recorded");
        assert_eq!(closed.payload()["origin"], "autopilot_weekend");
        assert_eq!(
            closed.payload()["rationale"],
            "Gap risk outweighs the bracket."
        );

        // One verdict per position per close: the same question is not bought
        // again by the next tick.
        let _ = tick(&harness.state).await;
        assert_eq!(engine.requests().len(), 1, "the verdict is asked once");
    }

    #[actix_web::test]
    async fn the_weekend_checkpoint_holds_when_the_analyst_holds() {
        let engine = StubEngine::answering_review(
            json!({"action": "hold", "rationale": "Stop is close; no gap edge either way."}),
            json!({"action": "none"}),
        );
        let harness = build_harness_at(
            friday_run_up(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        friday_book(
            &harness
                .state
                .broker()
                .expect("broker")
                .ea_link()
                .expect("link"),
        );

        assert_eq!(tick(&harness.state).await, TickOutcome::Held);
        let held = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "held")
            .expect("the weekend hold is recorded");
        assert_eq!(held.payload()["origin"], "autopilot_weekend");
        assert_eq!(held.payload()["symbol"], "EURUSD");
        // The window is shut for entries too, and the sweep says so without
        // asking the model: both questions cost one request between them.
        assert!(
            harness.trail.events().iter().any(|event| {
                event.payload()["outcome"] == "no_trade"
                    && event.payload()["reason"] == "weekend_approach"
            }),
            "the closed entry window is stated in the journal"
        );
        assert_eq!(engine.requests().len(), 1, "one review, no entry sweep");
    }

    #[actix_web::test]
    async fn the_weekend_checkpoint_runs_even_when_the_candle_was_reviewed() {
        // The candle question has already been answered on a later bar, so
        // the candle gate alone would skip this position; the weekend verdict
        // is keyed to the close and still runs.
        let engine = StubEngine::answering_review(
            json!({"action": "hold", "rationale": "Carry covers the gap risk."}),
            json!({"action": "none"}),
        );
        let harness = build_harness_at(
            friday_run_up(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness.state.review_watch().record(10650805, i64::MAX);
        friday_book(
            &harness
                .state
                .broker()
                .expect("broker")
                .ea_link()
                .expect("link"),
        );

        assert_eq!(tick(&harness.state).await, TickOutcome::Held);
        let held = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "held")
            .expect("the weekend hold is recorded");
        assert_eq!(
            held.payload()["origin"],
            "autopilot_weekend",
            "the weekend question is not tied to a new candle"
        );
    }

    #[actix_web::test]
    async fn the_weekend_flatten_preference_settles_the_book_without_the_model() {
        let engine = StubEngine::answering_review(
            json!({"action": "hold", "rationale": "never asked"}),
            json!({"action": "none"}),
        );
        let harness = build_harness_at(
            friday_run_up(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        let policy = harness
            .state
            .risk()
            .policy()
            .with_weekend_positions(WeekendPositions::Flatten);
        harness.state.risk().update_policy(policy);
        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        let mut snapshot = managed_snapshot(10650805, 1_789_680_000, 1_789_761_600);
        let mut second = snapshot.positions[0].clone();
        second.ticket = 10650806;
        snapshot.positions.push(second);
        snapshot.orders = 2;
        snapshot.lots = 0.02;
        link.retain_snapshot(snapshot);

        assert!(matches!(
            tick(&harness.state).await,
            TickOutcome::CloseQueued { .. }
        ));
        assert!(
            engine.requests().is_empty(),
            "the operator's verdict needs no model"
        );
        let closes = |harness: &Rig| {
            harness
                .trail
                .events()
                .iter()
                .filter(|event| event.payload()["outcome"] == "close_queued")
                .count()
        };
        assert_eq!(closes(&harness), 2, "the whole book settles in one tick");
        assert!(
            harness
                .trail
                .events()
                .iter()
                .filter(|event| event.payload()["outcome"] == "close_queued")
                .all(|event| event.payload()["origin"] == "autopilot_weekend")
        );

        // Settled once: the next tick does not queue the same closes again.
        let _ = tick(&harness.state).await;
        assert_eq!(closes(&harness), 2);
    }

    #[actix_web::test]
    async fn the_weekend_hold_preference_leaves_reviews_as_they_were() {
        let engine = StubEngine::answering_review(
            json!({"action": "hold", "rationale": "Bracket intact."}),
            json!({"action": "none"}),
        );
        let harness = build_harness_at(
            friday_run_up(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        let policy = harness
            .state
            .risk()
            .policy()
            .with_weekend_positions(WeekendPositions::Hold);
        harness.state.risk().update_policy(policy);
        friday_book(
            &harness
                .state
                .broker()
                .expect("broker")
                .ea_link()
                .expect("link"),
        );

        assert_eq!(tick(&harness.state).await, TickOutcome::Held);
        let request = &engine.requests()[0];
        assert!(
            !request.instructions.contains("weekend checkpoint"),
            "the operator took the weekend, so the reviewer is not asked about it"
        );
        let input: Value = serde_json::from_str(&request.input).expect("input is JSON");
        assert!(input["weekend"].is_null());
        let held = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "held")
            .expect("the candle hold is recorded");
        assert_eq!(held.payload()["origin"], "autopilot_review");
        assert!(
            !harness
                .trail
                .events()
                .iter()
                .any(|event| event.payload()["outcome"] == "close_queued"),
            "nothing is flattened for a weekend the operator accepted"
        );
    }

    #[actix_web::test]
    async fn the_weekend_checkpoint_waits_for_the_prep_window() {
        // Wednesday noon: the preference is the default agent one, but the
        // week is nowhere near closing, so the review stays the ordinary
        // candle review and no checkpoint runs.
        let engine = StubEngine::answering_review(
            json!({"action": "hold", "rationale": "Midweek, bracket intact."}),
            json!({"action": "none"}),
        );
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        friday_book(
            &harness
                .state
                .broker()
                .expect("broker")
                .ea_link()
                .expect("link"),
        );

        assert_eq!(tick(&harness.state).await, TickOutcome::Held);
        let request = &engine.requests()[0];
        assert!(!request.instructions.contains("weekend checkpoint"));
        let input: Value = serde_json::from_str(&request.input).expect("input is JSON");
        assert!(input["weekend"].is_null());
        let held = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "held")
            .expect("the candle hold is recorded");
        assert_eq!(held.payload()["origin"], "autopilot_review");
    }

    #[actix_web::test]
    async fn a_closed_market_stages_nothing_for_the_weekend() {
        // Sunday morning: the position has already sat through the weekend,
        // and the venue could not execute a close until the reopen anyway.
        let engine = StubEngine::answering_review(
            json!({"action": "close", "ticket": 10650805}),
            json!({"action": "none"}),
        );
        let harness = build_harness_at(
            sunday_morning(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness.state.review_watch().record(10650805, i64::MAX);
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(10650805, 1_789_752_000, 1_789_894_800));

        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert!(
            engine.requests().is_empty(),
            "nothing is decided while the market is shut"
        );
        assert!(
            !harness
                .trail
                .events()
                .iter()
                .any(|event| event.payload()["outcome"] == "close_queued"),
            "no close is staged that Sunday's gap could execute"
        );
    }

    #[test]
    fn a_refused_close_reads_the_same_from_every_caller() {
        use crate::broker::CommandId;

        // Each refusal carries the journal label, the reason, and the outcome
        // the caller returns; the review and the weekend flatten share them.
        assert_eq!(
            close_refusal(&StagedClose::TradingDisabled),
            (
                "close_rejected",
                "trading_disabled".to_owned(),
                TickOutcome::Rejected {
                    code: "trading_disabled"
                }
            )
        );
        assert_eq!(
            close_refusal(&StagedClose::ChannelUnavailable),
            (
                "unavailable",
                "command channel unavailable".to_owned(),
                TickOutcome::Unavailable {
                    reason: "command channel unavailable".to_owned()
                }
            )
        );
        for stale in [StagedClose::NoPositions, StagedClose::UnknownTicket] {
            assert_eq!(
                close_refusal(&stale),
                (
                    "close_rejected",
                    "stale_position".to_owned(),
                    TickOutcome::Rejected {
                        code: "stale_position"
                    }
                )
            );
        }
        assert_eq!(
            close_refusal(&StagedClose::NotVeyra),
            (
                "close_rejected",
                "not_a_veyra_position".to_owned(),
                TickOutcome::Rejected {
                    code: "not_a_veyra_position"
                }
            )
        );
        // A queued command is never asked about, but the mapping stays total.
        assert_eq!(
            close_refusal(&StagedClose::Queued {
                command: CommandId::new(),
                ticket: 42
            }),
            (
                "close_rejected",
                "stale_position".to_owned(),
                TickOutcome::Rejected {
                    code: "stale_position"
                }
            )
        );
    }

    #[test]
    fn the_weekend_labels_read_the_way_operators_do() {
        assert_eq!(utc_clock(1_789_765_200), "21:00");
        assert_eq!(utc_clock(0), "00:00");
        assert_eq!(duration_phrase(3_600), "1h 00m");
        assert_eq!(duration_phrase(4_320), "1h 12m");
        assert_eq!(duration_phrase(2_700), "45m");
        assert_eq!(duration_phrase(0), "0m");
    }

    #[actix_web::test]
    async fn a_refused_weekend_flatten_retries_on_the_next_tick() {
        // The switch is off, so the close cannot be queued: the refusal is
        // recorded, the position stays unmarked, and the next tick asks again
        // rather than leaving the book to the gap.
        let engine = StubEngine::answering_review(
            json!({"action": "hold", "rationale": "never asked"}),
            json!({"action": "none"}),
        );
        let harness = build_harness_at(
            friday_run_up(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            false,
            true,
        );
        let policy = harness
            .state
            .risk()
            .policy()
            .with_weekend_positions(WeekendPositions::Flatten);
        harness.state.risk().update_policy(policy);
        friday_book(
            &harness
                .state
                .broker()
                .expect("broker")
                .ea_link()
                .expect("link"),
        );

        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "trading_disabled"
            }
        );
        let refused = harness
            .trail
            .events()
            .into_iter()
            .find(|event| event.payload()["outcome"] == "close_rejected")
            .expect("the refusal is recorded");
        assert_eq!(refused.payload()["origin"], "autopilot_weekend");
        assert_eq!(refused.payload()["reason"], "trading_disabled");
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "trading_disabled"
            },
            "an unsettled position is asked about again"
        );
    }

    #[actix_web::test]
    async fn closed_sessions_are_not_asked_of_the_model() {
        // Friday past the cutoff: the gate would refuse any entry, so the
        // sweep states the block instead of buying a proposal that cannot run.
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness_at(
            friday_run_up(),
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(cash_snapshot(20.0, 20.0));

        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert!(
            engine.requests().is_empty(),
            "a shut window cannot open a position"
        );
        assert!(
            harness.trail.events().iter().any(|event| {
                event.payload()["outcome"] == "no_trade"
                    && event.payload()["reason"] == "weekend_approach"
            }),
            "the block is stated in the journal"
        );
        // The observation is settled, so the next tick does not repeat it.
        assert_eq!(tick(&harness.state).await, TickOutcome::Unchanged);
        assert!(engine.requests().is_empty());

        // The same book at Wednesday noon is asked as usual.
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(cash_snapshot(20.0, 20.0));
        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        assert_eq!(
            engine.requests().len(),
            1,
            "an open window consults the model"
        );
    }

    #[actix_web::test]
    async fn entry_path_runs_when_only_foreign_positions_are_open() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        let mut snapshot = managed_snapshot(1, 1_758_000_000, 1_758_003_600);
        snapshot.positions[0].magic = 0; // manual position
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(snapshot);

        assert_eq!(tick(&harness.state).await, TickOutcome::NoTrade);
        let request = &engine.requests()[0];
        assert!(
            request
                .instructions
                .contains("Decide for each instrument independently"),
            "the entry prompt ran, not the review prompt"
        );
    }

    fn managed_position(side: ManagedSide, entry: f64, stop: f64, current: f64) -> ManagedPosition {
        ManagedPosition {
            ticket: 1,
            symbol: "EURUSD".to_owned(),
            side,
            lots: 0.01,
            entry,
            profit: 0.0,
            stop_loss: stop,
            take_profit: 0.0,
            opened_at: 1_758_000_000,
            current,
            swap: 0.0,
            commission: 0.0,
            is_market: true,
        }
    }

    fn managed_position_named(symbol: &str) -> ManagedPosition {
        let mut position = managed_position(ManagedSide::Buy, 1.1, 1.09, 1.1);
        position.symbol = symbol.to_owned();
        position
    }

    #[test]
    fn candidate_symbols_merge_the_menu_with_open_positions() {
        let configured = vec![Symbol::parse("EURUSD").expect("symbol")];
        let managed = vec![
            managed_position_named("XAUUSD"),
            managed_position_named("EURUSD"),
        ];
        let candidates = candidate_symbols(&configured, &managed);
        assert_eq!(
            candidates
                .iter()
                .map(|symbol| symbol.as_str())
                .collect::<Vec<_>>(),
            ["EURUSD", "XAUUSD"],
            "configured order wins and open-position symbols are appended once"
        );

        let from_positions = candidate_symbols(&[], &[managed_position_named("GBPUSD")]);
        assert_eq!(
            from_positions.first().map(|symbol| symbol.as_str()),
            Some("GBPUSD"),
            "an open position is managed even without a configured menu"
        );
    }

    #[test]
    fn candle_closes_fill_missing_prices_without_replacing_live_ones() {
        let series = |symbol: &str| {
            CandleSeries::from_validated(
                Symbol::parse(symbol).expect("symbol"),
                Timeframe::H4,
                vec![Candle::from_validated(
                    1_700_000_000,
                    1.0,
                    1.2,
                    0.9,
                    1.1,
                    10,
                )],
            )
        };
        let markets = vec![
            (Symbol::parse("EURUSD").expect("symbol"), series("EURUSD")),
            (Symbol::parse("GBPUSD").expect("symbol"), series("GBPUSD")),
        ];
        // EURUSD already carries the venue's live close for an open position;
        // GBPUSD has none, so only it should take the candle's price.
        let facts = AccountFacts {
            trade_allowed: true,
            open_orders: 1,
            open_lots: 0.01,
            open_symbols: vec![Symbol::parse("EURUSD").expect("symbol")],
            open_positions: Vec::new(),
            prices: vec![(Symbol::parse("EURUSD").expect("symbol"), 1.9)],
            symbol_specs: Vec::new(),
            equity: None,
            free_margin: None,
            day_drawdown_percent: None,
            peak_drawdown_percent: None,
        };

        let priced = with_reference_prices(facts, &markets);

        let price = |symbol: &str| {
            priced
                .prices
                .iter()
                .find(|(known, _)| known.as_str() == symbol)
                .map(|(_, value)| *value)
        };
        assert_eq!(
            price("EURUSD"),
            Some(1.9),
            "a candle hours old must not displace the live snapshot price"
        );
        assert_eq!(price("GBPUSD"), Some(1.1));
    }

    #[test]
    fn series_and_judgements_resolve_per_symbol() {
        let series = CandleSeries::from_validated(
            Symbol::parse("EURUSD").expect("symbol"),
            Timeframe::H4,
            vec![Candle::from_validated(
                1_700_000_000,
                1.0,
                1.1,
                0.9,
                1.05,
                10,
            )],
        );
        let markets = vec![(Symbol::parse("EURUSD").expect("symbol"), series)];
        let judgements = vec![(
            Symbol::parse("EURUSD").expect("symbol"),
            json!({"direction": "long"}),
        )];

        assert!(series_for_symbol(&markets, "EURUSD").is_some());
        assert!(series_for_symbol(&markets, "GBPUSD").is_none());
        assert!(judgement_for_symbol(&judgements, "EURUSD").is_some());
        assert!(judgement_for_symbol(&judgements, "GBPUSD").is_none());
    }

    #[test]
    fn review_rotation_visits_every_position() {
        let mut first_position = managed_position_named("EURUSD");
        first_position.ticket = 101;
        let mut second_position = managed_position_named("GBPUSD");
        second_position.ticket = 202;
        let managed = vec![first_position, second_position];
        let counter = AtomicUsize::new(0);
        let first = next_review_position(&managed, &counter).expect("position");
        let second = next_review_position(&managed, &counter).expect("position");
        assert_eq!((first.ticket, second.ticket), (101, 202));
        assert!(next_review_position(&[], &AtomicUsize::new(0)).is_none());
    }

    fn planned(ticket: i64, stop: f64, kind: StopMoveKind) -> Option<StopMove> {
        Some(StopMove { ticket, stop, kind })
    }

    #[test]
    fn stop_plan_breaks_even_only_at_r_with_a_stop_behind_the_entry() {
        // Sell: risk 1.14757-1.1497 = 20 pips; at exactly 1R the plan fires.
        let at_r = managed_position(ManagedSide::Sell, 1.14757, 1.1497, 1.14544);
        assert_eq!(
            stop_plan(std::slice::from_ref(&at_r), 1.0, 0.0, &StopBasis::default()),
            planned(1, 1.14757, StopMoveKind::BreakEven)
        );
        assert_eq!(
            stop_plan(std::slice::from_ref(&at_r), 0.5, 0.0, &StopBasis::default()),
            planned(1, 1.14757, StopMoveKind::BreakEven),
            "a lower multiple fires sooner"
        );
        assert_eq!(
            stop_plan(
                &[managed_position(ManagedSide::Sell, 1.14757, 1.1497, 1.1470)],
                1.0,
                0.0,
                &StopBasis::default()
            ),
            None,
            "below R nothing moves"
        );
        assert_eq!(
            stop_plan(
                &[managed_position(
                    ManagedSide::Sell,
                    1.14757,
                    1.14757,
                    1.14544
                )],
                1.0,
                0.0,
                &StopBasis::default()
            ),
            None,
            "already at break-even"
        );
        assert_eq!(
            stop_plan(
                &[managed_position(ManagedSide::Sell, 1.14757, 0.0, 1.14544)],
                1.0,
                0.0,
                &StopBasis::default()
            ),
            None,
            "without a stop there is no risk to reference"
        );
        assert_eq!(
            stop_plan(
                &[managed_position(ManagedSide::Sell, 1.14757, 1.1497, 0.0)],
                1.0,
                0.0,
                &StopBasis::default()
            ),
            None,
            "without a current price nothing moves"
        );
        assert_eq!(
            stop_plan(std::slice::from_ref(&at_r), 0.0, 0.0, &StopBasis::default()),
            None,
            "zero disables the policy"
        );
        // Buy: symmetric.
        assert_eq!(
            stop_plan(
                &[managed_position(
                    ManagedSide::Buy,
                    1.14757,
                    1.14544,
                    1.14971
                )],
                1.0,
                0.0,
                &StopBasis::default()
            ),
            planned(1, 1.14757, StopMoveKind::BreakEven)
        );
    }

    #[test]
    fn stop_plan_trails_behind_the_best_price_and_only_moves_forward() {
        let risk = 1.1497 - 1.14757;
        // At 2R in favour the trail candidate beats break-even.
        let at_2r = managed_position(ManagedSide::Sell, 1.14757, 1.1497, 1.14757 - 2.0 * risk);
        let planned = stop_plan(
            std::slice::from_ref(&at_2r),
            1.0,
            1.0,
            &StopBasis::default(),
        )
        .expect("trail fires");
        assert_eq!(planned.kind, StopMoveKind::Trail);
        assert!(
            (planned.stop - (1.14757 - risk)).abs() < 1e-9,
            "one risk unit behind the price"
        );

        // At exactly 1R both candidates coincide; break-even wins the tie.
        let at_r = managed_position(ManagedSide::Sell, 1.14757, 1.1497, 1.14757 - risk);
        assert_eq!(
            stop_plan(std::slice::from_ref(&at_r), 1.0, 1.0, &StopBasis::default())
                .expect("plan")
                .kind,
            StopMoveKind::BreakEven
        );

        // The original risk is remembered the first time the ticket is seen,
        // so trailing still works after break-even has moved the stop to the
        // entry: a 0.05R improvement is not worth a round trip, but 0.2R is.
        let basis = StopBasis::default();
        let initial = managed_position(ManagedSide::Sell, 1.14757, 1.1497, 1.14757 - 0.5 * risk);
        assert_eq!(
            stop_plan(std::slice::from_ref(&initial), 1.0, 1.0, &basis),
            None,
            "half an R does nothing, but seeds the basis"
        );
        let small = managed_position(ManagedSide::Sell, 1.14757, 1.14757, 1.14757 - 1.05 * risk);
        assert_eq!(
            stop_plan(std::slice::from_ref(&small), 1.0, 1.0, &basis),
            None
        );
        let enough = managed_position(ManagedSide::Sell, 1.14757, 1.14757, 1.14757 - 1.2 * risk);
        let ratcheted =
            stop_plan(std::slice::from_ref(&enough), 1.0, 1.0, &basis).expect("trail ratchets");
        assert_eq!(ratcheted.kind, StopMoveKind::Trail);
        assert!(
            ratcheted.stop < 1.14757,
            "the stop is more protective than entry"
        );

        // Buy: symmetric trail.
        let buy = managed_position(ManagedSide::Buy, 1.14757, 1.14544, 1.14757 + 2.0 * risk);
        let bought = stop_plan(&[buy], 1.0, 1.0, &StopBasis::default()).expect("buy trail fires");
        assert_eq!(bought.kind, StopMoveKind::Trail);
        assert!((bought.stop - (1.14757 + risk)).abs() < 1e-9);
    }

    fn harvest_policy() -> ProfitHarvestPolicy {
        settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_PROFIT_HARVEST" => Ok("true".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .profit_harvest()
        .expect("harvest policy")
        .clone()
    }

    #[test]
    fn profit_harvest_arms_on_net_profit_then_banks_a_positive_giveback() {
        let policy = harvest_policy();
        let basis = StopBasis::default();
        let book = ProfitHarvestBook::default();
        let risk = 1.1497 - 1.14757;
        let mut position =
            managed_position(ManagedSide::Sell, 1.14757, 1.1497, 1.14757 - 0.25 * risk);
        position.profit = 0.66;
        position.swap = -0.11;
        position.commission = -0.05;
        basis.observe(std::slice::from_ref(&position));
        book.observe(
            std::slice::from_ref(&position),
            1_758_003_600,
            Duration::from_secs(900),
        );

        let ratchet = harvest_stop_plan(
            std::slice::from_ref(&position),
            &policy,
            &basis,
            &book,
            1_758_003_600,
        )
        .expect("the net 0.50 high-water mark arms harvesting");
        assert_eq!(ratchet.kind, StopMoveKind::ProfitHarvest);
        assert!(ratchet.stop < position.entry, "the sell stop locks profit");

        position.current = position.entry - 0.10 * risk;
        position.profit = 0.29;
        position.swap = 0.0;
        position.commission = 0.0;
        book.observe(
            std::slice::from_ref(&position),
            1_758_003_630,
            Duration::from_secs(900),
        );
        let close = harvest_close_plan(
            std::slice::from_ref(&position),
            &policy,
            &basis,
            &book,
            1_758_003_630,
            1_758_003_630,
        )
        .expect("a 42 percent giveback is banked while still positive");
        assert_eq!(close.ticket, position.ticket);
        assert_eq!(close.high_net_profit, 0.5);
        assert_eq!(close.net_profit, 0.29);

        position.profit = -0.01;
        book.observe(
            std::slice::from_ref(&position),
            1_758_003_660,
            Duration::from_secs(900),
        );
        assert_eq!(
            harvest_close_plan(
                &[position],
                &policy,
                &basis,
                &book,
                1_758_003_660,
                1_758_003_660,
            ),
            None,
            "a missed positive exit never becomes a deterministic losing close"
        );
    }

    #[test]
    fn profit_harvest_cooldown_and_high_water_survive_restart() {
        let book = ProfitHarvestBook::default();
        let mut position = managed_position(ManagedSide::Buy, 1.1, 1.09, 1.105);
        position.profit = 0.75;
        book.observe(
            std::slice::from_ref(&position),
            1_000,
            Duration::from_secs(900),
        );
        book.mark_armed(position.ticket);

        let snapshot = book.state_snapshot();
        let restarted = ProfitHarvestBook::default();
        restarted
            .restore_state(&snapshot)
            .expect("valid harvest state restores");
        assert_eq!(restarted.high_net_profit(position.ticket), Some(0.75));
        assert!(restarted.is_armed(position.ticket));

        restarted.observe(&[], 1_100, Duration::from_secs(900));
        let closed_snapshot = restarted.state_snapshot();
        let after_close_restart = ProfitHarvestBook::default();
        after_close_restart
            .restore_state(&closed_snapshot)
            .expect("cooldown state restores");
        assert!(after_close_restart.cooling("EURUSD", 1_999));
        assert!(!after_close_restart.cooling("EURUSD", 2_000));
        assert_eq!(
            after_close_restart.pending_fresh_baselines(),
            vec!["EURUSD".to_owned()]
        );
        assert!(after_close_restart.requires_fresh_market("EURUSD"));
        after_close_restart.mark_fresh_baselines(&["EURUSD".to_owned()]);
        assert!(after_close_restart.pending_fresh_baselines().is_empty());
        after_close_restart.mark_fresh_market(&["EURUSD".to_owned()]);
        assert!(!after_close_restart.requires_fresh_market("EURUSD"));

        for broken in [
            json!({"positions": {"0": {"symbol": "EURUSD", "highNetProfit": 1.0, "armed": true}}, "cooldowns": {}, "pendingFreshBaselines": []}),
            json!({"positions": {"1": {"symbol": "bad symbol", "highNetProfit": 1.0, "armed": true}}, "cooldowns": {}, "pendingFreshBaselines": []}),
            json!({"positions": {"1": {"symbol": "EURUSD", "highNetProfit": -1.0, "armed": true}}, "cooldowns": {}, "pendingFreshBaselines": []}),
            json!({"positions": {}, "cooldowns": {"EURUSD": -1}, "pendingFreshBaselines": []}),
            json!({"positions": {}, "cooldowns": {}, "pendingFreshBaselines": [], "freshMarketRequired": ["bad symbol"]}),
        ] {
            assert!(
                after_close_restart.restore_state(&broken).is_err(),
                "{broken}"
            );
        }
    }

    #[test]
    fn stop_basis_survives_a_restart_through_a_snapshot() {
        let risk = 1.1497 - 1.14757;
        let basis = StopBasis::default();
        // Half an R seeds the memory without moving the stop.
        let seed = managed_position(ManagedSide::Sell, 1.14757, 1.1497, 1.14757 - 0.5 * risk);
        assert_eq!(
            stop_plan(std::slice::from_ref(&seed), 1.0, 1.0, &basis),
            None
        );
        let snapshot = basis.state_snapshot();
        assert_eq!(snapshot["1"], json!(risk));

        // A restarted process resumes the memory, so trailing still ratchets
        // once the stop has already been moved to break-even.
        let restarted = StopBasis::default();
        restarted
            .restore_state(&snapshot)
            .expect("snapshot restores");
        assert_eq!(restarted.risk(1), Some(risk));
        let moved = managed_position(ManagedSide::Sell, 1.14757, 1.14757, 1.14757 - 1.2 * risk);
        assert!(
            stop_plan(std::slice::from_ref(&moved), 1.0, 1.0, &restarted).is_some(),
            "the restored basis keeps trailing alive"
        );

        // Malformed snapshots fail closed instead of adopting junk.
        for broken in [
            json!("nope"),
            json!({"not-a-ticket": 0.001}),
            json!({"7": -0.1}),
            json!({"7": "wide"}),
        ] {
            assert!(restarted.restore_state(&broken).is_err(), "{broken}");
        }
    }

    #[actix_web::test]
    async fn tick_moves_the_stop_to_break_even_before_any_review() {
        let settings = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_BREAKEVEN_R" => Ok("1.0".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        let engine =
            StubEngine::answering_review(json!({"action": "hold"}), json!({"action": "none"}));
        let harness = build_harness(
            settings,
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot_at(
                10650805,
                1_758_000_000,
                1_758_003_600,
                1.14544,
            ));

        match tick(&harness.state).await {
            TickOutcome::StopMoved { command } => assert!(!command.is_empty()),
            other => panic!("expected a stop move, got {other:?}"),
        }
        assert!(
            engine.requests().is_empty(),
            "the deterministic plan acts before the model is consulted"
        );
        assert!(outcomes(&harness.trail).contains(&"break_even".to_owned()));
        let stop = harness
            .trail
            .events()
            .into_iter()
            .find(|event| {
                event.kind() == AuditKind::ProposalEvaluated
                    && event.payload()["outcome"] == "break_even"
            })
            .expect("stop move recorded");
        assert!(
            stop.payload()["command_id"]
                .as_str()
                .is_some_and(|id| id.len() == 36),
            "the stop move links its command"
        );

        // Trailing enabled: at 2R the stop trails instead, audited as
        // `trailing_stop`, and the reviewer is not consulted.
        let trail_settings = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_BREAKEVEN_R" => Ok("1.0".to_owned()),
            "VEYRA_AUTOPILOT_TRAIL_R" => Ok("1.0".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        let engine = StubEngine::answering(json!({"action": "hold"}));
        let harness = build_harness(
            trail_settings,
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        let risk = 1.1497 - 1.14757;
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot_at(
                10650805,
                1_758_000_000,
                1_758_003_600,
                1.14757 - 2.0 * risk,
            ));
        assert!(matches!(
            tick(&harness.state).await,
            TickOutcome::StopMoved { .. }
        ));
        assert!(outcomes(&harness.trail).contains(&"trailing_stop".to_owned()));
        assert!(engine.requests().is_empty());
        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        assert!(link.has_pending(CommandKind::ModifyOrder));
    }

    #[actix_web::test]
    async fn position_loop_harvests_profit_without_market_or_model_work() {
        let settings = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_PROFIT_HARVEST" => Ok("true".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        let harness = build_harness(settings.clone(), None, None, None, true, true);
        let risk = 1.1497 - 1.14757;
        let mut snapshot = managed_snapshot_at(
            10650805,
            1_758_000_000,
            1_758_003_600,
            1.14757 - 0.25 * risk,
        );
        snapshot.positions[0].profit = 0.7;
        snapshot.positions[0].swap = -0.1;
        snapshot.positions[0].commission = -0.05;
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(snapshot);

        assert!(matches!(
            manage_open_positions(&harness.state).await,
            TickOutcome::StopMoved { .. }
        ));
        assert!(
            outcomes(&harness.trail).contains(&"profit_harvest_stop".to_owned()),
            "the deterministic ratchet is audited without needing a model"
        );

        let close_harness = build_harness(settings, None, None, None, true, true);
        let mut high = managed_snapshot_at(
            10650805,
            1_758_000_000,
            1_758_003_600,
            1.14757 - 0.25 * risk,
        );
        high.positions[0].profit = 0.7;
        high.positions[0].swap = -0.1;
        high.positions[0].commission = -0.05;
        close_harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(high);
        let managed = managed_positions(&close_harness.state);
        close_harness.state.stop_basis().observe(&managed);
        close_harness.state.profit_harvest_book().observe(
            &managed,
            1_758_003_600,
            Duration::from_secs(900),
        );
        assert!(
            harvest_stop_plan(
                &managed,
                close_harness
                    .state
                    .autopilot()
                    .and_then(AutopilotSettings::profit_harvest)
                    .expect("policy"),
                close_harness.state.stop_basis(),
                close_harness.state.profit_harvest_book(),
                1_758_003_600,
            )
            .is_some(),
            "the first profitable observation arms the high-water mark"
        );

        let mut retraced = managed_snapshot_at(
            10650805,
            1_758_000_000,
            1_758_003_630,
            1.14757 - 0.10 * risk,
        );
        retraced.positions[0].profit = 0.3;
        retraced.positions[0].swap = 0.0;
        retraced.positions[0].commission = 0.0;
        close_harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(retraced);

        assert!(matches!(
            manage_open_positions(&close_harness.state).await,
            TickOutcome::CloseQueued { .. }
        ));
        assert!(
            close_harness
                .state
                .broker()
                .expect("broker")
                .link()
                .has_pending(CommandKind::CloseOrder)
        );
        assert!(outcomes(&close_harness.trail).contains(&"profit_harvest_close".to_owned()));
    }

    #[actix_web::test]
    async fn tick_holds_at_r_when_break_even_is_disabled() {
        let engine =
            StubEngine::answering_review(json!({"action": "hold"}), json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot_at(
                10650805,
                1_758_000_000,
                1_758_003_600,
                1.14544,
            ));

        assert_eq!(tick(&harness.state).await, TickOutcome::Held);
        assert_eq!(
            engine.requests().len(),
            2,
            "the review runs, then the entry sweep declines"
        );
        let link = harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link");
        assert!(!link.has_pending(CommandKind::ModifyOrder));
    }

    #[actix_web::test]
    async fn break_even_reports_every_guard() {
        let plan = |ticket: i64| StopMove {
            ticket,
            stop: 1.14757,
            kind: StopMoveKind::BreakEven,
        };
        let _series = CandleSeries::from_validated(
            Symbol::parse("EURUSD").expect("symbol"),
            Timeframe::H4,
            vec![Candle::from_validated(
                1_700_000_000,
                1.1,
                1.2,
                1.0,
                1.15,
                1,
            )],
        );
        let feed = || {
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            })
        };

        // The service switch is off.
        let harness = build_harness(enabled_settings(), None, feed(), None, false, true);
        assert_eq!(
            move_stop(&harness.state, "EURUSD", plan(10650805)).await,
            TickOutcome::Rejected {
                code: "trading_disabled"
            }
        );

        // No command channel exists.
        let no_broker = AppState::new(config(true), None, None, gate())
            .with_autopilot(Some(enabled_settings()));
        assert!(matches!(
            move_stop(&no_broker, "EURUSD", plan(1)).await,
            TickOutcome::Unavailable { .. }
        ));

        // No completed snapshot has been retained.
        let harness = build_harness(enabled_settings(), None, feed(), None, true, false);
        assert_eq!(
            move_stop(&harness.state, "EURUSD", plan(1)).await,
            TickOutcome::Rejected {
                code: "stale_position"
            }
        );

        // The ticket is not in the latest snapshot.
        let harness = build_harness(enabled_settings(), None, feed(), None, true, false);
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(managed_snapshot(7, 1_758_000_000, 1_758_003_600));
        assert_eq!(
            move_stop(&harness.state, "EURUSD", plan(999)).await,
            TickOutcome::Rejected {
                code: "stale_position"
            }
        );

        // The ticket is a manual position.
        let harness = build_harness(enabled_settings(), None, feed(), None, true, false);
        let mut manual = managed_snapshot(7, 1_758_000_000, 1_758_003_600);
        manual.positions[0].magic = 0;
        harness
            .state
            .broker()
            .expect("broker")
            .ea_link()
            .expect("link")
            .retain_snapshot(manual);
        assert_eq!(
            move_stop(&harness.state, "EURUSD", plan(7)).await,
            TickOutcome::Rejected {
                code: "not_a_veyra_position"
            }
        );
        assert!(
            outcomes(&harness.trail).contains(&"stop_rejected".to_owned()),
            "refusals are audited"
        );
    }

    #[actix_web::test]
    async fn tick_records_market_and_model_failures() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: true,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        match tick(&harness.state).await {
            TickOutcome::Unavailable { reason } => {
                assert!(reason.contains("market unavailable"), "{reason}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }

        let harness = build_harness(
            enabled_settings(),
            Some(StubEngine::failing()),
            Some(StubFeed {
                bars: 20,
                fail: false,
                spec: None,
                spec_fail: false,
            }),
            None,
            true,
            true,
        );
        match tick(&harness.state).await {
            TickOutcome::Unavailable { reason } => {
                assert!(reason.contains("model unavailable"), "{reason}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
        assert_eq!(outcomes(&harness.trail), vec!["unavailable".to_owned()]);
    }
}
