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
                        "price": {"type": ["number", "null"]},
                        "volume": {"type": "number", "exclusiveMinimum": 0},
                        "stop_loss": {"type": ["number", "null"]},
                        "take_profit": {"type": ["number", "null"]},
                        "comment": {"type": ["string", "null"]}
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
) -> Result<PipelineOutcome, PipelineError> {
    let answer = engine
        .answer(DecisionRequest {
            instructions,
            input,
            format: proposal_format(),
            tier,
        })
        .await?;

    let proposal: TradeProposal = serde_json::from_value(normalize_proposal(answer.value))
        .map_err(|error| PipelineError::InvalidProposal {
            reason: error.to_string(),
        })?;

    match proposal {
        TradeProposal::None => Ok(PipelineOutcome::NoTrade),
        TradeProposal::Open(draft) => match gate.evaluate(&draft, account, now) {
            RiskDecision::Approved(intent) => Ok(PipelineOutcome::Approved(intent)),
            RiskDecision::Rejected(rejection) => Ok(PipelineOutcome::Rejected { draft, rejection }),
        },
    }
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
        })
    }

    fn open_answer() -> Value {
        json!({
            "action": "open",
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
    ) -> Result<PipelineOutcome, PipelineError> {
        evaluate_proposal(
            engine,
            gate,
            "Decide whether to trade.".to_owned(),
            "EURUSD closed above its average.".to_owned(),
            ModelTier::Balanced,
            account,
            SystemTime::now(),
        )
        .await
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

    #[actix_web::test]
    async fn approved_proposals_carry_identity_and_the_format_contract() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(open_answer());
        let outcome = run(&engine, &gate, facts()).await.expect("pipeline runs");

        let PipelineOutcome::Approved(intent) = outcome else {
            panic!("expected approval, got {outcome:?}");
        };
        assert_eq!(intent.draft().symbol().as_str(), "EURUSD");

        let request = engine.last_request();
        assert_eq!(request.format.name, PROPOSAL_SCHEMA_NAME);
        assert_eq!(request.tier, ModelTier::Balanced);
        assert_eq!(request.format.schema["required"], json!(["action"]));
    }

    #[actix_web::test]
    async fn declined_proposals_never_reach_the_gate() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(json!({"action": "none"}));
        let outcome = run(&engine, &gate, facts()).await.expect("pipeline runs");

        assert!(matches!(outcome, PipelineOutcome::NoTrade));
    }

    #[actix_web::test]
    async fn rejections_are_normal_outcomes_that_keep_the_draft() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(open_answer());
        let outcome = run(&engine, &gate, None).await.expect("pipeline runs");

        let PipelineOutcome::Rejected { draft, rejection } = outcome else {
            panic!("expected rejection, got {outcome:?}");
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
        let outcome = run(&engine, &gate, facts()).await.expect("pipeline runs");

        let PipelineOutcome::Rejected { rejection, .. } = outcome else {
            panic!("expected rejection, got {outcome:?}");
        };
        assert_eq!(rejection.code(), RiskCode::SymbolNotAllowed);
    }

    #[actix_web::test]
    async fn duplicate_proposals_are_suppressed() {
        let gate = RiskGate::new(policy());
        let engine = StubEngine::replying(open_answer());

        assert!(matches!(
            run(&engine, &gate, facts()).await.expect("first"),
            PipelineOutcome::Approved(_)
        ));
        let second = run(&engine, &gate, facts()).await.expect("second");
        let PipelineOutcome::Rejected { rejection, .. } = second else {
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
