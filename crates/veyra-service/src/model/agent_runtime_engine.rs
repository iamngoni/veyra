//! `agent-runtime`-backed implementation of [`DecisionEngine`].
//!
//! This adapter owns every `agent-runtime` type, so the rest of the service
//! never imports the dependency. Providers are chosen through
//! `AgentProviderKind`; models come from explicitly configured tiers, never
//! from library defaults (which are dated).
//!
//! Each attempt targets one explicit candidate; the ordered chain, failover,
//! and per-model cooldowns are decided by [`crate::model::route`], so this
//! engine behaves exactly like every other leg of a composite route.

use std::fmt;
use std::sync::Arc;

use agent_runtime::{
    Agent as RuntimeAgent, AgentProviderKind, ChatMessage, EventSink, Llm, ModelTiers,
    ProviderError, ResponseFormat, RetryPolicy, RuntimeEvent, Tool, ToolCall, ToolDefinition,
    ToolOutput, ToolRegistry, ToolSessionOutcome,
};
use async_trait::async_trait;
use serde_json::json;

use crate::model::cooldown::{CooldownFailure, CooldownReason, CooldownRegistry};
use crate::model::route::{self, AttemptError, Call, CandidateTransport, Leg, Telemetry};
use crate::model::settings::{ModelSettings, TierModels};
use crate::model::{
    DecisionAnswer, DecisionEngine, DecisionRequest, ModelError, ModelProvider, ModelTier,
    ReadOnlyTool, ReadOnlyToolDefinition, ToolProgressSink,
};

/// Maximum serialized observation returned to a model-directed tool session.
/// This bounds prompt growth when a broker reports a large position book or
/// an audit backend returns unexpectedly verbose payloads.
const MAX_TOOL_RESULT_CHARS: usize = 12_000;

/// Structured-decision adapter over `agent-runtime`.
pub struct AgentRuntimeEngine {
    llm: Llm,
    provider: ModelProvider,
    /// Ordered candidates per tier. The library's own tier resolution only
    /// knows the primary, so the chain is kept here and the model is passed
    /// explicitly on each attempt.
    tiers: TierModels,
    /// Last candidate requested and last one that answered.
    telemetry: Telemetry,
    /// Per-candidate cooldowns; shared with the application when built by
    /// the runtime so they survive engine rebuilds.
    cooldowns: CooldownRegistry,
}

impl fmt::Debug for AgentRuntimeEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRuntimeEngine")
            .field("provider", &self.provider)
            .finish()
    }
}

/// Minimal agent: instructions and one explicit model, no tools.
struct InlineAgent {
    instructions: String,
    model: String,
}

/// Adapts a provider-neutral observation tool to agent-runtime's tool trait.
/// Tool failures become bounded JSON observations so the model can explain
/// stale or unavailable state without retrying through an execution path.
pub(crate) struct RuntimeReadOnlyTool {
    pub(crate) tool: Arc<dyn ReadOnlyTool>,
    pub(crate) definition: ToolDefinition,
}

#[async_trait]
impl Tool<()> for RuntimeReadOnlyTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(&self, _context: (), call: &ToolCall) -> anyhow::Result<ToolOutput> {
        let content = match self.tool.execute(call.arguments.clone()).await {
            Ok(value) => value,
            Err(reason) => json!({ "available": false, "reason": reason }),
        };
        let content = match serde_json::to_string(&content) {
            Ok(serialized) if serialized.chars().count() <= MAX_TOOL_RESULT_CHARS => content,
            Ok(_) => json!({
                "available": false,
                "reason": "tool_result_too_large",
                "max_chars": MAX_TOOL_RESULT_CHARS,
            }),
            Err(_) => json!({
                "available": false,
                "reason": "tool_result_not_serializable",
            }),
        };
        Ok(ToolOutput { content })
    }
}

/// Bridges agent-runtime lifecycle events to the service's SSE-facing sink.
pub(crate) struct RuntimeProgress<'a> {
    pub(crate) sink: &'a mut dyn ToolProgressSink,
}

#[async_trait]
impl EventSink for RuntimeProgress<'_> {
    async fn emit(&mut self, event: RuntimeEvent) -> anyhow::Result<()> {
        match event {
            RuntimeEvent::ToolStarted { call } => {
                self.sink
                    .tool_started(&call.id, &call.name, &call.arguments)
                    .await;
            }
            RuntimeEvent::ToolCompleted { call, output } => {
                let available = output
                    .content
                    .get("available")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                self.sink
                    .tool_completed(&call.id, &call.name, &output.content, available)
                    .await;
            }
            RuntimeEvent::AssistantStarted { .. } | RuntimeEvent::AssistantDelta { .. } => {}
        }
        Ok(())
    }
}

pub(crate) fn runtime_definition(definition: ReadOnlyToolDefinition) -> ToolDefinition {
    ToolDefinition {
        name: definition.name,
        description: definition.description,
        input_schema: definition.input_schema,
    }
}

/// Builds the allowlisted tool registry for one tool-session attempt.
pub(crate) fn runtime_registry(tools: &[Arc<dyn ReadOnlyTool>]) -> ToolRegistry<()> {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        let definition = runtime_definition(tool.definition());
        registry.register(RuntimeReadOnlyTool {
            tool: tool.clone(),
            definition,
        });
    }
    registry
}

impl RuntimeAgent for InlineAgent {
    fn instructions(&self) -> String {
        self.instructions.clone()
    }

    fn model(&self) -> String {
        self.model.clone()
    }
}

