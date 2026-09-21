//! Agent decision loop.
//!
//! The model is not limited to one call. It receives the decision context,
//! then may call read-only tools — asking Jev for calibrated judgements,
//! fetching another market window, inspecting account/position/window state,
//! or dry-running a draft through the deterministic risk gate — and each
//! result is appended to a transcript and fed back as input. The loop ends
//! when the model answers the final schema (a proposal, or a hold/close
//! review), or when a step/tool bound is reached (fail closed).
//!
//! Tools are executed by the service, never by the model, so every provider
//! that supports structured completion gets the same agentic behaviour
//! without provider-native tool APIs, and every tool call is journaled as an
//! `agent_tool_called` audit event for the console.

use std::time::SystemTime;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::audit::{AuditEvent, AuditKind};
use crate::broker::{ORDER_MAGIC, Symbol};
use crate::market::{CandleRequest, CandleSeries, Timeframe};
use crate::model::{AnswerFormat, DecisionEngine, DecisionRequest, ModelTier};
use crate::risk::AccountFacts;
use crate::trading::autopilot::{
    ReviewDecision, change_pct, judgements_for, parse_review, window_high, window_low,
};
use crate::trading::intent::TradeProposal;
use crate::trading::pipeline::{self, PipelineError, ProposalEvaluation};

/// Forced schema name for every loop step.
pub(crate) const AGENT_SCHEMA_NAME: &str = "veyra_agent_step";
/// Hard bound on loop iterations (model calls, tool calls included) per
/// decision. Every iteration either executes one tool or ends the loop, so
/// this also bounds tool usage.
pub(crate) const MAX_STEPS: usize = 8;
/// Largest rendered tool result in the transcript.
const TRANSCRIPT_RESULT_CHARS: usize = 900;
/// Largest tool result kept in the audit journal.
const JOURNAL_RESULT_CHARS: usize = 2_000;
/// Largest tool arguments kept in the audit journal.
const JOURNAL_ARGS_CHARS: usize = 1_000;
/// Largest prompt body kept per turn. Generous on purpose: the cadence gate
/// bounds decisions to a handful a day, so the whole prompt normally fits and
/// the record is the exact text rather than an impression of it. The stored
/// character count exposes the rare case where it did not.
const JOURNAL_PROMPT_CHARS: usize = 64_000;
/// Largest model answer kept per turn.
const JOURNAL_ANSWER_CHARS: usize = 2_000;

/// Which final schema the loop is allowed to end on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentMode {
    /// Entry decision: `none` or one bracketed `open` intent.
    Proposal,
    /// Position review: `hold` or `close` a ticket.
    Review,
}

/// One agent decision session: the loop's dependencies and context.
pub(crate) struct AgentSession<'a> {
    /// Shared service state (audit, judge, market, broker, risk).
    pub state: &'a AppState,
    /// Model engine each step is answered by.
    pub engine: &'a dyn DecisionEngine,
    /// Final schema the loop must end on.
    pub mode: AgentMode,
    /// Candidate market windows available to this decision.
    pub markets: &'a [(Symbol, CandleSeries)],
    /// Account facts captured before the loop started.
    pub account: &'a AccountFacts,
    /// Judgements already computed for this tick (per symbol).
    pub judgements: &'a [(Symbol, Value)],
    /// Capability tier for every model call in the loop.
    pub tier: ModelTier,
    /// Evaluation instant, shared by the gate preview and window tools.
    pub now: SystemTime,
}

/// One executed tool call, for the final journal record.
#[derive(Debug, Clone)]
pub(crate) struct AgentToolUse {
    /// Tool name, in call order. Arguments and results are journaled by the
    /// `agent_tool_called` audit events this loop records.
    pub name: String,
}

/// How a loop ended.
#[derive(Debug)]
pub(crate) enum AgentDecision {
    /// Entry decision evaluated through the deterministic gate.
    Proposal(ProposalEvaluation),
    /// Position review verdict.
    Review(ReviewDecision),
}

/// Outcome of one agent decision.
#[derive(Debug)]
pub(crate) struct AgentOutcome {
    /// Final decision.
    pub decision: AgentDecision,
    /// Model rationale for the final answer.
    pub rationale: Option<String>,
    /// Read-only tools the model used along the way.
    pub tool_calls: Vec<AgentToolUse>,
}

/// Runs the loop until the model answers `mode`'s final schema.
///
/// # Errors
/// Returns [`PipelineError::AgentLoopLimit`] when a bound is reached and
/// [`PipelineError::InvalidProposal`] when an answer cannot be parsed.
pub(crate) async fn run(
    session: &AgentSession<'_>,
    instructions: &str,
    input: &str,
) -> Result<AgentOutcome, PipelineError> {
    let mut transcript: Vec<String> = Vec::new();
    let mut tools: Vec<AgentToolUse> = Vec::new();

    for step in 0..MAX_STEPS {
        let step_input = render_input(input, &transcript);
        let answer = session
            .engine
            .answer(DecisionRequest {
                instructions: instructions.to_owned(),
                input: step_input.clone(),
                format: format_for(session.mode),
                tier: session.tier,
            })
            .await?;
        record_turn(session, step + 1, instructions, &step_input, &answer.value).await;
        let rationale = pipeline::parse_rationale(&answer.value);

        match parse_step(&answer.value, session, rationale.clone())? {
            AgentStep::Tool { name, arguments } => {
                let result = match call_tool(session, &name, &arguments).await {
                    Ok(value) => value,
                    Err(message) => json!({ "error": message }),
                };
                record_tool_call(
                    session,
                    step + 1,
                    &name,
                    &arguments,
                    &result,
                    rationale.as_deref(),
                )
                .await;
                transcript.push(format!(
                    "#{} {} -> {}",
                    step + 1,
                    render_tool(&name, &arguments),
                    render_value(&result, TRANSCRIPT_RESULT_CHARS)
                ));
                tools.push(AgentToolUse { name });
            }
            AgentStep::Done {
                decision,
                rationale,
            } => {
                return Ok(AgentOutcome {
                    decision,
                    rationale,
                    tool_calls: tools,
                });
            }
        }
    }

    Err(PipelineError::AgentLoopLimit {
        reason: format!("step bound ({MAX_STEPS} model calls) reached without a final answer"),
    })
}

