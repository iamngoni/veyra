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

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::AppState;
use crate::audit::{AuditEvent, AuditKind};
use crate::broker::Symbol;
use crate::config::ConfigError;
use crate::control::{StagedExecution, queue_staged_order};
use crate::jev::{
    Answer, ChoiceOptions, Instructions, JevRequest, JevRuntime, NoulCriteria, Question,
    ScoreLevels, State as JevState,
};
use crate::market::{Candle, CandleRequest, CandleSeries, Timeframe};
use crate::model::ModelTier;
use crate::risk::AccountFacts;
use crate::trading::intent::TradeIntentDraft;
use crate::trading::pipeline::{PipelineOutcome, evaluate_proposal};

/// Default proposal cadence in seconds.
const DEFAULT_INTERVAL_SECS: u64 = 300;
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
    symbol: Option<Symbol>,
    timeframe: Timeframe,
    bars: u16,
    tier: ModelTier,
    interval: Duration,
    jev: JevPreference,
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
        let timeframe_raw = optional(&mut source, "VEYRA_AUTOPILOT_TIMEFRAME");
        let bars_raw = optional(&mut source, "VEYRA_AUTOPILOT_BARS");
        let tier_raw = optional(&mut source, "VEYRA_AUTOPILOT_TIER");
        let interval_raw = optional(&mut source, "VEYRA_AUTOPILOT_INTERVAL_SECS");
        let jev_raw = optional(&mut source, "VEYRA_AUTOPILOT_JEV");

        if enabled_raw.is_empty()
            && symbol_raw.is_empty()
            && timeframe_raw.is_empty()
            && bars_raw.is_empty()
            && tier_raw.is_empty()
            && interval_raw.is_empty()
            && jev_raw.is_empty()
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
        let symbol = match symbol_raw.as_str() {
            "" => None,
            other => Some(Symbol::parse(other).map_err(|_| {
                ConfigError::InvalidEnvironmentVariable {
                    name: "VEYRA_AUTOPILOT_SYMBOL",
                    reason: "must be 1-24 characters of letters, digits, '.', '_', '#', '+' or '-'",
                }
            })?),
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

        Ok(Some(Self {
            enabled,
            symbol,
            timeframe,
            bars,
            tier,
            interval,
            jev,
        }))
    }

    /// Whether the loop runs at all.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Explicit instrument, when configured; otherwise the chart symbol.
    pub fn symbol(&self) -> Option<&Symbol> {
        self.symbol.as_ref()
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
    let symbol = settings.symbol().cloned().or_else(|| {
        report
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.symbol().clone())
    });
    let Some(symbol) = symbol else {
        return TickOutcome::Skipped {
            reason: "symbol_unavailable",
        };
    };
    let Some(account) = crate::routes::account_facts(Some(broker)).await else {
        return TickOutcome::Skipped {
            reason: "account_unavailable",
        };
    };

    let request = match CandleRequest::new(symbol.clone(), settings.timeframe(), settings.bars()) {
        Ok(request) => request,
        Err(error) => {
            let reason = format!("market request: {error}");
            record(state, "unavailable", Some(&symbol), None, Some(&reason)).await;
            return TickOutcome::Unavailable { reason };
        }
    };
    let series = match market.feed().candles(request).await {
        Ok(series) => series,
        Err(error) => {
            let reason = format!("market unavailable: {error}");
            record(state, "unavailable", Some(&symbol), None, Some(&reason)).await;
            return TickOutcome::Unavailable { reason };
        }
    };
    if series.candles().is_empty() {
        let reason = "market returned no candles".to_owned();
        record(state, "unavailable", Some(&symbol), None, Some(&reason)).await;
        return TickOutcome::Unavailable { reason };
    }

    let judgements = match settings.jev() {
        JevPreference::Off => None,
        JevPreference::Auto => match state.jev() {
            None => None,
            Some(jev) => match judgements_for(jev, &series).await {
                Ok(summary) => Some(summary),
                Err(error) => {
                    let reason = format!("judgement unavailable: {error}");
                    record(state, "unavailable", Some(&symbol), None, Some(&reason)).await;
                    return TickOutcome::Unavailable { reason };
                }
            },
        },
    };

    let input = proposal_input(&series, account, judgements.as_ref());
    let instructions = proposal_instructions(state, &series, account);
    match evaluate_proposal(
        model.engine().as_ref(),
        state.risk(),
        instructions,
        input,
        settings.tier(),
        Some(account),
        SystemTime::now(),
    )
    .await
    {
        Err(error) => {
            let reason = format!("model unavailable: {error}");
            record(state, "unavailable", Some(&symbol), None, Some(&reason)).await;
            TickOutcome::Unavailable { reason }
        }
        Ok(PipelineOutcome::NoTrade) => {
            record(state, "no_trade", Some(&symbol), None, None).await;
            TickOutcome::NoTrade
        }
        Ok(PipelineOutcome::Rejected { rejection, draft }) => {
            record(
                state,
                "rejected",
                Some(&symbol),
                Some(&draft),
                Some(rejection.code().as_str()),
            )
            .await;
            TickOutcome::Rejected {
                code: rejection.code().as_str(),
            }
        }
        Ok(PipelineOutcome::Approved(intent)) => {
            let draft = intent.draft();
            if draft.stop_loss().is_none() || draft.take_profit().is_none() {
                record(
                    state,
                    "rejected",
                    Some(&symbol),
                    Some(draft),
                    Some("missing_stops"),
                )
                .await;
                return TickOutcome::Rejected {
                    code: "missing_stops",
                };
            }
            match queue_staged_order(state, &intent).await {
                StagedExecution::Queued { command, .. } => {
                    record(state, "queued", Some(&symbol), Some(draft), None).await;
                    TickOutcome::Queued {
                        command: command.to_string(),
                    }
                }
                StagedExecution::TradingDisabled => {
                    record(state, "approved_dry_run", Some(&symbol), Some(draft), None).await;
                    TickOutcome::ApprovedDryRun
                }
                StagedExecution::ChannelUnavailable => {
                    let reason = "command channel unavailable".to_owned();
                    record(
                        state,
                        "unavailable",
                        Some(&symbol),
                        Some(draft),
                        Some(&reason),
                    )
                    .await;
                    TickOutcome::Unavailable { reason }
                }
            }
        }
    }
}

