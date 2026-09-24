//! ChatGPT-subscription-first routing in front of the configured API chain.
//!
//! When the operator has connected a ChatGPT (Codex) subscription and left
//! `VEYRA_MODEL_PREFER_SUBSCRIPTION` on, every model call — autopilot
//! decisions, one-shot proposals, and the read-only assistant — tries the
//! subscription first with `VEYRA_MODEL_CHATGPT_MODEL`, then the configured
//! provider's normal chain on a failover-worthy failure. The subscription is
//! only ever sent its own model: the API tier models are a different
//! provider's identifiers and never reach it.
//!
//! A ChatGPT outage — the host unreachable, not just one refusal — also falls
//! through to the configured chain, because it is a different host. The
//! subscription leg is gated live on the connection state, so a disconnect or
//! a refresh that fails takes it out of the route immediately, before the
//! runtime is rebuilt. Only the ChatGPT subscription participates;
//! the Claude Code subscription is never part of this route.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::model::cooldown::CooldownRegistry;
use crate::model::route::{self, Call, CandidateTransport, Leg, Telemetry};
use crate::model::settings::TierModels;
use crate::model::{
    DecisionAnswer, DecisionEngine, DecisionRequest, ModelError, ModelProvider, ModelTier,
    ReadOnlyTool, ToolProgressSink,
};
use crate::subscription_auth::{SubscriptionAuthState, SubscriptionProvider};

/// Live readiness check for the preferred subscription leg.
///
/// Debug output never includes the connection state's credentials.
#[derive(Clone)]
pub(crate) struct SubscriptionGate(Arc<dyn Fn() -> bool + Send + Sync>);

impl fmt::Debug for SubscriptionGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SubscriptionGate(..)")
    }
}

impl SubscriptionGate {
    /// Ready while the ChatGPT subscription holds a usable credential.
    pub(crate) fn chatgpt(auth: SubscriptionAuthState) -> Self {
        Self(Arc::new(move || {
            auth.connected(SubscriptionProvider::Codex)
        }))
    }

    /// A gate driven by an arbitrary check; used by tests.
    #[cfg(test)]
    pub(crate) fn from_fn(check: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(check))
    }

    /// Whether the subscription leg may be attempted now.
    pub(crate) fn ready(&self) -> bool {
        (self.0)()
    }
}

/// Subscription-first composite over the configured API chain.
pub(crate) struct PreferredEngine {
    /// The configured API provider, reported as the runtime's provider.
    provider: ModelProvider,
    subscription: Arc<dyn CandidateTransport>,
    /// The subscription's single model, used for every tier.
    subscription_models: Vec<String>,
    gate: SubscriptionGate,
    api: Arc<dyn CandidateTransport>,
    api_tiers: TierModels,
    cooldowns: CooldownRegistry,
    telemetry: Telemetry,
}

impl fmt::Debug for PreferredEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreferredEngine")
            .field("provider", &self.provider)
            .field("subscription_model", &self.subscription_models)
            .finish_non_exhaustive()
    }
}

impl PreferredEngine {
    /// Composes the subscription leg (always `chatgpt_model`) in front of the
    /// API transport and its tier chains.
    pub(crate) fn new(
        subscription: Arc<dyn CandidateTransport>,
        chatgpt_model: &str,
        gate: SubscriptionGate,
        api: Arc<dyn CandidateTransport>,
        api_tiers: TierModels,
        cooldowns: CooldownRegistry,
    ) -> Self {
        Self {
            provider: api.candidate_provider(),
            subscription,
            subscription_models: vec![chatgpt_model.to_owned()],
            gate,
            api,
            api_tiers,
            cooldowns,
            telemetry: Telemetry::default(),
        }
    }

    /// The legs in force for `tier`: the subscription first while it is
    /// connected, then the configured chain.
    fn legs(&self, tier: ModelTier) -> Vec<Leg<'_>> {
        let mut legs = Vec::with_capacity(2);
        if self.gate.ready() {
            legs.push(Leg {
                transport: self.subscription.as_ref(),
                models: &self.subscription_models,
            });
        }
        legs.push(Leg {
            transport: self.api.as_ref(),
            models: self.api_tiers.chain(tier),
        });
        legs
    }
}

#[async_trait]
impl DecisionEngine for PreferredEngine {
    fn provider(&self) -> ModelProvider {
        self.provider
    }

    fn last_attempted_model(&self) -> Option<String> {
        self.telemetry.last_attempted()
    }

    fn last_successful_model(&self) -> Option<String> {
        self.telemetry.last_successful()
    }