/// One parsed loop step.
enum AgentStep {
    /// The model asked for a read-only tool.
    Tool { name: String, arguments: Value },
    /// The model gave its final answer for this mode.
    Done {
        decision: AgentDecision,
        rationale: Option<String>,
    },
}

/// Wire shape of one model answer.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RawAnswer {
    None,
    Open { intent: Value },
    Hold,
    Close { ticket: i64 },
    Tool { tool: RawTool },
}

/// Wire shape of one tool request.
#[derive(Debug, Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    arguments: Value,
}

/// Parses and mode-checks one model answer.
fn parse_step(
    value: &Value,
    session: &AgentSession<'_>,
    rationale: Option<String>,
) -> Result<AgentStep, PipelineError> {
    let raw: RawAnswer =
        serde_json::from_value(value.clone()).map_err(|error| PipelineError::InvalidProposal {
            reason: format!("not a valid agent step: {error}"),
        })?;
    let invalid = |reason: String| PipelineError::InvalidProposal { reason };
    match raw {
        RawAnswer::Tool { tool, .. } => Ok(AgentStep::Tool {
            name: tool.name,
            arguments: tool.arguments,
        }),
        RawAnswer::None if session.mode == AgentMode::Proposal => {
            let evaluation = pipeline::evaluate_answer(
                json!({ "action": "none", "rationale": rationale }),
                session.state.risk(),
                Some(session.account.clone()),
                session.now,
            )?;
            Ok(AgentStep::Done {
                decision: AgentDecision::Proposal(evaluation),
                rationale,
            })
        }
        RawAnswer::Open { intent, .. } if session.mode == AgentMode::Proposal => {
            let evaluation = pipeline::evaluate_answer(
                json!({ "action": "open", "rationale": rationale, "intent": intent }),
                session.state.risk(),
                Some(session.account.clone()),
                session.now,
            )?;
            Ok(AgentStep::Done {
                decision: AgentDecision::Proposal(evaluation),
                rationale,
            })
        }
        RawAnswer::Hold if session.mode == AgentMode::Review => Ok(AgentStep::Done {
            decision: AgentDecision::Review(ReviewDecision::Hold),
            rationale,
        }),
        RawAnswer::Close { ticket, .. } if session.mode == AgentMode::Review => {
            let answer = json!({ "action": "close", "ticket": ticket });
            let decision = parse_review(&answer).map_err(invalid)?;
            Ok(AgentStep::Done {
                decision: AgentDecision::Review(decision),
                rationale,
            })
        }
        wrong => Err(invalid(format!(
            "`{}` is not a valid answer for a {} decision",
            action_name(&wrong),
            match session.mode {
                AgentMode::Proposal => "entry",
                AgentMode::Review => "review",
            }
        ))),
    }
}

/// Action name of a parsed answer, for error messages.
fn action_name(answer: &RawAnswer) -> &'static str {
    match answer {
        RawAnswer::None => "none",
        RawAnswer::Open { .. } => "open",
        RawAnswer::Hold => "hold",
        RawAnswer::Close { .. } => "close",
        RawAnswer::Tool { .. } => "tool",
    }
}

/// Builds the forced schema for the loop's mode.
pub(crate) fn format_for(mode: AgentMode) -> AnswerFormat {
    let (actions, extra) = match mode {
        AgentMode::Proposal => (
            json!(["none", "open", "tool"]),
            Some(("intent", intent_schema())),
        ),
        AgentMode::Review => (
            json!(["hold", "close", "tool"]),
            Some(("ticket", ticket_schema())),
        ),
    };
    let mut properties = json!({
        "action": { "type": "string", "enum": actions },
        "rationale": {
            "type": ["string", "null"],
            "maxLength": 280,
            "description": "Short operator-facing explanation of this step. Always include it."
        },
        "tool": {
            "type": "object",
            "additionalProperties": false,
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "enum": [
                        "get_judgements",
                        "get_market",
                        "get_account",
                        "get_positions",
                        "get_market_window",
                        "check_risk"
                    ]
                },
                "arguments": { "type": "object" }
            }
        }
    });
    if let (Some((name, schema)), Some(map)) = (extra, properties.as_object_mut()) {
        map.insert(name.to_owned(), schema);
    }
    AnswerFormat {
        name: AGENT_SCHEMA_NAME.to_owned(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action"],
            "properties": properties
        }),
    }
}

/// Intent property shared with the one-shot proposal schema.
fn intent_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["symbol", "side", "order_type", "volume"],
        "properties": {
            "symbol": { "type": "string" },
            "side": { "type": "string", "enum": ["buy", "sell"] },
            "order_type": {
                "type": "string",
                "enum": ["market", "limit", "stop"],
                "description": "market executes immediately; omit `price` entirely for market orders"
            },
            "price": { "type": ["number", "null"] },
            "volume": { "type": "number", "exclusiveMinimum": 0 },
            "stop_loss": { "type": ["number", "null"] },
            "take_profit": { "type": ["number", "null"] },
            "comment": { "type": ["string", "null"] }
        }
    })
}