/// Records one decision attempt; best-effort and bounded in size.
async fn record(
    state: &AppState,
    outcome: &'static str,
    symbol: Option<&Symbol>,
    draft: Option<&TradeIntentDraft>,
    reason: Option<&str>,
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
    }
    if let Some(reason) = reason {
        payload["reason"] = json!(reason);
    }
    audit
        .try_record(AuditEvent::new(AuditKind::ProposalEvaluated, payload))
        .await;
}

/// Asks the judgement engine for calibrated answers over the same market
/// state, returning a compact JSON summary for the model input.
async fn judgements_for(jev: &JevRuntime, series: &CandleSeries) -> Result<Value, String> {
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
    series: &CandleSeries,
    account: AccountFacts,
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
        "account": {
            "open_orders": account.open_orders,
            "open_lots": account.open_lots,
            "trade_allowed": account.trade_allowed
        }
    });
    if let Some(judgements) = judgements {
        input["judgements"] = judgements.clone();
    }
    input.to_string()
}

/// Assembles the analyst instructions, including the enforced volume cap.
fn proposal_instructions(state: &AppState, series: &CandleSeries, account: AccountFacts) -> String {
    let max_volume = state.risk().policy().max_volume_per_order().value();
    format!(
        "You are the analyst for Veyra, a single-instrument trading bot. Decide whether to open one \
         {symbol} {timeframe} position right now.\n\
         Your input carries recent {timeframe} candles plus optional calibrated judgements; the last \
         candle is the most recent closed bar.\n\
         Answer with the provided schema only. Choose `none` unless the evidence is clear and \
         one-sided; answering none is normal and expected when the market is ambiguous.\n\
         Constraints: at most one open position at a time (`{open_orders}` order(s) already open \
         with {open_lots} lots); if you open, use a market order \u{2014} omit `price` entirely \u{2014} \
         with volume at most {max_volume} lots, and include both `stop_loss` and `take_profit` as \
         absolute prices bracketing the entry. Omit `comment` entirely (the bot annotates orders \
         itself). A deterministic risk gate will reject anything outside the configured limits, and \
         rejections are expected outcomes, not errors.",
        symbol = series.symbol().as_str(),
        timeframe = series.timeframe().as_str(),
        open_orders = account.open_orders,
        open_lots = account.open_lots,
        max_volume = max_volume
    )
}