    fn preflight(&self, tier: ModelTier) -> Result<(), ModelError> {
        route::preflight(&self.legs(tier), &self.cooldowns)
    }

    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
        let legs = self.legs(request.tier);
        route::run(
            &legs,
            &request,
            &mut Call::Structured,
            &self.cooldowns,
            &self.telemetry,
        )
        .await
    }

    async fn answer_with_tools(
        &self,
        request: DecisionRequest,
        tools: Vec<Arc<dyn ReadOnlyTool>>,
        progress: &mut dyn ToolProgressSink,
    ) -> Result<DecisionAnswer, ModelError> {
        if tools.is_empty() {
            return self.answer(request).await;
        }
        let legs = self.legs(request.tier);
        let mut call = Call::Tools {
            tools: &tools,
            progress,
        };
        route::run(&legs, &request, &mut call, &self.cooldowns, &self.telemetry).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::model::cooldown::test_clock::ManualClock;
    use crate::model::cooldown::{CooldownFailure, CooldownReason};
    use crate::model::route::AttemptError;
    use crate::model::route::test_support::{Scripted, ScriptedTransport};
    use crate::model::{AnswerFormat, BudgetPolicy, BudgetTracker, BudgetedEngine};

    struct Fixture {
        engine: PreferredEngine,
        subscription: Arc<ScriptedTransport>,
        api: Arc<ScriptedTransport>,
        connected: Arc<AtomicBool>,
        cooldowns: CooldownRegistry,
        clock: ManualClock,
    }

    fn fixture() -> Fixture {
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let subscription = Arc::new(ScriptedTransport::new(
            ModelProvider::Codex,
            Some("chatgpt"),
        ));
        let api = Arc::new(ScriptedTransport::new(ModelProvider::OpenRouter, None));
        let connected = Arc::new(AtomicBool::new(true));
        let gate = {
            let connected = connected.clone();
            SubscriptionGate::from_fn(move || connected.load(Ordering::SeqCst))
        };
        let tiers = TierModels::new("deepseek/fast", "deepseek/balanced", "deepseek/reasoning")
            .with_fallbacks(ModelTier::Balanced, vec!["z-ai/glm".to_owned()]);
        let engine = PreferredEngine::new(
            subscription.clone(),
            "gpt-6-luna",
            gate,
            api.clone(),
            tiers,
            cooldowns.clone(),
        );
        Fixture {
            engine,
            subscription,
            api,
            connected,
            cooldowns,
            clock,
        }
    }

    fn request(tier: ModelTier) -> DecisionRequest {
        DecisionRequest {
            instructions: "Decide.".to_owned(),
            input: "{}".to_owned(),
            format: AnswerFormat {
                name: "decision".to_owned(),
                schema: json!({"type": "object"}),
            },
            tier,
        }
    }

    fn fail(reason: CooldownReason, text: &str) -> Scripted {
        Scripted::Fail(AttemptError::cooldown(text, CooldownFailure::new(reason)))
    }

    #[derive(Default)]
    struct Sink(usize);

    #[async_trait]
    impl ToolProgressSink for Sink {
        async fn tool_started(&mut self, _id: &str, _name: &str, _arguments: &Value) {
            self.0 += 1;
        }
        async fn tool_completed(&mut self, _: &str, _: &str, _: &Value, _: bool) {}
    }

    #[actix_web::test]
    async fn the_subscription_answers_first_for_every_tier_with_its_own_model() {
        let f = fixture();
        assert!(format!("{:?}", f.engine).contains("gpt-6-luna"));
        for tier in [ModelTier::Fast, ModelTier::Balanced, ModelTier::Reasoning] {
            let answer = f.engine.answer(request(tier)).await.expect("answers");
            assert_eq!(answer.value["model"], "gpt-6-luna");
        }
        assert_eq!(f.subscription.asked(), ["gpt-6-luna"; 3]);
        assert!(f.api.asked().is_empty(), "the API chain is not touched");
        assert_eq!(f.engine.provider(), ModelProvider::OpenRouter);
        assert_eq!(
            f.engine.last_attempted_model().as_deref(),
            Some("chatgpt:gpt-6-luna")
        );
        assert_eq!(
            f.engine.last_successful_model().as_deref(),
            Some("chatgpt:gpt-6-luna")
        );
    }

    #[actix_web::test]
    async fn a_failover_worthy_subscription_failure_runs_the_configured_chain() {
        let f = fixture();
        f.subscription
            .push(fail(CooldownReason::RateLimited, "rate_limited"));
        f.api.push(fail(
            CooldownReason::InsufficientCredits,
            "insufficient_credits",
        ));
        let answer = f
            .engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("the API fallback answers");
        assert_eq!(answer.value["model"], "z-ai/glm");
        assert_eq!(f.subscription.asked(), ["gpt-6-luna"]);
        assert_eq!(f.api.asked(), ["deepseek/balanced", "z-ai/glm"]);
        assert_eq!(
            f.engine.last_successful_model().as_deref(),
            Some("z-ai/glm")
        );
        let cooled: Vec<String> = f
            .cooldowns
            .snapshot()
            .into_iter()
            .map(|entry| format!("{}/{}", entry.provider, entry.model))
            .collect();
        assert_eq!(
            cooled,
            ["codex/gpt-6-luna", "openrouter/deepseek/balanced"],
            "both failed candidates cool down"
        );

        // The next call skips both cooled candidates without a request.
        f.engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("answers");
        assert_eq!(f.subscription.asked(), ["gpt-6-luna"]);
        assert_eq!(f.api.asked(), ["deepseek/balanced", "z-ai/glm", "z-ai/glm"]);

        // The subscription's rate-limit window passes first; it is probed and
        // recovers.
        f.clock.advance(Duration::from_secs(120));
        let answer = f
            .engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("answers");
        assert_eq!(answer.value["model"], "gpt-6-luna");
        assert_eq!(
            f.cooldowns.len(),
            1,
            "only the API primary is still cooling"
        );
    }

    #[actix_web::test]
    async fn an_unreachable_subscription_falls_through_to_the_configured_chain() {
        let f = fixture();
        f.subscription
            .push(Scripted::Fail(AttemptError::transport("transport")));
        let answer = f
            .engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("the configured provider takes over");
        assert_eq!(answer.value["model"], "deepseek/fast");
        assert_eq!(f.subscription.asked(), ["gpt-6-luna"]);
        let entry = &f.cooldowns.snapshot()[0];
        assert_eq!(
            (entry.provider.as_str(), entry.reason.as_str()),
            ("codex", "unreachable")
        );

        // Held for a minute, then probed once; answering releases the hold.
        f.engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("answers");
        assert_eq!(f.subscription.asked(), ["gpt-6-luna"], "held: no request");
        f.clock.advance(Duration::from_secs(60));
        let answer = f
            .engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("answers");
        assert_eq!(answer.value["model"], "gpt-6-luna");
        assert!(f.cooldowns.is_empty());
    }

    #[actix_web::test]
    async fn a_disconnected_subscription_drops_out_of_the_route_live() {
        let f = fixture();
        f.connected.store(false, Ordering::SeqCst);
        let answer = f
            .engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("answers");
        assert_eq!(answer.value["model"], "deepseek/fast");
        assert!(
            f.subscription.asked().is_empty(),
            "no request to a disconnected subscription"
        );
        f.engine
            .preflight(ModelTier::Fast)
            .expect("the API chain is open");
    }

    #[actix_web::test]
    async fn tool_sessions_prefer_the_subscription_and_fail_over_too() {
        let f = fixture();
        let mut sink = Sink::default();
        f.engine
            .answer_with_tools(request(ModelTier::Fast), Vec::new(), &mut sink)
            .await
            .expect("an empty tool list is a structured answer");
        assert_eq!(*f.subscription.tool_calls.lock().expect("lock"), 0);

        struct Observe;
        #[async_trait]
        impl ReadOnlyTool for Observe {
            fn definition(&self) -> crate::model::ReadOnlyToolDefinition {
                crate::model::ReadOnlyToolDefinition {
                    name: "observe".to_owned(),
                    description: "Observe.".to_owned(),
                    input_schema: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _arguments: Value) -> Result<Value, String> {
                Ok(json!({}))
            }
        }
        f.subscription
            .push(fail(CooldownReason::InvalidResponse, "invalid_response"));
        let answer = f
            .engine
            .answer_with_tools(request(ModelTier::Fast), vec![Arc::new(Observe)], &mut sink)
            .await
            .expect("the API chain serves the session");
        assert_eq!(answer.value["model"], "deepseek/fast");
        assert_eq!(*f.subscription.tool_calls.lock().expect("lock"), 1);
        assert_eq!(*f.api.tool_calls.lock().expect("lock"), 1);
        assert_eq!(sink.0, 2, "each attempted leg reported its tool progress");
    }

    #[actix_web::test]
    async fn all_cooled_fails_before_the_budget_or_any_request() {
        let f = fixture();
        let fixture_subscription = f.subscription.clone();
        let fixture_api = f.api.clone();
        let cooldowns = f.cooldowns.clone();
        fixture_subscription.push(fail(
            CooldownReason::InsufficientCredits,
            "insufficient_credits",
        ));
        fixture_api.push(fail(
            CooldownReason::ProviderRejected,
            "provider_rejected (404)",
        ));
        let tracker = Arc::new(BudgetTracker::new(BudgetPolicy::new(100, 0)));
        let engine = BudgetedEngine::new(Arc::new(f.engine), tracker.clone());
        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("both candidates fail");
        assert_eq!(tracker.snapshot().hour_calls, 1);
        assert_eq!(cooldowns.len(), 2);

        let error = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("everything is cooling");
        assert!(
            error.to_string().contains(
                "every model candidate is cooling down; soonest retry at 2026-01-01T00:30:00Z"
            ),
            "{error}"
        );
        let mut sink = Sink::default();
        engine
            .answer_with_tools(request(ModelTier::Fast), Vec::new(), &mut sink)
            .await
            .expect_err("tool sessions are refused the same way");
        assert_eq!(tracker.snapshot().hour_calls, 1, "no budget was consumed");
        assert_eq!(fixture_subscription.asked().len(), 1, "no request was sent");
        assert_eq!(fixture_api.asked().len(), 1, "no request was sent");
        assert!(engine.preflight(ModelTier::Fast).is_err());
    }
}
