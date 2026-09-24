//! Opt-in end-to-end Codex subscription transport check. No credential values
//! are printed, persisted, or included in test output.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use veyra_service::AppState;
use veyra_service::config::{ConfigError, ServiceConfig};
use veyra_service::credential::CredentialVault;
use veyra_service::model::settings::ModelSettings;
use veyra_service::model::{
    AnswerFormat, DecisionRequest, ModelRuntime, ModelTier, ReadOnlyTool, ReadOnlyToolDefinition,
    ToolProgressSink,
};
use veyra_service::risk::{RiskGate, RiskPolicy};
use veyra_service::state::{RuntimeState, StateError, StateStore};
use veyra_service::subscription_auth::{SubscriptionCredential, SubscriptionProvider};

#[derive(Debug)]
struct EphemeralState;

#[async_trait]
impl StateStore for EphemeralState {
    async fn load(&self, _key: &str) -> Result<Option<Value>, StateError> {
        Ok(None)
    }
    async fn save(&self, _key: &str, _value: &Value) -> Result<(), StateError> {
        Ok(())
    }
}

struct ReadyTool;

#[async_trait]
impl ReadOnlyTool for ReadyTool {
    fn definition(&self) -> ReadOnlyToolDefinition {
        ReadOnlyToolDefinition {
            name: "test_observation".to_owned(),
            description: "Read the test service readiness fact. Use this before answering whether it is ready.".to_owned(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }
    }
    async fn execute(&self, arguments: Value) -> Result<Value, String> {
        if arguments != json!({}) {
            return Err("invalid arguments".to_owned());
        }
        Ok(json!({"ready":true}))
    }
}

#[derive(Default)]
struct Progress {
    started: usize,
    completed: usize,
}

#[async_trait]
impl ToolProgressSink for Progress {
    async fn tool_started(&mut self, _id: &str, _name: &str, _arguments: &Value) {
        self.started += 1;
    }
    async fn tool_completed(&mut self, _id: &str, _name: &str, _result: &Value, _available: bool) {
        self.completed += 1;
    }
}

#[actix_web::test]
#[ignore = "requires VEYRA_CODEX_AUTH_FILE and the encrypted vault environment"]
async fn codex_subscription_answers_and_requests_an_observation() {
    let path = std::env::var("VEYRA_CODEX_AUTH_FILE").expect("auth file path is required");
    let raw = std::fs::read_to_string(path).expect("auth file must be readable");
    let auth: Value = serde_json::from_str(&raw).expect("auth file must be JSON");
    let tokens = auth.get("tokens").expect("signed-in Codex tokens required");
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .expect("access token required");
    let refresh = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .expect("refresh token required");
    let identity = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .expect("identity token required");
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .expect("account ID required");

    let config = ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("test service config");
    let vault = CredentialVault::from_env()
        .expect("vault config")
        .expect("vault configured");
    let app = AppState::new(config, None, None, RiskGate::new(RiskPolicy::default()))
        .with_credential_vault(Some(vault))
        .with_runtime_state(RuntimeState::new(Some(Arc::new(EphemeralState))));
    app.subscription_auth()
        .set_credential(SubscriptionCredential {
            provider: SubscriptionProvider::Codex,
            access_token: access.to_owned(),
            id_token: identity.to_owned(),
            refresh_token: refresh.to_owned(),
            account_id: Some(account_id.to_owned()),
            account_uuid: None,
            organization_uuid: None,
            scopes: vec![],
            account_label: None,
            expires_at_unix: None,
        });
    let settings = ModelSettings::from_source(|name| match name {
        "VEYRA_MODEL_PROVIDER" => Ok("codex".to_owned()),
        "VEYRA_MODEL_FAST" | "VEYRA_MODEL_BALANCED" | "VEYRA_MODEL_REASONING" => {
            Ok("gpt-5.6-terra".to_owned())
        }
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("model config")
    .expect("model configured");
    let model = ModelRuntime::from_settings_with_app(settings, &app).expect("connected model");

    let answer = model.engine().answer(DecisionRequest {
        instructions: "Call the required answer function with one short true statement.".to_owned(),
        input: "Is this a live Codex subscription response?".to_owned(),
        format: AnswerFormat { name: "veyra_live_answer".to_owned(), schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}) },
        tier: ModelTier::Fast,
    }).await.expect("structured subscription answer");
    assert!(
        answer
            .value
            .get("answer")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    );

    let mut progress = Progress::default();
    let answer = model.answer_with_tools(DecisionRequest {
        instructions: "You are a read-only assistant. Use the test_observation tool to check readiness before answering.".to_owned(),
        input: "Use test_observation now. Is the test service ready?".to_owned(),
        format: AnswerFormat { name: "veyra_live_answer".to_owned(), schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}) },
        tier: ModelTier::Fast,
    }, vec![Arc::new(ReadyTool)], &mut progress).await.expect("tool-backed subscription answer");
    assert!(
        answer
            .value
            .get("answer")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    );
    assert!(progress.started > 0, "the model must request the tool");
    assert_eq!(progress.started, progress.completed);
}
