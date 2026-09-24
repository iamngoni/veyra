//! Model-to-gate proposal pipeline.
//!
//! Turns one structured model answer into either "no trade", a risk
//! rejection, or an approved intent. It never queues, transmits, or executes
//! anything; the autopilot and control surface hand approvals to the staged
//! execution path. Two narrow transport tolerances exist, both limited to
//! model phrasing that has no execution meaning: a reference `price` echoed on
//! a market order (a market order executes at market), and a `comment` the
//! model embellished past MT4's 31-character limit (the EA order request
//! carries no comment and the audit never stores it). Every field that affects
//! execution still fails strict parsing, and the gate re-validates every
//! draft.

use std::time::SystemTime;

use serde_json::json;

use crate::model::{AnswerFormat, DecisionEngine, DecisionRequest, ModelError, ModelTier};
use crate::risk::{AccountFacts, RiskDecision, RiskGate, RiskRejection};
use crate::trading::intent::{Comment, TradeIntent, TradeIntentDraft, TradeProposal};

/// Name of the forced function/schema the model answers with.
pub const PROPOSAL_SCHEMA_NAME: &str = "veyra_trade_proposal";

/// JSON Schema restricting an answer to "no trade" or one valid draft.
///
/// The schema is the transport-level guard; [`TradeProposal`] re-validates
/// every field, so a provider that ignores the schema cannot smuggle an
/// invalid intent into the gate.
pub fn proposal_format() -> AnswerFormat {
    AnswerFormat {
        name: PROPOSAL_SCHEMA_NAME.to_owned(),
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action"],
            "properties": {
                "action": {"type": "string", "enum": ["none", "open"]},
                "rationale": {
                    "type": "string",
                    "maxLength": 280,
                    "description": "Short operator-facing explanation of the decision: why this instrument and direction, or why nothing qualifies. Always include it."
                },
                "intent": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["symbol", "side", "order_type", "volume"],
                    "properties": {
                        "symbol": {"type": "string"},
                        "side": {"type": "string", "enum": ["buy", "sell"]},
                        "order_type": {
                            "type": "string",
                            "enum": ["market", "limit", "stop"],
                            "description": "market executes immediately; omit `price` entirely for market orders"
                        },
                        "price": { "type": "number", "description": "entry price for limit and stop orders; omit for market orders" },
                        "volume": {"type": "number", "exclusiveMinimum": 0},
                        "stop_loss": { "type": "number", "description": "absolute stop-loss price" },
                        "take_profit": { "type": "number", "description": "absolute take-profit price" },
                        "comment": { "type": "string", "description": "optional short note; omit when there is nothing to add" }
                    }
                }
            }
        }),
    }
}

/// Failures raised before the gate can decide.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The provider call failed or returned an unusable payload.
    #[error("model proposal failed: {0}")]
    Model(#[from] ModelError),
    /// The answer violated the intent contract after schema validation.
    #[error("model answer is not a valid trade proposal: {reason}")]
    InvalidProposal {
        /// Non-sensitive parser explanation.
        reason: String,
    },
    /// The decision loop hit a step or tool-call bound.
    #[error("agent decision loop limit: {reason}")]
    AgentLoopLimit {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Longest rationale kept in the journal; longer answers are truncated.
pub const RATIONALE_MAX_CHARS: usize = 280;

/// Sanitises a model rationale: trims, drops control characters, and bounds
/// the length. Missing, empty, or unusable text becomes `None`.
pub fn parse_rationale(value: &serde_json::Value) -> Option<String> {
    let text = value.get("rationale")?.as_str()?.trim();
    let cleaned: String = text
        .chars()
        .filter(|character| !character.is_control())
        .take(RATIONALE_MAX_CHARS)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_owned())
    }
}

/// One evaluation result plus the model's operator-facing rationale.
#[derive(Debug)]
pub struct ProposalEvaluation {
    /// Parsed outcome the caller acts on.
    pub outcome: PipelineOutcome,
    /// Short model-written explanation of the decision, when provided.
    pub rationale: Option<String>,
}

/// Outcome of one proposal evaluation.
#[derive(Debug)]
pub enum PipelineOutcome {
    /// The model declined to propose a trade.
    NoTrade,
    /// The gate rejected the proposal; nothing was queued.
    Rejected {
        /// Draft the gate evaluated.
        draft: TradeIntentDraft,
        /// Deterministic rejection.
        rejection: RiskRejection,
    },
    /// The gate approved the proposal; ready for the future command layer.
    Approved(TradeIntent),
}

