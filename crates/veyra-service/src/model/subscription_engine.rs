//! Subscription-backed model transport for Codex and Claude Code.
//!
//! Adapted from the CCS subscription provider protocol (Codecraft Solutions ZA,
//! LicenseRef-Heimdall-FSL). Only this model boundary speaks provider-specific
//! wire formats. Tokens remain in the encrypted runtime store, and the tool
//! registry is supplied by the read-only assistant boundary.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_runtime::{
    AgentProviderKind, AssistantTurn, ChatMessage, EventSink, MessageRole, ModelTiers,
    ProviderInfo, RuntimeEvent, TextProvider, ToolCall, ToolDefinition, ToolRegistry,
    ToolSessionOutcome, ToolSessionRequest, execute_tool_session,
};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as AsyncMutex;

use super::agent_runtime_engine::{RuntimeProgress, RuntimeReadOnlyTool, runtime_definition};
use super::{
    DecisionAnswer, DecisionEngine, DecisionRequest, ModelError, ModelProvider, ModelTier,
    ReadOnlyTool, ToolProgressSink,
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
    last_attempted: Arc<Mutex<Option<String>>>,
    last_successful: Arc<Mutex<Option<String>>>,
    session_id: uuid::Uuid,
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
    pub fn build(
        settings: &super::settings::ModelSettings,
        app: &AppState,
    ) -> Result<Self, ModelError> {
        let provider = match settings.provider() {
            ModelProvider::Codex => SubscriptionProvider::Codex,
            ModelProvider::ClaudeCode => SubscriptionProvider::ClaudeCode,
            _ => return Err(construction("not a subscription provider")),
        };
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
        Ok(Self {
            provider,
            auth: app.subscription_auth().clone(),
            vault,
            state: app.runtime_state().clone(),
            client,
            refresh_lock: AsyncMutex::new(()),
            tiers: settings.tiers().clone(),
            runtime_tiers: ModelTiers::new(
                settings.tiers().resolve(ModelTier::Balanced),
                settings.tiers().resolve(ModelTier::Fast),
                settings.tiers().resolve(ModelTier::Reasoning),
            ),
            last_attempted: Arc::new(Mutex::new(None)),
            last_successful: Arc::new(Mutex::new(None)),
            session_id: uuid::Uuid::new_v4(),
        })
    }

    fn state_key(&self) -> StateKey {
        match self.provider {
            SubscriptionProvider::Codex => StateKey::SubscriptionCodex,
            SubscriptionProvider::ClaudeCode => StateKey::SubscriptionClaudeCode,
        }
    }

    async fn credential(&self, force_refresh: bool) -> anyhow::Result<SubscriptionCredential> {
        let _guard = self.refresh_lock.lock().await;
        let credential = self
            .auth
            .credential(self.provider)
            .ok_or_else(|| anyhow::anyhow!("subscription disconnected"))?;
        let now = now_unix();
        let expired = credential
            .expires_at_unix
            .is_some_and(|expiry| expiry <= now + 300);
        if !force_refresh && !expired {
            return Ok(credential);
        }
        let refreshed = self.refresh(&credential).await?;
        let serialized = serde_json::to_string(&refreshed)
            .map_err(|_| anyhow::anyhow!("subscription credential cannot be serialized"))?;
        let encrypted = self
            .vault
            .seal_text(&serialized)
            .map_err(|_| anyhow::anyhow!("subscription credential cannot be encrypted"))?;
        self.state
            .save_required(self.state_key(), &encrypted)
            .await
            .map_err(|_| anyhow::anyhow!("subscription refresh cannot be persisted"))?;
        self.auth.set_credential(refreshed.clone());
        Ok(refreshed)
    }

    async fn refresh(
        &self,
        old: &SubscriptionCredential,
    ) -> anyhow::Result<SubscriptionCredential> {
        if old.refresh_token.is_empty() {
            anyhow::bail!("subscription needs to be reconnected");
        }
        let response = match self.provider {
            SubscriptionProvider::Codex => self.client.post(CODEX_TOKEN)
                .header("originator", "codex_cli_rs")
                .json(&json!({"client_id": CODEX_CLIENT_ID, "grant_type": "refresh_token", "refresh_token": old.refresh_token}))
                .send().await,
            SubscriptionProvider::ClaudeCode => self.client.post(CLAUDE_TOKEN)
                .json(&json!({"grant_type": "refresh_token", "client_id": CLAUDE_CLIENT_ID, "refresh_token": old.refresh_token, "scope": CLAUDE_SCOPE}))
                .send().await,
        }.map_err(|_| anyhow::anyhow!("subscription token refresh could not reach provider"))?;
        if !response.status().is_success() {
            anyhow::bail!(
                "subscription needs to be reconnected ({})",
                response.status()
            );
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("subscription refresh response is malformed"))?;
        let mut next = old.clone();
        next.access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("subscription refresh omitted access token"))?
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
                    .post(format!("{CODEX_BASE}/responses"))
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
            .map_err(|_| TransportError::Failure("subscription provider unavailable"))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(TransportError::Unauthorized);
        }
        if !status.is_success() {
            return Err(TransportError::Status(status.as_u16()));
        }
        let body = response
            .text()
            .await
            .map_err(|_| TransportError::Failure("subscription response unavailable"))?;
        match self.provider {
            SubscriptionProvider::Codex => parse_codex_sse(&body),
            SubscriptionProvider::ClaudeCode => parse_claude_response(&body),
        }
        .map_err(|_| TransportError::Failure("subscription response malformed"))
    }

    fn remember(slot: &Mutex<Option<String>>, name: &str) {
        match slot.lock() {
            Ok(mut value) => *value = Some(name.to_owned()),
            Err(poisoned) => *poisoned.into_inner() = Some(name.to_owned()),
        }
    }

    fn remembered(slot: &Mutex<Option<String>>) -> Option<String> {
        match slot.lock() {
            Ok(value) => value.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[derive(Debug)]
enum TransportError {
    Unauthorized,
    Status(u16),
    Failure(&'static str),
}

impl TransportError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Unauthorized => anyhow::anyhow!("subscription needs to be reconnected"),
            Self::Status(status) => {
                anyhow::anyhow!("subscription provider rejected request ({status})")
            }
            Self::Failure(reason) => anyhow::anyhow!(reason),
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
impl DecisionEngine for SubscriptionEngine {
    fn provider(&self) -> ModelProvider {
        match self.provider {
            SubscriptionProvider::Codex => ModelProvider::Codex,
            SubscriptionProvider::ClaudeCode => ModelProvider::ClaudeCode,
        }
    }
    fn last_attempted_model(&self) -> Option<String> {
        Self::remembered(&self.last_attempted)
    }
    fn last_successful_model(&self) -> Option<String> {
        Self::remembered(&self.last_successful)
    }

    async fn answer(&self, request: DecisionRequest) -> Result<DecisionAnswer, ModelError> {
        let mut last = "subscription request failed".to_owned();
        for model in self.tiers.chain(request.tier) {
            Self::remember(&self.last_attempted, model);
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
                        Self::remember(&self.last_successful, model);
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
        Self::remember(&self.last_attempted, &model);
        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry.register(RuntimeReadOnlyTool {
                definition: runtime_definition(tool.definition()),
                tool,
            });
        }
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
        Self::remember(&self.last_successful, &model);
        Ok(DecisionAnswer {
            value: json!({"answer": answer}),
        })
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
