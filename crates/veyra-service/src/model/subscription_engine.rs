//! Subscription-backed model transport for Codex and Claude Code.
//!
//! Adapted from the CCS subscription provider protocol (Codecraft Solutions ZA,
//! LicenseRef-Heimdall-FSL). Only this model boundary speaks provider-specific
//! wire formats. Tokens remain in the encrypted runtime store, and the tool
//! registry is supplied by the read-only assistant boundary.
//!
//! The ChatGPT (Codex) subscription routes through [`crate::model::route`]:
//! its candidates are labelled `chatgpt:<model>`, skipped while cooling down,
//! and it can lead the subscription-first composite with the single model from
//! `VEYRA_MODEL_CHATGPT_MODEL`. A refresh the provider rejects marks it as
//! needing to be reconnected, which takes it out of every route until the
//! operator reconnects. The Claude Code subscription keeps its original
//! request loop and is not affected by cooldowns or preference.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_runtime::{
    AgentProviderKind, AssistantTurn, ChatMessage, EventSink, MessageRole, ModelTiers,
    ProviderInfo, RuntimeEvent, TextProvider, ToolCall, ToolDefinition, ToolSessionOutcome,
    ToolSessionRequest, execute_tool_session,
};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as AsyncMutex;

use super::agent_runtime_engine::{RuntimeProgress, runtime_registry};
use super::cooldown::{CooldownFailure, CooldownReason, CooldownRegistry};
use super::route::{self, AttemptError, Call, CandidateTransport, Leg, Telemetry};
use super::{
    DecisionAnswer, DecisionEngine, DecisionRequest, ModelError, ModelProvider, ModelTier,
    ReadOnlyTool, ToolProgressSink, chatgpt_label,
};
use crate::AppState;
use crate::credential::CredentialVault;
use crate::state::{RuntimeState, StateKey};
use crate::subscription_auth::{
    SubscriptionAuthState, SubscriptionCredential, SubscriptionProvider,
};

const CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CLAUDE_BASE: &str = "https://api.anthropic.com";
const CLAUDE_TOKEN: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CLAUDE_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24,extended-cache-ttl-2025-04-11";

/// One authenticated subscription implementation of the decision contract.
pub struct SubscriptionEngine {
    provider: SubscriptionProvider,
    auth: SubscriptionAuthState,
    vault: CredentialVault,
    state: RuntimeState,
    client: reqwest::Client,
    refresh_lock: AsyncMutex<()>,
    tiers: super::settings::TierModels,
    runtime_tiers: ModelTiers,
    telemetry: Telemetry,
    session_id: uuid::Uuid,
    /// Shared cooldowns for the ChatGPT subscription. `None` for Claude Code,
    /// which keeps its original request loop.
    cooldowns: Option<CooldownRegistry>,
    /// ChatGPT endpoints; fixed in production, pointed at a loopback server
    /// by tests so no test ever reaches the real provider.
    codex_base: String,
    codex_token: String,
}

