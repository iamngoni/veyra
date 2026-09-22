//! Model-provider integration boundary.
//!
//! Every decision provider implements [`DecisionEngine`], so the service
//! depends on one narrow, structured contract instead of a vendor SDK. The
//! active implementation is selected by configuration
//! (`VEYRA_MODEL_PROVIDER`) and constructed once at startup by
//! [`ModelRuntime`]. Adding a provider (or Jev) means adding an implementation
//! plus a selector; callers do not change.

pub mod agent_runtime_engine;
pub mod budget;
pub mod settings;

pub use agent_runtime_engine::AgentRuntimeEngine;
pub use budget::{BudgetPolicy, BudgetSnapshot, BudgetTracker, BudgetedEngine};
pub use settings::{ApiKey, ModelSettings, TierModels};

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

/// Capability tier a caller asks for; configuration resolves it to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    /// Cheapest configured model.
    Fast,
    /// Default balanced model.
    Balanced,
    /// Strongest configured model.
    Reasoning,
}

impl ModelTier {
    /// Stable name used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Reasoning => "reasoning",
        }
    }

    /// Parses a configuration value; unknown tiers are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fast" => Some(Self::Fast),
            "balanced" => Some(Self::Balanced),
            "reasoning" => Some(Self::Reasoning),
            _ => None,
        }
    }
}

impl fmt::Display for ModelTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Supported model provider implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProvider {
    /// OpenRouter (or any OpenAI-compatible endpoint configured the same way).
    OpenRouter,
}

impl ModelProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
        }
    }

    /// Parses a configuration value; unknown providers are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }
}

impl fmt::Display for ModelProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Response schema plus a stable name for the structured answer.
#[derive(Debug, Clone)]
pub struct AnswerFormat {
    /// Stable schema name (the forced function name on OpenAI-compatible backends).
    pub name: String,
    /// JSON Schema the answer must satisfy.
    pub schema: Value,
}

/// One structured question for a provider.
#[derive(Debug, Clone)]
pub struct DecisionRequest {
    /// System instructions describing the role and hard constraints.
    pub instructions: String,
    /// The concrete question or context to answer.
    pub input: String,
    /// Schema the answer is constrained to.
    pub format: AnswerFormat,
    /// Requested capability tier.
    pub tier: ModelTier,
}

/// Structured answer parsed from the provider response.
#[derive(Debug, Clone)]
pub struct DecisionAnswer {
    /// Parsed JSON payload satisfying [`DecisionRequest::format`].
    pub value: Value,
}

/// Errors raised while constructing or using a model provider.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The provider implementation could not be constructed.
    #[error("model engine construction failed: {reason}")]
    Construction {
        /// Non-sensitive explanation.
        reason: String,
    },
    /// A structured request failed or returned an unusable payload.
    #[error("model request failed: {reason}")]
    Request {
        /// Non-sensitive explanation.
        reason: String,
    },
}

/// Narrow contract every decision provider implements.
#[async_trait]
pub trait DecisionEngine: Send + Sync + fmt::Debug + 'static {
    /// Provider identifier for status output and logs.
    fn provider(&self) -> ModelProvider;

    /// Runs one structured request and returns the parsed answer.
    ///
    /// # Errors
    /// Returns [`ModelError::Request`] when the provider call fails or the
    /// response does not satisfy the requested schema.
    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError>;
}

/// Active model integration selected by configuration.
#[derive(Debug, Clone)]
pub struct ModelRuntime {
    provider: ModelProvider,
    engine: Arc<dyn DecisionEngine>,
    budget: Arc<BudgetTracker>,
    /// Kept so a surface can report which models a tier will actually try,
    /// which is the difference between "the decision failed" and "the decision
    /// failed on every model configured for it".
    tiers: TierModels,
}

impl ModelRuntime {
    /// Builds the implementation selected by settings, wrapped in the call
    /// budget so a runaway loop cannot silently multiply provider cost.
    ///
    /// # Errors
    /// Returns [`ModelError::Construction`] when the provider rejects its
    /// configuration.
    pub fn from_settings(settings: ModelSettings) -> Result<Self, ModelError> {
        match settings.provider() {
            ModelProvider::OpenRouter => {
                let engine = AgentRuntimeEngine::build(&settings)?;
                let budget = Arc::new(BudgetTracker::new(*settings.budget()));
                let engine = Arc::new(BudgetedEngine::new(Arc::new(engine), budget.clone()));
                Ok(Self {
                    provider: settings.provider(),
                    engine,
                    budget,
                    tiers: settings.tiers().clone(),
                })
            }
        }
    }

    /// Provider identifier of the active implementation.
    pub fn provider(&self) -> ModelProvider {
        self.provider
    }

    /// Domain-level engine contract used by the decision layer.
    pub fn engine(&self) -> Arc<dyn DecisionEngine> {
        self.engine.clone()
    }

    /// The ordered candidate models for a tier: primary first, then the
    /// fallbacks tried when it cannot serve a decision.
    pub fn chain(&self, tier: ModelTier) -> &[String] {
        self.tiers.chain(tier)
    }

    /// Current call usage against the configured budget.
    pub fn budget(&self) -> BudgetSnapshot {
        self.budget.snapshot()
    }

    /// Serializable snapshot of the call-budget windows.
    pub fn state_snapshot(&self) -> Value {
        self.budget.state_snapshot()
    }

    /// Restores the call-budget windows from a stored snapshot.
    ///
    /// # Errors
    /// Returns a description when the value is not a budget snapshot.
    pub fn restore_state(&self, value: &Value) -> Result<(), String> {
        self.budget.restore_state(value)
    }

    /// Builds a runtime around an injected engine; used by tests.
    #[cfg(test)]
    pub(crate) fn with_engine(provider: ModelProvider, engine: Arc<dyn DecisionEngine>) -> Self {
        Self {
            provider,
            engine,
            budget: Arc::new(BudgetTracker::new(BudgetPolicy::default())),
            tiers: TierModels::new("test/fast", "test/balanced", "test/reasoning"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_names_round_trip() {
        for (tier, name) in [
            (ModelTier::Fast, "fast"),
            (ModelTier::Balanced, "balanced"),
            (ModelTier::Reasoning, "reasoning"),
        ] {
            assert_eq!(tier.as_str(), name);
            assert_eq!(ModelTier::parse(name), Some(tier));
            assert_eq!(ModelTier::parse(&name.to_ascii_uppercase()), Some(tier));
            assert_eq!(tier.to_string(), name);
        }
        assert_eq!(ModelTier::parse("genius"), None);
    }
}