/// Runs one structured proposal through the deterministic gate.
///
/// # Errors
/// Returns [`PipelineError`] when the provider fails or the answer is not a
/// valid proposal. A rejection is a normal outcome, not an error.
pub async fn evaluate_proposal(
    engine: &dyn DecisionEngine,
    gate: &RiskGate,
    instructions: String,
    input: String,
    tier: ModelTier,
    account: Option<AccountFacts>,
    now: SystemTime,
) -> Result<ProposalEvaluation, PipelineError> {
    let answer = engine
        .answer(DecisionRequest {
            instructions,
            input,
            format: proposal_format(),
            tier,
        })
        .await?;

    evaluate_answer(answer.value, gate, account, now)
}

/// Evaluates one already-structured model answer: the pure half of
/// [`evaluate_proposal`], shared with the agent decision loop.
///
/// # Errors
/// Returns [`PipelineError::InvalidProposal`] when the answer violates the
/// intent contract.
pub fn evaluate_answer(
    value: serde_json::Value,
    gate: &RiskGate,
    account: Option<AccountFacts>,
    now: SystemTime,
) -> Result<ProposalEvaluation, PipelineError> {
    let mut normalized = normalize_proposal(value);
    let rationale = parse_rationale(&normalized);
    // Rationale and loop-only keys are journal metadata, not part of the
    // intent contract; `TradeProposal` stays strict, so strip them first.
    if let Some(object) = normalized.as_object_mut() {
        object.remove("rationale");
        object.remove("tool");
    }
    let proposal: TradeProposal =
        serde_json::from_value(normalized).map_err(|error| PipelineError::InvalidProposal {
            reason: error.to_string(),
        })?;

    let outcome = match proposal {
        TradeProposal::None => PipelineOutcome::NoTrade,
        TradeProposal::Open(draft) => match gate.evaluate(&draft, account, now) {
            RiskDecision::Approved(intent) => PipelineOutcome::Approved(intent),
            RiskDecision::Rejected(rejection) => PipelineOutcome::Rejected { draft, rejection },
        },
    };
    Ok(ProposalEvaluation { outcome, rationale })
}

/// Drops a reference `price` echoed on a market-order proposal.
///
/// The draft contract keeps `price` absent for market orders so a
/// contradictory request can never be smuggled through; models occasionally
/// echo the current price anyway, and a market order ignores it by
/// definition. Only this exact case is tolerated: everything else still fails
/// strict parsing, and the gate re-validates every field.
fn normalize_proposal(value: serde_json::Value) -> serde_json::Value {
    let mut value = value;
    // A declined proposal that still carries a draft is a decline: the stray
    // intent is dropped, which can only ever produce `NoTrade`.
    if value.get("action").and_then(serde_json::Value::as_str) == Some("none") {
        if let Some(object) = value.as_object_mut() {
            object.remove("intent");
        }
        return value;
    }
    let is_market = value
        .get("intent")
        .and_then(|intent| intent.get("order_type"))
        .and_then(serde_json::Value::as_str)
        == Some("market");
    let has_price = value
        .get("intent")
        .and_then(|intent| intent.get("price"))
        .is_some_and(|price| !price.is_null());
    let stale_comment = value
        .get("intent")
        .and_then(|intent| intent.get("comment"))
        .is_some_and(|comment| !comment.is_null() && !comment_is_usable(comment));
    if ((is_market && has_price) || stale_comment)
        && let Some(intent) = value
            .get_mut("intent")
            .and_then(serde_json::Value::as_object_mut)
    {
        if is_market && has_price {
            intent.remove("price");
        }
        if stale_comment {
            intent.remove("comment");
        }
    }
    value
}

