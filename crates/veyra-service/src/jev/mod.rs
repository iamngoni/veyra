//! TypeSafe/Jev semantic-judgement boundary.
//!
//! Jev is a System One model: state plus typed questions in, structured
//! judgements out — a choice with a probability distribution, a noul
//! probability, or a score with a legend, each carrying confidence. It is
//! deliberately **not** a [`DecisionEngine`](crate::model::DecisionEngine):
//! it returns calibrated judgements rather than schema-constrained
//! generations, so it has its own narrow contract ([`SemanticJudge`]), its own
//! validated types ([`contract`]), and its own provider selector
//! (`VEYRA_JEV_PROVIDER`). Adding another System One provider means adding an
//! implementation plus a variant; callers do not change.
//!
//! The configured judge is always TypeSafe. The operator may layer OpenAI
//! Decisions ([`openai`]) over it at runtime as a primary with the configured
//! judge as automatic fallback ([`fallback`]); [`JevRuntime`] holds that
//! swappable slot so callers keep one stable handle (see [`crate::judge`]).
//!
//! The trade pipeline still owns every decision: Jev judgements are inputs
//! code may consult, never execution authority.

pub mod contract;
pub mod fallback;
pub mod http;
pub mod openai;
pub mod settings;

pub use contract::{
    Answer, ChoiceAnswer, ChoiceOptions, Confidence, Instructions, JevRequest, JevResponse,
    NoulAnswer, NoulCriteria, Probability, Question, ScoreAnswer, ScoreLevels, State, Usage,
};
pub use fallback::FallbackJudge;
pub use http::HttpJev;
pub use openai::{OpenAiApiKey, OpenAiDecisions, OpenAiJudge, OpenAiKeyError, OpenAiSettings};
pub use settings::{JevApiKey, JevSettings};

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;

/// Supported System One providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevProvider {
    /// TypeSafe System One, currently the Jev model family.
    TypeSafe,
    /// OpenAI Decisions (limited preview). Only ever an operator-selected
    /// primary over a configured TypeSafe judge, never configured alone.
    OpenAi,
}

impl JevProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TypeSafe => "typesafe",
            Self::OpenAi => "openai",
        }
    }

    /// Parses a provider name; unknown providers are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "typesafe" => Some(Self::TypeSafe),
            "openai" => Some(Self::OpenAi),
            _ => None,
        }
    }
}

