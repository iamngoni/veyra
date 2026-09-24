//! Ordered, cooldown-aware routing across one or more model transports.
//!
//! A route is a list of legs; each leg is one transport — one host: an API
//! provider or the ChatGPT subscription — plus the model candidates it serves
//! for the requested tier. Candidates are tried strictly in order. A candidate
//! that is cooling down is skipped without a request. A failure another model
//! could avoid cools the candidate and moves on to the next one.
//!
//! A transport fault is a property of the host, not the model: the rest of
//! that host's candidates would fail the same way, and the runtime's retry
//! policy has already retried it. So the fault holds every remaining candidate
//! on that host under the short `unreachable` schedule and the route moves on
//! to the next host, which is what makes a ChatGPT outage fall through to the
//! configured provider. When every candidate is cooling, the route fails
//! before any request with the soonest retry time.
//!
//! This is the single place that decides failover, so the API chain, the
//! subscription chain, and the subscription-first composite behave the same.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::model::cooldown::{
    Admission, CooldownFailure, CooldownReason, CooldownRegistry, Cooling, format_utc,
};
use crate::model::{
    DecisionAnswer, DecisionRequest, ModelError, ModelProvider, ReadOnlyTool, ToolProgressSink,
};

/// How a failed attempt affects the rest of the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    /// The host was not reached. Every remaining candidate on it is held as
    /// `unreachable` and the route moves on to the next host.
    Transport,
    /// A property of the candidate: cool it down and try the next one.
    Cooldown(CooldownFailure),
}

/// One failed attempt: a bounded, non-sensitive category plus its effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptError {
    /// Safe category for operators, e.g. `insufficient_credits` or
    /// `provider_rejected (404)`. Never a provider body.
    pub(crate) reason: String,
    /// What the route does next.
    pub(crate) class: FailureClass,
}

impl AttemptError {
    /// A failure that cools the candidate down.
    pub(crate) fn cooldown(reason: impl Into<String>, failure: CooldownFailure) -> Self {
        Self {
            reason: reason.into(),
            class: FailureClass::Cooldown(failure),
        }
    }

    /// A transport fault: the candidate's host could not be reached.
    pub(crate) fn transport(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            class: FailureClass::Transport,
        }
    }
}

/// What one attempt is asked to do.
pub(crate) enum Call<'a> {
    /// One structured answer constrained to the request schema.
    Structured,
    /// A model-directed session over allowlisted read-only tools.
    Tools {
        /// The observation tools the model may call.
        tools: &'a [Arc<dyn ReadOnlyTool>],
        /// Receives tool lifecycle events.
        progress: &'a mut dyn ToolProgressSink,
    },
}

/// One transport able to attempt one explicit model candidate.
#[async_trait]
pub(crate) trait CandidateTransport: Send + Sync {
    /// Provider that serves this transport's candidates; the cooldown key.
    fn candidate_provider(&self) -> ModelProvider;

    /// Operator-facing name of a candidate, e.g. `chatgpt:gpt-6-luna`.
    fn label(&self, model: &str) -> String {
        model.to_owned()
    }

    /// Makes exactly one attempt against `model`.
    async fn attempt(
        &self,
        model: &str,
        request: &DecisionRequest,
        call: &mut Call<'_>,
    ) -> Result<DecisionAnswer, AttemptError>;
}

/// One transport and the candidates it serves for the requested tier.
pub(crate) struct Leg<'a> {
    /// The transport attempting these candidates.
    pub(crate) transport: &'a dyn CandidateTransport,
    /// Ordered candidates for the tier.
    pub(crate) models: &'a [String],
}

/// Last attempted and last successful candidate, for status output.
#[derive(Debug, Default)]
pub(crate) struct Telemetry {
    attempted: Mutex<Option<String>>,
    successful: Mutex<Option<String>>,
}

impl Telemetry {
    fn store(slot: &Mutex<Option<String>>, label: &str) {
        match slot.lock() {
            Ok(mut value) => *value = Some(label.to_owned()),
            Err(poisoned) => *poisoned.into_inner() = Some(label.to_owned()),
        }
    }