/// Ticket property for the review schema.
fn ticket_schema() -> Value {
    json!({
        "type": ["integer", "null"],
        "description": "the position to close; required when action is close"
    })
}

/// Renders the base input plus the transcript so far.
fn render_input(base: &str, transcript: &[String]) -> String {
    if transcript.is_empty() {
        return base.to_owned();
    }
    format!(
        "{base}\n\n## Agent transcript\n{}\n\n\
         Answer with the next step. Call a tool when it would change your decision; \
         otherwise give the final answer for this decision.",
        transcript.join("\n")
    )
}

/// Renders one tool invocation for the transcript.
fn render_tool(name: &str, arguments: &Value) -> String {
    format!("{name}({})", render_value(arguments, 200))
}

/// Compact one JSON value for a bounded log line.
fn render_value(value: &Value, max_chars: usize) -> String {
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    if rendered.len() <= max_chars {
        rendered
    } else {
        format!("{}…", &rendered[..max_chars.min(rendered.len())])
    }
}

/// Bounded copy for the durable journal.
fn bounded(value: &Value, max_chars: usize) -> Value {
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    if rendered.len() <= max_chars {
        value.clone()
    } else {
        json!({ "truncated": format!("{}…", &rendered[..max_chars.min(rendered.len())]) })
    }
}

/// Executes one read-only tool. Errors are observations the model can correct.
async fn call_tool(
    session: &AgentSession<'_>,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    match name {
        "get_judgements" => tool_judgements(session, arguments).await,
        "get_market" => tool_market(session, arguments).await,
        "get_account" => Ok(tool_account(session)),
        "get_positions" => tool_positions(session),
        "get_market_window" => tool_market_window(session),
        "check_risk" => tool_check_risk(session, arguments),
        other => Err(format!(
            "unknown tool `{other}`; available tools: get_judgements, get_market, \
             get_account, get_positions, get_market_window, check_risk"
        )),
    }
}

/// Parses the `symbol` argument.
fn required_symbol(arguments: &Value) -> Result<Symbol, String> {
    let raw = arguments
        .get("symbol")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing `symbol` argument".to_owned())?;
    Symbol::parse(raw).map_err(|error| format!("invalid symbol: {error}"))
}

/// Jev judgements for one candidate: cached from the tick, or computed now.
async fn tool_judgements(session: &AgentSession<'_>, arguments: &Value) -> Result<Value, String> {
    let symbol = required_symbol(arguments)?;
    if let Some((_, summary)) = session
        .judgements
        .iter()
        .find(|(candidate, _)| candidate.as_str() == symbol.as_str())
    {
        return Ok(summary.clone());
    }
    let Some((_, series)) = session
        .markets
        .iter()
        .find(|(candidate, _)| candidate.as_str() == symbol.as_str())
    else {
        let menu = session
            .markets
            .iter()
            .map(|(candidate, _)| candidate.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "`{}` is not in this decision's candidate menu ({menu})",
            symbol.as_str()
        ));
    };
    let Some(jev) = session.state.jev() else {
        return Err("no judgement provider is configured".to_owned());
    };
    judgements_for(jev, series).await
}

/// Closed candles for an allowed symbol and timeframe.
async fn tool_market(session: &AgentSession<'_>, arguments: &Value) -> Result<Value, String> {
    let symbol = required_symbol(arguments)?;
    let in_menu = session
        .markets
        .iter()
        .any(|(candidate, _)| candidate.as_str() == symbol.as_str());
    if !in_menu && !session.state.risk().policy().allows_symbol(&symbol) {
        return Err(format!(
            "`{}` is outside the configured symbol allowlist",
            symbol.as_str()
        ));
    }
    let timeframe = match arguments.get("timeframe").and_then(Value::as_str) {
        Some(raw) => Timeframe::parse(raw).ok_or_else(|| format!("unknown timeframe `{raw}`"))?,
        None => Timeframe::H4,
    };
    let bars = match arguments.get("bars") {
        Some(value) => value.as_u64().ok_or("`bars` must be an integer")?,
        None => 48,
    };
    if bars == 0 || bars > 240 {
        return Err("`bars` must be from 1 through 240".to_owned());
    }
    let request = CandleRequest::new(symbol, timeframe, bars as u16).map_err(|e| e.to_string())?;
    let market = session
        .state
        .market()
        .ok_or("no market feed is configured")?;
    let series = market
        .feed()
        .candles(request)
        .await
        .map_err(|error| error.to_string())?;
    Ok(market_block(&series))
}

/// Compact market summary for one series.
fn market_block(series: &CandleSeries) -> Value {
    let recent: Vec<Value> = series
        .candles()
        .iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
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
    json!({
        "symbol": series.symbol().as_str(),
        "timeframe": series.timeframe().as_str(),
        "bars": series.candles().len(),
        "last_close": series.last().map(|candle| candle.close()),
        "window_high": window_high(series),
        "window_low": window_low(series),
        "change_pct": change_pct(series),
        "recent": recent
    })
}

/// Current gate facts for the model.
fn tool_account(session: &AgentSession<'_>) -> Value {
    let account = session.account;
    json!({
        "open_orders": account.open_orders,
        "open_lots": account.open_lots,
        "trade_allowed": account.trade_allowed,
        "equity": account.equity,
        "open_symbols": account.open_symbols.iter().map(|symbol| symbol.as_str()).collect::<Vec<_>>()
    })
}