impl fmt::Display for JevProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Errors raised while building or using a judgement engine.
#[derive(Debug, thiserror::Error)]
pub enum JevError {
    /// The request or response violated the judgement contract.
    #[error("jev contract violation: {reason}")]
    Contract {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// The service rejected the credential.
    #[error("jev rejected the credential")]
    Unauthorized,
    /// The service refused access (401/403) and said why, for example a
    /// preview API that is not enabled for the account.
    #[error("judge denied access (status {status}): {detail}")]
    Denied {
        /// HTTP status reported by the service.
        status: u16,
        /// Non-sensitive explanation from the service, keys scrubbed.
        detail: String,
    },
    /// The service rejected the request payload.
    #[error("jev rejected the request: {detail}")]
    Rejected {
        /// Non-sensitive explanation from the service.
        detail: String,
    },
    /// The service asked for a backoff; callers retry later.
    #[error("jev requested a backoff (status {status})")]
    Unavailable {
        /// HTTP status reported by the service.
        status: u16,
    },
    /// Network, timeout, or unexpected-status failure.
    #[error("jev transport failed: {reason}")]
    Transport {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// The success response did not satisfy the documented contract.
    #[error("jev response violated the contract: {reason}")]
    MalformedResponse {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Narrow contract every System One integration implements.
#[async_trait]
pub trait SemanticJudge: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output and logs.
    fn provider(&self) -> JevProvider;

    /// Runs one judgement request and returns validated answers.
    ///
    /// # Errors
    /// Returns [`JevError`] when the transport fails or the request or
    /// response violates the contract.
    async fn judge(&self, request: JevRequest) -> Result<JevResponse, JevError>;
}

/// Process-lifetime count of judge calls and the token usage they reported.
///
/// The provider's own dashboard can lag, aggregate differently, or belong to a
/// different project; this is the service's authoritative view of what it
/// actually spent.
#[derive(Debug, Default)]
pub struct JevUsage {
    calls: AtomicU64,
    failures: AtomicU64,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    /// Requests an operator-selected primary handed to the configured judge.
    fallbacks: AtomicU64,
}

/// Snapshot of [`JevUsage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JevUsageSnapshot {
    /// Judgement calls attempted.
    pub calls: u64,
    /// Calls that returned an error.
    pub failures: u64,
    /// Prompt tokens reported by the provider.
    pub input_tokens: u64,
    /// Completion tokens reported by the provider.
    pub output_tokens: u64,
    /// Calls the configured judge answered because the selected primary
    /// failed or was in its post-failure bypass window.
    pub fallbacks: u64,
}

/// Active judgement integration selected by configuration, plus an optional
/// operator-selected primary layered over it.
///
/// Clones share one primary slot, so a switch made through any clone (the
/// console's) is seen by every holder (the autopilot's) on its next call.
#[derive(Debug, Clone)]
pub struct JevRuntime {
    provider: JevProvider,
    judge: Arc<dyn SemanticJudge>,
    /// A [`FallbackJudge`] over `judge` while a primary is selected.
    primary: Arc<RwLock<Option<Arc<dyn SemanticJudge>>>>,
    usage: Arc<JevUsage>,
}

impl JevRuntime {
    /// Builds the implementation selected by settings.
    ///
    /// # Errors
    /// Returns [`JevError`] when the selected implementation cannot be built.
    pub fn from_settings(settings: JevSettings) -> Result<Self, JevError> {
        match settings.provider() {
            JevProvider::TypeSafe => {
                let judge = HttpJev::from_settings(&settings)?;
                Ok(Self::assemble(settings.provider(), Arc::new(judge)))
            }
            // Settings never select OpenAI (see `JevSettings::from_source`);
            // it is only layered over a configured judge at runtime.
            JevProvider::OpenAi => Err(JevError::Contract {
                reason: "OpenAI is selected in the console, not configured as the base judge"
                    .to_owned(),
            }),
        }
    }

    fn assemble(provider: JevProvider, judge: Arc<dyn SemanticJudge>) -> Self {
        Self {
            provider,
            judge,
            primary: Arc::new(RwLock::new(None)),
            usage: Arc::new(JevUsage::default()),
        }
    }

    fn primary_slot(&self) -> Option<Arc<dyn SemanticJudge>> {
        self.primary
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Provider answering first: the selected primary, else the configured
    /// judge.
    pub fn provider(&self) -> JevProvider {
        self.primary_slot()
            .map_or(self.provider, |primary| primary.provider())
    }

    /// Provider of the configured judge, which is also the fallback.
    pub fn configured_provider(&self) -> JevProvider {
        self.provider
    }

    /// Domain-level judgement contract used by application code: the
    /// primary-with-fallback judge while a primary is selected, else the
    /// configured judge.
    pub fn judge(&self) -> Arc<dyn SemanticJudge> {
        self.primary_slot()
            .unwrap_or_else(|| Arc::clone(&self.judge))
    }

    /// Makes `primary` answer first, with the configured judge as automatic
    /// fallback for any primary error (see [`FallbackJudge`]).
    pub fn use_primary(&self, primary: Arc<dyn SemanticJudge>) {
        let layered: Arc<dyn SemanticJudge> = Arc::new(FallbackJudge::new(
            primary,
            Arc::clone(&self.judge),
            Arc::clone(&self.usage),
            FallbackJudge::RETRY_AFTER,
        ));
        *self.primary.write().unwrap_or_else(PoisonError::into_inner) = Some(layered);
    }

    /// Returns to the configured judge alone.
    pub fn clear_primary(&self) {
        *self.primary.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Runs one judgement request, counting the call and the token usage the
    /// provider reports. Errors are counted as failures and returned as-is.
    ///
    /// # Errors
    /// Returns [`JevError`] from the active judge unchanged.
    pub async fn evaluate(&self, request: JevRequest) -> Result<JevResponse, JevError> {
        self.usage.calls.fetch_add(1, Ordering::Relaxed);
        match self.judge().judge(request).await {
            Ok(response) => {
                let usage = response.usage();
                self.usage
                    .input_tokens
                    .fetch_add(usage.input_tokens, Ordering::Relaxed);
                self.usage
                    .output_tokens
                    .fetch_add(usage.output_tokens, Ordering::Relaxed);
                Ok(response)
            }
            Err(error) => {
                self.usage.failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    /// Serializable snapshot of the cumulative usage counters, so a restart
    /// resumes the totals instead of resetting them.
    pub fn state_snapshot(&self) -> serde_json::Value {
        let usage = self.usage();
        serde_json::json!({
            "calls": usage.calls,
            "failures": usage.failures,
            "inputTokens": usage.input_tokens,
            "outputTokens": usage.output_tokens,
            "fallbacks": usage.fallbacks
        })
    }

    /// Restores the cumulative counters from a stored snapshot.
    ///
    /// `fallbacks` postdates the other counters, so a snapshot written before
    /// it existed restores it as zero; a present but non-numeric value is
    /// still rejected.
    ///
    /// # Errors
    /// Returns a description when the value is not a usage snapshot.
    pub fn restore_state(&self, value: &serde_json::Value) -> Result<(), String> {
        let field = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("usage snapshot is missing `{name}`"))
        };
        let fallbacks = match value.get("fallbacks") {
            None => 0,
            Some(_) => field("fallbacks")?,
        };
        self.usage.calls.store(field("calls")?, Ordering::Relaxed);
        self.usage
            .failures
            .store(field("failures")?, Ordering::Relaxed);
        self.usage
            .input_tokens
            .store(field("inputTokens")?, Ordering::Relaxed);
        self.usage
            .output_tokens
            .store(field("outputTokens")?, Ordering::Relaxed);
        self.usage.fallbacks.store(fallbacks, Ordering::Relaxed);
        Ok(())
    }

    /// Returns a snapshot of the process-lifetime judge usage.
    pub fn usage(&self) -> JevUsageSnapshot {
        JevUsageSnapshot {
            calls: self.usage.calls.load(Ordering::Relaxed),
            failures: self.usage.failures.load(Ordering::Relaxed),
            input_tokens: self.usage.input_tokens.load(Ordering::Relaxed),
            output_tokens: self.usage.output_tokens.load(Ordering::Relaxed),
            fallbacks: self.usage.fallbacks.load(Ordering::Relaxed),
        }
    }

    /// Builds a runtime around an injected judge; used by tests.
    #[cfg(test)]
    pub(crate) fn with_judge(provider: JevProvider, judge: Arc<dyn SemanticJudge>) -> Self {
        Self::assemble(provider, judge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigError;

    #[test]
    fn provider_names_are_stable() {
        assert_eq!(JevProvider::TypeSafe.as_str(), "typesafe");
        assert_eq!(
            JevProvider::parse(" TypeSafe "),
            Some(JevProvider::TypeSafe)
        );
        assert_eq!(JevProvider::parse(" OpenAI "), Some(JevProvider::OpenAi));
        assert_eq!(JevProvider::parse("anthropic"), None);
        assert_eq!(JevProvider::TypeSafe.to_string(), "typesafe");
        assert_eq!(JevProvider::OpenAi.to_string(), "openai");
    }

    /// Judge that always fails with a provider-reported denial.
    #[derive(Debug)]
    struct DeniedJudge;

    #[async_trait]
    impl SemanticJudge for DeniedJudge {
        fn provider(&self) -> JevProvider {
            JevProvider::OpenAi
        }

        async fn judge(&self, _request: JevRequest) -> Result<JevResponse, JevError> {
            Err(JevError::Denied {
                status: 403,
                detail: "Decision API is not enabled for this user.".to_owned(),
            })
        }
    }

    #[actix_web::test]
    async fn a_primary_is_shared_by_clones_and_falls_back_to_the_configured_judge() {
        let runtime =
            JevRuntime::with_judge(JevProvider::TypeSafe, Arc::new(StubJudge { fail: false }));
        let autopilot_handle = runtime.clone();
        assert_eq!(runtime.provider(), JevProvider::TypeSafe);

        runtime.use_primary(Arc::new(DeniedJudge));
        assert_eq!(autopilot_handle.provider(), JevProvider::OpenAi);
        assert_eq!(
            autopilot_handle.configured_provider(),
            JevProvider::TypeSafe
        );
        assert_eq!(autopilot_handle.judge().provider(), JevProvider::OpenAi);

        // The primary's failure never reaches the caller: the configured
        // judge answers and the fallback is counted and persisted.
        let answer = autopilot_handle
            .evaluate(sample_request())
            .await
            .expect("fallback answers");
        assert_eq!(answer.model(), "jev-test");
        let usage = runtime.usage();
        assert_eq!((usage.calls, usage.failures, usage.fallbacks), (1, 0, 1));
        assert_eq!(runtime.state_snapshot()["fallbacks"], 1);

        runtime.clear_primary();
        assert_eq!(autopilot_handle.provider(), JevProvider::TypeSafe);
        autopilot_handle
            .evaluate(sample_request())
            .await
            .expect("configured judge answers");
        assert_eq!(runtime.usage().fallbacks, 1);
    }

    #[test]
    fn snapshots_without_fallbacks_still_restore() {
        let runtime =
            JevRuntime::with_judge(JevProvider::TypeSafe, Arc::new(StubJudge { fail: false }));
        let legacy =
            serde_json::json!({"calls": 3, "failures": 1, "inputTokens": 9, "outputTokens": 4});
        runtime.restore_state(&legacy).expect("legacy snapshot");
        assert_eq!(runtime.usage().fallbacks, 0);
        assert_eq!(runtime.usage().calls, 3);

        let mut malformed = legacy.clone();
        malformed["fallbacks"] = serde_json::json!("two");
        assert!(runtime.restore_state(&malformed).is_err());

        let mut current = legacy;
        current["fallbacks"] = serde_json::json!(2);
        runtime.restore_state(&current).expect("current snapshot");
        assert_eq!(runtime.usage().fallbacks, 2);
    }

    /// Stub judge returning the canonical contract sample, or failing.
    #[derive(Debug)]
    struct StubJudge {
        fail: bool,
    }

    #[async_trait]
    impl SemanticJudge for StubJudge {
        fn provider(&self) -> JevProvider {
            JevProvider::TypeSafe
        }

        async fn judge(&self, _request: JevRequest) -> Result<JevResponse, JevError> {
            if self.fail {
                return Err(JevError::Transport {
                    reason: "probe down".to_owned(),
                });
            }
            let body = serde_json::json!({
                "model": "jev-test",
                "answers": {
                    "trending": {"type": "noul", "noul": 0.6}
                },
                "usage": {"input_tokens": 402, "output_tokens": 73}
            });
            crate::jev::contract::parse_response_body(body.to_string().as_bytes())
        }
    }

    fn sample_request() -> JevRequest {
        let state = State::text("EURUSD H4 probe").expect("state");
        let mut questions = std::collections::BTreeMap::new();
        questions.insert(
            "trending".to_owned(),
            Question::noul(
                Instructions::text("Does this look trending?").expect("instructions"),
                NoulCriteria::default(),
            ),
        );
        JevRequest::new(state, questions).expect("request")
    }

    #[actix_web::test]
    async fn evaluate_counts_calls_and_reported_tokens() {
        let runtime =
            JevRuntime::with_judge(JevProvider::TypeSafe, Arc::new(StubJudge { fail: false }));
        assert_eq!(runtime.usage().calls, 0);

        runtime
            .evaluate(sample_request())
            .await
            .expect("judgement succeeds");
        runtime
            .evaluate(sample_request())
            .await
            .expect("judgement succeeds");

        let usage = runtime.usage();
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.failures, 0);
        assert_eq!(usage.input_tokens, 804);
        assert_eq!(usage.output_tokens, 146);
    }

    #[actix_web::test]
    async fn usage_survives_a_restart_through_a_snapshot() {
        let runtime =
            JevRuntime::with_judge(JevProvider::TypeSafe, Arc::new(StubJudge { fail: false }));
        runtime
            .evaluate(sample_request())
            .await
            .expect("judge answers");
        runtime
            .evaluate(sample_request())
            .await
            .expect("judge answers");
        let snapshot = runtime.state_snapshot();
        assert_eq!(snapshot["calls"], 2);
        assert_eq!(snapshot["inputTokens"], 804);
        assert_eq!(snapshot["outputTokens"], 146);

        let restarted =
            JevRuntime::with_judge(JevProvider::TypeSafe, Arc::new(StubJudge { fail: true }));
        restarted
            .evaluate(sample_request())
            .await
            .expect_err("failing judge counts a failure");
        assert_eq!(restarted.usage().failures, 1);

        restarted
            .restore_state(&snapshot)
            .expect("snapshot restores");
        let usage = restarted.usage();
        assert_eq!(usage.calls, 2, "totals resume instead of resetting");
        assert_eq!(usage.failures, 0);
        assert_eq!(usage.input_tokens, 804);
        assert_eq!(usage.output_tokens, 146);

        assert!(
            restarted
                .restore_state(&serde_json::json!({"calls": 1}))
                .is_err()
        );
        assert_eq!(snapshot["fallbacks"], 0);
    }

    #[actix_web::test]
    async fn evaluate_counts_failures_without_tokens() {
        let runtime =
            JevRuntime::with_judge(JevProvider::TypeSafe, Arc::new(StubJudge { fail: true }));
        runtime.evaluate(sample_request()).await.expect_err("fails");
        let usage = runtime.usage();
        assert_eq!((usage.calls, usage.failures), (1, 1));
        assert_eq!((usage.input_tokens, usage.output_tokens), (0, 0));
    }

    #[test]
    fn runtime_builds_from_settings_without_network_io() {
        let settings = JevSettings::from_source(|name| match name {
            "VEYRA_JEV_API_KEY" => Ok("apikey_test_1234567890".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings parse")
        .expect("configured");

        let runtime = JevRuntime::from_settings(settings).expect("runtime builds");
        assert_eq!(runtime.provider(), JevProvider::TypeSafe);
        assert_eq!(runtime.judge().provider(), JevProvider::TypeSafe);
        assert!(format!("{runtime:?}").contains("redacted"));
    }
}
