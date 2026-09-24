//! `agent-runtime`-backed implementation of [`DecisionEngine`].
//!
//! This adapter owns every `agent-runtime` type, so the rest of the service
//! never imports the dependency. Providers are chosen through
//! `AgentProviderKind`; models come from explicitly configured tiers, never
//! from library defaults (which are dated).

use std::fmt;
use std::sync::{Arc, Mutex};

use agent_runtime::{
    Agent as RuntimeAgent, AgentProviderKind, ChatMessage, EventSink, Llm, ModelTiers,
    ProviderError, ResponseFormat, RetryPolicy, RuntimeEvent, Tool, ToolCall, ToolDefinition,
    ToolOutput, ToolRegistry, ToolSessionOutcome,
};
use async_trait::async_trait;
use serde_json::json;

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
    /// Last candidate requested, including one that failed.
    last_attempted_model: Arc<Mutex<Option<String>>>,
    /// Last candidate that returned a structured answer.
    last_successful_model: Arc<Mutex<Option<String>>>,
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
            .structured_strategy(if settings.compel_structured_answer() {
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
            last_attempted_model: Arc::new(Mutex::new(None)),
            last_successful_model: Arc::new(Mutex::new(None)),
        })
    }

    /// Records the latest model candidate without allowing telemetry failure
    /// to affect the decision path.
    fn remember(slot: &Mutex<Option<String>>, model: &str) {
        match slot.lock() {
            Ok(mut value) => *value = Some(model.to_owned()),
            Err(poisoned) => *poisoned.into_inner() = Some(model.to_owned()),
        }
    }

    /// Reads a telemetry value while recovering from a poisoned lock.
    fn remembered(slot: &Mutex<Option<String>>) -> Option<String> {
        match slot.lock() {
            Ok(value) => value.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Whether another model is worth trying after this failure.
///
/// The question is never "was this request valid" but "could a different model
/// answer it". An exhausted balance, a rate limit, an overloaded upstream, or a
/// flat rejection are all properties of the model that was asked, so the next
/// candidate gets a turn. A transport fault is the exception: the provider was
/// never reached, so the same failure would repeat on every candidate and the
/// runtime's own retry policy is the right layer to handle it.
fn worth_failing_over(error: &anyhow::Error) -> bool {
    match provider_error(error) {
        Some(ProviderError::Transport { .. }) => false,
        // A classified provider refusal: credits, rate limit, overload, an
        // unexpected status, or a response with nothing usable in it.
        Some(_) => true,
        // Not a provider error at all — most often the answer came back but
        // did not satisfy the schema. Another model may well comply.
        None => true,
    }
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
        Some(ProviderError::InsufficientCredits { .. }) => "insufficient_credits".to_owned(),
        Some(ProviderError::Overloaded { .. }) => "overloaded".to_owned(),
        Some(ProviderError::Status { status, .. }) => format!("provider_rejected ({status})"),
        Some(ProviderError::Transport { .. }) => "transport".to_owned(),
        Some(ProviderError::EmptyResponse { .. }) => "empty_response".to_owned(),
        None => "invalid_response".to_owned(),
    }
}

#[async_trait]
impl DecisionEngine for AgentRuntimeEngine {
    fn provider(&self) -> ModelProvider {
        self.provider
    }

    fn last_attempted_model(&self) -> Option<String> {
        Self::remembered(&self.last_attempted_model)
    }

    fn last_successful_model(&self) -> Option<String> {
        Self::remembered(&self.last_successful_model)
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

        let model = self.tiers.resolve(request.tier).to_owned();
        Self::remember(&self.last_attempted_model, &model);
        let mut registry = ToolRegistry::new();
        for tool in tools {
            let definition = runtime_definition(tool.definition());
            registry.register(RuntimeReadOnlyTool { tool, definition });
        }

        let mut runtime_progress = RuntimeProgress { sink: progress };
        let history = [ChatMessage::user(request.input)];
        let outcome = self
            .llm
            .execute_tool_session(
                agent_runtime::ToolSessionRequest {
                    model: &model,
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
            .map_err(|error| ModelError::Request {
                reason: safe_failure_reason(&error),
            })?;

        let message = match outcome {
            ToolSessionOutcome::Direct { message, .. }
            | ToolSessionOutcome::ToolBacked { message, .. } => message,
        };
        if message.trim().is_empty() {
            return Err(ModelError::Request {
                reason: "provider returned an empty tool-session answer".to_owned(),
            });
        }
        Self::remember(&self.last_successful_model, &model);
        Ok(DecisionAnswer {
            value: json!({ "answer": message }),
        })
    }

    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
        let candidates = self.tiers.chain(request.tier);
        let mut failures: Vec<String> = Vec::new();

        for (position, model) in candidates.iter().enumerate() {
            Self::remember(&self.last_attempted_model, model);
            let agent = InlineAgent {
                instructions: request.instructions.clone(),
                model: model.clone(),
            };
            let format =
                ResponseFormat::new(request.format.name.clone(), request.format.schema.clone());

            match self
                .llm
                .run_structured_with_format(&agent, request.input.clone(), format)
                .await
            {
                Ok(value) => {
                    Self::remember(&self.last_successful_model, model);
                    if position > 0 {
                        tracing::warn!(
                            tier = %request.tier,
                            model = %model,
                            skipped = position,
                            "model fallback served the decision"
                        );
                    }
                    return Ok(DecisionAnswer { value });
                }
                Err(error) => {
                    let reason = safe_failure_reason(&error);
                    let last = position + 1 == candidates.len();
                    if last || !worth_failing_over(&error) {
                        failures.push(format!("{model}: {reason}"));
                        break;
                    }
                    tracing::warn!(
                        tier = %request.tier,
                        model = %model,
                        %reason,
                        "model failed; trying the next fallback"
                    );
                    failures.push(format!("{model}: {reason}"));
                }
            }
        }

        Err(ModelError::Request {
            reason: failures.join(" | "),
        })
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
}