impl fmt::Debug for SubscriptionEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubscriptionEngine")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl SubscriptionEngine {
    /// Builds a provider only when an encrypted connection is present.
    ///
    /// # Errors
    /// Returns [`ModelError::Construction`] when the settings do not select a
    /// subscription, it is not connected, or encrypted storage is missing.
    pub fn build(
        settings: &super::settings::ModelSettings,
        app: &AppState,
    ) -> Result<Self, ModelError> {
        let provider = match settings.provider() {
            ModelProvider::Codex => SubscriptionProvider::Codex,
            ModelProvider::ClaudeCode => SubscriptionProvider::ClaudeCode,
            _ => return Err(construction("not a subscription provider")),
        };
        Self::assemble(provider, settings.tiers().clone(), app)
    }

    /// Builds the ChatGPT subscription leg of the subscription-first route.
    ///
    /// `model` is used for every tier: the configured API tier models belong
    /// to another provider and are never sent to the subscription.
    ///
    /// # Errors
    /// Returns [`ModelError::Construction`] when the subscription is not
    /// connected or encrypted storage is missing.
    pub fn build_chatgpt(model: &str, app: &AppState) -> Result<Self, ModelError> {
        Self::assemble(
            SubscriptionProvider::Codex,
            super::settings::TierModels::new(model, model, model),
            app,
        )
    }

    fn assemble(
        provider: SubscriptionProvider,
        tiers: super::settings::TierModels,
        app: &AppState,
    ) -> Result<Self, ModelError> {
        if !app.subscription_auth().connected(provider) {
            return Err(construction("connect the selected subscription first"));
        }
        let vault = app
            .credential_vault()
            .cloned()
            .ok_or_else(|| construction("encrypted credential storage is unavailable"))?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| construction("subscription HTTP client is unavailable"))?;
        let runtime_tiers = ModelTiers::new(
            tiers.resolve(ModelTier::Balanced),
            tiers.resolve(ModelTier::Fast),
            tiers.resolve(ModelTier::Reasoning),
        );
        Ok(Self {
            provider,
            auth: app.subscription_auth().clone(),
            vault,
            state: app.runtime_state().clone(),
            client,
            refresh_lock: AsyncMutex::new(()),
            tiers,
            runtime_tiers,
            telemetry: Telemetry::default(),
            session_id: uuid::Uuid::new_v4(),
            cooldowns: (provider == SubscriptionProvider::Codex)
                .then(|| app.model_cooldowns().clone()),
            codex_base: CODEX_BASE.to_owned(),
            codex_token: CODEX_TOKEN.to_owned(),
        })
    }

    /// Points the ChatGPT transport at a loopback test server.
    #[cfg(test)]
    pub(crate) fn with_codex_endpoints(mut self, base: &str, token: &str) -> Self {
        self.codex_base = base.to_owned();
        self.codex_token = token.to_owned();
        self
    }

    fn state_key(&self) -> StateKey {
        match self.provider {
            SubscriptionProvider::Codex => StateKey::SubscriptionCodex,
            SubscriptionProvider::ClaudeCode => StateKey::SubscriptionClaudeCode,
        }
    }

    fn legs(&self, tier: ModelTier) -> [Leg<'_>; 1] {
        [Leg {
            transport: self,
            models: self.tiers.chain(tier),
        }]
    }

    async fn credential(&self, force_refresh: bool) -> anyhow::Result<SubscriptionCredential> {
        let _guard = self.refresh_lock.lock().await;
        let credential = self
            .auth
            .credential(self.provider)
            .ok_or_else(|| SubscriptionFailure::reconnect("subscription disconnected"))?;
        // A rejected refresh already proved the stored grant is dead; asking
        // the token endpoint again on every call would only repeat it.
        if self.auth.needs_reconnect(self.provider) {
            return Err(SubscriptionFailure::reconnect(
                "subscription needs to be reconnected",
            ));
        }
        let now = now_unix();
        let expired = credential
            .expires_at_unix
            .is_some_and(|expiry| expiry <= now + 300);
        if !force_refresh && !expired {
            return Ok(credential);
        }
        let refreshed = match self.refresh(&credential).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                self.note_refresh_failure(&error);
                return Err(error);
            }
        };
        let serialized = serde_json::to_string(&refreshed).map_err(|_| {
            SubscriptionFailure::local("subscription credential cannot be serialized")
        })?;
        let encrypted = self.vault.seal_text(&serialized).map_err(|_| {
            SubscriptionFailure::local("subscription credential cannot be encrypted")
        })?;
        self.state
            .save_required(self.state_key(), &encrypted)
            .await
            .map_err(|_| SubscriptionFailure::local("subscription refresh cannot be persisted"))?;
        self.auth.set_credential(refreshed.clone());
        Ok(refreshed)
    }

    /// Marks the ChatGPT subscription as needing to be reconnected when the
    /// provider rejected its refresh, so every route drops it and the model
    /// runtime is rebuilt without it. Claude Code keeps its original
    /// behaviour.
    fn note_refresh_failure(&self, error: &anyhow::Error) {
        if self.provider != SubscriptionProvider::Codex {
            return;
        }
        let rejected = subscription_failure(error)
            .is_some_and(|failure| matches!(failure.kind, FailureKind::Reconnect));
        if rejected {
            self.auth.mark_needs_reconnect(self.provider);
            tracing::warn!(
                provider = self.provider.as_str(),
                "ChatGPT subscription refresh was rejected; reconnect it to use the subscription again"
            );
        }
    }

    async fn refresh(
        &self,
        old: &SubscriptionCredential,
    ) -> anyhow::Result<SubscriptionCredential> {
        if old.refresh_token.is_empty() {
            return Err(SubscriptionFailure::reconnect(
                "subscription needs to be reconnected",
            ));
        }
        let response = match self.provider {
            SubscriptionProvider::Codex => self.client.post(&self.codex_token)
                .header("originator", "codex_cli_rs")
                .json(&json!({"client_id": CODEX_CLIENT_ID, "grant_type": "refresh_token", "refresh_token": old.refresh_token}))
                .send().await,
            SubscriptionProvider::ClaudeCode => self.client.post(CLAUDE_TOKEN)
                .json(&json!({"grant_type": "refresh_token", "client_id": CLAUDE_CLIENT_ID, "refresh_token": old.refresh_token, "scope": CLAUDE_SCOPE}))
                .send().await,
        }.map_err(|_| SubscriptionFailure::unreachable("subscription token refresh could not reach provider"))?;
        if !response.status().is_success() {
            return Err(SubscriptionFailure::reconnect(format!(
                "subscription needs to be reconnected ({})",
                response.status()
            )));
        }
        let body: Value = response.json().await.map_err(|_| {
            SubscriptionFailure::malformed("subscription refresh response is malformed")
        })?;
        let mut next = old.clone();
        next.access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SubscriptionFailure::malformed("subscription refresh omitted access token")
            })?
            .to_owned();
        if let Some(refresh) = body.get("refresh_token").and_then(Value::as_str) {
            next.refresh_token = refresh.to_owned();
        }
        if let Some(id) = body.get("id_token").and_then(Value::as_str) {
            next.id_token = id.to_owned();
            next.account_id = account_id_from_jwt(id).or(next.account_id);
        }
        if let Some(expires) = body.get("expires_in").and_then(Value::as_i64) {
            next.expires_at_unix = Some(now_unix().saturating_add(expires.max(0)));
        }
        if let Some(scope) = body.get("scope").and_then(Value::as_str) {
            next.scopes = scope.split_whitespace().map(str::to_owned).collect();
        }
        Ok(next)
    }

    async fn complete(
        &self,
        model: &str,
        instructions: &str,
        history: &[ChatMessage],
        tools: &[ToolDefinition],
        forced: Option<&str>,
    ) -> anyhow::Result<AssistantTurn> {
        let credential = self.credential(false).await?;
        let response = self
            .send_once(model, instructions, history, tools, forced, &credential)
            .await;
        if matches!(response, Err(TransportError::Unauthorized)) {
            let refreshed = self.credential(true).await?;
            return self
                .send_once(model, instructions, history, tools, forced, &refreshed)
                .await
                .map_err(TransportError::into_anyhow);
        }
        response.map_err(TransportError::into_anyhow)
    }

    async fn send_once(
        &self,
        model: &str,
        instructions: &str,
        history: &[ChatMessage],
        tools: &[ToolDefinition],
        forced: Option<&str>,
        credential: &SubscriptionCredential,
    ) -> Result<AssistantTurn, TransportError> {
        let request = match self.provider {
            SubscriptionProvider::Codex => {
                let mut body = codex_body(model, instructions, history, tools);
                if let Some(name) = forced {
                    body["tool_choice"] = json!({"type":"function","name":name});
                }
                let mut request = self
                    .client
                    .post(format!("{}/responses", self.codex_base))
                    .bearer_auth(&credential.access_token)
                    .header("Accept", "text/event-stream")
                    .header("originator", "codex_cli_rs")
                    .header("User-Agent", "codex_cli_rs/veyra")
                    .json(&body);
                if let Some(id) = &credential.account_id {
                    request = request.header("ChatGPT-Account-ID", id);
                }
                request
            }
            SubscriptionProvider::ClaudeCode => {
                let mut body = claude_body(
                    model,
                    instructions,
                    history,
                    tools,
                    self.session_id,
                    credential,
                );
                if let Some(name) = forced {
                    body["tool_choice"] = json!({"type":"tool","name":name});
                }
                self.client
                    .post(format!("{CLAUDE_BASE}/v1/messages?beta=true"))
                    .bearer_auth(&credential.access_token)
                    .header("anthropic-version", "2023-06-01")
                    .header("anthropic-beta", CLAUDE_BETAS)
                    .header("anthropic-dangerous-direct-browser-access", "true")
                    .header("user-agent", "claude-cli/2.1.261 (external, sdk-cli)")
                    .header("x-app", "cli")
                    .header("x-claude-code-session-id", self.session_id.to_string())
                    .json(&body)
            }
        };
        let response = request
            .send()
            .await
            .map_err(|_| TransportError::Unreachable("subscription provider unavailable"))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(TransportError::Unauthorized);
        }
        if !status.is_success() {
            return Err(TransportError::Status {
                status: status.as_u16(),
                retry_after: retry_after(response.headers()),
            });
        }
        let body = response
            .text()
            .await
            .map_err(|_| TransportError::Unreachable("subscription response unavailable"))?;
        match self.provider {
            SubscriptionProvider::Codex => parse_codex_sse(&body),
            SubscriptionProvider::ClaudeCode => parse_claude_response(&body),
        }
        .map_err(|_| TransportError::Malformed)
    }

    /// The original Claude Code request loop, kept exactly as it was.
    async fn legacy_answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
        let mut last = "subscription request failed".to_owned();
        for model in self.tiers.chain(request.tier) {
            self.telemetry.attempted(model);
            let tool = ToolDefinition {
                name: request.format.name.clone(),
                description: "Return the required structured decision".to_owned(),
                input_schema: request.format.schema.clone(),
            };
            match self
                .complete(
                    model,
                    &request.instructions,
                    &[ChatMessage::user(request.input.clone())],
                    &[tool],
                    Some(&request.format.name),
                )
                .await
            {
                Ok(turn) => {
                    if let Some(call) = turn
                        .tool_calls
                        .into_iter()
                        .find(|call| call.name == request.format.name)
                    {
                        self.telemetry.succeeded(model);
                        return Ok(DecisionAnswer {
                            value: call.arguments,
                        });
                    }
                    last = "subscription response omitted required structured answer".to_owned();
                }
                Err(error) => last = error.to_string(),
            }
        }
        Err(ModelError::Request { reason: last })
    }

    /// The original Claude Code tool session, kept exactly as it was.
    async fn legacy_tool_session(
        &self,
        request: DecisionRequest,
        tools: Vec<Arc<dyn ReadOnlyTool>>,
        progress: &mut dyn ToolProgressSink,
    ) -> Result<DecisionAnswer, ModelError> {
        let model = self.tiers.resolve(request.tier).to_owned();
        self.telemetry.attempted(&model);
        let registry = runtime_registry(&tools);
        let mut sink = RuntimeProgress { sink: progress };
        let history = [ChatMessage::user(request.input)];
        let outcome = execute_tool_session(
            self,
            ToolSessionRequest {
                model: &model,
                decision_system_prompt: &request.instructions,
                followup_system_prompt: &request.instructions,
                history: &history,
                tool_registry: &registry,
                tool_context: (),
                max_tool_calls: 8,
            },
            &mut sink,
        )
        .await
        .map_err(|_| ModelError::Request {
            reason: "subscription tool session failed".to_owned(),
        })?;
        let answer = match outcome {
            ToolSessionOutcome::Direct { message, .. }
            | ToolSessionOutcome::ToolBacked { message, .. } => message,
        };
        if answer.trim().is_empty() {
            return Err(ModelError::Request {
                reason: "subscription returned no answer".to_owned(),
            });
        }
        self.telemetry.succeeded(&model);
        Ok(DecisionAnswer {
            value: json!({"answer": answer}),
        })
    }
}

