//! Model-provider integration boundary.
//!
//! Every decision provider implements [`DecisionEngine`], so the service
//! depends on one narrow, structured contract instead of a vendor SDK. The
//! active implementation is selected by configuration
//! (`VEYRA_MODEL_PROVIDER`) and constructed once at startup by
//! [`ModelRuntime`]. Adding a provider (or Jev) means adding an implementation
//! plus a selector; callers do not change.
//!
//! When a ChatGPT subscription is connected and
//! `VEYRA_MODEL_PREFER_SUBSCRIPTION` is on, the runtime routes every call to
//! the subscription first and falls back to the configured provider's chain.
//! Every candidate on every route is subject to the shared per-model
//! cooldowns in [`cooldown`]; the call budget wraps the whole route once.

pub mod agent_runtime_engine;
pub mod budget;
pub mod cooldown;
mod preferred;
pub(crate) mod route;
pub mod settings;
pub mod subscription_engine;

pub use agent_runtime_engine::AgentRuntimeEngine;
pub use budget::{BudgetPolicy, BudgetSnapshot, BudgetTracker, BudgetedEngine};
pub use cooldown::{
    Admission, AttemptTicket, CooldownClock, CooldownEntry, CooldownFailure, CooldownReason,
    CooldownRegistry, Cooling,
};
pub use settings::{ApiKey, ModelSettings, TierModels};
pub use subscription_engine::SubscriptionEngine;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::subscription_auth::{SubscriptionAuthState, SubscriptionProvider};
use preferred::{PreferredEngine, SubscriptionGate};

/// Operator-facing label for a candidate served by the ChatGPT subscription,
/// so status output can tell it apart from an API model of the same name.
pub fn chatgpt_label(model: &str) -> String {
    format!("chatgpt:{model}")
}

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
    /// Z.AI (Zhipu GLM) OpenAI-compatible API. Accepts only an automatic
    /// tool choice, so structured answers are never compelled.
    Zai,
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
            Self::Zai => "zai",
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
            "zai" | "z-ai" | "zhipu" => Some(Self::Zai),
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

    /// Refuses a request before any cost is incurred when no candidate could
    /// serve `tier` right now — every one is cooling down.
    ///
    /// Callers that meter cost (the call budget) check this first, so an
    /// all-cooled route neither sends a request nor spends budget. The default
    /// admits everything.
    ///
    /// # Errors
    /// Returns [`ModelError::Request`] naming the soonest retry time.
    fn preflight(&self, tier: ModelTier) -> Result<(), ModelError> {
        let _ = tier;
        Ok(())
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
    /// Settings this runtime was built from; absent for injected test engines.
    settings: Option<ModelSettings>,
    /// The ChatGPT subscription leg in front of the configured chain, when the
    /// composite was built.
    preferred: Option<PreferredLeg>,
    /// ChatGPT connection state observed at build time, recorded only when the
    /// subscription preference applies. A difference from the live state means
    /// the route is stale and must be rebuilt.
    chatgpt_connected_at_build: Option<bool>,
}

/// The preferred subscription leg as the runtime reports it.
#[derive(Debug, Clone)]
struct PreferredLeg {
    model: String,
    gate: SubscriptionGate,
}

/// Route facts decided while building, beyond the engine itself.
#[derive(Debug, Default)]
struct RouteShape {
    preferred: Option<PreferredLeg>,
    chatgpt_connected_at_build: Option<bool>,
}

