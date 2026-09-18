//! `agent-runtime`-backed implementation of [`DecisionEngine`].
//!
//! This adapter owns every `agent-runtime` type, so the rest of the service
//! never imports the dependency. Providers are chosen through
//! `AgentProviderKind`; models come from explicitly configured tiers, never
//! from library defaults (which are dated).

use std::fmt;

use agent_runtime::{
    Agent as RuntimeAgent, AgentProviderKind, Llm, ModelTier as ProviderModelTier, ModelTiers,
    ResponseFormat, RetryPolicy,
};
use async_trait::async_trait;

use crate::model::settings::ModelSettings;
use crate::model::{
    DecisionAnswer, DecisionEngine, DecisionRequest, ModelError, ModelProvider, ModelTier,
};

/// Structured-decision adapter over `agent-runtime`.
pub struct AgentRuntimeEngine {
    llm: Llm,
    provider: ModelProvider,
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
            ModelProvider::OpenRouter => AgentProviderKind::OpenRouter,
        };

        let mut builder = Llm::builder()
            .provider(kind)
            .api_key(settings.api_key().expose());

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
            .verbose(false);

        // Attribution is keyed on the URL: the provider creates no app entry
        // without it, so the title and visibility headers only carry meaning
        // when it is present.
        if let Some(referer) = settings.http_referer() {
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
        })
    }
}

#[async_trait]
impl DecisionEngine for AgentRuntimeEngine {
    fn provider(&self) -> ModelProvider {
        self.provider
    }

    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
        let provider_tier = match request.tier {
            ModelTier::Fast => ProviderModelTier::Cheapest,
            ModelTier::Balanced => ProviderModelTier::Default,
            ModelTier::Reasoning => ProviderModelTier::Smartest,
        };

        let agent = InlineAgent {
            instructions: request.instructions,
            model: self.llm.model_for(provider_tier).to_string(),
        };
        let format = ResponseFormat::new(request.format.name, request.format.schema);

        let value = self
            .llm
            .run_structured_with_format(&agent, request.input, format)
            .await
            .map_err(|error| ModelError::Request {
                reason: format!("{error:#}"),
            })?;

        Ok(DecisionAnswer { value })
    }
}

#[cfg(test)]
mod tests {
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
}