/// A classified subscription failure. `Display` keeps the historical wording,
/// which is all the Claude Code loop and every log line ever see; the kind
/// only decides failover and cooldowns.
#[derive(Debug)]
struct SubscriptionFailure {
    message: String,
    kind: FailureKind,
}

#[derive(Debug, Clone, Copy)]
enum FailureKind {
    /// Disconnected, a refresh the provider rejected, or a refreshed token it
    /// still refuses.
    Reconnect,
    /// Any other non-success status.
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// The provider was not reached.
    Unreachable,
    /// The provider answered with something unusable.
    Malformed,
    /// Local credential persistence failed.
    Local,
}

impl fmt::Display for SubscriptionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SubscriptionFailure {}

impl SubscriptionFailure {
    fn error(kind: FailureKind, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            message: message.into(),
            kind,
        })
    }

    fn reconnect(message: impl Into<String>) -> anyhow::Error {
        Self::error(FailureKind::Reconnect, message)
    }

    fn unreachable(message: impl Into<String>) -> anyhow::Error {
        Self::error(FailureKind::Unreachable, message)
    }

    fn malformed(message: impl Into<String>) -> anyhow::Error {
        Self::error(FailureKind::Malformed, message)
    }

    fn local(message: impl Into<String>) -> anyhow::Error {
        Self::error(FailureKind::Local, message)
    }

    /// How the route treats this failure. Unreachable and local faults are
    /// not properties of the model, so they neither fail over nor cool down.
    fn attempt_error(&self) -> AttemptError {
        match self.kind {
            FailureKind::Reconnect => AttemptError::cooldown(
                "unauthorized",
                CooldownFailure::new(CooldownReason::Unauthorized),
            ),
            FailureKind::Status {
                status,
                retry_after,
            } => AttemptError::cooldown(
                status_reason(status),
                CooldownFailure::from_status(status, retry_after),
            ),
            FailureKind::Unreachable => AttemptError::transport("transport"),
            FailureKind::Malformed => invalid_response(),
            FailureKind::Local => AttemptError::transport("subscription_state_unavailable"),
        }
    }
}

/// Finds the classified subscription failure beneath any runtime context.
fn subscription_failure(error: &anyhow::Error) -> Option<&SubscriptionFailure> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SubscriptionFailure>())
}

/// Classifies any failure from one subscription attempt. An error the
/// transport did not classify came from the tool-session runtime itself —
/// the model's turn could not be used — so another model may do better.
fn classify(error: &anyhow::Error) -> AttemptError {
    subscription_failure(error).map_or_else(invalid_response, SubscriptionFailure::attempt_error)
}

fn invalid_response() -> AttemptError {
    AttemptError::cooldown(
        "invalid_response",
        CooldownFailure::new(CooldownReason::InvalidResponse),
    )
}

/// The same safe categories the API engine reports for a status.
fn status_reason(status: u16) -> String {
    match status {
        402 => "insufficient_credits".to_owned(),
        429 => "rate_limited".to_owned(),
        503 | 529 => "overloaded".to_owned(),
        _ => format!("provider_rejected ({status})"),
    }
}

/// Reads a delta-seconds `Retry-After` header. HTTP-date forms are ignored and
/// fall back to the rate-limit schedule.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: f64 = raw.trim().parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(seconds).ok()
}

#[derive(Debug)]
enum TransportError {
    Unauthorized,
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
    Unreachable(&'static str),
    Malformed,
}

impl TransportError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Unauthorized => {
                SubscriptionFailure::reconnect("subscription needs to be reconnected")
            }
            Self::Status {
                status,
                retry_after,
            } => SubscriptionFailure::error(
                FailureKind::Status {
                    status,
                    retry_after,
                },
                format!("subscription provider rejected request ({status})"),
            ),
            Self::Unreachable(reason) => SubscriptionFailure::unreachable(reason),
            Self::Malformed => SubscriptionFailure::malformed("subscription response malformed"),
        }
    }
}

impl ProviderInfo for SubscriptionEngine {
    fn kind(&self) -> AgentProviderKind {
        AgentProviderKind::Custom(self.provider.as_str().to_owned())
    }
    fn verbose(&self) -> bool {
        false
    }
    fn model_tiers(&self) -> &ModelTiers {
        &self.runtime_tiers
    }
}

#[async_trait]
impl TextProvider for SubscriptionEngine {
    async fn request_assistant_turn(
        &self,
        model: &str,
        system_prompt: &str,
        history: &[ChatMessage],
        tool_definitions: &[ToolDefinition],
    ) -> anyhow::Result<AssistantTurn> {
        self.complete(model, system_prompt, history, tool_definitions, None)
            .await
    }
    async fn stream_message(
        &self,
        model: &str,
        system_prompt: &str,
        messages: &[ChatMessage],
        sink: &mut dyn EventSink,
    ) -> anyhow::Result<String> {
        let turn = self
            .complete(model, system_prompt, messages, &[], None)
            .await?;
        let content = turn.content.unwrap_or_default();
        sink.emit(RuntimeEvent::AssistantDelta {
            delta: content.clone(),
        })
        .await?;
        Ok(content)
    }
}