impl ModelRuntime {
    /// Builds the implementation selected by settings, wrapped in the call
    /// budget so a runaway loop cannot silently multiply provider cost.
    ///
    /// Without the application state there is no subscription connection to
    /// prefer and no shared cooldown registry, so this builds the configured
    /// API chain with a private registry.
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
        Ok(Self::wrap_engine(
            settings,
            Arc::new(engine),
            RouteShape::default(),
        ))
    }

    /// Builds an API or connected subscription provider from the same narrow
    /// decision contract, preserving the call budget for both transports.
    ///
    /// For an API provider with `VEYRA_MODEL_PREFER_SUBSCRIPTION` on and the
    /// ChatGPT subscription connected, the engine is the subscription-first
    /// composite. A subscription leg that cannot be built (for example without
    /// encrypted storage) is logged and left out rather than disabling the
    /// configured provider. The Claude Code provider is built exactly as
    /// configured and never joins a composite.
    ///
    /// # Errors
    /// Returns [`ModelError::Construction`] when the configured provider
    /// rejects its configuration or a selected subscription is not connected.
    pub fn from_settings_with_app(
        settings: ModelSettings,
        app: &crate::AppState,
    ) -> Result<Self, ModelError> {
        if matches!(
            settings.provider(),
            ModelProvider::Codex | ModelProvider::ClaudeCode
        ) {
            let engine = Arc::new(SubscriptionEngine::build(&settings, app)?);
            return Ok(Self::wrap_engine(settings, engine, RouteShape::default()));
        }

        let api = Arc::new(
            AgentRuntimeEngine::build(&settings)?.with_cooldowns(app.model_cooldowns().clone()),
        );
        if !settings.prefer_subscription() {
            return Ok(Self::wrap_engine(settings, api, RouteShape::default()));
        }
        let connected = app
            .subscription_auth()
            .connected(SubscriptionProvider::Codex);
        let mut shape = RouteShape {
            preferred: None,
            chatgpt_connected_at_build: Some(connected),
        };
        if !connected {
            return Ok(Self::wrap_engine(settings, api, shape));
        }
        let subscription = match SubscriptionEngine::build_chatgpt(settings.chatgpt_model(), app) {
            Ok(subscription) => Arc::new(subscription),
            Err(error) => {
                tracing::warn!(
                    %error,
                    provider = settings.provider().as_str(),
                    "ChatGPT subscription is connected but cannot be used; continuing with the configured provider"
                );
                return Ok(Self::wrap_engine(settings, api, shape));
            }
        };
        let gate = SubscriptionGate::chatgpt(app.subscription_auth().clone());
        let composite = PreferredEngine::new(
            subscription,
            settings.chatgpt_model(),
            gate.clone(),
            api,
            settings.tiers().clone(),
            app.model_cooldowns().clone(),
        );
        shape.preferred = Some(PreferredLeg {
            model: settings.chatgpt_model().to_owned(),
            gate,
        });
        tracing::info!(
            model = %chatgpt_label(settings.chatgpt_model()),
            fallback_provider = settings.provider().as_str(),
            "ChatGPT subscription is the preferred model route"
        );
        Ok(Self::wrap_engine(settings, Arc::new(composite), shape))
    }

    /// Wraps the route in the call budget exactly once.
    fn wrap_engine(
        settings: ModelSettings,
        engine: Arc<dyn DecisionEngine>,
        shape: RouteShape,
    ) -> Self {
        let budget = Arc::new(BudgetTracker::new(*settings.budget()));
        let engine = Arc::new(BudgetedEngine::new(engine, budget.clone()));
        Self {
            provider: settings.provider(),
            engine,
            budget,
            tiers: settings.tiers().clone(),
            preferred: shape.preferred,
            chatgpt_connected_at_build: shape.chatgpt_connected_at_build,
            settings: Some(settings),
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

    /// Every candidate currently in force for a tier, in the order they are
    /// tried, labelled as `/status` reports them: the ChatGPT subscription
    /// (`chatgpt:<model>`) first while it is preferred and connected, then the
    /// configured chain. Cooling candidates are included; they are listed
    /// separately by the cooldown registry.
    pub fn route(&self, tier: ModelTier) -> Vec<String> {
        let mut route = Vec::new();
        if let Some(preferred) = &self.preferred
            && preferred.gate.ready()
        {
            route.push(chatgpt_label(&preferred.model));
        }
        let chain = self.tiers.chain(tier);
        if self.provider == ModelProvider::Codex {
            route.extend(chain.iter().map(|model| chatgpt_label(model)));
        } else {
            route.extend(chain.iter().cloned());
        }
        route
    }

    /// Whether the ChatGPT subscription leg was built into this runtime.
    pub fn prefers_subscription(&self) -> bool {
        self.preferred.is_some()
    }

    /// Settings the runtime was built from, when it was built from settings.
    pub fn settings(&self) -> Option<&ModelSettings> {
        self.settings.as_ref()
    }

    /// Whether the ChatGPT connection has changed since this runtime was
    /// built while the subscription preference applies — a connect, a
    /// disconnect, or a refresh that left the subscription needing to be
    /// reconnected. A stale runtime must be rebuilt to put its route back in
    /// step with the connection.
    pub fn subscription_route_stale(&self, auth: &SubscriptionAuthState) -> bool {
        self.chatgpt_connected_at_build
            .is_some_and(|at_build| at_build != auth.connected(SubscriptionProvider::Codex))
    }

    /// Whether two runtimes route the same candidates with the same
    /// credential, so cooldowns recorded against one remain true of the other.
    pub fn same_route(&self, other: &Self) -> bool {
        let same_settings = match (&self.settings, &other.settings) {
            (Some(left), Some(right)) => left.same_route(right),
            _ => false,
        };
        same_settings
            && self.preferred.as_ref().map(|leg| &leg.model)
                == other.preferred.as_ref().map(|leg| &leg.model)
            && self.chatgpt_connected_at_build == other.chatgpt_connected_at_build
    }

    /// Whether both handles share the same engine instance.
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.engine, &other.engine)
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
            settings: None,
            preferred: None,
            chatgpt_connected_at_build: None,
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

    use crate::config::ConfigError;
    use crate::model::subscription_engine::test_support::{app, credential};

    const API_ROUTE: [&str; 3] = [
        "deepseek/deepseek-v4.1-flash",
        "z-ai/glm-5.3-flash",
        "xiaomi/mimo-v2.6-flash",
    ];

    fn settings(overrides: &[(&'static str, &'static str)]) -> ModelSettings {
        let base: [(&'static str, &'static str); 6] = [
            ("VEYRA_MODEL_PROVIDER", "openrouter"),
            ("VEYRA_MODEL_API_KEY", "test-key-12345678"),
            ("VEYRA_MODEL_FAST", API_ROUTE[0]),
            ("VEYRA_MODEL_BALANCED", API_ROUTE[0]),
            ("VEYRA_MODEL_REASONING", API_ROUTE[0]),
            (
                "VEYRA_MODEL_FALLBACKS",
                "z-ai/glm-5.3-flash,xiaomi/mimo-v2.6-flash",
            ),
        ];
        let overrides = overrides.to_vec();
        ModelSettings::from_source(move |name| {
            overrides
                .iter()
                .chain(base.iter())
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(ConfigError::MissingEnvironmentVariable { name })
        })
        .expect("settings parse")
        .expect("configured")
    }

    #[actix_web::test]
    async fn a_connected_chatgpt_subscription_leads_every_route() {
        let (state, _store) = app(CooldownRegistry::new(), &[SubscriptionProvider::Codex]);
        let runtime = ModelRuntime::from_settings_with_app(settings(&[]), &state)
            .expect("the composite builds");
        assert!(runtime.prefers_subscription());
        assert_eq!(runtime.provider(), ModelProvider::OpenRouter);
        assert_eq!(runtime.engine().provider(), ModelProvider::OpenRouter);
        for tier in [ModelTier::Fast, ModelTier::Balanced, ModelTier::Reasoning] {
            let mut expected = vec!["chatgpt:gpt-6-luna".to_owned()];
            expected.extend(API_ROUTE.iter().map(|model| (*model).to_owned()));
            assert_eq!(runtime.route(tier), expected, "{tier}");
            assert_eq!(runtime.chain(tier), API_ROUTE, "the API chain is unchanged");
        }
        assert!(!runtime.subscription_route_stale(state.subscription_auth()));
        assert!(runtime.settings().is_some());

        let custom = ModelRuntime::from_settings_with_app(
            settings(&[("VEYRA_MODEL_CHATGPT_MODEL", "gpt-5.6-terra")]),
            &state,
        )
        .expect("builds");
        assert_eq!(custom.route(ModelTier::Fast)[0], "chatgpt:gpt-5.6-terra");
        assert!(
            !runtime.same_route(&custom),
            "a different ChatGPT model is a new route"
        );
        assert!(runtime.same_route(&runtime.clone()));
        assert!(runtime.same_instance(&runtime.clone()));
        assert!(!runtime.same_instance(&custom));
    }

    #[actix_web::test]
    async fn without_preference_connection_or_storage_the_configured_chain_stands() {
        let (state, _store) = app(CooldownRegistry::new(), &[SubscriptionProvider::Codex]);
        let off = ModelRuntime::from_settings_with_app(
            settings(&[("VEYRA_MODEL_PREFER_SUBSCRIPTION", "false")]),
            &state,
        )
        .expect("builds");
        assert!(!off.prefers_subscription());
        assert_eq!(off.route(ModelTier::Fast), API_ROUTE);
        assert!(!off.subscription_route_stale(state.subscription_auth()));

        let (disconnected, _store) = app(CooldownRegistry::new(), &[]);
        let plain =
            ModelRuntime::from_settings_with_app(settings(&[]), &disconnected).expect("builds");
        assert!(!plain.prefers_subscription());
        assert_eq!(plain.route(ModelTier::Fast), API_ROUTE);

        // Connected, but no encrypted storage: the leg is left out rather
        // than disabling the configured provider, and it is not retried on
        // every read.
        let unstored = crate::AppState::new(
            crate::model::subscription_engine::test_support::config(),
            None,
            None,
            crate::risk::RiskGate::new(crate::risk::RiskPolicy::default()),
        );
        unstored
            .subscription_auth()
            .set_credential(credential(SubscriptionProvider::Codex, None));
        let degraded =
            ModelRuntime::from_settings_with_app(settings(&[]), &unstored).expect("builds");
        assert!(!degraded.prefers_subscription());
        assert!(!degraded.subscription_route_stale(unstored.subscription_auth()));

        let detached = ModelRuntime::from_settings(settings(&[])).expect("builds");
        assert_eq!(detached.route(ModelTier::Balanced), API_ROUTE);
        assert!(!detached.subscription_route_stale(state.subscription_auth()));
    }

    #[actix_web::test]
    async fn a_codex_provider_is_not_duplicated_and_claude_code_never_joins() {
        let (state, _store) = app(
            CooldownRegistry::new(),
            &[
                SubscriptionProvider::Codex,
                SubscriptionProvider::ClaudeCode,
            ],
        );
        let codex = ModelRuntime::from_settings_with_app(
            settings(&[
                ("VEYRA_MODEL_PROVIDER", "codex"),
                ("VEYRA_MODEL_FAST", "gpt-6-luna"),
                ("VEYRA_MODEL_BALANCED", "gpt-6-luna"),
                ("VEYRA_MODEL_REASONING", "gpt-6-luna"),
                ("VEYRA_MODEL_FALLBACKS", ""),
            ]),
            &state,
        )
        .expect("the selected subscription builds");
        assert!(!codex.prefers_subscription());
        assert_eq!(codex.provider(), ModelProvider::Codex);
        assert_eq!(codex.route(ModelTier::Fast), ["chatgpt:gpt-6-luna"]);

        let claude = ModelRuntime::from_settings_with_app(
            settings(&[
                ("VEYRA_MODEL_PROVIDER", "claude_code"),
                ("VEYRA_MODEL_FAST", "claude-model"),
                ("VEYRA_MODEL_BALANCED", "claude-model"),
                ("VEYRA_MODEL_REASONING", "claude-model"),
                ("VEYRA_MODEL_FALLBACKS", ""),
            ]),
            &state,
        )
        .expect("the selected subscription builds");
        assert!(!claude.prefers_subscription());
        assert_eq!(claude.provider(), ModelProvider::ClaudeCode);
        assert_eq!(claude.route(ModelTier::Fast), ["claude-model"]);
        assert!(!claude.subscription_route_stale(state.subscription_auth()));

        assert!(
            ModelRuntime::from_settings(settings(&[("VEYRA_MODEL_PROVIDER", "codex")])).is_err(),
            "a subscription needs its encrypted connection"
        );
    }

    #[actix_web::test]
    async fn the_model_route_follows_the_chatgpt_connection() {
        let (state, _store) = app(CooldownRegistry::new(), &[SubscriptionProvider::Codex]);
        let runtime = ModelRuntime::from_settings_with_app(settings(&[]), &state).expect("builds");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs();
        runtime
            .restore_state(&serde_json::json!({
                "hourStart": now, "hourCalls": 3, "dayStart": now, "dayCalls": 7
            }))
            .expect("budget snapshot restores");
        state.set_model(Some(runtime.clone()));
        assert!(
            state
                .model()
                .is_some_and(|current| current.same_instance(&runtime))
        );

        // A rejected refresh: the leg drops out live, and the next read
        // rebuilds the runtime without it, carrying the call budget across.
        state
            .subscription_auth()
            .mark_needs_reconnect(SubscriptionProvider::Codex);
        assert_eq!(runtime.route(ModelTier::Fast), API_ROUTE, "gated live");
        assert!(runtime.subscription_route_stale(state.subscription_auth()));
        let rebuilt = state.model().expect("still configured");
        assert!(!rebuilt.same_instance(&runtime));
        assert!(!rebuilt.prefers_subscription());
        assert_eq!(rebuilt.budget().hour_calls, 3);
        assert_eq!(rebuilt.budget().day_calls, 7);
        assert!(
            state
                .model()
                .is_some_and(|current| current.same_instance(&rebuilt)),
            "a fresh route is not rebuilt again"
        );

        // Reconnecting brings the subscription back to the front.
        state
            .subscription_auth()
            .set_credential(credential(SubscriptionProvider::Codex, None));
        let restored = state.model().expect("configured");
        assert!(restored.prefers_subscription());
        assert_eq!(restored.route(ModelTier::Fast)[0], "chatgpt:gpt-6-luna");

        // A disabled model stays disabled whatever the connection does.
        state.set_model(None);
        state
            .subscription_auth()
            .remove_credential(SubscriptionProvider::Codex);
        assert!(state.model().is_none());

        // Injected test engines carry no settings and are never rebuilt.
        let injected = ModelRuntime::with_engine(
            ModelProvider::OpenRouter,
            Arc::new(crate::model::budget::BudgetedEngine::new(
                Arc::new(AgentRuntimeEngine::build(&settings(&[])).expect("builds")),
                Arc::new(BudgetTracker::new(BudgetPolicy::default())),
            )),
        );
        assert!(!injected.same_route(&restored));
        assert!(!injected.subscription_route_stale(state.subscription_auth()));
    }

    #[test]
    fn chatgpt_labels_and_new_provider_names_are_stable() {
        assert_eq!(chatgpt_label("gpt-6-luna"), "chatgpt:gpt-6-luna");
        assert_eq!(ModelProvider::Zai.as_str(), "zai");
        assert_eq!(ModelProvider::Zai.to_string(), "zai");
        assert_eq!(ModelProvider::parse("zai"), Some(ModelProvider::Zai));
    }
}