/// Highest candle high across the window.
fn window_high(series: &CandleSeries) -> f64 {
    series
        .candles()
        .iter()
        .map(|candle| candle.high())
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Lowest candle low across the window.
fn window_low(series: &CandleSeries) -> f64 {
    series
        .candles()
        .iter()
        .map(|candle| candle.low())
        .fold(f64::INFINITY, f64::min)
}

/// Percentage change from the first to the last close, rounded to four
/// decimals to keep the model input compact.
fn change_pct(series: &CandleSeries) -> f64 {
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
    use crate::broker::ea::CommandKind;
    use crate::broker::{AccountLogin, AccountSnapshot, BrokerRuntime, BrokerSettings, ServerName};
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
        seen: Mutex<Vec<DecisionRequest>>,
    }

    impl StubEngine {
        fn answering(answer: Value) -> Arc<Self> {
            Arc::new(Self {
                answer: Some(answer),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                answer: None,
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
            self.seen.lock().expect("lock").push(request);
            match &self.answer {
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
        json!({ "action": "open", "intent": intent })
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
        assert!(defaults.symbol().is_none());

        let custom = settings_from(|name| match name {
            "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
            "VEYRA_AUTOPILOT_SYMBOL" => Ok("gbpusd".to_owned()),
            "VEYRA_AUTOPILOT_TIMEFRAME" => Ok("240".to_owned()),
            "VEYRA_AUTOPILOT_BARS" => Ok("96".to_owned()),
            "VEYRA_AUTOPILOT_TIER" => Ok("reasoning".to_owned()),
            "VEYRA_AUTOPILOT_INTERVAL_SECS" => Ok("60".to_owned()),
            "VEYRA_AUTOPILOT_JEV" => Ok("off".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        });
        assert!(custom.enabled());
        assert_eq!(custom.symbol().expect("symbol").as_str(), "gbpusd");
        assert_eq!(custom.timeframe(), Timeframe::H4);
        assert_eq!(custom.bars(), 96);
        assert_eq!(custom.tier(), ModelTier::Reasoning);
        assert_eq!(custom.interval(), Duration::from_secs(60));
        assert_eq!(custom.jev(), JevPreference::Off);
        assert_eq!(JevPreference::parse(" TRUE "), Some(JevPreference::Auto));
        assert_eq!(JevPreference::parse("no"), None);
        assert_eq!(JevPreference::Auto.as_str(), "auto");
        assert_eq!(JevPreference::Off.as_str(), "off");

        for (name, value) in [
            ("VEYRA_AUTOPILOT_ENABLED", "sure"),
            ("VEYRA_AUTOPILOT_SYMBOL", "bad symbol"),
            ("VEYRA_AUTOPILOT_TIMEFRAME", "H6"),
            ("VEYRA_AUTOPILOT_BARS", "9"),
            ("VEYRA_AUTOPILOT_BARS", "241"),
            ("VEYRA_AUTOPILOT_TIER", "genius"),
            ("VEYRA_AUTOPILOT_INTERVAL_SECS", "29"),
            ("VEYRA_AUTOPILOT_INTERVAL_SECS", "many"),
            ("VEYRA_AUTOPILOT_JEV", "always"),
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
        assert_eq!(event.payload()["symbol"], "EURUSD");
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
            None,
            true,
            true,
        );
        let outcome = tick(&harness.state).await;
        let command = match outcome {
            TickOutcome::Queued { command } => command,
            other => panic!("expected a queued command, got {other:?}"),
        };
        assert!(outcomes(&harness.trail).contains(&"queued".to_owned()));
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
        let id = crate::broker::ea::CommandId::parse(&command).expect("command id");
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
        assert_eq!(input["judgements"]["direction"]["choice"], "long");
        assert_eq!(input["judgements"]["trending"]["probability"], 0.7);
        assert_eq!(input["judgements"]["momentum"]["score"], 1.5);
        assert_eq!(input["market"]["bars"], 20);
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