#[async_trait]
impl CandidateTransport for SubscriptionEngine {
    fn candidate_provider(&self) -> ModelProvider {
        match self.provider {
            SubscriptionProvider::Codex => ModelProvider::Codex,
            SubscriptionProvider::ClaudeCode => ModelProvider::ClaudeCode,
        }
    }

    fn label(&self, model: &str) -> String {
        match self.provider {
            SubscriptionProvider::Codex => chatgpt_label(model),
            SubscriptionProvider::ClaudeCode => model.to_owned(),
        }
    }

    async fn attempt(
        &self,
        model: &str,
        request: &DecisionRequest,
        call: &mut Call<'_>,
    ) -> Result<DecisionAnswer, AttemptError> {
        match call {
            Call::Structured => {
                let tool = ToolDefinition {
                    name: request.format.name.clone(),
                    description: "Return the required structured decision".to_owned(),
                    input_schema: request.format.schema.clone(),
                };
                let turn = self
                    .complete(
                        model,
                        &request.instructions,
                        &[ChatMessage::user(request.input.clone())],
                        &[tool],
                        Some(&request.format.name),
                    )
                    .await
                    .map_err(|error| classify(&error))?;
                turn.tool_calls
                    .into_iter()
                    .find(|call| call.name == request.format.name)
                    .map(|call| DecisionAnswer {
                        value: call.arguments,
                    })
                    .ok_or_else(invalid_response)
            }
            Call::Tools { tools, progress } => {
                let registry = runtime_registry(tools);
                let mut sink = RuntimeProgress {
                    sink: &mut **progress,
                };
                let history = [ChatMessage::user(request.input.clone())];
                let outcome = execute_tool_session(
                    self,
                    ToolSessionRequest {
                        model,
                        decision_system_prompt: &request.instructions,
                        followup_system_prompt: &request.instructions,
                        history: &history,
                        tool_registry: &registry,
                        tool_context: (),
                        max_tool_calls: 8,
                    },
                    &mut sink,
                )
                .await
                .map_err(|error| classify(&error))?;
                let answer = match outcome {
                    ToolSessionOutcome::Direct { message, .. }
                    | ToolSessionOutcome::ToolBacked { message, .. } => message,
                };
                if answer.trim().is_empty() {
                    return Err(invalid_response());
                }
                Ok(DecisionAnswer {
                    value: json!({"answer": answer}),
                })
            }
        }
    }
}

#[async_trait]
impl DecisionEngine for SubscriptionEngine {
    fn provider(&self) -> ModelProvider {
        self.candidate_provider()
    }
    fn last_attempted_model(&self) -> Option<String> {
        self.telemetry.last_attempted()
    }
    fn last_successful_model(&self) -> Option<String> {
        self.telemetry.last_successful()
    }

    fn preflight(&self, tier: ModelTier) -> Result<(), ModelError> {
        match &self.cooldowns {
            Some(cooldowns) => route::preflight(&self.legs(tier), cooldowns),
            None => Ok(()),
        }
    }

    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
        let Some(cooldowns) = &self.cooldowns else {
            return self.legacy_answer(request).await;
        };
        let legs = self.legs(request.tier);
        route::run(
            &legs,
            &request,
            &mut Call::Structured,
            cooldowns,
            &self.telemetry,
        )
        .await
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
        let Some(cooldowns) = &self.cooldowns else {
            return self.legacy_tool_session(request, tools, progress).await;
        };
        let legs = self.legs(request.tier);
        let mut call = Call::Tools {
            tools: &tools,
            progress,
        };
        route::run(&legs, &request, &mut call, cooldowns, &self.telemetry).await
    }
}

fn codex_body(
    model: &str,
    instructions: &str,
    history: &[ChatMessage],
    tools: &[ToolDefinition],
) -> Value {
    let mut input = Vec::new();
    for message in history {
        match message.role {
            MessageRole::System => {}
            MessageRole::User | MessageRole::Assistant => {
                let role = if message.role == MessageRole::Assistant {
                    "assistant"
                } else {
                    "user"
                };
                if let Some(content) = message.content.as_deref().filter(|value| !value.is_empty())
                {
                    input.push(json!({"type":"message","role":role,"content":[{"type": if role == "assistant" {"output_text"} else {"input_text"},"text":content}]}));
                }
                for call in &message.tool_calls {
                    input.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments.to_string()}));
                }
            }
            MessageRole::Tool => {
                if let Some(id) = message.tool_call_id.as_deref() {
                    input.push(json!({"type":"function_call_output","call_id":id,"output":message.content.as_deref().unwrap_or_default()}));
                }
            }
        }
    }
    let mut body = json!({"model":model,"instructions":instructions,"input":input,"reasoning":null,"store":false,"stream":true,"include":[],"client_metadata":{"application":"veyra"}});
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema})).collect());
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }
    body
}

fn parse_codex_sse(body: &str) -> anyhow::Result<AssistantTurn> {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut lines = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            flush_codex_event(&mut lines, &mut text, &mut calls)?;
        } else if let Some(data) = line.strip_prefix("data:") {
            lines.push(data.trim_start().to_owned());
        }
    }
    flush_codex_event(&mut lines, &mut text, &mut calls)?;
    if text.is_empty() && calls.is_empty() {
        anyhow::bail!("empty Codex response");
    }
    Ok(AssistantTurn {
        content: (!text.is_empty()).then_some(text),
        tool_calls: calls,
    })
}

fn flush_codex_event(
    lines: &mut Vec<String>,
    text: &mut String,
    calls: &mut Vec<ToolCall>,
) -> anyhow::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let raw = lines.join("\n");
    lines.clear();
    if raw == "[DONE]" {
        return Ok(());
    }
    let event: Value =
        serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("malformed Codex event"))?;
    match event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                text.push_str(delta);
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                parse_codex_item(item, text, calls)?;
            }
        }
        "response.failed" | "error" => anyhow::bail!("Codex response failed"),
        _ => {}
    }
    Ok(())
}