/// Open positions as the venue reports them.
fn tool_positions(session: &AgentSession<'_>) -> Result<Value, String> {
    let broker = session.state.broker().ok_or("no broker is configured")?;
    let link = broker.link();
    let snapshot = link
        .last_account()
        .ok_or("no validated account snapshot has been retained yet")?;
    let age_secs = link.last_account_age(session.now).map(|age| age.as_secs());
    let positions: Vec<Value> = snapshot
        .positions
        .iter()
        .map(|position| {
            json!({
                "ticket": position.ticket,
                "symbol": position.symbol,
                "kind": serde_json::to_value(position.kind).unwrap_or(Value::Null),
                "lots": position.lots,
                "entry": position.price,
                "current": position.current,
                "profit": position.profit,
                "stop_loss": position.stop_loss,
                "take_profit": position.take_profit,
                "opened_at": position.opened_at,
                "managed_by_veyra": position.magic == ORDER_MAGIC
            })
        })
        .collect();
    Ok(json!({
        "server_time": snapshot.server_time,
        "snapshot_age_secs": age_secs,
        "orders": snapshot.orders,
        "lots": snapshot.lots,
        "positions": positions
    }))
}

/// Calendar state: session, rollover blackout, weekend guards.
fn tool_market_window(session: &AgentSession<'_>) -> Result<Value, String> {
    let policy = session.state.risk().policy();
    let (weekday, minute) = crate::risk::window::utc_now_parts(session.now)
        .ok_or("the system clock is before the Unix epoch")?;
    let weekday_name = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ]
    .get(weekday as usize)
    .unwrap_or(&"Unknown");
    let session_closed = policy
        .session()
        .is_some_and(|window| !window.contains((minute / 60) as u8));
    let candidates: Vec<Value> = session
        .markets
        .iter()
        .map(|(symbol, _)| {
            let block = if session_closed {
                Some(crate::risk::window::WindowBlock::SessionClosed)
            } else if policy.allows_weekend(symbol) {
                None
            } else {
                crate::risk::window::entry_block(session.now, None)
            };
            json!({
                "symbol": symbol.as_str(),
                "entry_window_open": block.is_none(),
                "block": block.map(|value| value.as_str())
            })
        })
        .collect();
    let any_open = candidates
        .iter()
        .any(|candidate| candidate["entry_window_open"] == true);
    let aggregate_block = if any_open {
        None
    } else if session_closed {
        Some(crate::risk::window::WindowBlock::SessionClosed)
    } else {
        crate::risk::window::entry_block(session.now, None)
    };
    Ok(json!({
        "utc_weekday": weekday_name,
        "utc_minute_of_day": minute,
        "entry_window_open": any_open,
        "block": aggregate_block.map(|block| json!({
            "code": block.as_str(),
            "detail": block.detail()
        })),
        "candidates": candidates,
        "configured_session_utc": policy.session().map(|window| format!(
            "{:02}:00-{:02}:00",
            window.start_hour(),
            window.end_hour()
        )),
        "note": "entries are refused while the window is closed; stops and reviews continue"
    }))
}

/// Dry-runs one draft through the gate without touching duplicate memory.
fn tool_check_risk(session: &AgentSession<'_>, arguments: &Value) -> Result<Value, String> {
    let intent = arguments
        .get("intent")
        .cloned()
        .ok_or("missing `intent` argument; pass the same object shape as a proposal intent")?;
    let proposal: TradeProposal = serde_json::from_value(json!({
        "action": "open",
        "intent": intent
    }))
    .map_err(|error| format!("invalid intent: {error}"))?;
    let TradeProposal::Open(draft) = proposal else {
        return Err("intent did not parse as an open proposal".to_owned());
    };
    let decision = session
        .state
        .risk()
        .preview(&draft, Some(session.account.clone()), session.now);
    serde_json::to_value(&decision).map_err(|error| error.to_string())
}

/// Journals one model turn: what it was shown, and what it answered.
///
/// The prompt is stored whole up to a generous bound, so the record is the
/// text the model actually saw rather than a summary of it. The character
/// count is stored beside it, which is the only way a reader can tell a
/// complete record from a clipped one.
///
/// No credential can reach here. The instructions and input are built from
/// market data and account facts; the API key never enters either, and travels
/// to the provider through the HTTP client alone.
async fn record_turn(
    session: &AgentSession<'_>,
    step: usize,
    instructions: &str,
    input: &str,
    answer: &Value,
) {
    let Some(audit) = session.state.audit() else {
        return;
    };
    let clipped = |text: &str, max: usize| -> Value {
        match text.char_indices().nth(max) {
            None => json!(text),
            Some((cut, _)) => json!(format!("{}…", &text[..cut])),
        }
    };
    audit
        .try_record(AuditEvent::new(
            AuditKind::AgentTurn,
            json!({
                "outcome": "agent_turn",
                "origin": "autopilot_agent",
                "step": step,
                "mode": match session.mode {
                    AgentMode::Proposal => "proposal",
                    AgentMode::Review => "review",
                },
                "instructions": clipped(instructions, JOURNAL_PROMPT_CHARS),
                "instructionsChars": instructions.chars().count(),
                "input": clipped(input, JOURNAL_PROMPT_CHARS),
                "inputChars": input.chars().count(),
                "answer": bounded(answer, JOURNAL_ANSWER_CHARS)
            }),
        ))
        .await;
}