impl AgentRuntimeEngine {
    /// Builds the engine with a bounded retry policy for transient failures.
    ///
    /// # Errors
    /// Returns [`ModelError::Construction`] when the provider rejects its
    /// configuration.
    pub fn build(settings: &ModelSettings) -> Result<Self, ModelError> {
        Self::build_with_client(settings, None)
    }

    /// Builds the engine with an injected HTTP client (used by tests and
    /// future proxying); production uses the shared reqwest client.
    ///
    /// # Errors
    /// Returns [`ModelError::Construction`] when the provider rejects its
    /// configuration.
    pub fn build_with_client(
        settings: &ModelSettings,
        client: Option<agent_runtime::SharedHttpClient>,
    ) -> Result<Self, ModelError> {
        let kind = match settings.provider() {
            ModelProvider::Codex | ModelProvider::ClaudeCode => {
                return Err(ModelError::Construction {
                    reason: "subscription provider requires subscription runtime".to_owned(),
                });
            }
            ModelProvider::OpenAi => AgentProviderKind::OpenAi,
            ModelProvider::Anthropic => AgentProviderKind::Anthropic,
            ModelProvider::OpenRouter => AgentProviderKind::OpenRouter,
            ModelProvider::Groq => AgentProviderKind::Groq,
            ModelProvider::DeepSeek => AgentProviderKind::DeepSeek,
            ModelProvider::Xai => AgentProviderKind::Xai,
            ModelProvider::Mistral => AgentProviderKind::Mistral,
            ModelProvider::Kimi => AgentProviderKind::Kimi,
            // Z.AI speaks the OpenAI wire format, so it rides the compatible
            // client under its own name with its documented default endpoint.
            ModelProvider::Zai => AgentProviderKind::Custom("zai".to_owned()),
            ModelProvider::Ollama => AgentProviderKind::Ollama,
            ModelProvider::Custom => AgentProviderKind::Custom("custom".to_owned()),
        };

        let mut builder = Llm::builder()
            .provider(kind)
            // agent-runtime requires a string at construction time. Ollama
            // ignores this header and is the only provider accepted without a
            // configured key; hosted providers were checked by settings.
            .api_key(settings.api_key().map_or("", |key| key.expose()));

        if let Some(base_url) = settings.base_url() {
            builder = builder.base_url(base_url);
        } else if settings.provider() == ModelProvider::Zai {
            builder = builder.base_url(crate::model::settings::ZAI_DEFAULT_BASE_URL);
        }

        builder = builder
            .model_tiers(ModelTiers::new(
                settings.tiers().resolve(ModelTier::Balanced),
                settings.tiers().resolve(ModelTier::Fast),
                settings.tiers().resolve(ModelTier::Reasoning),
            ))
            .retry(RetryPolicy::with_retries(2))
            // A reasoning model refuses a compelled tool choice outright, so
            // this is the difference between the loop working and every
            // request failing — not a preference about answer strictness.
            // Z.AI rejects any `tool_choice` other than "auto", so it is
            // never compelled whatever VEYRA_MODEL_COMPEL_STRUCTURED says.
            .structured_strategy(if settings.compels_structured_answer_on_wire() {
                agent_runtime::StructuredStrategy::ForcedTool
            } else {
                agent_runtime::StructuredStrategy::AutoTool
            })
            .verbose(false);

        // Attribution is keyed on the URL: the provider creates no app entry
        // without it, so the title and visibility headers only carry meaning
        // when it is present.
        if settings.provider() == ModelProvider::OpenRouter
            && let Some(referer) = settings.http_referer()
        {
            builder = builder.header("HTTP-Referer", referer);
            if let Some(title) = settings.app_title() {
                builder = builder.header("X-OpenRouter-Title", title);
            }
            if settings.app_hidden() {
                builder = builder.header("X-OpenRouter-App-Visibility", "hidden");
            }
        }
        if let Some(client) = client {
            builder = builder.http_client(client);
        }

        let llm = builder.build().map_err(|error| ModelError::Construction {
            reason: error.to_string(),
        })?;

        Ok(Self {
            llm,
            provider: settings.provider(),
            tiers: settings.tiers().clone(),
            telemetry: Telemetry::default(),
            cooldowns: CooldownRegistry::new(),
        })
    }

    /// Uses a shared cooldown registry instead of a private one, so cooldowns
    /// survive engine rebuilds.
    #[must_use]
    pub fn with_cooldowns(mut self, cooldowns: CooldownRegistry) -> Self {
        self.cooldowns = cooldowns;
        self
    }

    fn legs(&self, tier: ModelTier) -> [Leg<'_>; 1] {
        [Leg {
            transport: self,
            models: self.tiers.chain(tier),
        }]
    }
}

/// Classifies a failed attempt: whether another model is worth trying, and
/// for how long this one should cool down.
///
/// The question is never "was this request valid" but "could a different model
/// answer it". An exhausted balance, a rate limit, an overloaded upstream, or a
/// flat rejection are all properties of the model that was asked, so it cools
/// down and the next candidate gets a turn. A transport fault is the
/// exception: the provider was never reached, so the same failure would repeat
/// on every candidate this host serves. The route holds the whole host briefly
/// and moves on to a different host, if there is one.
///
/// Rate-limit hints come from the provider error body; `agent-runtime` does not
/// surface response headers.
fn classify(error: &anyhow::Error) -> AttemptError {
    let reason = safe_failure_reason(error);
    let failure = match provider_error(error) {
        Some(ProviderError::Transport { .. }) => return AttemptError::transport(reason),
        Some(ProviderError::RateLimited { retry_after, .. }) => {
            CooldownFailure::rate_limited(*retry_after)
        }
        Some(ProviderError::InsufficientCredits { .. }) => {
            CooldownFailure::new(CooldownReason::InsufficientCredits)
        }
        Some(ProviderError::Overloaded { .. }) => CooldownFailure::new(CooldownReason::Overloaded),
        Some(ProviderError::Status { status, .. }) => CooldownFailure::from_status(*status, None),
        // An empty answer, or not a provider error at all — most often the
        // answer came back but did not satisfy the schema. Another model may
        // well comply.
        Some(ProviderError::EmptyResponse { .. }) | None => {
            CooldownFailure::new(CooldownReason::InvalidResponse)
        }
    };
    AttemptError::cooldown(reason, failure)
}