fn parse_codex_item(
    item: &Value,
    text: &mut String,
    calls: &mut Vec<ToolCall>,
) -> anyhow::Result<()> {
    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "function_call" => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Codex call omitted ID"))?;
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Codex call omitted name"))?;
            let raw = item
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Codex call omitted arguments"))?;
            let arguments = serde_json::from_str(raw)
                .map_err(|_| anyhow::anyhow!("Codex call arguments malformed"))?;
            calls.push(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments,
            });
        }
        "message" if text.is_empty() => {
            if let Some(items) = item.get("content").and_then(Value::as_array) {
                for part in items {
                    if let Some(value) = part.get("text").and_then(Value::as_str) {
                        text.push_str(value);
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn claude_body(
    model: &str,
    instructions: &str,
    history: &[ChatMessage],
    tools: &[ToolDefinition],
    session_id: uuid::Uuid,
    credential: &SubscriptionCredential,
) -> Value {
    let mut messages = Vec::new();
    for message in history {
        match message.role {
            MessageRole::System => {}
            MessageRole::User => push_claude(
                &mut messages,
                "user",
                vec![json!({"type":"text","text":message.content.as_deref().unwrap_or_default()})],
            ),
            MessageRole::Assistant => {
                let mut blocks = Vec::new();
                if let Some(content) = message.content.as_deref().filter(|value| !value.is_empty())
                {
                    blocks.push(json!({"type":"text","text":content}));
                }
                blocks.extend(message.tool_calls.iter().map(|call| json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments})));
                push_claude(&mut messages, "assistant", blocks);
            }
            MessageRole::Tool => {
                if let Some(id) = message.tool_call_id.as_deref() {
                    push_claude(
                        &mut messages,
                        "user",
                        vec![
                            json!({"type":"tool_result","tool_use_id":id,"content":message.content.as_deref().unwrap_or_default()}),
                        ],
                    );
                }
            }
        }
    }
    let account = credential.account_uuid.as_deref().unwrap_or_default();
    json!({
        "model":model,"max_tokens":4096,
        "system":[
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.261.cfa; cc_entrypoint=sdk-cli;"},
            {"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK."},
            {"type":"text","text":instructions}
        ],
        "messages":messages,
        "tools":tools.iter().map(|tool| json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema})).collect::<Vec<_>>(),
        "thinking":{"type":"adaptive","display":"omitted"},
        "context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},
        "output_config":{"effort":"high"},
        "metadata":{"user_id":json!({"device_id":"veyra","account_uuid":account,"session_id":session_id}).to_string()},
        "stream":false
    })
}

fn push_claude(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({"role":role,"content":blocks}));
}

fn parse_claude_response(body: &str) -> anyhow::Result<AssistantTurn> {
    let response: Value =
        serde_json::from_str(body).map_err(|_| anyhow::anyhow!("malformed Claude response"))?;
    let blocks = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Claude content missing"))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                if let Some(value) = block.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Claude call omitted ID"))?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("Claude call omitted name"))?;
                let arguments = block
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()));
                calls.push(ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments,
                });
            }
            _ => {}
        }
    }
    if text.is_empty() && calls.is_empty() {
        anyhow::bail!("empty Claude response");
    }
    Ok(AssistantTurn {
        content: (!text.is_empty()).then_some(text),
        tool_calls: calls,
    })
}