    fn load(slot: &Mutex<Option<String>>) -> Option<String> {
        match slot.lock() {
            Ok(value) => value.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Records a candidate that is about to be requested.
    pub(crate) fn attempted(&self, label: &str) {
        Self::store(&self.attempted, label);
    }

    /// Records a candidate that returned an answer.
    pub(crate) fn succeeded(&self, label: &str) {
        Self::store(&self.successful, label);
    }

    /// The most recent candidate requested, including one that failed.
    pub(crate) fn last_attempted(&self) -> Option<String> {
        Self::load(&self.attempted)
    }

    /// The most recent candidate that answered.
    pub(crate) fn last_successful(&self) -> Option<String> {
        Self::load(&self.successful)
    }
}

/// The request-free refusal when every candidate is cooling down.
fn all_cooling(held: &[(String, Cooling)]) -> ModelError {
    let soonest = held
        .iter()
        .map(|(_, cooling)| cooling.until)
        .min()
        .unwrap_or_else(SystemTime::now);
    let details = held
        .iter()
        .map(|(label, cooling)| format!("{label}: {}", cooling.reason))
        .collect::<Vec<_>>()
        .join(", ");
    ModelError::Request {
        reason: format!(
            "every model candidate is cooling down; soonest retry at {} ({details})",
            format_utc(soonest)
        ),
    }
}

/// Refuses up front when every candidate on the route is cooling down.
///
/// Read-only: it never claims a half-open probe, so the call that follows can.
///
/// # Errors
/// Returns [`ModelError::Request`] naming the soonest retry time.
pub(crate) fn preflight(legs: &[Leg<'_>], cooldowns: &CooldownRegistry) -> Result<(), ModelError> {
    let mut held = Vec::new();
    for leg in legs {
        for model in leg.models {
            match cooldowns.peek(leg.transport.candidate_provider(), model) {
                Some(cooling) => held.push((leg.transport.label(model), cooling)),
                None => return Ok(()),
            }
        }
    }
    if held.is_empty() {
        // An empty route has nothing to refuse on cooldown grounds; the run
        // itself reports that nothing could be attempted.
        return Ok(());
    }
    Err(all_cooling(&held))
}

/// Runs one request along the route.
///
/// # Errors
/// Returns [`ModelError::Request`] listing each attempted candidate and why it
/// failed, or — when nothing could be attempted — the soonest retry time.
pub(crate) async fn run(
    legs: &[Leg<'_>],
    request: &DecisionRequest,
    call: &mut Call<'_>,
    cooldowns: &CooldownRegistry,
    telemetry: &Telemetry,
) -> Result<DecisionAnswer, ModelError> {
    let total: usize = legs.iter().map(|leg| leg.models.len()).sum();
    let mut failures: Vec<String> = Vec::new();
    let mut held: Vec<(String, Cooling)> = Vec::new();
    let mut host_skipped: Vec<String> = Vec::new();
    let mut position = 0_usize;

    'route: for leg in legs {
        let provider = leg.transport.candidate_provider();
        for (index, model) in leg.models.iter().enumerate() {
            position += 1;
            let label = leg.transport.label(model);
            let ticket = match cooldowns.admit(provider, model) {
                Admission::Ready(ticket) => ticket,
                Admission::Cooling(cooling) => {
                    tracing::debug!(
                        tier = %request.tier,
                        model = %label,
                        reason = cooling.reason.as_str(),
                        "skipping a model candidate that is cooling down"
                    );
                    held.push((label, cooling));
                    continue;
                }
            };
            telemetry.attempted(&label);
            match leg.transport.attempt(model, request, call).await {
                Ok(answer) => {
                    ticket.succeed();
                    cooldowns.host_reachable(provider);
                    telemetry.succeeded(&label);
                    if position > 1 {
                        tracing::warn!(
                            tier = %request.tier,
                            model = %label,
                            skipped = position - 1,
                            "model fallback served the decision"
                        );
                    }
                    return Ok(answer);
                }
                Err(error) => {
                    failures.push(format!("{label}: {}", error.reason));
                    match error.class {
                        FailureClass::Transport => {
                            ticket.fail(CooldownFailure::new(CooldownReason::Unreachable));
                            let step = cooldowns.failures(provider, model);
                            let rest = &leg.models[index + 1..];
                            for other in rest {
                                cooldowns.hold_unreachable(provider, other, step);
                                host_skipped.push(format!(
                                    "{}: skipped (host unreachable)",
                                    leg.transport.label(other)
                                ));
                            }
                            position += rest.len();
                            if position < total {
                                tracing::warn!(
                                    tier = %request.tier,
                                    model = %label,
                                    provider = provider.as_str(),
                                    "model host unreachable; trying the next host"
                                );
                            }
                            continue 'route;
                        }
                        FailureClass::Cooldown(failure) => {
                            ticket.fail(failure);
                            if position < total {
                                tracing::warn!(
                                    tier = %request.tier,
                                    model = %label,
                                    reason = %error.reason,
                                    "model failed; trying the next candidate"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if failures.is_empty() {
        if held.is_empty() {
            return Err(ModelError::Request {
                reason: "no model candidate is configured for this request".to_owned(),
            });
        }
        return Err(all_cooling(&held));
    }
    failures.extend(host_skipped);
    for (label, cooling) in &held {
        failures.push(format!("{label}: cooling down ({})", cooling.reason));
    }
    Err(ModelError::Request {
        reason: failures.join(" | "),
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Scripted transports for routing tests. Nothing here reaches a network.

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    /// What a scripted attempt returns.
    #[derive(Debug, Clone)]
    pub(crate) enum Scripted {
        Answer,
        Fail(AttemptError),
    }

    /// A transport that records every model it was asked for and replays a
    /// scripted outcome per attempt (answering once the script runs out).
    pub(crate) struct ScriptedTransport {
        provider: ModelProvider,
        prefix: Option<&'static str>,
        script: Mutex<VecDeque<Scripted>>,
        pub(crate) asked: Mutex<Vec<String>>,
        pub(crate) tool_calls: Mutex<usize>,
    }

    impl ScriptedTransport {
        pub(crate) fn new(provider: ModelProvider, prefix: Option<&'static str>) -> Self {
            Self {
                provider,
                prefix,
                script: Mutex::new(VecDeque::new()),
                asked: Mutex::new(Vec::new()),
                tool_calls: Mutex::new(0),
            }
        }

        pub(crate) fn push(&self, outcome: Scripted) {
            self.script.lock().expect("script lock").push_back(outcome);
        }

        pub(crate) fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("asked lock").clone()
        }
    }

    #[async_trait]
    impl CandidateTransport for ScriptedTransport {
        fn candidate_provider(&self) -> ModelProvider {
            self.provider
        }

        fn label(&self, model: &str) -> String {
            match self.prefix {
                Some(prefix) => format!("{prefix}:{model}"),
                None => model.to_owned(),
            }
        }

        async fn attempt(
            &self,
            model: &str,
            request: &DecisionRequest,
            call: &mut Call<'_>,
        ) -> Result<DecisionAnswer, AttemptError> {
            self.asked
                .lock()
                .expect("asked lock")
                .push(model.to_owned());
            if let Call::Tools { progress, .. } = call {
                *self.tool_calls.lock().expect("tool lock") += 1;
                progress.tool_started("call-1", "observe", &json!({})).await;
            }
            let next = self.script.lock().expect("script lock").pop_front();
            match next {
                Some(Scripted::Fail(error)) => Err(error),
                Some(Scripted::Answer) | None => Ok(DecisionAnswer {
                    value: json!({ "model": model, "tier": request.tier.as_str() }),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::test_support::{Scripted, ScriptedTransport};
    use super::*;
    use crate::model::cooldown::CooldownReason;
    use crate::model::cooldown::test_clock::ManualClock;
    use crate::model::{AnswerFormat, ModelTier};

    fn request() -> DecisionRequest {
        DecisionRequest {
            instructions: "Decide.".to_owned(),
            input: "{}".to_owned(),
            format: AnswerFormat {
                name: "decision".to_owned(),
                schema: json!({"type": "object"}),
            },
            tier: ModelTier::Balanced,
        }
    }

    fn credits() -> AttemptError {
        AttemptError::cooldown(
            "insufficient_credits",
            CooldownFailure::new(CooldownReason::InsufficientCredits),
        )
    }

    fn models(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[actix_web::test]
    async fn candidates_run_in_order_and_cooled_ones_are_skipped_without_a_request() {
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let transport = ScriptedTransport::new(ModelProvider::OpenRouter, None);
        let chain = models(&["vendor/a", "vendor/b"]);
        let legs = [Leg {
            transport: &transport,
            models: &chain,
        }];
        let telemetry = Telemetry::default();

        transport.push(Scripted::Fail(credits()));
        let answer = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &telemetry,
        )
        .await
        .expect("the second candidate answers");
        assert_eq!(answer.value["model"], "vendor/b");
        assert_eq!(transport.asked(), ["vendor/a", "vendor/b"]);
        assert_eq!(telemetry.last_attempted().as_deref(), Some("vendor/b"));
        assert_eq!(telemetry.last_successful().as_deref(), Some("vendor/b"));

        // The next tick skips the cooled primary entirely.
        run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &telemetry,
        )
        .await
        .expect("answers");
        assert_eq!(transport.asked(), ["vendor/a", "vendor/b", "vendor/b"]);

        // After the cooldown the primary gets exactly one probe, which clears it.
        clock.advance(Duration::from_secs(30 * 60));
        run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &telemetry,
        )
        .await
        .expect("answers");
        assert_eq!(
            transport.asked().last().map(String::as_str),
            Some("vendor/a")
        );
        assert!(cooldowns.is_empty(), "a successful probe clears the entry");
    }

    #[actix_web::test]
    async fn a_transport_fault_holds_its_host_and_does_not_retry_it() {
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let transport = ScriptedTransport::new(ModelProvider::OpenRouter, None);
        transport.push(Scripted::Fail(AttemptError::transport("transport")));
        let chain = models(&["vendor/a", "vendor/b"]);
        let legs = [Leg {
            transport: &transport,
            models: &chain,
        }];
        let error = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect_err("the only host is unreachable");
        assert_eq!(
            error.to_string(),
            "model request failed: vendor/a: transport | vendor/b: skipped (host unreachable)"
        );
        assert_eq!(
            transport.asked(),
            ["vendor/a"],
            "the same host is not asked twice"
        );
        let held: Vec<(String, String, u64)> = cooldowns
            .snapshot()
            .into_iter()
            .map(|entry| (entry.model, entry.reason, entry.until_ms))
            .collect();
        let until = crate::model::cooldown::epoch_millis(clock.now() + Duration::from_secs(60));
        assert_eq!(
            held,
            [
                ("vendor/a".to_owned(), "unreachable".to_owned(), until),
                ("vendor/b".to_owned(), "unreachable".to_owned(), until)
            ]
        );

        // Held: refused without a request until the minute passes.
        preflight(&legs, &cooldowns).expect_err("the whole host is held");

        // The host stays down: the probe fails and every hold doubles.
        clock.advance(Duration::from_secs(60));
        transport.push(Scripted::Fail(AttemptError::transport("transport")));
        run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect_err("still unreachable");
        assert_eq!(transport.asked(), ["vendor/a", "vendor/a"]);
        let until = crate::model::cooldown::epoch_millis(clock.now() + Duration::from_secs(120));
        assert!(
            cooldowns
                .snapshot()
                .iter()
                .all(|entry| entry.until_ms == until)
        );

        // The host answers again: every unreachable hold on it is released.
        clock.advance(Duration::from_secs(120));
        run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect("the host is back");
        assert!(cooldowns.is_empty());
    }

    #[actix_web::test]
    async fn a_transport_fault_fails_over_to_another_host() {
        let cooldowns = CooldownRegistry::new();
        let subscription = ScriptedTransport::new(ModelProvider::Codex, Some("chatgpt"));
        let api = ScriptedTransport::new(ModelProvider::OpenRouter, None);
        let luna = models(&["gpt-6-luna"]);
        let chain = models(&["vendor/a", "vendor/b"]);
        let legs = [
            Leg {
                transport: &subscription,
                models: &luna,
            },
            Leg {
                transport: &api,
                models: &chain,
            },
        ];
        subscription.push(Scripted::Fail(AttemptError::transport("transport")));
        let telemetry = Telemetry::default();
        let answer = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &telemetry,
        )
        .await
        .expect("the configured provider takes over");
        assert_eq!(answer.value["model"], "vendor/a");
        assert_eq!(telemetry.last_successful().as_deref(), Some("vendor/a"));
        let entry = &cooldowns.snapshot()[0];
        assert_eq!(
            (entry.provider.as_str(), entry.reason.as_str()),
            ("codex", "unreachable")
        );

        // Both hosts unreachable: each is asked once, then both are held.
        let cooldowns = CooldownRegistry::new();
        subscription.push(Scripted::Fail(AttemptError::transport("transport")));
        api.push(Scripted::Fail(AttemptError::transport("transport")));
        let error = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &telemetry,
        )
        .await
        .expect_err("nothing is reachable");
        assert_eq!(
            error.to_string(),
            "model request failed: chatgpt:gpt-6-luna: transport | vendor/a: transport | vendor/b: skipped (host unreachable)"
        );
        assert_eq!(cooldowns.len(), 3);
    }

    #[actix_web::test]
    async fn every_candidate_cooling_fails_fast_with_the_soonest_retry() {
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let transport = ScriptedTransport::new(ModelProvider::OpenRouter, None);
        let subscription = ScriptedTransport::new(ModelProvider::Codex, Some("chatgpt"));
        let chain = models(&["vendor/a"]);
        let luna = models(&["gpt-6-luna"]);
        let legs = [
            Leg {
                transport: &subscription,
                models: &luna,
            },
            Leg {
                transport: &transport,
                models: &chain,
            },
        ];
        subscription.push(Scripted::Fail(AttemptError::cooldown(
            "overloaded",
            CooldownFailure::new(CooldownReason::Overloaded),
        )));
        transport.push(Scripted::Fail(credits()));
        let error = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect_err("both fail");
        assert_eq!(
            error.to_string(),
            "model request failed: chatgpt:gpt-6-luna: overloaded | vendor/a: insufficient_credits"
        );

        let refusal = preflight(&legs, &cooldowns).expect_err("everything is cooling");
        let expected = "model request failed: every model candidate is cooling down; soonest retry at 2026-01-01T00:01:00Z (chatgpt:gpt-6-luna: overloaded, vendor/a: insufficient_credits)";
        assert_eq!(refusal.to_string(), expected);
        let error = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect_err("nothing may be attempted");
        assert_eq!(error.to_string(), expected);
        assert_eq!(subscription.asked().len(), 1, "no further request was made");
        assert_eq!(transport.asked().len(), 1, "no further request was made");

        // Once one candidate's period passes, preflight lets the call through.
        clock.advance(Duration::from_secs(60));
        preflight(&legs, &cooldowns).expect("the subscription may be probed");
    }

    #[actix_web::test]
    async fn skipped_candidates_are_named_when_the_rest_fail() {
        let cooldowns = CooldownRegistry::new();
        let transport = ScriptedTransport::new(ModelProvider::OpenRouter, None);
        let chain = models(&["vendor/a", "vendor/b"]);
        let legs = [Leg {
            transport: &transport,
            models: &chain,
        }];
        transport.push(Scripted::Answer);
        transport.push(Scripted::Fail(credits()));
        // Cool vendor/a first via a failing single-candidate route.
        let only_a = models(&["vendor/a"]);
        let single = [Leg {
            transport: &transport,
            models: &only_a,
        }];
        run(
            &single,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect("answers");
        run(
            &single,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect_err("cools vendor/a");

        transport.push(Scripted::Fail(AttemptError::cooldown(
            "provider_rejected (404)",
            CooldownFailure::new(CooldownReason::ProviderRejected),
        )));
        let error = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect_err("vendor/b is rejected");
        assert_eq!(
            error.to_string(),
            "model request failed: vendor/b: provider_rejected (404) | vendor/a: cooling down (insufficient_credits)"
        );
    }

    #[actix_web::test]
    async fn an_empty_route_is_refused() {
        let cooldowns = CooldownRegistry::new();
        let transport = ScriptedTransport::new(ModelProvider::OpenRouter, None);
        let none: Vec<String> = Vec::new();
        let legs = [Leg {
            transport: &transport,
            models: &none,
        }];
        preflight(&legs, &cooldowns).expect("nothing to refuse on cooldown grounds");
        let error = run(
            &legs,
            &request(),
            &mut Call::Structured,
            &cooldowns,
            &Telemetry::default(),
        )
        .await
        .expect_err("nothing to attempt");
        assert!(error.to_string().contains("no model candidate"));
    }
}