/// Whether a proposed comment fits the draft contract's MT4 limit.
fn comment_is_usable(comment: &serde_json::Value) -> bool {
    comment
        .as_str()
        .is_some_and(|text| Comment::parse(text).is_ok())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::model::{DecisionAnswer, ModelProvider};
    use crate::risk::{RiskCode, RiskPolicy};
    use crate::trading::intent::{Volume, parse_instrument};

    /// Deterministic engine stub: records the request it was asked and returns
    /// a canned answer. It exercises the pipeline without any network call.
    #[derive(Debug)]
    struct StubEngine {
        reply: Result<Value, &'static str>,
        requests: Mutex<Vec<DecisionRequest>>,
    }

    impl StubEngine {
        fn replying(reply: Value) -> Self {
            Self {
                reply: Ok(reply),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn failing(reason: &'static str) -> Self {
            Self {
                reply: Err(reason),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn last_request(&self) -> DecisionRequest {
            self.requests
                .lock()
                .expect("request lock")
                .last()
                .cloned()
                .expect("a request must have been made")
        }
    }

    #[async_trait]
    impl DecisionEngine for StubEngine {
        fn provider(&self) -> ModelProvider {
            ModelProvider::OpenRouter
        }

        async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
            self.requests.lock().expect("request lock").push(request);
            match &self.reply {
                Ok(value) => Ok(DecisionAnswer {
                    value: value.clone(),
                }),
                Err(reason) => Err(ModelError::Request {
                    reason: (*reason).to_owned(),
                }),
            }
        }
    }

    fn policy() -> RiskPolicy {
        RiskPolicy::new(
            false,
            vec![parse_instrument("eurusd").expect("symbol")],
            Volume::parse(0.5).expect("volume"),
            Volume::parse(0.5).expect("volume"),
            2,
            Duration::from_secs(60),
            None,
        )
    }

    fn facts() -> Option<AccountFacts> {
        Some(AccountFacts {
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
        })
    }

    fn open_answer() -> Value {
        json!({
            "action": "open",
            "rationale": "Momentum favours the upside.",
            "intent": {
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "market",
                "volume": 0.01
            }
        })
    }

    async fn run(
        engine: &StubEngine,
        gate: &RiskGate,
        account: Option<AccountFacts>,
    ) -> Result<ProposalEvaluation, PipelineError> {
        evaluate_proposal(
            engine,
            gate,
            "Decide whether to trade.".to_owned(),
            "EURUSD closed above its average.".to_owned(),
            ModelTier::Balanced,
            account,
            test_now(),
        )
        .await
    }

    /// Wednesday 2026-01-07 12:00 UTC: the gate's entry window is open, so the
    /// pipeline tests never depend on the day the suite runs.
    fn test_now() -> SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_767_787_200)
    }

    #[test]
    fn market_prices_are_dropped_but_other_violations_survive() {
        let normalization = |intent: Value| {
            serde_json::from_value::<TradeProposal>(normalize_proposal(
                json!({"action": "open", "intent": intent}),
            ))
        };

        // A market order with an echoed reference price parses after cleanup.
        let proposal = normalization(json!({
            "symbol": "EURUSD",
            "side": "buy",
            "order_type": "market",
            "price": 1.147,
            "volume": 0.01,
            "stop_loss": 1.140,
            "take_profit": 1.160
        }))
        .expect("market price is tolerated");
        match proposal {
            TradeProposal::Open(draft) => {
                assert_eq!(draft.order().as_str(), "market");
                assert!(draft.order().price().is_none());
            }
            other => panic!("unexpected proposal: {other:?}"),
        }

        // A null price was already fine and stays fine.
        assert!(
            normalization(json!({
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "market",
                "price": null,
                "volume": 0.01
            }))
            .is_ok()
        );

        // A non-market order with no price must still fail.
        assert!(
            normalization(json!({
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "limit",
                "volume": 0.01
            }))
            .is_err()
        );

        // Everything else is untouched by the tolerance.
        assert!(
            normalization(json!({
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "market",
                "price": 1.147,
                "volume": 0.01,
                "unknown": true
            }))
            .is_err()
        );

        // A decline that carries a stray intent is still a decline.
        let proposal = serde_json::from_value::<TradeProposal>(normalize_proposal(json!({
            "action": "none",
            "intent": {
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "market",
                "volume": 0.01
            }
        })))
        .expect("stray intents on declines are dropped");
        assert!(matches!(proposal, TradeProposal::None));

        // An over-long or non-ASCII comment is dropped, not fatal.
        let proposal = normalization(json!({
            "symbol": "EURUSD",
            "side": "buy",
            "order_type": "market",
            "volume": 0.01,
            "comment": "a very long commentary that exceeds the MT4 limit by far"
        }))
        .expect("embellished comments are dropped");
        match proposal {
            TradeProposal::Open(draft) => assert!(draft.comment().is_none()),
            other => panic!("unexpected proposal: {other:?}"),
        }

        // A comment that fits the contract is preserved.
        let proposal = normalization(json!({
            "symbol": "EURUSD",
            "side": "buy",
            "order_type": "market",
            "volume": 0.01,
            "comment": "trend follow"
        }))
        .expect("short comments survive");
        match proposal {
            TradeProposal::Open(draft) => {
                assert_eq!(
                    draft.comment().map(|comment| comment.as_str()),
                    Some("trend follow")
                );
            }
            other => panic!("unexpected proposal: {other:?}"),
        }
    }

    #[test]
    fn rationales_are_sanitised_and_bounded() {
        assert_eq!(
            parse_rationale(&json!({"rationale": "  spaced  "})).as_deref(),
            Some("spaced")
        );
        assert_eq!(
            parse_rationale(&json!({"rationale": "ab\u{0007}cd"})).as_deref(),
            Some("abcd"),
            "control characters never reach the journal"
        );
        let long = "x".repeat(400);
        assert_eq!(
            parse_rationale(&json!({ "rationale": long }))
                .expect("long rationale")
                .chars()
                .count(),
            280,
            "rationales are truncated at the schema bound"
        );
        assert!(parse_rationale(&json!({})).is_none());
        assert!(parse_rationale(&json!({"rationale": "   "})).is_none());
        assert!(parse_rationale(&json!({"rationale": 7})).is_none());
    }

    #[actix_web::test]
    async fn approved_proposals_carry_identity_and_the_format_contract() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(open_answer());
        let evaluation = run(&engine, &gate, facts()).await.expect("pipeline runs");

        let PipelineOutcome::Approved(intent) = evaluation.outcome else {
            panic!("expected approval, got {evaluation:?}");
        };
        assert_eq!(intent.draft().symbol().as_str(), "EURUSD");
        assert_eq!(
            evaluation.rationale.as_deref(),
            Some("Momentum favours the upside."),
            "the model's rationale travels with the evaluation"
        );

        let request = engine.last_request();
        assert_eq!(request.format.name, PROPOSAL_SCHEMA_NAME);
        assert_eq!(request.tier, ModelTier::Balanced);
        assert_eq!(request.format.schema["required"], json!(["action"]));
        assert_eq!(
            request.format.schema["properties"]["rationale"]["maxLength"],
            280
        );
    }

    #[actix_web::test]
    async fn declined_proposals_never_reach_the_gate() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(json!({
            "action": "none",
            "rationale": "No instrument shows a clear edge."
        }));
        let evaluation = run(&engine, &gate, facts()).await.expect("pipeline runs");

        assert!(matches!(evaluation.outcome, PipelineOutcome::NoTrade));
        assert_eq!(
            evaluation.rationale.as_deref(),
            Some("No instrument shows a clear edge."),
            "a skip also carries its reason"
        );
    }

    #[actix_web::test]
    async fn rejections_are_normal_outcomes_that_keep_the_draft() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(open_answer());
        let evaluation = run(&engine, &gate, None).await.expect("pipeline runs");

        let PipelineOutcome::Rejected { draft, rejection } = evaluation.outcome else {
            panic!("expected rejection, got {evaluation:?}");
        };
        assert_eq!(draft.symbol().as_str(), "EURUSD");
        assert_eq!(rejection.code(), RiskCode::AccountStateUnavailable);
    }

    #[actix_web::test]
    async fn hallucinated_instruments_are_rejected_by_the_allowlist() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(json!({
            "action": "open",
            "intent": {
                "symbol": "NOTREAL",
                "side": "buy",
                "order_type": "market",
                "volume": 0.01
            }
        }));
        let evaluation = run(&engine, &gate, facts()).await.expect("pipeline runs");

        let PipelineOutcome::Rejected { rejection, .. } = evaluation.outcome else {
            panic!("expected rejection, got {evaluation:?}");
        };
        assert_eq!(rejection.code(), RiskCode::SymbolNotAllowed);
    }

    #[actix_web::test]
    async fn duplicate_proposals_are_suppressed() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(open_answer());

        assert!(matches!(
            run(&engine, &gate, facts()).await.expect("first").outcome,
            PipelineOutcome::Approved(_)
        ));
        let second = run(&engine, &gate, facts()).await.expect("second");
        let PipelineOutcome::Rejected { rejection, .. } = second.outcome else {
            panic!("expected duplicate rejection, got {second:?}");
        };
        assert_eq!(rejection.code(), RiskCode::DuplicateIntent);
    }

    #[actix_web::test]
    async fn invalid_and_failed_answers_are_errors() {
        let gate = RiskGate::new(policy());

        let invalid = StubEngine::replying(json!({
            "action": "open",
            "intent": {
                "symbol": "EURUSD",
                "side": "buy",
                "order_type": "market",
                "volume": -1.0
            }
        }));
        let error = run(&invalid, &gate, facts()).await.expect_err("invalid");
        assert!(matches!(error, PipelineError::InvalidProposal { .. }));

        let incomplete = StubEngine::replying(json!({"action": "open"}));
        let error = run(&incomplete, &gate, facts())
            .await
            .expect_err("incomplete");
        assert!(matches!(error, PipelineError::InvalidProposal { .. }));

        let failing = StubEngine::failing("provider exploded");
        let error = run(&failing, &gate, facts()).await.expect_err("failure");
        assert!(matches!(error, PipelineError::Model(_)));
    }
}