fn account_id_from_jwt(token: &str) -> Option<String> {
    let encoded = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

fn construction(reason: &str) -> ModelError {
    ModelError::Construction {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_tool_round_trip_is_structured() {
        let history = [
            ChatMessage::assistant_with_tools(
                None,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "positions".into(),
                    arguments: json!({}),
                }],
            ),
            ChatMessage::tool("c1", "{\"count\":1}"),
        ];
        let body = codex_body("gpt-test", "system", &history, &[]);
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][1]["type"], "function_call_output");
    }

    #[test]
    fn parses_codex_text_and_tool_calls() {
        let raw = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"positions\",\"arguments\":\"{}\"}}\n\n";
        let turn = parse_codex_sse(raw).expect("valid event stream");
        assert_eq!(turn.content.as_deref(), Some("Hello"));
        assert_eq!(turn.tool_calls[0].name, "positions");
    }

    #[test]
    fn claude_tool_round_trip_is_structured() {
        let history = [
            ChatMessage::assistant_with_tools(
                None,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "positions".into(),
                    arguments: json!({}),
                }],
            ),
            ChatMessage::tool("c1", "done"),
        ];
        let credential = SubscriptionCredential {
            provider: SubscriptionProvider::ClaudeCode,
            access_token: "token".into(),
            id_token: String::new(),
            refresh_token: String::new(),
            account_id: None,
            account_uuid: None,
            organization_uuid: None,
            scopes: vec![],
            account_label: None,
            expires_at_unix: None,
        };
        let body = claude_body(
            "model",
            "system",
            &history,
            &[],
            uuid::Uuid::nil(),
            &credential,
        );
        assert_eq!(body["messages"][0]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Loopback ChatGPT endpoints and a connected application state. Nothing
    //! here reaches the real provider.

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
    use serde_json::Value;

    use crate::AppState;
    use crate::config::{ConfigError, ServiceConfig};
    use crate::model::CooldownRegistry;
    use crate::risk::{RiskGate, RiskPolicy};
    use crate::state::RuntimeState;
    use crate::state::test_support::MemoryState;
    use crate::subscription_auth::{SubscriptionCredential, SubscriptionProvider};

    /// One scripted HTTP reply.
    #[derive(Debug, Clone)]
    pub(crate) struct Reply {
        pub(crate) status: u16,
        pub(crate) headers: Vec<(&'static str, String)>,
        pub(crate) body: String,
    }

    impl Reply {
        pub(crate) fn new(status: u16, body: impl Into<String>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }

        pub(crate) fn header(mut self, name: &'static str, value: &str) -> Self {
            self.headers.push((name, value.to_owned()));
            self
        }

        fn respond(self) -> HttpResponse {
            let status = actix_web::http::StatusCode::from_u16(self.status)
                .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);
            let mut response = HttpResponse::build(status);
            for (name, value) in self.headers {
                response.insert_header((name, value));
            }
            response.body(self.body)
        }
    }

    /// Structured answer in the Codex event-stream shape.
    pub(crate) fn structured_sse(name: &str, arguments: &str) -> String {
        let item = serde_json::json!({
            "type": "response.output_item.done",
            "item": {"type": "function_call", "call_id": "c1", "name": name, "arguments": arguments}
        });
        format!("data: {item}\n\ndata: [DONE]\n\n")
    }

    /// Plain text answer in the Codex event-stream shape.
    pub(crate) fn text_sse(text: &str) -> String {
        let delta = serde_json::json!({"type": "response.output_text.delta", "delta": text});
        format!("data: {delta}\n\n")
    }

    #[derive(Default)]
    pub(crate) struct Recorded {
        responses: Mutex<VecDeque<Reply>>,
        tokens: Mutex<VecDeque<Reply>>,
        pub(crate) bodies: Mutex<Vec<Value>>,
        pub(crate) token_calls: Mutex<usize>,
    }

    async fn responses(state: web::Data<Arc<Recorded>>, body: web::Bytes) -> HttpResponse {
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        state.bodies.lock().expect("lock").push(parsed);
        let reply = state.responses.lock().expect("lock").pop_front();
        reply
            .unwrap_or_else(|| Reply::new(500, "no scripted reply"))
            .respond()
    }

    async fn token(state: web::Data<Arc<Recorded>>, _request: HttpRequest) -> HttpResponse {
        *state.token_calls.lock().expect("lock") += 1;
        let reply = state.tokens.lock().expect("lock").pop_front();
        reply
            .unwrap_or_else(|| Reply::new(500, "no scripted reply"))
            .respond()
    }

    /// A loopback stand-in for the ChatGPT responses and token endpoints.
    pub(crate) struct CodexServer {
        handle: actix_web::dev::ServerHandle,
        pub(crate) base: String,
        pub(crate) token_url: String,
        pub(crate) recorded: Arc<Recorded>,
    }

    impl CodexServer {
        pub(crate) async fn spawn() -> Self {
            let recorded = Arc::new(Recorded::default());
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = listener.local_addr().expect("addr");
            let server = HttpServer::new({
                let recorded = recorded.clone();
                move || {
                    App::new()
                        .app_data(web::Data::new(recorded.clone()))
                        .route("/responses", web::post().to(responses))
                        .route("/oauth/token", web::post().to(token))
                }
            })
            .workers(1)
            .listen(listener)
            .expect("listen")
            .run();
            let handle = server.handle();
            actix_web::rt::spawn(server);
            Self {
                handle,
                base: format!("http://{address}"),
                token_url: format!("http://{address}/oauth/token"),
                recorded,
            }
        }

        pub(crate) fn reply(&self, reply: Reply) {
            self.recorded
                .responses
                .lock()
                .expect("lock")
                .push_back(reply);
        }

        pub(crate) fn token_reply(&self, reply: Reply) {
            self.recorded.tokens.lock().expect("lock").push_back(reply);
        }

        /// The `model` field of every responses request, in order.
        pub(crate) fn models(&self) -> Vec<String> {
            self.recorded
                .bodies
                .lock()
                .expect("lock")
                .iter()
                .map(|body| body["model"].as_str().unwrap_or_default().to_owned())
                .collect()
        }

        pub(crate) fn token_calls(&self) -> usize {
            *self.recorded.token_calls.lock().expect("lock")
        }

        pub(crate) async fn shutdown(self) {
            self.handle.stop(true).await;
        }
    }

    pub(crate) fn config() -> ServiceConfig {
        ServiceConfig::from_source(|name| match name {
            "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
            "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
            "VEYRA_ENV" => Ok("development".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("config must parse")
    }

    pub(crate) fn credential(
        provider: SubscriptionProvider,
        expires_at_unix: Option<i64>,
    ) -> SubscriptionCredential {
        SubscriptionCredential {
            provider,
            access_token: "access-token-for-tests".to_owned(),
            id_token: String::new(),
            refresh_token: "refresh-token-for-tests".to_owned(),
            account_id: Some("account-for-tests".to_owned()),
            account_uuid: None,
            organization_uuid: None,
            scopes: Vec::new(),
            account_label: Some("operator".to_owned()),
            expires_at_unix,
        }
    }

    /// Application state with encrypted storage and an in-memory state
    /// store; `connected` subscriptions hold a non-expiring credential.
    pub(crate) fn app(
        cooldowns: CooldownRegistry,
        connected: &[SubscriptionProvider],
    ) -> (AppState, Arc<MemoryState>) {
        let store = Arc::new(MemoryState::default());
        let app = AppState::new(config(), None, None, RiskGate::new(RiskPolicy::default()))
            .with_credential_vault(Some(crate::credential::test_vault()))
            .with_runtime_state(RuntimeState::new(Some(store.clone())))
            .with_model_cooldowns(cooldowns);
        for provider in connected {
            app.subscription_auth()
                .set_credential(credential(*provider, None));
        }
        (app, store)
    }
}

#[cfg(test)]
mod transport_tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::test_support::{CodexServer, Reply, app, credential, structured_sse, text_sse};
    use super::*;
    use crate::model::cooldown::test_clock::ManualClock;
    use crate::model::settings::ModelSettings;
    use crate::model::{AnswerFormat, CooldownRegistry};

    fn request(tier: ModelTier) -> DecisionRequest {
        DecisionRequest {
            instructions: "Decide.".to_owned(),
            input: "{}".to_owned(),
            format: AnswerFormat {
                name: "decision".to_owned(),
                schema: json!({"type": "object"}),
            },
            tier,
        }
    }

    async fn chatgpt(
        server: &CodexServer,
    ) -> (SubscriptionEngine, AppState, CooldownRegistry, ManualClock) {
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let (app, _store) = app(cooldowns.clone(), &[SubscriptionProvider::Codex]);
        let engine = SubscriptionEngine::build_chatgpt("gpt-6-luna", &app)
            .expect("connected subscription builds")
            .with_codex_endpoints(&server.base, &server.token_url);
        (engine, app, cooldowns, clock)
    }

    #[actix_web::test]
    async fn the_chatgpt_leg_sends_only_its_own_model_for_every_tier() {
        let server = CodexServer::spawn().await;
        let (engine, _app, _cooldowns, _clock) = chatgpt(&server).await;
        for _ in 0..3 {
            server.reply(Reply::new(
                200,
                structured_sse("decision", r#"{"action":"hold"}"#),
            ));
        }
        for tier in [ModelTier::Fast, ModelTier::Balanced, ModelTier::Reasoning] {
            let answer = engine.answer(request(tier)).await.expect("answers");
            assert_eq!(answer.value["action"], "hold");
        }
        assert_eq!(server.models(), ["gpt-6-luna"; 3]);
        assert_eq!(
            engine.last_successful_model().as_deref(),
            Some("chatgpt:gpt-6-luna")
        );
        assert_eq!(DecisionEngine::provider(&engine), ModelProvider::Codex);
        let body = server.recorded.bodies.lock().expect("lock")[0].clone();
        assert_eq!(body["tool_choice"]["name"], "decision");
        server.shutdown().await;
    }

    #[actix_web::test]
    async fn statuses_cool_the_subscription_by_reason_and_honour_retry_after() {
        let server = CodexServer::spawn().await;
        let (engine, _app, cooldowns, clock) = chatgpt(&server).await;

        server.reply(Reply::new(429, "slow down").header("retry-after", "90"));
        let error = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("rate limited");
        assert_eq!(
            error.to_string(),
            "model request failed: chatgpt:gpt-6-luna: rate_limited"
        );
        let entry = &cooldowns.snapshot()[0];
        assert_eq!(
            (
                entry.provider.as_str(),
                entry.model.as_str(),
                entry.reason.as_str()
            ),
            ("codex", "gpt-6-luna", "rate_limited")
        );
        assert_eq!(
            entry.until_ms,
            crate::model::cooldown::epoch_millis(clock.now() + Duration::from_secs(90))
        );

        // Cooling: refused before any request.
        let refused = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("cooling");
        assert!(
            refused
                .to_string()
                .contains("every model candidate is cooling down")
        );
        assert!(engine.preflight(ModelTier::Fast).is_err());
        assert_eq!(server.models().len(), 1);

        for (status, reason, text) in [
            (402, "insufficient_credits", "insufficient_credits"),
            (404, "provider_rejected", "provider_rejected (404)"),
            (403, "unauthorized", "provider_rejected (403)"),
            (500, "overloaded", "provider_rejected (500)"),
            (503, "overloaded", "overloaded"),
        ] {
            cooldowns.clear("test");
            server.reply(Reply::new(status, "refused"));
            let error = engine
                .answer(request(ModelTier::Fast))
                .await
                .expect_err("refused");
            assert_eq!(
                error.to_string(),
                format!("model request failed: chatgpt:gpt-6-luna: {text}")
            );
            assert_eq!(cooldowns.snapshot()[0].reason, reason, "{status}");
        }
        server.shutdown().await;
    }

    #[actix_web::test]
    async fn a_malformed_or_incomplete_answer_is_an_invalid_response() {
        let server = CodexServer::spawn().await;
        let (engine, _app, cooldowns, _clock) = chatgpt(&server).await;
        server.reply(Reply::new(200, "data: {not json}\n\n"));
        let error = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("malformed");
        assert!(
            error
                .to_string()
                .ends_with("chatgpt:gpt-6-luna: invalid_response")
        );
        assert_eq!(cooldowns.snapshot()[0].reason, "invalid_response");

        cooldowns.clear("test");
        server.reply(Reply::new(200, text_sse("no structured answer")));
        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("the structured answer is missing");
        assert_eq!(cooldowns.snapshot()[0].reason, "invalid_response");
        server.shutdown().await;
    }

    #[actix_web::test]
    async fn an_unreachable_subscription_is_held_briefly() {
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let (app, _store) = app(cooldowns.clone(), &[SubscriptionProvider::Codex]);
        // Port 9 on loopback: nothing listens, so the connection is refused.
        let engine = SubscriptionEngine::build_chatgpt("gpt-6-luna", &app)
            .expect("builds")
            .with_codex_endpoints("http://127.0.0.1:9", "http://127.0.0.1:9/oauth/token");
        let error = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("unreachable");
        assert_eq!(
            error.to_string(),
            "model request failed: chatgpt:gpt-6-luna: transport"
        );
        let entry = &cooldowns.snapshot()[0];
        assert_eq!(
            (entry.provider.as_str(), entry.reason.as_str()),
            ("codex", "unreachable")
        );
        assert_eq!(
            entry.until_ms,
            crate::model::cooldown::epoch_millis(clock.now() + Duration::from_secs(60))
        );
    }

    #[actix_web::test]
    async fn a_rejected_refresh_marks_the_subscription_for_reconnection() {
        let server = CodexServer::spawn().await;
        let (engine, app, cooldowns, _clock) = chatgpt(&server).await;
        server.reply(Reply::new(401, "expired"));
        server.token_reply(Reply::new(400, r#"{"error":"invalid_grant"}"#));
        let error = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("the grant is dead");
        assert_eq!(
            error.to_string(),
            "model request failed: chatgpt:gpt-6-luna: unauthorized"
        );
        assert!(
            app.subscription_auth()
                .needs_reconnect(SubscriptionProvider::Codex)
        );
        assert!(
            !app.subscription_auth()
                .connected(SubscriptionProvider::Codex)
        );
        assert_eq!(cooldowns.snapshot()[0].reason, "unauthorized");

        // Even past the cooldown, the dead grant is not refreshed again.
        cooldowns.clear("test");
        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("still needs reconnecting");
        assert_eq!(server.token_calls(), 1, "no refresh storm");
        assert_eq!(server.models().len(), 1);

        // Reconnecting clears the marker.
        app.subscription_auth()
            .set_credential(credential(SubscriptionProvider::Codex, None));
        assert!(
            app.subscription_auth()
                .connected(SubscriptionProvider::Codex)
        );
        server.shutdown().await;
    }

    #[actix_web::test]
    async fn an_expired_token_is_refreshed_and_persisted_before_the_request() {
        let server = CodexServer::spawn().await;
        let clock = ManualClock::new();
        let cooldowns = CooldownRegistry::with_clock(clock.clock());
        let (app, store) = app(cooldowns, &[]);
        app.subscription_auth()
            .set_credential(credential(SubscriptionProvider::Codex, Some(1)));
        let engine = SubscriptionEngine::build_chatgpt("gpt-6-luna", &app)
            .expect("an expired token with a refresh grant is connected")
            .with_codex_endpoints(&server.base, &server.token_url);
        server.token_reply(Reply::new(
            200,
            json!({"access_token": "fresh-access", "refresh_token": "fresh-refresh", "expires_in": 3600, "scope": "openid profile"}).to_string(),
        ));
        server.reply(Reply::new(
            200,
            structured_sse("decision", r#"{"action":"hold"}"#),
        ));
        engine
            .answer(request(ModelTier::Balanced))
            .await
            .expect("answers after refreshing");
        assert_eq!(server.token_calls(), 1);
        let stored = store
            .saved(crate::state::StateKey::SubscriptionCodex)
            .expect("the refreshed grant is persisted");
        assert!(
            !stored.to_string().contains("fresh-access"),
            "only ciphertext is stored"
        );
        let current = app
            .subscription_auth()
            .credential(SubscriptionProvider::Codex)
            .expect("credential");
        assert_eq!(current.access_token, "fresh-access");
        assert_eq!(current.scopes, ["openid", "profile"]);

        // A malformed refresh body is an invalid response, not a reconnect.
        app.subscription_auth()
            .set_credential(credential(SubscriptionProvider::Codex, Some(1)));
        server.token_reply(Reply::new(200, "{}"));
        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("refresh omitted the token");
        assert!(
            !app.subscription_auth()
                .needs_reconnect(SubscriptionProvider::Codex)
        );
        server.shutdown().await;
    }

    #[derive(Default)]
    struct Progress(usize);

    #[async_trait]
    impl ToolProgressSink for Progress {
        async fn tool_started(&mut self, _id: &str, _name: &str, _arguments: &Value) {
            self.0 += 1;
        }
        async fn tool_completed(&mut self, _: &str, _: &str, _: &Value, _: bool) {}
    }

    struct Observe;

    #[async_trait]
    impl ReadOnlyTool for Observe {
        fn definition(&self) -> crate::model::ReadOnlyToolDefinition {
            crate::model::ReadOnlyToolDefinition {
                name: "observe".to_owned(),
                description: "Observe the service.".to_owned(),
                input_schema: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _arguments: Value) -> Result<Value, String> {
            Ok(json!({"ready": true}))
        }
    }

    #[actix_web::test]
    async fn tool_sessions_route_through_the_chatgpt_model() {
        let server = CodexServer::spawn().await;
        let (engine, _app, cooldowns, _clock) = chatgpt(&server).await;
        server.reply(Reply::new(200, text_sse("All systems ready.")));
        let mut progress = Progress::default();
        let answer = engine
            .answer_with_tools(
                request(ModelTier::Fast),
                vec![Arc::new(Observe)],
                &mut progress,
            )
            .await
            .expect("a direct answer");
        assert_eq!(answer.value["answer"], "All systems ready.");
        assert_eq!(server.models(), ["gpt-6-luna"]);
        let body = server.recorded.bodies.lock().expect("lock")[0].clone();
        assert_eq!(body["tools"][0]["name"], "observe");

        server.reply(Reply::new(529, "busy"));
        engine
            .answer_with_tools(
                request(ModelTier::Fast),
                vec![Arc::new(Observe)],
                &mut progress,
            )
            .await
            .expect_err("overloaded");
        assert_eq!(cooldowns.snapshot()[0].reason, "overloaded");

        // An empty tool list is a plain structured answer.
        cooldowns.clear("test");
        server.reply(Reply::new(200, structured_sse("decision", "{}")));
        engine
            .answer_with_tools(request(ModelTier::Fast), Vec::new(), &mut progress)
            .await
            .expect("structured");
        server.shutdown().await;
    }

    #[actix_web::test]
    async fn claude_code_keeps_its_original_loop_without_cooldowns() {
        let cooldowns = CooldownRegistry::new();
        let (app, _store) = app(cooldowns.clone(), &[SubscriptionProvider::ClaudeCode]);
        let settings = ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "claude_code",
                "VEYRA_MODEL_FAST" | "VEYRA_MODEL_BALANCED" | "VEYRA_MODEL_REASONING" => {
                    "claude-model"
                }
                _ => return Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("parses")
        .expect("configured");
        let engine = SubscriptionEngine::build(&settings, &app).expect("connected");
        assert!(engine.cooldowns.is_none());
        assert_eq!(engine.label("claude-model"), "claude-model");
        assert_eq!(DecisionEngine::provider(&engine), ModelProvider::ClaudeCode);
        // Even a cooled entry under its name does not gate it.
        if let crate::model::Admission::Ready(ticket) =
            cooldowns.admit(ModelProvider::ClaudeCode, "claude-model")
        {
            ticket.fail(CooldownFailure::new(CooldownReason::Overloaded));
        }
        engine
            .preflight(ModelTier::Fast)
            .expect("Claude Code is never refused on cooldown grounds");

        let unconfigured = ModelSettings::from_source(|name| {
            Ok(match name {
                "VEYRA_MODEL_PROVIDER" => "codex",
                "VEYRA_MODEL_FAST" | "VEYRA_MODEL_BALANCED" | "VEYRA_MODEL_REASONING" => {
                    "gpt-6-luna"
                }
                _ => return Err(crate::config::ConfigError::MissingEnvironmentVariable { name }),
            }
            .to_owned())
        })
        .expect("parses")
        .expect("configured");
        let error =
            SubscriptionEngine::build(&unconfigured, &app).expect_err("Codex is not connected");
        assert!(
            error
                .to_string()
                .contains("connect the selected subscription first")
        );
    }

    #[test]
    fn retry_after_accepts_delta_seconds_only() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        for (raw, expected) in [
            ("120", Some(Duration::from_secs(120))),
            (" 1.5 ", Some(Duration::from_millis(1_500))),
            ("-3", None),
            ("Wed, 21 Oct 2015 07:28:00 GMT", None),
            ("1e300", None),
        ] {
            headers.insert(
                reqwest::header::RETRY_AFTER,
                reqwest::header::HeaderValue::from_str(raw).expect("header value"),
            );
            assert_eq!(retry_after(&headers), expected, "{raw}");
        }
    }

    #[test]
    fn failure_kinds_map_to_route_effects() {
        let local = SubscriptionFailure::local("subscription refresh cannot be persisted");
        assert_eq!(
            local.to_string(),
            "subscription refresh cannot be persisted"
        );
        assert_eq!(
            classify(&local),
            AttemptError::transport("subscription_state_unavailable")
        );
        assert_eq!(
            classify(&anyhow::anyhow!("tool session exceeded its budget")),
            invalid_response(),
            "an unclassified runtime error is a response another model may beat"
        );
        assert_eq!(
            classify(&TransportError::Unauthorized.into_anyhow()).reason,
            "unauthorized"
        );
        assert_eq!(status_reason(402), "insufficient_credits");
        assert_eq!(status_reason(429), "rate_limited");
        assert_eq!(status_reason(418), "provider_rejected (418)");
    }

    #[actix_web::test]
    async fn a_tool_backed_session_runs_the_tool_and_streams_the_follow_up() {
        let server = CodexServer::spawn().await;
        let (engine, _app, cooldowns, _clock) = chatgpt(&server).await;
        let call = json!({
            "type": "response.output_item.done",
            "item": {"type": "function_call", "call_id": "obs-1", "name": "observe", "arguments": "{}"}
        });
        server.reply(Reply::new(200, format!("data: {call}\n\n")));
        let message = json!({
            "type": "response.output_item.done",
            "item": {"type": "message", "content": [{"type": "output_text", "text": "Ready to trade."}]}
        });
        server.reply(Reply::new(200, format!("data: {message}\n\n")));
        let mut progress = Progress::default();
        let answer = engine
            .answer_with_tools(
                request(ModelTier::Reasoning),
                vec![Arc::new(Observe)],
                &mut progress,
            )
            .await
            .expect("tool-backed answer");
        assert_eq!(answer.value["answer"], "Ready to trade.");
        assert_eq!(progress.0, 1, "the allowlisted tool ran once");
        assert_eq!(server.models(), ["gpt-6-luna", "gpt-6-luna"]);
        let follow_up = server.recorded.bodies.lock().expect("lock")[1].clone();
        let kinds: Vec<&str> = follow_up["input"]
            .as_array()
            .expect("input items")
            .iter()
            .filter_map(|item| item["type"].as_str())
            .collect();
        assert_eq!(kinds, ["message", "function_call", "function_call_output"]);
        assert!(cooldowns.is_empty());
        server.shutdown().await;
    }

    #[actix_web::test]
    async fn an_unauthorized_request_refreshes_once_and_retries() {
        let server = CodexServer::spawn().await;
        let (engine, app, _cooldowns, _clock) = chatgpt(&server).await;
        server.reply(Reply::new(401, "token expired"));
        server.token_reply(Reply::new(
            200,
            json!({"access_token": "rotated-access"}).to_string(),
        ));
        server.reply(Reply::new(
            200,
            structured_sse("decision", r#"{"action":"close"}"#),
        ));
        let answer = engine
            .answer(request(ModelTier::Fast))
            .await
            .expect("the retry with a fresh token answers");
        assert_eq!(answer.value["action"], "close");
        assert_eq!(server.token_calls(), 1);
        assert_eq!(
            app.subscription_auth()
                .credential(SubscriptionProvider::Codex)
                .expect("credential")
                .access_token,
            "rotated-access"
        );
        server.shutdown().await;
    }

    #[actix_web::test]
    async fn a_failed_codex_event_is_an_invalid_response() {
        let server = CodexServer::spawn().await;
        let (engine, _app, cooldowns, _clock) = chatgpt(&server).await;
        server.reply(Reply::new(200, "data: {\"type\":\"response.failed\"}\n\n"));
        engine
            .answer(request(ModelTier::Fast))
            .await
            .expect_err("the provider reported a failed response");
        assert_eq!(cooldowns.snapshot()[0].reason, "invalid_response");
        server.shutdown().await;
    }
}