/// Finds the classified provider error beneath any context added by the
/// runtime library.
fn provider_error(error: &anyhow::Error) -> Option<&ProviderError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProviderError>())
}

/// Converts a provider failure into a bounded, non-sensitive diagnostic.
///
/// Provider bodies can contain account identifiers, routing details, or large
/// nested JSON payloads. The candidate model and category are enough to
/// explain failover on the Veyra surface; the raw body must not cross that
/// boundary.
fn safe_failure_reason(error: &anyhow::Error) -> String {
    match provider_error(error) {
        Some(ProviderError::RateLimited { .. }) => "rate_limited".to_owned(),
        Some(ProviderError::InsufficientCredits { body, .. }) => match credit_source(body) {
            Some(source) => format!("insufficient_credits ({source})"),
            None => "insufficient_credits".to_owned(),
        },
        Some(ProviderError::Overloaded { .. }) => "overloaded".to_owned(),
        Some(ProviderError::Status { status, .. }) => format!("provider_rejected ({status})"),
        Some(ProviderError::Transport { .. }) => "transport".to_owned(),
        Some(ProviderError::EmptyResponse { .. }) => "empty_response".to_owned(),
        None => "invalid_response".to_owned(),
    }
}

/// Names whose balance ran out, from OpenRouter's structured 402 metadata.
///
/// OpenRouter distinguishes an empty balance at the upstream provider behind
/// a bring-your-own-key route (`is_byok` plus `provider_name`) from its own
/// account credits (`limit_source: "openrouter_credits"`). Only those fields
/// are read — never the message or the upstream's raw body — and the provider
/// name must look like a name, so nothing else from the body can cross into
/// status output or logs.
fn credit_source(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let metadata = value.pointer("/error/metadata")?;
    if metadata.get("is_byok").and_then(serde_json::Value::as_bool) == Some(true) {
        let name = metadata
            .get("provider_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| looks_like_a_name(name));
        return Some(match name {
            Some(name) => format!("{name} balance, BYOK"),
            None => "provider balance, BYOK".to_owned(),
        });
    }
    (metadata
        .get("limit_source")
        .and_then(serde_json::Value::as_str)
        == Some("openrouter_credits"))
    .then(|| "OpenRouter credits".to_owned())
}

/// A short provider display name: letters, digits, spaces, `.`, `-`, `_`.
fn looks_like_a_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.chars().count() <= 40
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || " .-_".contains(character))
}

#[async_trait]
impl CandidateTransport for AgentRuntimeEngine {
    fn candidate_provider(&self) -> ModelProvider {
        self.provider
    }

    async fn attempt(
        &self,
        model: &str,
        request: &DecisionRequest,
        call: &mut Call<'_>,
    ) -> Result<DecisionAnswer, AttemptError> {
        match call {
            Call::Structured => {
                let agent = InlineAgent {
                    instructions: request.instructions.clone(),
                    model: model.to_owned(),
                };
                let format =
                    ResponseFormat::new(request.format.name.clone(), request.format.schema.clone());
                self.llm
                    .run_structured_with_format(&agent, request.input.clone(), format)
                    .await
                    .map(|value| DecisionAnswer { value })
                    .map_err(|error| classify(&error))
            }
            Call::Tools { tools, progress } => {
                let registry = runtime_registry(tools);
                let mut runtime_progress = RuntimeProgress {
                    sink: &mut **progress,
                };
                let history = [ChatMessage::user(request.input.clone())];
                let outcome = self
                    .llm
                    .execute_tool_session(
                        agent_runtime::ToolSessionRequest {
                            model,
                            decision_system_prompt: &request.instructions,
                            followup_system_prompt: &request.instructions,
                            history: &history,
                            tool_registry: &registry,
                            tool_context: (),
                            max_tool_calls: 8,
                        },
                        &mut runtime_progress,
                    )
                    .await
                    .map_err(|error| classify(&error))?;
                let message = match outcome {
                    ToolSessionOutcome::Direct { message, .. }
                    | ToolSessionOutcome::ToolBacked { message, .. } => message,
                };
                if message.trim().is_empty() {
                    return Err(AttemptError::cooldown(
                        "empty_response",
                        CooldownFailure::new(CooldownReason::InvalidResponse),
                    ));
                }
                Ok(DecisionAnswer {
                    value: json!({ "answer": message }),
                })
            }
        }
    }
}

#[async_trait]
impl DecisionEngine for AgentRuntimeEngine {
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
}

#[cfg(test)]
mod tests {
    // Asserts the wire shape rather than the helper, because the whole point
    // of the switch is what the provider receives.
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use agent_runtime::{
        HttpClient, HttpRequest, HttpResponse, HttpStreamResponse, SharedHttpClient,
    };
    use serde_json::json;

