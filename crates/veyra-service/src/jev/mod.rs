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

/// Active judgement integration selected by configuration.
#[derive(Debug, Clone)]
pub struct JevRuntime {
    provider: JevProvider,
    judge: Arc<dyn SemanticJudge>,
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
