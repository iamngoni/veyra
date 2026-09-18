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
//! The trade pipeline still owns every decision: Jev judgements are inputs
//! code may consult, never execution authority.

pub mod contract;
pub mod http;
pub mod settings;

pub use contract::{
    Answer, ChoiceAnswer, ChoiceOptions, Confidence, Instructions, JevRequest, JevResponse,
    NoulAnswer, NoulCriteria, Probability, Question, ScoreAnswer, ScoreLevels, State, Usage,
};
pub use http::HttpJev;
pub use settings::{JevApiKey, JevSettings};

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

/// Supported System One providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevProvider {
    /// TypeSafe System One, currently the Jev model family.
    TypeSafe,
}

impl JevProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TypeSafe => "typesafe",
        }
    }

    /// Parses a configuration value; unknown providers are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "typesafe" => Some(Self::TypeSafe),
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
}

/// Active judgement integration selected by configuration.
#[derive(Debug, Clone)]
pub struct JevRuntime {
    provider: JevProvider,
    judge: Arc<dyn SemanticJudge>,
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
                Ok(Self {
                    provider: settings.provider(),
                    judge: Arc::new(judge),
                    usage: Arc::new(JevUsage::default()),
                })
            }
        }
    }

    /// Provider identifier of the active implementation.
    pub fn provider(&self) -> JevProvider {
        self.provider
    }

    /// Domain-level judgement contract used by application code.
    pub fn judge(&self) -> &Arc<dyn SemanticJudge> {
        &self.judge
    }

    /// Runs one judgement request, counting the call and the token usage the
    /// provider reports. Errors are counted as failures and returned as-is.
    ///
    /// # Errors
    /// Returns [`JevError`] from the active judge unchanged.
    pub async fn evaluate(&self, request: JevRequest) -> Result<JevResponse, JevError> {
        self.usage.calls.fetch_add(1, Ordering::Relaxed);
        match self.judge.judge(request).await {
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

    /// Returns a snapshot of the process-lifetime judge usage.
    pub fn usage(&self) -> JevUsageSnapshot {
        JevUsageSnapshot {
            calls: self.usage.calls.load(Ordering::Relaxed),
            failures: self.usage.failures.load(Ordering::Relaxed),
            input_tokens: self.usage.input_tokens.load(Ordering::Relaxed),
            output_tokens: self.usage.output_tokens.load(Ordering::Relaxed),
        }
    }

    /// Builds a runtime around an injected judge; used by tests.
    #[cfg(test)]
    pub(crate) fn with_judge(provider: JevProvider, judge: Arc<dyn SemanticJudge>) -> Self {
        Self {
            provider,
            judge,
            usage: Arc::new(JevUsage::default()),
        }
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
        assert_eq!(JevProvider::parse("openai"), None);
        assert_eq!(JevProvider::TypeSafe.to_string(), "typesafe");
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