    use super::*;
    use crate::config::ConfigError;
    use crate::model::AnswerFormat;
    use crate::model::cooldown::test_clock::ManualClock;

    #[derive(Default)]
    struct QueueClient {
        responses: Mutex<VecDeque<(u16, String)>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl QueueClient {
        fn push(&self, status: u16, body: impl Into<String>) {
            self.responses
                .lock()
                .expect("mock mutex")
                .push_back((status, body.into()));
        }

        fn last_body(&self) -> String {
            let requests = self.requests.lock().expect("mock mutex");
            let request = requests.last().expect("a request must have been sent");
            String::from_utf8(request.body.clone()).expect("request body is UTF-8")
        }
    }

    #[async_trait]
    impl HttpClient for QueueClient {
        async fn send(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
            self.requests.lock().expect("mock mutex").push(request);
            let (status, body) = self
                .responses
                .lock()
                .expect("mock mutex")
                .pop_front()
                .expect("mock response queued");
            Ok(HttpResponse {
                status,
                body: body.into_bytes(),
            })
        }

        async fn send_streaming(
            &self,
            _request: HttpRequest,
        ) -> anyhow::Result<HttpStreamResponse> {
            anyhow::bail!("the structured engine never streams")
        }
    }

    fn settings() -> ModelSettings {
        ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "openrouter",
                "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                "VEYRA_MODEL_FAST" => "vendor/fast",
                "VEYRA_MODEL_BALANCED" => "vendor/balanced",
                "VEYRA_MODEL_REASONING" => "vendor/reasoning",
                _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("settings parse")
        .expect("model configured")
    }

    /// Settings with the compel switch set explicitly.
    fn settings_compelling(compel: bool) -> ModelSettings {
        ModelSettings::from_source(move |name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "openrouter",
                "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                "VEYRA_MODEL_FAST" => "vendor/fast",
                "VEYRA_MODEL_BALANCED" => "vendor/balanced",
                "VEYRA_MODEL_REASONING" => "vendor/reasoning",
                "VEYRA_MODEL_COMPEL_STRUCTURED" => {
                    if compel {
                        "true"
                    } else {
                        "false"
                    }
                }
                _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("settings parse")
        .expect("model configured")
    }

    fn request(tier: ModelTier) -> DecisionRequest {
        DecisionRequest {
            instructions: "Classify the bias.".to_owned(),
            input: "EURUSD closed above its average.".to_owned(),
            format: AnswerFormat {
                name: "bias".to_owned(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "bias": {"type": "string", "enum": ["bullish", "bearish"]},
                        "confidence": {"type": "number"}
                    },
                    "required": ["bias", "confidence"]
                }),
            },
            tier,
        }
    }

