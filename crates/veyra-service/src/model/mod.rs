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
pub mod subscription_engine;

pub use agent_runtime_engine::AgentRuntimeEngine;
pub use budget::{BudgetPolicy, BudgetSnapshot, BudgetTracker, BudgetedEngine};
pub use settings::{ApiKey, ModelSettings, TierModels};
pub use subscription_engine::SubscriptionEngine;

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
    /// ChatGPT consumer subscription through the Codex protocol.
    Codex,
    /// Claude consumer subscription through the Claude Code protocol.
    ClaudeCode,
    /// OpenAI's hosted API.
    OpenAi,
    /// Anthropic's native messages API.
    Anthropic,
    /// OpenRouter's OpenAI-compatible gateway.
    OpenRouter,
    /// Groq's OpenAI-compatible API.
    Groq,
    /// DeepSeek's OpenAI-compatible API.
    DeepSeek,
    /// xAI's OpenAI-compatible API.
    Xai,
    /// Mistral's OpenAI-compatible API.
    Mistral,
    /// Moonshot/Kimi's OpenAI-compatible API.
    Kimi,
    /// A local Ollama OpenAI-compatible API. Its API key is optional.
    Ollama,
    /// One explicitly configured OpenAI-compatible endpoint.
    Custom,
}

impl ModelProvider {
    /// Short identifier used in configuration and status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::OpenRouter => "openrouter",
            Self::Groq => "groq",
            Self::DeepSeek => "deepseek",
            Self::Xai => "xai",
            Self::Mistral => "mistral",
            Self::Kimi => "kimi",
            Self::Ollama => "ollama",
            Self::Custom => "custom",
        }
    }

    /// Parses a configuration value; unknown providers are rejected.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" | "chatgpt_subscription" => Some(Self::Codex),
            "claude_code" | "claude_subscription" => Some(Self::ClaudeCode),
            "openai" => Some(Self::OpenAi),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "openrouter" => Some(Self::OpenRouter),
            "groq" => Some(Self::Groq),
            "deepseek" => Some(Self::DeepSeek),
            "xai" | "grok" => Some(Self::Xai),
            "mistral" => Some(Self::Mistral),
            "kimi" | "moonshot" => Some(Self::Kimi),
            "ollama" => Some(Self::Ollama),
            "custom" | "openai-compatible" | "openai_compatible" => Some(Self::Custom),
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

/// Provider-neutral metadata for one tool the model may call.
#[derive(Debug, Clone)]
pub struct ReadOnlyToolDefinition {
    /// Stable tool name sent to the provider.
    pub name: String,
    /// Human-readable description used for model tool selection.
    pub description: String,
    /// JSON Schema for the tool arguments.
    pub input_schema: Value,
}

/// A model-directed observation tool.
///
/// Implementations must only inspect already-retained service state. They
/// must not enqueue broker commands, mutate configuration, or place orders.
#[async_trait]
pub trait ReadOnlyTool: Send + Sync + 'static {
    /// Returns the provider-neutral definition exposed to the model.
    fn definition(&self) -> ReadOnlyToolDefinition;

    /// Executes one validated observation request.
    ///
    /// Errors are returned as bounded, non-sensitive text and remain model
    /// evidence rather than becoming execution authority.
    async fn execute(&self, arguments: Value) -> Result<Value, String>;
}

/// Receives lifecycle notifications for model-directed read-only tools.
#[async_trait]
pub trait ToolProgressSink: Send {
    /// Called immediately before an allowlisted tool executes.
    async fn tool_started(&mut self, call_id: &str, name: &str, arguments: &Value);

    /// Called after an allowlisted tool returns an observation or bounded
    /// failure. `available` distinguishes a missing dependency from a result.
    async fn tool_completed(&mut self, call_id: &str, name: &str, result: &Value, available: bool);
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

    /// The most recent model candidate that was actually requested.
    ///
    /// This includes a candidate that failed, which lets an operator tell
    /// whether the configured fallback chain was reached during an outage.
    fn last_attempted_model(&self) -> Option<String> {
        None
    }

    /// The most recent model candidate that returned a structured answer.
    fn last_successful_model(&self) -> Option<String> {
        None
    }

    /// Runs one structured request and returns the parsed answer.
    ///
    /// # Errors
    /// Returns [`ModelError::Request`] when the provider call fails or the
    /// response does not satisfy the requested schema.
    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError>;

    /// Runs a model-directed, read-only tool session and returns a natural
    /// language answer in the standard `{"answer": ...}` envelope.
    ///
    /// Implementations that do not expose provider tool sessions fail closed;
    /// callers must not silently fall back to execution-capable paths.
    async fn answer_with_tools(
        &self,
        request: DecisionRequest,
        tools: Vec<Arc<dyn ReadOnlyTool>>,
        progress: &mut dyn ToolProgressSink,
    ) -> Result<DecisionAnswer, ModelError> {
        let _ = (request, tools, progress);
        Err(ModelError::Request {
            reason: "read-only tool sessions are unavailable".to_owned(),
        })
    }
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
        if matches!(
            settings.provider(),
            ModelProvider::Codex | ModelProvider::ClaudeCode
        ) {
            return Err(ModelError::Construction {
                reason: "subscription runtime needs its encrypted connection".to_owned(),
            });
        }
        let engine = AgentRuntimeEngine::build(&settings)?;
        Ok(Self::wrap_engine(&settings, Arc::new(engine)))
    }

    /// Builds an API or connected subscription provider from the same narrow
    /// decision contract, preserving the call budget for both transports.
    pub fn from_settings_with_app(
        settings: ModelSettings,
        app: &crate::AppState,
    ) -> Result<Self, ModelError> {
        let engine: Arc<dyn DecisionEngine> = if matches!(
            settings.provider(),
            ModelProvider::Codex | ModelProvider::ClaudeCode
        ) {
            Arc::new(SubscriptionEngine::build(&settings, app)?)
        } else {
            Arc::new(AgentRuntimeEngine::build(&settings)?)
        };
        Ok(Self::wrap_engine(&settings, engine))
    }

    fn wrap_engine(settings: &ModelSettings, engine: Arc<dyn DecisionEngine>) -> Self {
        let budget = Arc::new(BudgetTracker::new(*settings.budget()));
        let engine = Arc::new(BudgetedEngine::new(engine, budget.clone()));
        Self {
            provider: settings.provider(),
            engine,
            budget,
            tiers: settings.tiers().clone(),
        }
    }

    /// Provider identifier of the active implementation.
    pub fn provider(&self) -> ModelProvider {
        self.provider
    }

    /// The most recent model candidate that was actually requested.
    pub fn last_attempted_model(&self) -> Option<String> {
        self.engine.last_attempted_model()
    }

    /// The most recent model candidate that returned a structured answer.
    pub fn last_successful_model(&self) -> Option<String> {
        self.engine.last_successful_model()
    }

    /// Domain-level engine contract used by the decision layer.
    pub fn engine(&self) -> Arc<dyn DecisionEngine> {
        self.engine.clone()
    }

    /// Runs a model-directed, read-only tool session through the configured
    /// provider while preserving the call-budget wrapper.
    pub async fn answer_with_tools(
        &self,
        request: DecisionRequest,
        tools: Vec<Arc<dyn ReadOnlyTool>>,
        progress: &mut dyn ToolProgressSink,
    ) -> Result<DecisionAnswer, ModelError> {
        self.engine
            .answer_with_tools(request, tools, progress)
            .await
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