/// Journals one tool execution for the console and audit trail.
async fn record_tool_call(
    session: &AgentSession<'_>,
    step: usize,
    name: &str,
    arguments: &Value,
    result: &Value,
    rationale: Option<&str>,
) {
    let Some(audit) = session.state.audit() else {
        return;
    };
    let mut payload = json!({
        "outcome": "tool_call",
        "origin": "autopilot_agent",
        "step": step,
        "tool": name,
        "arguments": bounded(arguments, JOURNAL_ARGS_CHARS),
        "result": bounded(result, JOURNAL_RESULT_CHARS)
    });
    if let Some(rationale) = rationale {
        payload["rationale"] = json!(rationale);
    }
    audit
        .try_record(AuditEvent::new(AuditKind::AgentToolCalled, payload))
        .await;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::audit::{AuditKind, AuditRuntime, MemoryTrail};
    use crate::config::{ConfigError, ServiceConfig};
    use crate::market::{Candle, MarketError, MarketFeed, MarketProvider, MarketRuntime};
    use crate::model::{DecisionAnswer, ModelError, ModelProvider};
    use crate::risk::{RiskGate, RiskPolicy};
    use crate::trading::intent::Volume;
    use crate::trading::pipeline::PipelineOutcome;

    /// Deterministic engine: pops one scripted answer per call, repeating the
    /// last one once the script runs out.
    #[derive(Debug)]
    struct ScriptedEngine {
        answers: Mutex<VecDeque<Value>>,
        inputs: Mutex<Vec<String>>,
    }

    impl ScriptedEngine {
        fn new(answers: Vec<Value>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                inputs: Mutex::new(Vec::new()),
            })
        }

        fn inputs(&self) -> Vec<String> {
            self.inputs.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl DecisionEngine for ScriptedEngine {
        fn provider(&self) -> ModelProvider {
            ModelProvider::OpenRouter
        }

        async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
            self.inputs
                .lock()
                .expect("lock")
                .push(request.input.clone());
            let mut answers = self.answers.lock().expect("lock");
            let value = match answers.len() {
                0 => {
                    return Err(ModelError::Request {
                        reason: "script exhausted".to_owned(),
                    });
                }
                1 => answers.front().cloned().expect("front"),
                _ => answers.pop_front().expect("pop"),
            };
            Ok(DecisionAnswer { value })
        }
    }

    /// Feed that answers any request with one validated candle.
    #[derive(Debug)]
    struct StubFeed;

    #[async_trait]
    impl MarketFeed for StubFeed {
        fn provider(&self) -> MarketProvider {
            MarketProvider::Ea
        }

        async fn candles(&self, request: CandleRequest) -> Result<CandleSeries, MarketError> {
            Ok(CandleSeries::from_validated(
                request.symbol().clone(),
                request.timeframe(),
                vec![Candle::from_validated(
                    1_700_000_000,
                    1.0,
                    1.1,
                    0.9,
                    1.05,
                    10,
                )],
            ))
        }

        async fn symbol_spec(
            &self,
            _symbol: &Symbol,
        ) -> Result<crate::broker::SymbolSpecPayload, MarketError> {
            Err(MarketError::Unavailable {
                reason: "stub has no contract data".to_owned(),
            })
        }
    }

    fn config() -> ServiceConfig {
        ServiceConfig::from_source(|name| match name {
            "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
            "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
            "VEYRA_ENV" => Ok("development".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("config parses")
    }

    fn gate() -> RiskGate {
        RiskGate::new(RiskPolicy::new(
            false,
            vec![Symbol::parse("EURUSD").expect("symbol")],
            Volume::parse(0.05).expect("volume"),
            Volume::parse(0.05).expect("volume"),
            5,
            Duration::from_secs(60),
            None,
        ))
    }

    fn facts() -> AccountFacts {
        AccountFacts {
            trade_allowed: true,
            open_orders: 0,
            open_lots: 0.0,
            open_symbols: Vec::new(),
            equity: Some(1_000.0),
            free_margin: Some(1_000.0),
            open_positions: Vec::new(),
            prices: Vec::new(),
            symbol_specs: Vec::new(),
            day_drawdown_percent: None,
            peak_drawdown_percent: None,
        }
    }

    fn series() -> CandleSeries {
        CandleSeries::from_validated(
            Symbol::parse("EURUSD").expect("symbol"),
            Timeframe::H4,
            vec![
                Candle::from_validated(1_700_000_000, 1.0, 1.1, 0.9, 1.05, 10),
                Candle::from_validated(1_700_014_400, 1.05, 1.2, 1.0, 1.15, 12),
            ],
        )
    }

    /// Judge that answers the three questions without any network call.
    #[derive(Debug)]
    struct StubJudge;

    #[async_trait]
    impl crate::jev::SemanticJudge for StubJudge {
        fn provider(&self) -> crate::jev::JevProvider {
            crate::jev::JevProvider::TypeSafe
        }

        async fn judge(
            &self,
            _request: crate::jev::JevRequest,
        ) -> Result<crate::jev::JevResponse, crate::jev::JevError> {
            let body = json!({
                "model": "stub",
                "answers": {
                    "direction": {
                        "type": "choice",
                        "choice": "long",
                        "probabilities": {"long": 0.6, "short": 0.2, "flat": 0.2},
                        "confidence": 0.7
                    },
                    "trending": {"type": "noul", "noul": 0.6},
                    "momentum": {
                        "type": "score",
                        "score": 1.2,
                        "legend": {"0": "Weak", "1": "Neutral", "2": "Strong"},
                        "probabilities": {"0": 0.1, "1": 0.3, "2": 0.6},
                        "confidence": 0.7
                    }
                },
                "usage": {"input_tokens": 1, "output_tokens": 1}
            });
            crate::jev::contract::parse_response_body(body.to_string().as_bytes())
        }
    }

    /// Wednesday 2026-01-07 12:00 UTC: an open entry window.
    fn open_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_767_787_200)
    }

    struct Harness {
        state: AppState,
        trail: Arc<MemoryTrail>,
        engine: Arc<ScriptedEngine>,
        markets: Vec<(Symbol, CandleSeries)>,
        facts: AccountFacts,
        judgements: Vec<(Symbol, Value)>,
    }

    fn harness(answers: Vec<Value>) -> Harness {
        let trail = Arc::new(MemoryTrail::default());
        let state = AppState::new(config(), None, None, gate())
            .with_audit(Some(AuditRuntime::new(trail.clone())));
        Harness {
            state,
            trail,
            engine: ScriptedEngine::new(answers),
            markets: vec![(Symbol::parse("EURUSD").expect("symbol"), series())],
            facts: facts(),
            judgements: Vec::new(),
        }
    }

    impl Harness {
        fn session(&self, mode: AgentMode) -> AgentSession<'_> {
            AgentSession {
                state: &self.state,
                engine: self.engine.as_ref(),
                mode,
                markets: &self.markets,
                account: &self.facts,
                judgements: &self.judgements,
                tier: ModelTier::Balanced,
                now: open_time(),
            }
        }

        fn tool_events(&self) -> Vec<Value> {
            self.trail
                .events()
                .into_iter()
                .filter(|event| event.kind() == AuditKind::AgentToolCalled)
                .map(|event| event.payload().clone())
                .collect()
        }
    }

    fn tool(name: &str, arguments: Value) -> Value {
        json!({ "action": "tool", "tool": { "name": name, "arguments": arguments } })
    }

    /// Broker runtime with no validated snapshot retained yet.
    fn broker_without_snapshot() -> crate::broker::BrokerRuntime {
        use crate::broker::{BrokerRuntime, BrokerSettings};
        let settings = BrokerSettings::from_source(|name| match name {
            "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
            "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");
        BrokerRuntime::from_settings(settings).expect("runtime builds")
    }

    #[actix_web::test]
    async fn tool_then_final_runs_the_transcript_and_journals() {
        let harness = harness(vec![
            tool("get_market_window", json!({})),
            json!({ "action": "none", "rationale": "Nothing qualifies yet." }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        let outcome = run(&session, "decide", "base input")
            .await
            .expect("loop completes");

        assert!(matches!(outcome.decision, AgentDecision::Proposal(_)));
        assert_eq!(
            outcome
                .tool_calls
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["get_market_window"]
        );
        assert_eq!(outcome.rationale.as_deref(), Some("Nothing qualifies yet."));

        let events = harness.tool_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["tool"], "get_market_window");
        assert_eq!(events[0]["outcome"], "tool_call");
        assert_eq!(events[0]["result"]["entry_window_open"], true);

        let inputs = harness.engine.inputs();
        assert_eq!(inputs.len(), 2, "one step per model call");
        assert!(
            inputs[1].contains("Agent transcript"),
            "tool results feed back as input"
        );
        assert!(inputs[1].contains("get_market_window"));
    }

    #[actix_web::test]
    async fn unknown_tool_is_an_observation_not_a_failure() {
        let harness = harness(vec![
            tool("fly_to_the_moon", json!({})),
            json!({ "action": "none" }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        let outcome = run(&session, "decide", "base")
            .await
            .expect("loop completes");
        assert_eq!(outcome.tool_calls.len(), 1);
        let events = harness.tool_events();
        assert!(
            events[0]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("unknown tool")),
            "the model sees the error and can correct: {:?}",
            events[0]
        );
    }

    #[actix_web::test]
    async fn check_risk_previews_the_gate_without_consuming_it() {
        let draft = json!({
            "symbol": "GBPUSD",
            "side": "buy",
            "order_type": "market",
            "volume": 0.01
        });
        let harness = harness(vec![
            tool("check_risk", json!({ "intent": draft.clone() })),
            json!({ "action": "none" }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert_eq!(events[0]["result"]["decision"], "rejected");
        assert_eq!(events[0]["result"]["code"], "symbol_not_allowed");

        // The preview never wrote to duplicate memory: the same draft still
        // passes the real gate for an allowed symbol.
        let allowed = json!({
            "symbol": "EURUSD",
            "side": "buy",
            "order_type": "market",
            "volume": 0.01
        });
        let trade = serde_json::from_value::<TradeProposal>(json!({
            "action": "open",
            "intent": allowed
        }))
        .expect("draft parses");
        let TradeProposal::Open(draft) = trade else {
            panic!("expected open");
        };
        assert!(matches!(
            harness
                .state
                .risk()
                .evaluate(&draft, Some(facts()), open_time()),
            crate::risk::RiskDecision::Approved(_)
        ));
    }

    #[actix_web::test]
    async fn loop_limits_fail_closed() {
        let harness = harness(vec![tool("get_market_window", json!({}))]);
        let session = harness.session(AgentMode::Proposal);
        let error = run(&session, "decide", "base")
            .await
            .expect_err("step bound reached");
        assert!(
            matches!(error, PipelineError::AgentLoopLimit { ref reason } if reason.contains("step bound")),
            "unexpected error: {error:?}"
        );
    }

    #[actix_web::test]
    async fn review_mode_accepts_hold_and_rejects_proposal_actions() {
        let hold_harness = harness(vec![
            json!({ "action": "hold", "rationale": "Bracket stands." }),
        ]);
        let session = hold_harness.session(AgentMode::Review);
        let outcome = run(&session, "review", "base").await.expect("hold parses");
        assert!(matches!(
            outcome.decision,
            AgentDecision::Review(ReviewDecision::Hold)
        ));

        let second = harness(vec![json!({ "action": "none" })]);
        let session = second.session(AgentMode::Review);
        let error = run(&session, "review", "base")
            .await
            .expect_err("none is not a review");
        assert!(
            matches!(error, PipelineError::InvalidProposal { ref reason } if reason.contains("review decision")),
            "unexpected error: {error:?}"
        );
    }

    #[actix_web::test]
    async fn open_final_is_evaluated_by_the_gate() {
        let harness = harness(vec![json!({
            "action": "open",
            "rationale": "Breakout confirmed.",
            "intent": {
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "market",
                "volume": 0.01,
                "stop_loss": 1.10,
                "take_profit": 1.20
            }
        })]);
        let session = harness.session(AgentMode::Proposal);
        let outcome = run(&session, "decide", "base").await.expect("approval");
        let AgentDecision::Proposal(evaluation) = outcome.decision else {
            panic!("expected proposal");
        };
        assert!(matches!(evaluation.outcome, PipelineOutcome::Approved(_)));
    }

    #[actix_web::test]
    async fn judgements_tool_serves_cached_summaries() {
        let mut harness = harness(vec![
            tool("get_judgements", json!({ "symbol": "EURUSD" })),
            json!({ "action": "none" }),
        ]);
        harness.judgements = vec![(
            Symbol::parse("EURUSD").expect("symbol"),
            json!({ "direction": { "choice": "long", "confidence": 0.7 } }),
        )];
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert_eq!(events[0]["result"]["direction"]["choice"], "long");
    }

    #[actix_web::test]
    async fn market_tool_fetches_allowed_symbols_and_refuses_others() {
        let mut harness = harness(vec![
            tool("get_market", json!({ "symbol": "EURUSD", "bars": 10 })),
            tool("get_market", json!({ "symbol": "GBPUSD" })),
            json!({ "action": "none" }),
        ]);
        harness.state = harness
            .state
            .clone()
            .with_market(Some(MarketRuntime::from_feed(Arc::new(StubFeed))));
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert_eq!(events[0]["result"]["symbol"], "EURUSD");
        assert_eq!(events[0]["result"]["bars"], 1);
        assert!(
            events[1]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("allowlist")),
            "disallowed symbols are refused: {:?}",
            events[1]
        );
    }

    #[actix_web::test]
    async fn market_tool_validates_every_argument() {
        let harness = harness(vec![
            tool("get_market", json!({})),
            tool("get_market", json!({ "symbol": "bad symbol" })),
            tool("get_market", json!({ "symbol": "EURUSD", "bars": 0 })),
            tool("get_market", json!({ "symbol": "EURUSD", "bars": 241 })),
            tool("get_market", json!({ "symbol": "EURUSD", "bars": "ten" })),
            tool(
                "get_market",
                json!({ "symbol": "EURUSD", "timeframe": "H6" }),
            ),
            tool("get_market", json!({ "symbol": "EURUSD" })),
            json!({ "action": "none" }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let messages: Vec<String> = harness
            .tool_events()
            .iter()
            .map(|event| event["result"]["error"].as_str().unwrap_or("").to_owned())
            .collect();
        assert!(messages[0].contains("missing `symbol`"), "{messages:?}");
        assert!(messages[1].contains("invalid symbol"), "{messages:?}");
        assert!(messages[2].contains("bars"), "{messages:?}");
        assert!(messages[3].contains("bars"), "{messages:?}");
        assert!(messages[4].contains("must be an integer"), "{messages:?}");
        assert!(messages[5].contains("unknown timeframe"), "{messages:?}");
        assert!(
            messages[6].contains("no market feed"),
            "a missing feed is an observation: {messages:?}"
        );
    }

    #[actix_web::test]
    async fn judgements_tool_reports_unavailable_paths() {
        let harness = harness(vec![
            tool("get_judgements", json!({ "symbol": "GBPUSD" })),
            tool("get_judgements", json!({ "symbol": "EURUSD" })),
            json!({ "action": "none" }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert!(
            events[0]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("candidate menu")),
            "{:?}",
            events[0]
        );
        assert!(
            events[1]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("no judgement provider")),
            "{:?}",
            events[1]
        );
    }

    #[actix_web::test]
    async fn positions_tool_requires_a_validated_snapshot() {
        let mut harness = harness(vec![
            tool("get_positions", json!({})),
            json!({ "action": "none" }),
        ]);
        harness.state = AppState::new(config(), Some(broker_without_snapshot()), None, gate())
            .with_audit(Some(AuditRuntime::new(harness.trail.clone())));
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert!(
            events[0]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("snapshot")),
            "{:?}",
            events[0]
        );
    }

    #[actix_web::test]
    async fn check_risk_validates_its_argument_and_bounds_the_journal() {
        let huge_comment = "x".repeat(2_500);
        let harness = harness(vec![
            tool("check_risk", json!({})),
            tool(
                "check_risk",
                json!({ "intent": { "symbol": "EURUSD", "side": "sideways", "order_type": "market", "volume": 0.01 } }),
            ),
            tool(
                "check_risk",
                json!({ "intent": { "symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": 0.01, "comment": huge_comment } }),
            ),
            json!({ "action": "none" }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert!(
            events[0]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("missing `intent`")),
            "{:?}",
            events[0]
        );
        assert!(
            events[1]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("invalid intent")),
            "{:?}",
            events[1]
        );
        assert!(
            events[2]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("invalid intent")),
            "an over-long comment is a contract violation: {:?}",
            events[2]
        );
        assert!(
            events[2]["arguments"]["truncated"].is_string(),
            "journaled arguments are bounded"
        );
    }

    #[actix_web::test]
    async fn position_and_account_tools_report_missing_state_as_observations() {
        let harness = harness(vec![
            tool("get_positions", json!({})),
            tool("get_account", json!({})),
            json!({ "action": "none" }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert!(
            events[0]["result"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("broker")),
            "no broker is an observation: {:?}",
            events[0]
        );
        assert_eq!(events[1]["result"]["open_orders"], 0);
        assert_eq!(events[1]["result"]["equity"], 1_000.0);
    }

    #[actix_web::test]
    async fn check_risk_preview_approves_allowed_drafts() {
        let harness = harness(vec![
            tool(
                "check_risk",
                json!({ "intent": {
                    "symbol": "EURUSD",
                    "side": "buy",
                    "order_type": "market",
                    "volume": 0.01,
                    "stop_loss": 1.10,
                    "take_profit": 1.20
                } }),
            ),
            json!({ "action": "none" }),
        ]);
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert_eq!(events[0]["result"]["decision"], "approved");
    }

    #[actix_web::test]
    async fn positions_tool_reports_retained_snapshots() {
        use crate::broker::{AccountSnapshotPayload, PositionKind, PositionPayload};

        let mut harness = harness(vec![
            tool("get_positions", json!({})),
            json!({ "action": "none" }),
        ]);
        let broker = broker_without_snapshot();
        broker
            .ea_link()
            .expect("link")
            .retain_snapshot(AccountSnapshotPayload {
                balance: 1_000.0,
                equity: 1_010.0,
                free_margin: 900.0,
                orders: 1,
                lots: 0.01,
                positions: vec![PositionPayload {
                    ticket: 42,
                    symbol: "EURUSD".to_owned(),
                    magic: ORDER_MAGIC,
                    kind: PositionKind::Buy,
                    lots: 0.01,
                    price: 1.10,
                    profit: 0.5,
                    stop_loss: 1.09,
                    take_profit: 1.20,
                    opened_at: 1_700_000_000,
                    current: 1.105,
                    swap: -0.11,
                    commission: 0.0,
                }],
                positions_truncated: false,
                server_time: 1_700_000_000,
                leverage: 100,
                margin_level: 0.0,
            });
        harness.state = AppState::new(config(), Some(broker), None, gate())
            .with_audit(Some(AuditRuntime::new(harness.trail.clone())));
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert_eq!(events[0]["result"]["positions"][0]["ticket"], 42);
        assert_eq!(
            events[0]["result"]["positions"][0]["managed_by_veyra"],
            true
        );
        assert_eq!(events[0]["result"]["lots"], 0.01);
    }

    #[actix_web::test]
    async fn window_tool_reports_a_closed_rollover_window() {
        let harness = harness(vec![
            tool("get_market_window", json!({})),
            json!({ "action": "none" }),
        ]);
        let mut session = harness.session(AgentMode::Proposal);
        // Thursday 2026-01-08 21:00 UTC: rollover blackout.
        session.now = UNIX_EPOCH + Duration::from_secs(1_767_906_000);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert_eq!(events[0]["result"]["entry_window_open"], false);
        assert_eq!(events[0]["result"]["block"]["code"], "rollover_blackout");
    }

    #[actix_web::test]
    async fn cached_judgements_are_bounded_in_the_journal() {
        let mut harness = harness(vec![
            tool("get_judgements", json!({ "symbol": "EURUSD" })),
            json!({ "action": "none" }),
        ]);
        harness.judgements = vec![(
            Symbol::parse("EURUSD").expect("symbol"),
            json!({ "blob": "y".repeat(3_000) }),
        )];
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert!(
            events[0]["result"]["truncated"].is_string(),
            "oversized results are bounded in the journal: {:?}",
            events[0]
        );
    }

    #[actix_web::test]
    async fn proposal_mode_rejects_review_actions() {
        let harness = harness(vec![json!({ "action": "close", "ticket": 1 })]);
        let session = harness.session(AgentMode::Proposal);
        let error = run(&session, "decide", "base")
            .await
            .expect_err("close is not an entry answer");
        assert!(
            matches!(error, PipelineError::InvalidProposal { ref reason } if reason.contains("entry decision")),
            "unexpected error: {error:?}"
        );
    }

    #[actix_web::test]
    async fn judgements_tool_computes_when_not_cached() {
        let mut harness = harness(vec![
            tool("get_judgements", json!({ "symbol": "EURUSD" })),
            json!({ "action": "none" }),
        ]);
        harness.state = harness
            .state
            .clone()
            .with_jev(Some(crate::jev::JevRuntime::with_judge(
                crate::jev::JevProvider::TypeSafe,
                Arc::new(StubJudge),
            )));
        let session = harness.session(AgentMode::Proposal);
        run(&session, "decide", "base")
            .await
            .expect("loop completes");

        let events = harness.tool_events();
        assert_eq!(events[0]["result"]["direction"]["choice"], "long");
        assert_eq!(events[0]["result"]["trending"]["probability"], 0.6);
    }
}