    fn structured_response(name: &str, arguments: &str) -> String {
        json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                }
            }]
        })
        .to_string()
    }

    #[actix_web::test]
    async fn answer_sends_caller_schema_and_parses_payload() {
        let mock = Arc::new(QueueClient::default());
        mock.push(
            200,
            structured_response("bias", "{\"bias\":\"bullish\",\"confidence\":0.7}"),
        );

        let engine = AgentRuntimeEngine::build_with_client(
            &settings(),
            Some(mock.clone() as SharedHttpClient),
        )
        .expect("engine builds");
        assert_eq!(engine.provider(), ModelProvider::OpenRouter);

        let answer = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("answer");
        assert_eq!(answer.value["bias"], "bullish");
        assert_eq!(answer.value["confidence"], 0.7);

        let sent = mock.last_body();
        assert!(
            sent.contains("\"enum\":[\"bullish\",\"bearish\"]"),
            "caller schema must be sent: {sent}"
        );
        assert!(
            sent.contains("vendor/fast"),
            "requested tier's model must be sent: {sent}"
        );
    }

    #[actix_web::test]
    async fn the_compel_switch_decides_the_tool_choice_on_the_wire() {
        // Compelled: the provider is told it must answer through the channel.
        let mock = Arc::new(QueueClient::default());
        mock.push(200, structured_response("bias", "{\"bias\":\"bullish\"}"));
        let engine = AgentRuntimeEngine::build_with_client(
            &settings_compelling(true),
            Some(mock.clone() as SharedHttpClient),
        )
        .expect("engine builds");
        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("answer");
        // Parsed, not matched as text: the body serialises keys alphabetically
        // so a substring assertion would pin field order rather than meaning.
        let sent: serde_json::Value =
            serde_json::from_str(&mock.last_body()).expect("request body is JSON");
        assert_eq!(
            sent["tool_choice"],
            json!({ "type": "function", "function": { "name": "bias" } }),
            "a compelled choice must name the function"
        );

        // Not compelled: reasoning models reject the compelled form outright.
        let mock = Arc::new(QueueClient::default());
        mock.push(200, structured_response("bias", "{\"bias\":\"bearish\"}"));
        let engine = AgentRuntimeEngine::build_with_client(
            &settings_compelling(false),
            Some(mock.clone() as SharedHttpClient),
        )
        .expect("engine builds");
        let answer = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("answer");
        assert_eq!(answer.value["bias"], "bearish");
        let sent: serde_json::Value =
            serde_json::from_str(&mock.last_body()).expect("request body is JSON");
        assert_eq!(sent["tool_choice"], json!("auto"));
        // The schema still travels, so the answer shape is still requested.
        assert_eq!(
            sent["tools"][0]["function"]["parameters"]["properties"]["bias"]["enum"],
            json!(["bullish", "bearish"])
        );
    }

    #[actix_web::test]
    async fn balanced_and_reasoning_tiers_choose_configured_models() {
        for (tier, expected) in [
            (ModelTier::Balanced, "vendor/balanced"),
            (ModelTier::Reasoning, "vendor/reasoning"),
        ] {
            let mock = Arc::new(QueueClient::default());
            mock.push(
                200,
                structured_response("bias", "{\"bias\":\"bearish\",\"confidence\":0.4}"),
            );
            let engine = AgentRuntimeEngine::build_with_client(
                &settings(),
                Some(mock.clone() as SharedHttpClient),
            )
            .expect("engine builds");

            let answer = engine.answer(request(tier)).await.expect("answer");
            assert_eq!(answer.value["bias"], "bearish");
            assert!(
                mock.last_body().contains(expected),
                "tier {tier:?} must resolve to {expected}"
            );
        }
    }

    #[actix_web::test]
    async fn optional_settings_paths_build_and_debug_stays_redacted() {
        let settings = ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "openrouter",
                "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                "VEYRA_MODEL_BASE_URL" => "https://example.test/v1",
                "VEYRA_MODEL_HTTP_REFERER" => "https://github.com/iamngoni/veyra",
                "VEYRA_MODEL_FAST" | "VEYRA_MODEL_BALANCED" | "VEYRA_MODEL_REASONING" => {
                    "vendor/model"
                }
                _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("settings parse")
        .expect("model configured");

        let engine = AgentRuntimeEngine::build(&settings).expect("engine builds");
        let debug = format!("{engine:?}");
        assert!(debug.contains("AgentRuntimeEngine"), "debug: {debug}");
        assert!(debug.contains("OpenRouter"), "debug: {debug}");
        assert!(!debug.contains("test-key-12345678"), "debug: {debug}");
    }

    #[test]
    fn provider_selection_maps_to_agent_runtime_without_vendor_model_fallbacks() {
        for (raw, expected) in [
            ("openai", AgentProviderKind::OpenAi),
            ("anthropic", AgentProviderKind::Anthropic),
            ("openrouter", AgentProviderKind::OpenRouter),
            ("groq", AgentProviderKind::Groq),
            ("deepseek", AgentProviderKind::DeepSeek),
            ("xai", AgentProviderKind::Xai),
            ("mistral", AgentProviderKind::Mistral),
            ("kimi", AgentProviderKind::Kimi),
            ("ollama", AgentProviderKind::Ollama),
        ] {
            let (fast, balanced, reasoning) = if raw == "openrouter" {
                (
                    "vendor/native-fast",
                    "vendor/native-balanced",
                    "vendor/native-reasoning",
                )
            } else {
                ("native-fast", "native-balanced", "native-reasoning")
            };
            let settings = ModelSettings::from_source(|name| {
                Ok(match name {
                    "VEYRA_MODEL_PROVIDER" => raw,
                    "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                    "VEYRA_MODEL_FAST" => fast,
                    "VEYRA_MODEL_BALANCED" => balanced,
                    "VEYRA_MODEL_REASONING" => reasoning,
                    _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
                }
                .to_owned())
            })
            .expect("settings parse")
            .expect("model configured");
            let engine =
                AgentRuntimeEngine::build_with_client(&settings, None).expect("engine builds");
            assert_eq!(engine.llm.provider_kind(), expected);
        }

        let settings = ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "custom",
                "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                "VEYRA_MODEL_BASE_URL" => "https://llm.example.test/v1",
                "VEYRA_MODEL_FAST" | "VEYRA_MODEL_BALANCED" | "VEYRA_MODEL_REASONING" => {
                    "native-model"
                }
                _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("settings parse")
        .expect("model configured");
        let engine = AgentRuntimeEngine::build_with_client(&settings, None).expect("build");
        assert_eq!(
            engine.llm.provider_kind(),
            AgentProviderKind::Custom("custom".to_owned())
        );
    }

    #[actix_web::test]
    async fn streaming_stub_refuses_instead_of_faking_a_stream() {
        let mock = QueueClient::default();
        // `HttpStreamResponse` is not `Debug`, so match instead of expect_err.
        let error = match mock
            .send_streaming(HttpRequest::post("http://unused.test"))
            .await
        {
            Ok(_) => panic!("the structured engine never streams"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("never streams"));
    }

    #[actix_web::test]
    async fn provider_failures_map_to_request_errors() {
        let mock = Arc::new(QueueClient::default());
        for _ in 0..3 {
            mock.push(
                401,
                json!({"error": {"message": "unauthorized"}}).to_string(),
            );
        }
        let engine = AgentRuntimeEngine::build_with_client(
            &settings(),
            Some(mock.clone() as SharedHttpClient),
        )
        .expect("engine builds");

        let error = engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect_err("must fail");
        assert!(matches!(error, ModelError::Request { .. }));
    }

    /// Settings whose balanced tier carries two fallbacks behind the primary.
    fn settings_with_fallbacks() -> ModelSettings {
        ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "openrouter",
                "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                "VEYRA_MODEL_FAST" => "vendor/fast",
                "VEYRA_MODEL_BALANCED" => "vendor/balanced",
                "VEYRA_MODEL_REASONING" => "vendor/reasoning",
                "VEYRA_MODEL_BALANCED_FALLBACKS" => "vendor/second, vendor/third",
                _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("settings parse")
        .expect("model configured")
    }

    /// The model each queued request actually asked for, in order.
    fn models_requested(mock: &QueueClient) -> Vec<String> {
        mock.requests
            .lock()
            .expect("mock mutex")
            .iter()
            .map(|request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("request body is JSON");
                body["model"].as_str().expect("model field").to_owned()
            })
            .collect()
    }

    // The motivating outage: the primary's balance is gone, so the tick must be
    // served by the next model rather than lost.
    #[actix_web::test]
    async fn an_exhausted_balance_falls_over_to_the_next_model() {
        let mock = Arc::new(QueueClient::default());
        mock.push(
            402,
            json!({"error": {"message": "Insufficient credits"}}).to_string(),
        );
        mock.push(
            200,
            structured_response("bias", r#"{"bias":"bullish","confidence":0.7}"#),
        );
        let engine = AgentRuntimeEngine::build_with_client(
            &settings_with_fallbacks(),
            Some(mock.clone() as SharedHttpClient),
        )
        .expect("engine builds");

        let answer = engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("the fallback must serve the decision");

        assert_eq!(answer.value["bias"], "bullish");
        assert_eq!(
            models_requested(&mock),
            vec!["vendor/balanced", "vendor/second"],
            "the primary is tried first, then exactly one fallback"
        );
        assert_eq!(
            engine.last_attempted_model().as_deref(),
            Some("vendor/second")
        );
        assert_eq!(
            engine.last_successful_model().as_deref(),
            Some("vendor/second")
        );
    }

    // Exhausting the chain must report every model that refused, so the log
    // says which options were actually burned rather than only the last one.
    #[actix_web::test]
    async fn exhausting_the_chain_reports_each_failure() {
        let mock = Arc::new(QueueClient::default());
        for _ in 0..3 {
            mock.push(
                402,
                json!({"error": {"message": "Insufficient credits"}}).to_string(),
            );
        }
        let engine = AgentRuntimeEngine::build_with_client(
            &settings_with_fallbacks(),
            Some(mock.clone() as SharedHttpClient),
        )
        .expect("engine builds");

        let error = engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect_err("every candidate refused");

        let ModelError::Request { reason } = error else {
            panic!("a provider refusal is a request error");
        };
        for model in ["vendor/balanced", "vendor/second", "vendor/third"] {
            assert!(reason.contains(model), "{model} must appear in: {reason}");
        }
        assert!(reason.contains("insufficient_credits"));
        assert!(
            !reason.contains("Insufficient credits"),
            "provider response bodies must not cross the model error boundary: {reason}"
        );
        assert_eq!(models_requested(&mock).len(), 3, "the chain is not re-run");
        assert_eq!(
            engine.last_attempted_model().as_deref(),
            Some("vendor/third")
        );
        assert!(
            engine.last_successful_model().is_none(),
            "an exhausted chain must not report a successful model"
        );
    }

    // A tier without fallbacks must keep its old single-shot behaviour, so the
    // failover path cannot silently multiply spend on an unconfigured tier.
    #[actix_web::test]
    async fn a_tier_without_fallbacks_is_asked_exactly_once() {
        let mock = Arc::new(QueueClient::default());
        mock.push(
            402,
            json!({"error": {"message": "Insufficient credits"}}).to_string(),
        );
        let engine = AgentRuntimeEngine::build_with_client(
            &settings_with_fallbacks(),
            Some(mock.clone() as SharedHttpClient),
        )
        .expect("engine builds");

        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("no fallback is configured for the fast tier");
        assert_eq!(models_requested(&mock), vec!["vendor/fast"]);
    }

    fn engine_with(
        settings: &ModelSettings,
        mock: &Arc<QueueClient>,
    ) -> (AgentRuntimeEngine, CooldownRegistry, ManualClock) {
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let engine =
            AgentRuntimeEngine::build_with_client(settings, Some(mock.clone() as SharedHttpClient))
                .expect("engine builds")
                .with_cooldowns(cooldowns.clone());
        (engine, cooldowns, clock)
    }

    #[actix_web::test]
    async fn a_cooled_primary_is_skipped_without_a_request_until_its_probe() {
        let mock = Arc::new(QueueClient::default());
        let (engine, cooldowns, clock) = engine_with(&settings_with_fallbacks(), &mock);
        mock.push(
            402,
            json!({"error": {"message": "Insufficient credits"}}).to_string(),
        );
        mock.push(200, structured_response("bias", r#"{"bias":"bullish"}"#));
        engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("the fallback answers");

        mock.push(200, structured_response("bias", r#"{"bias":"bearish"}"#));
        engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("answers");
        assert_eq!(
            models_requested(&mock),
            ["vendor/balanced", "vendor/second", "vendor/second"],
            "the cooled primary costs nothing on the next tick"
        );
        let entry = &cooldowns.snapshot()[0];
        assert_eq!(
            (entry.model.as_str(), entry.reason.as_str(), entry.failures),
            ("vendor/balanced", "insufficient_credits", 1)
        );

        // After 30 minutes the primary is probed once; recovering clears it.
        clock.advance(std::time::Duration::from_secs(30 * 60));
        mock.push(200, structured_response("bias", r#"{"bias":"bullish"}"#));
        engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("the primary recovered");
        assert_eq!(
            models_requested(&mock).last().map(String::as_str),
            Some("vendor/balanced")
        );
        assert!(cooldowns.is_empty());
    }

    // The live GLM outage: OpenRouter's account policy excludes the provider,
    // which is a property of the model on this account, so it cools on the
    // long schedule.
    #[actix_web::test]
    async fn an_excluded_provider_404_cools_on_the_rejection_schedule() {
        let mock = Arc::new(QueueClient::default());
        let (engine, cooldowns, clock) = engine_with(&settings(), &mock);
        mock.push(
            404,
            json!({"message": "No allowed providers are available for the selected model. your account's allowed-providers setting permits only: meta, openai", "code": 404}).to_string(),
        );
        let error = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("rejected");
        assert_eq!(
            error.to_string(),
            "model request failed: vendor/fast: provider_rejected (404)"
        );
        assert!(
            !error.to_string().contains("allowed"),
            "no provider body crosses"
        );
        let entry = &cooldowns.snapshot()[0];
        assert_eq!(entry.reason, "provider_rejected");
        assert_eq!(
            entry.until_ms,
            crate::model::cooldown::epoch_millis(
                clock.now() + std::time::Duration::from_secs(3_600)
            )
        );
        engine
            .preflight(ModelTier::Fast)
            .expect_err("the only candidate is cooling");
        engine
            .preflight(ModelTier::Balanced)
            .expect("another tier's candidate is open");
    }

    // The live MiMo failure: truncated tool-call arguments with the native
    // tool-call markup leaked into the content. A real schema failure.
    #[actix_web::test]
    async fn truncated_tool_arguments_cool_on_the_invalid_response_schedule() {
        let mock = Arc::new(QueueClient::default());
        let (engine, cooldowns, _clock) = engine_with(&settings(), &mock);
        // The runtime asks a malformed answer twice before giving up.
        for _ in 0..2 {
            mock.push(
                200,
                json!({
                    "choices": [{
                        "message": {
                            "content": "<tool_call><function=bias><parameter=bias>bull",
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": { "name": "bias", "arguments": "{\"bias\":\"bull" }
                            }]
                        }
                    }]
                })
                .to_string(),
            );
        }
        let error = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("unusable answer");
        assert_eq!(
            error.to_string(),
            "model request failed: vendor/fast: invalid_response"
        );
        assert_eq!(cooldowns.snapshot()[0].reason, "invalid_response");
    }

    #[actix_web::test]
    async fn an_exhausted_balance_names_its_source_without_the_body() {
        let byok =
            json!({"error": {"message": "Provider returned error", "code": 402, "metadata": {
                "raw": "{\"error\":{\"message\":\"Insufficient Balance for account 12345\"}}",
                "provider_name": "DeepSeek", "is_byok": true
            }}})
            .to_string();
        let credits = json!({"error": {"message": "Insufficient credits. This account never purchased credits", "code": 402, "metadata": {
            "limit_source": "openrouter_credits"
        }}})
        .to_string();
        for (body, expected) in [
            (byok, "insufficient_credits (DeepSeek balance, BYOK)"),
            (credits, "insufficient_credits (OpenRouter credits)"),
        ] {
            let mock = Arc::new(QueueClient::default());
            let (engine, cooldowns, _clock) = engine_with(&settings(), &mock);
            mock.push(402, body);
            let error = engine
                .answer(request(ModelTier::Fast))
                .await
                .expect_err("no balance");
            assert_eq!(
                error.to_string(),
                format!("model request failed: vendor/fast: {expected}")
            );
            assert!(!error.to_string().contains("12345"));
            assert!(!error.to_string().contains("purchased"));
            assert_eq!(cooldowns.snapshot()[0].reason, "insufficient_credits");
        }

        assert_eq!(credit_source("not json"), None);
        assert_eq!(credit_source(r#"{"error":{"message":"plain"}}"#), None);
        assert_eq!(
            credit_source(r#"{"error":{"metadata":{"is_byok":true,"provider_name":"<script>"}}}"#)
                .as_deref(),
            Some("provider balance, BYOK"),
            "a provider name that does not look like one is dropped"
        );
        assert_eq!(
            credit_source(r#"{"error":{"metadata":{"limit_source":"key_limit"}}}"#),
            None
        );
    }

    #[test]
    fn provider_errors_classify_into_route_effects() {
        use agent_runtime::ProviderError;
        let wrap = |error: ProviderError| anyhow::Error::new(error).context("runtime context");
        let limited = classify(&wrap(ProviderError::RateLimited {
            provider: "openrouter".into(),
            retry_after: Some(std::time::Duration::from_secs(42)),
            body: "slow down".into(),
        }));
        assert_eq!(limited.reason, "rate_limited");
        assert_eq!(
            limited.class,
            crate::model::route::FailureClass::Cooldown(CooldownFailure::rate_limited(Some(
                std::time::Duration::from_secs(42)
            )))
        );
        let transport = classify(&wrap(ProviderError::Transport {
            provider: "openrouter".into(),
            source: anyhow::anyhow!("connection reset"),
        }));
        assert_eq!(transport, AttemptError::transport("transport"));
        for (error, reason, cooldown) in [
            (
                ProviderError::Overloaded {
                    provider: "p".into(),
                    body: String::new(),
                },
                "overloaded",
                CooldownReason::Overloaded,
            ),
            (
                ProviderError::Status {
                    provider: "p".into(),
                    status: 401,
                    body: String::new(),
                },
                "provider_rejected (401)",
                CooldownReason::Unauthorized,
            ),
            (
                ProviderError::Status {
                    provider: "p".into(),
                    status: 502,
                    body: String::new(),
                },
                "provider_rejected (502)",
                CooldownReason::Overloaded,
            ),
            (
                ProviderError::EmptyResponse {
                    provider: "p".into(),
                },
                "empty_response",
                CooldownReason::InvalidResponse,
            ),
        ] {
            assert_eq!(
                classify(&wrap(error)),
                AttemptError::cooldown(reason, CooldownFailure::new(cooldown))
            );
        }
    }

    fn zai_settings() -> ModelSettings {
        ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "z-ai",
                "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                "VEYRA_MODEL_FAST" => "glm-5.3-flash",
                "VEYRA_MODEL_BALANCED" | "VEYRA_MODEL_REASONING" => "glm-5.3",
                "VEYRA_MODEL_FALLBACKS" => "glm-5.3-flash",
                "VEYRA_MODEL_COMPEL_STRUCTURED" => "true",
                _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("native Z.AI ids parse")
        .expect("configured")
    }

    #[actix_web::test]
    async fn zai_uses_its_endpoint_bearer_key_and_an_automatic_tool_choice() {
        let mock = Arc::new(QueueClient::default());
        let (engine, _cooldowns, _clock) = engine_with(&zai_settings(), &mock);
        assert_eq!(
            engine.llm.provider_kind(),
            AgentProviderKind::Custom("zai".to_owned())
        );
        mock.push(500, "upstream down");
        mock.push(200, structured_response("bias", r#"{"bias":"bullish"}"#));
        let answer = engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("the native fallback answers");
        assert_eq!(answer.value["bias"], "bullish");
        assert_eq!(
            models_requested(&mock),
            ["glm-5.3", "glm-5.3-flash"],
            "a 500 is not retried by the runtime and fails over"
        );

        let requests = mock.requests.lock().expect("mock mutex");
        let sent = requests.last().expect("a request");
        assert_eq!(sent.url, "https://api.z.ai/api/paas/v4/chat/completions");
        assert!(
            sent.headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                    && value == "Bearer test-key-12345678"),
            "bearer key is sent"
        );
        let body: serde_json::Value = serde_json::from_slice(&sent.body).expect("JSON body");
        assert_eq!(
            body["tool_choice"],
            json!("auto"),
            "compel is ignored for Z.AI"
        );
    }

    #[actix_web::test]
    async fn zai_honours_a_base_url_override() {
        let settings = ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "zhipu",
                "VEYRA_MODEL_API_KEY" => "test-key-12345678",
                "VEYRA_MODEL_BASE_URL" => "https://open.bigmodel.cn/api/paas/v4",
                "VEYRA_MODEL_FAST" | "VEYRA_MODEL_BALANCED" | "VEYRA_MODEL_REASONING" => "glm-5.3",
                _ => return Err(ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("parses")
        .expect("configured");
        let mock = Arc::new(QueueClient::default());
        let (engine, _cooldowns, _clock) = engine_with(&settings, &mock);
        mock.push(200, structured_response("bias", r#"{"bias":"bearish"}"#));
        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("answers");
        let requests = mock.requests.lock().expect("mock mutex");
        assert_eq!(
            requests[0].url,
            "https://open.bigmodel.cn/api/paas/v4/chat/completions"
        );
    }

    #[derive(Default)]
    struct Progress(usize);

    #[async_trait]
    impl ToolProgressSink for Progress {
        async fn tool_started(&mut self, _id: &str, _name: &str, _arguments: &serde_json::Value) {
            self.0 += 1;
        }
        async fn tool_completed(
            &mut self,
            _id: &str,
            _name: &str,
            _result: &serde_json::Value,
            _available: bool,
        ) {
        }
    }

    struct Observe;

    #[async_trait]
    impl ReadOnlyTool for Observe {
        fn definition(&self) -> ReadOnlyToolDefinition {
            ReadOnlyToolDefinition {
                name: "observe".to_owned(),
                description: "Observe.".to_owned(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({"ready": true}))
        }
    }

    fn direct(content: &str) -> String {
        json!({"choices": [{"message": {"content": content}}]}).to_string()
    }

    #[actix_web::test]
    async fn tool_sessions_walk_the_chain_and_skip_cooled_candidates() {
        let mock = Arc::new(QueueClient::default());
        let (engine, cooldowns, _clock) = engine_with(&settings_with_fallbacks(), &mock);
        let mut progress = Progress::default();
        mock.push(
            402,
            json!({"error": {"message": "Insufficient credits"}}).to_string(),
        );
        mock.push(200, direct("Everything is ready."));
        let answer = engine
            .answer_with_tools(
                request(ModelTier::Balanced),
                vec![Arc::new(Observe)],
                &mut progress,
            )
            .await
            .expect("the fallback serves the session");
        assert_eq!(answer.value["answer"], "Everything is ready.");
        assert_eq!(
            models_requested(&mock),
            ["vendor/balanced", "vendor/second"]
        );
        assert_eq!(cooldowns.len(), 1);

        mock.push(200, direct("   "));
        mock.push(200, direct("The third candidate answers."));
        let answer = engine
            .answer_with_tools(
                request(ModelTier::Balanced),
                vec![Arc::new(Observe)],
                &mut progress,
            )
            .await
            .expect("an empty session answer fails over");
        assert_eq!(answer.value["answer"], "The third candidate answers.");
        assert_eq!(
            models_requested(&mock),
            [
                "vendor/balanced",
                "vendor/second",
                "vendor/second",
                "vendor/third"
            ],
            "the cooled primary was skipped; the empty answer cooled the second"
        );
        let reasons: Vec<String> = cooldowns
            .snapshot()
            .into_iter()
            .map(|entry| format!("{}={}", entry.model, entry.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                "vendor/second=invalid_response",
                "vendor/balanced=insufficient_credits"
            ]
        );
        assert_eq!(progress.0, 0, "direct answers run no tools");
    }
}
