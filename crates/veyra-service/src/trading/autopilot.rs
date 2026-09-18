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

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::AppState;
use crate::audit::{AuditEvent, AuditKind};
use crate::broker::Symbol;
use crate::broker::{ORDER_MAGIC, PositionPayload};
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
use crate::risk::AccountFacts;
use crate::trading::agent::{self, AgentDecision, AgentMode, AgentSession};
use crate::trading::intent::TradeIntentDraft;
use crate::trading::pipeline::{PipelineError, PipelineOutcome};

/// Default proposal cadence in seconds.
const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Largest number of instruments one autopilot rotation accepts.
const MAX_SYMBOLS: usize = 8;
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
/// Default minimum position age before an autonomous close is allowed.
const DEFAULT_MIN_HOLD_SECS: u64 = 300;
/// Largest minimum-hold window the parser accepts.
const MAX_MIN_HOLD_SECS: u64 = 86_400;

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

    /// Distance kept behind the best favourable price once trailing starts,
    /// as a multiple of the entry risk; zero disables the policy.
    pub fn trail_r(&self) -> f64 {
        self.trail_r
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
    let Some(settings) = state.autopilot() else {
        return TickOutcome::Skipped {
            reason: "not_configured",
        };
    };
    if !settings.enabled() {
        return TickOutcome::Skipped { reason: "disabled" };
    }
    let Some(model) = state.model() else {
        return TickOutcome::Skipped { reason: "no_model" };
    };
    let Some(market) = state.market() else {
        return TickOutcome::Skipped {
            reason: "no_market",
        };
    };
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
    // Candidate menu: configured symbols first, then any symbol carrying an
    // open Veyra position (so every managed position stays managed), capped by
    // the settings parser. With nothing configured, the chart symbol is the
    // whole menu.
    let managed = managed_positions(state);
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
    let account = with_reference_prices(account, &markets);

    // Calibrated judgements per candidate. They are advisory inputs, but a
    // configured judge that fails still aborts the tick: no model call runs on
    // partial inputs.
    let mut judgements: Vec<(Symbol, Value)> = Vec::new();
    if settings.jev() != JevPreference::Off
        && let Some(jev) = state.jev()
    {
        for (symbol, series) in &markets {
            match judgements_for(jev, series).await {
                Ok(summary) => judgements.push((symbol.clone(), summary)),
                Err(error) => {
                    let reason = format!("judgement unavailable: {error}");
                    record(state, "unavailable", Some(symbol), None, Some(&reason)).await;
                    return TickOutcome::Unavailable { reason };
                }
            }
        }
    }

    // Capital preservation first: any managed position whose stop policy is
    // due moves before reviews or entries, and a stop move ends the tick.
    if state.config().trading_enabled()
        && let Some(plan) = stop_plan(
            &managed,
            settings.breakeven_r(),
            settings.trail_r(),
            state.stop_basis(),
        )
    {
        let symbol = managed
            .iter()
            .find(|position| position.ticket == plan.ticket)
            .map(|position| position.symbol.clone())
            .unwrap_or_default();
        return move_stop(state, &symbol, plan).await;
    }

    // One position review per tick, rotating through the open book; a close
    // ends the tick, a hold falls through so entries can still be considered.
    let mut reviewed_hold = false;
    if let Some(position) = next_review_position(&managed, state.rotation())
        && let Some(series) = series_for_symbol(&markets, &position.symbol)
    {
        let engine = model.engine();
        let session = AgentSession {
            state,
            engine: engine.as_ref(),
            mode: AgentMode::Review,
            markets: &markets,
            account: &account,
            judgements: &judgements,
            tier: settings.tier(),
            now: SystemTime::now(),
        };
        let outcome = review_positions(
            state,
            settings,
            &session,
            series,
            std::slice::from_ref(&position),
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
    let policy = state.risk().policy();
    let entry_outcome = if account.open_orders >= policy.max_open_orders() {
        record(state, "no_trade", None, None, Some("open_order_cap")).await;
        TickOutcome::NoTrade
    } else if policy.max_total_lots().value() - account.open_lots <= 0.0 {
        record(state, "no_trade", None, None, Some("exposure_cap")).await;
        TickOutcome::NoTrade
    } else {
        let input = proposal_input(&markets, &judgements, &account, &managed);
        let instructions = proposal_instructions(state, &markets, &account);
        let engine = model.engine();
        let session = AgentSession {
            state,
            engine: engine.as_ref(),
            mode: AgentMode::Proposal,
            markets: &markets,
            account: &account,
            judgements: &judgements,
            tier: settings.tier(),
            now: SystemTime::now(),
        };
        match agent::run(&session, &instructions, &input).await {
            Err(error) => {
                let reason = match error {
                    PipelineError::AgentLoopLimit { reason } => format!("agent loop: {reason}"),
                    other => format!("model unavailable: {other}"),
                };
                record(state, "unavailable", None, None, Some(&reason)).await;
                TickOutcome::Unavailable { reason }
            }
            Ok(outcome) => {
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
        match account.prices.iter_mut().find(|(known, _)| known == symbol) {
            Some((_, price)) => *price = last.close(),
            None => account.prices.push((symbol.clone(), last.close())),
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

/// Judgement summary for one symbol, when the judge produced one.
fn judgement_for_symbol<'a>(judgements: &'a [(Symbol, Value)], symbol: &str) -> Option<&'a Value> {
    judgements
        .iter()
        .find(|(candidate, _)| candidate.as_str() == symbol)
        .map(|(_, summary)| summary)
}

/// Parses a comma-separated symbol list: 1-8 distinct validated instruments.
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
        return Err(invalid("at most 8 symbols may rotate"));
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

/// Which policy produced a stop move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMoveKind {
    /// The stop moved to the entry price.
    BreakEven,
    /// The stop trailed behind the best favourable price.
    Trail,
}

impl StopMoveKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::BreakEven => "break_even",
            Self::Trail => "trailing_stop",
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

    /// Remembered entry risk for one ticket.
    fn risk(&self, ticket: i64) -> Option<f64> {
        let risks = match self.risks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        risks.get(&ticket).copied()
    }
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
async fn review_positions(
    state: &AppState,
    settings: &AutopilotSettings,
    session: &AgentSession<'_>,
    series: &CandleSeries,
    positions: &[ManagedPosition],
) -> TickOutcome {
    let judgements = judgement_for_symbol(session.judgements, series.symbol().as_str());
    let instructions = review_instructions(series, positions);
    let input = review_input(state, series, positions, judgements);
    let outcome = match agent::run(session, &instructions, &input).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let reason = match error {
                PipelineError::AgentLoopLimit { reason } => format!("agent loop: {reason}"),
                other => format!("model unavailable: {other}"),
            };
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
                "autopilot_review",
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
                "autopilot_review",
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
                    "autopilot_review",
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
                        "autopilot_review",
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
                        "autopilot_review",
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
                        "autopilot_review",
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
                StagedClose::TradingDisabled => {
                    record_position_context(
                        state,
                        "close_rejected",
                        "autopilot_review",
                        series,
                        Some(ticket),
                        Some("trading_disabled"),
                        None,
                        context,
                    )
                    .await;
                    TickOutcome::Rejected {
                        code: "trading_disabled",
                    }
                }
                StagedClose::ChannelUnavailable => {
                    let reason = "command channel unavailable".to_owned();
                    record_position_context(
                        state,
                        "unavailable",
                        "autopilot_review",
                        series,
                        Some(ticket),
                        Some(&reason),
                        None,
                        context,
                    )
                    .await;
                    TickOutcome::Unavailable { reason }
                }
                StagedClose::NoPositions | StagedClose::UnknownTicket => {
                    record_position_context(
                        state,
                        "close_rejected",
                        "autopilot_review",
                        series,
                        Some(ticket),
                        Some("stale_position"),
                        None,
                        context,
                    )
                    .await;
                    TickOutcome::Rejected {
                        code: "stale_position",
                    }
                }
                StagedClose::NotVeyra => {
                    record_position_context(
                        state,
                        "close_rejected",
                        "autopilot_review",
                        series,
                        Some(ticket),
                        Some("not_a_veyra_position"),
                        None,
                        context,
                    )
                    .await;
                    TickOutcome::Rejected {
                        code: "not_a_veyra_position",
                    }
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

/// Instructions for the hold-or-close review.
fn review_instructions(series: &CandleSeries, positions: &[ManagedPosition]) -> String {
    let tickets: Vec<String> = positions
        .iter()
        .map(|position| position.ticket.to_string())
        .collect();
    format!(
        "You are the analyst for Veyra, a single-instrument trading bot. Your open {symbol}          {timeframe} position(s) (ticket(s) {tickets}) were entered by this bot with a stop loss          and take profit attached.
         Decide for the reported position: `hold` keeps the entry bracket and lets the plan play          out; `close` flattens the ticket now because the thesis that justified the entry is no          longer supported by the latest candles and judgements.
         Closing costs the spread and abandons the bracket, so hold unless the evidence has          genuinely shifted; do not close merely because the position shows a small loss — the          attached stop defines the risk.
         Answer with the provided schema only, including the ticket when you close, and a short `rationale` (a sentence or two, at most 280 characters) explaining the decision. Before answering you may call read-only tools: get_market(symbol, timeframe?, bars?), get_judgements(symbol), get_account(), get_positions(), and get_market_window(). Call a tool only when its result would change your decision.",
        symbol = series.symbol().as_str(),
        timeframe = series.timeframe().as_str(),
        tickets = tickets.join(", ")
    )
}

/// Model input for the review: the same market block plus the position and
/// its bracket.
fn review_input(
    state: &AppState,
    series: &CandleSeries,
    positions: &[ManagedPosition],
    judgements: Option<&Value>,
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

    let server_time = state
        .broker()
        .and_then(|broker| broker.link().last_account())
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
            "recent": recent
        },
        "open_positions": open_positions
    });
    if let Some(judgements) = judgements {
        input["judgements"] = judgements.clone();
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
        .judge()
        .judge(request.clone())
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
                    "recent": recent
                }
            });
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
        "You are the analyst for Veyra, a systematic multi-asset trading bot. You are given a menu of          instruments ({menu}) with recent {timeframe} candles and optional calibrated judgements; the last          candle of each block is the most recent closed bar.\n         Decide for each instrument independently whether the evidence justifies opening a position right          now. You may open at most one instrument per answer. If none of them is suitable, answer `none`          \u{2014} skipping is normal and expected, and every instrument is reconsidered on the next tick.\n         Use the symbol exactly as written. Constraints: at most {max_orders} open orders and {max_total}          lots total exposure ({open_lots} lots currently open), one position per instrument, and volume at          most {max_volume} lots. If you open, use a market order \u{2014} omit `price` entirely \u{2014} with          both `stop_loss` and `take_profit` as absolute prices bracketing the entry, and stay within the          instrument's own price scale. Omit `comment` entirely (the bot annotates orders itself). A          deterministic risk gate re-validates everything and will reject anything outside these limits;          rejections are expected outcomes, not errors. Always include a short `rationale` (at most 280 characters) explaining why this instrument and direction `-` or, when answering none, why no instrument qualifies; the operator sees it in the decision journal. Before answering you may call read-only tools: get_judgements(symbol) for calibrated probabilities, get_market(symbol, timeframe?, bars?) for another window, get_account(), get_positions(), get_market_window() for session and rollover state, and check_risk(intent) to dry-run a draft through the deterministic gate. Call a tool only when its result would change your decision; otherwise answer none or open.",
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

    fn build_harness(
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
            ("VEYRA_AUTOPILOT_SYMBOLS", "A,B,C,D,E,F,G,H,I"),
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
        ] {
            let error = AutopilotSettings::from_source(|requested| match requested {
                _ if requested == name => Ok(value.to_owned()),
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
    async fn tick_feeds_judgements_into_the_model_input() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let judge = StubJudge::responding(judgements_response());
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
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

    #[actix_web::test]
    async fn tick_reports_empty_market_windows() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 0,
                fail: false,
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
                magic: crate::broker::ORDER_MAGIC,
            }],
            positions_truncated: false,
            server_time,
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
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "position_too_young"
            }
        );
        assert_eq!(outcomes(&harness.trail), vec!["close_rejected".to_owned()]);

        // Age cannot be verified.
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 7}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
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
        assert_eq!(
            tick(&harness.state).await,
            TickOutcome::Rejected {
                code: "position_age_unknown"
            }
        );

        // An unknown ticket is refused before any command exists.
        let engine = StubEngine::answering(json!({"action": "close", "ticket": 999}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine),
            Some(StubFeed {
                bars: 20,
                fail: false,
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

    #[actix_web::test]
    async fn entry_path_runs_when_only_foreign_positions_are_open() {
        let engine = StubEngine::answering(json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
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
    async fn tick_holds_at_r_when_break_even_is_disabled() {
        let engine =
            StubEngine::answering_review(json!({"action": "hold"}), json!({"action": "none"}));
        let harness = build_harness(
            enabled_settings(),
            Some(engine.clone()),
            Some(StubFeed {
                bars: 20,
                fail: false,
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
