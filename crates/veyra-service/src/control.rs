//! Loopback control surface for the active broker's command channel.
//!
//! `POST /intents/check` evaluates a draft through the deterministic risk gate
//! and, only when the gate approves it, asks the terminal to validate the
//! request with `order_check` — a broker-side check that never places an
//! order. `GET /commands/{id}` reports one command's state and validated
//! result without exposing credentials or account balances.
//!
//! Provider mapping lives in the provider module (today `broker/ea.rs`): a
//! second implementation brings its own request type and one match arm here,
//! leaving config, domain, risk, and trading code untouched.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use actix_web::web::{self, Data};
use actix_web::{HttpRequest, HttpResponse, delete, get, post};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::audit::{AuditEvent, AuditKind};
use crate::balance::{BalanceAccount, BalanceHistory, sample_points};
use crate::broker::BrokerLink;
use crate::broker::Symbol;
use crate::broker::{
    CloseOrderRequest, CommandId, CommandPayload, CommandState, ModifyOrderRequest, ORDER_MAGIC,
    OrderHistoryRequest, OrderRequest,
};
use crate::market::{CandleRequest, Timeframe};
use crate::risk::{RiskDecision, RiskRejection};
use crate::state::StateKey;
use crate::trading::{TradeIntent, TradeIntentDraft};

#[post("/intents/check")]
/// Evaluates a draft and, when approved, queues one terminal `order_check`.
///
/// The response carries the deterministic decision and, for approvals, the
/// command id to poll on `GET /commands/{id}`. Nothing is traded: the terminal
/// only validates the request and reports its retcode.
pub async fn check_intent(
    state: Data<AppState>,
    draft: web::Json<TradeIntentDraft>,
) -> HttpResponse {
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    };
    let draft = draft.into_inner();
    let account = crate::routes::account_facts_for_draft(state.as_ref(), &draft).await;
    match state.risk().evaluate(&draft, account, state.now()) {
        RiskDecision::Rejected(rejection) => {
            HttpResponse::Ok().json(RiskDecision::Rejected(rejection))
        }
        RiskDecision::Approved(intent) => {
            let command = link.enqueue_order_check(OrderRequest::from_intent(&intent));
            audit(
                &state,
                AuditKind::CommandQueued,
                json!({
                    "command_id": command.to_string(),
                    "kind": "order_check",
                    "intent_id": intent.id().to_string()
                }),
            )
            .await;
            HttpResponse::Ok().json(json!({
                "decision": "approved",
                "intent_id": intent.id().to_string(),
                "command": "order_check",
                "command_id": command.to_string(),
                "status": "pending"
            }))
        }
    }
}

#[post("/commands/account_snapshot")]
/// Queues the read-only `account_snapshot` command and returns its id.
///
/// Operators use this to refresh venue state (orders, open volume, positions)
/// on demand; it never sends an order.
pub async fn request_account_snapshot(state: Data<AppState>) -> HttpResponse {
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    };
    let command = link.enqueue_account_snapshot();
    audit(
        &state,
        AuditKind::CommandQueued,
        json!({
            "command_id": command.to_string(),
            "kind": "account_snapshot"
        }),
    )
    .await;
    HttpResponse::Ok().json(json!({
        "command": "account_snapshot",
        "command_id": command.to_string(),
        "status": "pending"
    }))
}

/// Returns the live risk policy exactly as the console edits it.
#[get("/risk/policy")]
pub async fn risk_policy(state: Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(state.risk().policy().summary())
}

#[post("/risk/policy")]
/// Applies a validated partial update to the live risk policy.
///
/// The same rules that validate `.env` validate this route, so a console edit
/// can never widen behaviour beyond what a restart would accept. Every change
/// is journaled with the resulting policy, and the kill switch is just one of
/// the fields.
pub async fn update_risk_policy(
    state: Data<AppState>,
    patch: web::Json<crate::risk::RiskPolicyPatch>,
) -> HttpResponse {
    let current = state.risk().policy();
    match current.apply_patch(&patch.into_inner()) {
        Err(error) => HttpResponse::BadRequest().json(json!({
            "error": "invalid_policy",
            "field": error.name,
            "reason": error.reason
        })),
        Ok(updated) => {
            state.risk().update_policy(updated.clone());
            // Persist the effective policy so a restart resumes the operator's
            // intent instead of reverting to the environment baseline.
            match serde_json::to_value(updated.snapshot_patch()) {
                Ok(value) => {
                    state
                        .runtime_state()
                        .save(StateKey::RiskPolicy, &value)
                        .await
                }
                Err(error) => {
                    tracing::warn!(%error, "risk policy snapshot could not be serialized")
                }
            }
            audit(
                &state,
                AuditKind::RiskPolicyUpdated,
                json!({
                    "origin": "control_surface",
                    "policy": updated.summary()
                }),
            )
            .await;
            HttpResponse::Ok().json(updated.summary())
        }
    }
}

/// Returns every live setting with its effective value.
///
/// `overridden` marks the ones an operator has moved away from the deployed
/// baseline; `applies` says whether a change lands immediately or waits for a
/// restart, so the console never implies an edit took effect when it did not.
#[get("/config")]
pub async fn runtime_config(state: Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "settings": state.runtime_config().effective(),
        "live_sections": crate::runtime_config::LIVE_SECTIONS,
        "secret_store": state.credential_vault().is_some(),
        "secrets": { "VEYRA_MODEL_API_KEY": state.runtime_config().model_key_status() },
    }))
}

#[derive(Debug, Deserialize)]
struct CredentialPatch {
    key: String,
}

#[derive(Debug, Deserialize)]
struct SubscriptionProviderPatch {
    provider: String,
}

#[derive(Debug, Deserialize)]
struct SubscriptionCompletePatch {
    provider: String,
    callback_value: String,
}

fn subscription_provider(
    value: &str,
) -> Result<crate::subscription_auth::SubscriptionProvider, Box<HttpResponse>> {
    crate::subscription_auth::SubscriptionProvider::parse(value).ok_or_else(|| {
        Box::new(
            HttpResponse::BadRequest().json(json!({"error": "unsupported_subscription_provider"})),
        )
    })
}

fn subscription_state_key(provider: crate::subscription_auth::SubscriptionProvider) -> StateKey {
    match provider {
        crate::subscription_auth::SubscriptionProvider::Codex => StateKey::SubscriptionCodex,
        crate::subscription_auth::SubscriptionProvider::ClaudeCode => {
            StateKey::SubscriptionClaudeCode
        }
    }
}

fn active_subscription_selected(
    state: &AppState,
    provider: crate::subscription_auth::SubscriptionProvider,
) -> bool {
    let selected = match provider {
        crate::subscription_auth::SubscriptionProvider::Codex => crate::model::ModelProvider::Codex,
        crate::subscription_auth::SubscriptionProvider::ClaudeCode => {
            crate::model::ModelProvider::ClaudeCode
        }
    };
    crate::model::ModelSettings::from_source(state.runtime_config().source())
        .ok()
        .flatten()
        .is_some_and(|settings| settings.provider() == selected)
}

/// Returns non-secret subscription connection status.
#[get("/model/subscriptions")]
pub async fn model_subscriptions(state: Data<AppState>) -> HttpResponse {
    let status = |provider| json!({"connected": state.subscription_auth().connected(provider), "account_label": state.subscription_auth().credential(provider).and_then(|credential| credential.account_label)});
    HttpResponse::Ok().json(json!({"subscriptions": {"codex": status(crate::subscription_auth::SubscriptionProvider::Codex), "claude_code": status(crate::subscription_auth::SubscriptionProvider::ClaudeCode)}}))
}

/// Starts one provider's browser PKCE authorization flow.
#[post("/model/subscriptions/start")]
pub async fn start_model_subscription(
    state: Data<AppState>,
    request: HttpRequest,
    body: web::Json<SubscriptionProviderPatch>,
) -> HttpResponse {
    if let Some(response) = credential_rejection(&request, &state) {
        return response;
    }
    let provider = match subscription_provider(&body.provider) {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    match crate::subscription_auth::prepare(provider) {
        Ok(pending) => {
            let response = json!({"provider": provider.as_str(), "authorize_url": pending.authorize_url, "state": pending.state});
            state.subscription_auth().put_pending(pending);
            HttpResponse::Ok().json(response)
        }
        Err(_) => HttpResponse::InternalServerError()
            .json(json!({"error": "subscription_authorization_unavailable"})),
    }
}

/// Completes a provider browser flow and durably encrypts the resulting token.
#[post("/model/subscriptions/complete")]
pub async fn complete_model_subscription(
    state: Data<AppState>,
    request: HttpRequest,
    body: web::Json<SubscriptionCompletePatch>,
) -> HttpResponse {
    if let Some(response) = credential_rejection(&request, &state) {
        return response;
    }
    let provider = match subscription_provider(&body.provider) {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    let (code, callback_state) =
        match crate::subscription_auth::callback_parts(provider, &body.callback_value) {
            Ok(parts) => parts,
            Err(_) => {
                return HttpResponse::BadRequest()
                    .json(json!({"error": "invalid_subscription_callback"}));
            }
        };
    let pending = match state
        .subscription_auth()
        .take_pending(provider, &callback_state)
    {
        Ok(pending) => pending,
        Err(_) => {
            return HttpResponse::BadRequest().json(json!({"error": "invalid_subscription_state"}));
        }
    };
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return HttpResponse::InternalServerError()
                .json(json!({"error": "subscription_client_unavailable"}));
        }
    };
    let credential = match crate::subscription_auth::exchange(&client, &pending, &code).await {
        Ok(credential) => credential,
        Err(_) => {
            return HttpResponse::BadGateway()
                .json(json!({"error": "subscription_exchange_failed"}));
        }
    };
    let Some(vault) = state.credential_vault() else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({"error": "credential_store_unavailable"}));
    };
    let serialized = match serde_json::to_string(&credential) {
        Ok(value) => value,
        Err(_) => {
            return HttpResponse::InternalServerError()
                .json(json!({"error": "credential_serialization_failed"}));
        }
    };
    let encrypted = match vault.seal_text(&serialized) {
        Ok(value) => value,
        Err(_) => {
            return HttpResponse::InternalServerError()
                .json(json!({"error": "credential_encryption_failed"}));
        }
    };
    if state
        .runtime_state()
        .save_required(subscription_state_key(provider), &encrypted)
        .await
        .is_err()
    {
        return HttpResponse::ServiceUnavailable()
            .json(json!({"error": "credential_storage_failed"}));
    }
    let label = credential.account_label.clone();
    state.subscription_auth().set_credential(credential);
    if active_subscription_selected(&state, provider) {
        let pending = std::collections::BTreeMap::new();
        let applied = crate::runtime_config::validate(state.runtime_config(), &pending)
            .map_err(|_| "model_settings_incomplete")
            .and_then(|staged| {
                crate::runtime_config::adopt(&state, staged).map_err(|_| "model_rebuild_failed")
            });
        if let Err(reason) = applied {
            state.set_model(None);
            chatgpt_connection_changed(&state, provider, "ChatGPT subscription connected");
            return HttpResponse::Accepted().json(json!({"connected": true, "active": false, "provider": provider.as_str(), "account_label": label, "reason": reason}));
        }
    } else {
        rebuild_for_chatgpt_preference(&state, provider);
    }
    chatgpt_connection_changed(&state, provider, "ChatGPT subscription connected");
    HttpResponse::Ok().json(json!({"connected": true, "active": active_subscription_selected(&state, provider), "provider": provider.as_str(), "account_label": label}))
}

/// Rebuilds the model route after the ChatGPT subscription connects or
/// disconnects while another provider is selected, so the subscription-first
/// preference follows the connection. Claude Code never takes part.
fn rebuild_for_chatgpt_preference(
    state: &AppState,
    provider: crate::subscription_auth::SubscriptionProvider,
) {
    if provider != crate::subscription_auth::SubscriptionProvider::Codex {
        return;
    }
    // Only an API provider can put the subscription in front of its chain; a
    // selected subscription runtime is left exactly as it is.
    let api_provider = crate::model::ModelSettings::from_source(state.runtime_config().source())
        .ok()
        .flatten()
        .is_some_and(|settings| {
            !matches!(
                settings.provider(),
                crate::model::ModelProvider::Codex | crate::model::ModelProvider::ClaudeCode
            )
        });
    if !api_provider {
        return;
    }
    if let Err(reason) = crate::runtime_config::rebuild_model(state) {
        tracing::warn!(%reason, "model route was not rebuilt after a ChatGPT subscription change");
    }
}

/// A ChatGPT connection change invalidates every cooldown: the route itself
/// is different now.
fn chatgpt_connection_changed(
    state: &AppState,
    provider: crate::subscription_auth::SubscriptionProvider,
    cause: &str,
) {
    if provider == crate::subscription_auth::SubscriptionProvider::Codex {
        state.model_cooldowns().clear(cause);
    }
}

/// Removes a stored subscription credential.
#[delete("/model/subscriptions/{provider}")]
pub async fn delete_model_subscription(
    state: Data<AppState>,
    request: HttpRequest,
    path: web::Path<String>,
) -> HttpResponse {
    if let Some(response) = credential_rejection(&request, &state) {
        return response;
    }
    let provider = match subscription_provider(&path.into_inner()) {
        Ok(provider) => provider,
        Err(response) => return *response,
    };
    let tombstone = json!({"version": 1, "ciphertext": null});
    if state
        .runtime_state()
        .save_required(subscription_state_key(provider), &tombstone)
        .await
        .is_err()
    {
        return HttpResponse::ServiceUnavailable()
            .json(json!({"error": "credential_storage_failed"}));
    }
    let deleted = state.subscription_auth().remove_credential(provider);
    if active_subscription_selected(&state, provider) {
        state.set_model(None);
    } else {
        rebuild_for_chatgpt_preference(&state, provider);
    }
    chatgpt_connection_changed(&state, provider, "ChatGPT subscription disconnected");
    HttpResponse::Ok().json(json!({"deleted": deleted, "provider": provider.as_str()}))
}

/// Returns the model cooldowns now in force, soonest retry first.
#[get("/model/cooldowns")]
pub async fn model_cooldowns(state: Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(json!({"model_cooldowns": state.model_cooldowns().snapshot()}))
}

/// Clears every model cooldown so the next call tries each candidate again,
/// for an operator who has just fixed the cause (topped up credits, allowed a
/// provider). Returns the now-empty list.
#[post("/model/cooldowns/clear")]
pub async fn clear_model_cooldowns(state: Data<AppState>) -> HttpResponse {
    let cleared = state.model_cooldowns().clear("operator request");
    HttpResponse::Ok().json(json!({
        "cleared": cleared,
        "model_cooldowns": state.model_cooldowns().snapshot()
    }))
}

fn credential_rejection(request: &HttpRequest, state: &AppState) -> Option<HttpResponse> {
    let Some(vault) = state.credential_vault() else {
        return Some(HttpResponse::ServiceUnavailable().json(json!({
            "error": "credential_store_unavailable",
            "reason": "Configure console secret storage before saving a model key."
        })));
    };
    let supplied = request
        .headers()
        .get("x-veyra-admin-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !vault.authenticates(supplied) {
        return Some(HttpResponse::Unauthorized().json(json!({"error": "invalid_operator_token"})));
    }
    None
}

async fn persist_model_credential(
    state: &AppState,
    key: Option<crate::model::ApiKey>,
) -> HttpResponse {
    let Some(vault) = state.credential_vault() else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({"error": "credential_store_unavailable"}));
    };
    let encrypted = match &key {
        Some(key) => match vault.seal(key) {
            Ok(value) => value,
            Err(_) => {
                return HttpResponse::InternalServerError()
                    .json(json!({"error": "credential_encryption_failed"}));
            }
        },
        None => json!({"version": 1, "ciphertext": null}),
    };
    if state
        .runtime_state()
        .save_required(StateKey::ModelSecret, &encrypted)
        .await
        .is_err()
    {
        return HttpResponse::ServiceUnavailable()
            .json(json!({"error": "credential_storage_failed"}));
    }
    state.runtime_config().set_model_key(key);
    let pending = std::collections::BTreeMap::new();
    let applied = match crate::runtime_config::validate(state.runtime_config(), &pending) {
        Ok(staged) => {
            crate::runtime_config::adopt(state, staged).map_err(|_| "model_rebuild_failed")
        }
        Err(_) => Err("model_settings_incomplete"),
    };
    // A different credential can reach models the old one could not (or pay
    // for them), so nothing the old key learned is evidence any more.
    state.model_cooldowns().clear("model credential changed");
    if let Err(reason) = applied {
        // A saved key must never leave an engine with a superseded credential
        // running. The operator can complete settings and re-enable it.
        state.set_model(None);
        return HttpResponse::Accepted().json(json!({
            "saved": true,
            "active": false,
            "reason": reason,
            "secret": state.runtime_config().model_key_status()
        }));
    }
    HttpResponse::Ok().json(json!({
        "saved": true,
        "active": state.model().is_some(),
        "secret": state.runtime_config().model_key_status()
    }))
}

/// Saves a model API key in encrypted durable state. The operator token is
/// separate from the normal settings path and is never returned or journaled.
#[post("/model/credential")]
pub async fn set_model_credential(
    state: Data<AppState>,
    request: HttpRequest,
    body: web::Json<CredentialPatch>,
) -> HttpResponse {
    if let Some(response) = credential_rejection(&request, &state) {
        return response;
    }
    if body.key.len() > 4_096 {
        return HttpResponse::BadRequest().json(json!({"error": "invalid_model_key"}));
    }
    let key = match crate::model::ApiKey::parse(&body.key) {
        Ok(key) => key,
        Err(_) => return HttpResponse::BadRequest().json(json!({"error": "invalid_model_key"})),
    };
    persist_model_credential(&state, Some(key)).await
}

/// Removes the console credential; an environment key, if present, becomes
/// effective again. A failed model rebuild leaves model decisions disabled.
#[delete("/model/credential")]
pub async fn delete_model_credential(state: Data<AppState>, request: HttpRequest) -> HttpResponse {
    if let Some(response) = credential_rejection(&request, &state) {
        return response;
    }
    persist_model_credential(&state, None).await
}

/// Applies a validated partial update to the live settings.
///
/// Validation is deliberately not written here: a proposed edit is layered
/// over the current overlay and every affected section is re-parsed through
/// the same `from_source` that validates `.env`. A console edit therefore
/// cannot widen behaviour beyond what a restart would accept, and an
/// acceptance rule only ever exists in one place.
///
/// Nothing is committed until every section parses, so a patch touching three
/// settings with one bad value changes none of them.
#[post("/config")]
pub async fn update_runtime_config(
    state: Data<AppState>,
    patch: web::Json<serde_json::Map<String, serde_json::Value>>,
) -> HttpResponse {
    let patch = patch.into_inner();
    if patch.is_empty() {
        return HttpResponse::BadRequest().json(json!({
            "error": "empty_patch",
            "reason": "supply at least one setting to change"
        }));
    }

    let accepted = match state.runtime_config().screen(&patch) {
        Ok(accepted) => accepted,
        Err(rejected) => {
            return HttpResponse::BadRequest().json(json!({
                "error": "invalid_settings",
                "rejected": rejected
                    .iter()
                    .map(|edit| json!({"field": edit.name, "reason": edit.reason}))
                    .collect::<Vec<_>>()
            }));
        }
    };

    // Re-parse every affected section against the proposed overlay before any
    // of it is committed.
    let staged = match crate::runtime_config::validate(state.runtime_config(), &accepted) {
        Ok(staged) => staged,
        Err(edit) => {
            return HttpResponse::BadRequest().json(json!({
                "error": "invalid_settings",
                "rejected": [{"field": edit.name, "reason": edit.reason}]
            }));
        }
    };

    let changed: Vec<String> = accepted.keys().cloned().collect();
    state.runtime_config().commit(accepted);

    if let Err(reason) = crate::runtime_config::adopt(&state, staged) {
        // The overlay is already committed, so the edit is not lost: it will
        // be picked up on the next start even though the swap failed here.
        return HttpResponse::InternalServerError().json(json!({
            "error": "settings_saved_but_not_applied",
            "reason": reason
        }));
    }

    let snapshot = state.runtime_config().snapshot();
    state
        .runtime_state()
        .save(StateKey::RuntimeConfig, &snapshot)
        .await;

    audit(
        &state,
        AuditKind::RuntimeConfigUpdated,
        json!({
            "origin": "control_surface",
            "changed": changed,
            "settings": snapshot
        }),
    )
    .await;

    HttpResponse::Ok().json(json!({
        "changed": changed,
        "settings": state.runtime_config().effective()
    }))
}

/// Query for `GET /commands`.
#[derive(Debug, Deserialize)]
pub struct CommandsQuery {
    /// Newest commands to return (1-100); defaults to 20.
    pub limit: Option<u32>,
}

#[get("/commands")]
/// Lists recent commands with lifecycle status and bounded results.
///
/// Read-only: nothing is delivered or executed. Completed payloads are
/// summarised (counts and verdicts), never raw account balances.
pub async fn command_list(state: Data<AppState>, query: web::Query<CommandsQuery>) -> HttpResponse {
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    };
    let limit = query.limit.unwrap_or(20).clamp(1, 100) as usize;
    let commands: Vec<serde_json::Value> = link
        .recent_commands(limit)
        .into_iter()
        .map(|listed| {
            json!({
                "id": listed.id.to_string(),
                "kind": listed.kind.as_str(),
                "status": listed.status,
                "summary": listed.summary,
                "reason": listed.reason
            })
        })
        .collect();
    HttpResponse::Ok().json(json!({ "commands": commands }))
}

/// Query for `GET /events`.
#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// Cursor: return events with a sequence number greater than this.
    pub after: Option<u64>,
    /// Maximum events per response (1-200); defaults to 50.
    pub limit: Option<u32>,
    /// Long-poll window in milliseconds (0-25000); defaults to 15000.
    /// Ignored when no cursor is supplied, because the tail returns at once.
    pub wait_ms: Option<u64>,
}

#[get("/events")]
/// Live event feed for consoles: recent audit events with sequence cursors.
///
/// With no cursor the buffered tail returns immediately; with a cursor the
/// request long-polls up to the wait window, so a console sees events within
/// about 250 ms of recording without polling the durable trail.
pub async fn event_feed(state: Data<AppState>, query: web::Query<EventsQuery>) -> HttpResponse {
    let Some(runtime) = state.audit() else {
        return HttpResponse::ServiceUnavailable().json(json!({ "error": "audit_unavailable" }));
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 200) as usize;
    let latest = runtime.feed_latest();
    let (after, wait) = match query.after {
        Some(after) => (
            after,
            Duration::from_millis(query.wait_ms.unwrap_or(15_000).min(25_000)),
        ),
        None => (latest.saturating_sub(limit as u64), Duration::ZERO),
    };
    let events = runtime.feed_after(after, limit, wait).await;
    let next = events.last().map(|event| event.seq).unwrap_or(after);
    HttpResponse::Ok().json(json!({
        "events": events
            .iter()
            .map(|event| json!({
                "seq": event.seq,
                "at_ms": event.at_ms,
                "kind": event.kind.as_str(),
                "payload": event.payload
            }))
            .collect::<Vec<_>>(),
        "latest": runtime.feed_latest(),
        "next": next
    }))
}

#[get("/account")]
/// Owner-facing account state: money, exposure, and both control switches.
///
/// Served on the loopback control surface only. Exposing it beyond loopback
/// (for example through the tunnel) requires authentication first.
pub async fn account_state(state: Data<AppState>) -> HttpResponse {
    let Some(broker) = state.broker() else {
        return HttpResponse::ServiceUnavailable().json(json!({ "error": "broker_unavailable" }));
    };
    let link = broker.link();
    let report = link.report().await;
    let snapshot = report.snapshot.as_ref();
    let mut body = json!({
        "fresh": report.fresh,
        "connected": snapshot.is_some_and(|snapshot| snapshot.connected()),
        "tradeAllowed": snapshot.is_some_and(|snapshot| snapshot.trade_allowed()),
        "liveOrders": snapshot.is_some_and(|snapshot| snapshot.live_orders()),
    });
    if let Some(snapshot) = snapshot {
        body["login"] = json!(snapshot.login().value());
        body["server"] = json!(snapshot.server().as_str());
        body["symbol"] = json!(snapshot.symbol().as_str());
    }
    body["ageSecs"] = json!(
        link.last_account_age(SystemTime::now())
            .map(|age| age.as_secs())
            .unwrap_or(0)
    );
    if let Some(account) = link.last_account() {
        body["balance"] = json!(account.balance);
        body["equity"] = json!(account.equity);
        body["freeMargin"] = json!(account.free_margin);
        body["marginLevel"] = json!(account.margin_level);
        body["leverage"] = json!(account.leverage);
        body["orders"] = json!(account.orders);
        body["lots"] = json!(account.lots);
        body["positions"] = json!(account.positions);
        body["positionsTruncated"] = json!(account.positions_truncated);
        body["serverTime"] = json!(account.server_time);
    }
    HttpResponse::Ok().json(body)
}

/// Optional lookback window for real broker-balance observations.
#[derive(Debug, Deserialize)]
pub struct BalanceHistoryQuery {
    /// Days to include, from 1 through 365; defaults to 30.
    pub days: Option<u16>,
}

#[get("/account/balance-history")]
/// Reads persisted `AccountBalance()` observations for the active account.
///
/// This never enqueues a broker command and never reconstructs balances from
/// trade P/L. Balances include deposits, withdrawals, and non-Veyra activity.
pub async fn balance_history(
    state: Data<AppState>,
    query: web::Query<BalanceHistoryQuery>,
) -> HttpResponse {
    let days = query.days.unwrap_or(30);
    if !(1..=365).contains(&days) {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid_days" }));
    }
    let report = if let Some(broker) = state.broker() {
        Some(broker.link().report().await)
    } else {
        None
    };
    let account = report
        .as_ref()
        .and_then(|report| report.snapshot.as_ref())
        .map(|snapshot| BalanceAccount {
            login: snapshot.login().value(),
            server: snapshot.server().as_str().to_owned(),
        });
    let retention_days = state.config().audit_retention_days();
    let Some(audit) = state.audit() else {
        return HttpResponse::Ok().json(BalanceHistory {
            status: "disabled",
            source: "broker_balance",
            account,
            days,
            retention_days,
            currency: None,
            points: Vec::new(),
            first_observed_at_ms: None,
            last_observed_at_ms: None,
            sampled: false,
            fresh: false,
        });
    };
    let Some(account) = account else {
        return HttpResponse::Ok().json(BalanceHistory {
            status: "waiting_for_account",
            source: "broker_balance",
            account: None,
            days,
            retention_days,
            currency: None,
            points: Vec::new(),
            first_observed_at_ms: None,
            last_observed_at_ms: None,
            sampled: false,
            fresh: false,
        });
    };
    let now_ms = state
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let since_ms = now_ms.saturating_sub(u64::from(days) * 86_400_000);
    let points = match audit
        .trail()
        .balance_history(account.login, &account.server, since_ms)
        .await
    {
        Ok(points) => points,
        Err(error) => {
            tracing::warn!(%error, "balance history read failed");
            return HttpResponse::ServiceUnavailable()
                .json(json!({ "error": "balance_history_unavailable" }));
        }
    };
    let (points, sampled) = sample_points(points);
    let first_observed_at_ms = points.first().map(|point| point.at_ms);
    let last_observed_at_ms = points.last().map(|point| point.at_ms);
    let fresh = report.as_ref().is_some_and(|report| {
        report.fresh
            && report
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.connected())
    }) && last_observed_at_ms
        .is_some_and(|at_ms| now_ms.saturating_sub(at_ms) <= 90_000);
    HttpResponse::Ok().json(BalanceHistory {
        status: "ok",
        source: "broker_balance",
        account: Some(account),
        days,
        retention_days,
        currency: None,
        points,
        first_observed_at_ms,
        last_observed_at_ms,
        sampled,
        fresh,
    })
}

/// Query for `GET /market/candles`; every field is optional.
#[derive(Debug, Deserialize)]
pub struct CandleQuery {
    /// Instrument; defaults to the terminal's chart symbol.
    pub symbol: Option<String>,
    /// Timeframe name (M1 through MN1) or standard minutes; defaults to `H4`.
    pub timeframe: Option<String>,
    /// Closed candles to return (1-240); defaults to 48.
    pub bars: Option<u16>,
}

#[get("/market/candles")]
/// Returns recent closed candles from the active market feed.
///
/// Read-only: at most one `rates` command is queued on the control channel and
/// no order path is touched. Invalid symbols, timeframes, or windows are
/// rejected before anything is queued, and a terminal that does not answer
/// within the configured window is reported as a gateway failure.
pub async fn market_candles(state: Data<AppState>, query: web::Query<CandleQuery>) -> HttpResponse {
    let Some(runtime) = state.market() else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "market_feed_unavailable" }));
    };
    let symbol = match query.symbol.as_deref() {
        Some(raw) => match Symbol::parse(raw) {
            Ok(symbol) => symbol,
            Err(_) => {
                return HttpResponse::BadRequest().json(json!({ "error": "invalid_symbol" }));
            }
        },
        None => match default_symbol(&state).await {
            Some(symbol) => symbol,
            None => {
                return HttpResponse::Conflict().json(json!({ "error": "symbol_unavailable" }));
            }
        },
    };
    let timeframe = match query.timeframe.as_deref() {
        Some(raw) => match Timeframe::parse(raw) {
            Some(timeframe) => timeframe,
            None => {
                return HttpResponse::BadRequest().json(json!({ "error": "invalid_timeframe" }));
            }
        },
        None => Timeframe::H4,
    };
    let request = match CandleRequest::new(symbol, timeframe, query.bars.unwrap_or(48)) {
        Ok(request) => request,
        Err(error) => {
            return HttpResponse::BadRequest()
                .json(json!({ "error": "invalid_window", "reason": error.to_string() }));
        }
    };
    match runtime.feed().candles(request).await {
        Ok(series) => HttpResponse::Ok().json(json!({
            "symbol": series.symbol().as_str(),
            "timeframe": series.timeframe().as_str(),
            "candles": series
                .candles()
                .iter()
                .map(|candle| json!({
                    "time": candle.time(),
                    "open": candle.open(),
                    "high": candle.high(),
                    "low": candle.low(),
                    "close": candle.close(),
                    "volume": candle.volume()
                }))
                .collect::<Vec<_>>()
        })),
        Err(error) => HttpResponse::BadGateway()
            .json(json!({ "error": "market_feed_failed", "reason": error.to_string() })),
    }
}

#[get("/market/sessions")]
/// Returns the standard trading week and our entry policy.
///
/// Read-only and computed from the clock: the FX/metals week opens Sunday
/// 21:00 UTC, closes Friday 21:00 UTC, and pauses daily 21:00-22:00 UTC
/// Monday through Thursday. The `entries` block reports whether *our* policy
/// currently admits entries (rollover blackout, Friday cutoff, Sunday reopen,
/// or the configured session window), so the console can show both the market
/// and the bot's own hours. The `weekend` block reports the open-position
/// preference and the countdown to the pre-close checkpoint, which runs in the
/// final two hours before Friday's close.
pub async fn market_sessions(state: Data<AppState>) -> HttpResponse {
    use crate::risk::window;

    let now = state.now();
    let Some(session) = window::market_session(now) else {
        return HttpResponse::ServiceUnavailable().json(json!({ "error": "clock_unavailable" }));
    };
    let block = window::entry_block(now, state.risk().policy().session());
    let policy = state.risk().policy();
    let now_unix = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    HttpResponse::Ok().json(json!({
        "now": now_unix,
        "market": {
            "state": session.state.as_str(),
            "nextEvent": session.next_event.as_str(),
            "nextAt": session.next_at
        },
        "entries": {
            "open": block.is_none(),
            "blockedBy": block.map(|block| block.as_str()),
            "detail": block.map(|block| block.detail())
        },
        "policy": {
            "rolloverBlackout": {
                "startMinute": window::ROLLOVER_START_MINUTE,
                "endMinute": window::ROLLOVER_END_MINUTE
            },
            "fridayEntryCutoffMinute": window::FRIDAY_CUTOFF_MINUTE,
            "sundayEntryOpenMinute": window::SUNDAY_OPEN_MINUTE
        },
        "weekend": {
            "policy": policy.weekend_positions().as_str(),
            "closesInSecs": window::weekend_prep(now).map(|prep| prep.closes_in_secs)
        }
    }))
}

/// Query for the realized-performance window.
#[derive(Debug, Deserialize)]
pub struct PerformanceQuery {
    /// Days of account history to include (1-365); defaults to 30.
    pub days: Option<u32>,
}

#[get("/performance")]
/// Returns realized performance from the venue's closed orders.
///
/// Read-only: one `order_history` command is queued for the Veyra magic
/// number and awaited, so the numbers come from actual fills (profit + swap +
/// commission) rather than floating snapshots. Invalid windows are rejected
/// before anything is queued, and a terminal that does not answer within the
/// configured window is reported as a gateway failure.
pub async fn performance(
    state: Data<AppState>,
    query: web::Query<PerformanceQuery>,
) -> HttpResponse {
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable().json(json!({ "error": "broker_unavailable" }));
    };
    let days = query.days.unwrap_or(OrderHistoryRequest::DEFAULT_DAYS);
    let request = match OrderHistoryRequest::new(days, ORDER_MAGIC) {
        Ok(request) => request,
        Err(error) => {
            return HttpResponse::BadRequest()
                .json(json!({ "error": "invalid_window", "reason": error.to_string() }));
        }
    };
    let id = link.enqueue_order_history(request);
    match link.await_command(id, Duration::from_secs(20)).await {
        CommandState::Completed {
            payload: CommandPayload::OrderHistory(history),
        } => {
            let report = crate::performance::summarize(&history.orders);
            HttpResponse::Ok().json(json!({
                "days": days,
                "report": report,
                "trades": history.orders,
                "total": history.total,
                "truncated": history.truncated
            }))
        }
        CommandState::Completed { .. } => HttpResponse::BadGateway().json(json!({
            "error": "history_failed",
            "reason": "history command completed with a different payload"
        })),
        CommandState::Failed { reason } => {
            HttpResponse::BadGateway().json(json!({ "error": "history_failed", "reason": reason }))
        }
        CommandState::Pending => HttpResponse::BadGateway().json(json!({
            "error": "history_failed",
            "reason": "history command still pending after the await window"
        })),
    }
}

/// Query for `GET /trades`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradesQuery {
    /// Days of account history to include (1-365); defaults to 30.
    pub days: Option<u32>,
    /// 1-based page of the newest-first list; defaults to 1.
    pub page: Option<u32>,
    /// Trades per page (1-100); defaults to 20.
    pub page_size: Option<u32>,
}

#[get("/trades")]
/// Returns closed Veyra trades with why each one closed.
///
/// Read-only: queues the same `order_history` command `/performance` uses,
/// for the Veyra magic number, with the same 20 s await, 1-365 day window,
/// and 256-order terminal cap. [`crate::trades::build`] then joins each
/// fill on the requested page (`page`, `pageSize`) with the audit trail's
/// journal-linking evidence (the entry decision,
/// any recorded stop moves, and any recorded close) to classify why it
/// closed; nothing here enqueues a second command or reaches an order path.
pub async fn trades(state: Data<AppState>, query: web::Query<TradesQuery>) -> HttpResponse {
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable().json(json!({ "error": "broker_unavailable" }));
    };
    let page = match crate::trades::TradePage::new(query.page, query.page_size) {
        Ok(page) => page,
        Err(error) => {
            return HttpResponse::BadRequest()
                .json(json!({ "error": "invalid_page", "reason": error.to_string() }));
        }
    };
    let days = query.days.unwrap_or(OrderHistoryRequest::DEFAULT_DAYS);
    let request = match OrderHistoryRequest::new(days, ORDER_MAGIC) {
        Ok(request) => request,
        Err(error) => {
            return HttpResponse::BadRequest()
                .json(json!({ "error": "invalid_window", "reason": error.to_string() }));
        }
    };
    let id = link.enqueue_order_history(request);
    match link.await_command(id, Duration::from_secs(20)).await {
        CommandState::Completed {
            payload: CommandPayload::OrderHistory(history),
        } => {
            let report = crate::trades::build(&state, &history, page).await;
            HttpResponse::Ok().json(json!({
                "days": days,
                "truncated": history.truncated,
                "total": history.total,
                "page": page.number(),
                "pageSize": page.size(),
                "pageCount": report.page_count,
                "brokerOffsetSecs": report.broker_offset_secs,
                "summary": report.summary,
                "trades": report.trades
            }))
        }
        CommandState::Completed { .. } => HttpResponse::BadGateway().json(json!({
            "error": "history_failed",
            "reason": "history command completed with a different payload"
        })),
        CommandState::Failed { reason } => {
            HttpResponse::BadGateway().json(json!({ "error": "history_failed", "reason": reason }))
        }
        CommandState::Pending => HttpResponse::BadGateway().json(json!({
            "error": "history_failed",
            "reason": "history command still pending after the await window"
        })),
    }
}

/// Query for the economic calendar window.
#[derive(Debug, Deserialize)]
pub struct CalendarQuery {
    /// Hours ahead to list (1-168); defaults to 24.
    pub hours: Option<u32>,
}

#[get("/calendar")]
/// Returns the scheduled events the entry path sees for the requested window.
///
/// Read-only: the provider is fetched at most once per cached window and no
/// order path is touched. Without a configured calendar the route reports
/// unavailable, matching the market routes.
pub async fn calendar_events(
    state: Data<AppState>,
    query: web::Query<CalendarQuery>,
) -> HttpResponse {
    let Some(runtime) = state.calendar() else {
        return HttpResponse::ServiceUnavailable().json(json!({ "error": "calendar_unavailable" }));
    };
    let hours = query.hours.unwrap_or(24);
    if !(1..=168).contains(&hours) {
        return HttpResponse::BadRequest()
            .json(json!({ "error": "invalid_window", "reason": "hours must be 1-168" }));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let until = now.saturating_add(i64::from(hours) * 3_600);
    let feed = runtime.feed();
    match feed.events(now, until).await {
        Ok(events) => HttpResponse::Ok().json(json!({
            "provider": feed.provider().as_str(),
            "from": now,
            "until": until,
            "events": events
                .iter()
                .map(|event| json!({
                    "title": event.title(),
                    "currency": event.currency(),
                    "impact": event.impact().as_str(),
                    "time": event.time()
                }))
                .collect::<Vec<_>>()
        })),
        Err(error) => HttpResponse::BadGateway()
            .json(json!({ "error": "calendar_failed", "reason": error.to_string() })),
    }
}

/// Query for the instrument contract.
#[derive(Debug, Deserialize)]
pub struct SpecQuery {
    /// Instrument; defaults to the terminal's chart symbol.
    pub symbol: Option<String>,
}

#[get("/market/spec")]
/// Returns the venue's contract details for one instrument.
///
/// Read-only: at most one `symbol_spec` command is queued on the control
/// channel and no order path is touched. Invalid symbols are rejected before
/// anything is queued, and a terminal that does not answer within the
/// configured window is reported as a gateway failure.
pub async fn market_spec(state: Data<AppState>, query: web::Query<SpecQuery>) -> HttpResponse {
    let Some(runtime) = state.market() else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "market_feed_unavailable" }));
    };
    let symbol = match query.symbol.as_deref() {
        Some(raw) => match Symbol::parse(raw) {
            Ok(symbol) => symbol,
            Err(_) => {
                return HttpResponse::BadRequest().json(json!({ "error": "invalid_symbol" }));
            }
        },
        None => match default_symbol(&state).await {
            Some(symbol) => symbol,
            None => {
                return HttpResponse::Conflict().json(json!({ "error": "symbol_unavailable" }));
            }
        },
    };
    match runtime.feed().symbol_spec(&symbol).await {
        Ok(spec) => HttpResponse::Ok().json(spec),
        Err(error) => HttpResponse::BadGateway()
            .json(json!({ "error": "market_feed_failed", "reason": error.to_string() })),
    }
}

/// Falls back to the symbol of the terminal's hosting chart.
async fn default_symbol(state: &AppState) -> Option<Symbol> {
    let broker = state.broker()?;
    let report = broker.link().report().await;
    report.snapshot.map(|snapshot| snapshot.symbol().clone())
}

#[post("/intents/execute")]
/// Queues a live order when execution is explicitly enabled.
///
/// Refuses with `403 trading_disabled` unless `VEYRA_TRADING_ENABLED=true`, so
/// an approved intent alone cannot trade. With the service switch on, the
/// terminal still requires its own live-orders input before anything reaches
/// the broker; otherwise it validates the request and reports a dry run.
pub async fn execute_intent(
    state: Data<AppState>,
    draft: web::Json<TradeIntentDraft>,
) -> HttpResponse {
    if command_link(&state).is_none() {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    }
    if !state.trading_enabled() {
        return HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }));
    }
    let draft = draft.into_inner();
    let account = crate::routes::account_facts_for_draft(state.as_ref(), &draft).await;
    match state.risk().evaluate(&draft, account, state.now()) {
        RiskDecision::Rejected(rejection) => {
            HttpResponse::Ok().json(RiskDecision::Rejected(rejection))
        }
        RiskDecision::Approved(intent) => match queue_staged_order(&state, &intent).await {
            StagedExecution::Queued { command, intent_id } => HttpResponse::Ok().json(json!({
                "decision": "approved",
                "intent_id": intent_id,
                "command": "open_order",
                "command_id": command.to_string(),
                "status": "pending"
            })),
            // Both refusals were checked above; a change mid-request is a
            // conflict, not a silent no-op.
            StagedExecution::TradingDisabled => {
                HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }))
            }
            StagedExecution::ChannelUnavailable => HttpResponse::ServiceUnavailable()
                .json(json!({ "error": "command_channel_unavailable" })),
            StagedExecution::Rejected { rejection } => {
                HttpResponse::Ok().json(RiskDecision::Rejected(rejection))
            }
        },
    }
}

/// Outcome of handing one gate-approved intent to the command channel.
#[derive(Debug, Clone, PartialEq)]
pub enum StagedExecution {
    /// The order command was queued and audited; poll it by id.
    Queued {
        /// Identifier of the queued command.
        command: CommandId,
        /// Identifier of the approved intent the command carries.
        intent_id: String,
    },
    /// The operator switch is off; nothing was queued.
    TradingDisabled,
    /// The active broker exposes no command channel.
    ChannelUnavailable,
    /// Dynamic account facts changed after the original approval, so the
    /// final broker-boundary gate refused the order.
    Rejected {
        /// Stable deterministic reason.
        rejection: RiskRejection,
    },
}

/// Queues one approved intent as a live order command.
///
/// This is the single execution path shared by the control surface and the
/// autonomous loop: it re-checks both operator controls, stamps the Veyra
/// magic through [`OrderRequest::from_intent`], and audits the queueing.
/// Approval alone can never trade.
pub async fn queue_staged_order(state: &AppState, intent: &TradeIntent) -> StagedExecution {
    let Some(link) = command_link(state) else {
        return StagedExecution::ChannelUnavailable;
    };
    if !state.trading_enabled() {
        return StagedExecution::TradingDisabled;
    }

    // Approval can precede execution by an arbitrarily slow model/tool turn.
    // Serialize this last check with enqueue, reject while a prior open has not
    // reached a newer account snapshot, and rebuild facts from the latest book.
    // The preview applies every deterministic rule without consuming the
    // duplicate-intent memory a second time.
    let _admission = state.order_admission().lock().await;
    let account = if link.has_unreconciled_open_order() {
        None
    } else {
        crate::routes::account_facts_for_draft(state, intent.draft()).await
    };
    if let RiskDecision::Rejected(rejection) =
        state.risk().preview(intent.draft(), account, state.now())
    {
        return StagedExecution::Rejected { rejection };
    }

    let command = link.enqueue_open_order(OrderRequest::from_intent(intent));
    audit(
        state,
        AuditKind::CommandQueued,
        json!({
            "command_id": command.to_string(),
            "kind": "open_order",
            "intent_id": intent.id().to_string()
        }),
    )
    .await;
    StagedExecution::Queued {
        command,
        intent_id: intent.id().to_string(),
    }
}

/// Body of `POST /intents/close`.
#[derive(Debug, Deserialize)]
pub struct CloseRequest {
    /// Ticket of the Veyra-owned position to close.
    pub ticket: i64,
}

#[post("/intents/close")]
/// Closes one Veyra-owned position by ticket.
///
/// Refuses with `403 trading_disabled` unless `VEYRA_TRADING_ENABLED=true`,
/// and only accepts tickets that appear in the latest completed
/// `account_snapshot` with the Veyra magic number — a manually placed position
/// can never be closed through this route. The terminal re-validates the
/// ticket and reports a dry run while its live-orders input is disabled.
pub async fn close_position(state: Data<AppState>, body: web::Json<CloseRequest>) -> HttpResponse {
    if command_link(&state).is_none() {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    }
    if !state.trading_enabled() {
        return HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }));
    }
    if body.ticket <= 0 {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid_ticket" }));
    }
    match queue_staged_close(&state, body.ticket).await {
        StagedClose::Queued { command, ticket } => HttpResponse::Ok().json(json!({
            "command": "close_order",
            "command_id": command.to_string(),
            "ticket": ticket,
            "status": "pending"
        })),
        StagedClose::TradingDisabled => {
            HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }))
        }
        StagedClose::ChannelUnavailable => HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" })),
        StagedClose::NoPositions => {
            HttpResponse::Conflict().json(json!({ "error": "position_state_unavailable" }))
        }
        StagedClose::UnknownTicket => {
            HttpResponse::NotFound().json(json!({ "error": "unknown_position" }))
        }
        StagedClose::NotVeyra => {
            HttpResponse::Conflict().json(json!({ "error": "not_a_veyra_position" }))
        }
    }
}

/// Outcome of handing one ticket to the close path.
#[derive(Debug, Clone, PartialEq)]
pub enum StagedClose {
    /// The close command was queued and audited; poll it by id.
    Queued {
        /// Identifier of the queued command.
        command: CommandId,
        /// Ticket being closed.
        ticket: i64,
    },
    /// The operator switch is off; nothing was queued.
    TradingDisabled,
    /// The active broker exposes no command channel.
    ChannelUnavailable,
    /// No completed account snapshot is retained yet.
    NoPositions,
    /// The ticket is not in the latest completed snapshot.
    UnknownTicket,
    /// The ticket exists but is not Veyra-owned.
    NotVeyra,
}

/// Queues one close for a ticket from the latest completed snapshot.
///
/// The single close path shared by the control surface and the autonomous
/// loop: only tickets carrying the Veyra magic number in the retained
/// snapshot are accepted, and the terminal re-validates before acting, so a
/// manually placed position can never be closed through either caller.
pub async fn queue_staged_close(state: &AppState, ticket: i64) -> StagedClose {
    let Some(link) = command_link(state) else {
        return StagedClose::ChannelUnavailable;
    };
    if !state.trading_enabled() {
        return StagedClose::TradingDisabled;
    }
    let Some(snapshot) = link.last_account() else {
        return StagedClose::NoPositions;
    };
    let Some(position) = snapshot
        .positions
        .iter()
        .find(|position| position.ticket == ticket)
    else {
        return StagedClose::UnknownTicket;
    };
    if position.magic != ORDER_MAGIC {
        return StagedClose::NotVeyra;
    }
    let command = link.enqueue_close_order(CloseOrderRequest::new(position.ticket, position.magic));
    audit(
        state,
        AuditKind::CommandQueued,
        json!({
            "command_id": command.to_string(),
            "kind": "close_order",
            "ticket": position.ticket
        }),
    )
    .await;
    StagedClose::Queued {
        command,
        ticket: position.ticket,
    }
}

/// Body of `POST /intents/modify`.
#[derive(Debug, Deserialize)]
pub struct ModifyRequest {
    /// Ticket of the Veyra-owned position whose stops change.
    pub ticket: i64,
    /// New stop loss, when provided.
    #[serde(default)]
    pub stop_loss: Option<f64>,
    /// New take profit, when provided.
    #[serde(default)]
    pub take_profit: Option<f64>,
}

#[post("/intents/modify")]
/// Changes the stops on one Veyra-owned position.
///
/// Same guards as closing: `403 trading_disabled` unless enabled, and only
/// tickets from the latest completed `account_snapshot` carrying the Veyra
/// magic number are accepted. At least one finite, positive stop is required;
/// the terminal re-validates distances and reports a dry run while its
/// live-orders input is disabled.
pub async fn modify_position(
    state: Data<AppState>,
    body: web::Json<ModifyRequest>,
) -> HttpResponse {
    if command_link(&state).is_none() {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    }
    if !state.trading_enabled() {
        return HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }));
    }
    if body.ticket <= 0 {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid_ticket" }));
    }
    let valid_stop = |stop: f64| stop.is_finite() && stop > 0.0;
    let provided = body.stop_loss.is_some() || body.take_profit.is_some();
    let stops_valid =
        body.stop_loss.is_none_or(valid_stop) && body.take_profit.is_none_or(valid_stop);
    if !provided || !stops_valid {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid_stops" }));
    }
    match queue_staged_modify(&state, body.ticket, body.stop_loss, body.take_profit).await {
        StagedModify::Queued { command, ticket } => HttpResponse::Ok().json(json!({
            "command": "modify_order",
            "command_id": command.to_string(),
            "ticket": ticket,
            "status": "pending"
        })),
        StagedModify::TradingDisabled => {
            HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }))
        }
        StagedModify::ChannelUnavailable => HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" })),
        StagedModify::NoPositions => {
            HttpResponse::Conflict().json(json!({ "error": "position_state_unavailable" }))
        }
        StagedModify::UnknownTicket => {
            HttpResponse::NotFound().json(json!({ "error": "unknown_position" }))
        }
        StagedModify::NotVeyra => {
            HttpResponse::Conflict().json(json!({ "error": "not_a_veyra_position" }))
        }
    }
}

/// Outcome of handing a stop change to the modify path.
#[derive(Debug, Clone, PartialEq)]
pub enum StagedModify {
    /// The modify command was queued and audited; poll it by id.
    Queued {
        /// Identifier of the queued command.
        command: CommandId,
        /// Ticket whose stops change.
        ticket: i64,
    },
    /// The operator switch is off; nothing was queued.
    TradingDisabled,
    /// The active broker exposes no command channel.
    ChannelUnavailable,
    /// No completed account snapshot is retained yet.
    NoPositions,
    /// The ticket is not in the latest completed snapshot.
    UnknownTicket,
    /// The ticket exists but is not Veyra-owned.
    NotVeyra,
}

/// Queues one stop change for a ticket from the latest completed snapshot.
///
/// The single modify path shared by the control surface and the autonomous
/// loop: same ownership guards as closing, and the terminal re-validates
/// every stop distance before acting. Callers validate that at least one
/// finite positive stop is provided.
pub async fn queue_staged_modify(
    state: &AppState,
    ticket: i64,
    stop_loss: Option<f64>,
    take_profit: Option<f64>,
) -> StagedModify {
    let Some(link) = command_link(state) else {
        return StagedModify::ChannelUnavailable;
    };
    if !state.trading_enabled() {
        return StagedModify::TradingDisabled;
    }
    let Some(snapshot) = link.last_account() else {
        return StagedModify::NoPositions;
    };
    let Some(position) = snapshot
        .positions
        .iter()
        .find(|position| position.ticket == ticket)
    else {
        return StagedModify::UnknownTicket;
    };
    if position.magic != ORDER_MAGIC {
        return StagedModify::NotVeyra;
    }
    let command = link.enqueue_modify_order(ModifyOrderRequest::new(
        position.ticket,
        position.magic,
        stop_loss,
        take_profit,
    ));
    audit(
        state,
        AuditKind::CommandQueued,
        json!({
            "command_id": command.to_string(),
            "kind": "modify_order",
            "ticket": position.ticket
        }),
    )
    .await;
    StagedModify::Queued {
        command,
        ticket: position.ticket,
    }
}

#[get("/reconciliation")]
/// Reports how the terminal's open orders relate to Veyra's ownership.
///
/// `status` is `unavailable` (no command channel), `stale` (the terminal is
/// not polling), `no_snapshot` (nothing retained yet), `reconciled` (every
/// order is Veyra-managed), or `drift` (unknown orders or a truncated list).
pub async fn reconciliation(state: Data<AppState>) -> HttpResponse {
    let Some(runtime) = state.broker() else {
        return HttpResponse::Ok().json(json!({ "status": "unavailable" }));
    };
    let link = runtime.link();
    if !runtime.link().report().await.fresh {
        return HttpResponse::Ok().json(json!({ "status": "stale" }));
    }
    let Some(snapshot) = link.last_account() else {
        return HttpResponse::Ok().json(json!({ "status": "no_snapshot" }));
    };
    let report = crate::reconciliation::assess(&snapshot);
    let account_age_secs = link
        .last_account_age(SystemTime::now())
        .map(|age| age.as_secs())
        .unwrap_or(0);
    let positions = report
        .positions
        .iter()
        .map(|position| {
            json!({
                "ticket": position.ticket,
                "symbol": position.symbol,
                "magic": position.magic,
                "managed": position.managed,
                "lots": position.lots
            })
        })
        .collect::<Vec<_>>();
    HttpResponse::Ok().json(json!({
        "status": if report.is_reconciled() { "reconciled" } else { "drift" },
        "orders": snapshot.orders,
        "lots": report.lots,
        "positions": positions,
        "unknownTickets": report.unknown_tickets,
        "positionsTruncated": report.positions_truncated,
        "accountAgeSecs": account_age_secs
    }))
}

/// Query for `GET /audit`.
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    /// Maximum rows to return (1-200, default 50).
    pub limit: Option<u32>,
}

#[get("/audit")]
/// Returns the newest audit events, newest first.
pub async fn audit_log(state: Data<AppState>, query: web::Query<AuditQuery>) -> HttpResponse {
    let Some(runtime) = state.audit() else {
        return HttpResponse::Ok().json(json!({ "status": "disabled", "events": [] }));
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    match runtime.trail().recent(limit).await {
        Ok(rows) => HttpResponse::Ok().json(json!({
            "status": "ok",
            "provider": runtime.provider().as_str(),
            "events": rows
                .iter()
                .map(|row| json!({
                    "id": row.id,
                    "at": row.at,
                    "kind": row.kind,
                    "payload": row.payload
                }))
                .collect::<Vec<_>>()
        })),
        Err(error) => HttpResponse::ServiceUnavailable()
            .json(json!({ "status": "unavailable", "error": error.to_string() })),
    }
}

#[get("/commands/{id}")]
/// Reports one command's lifecycle state and validated result.
pub async fn command_status(state: Data<AppState>, id: web::Path<String>) -> HttpResponse {
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    };
    let Some(command_id) = CommandId::parse(id.as_str()) else {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid_command_id" }));
    };
    let Some(record) = link.command(command_id) else {
        return HttpResponse::NotFound().json(json!({ "error": "unknown_command" }));
    };
    let kind = record.kind.as_str();
    let body = match record.state {
        CommandState::Pending => json!({
            "id": record.id.to_string(),
            "kind": kind,
            "status": "pending"
        }),
        CommandState::Completed { payload } => json!({
            "id": record.id.to_string(),
            "kind": kind,
            "status": "completed",
            "result": command_result(payload)
        }),
        CommandState::Failed { reason } => json!({
            "id": record.id.to_string(),
            "kind": kind,
            "status": "failed",
            "error": reason
        }),
    };
    HttpResponse::Ok().json(body)
}

/// Returns the EA command channel of the active provider, when it exposes one.
fn command_link(state: &AppState) -> Option<Arc<dyn BrokerLink>> {
    Some(state.broker()?.link())
}

/// Records an audit event best-effort, when a trail is configured.
async fn audit(state: &AppState, kind: AuditKind, payload: serde_json::Value) {
    if let Some(runtime) = state.audit() {
        runtime.try_record(AuditEvent::new(kind, payload)).await;
    }
}

/// Flattens a validated payload for operators; account balances stay out of
/// the control surface.
fn command_result(payload: CommandPayload) -> serde_json::Value {
    match payload {
        CommandPayload::Ping => json!({}),
        CommandPayload::AccountSnapshot(snapshot) => json!({
            "orders": snapshot.orders,
            "lots": snapshot.lots,
            "positions": snapshot.positions,
            "positionsTruncated": snapshot.positions_truncated
        }),
        CommandPayload::OrderCheck(check) => json!({
            "passed": check.passed,
            "retcode": check.retcode,
            "comment": check.comment,
            "margin": check.margin
        }),
        CommandPayload::Rates(rates) => json!({
            "symbol": rates.symbol,
            "timeframeMinutes": rates.timeframe_minutes,
            "candles": rates.candles
        }),
        CommandPayload::SymbolSpec(spec) => json!({
            "symbol": spec.symbol,
            "digits": spec.digits,
            "point": spec.point,
            "spreadPoints": spec.spread_points,
            "stopLevelPoints": spec.stop_level_points,
            "freezeLevelPoints": spec.freeze_level_points,
            "lotMin": spec.lot_min,
            "lotMax": spec.lot_max,
            "lotStep": spec.lot_step,
            "tickValue": spec.tick_value,
            "tickSize": spec.tick_size,
            "marginRequired": spec.margin_required,
            "swapLong": spec.swap_long,
            "swapShort": spec.swap_short,
            "swapType": spec.swap_type,
            "tradeAllowed": spec.trade_allowed
        }),
        CommandPayload::OrderHistory(history) => json!({
            "orders": history.orders,
            "total": history.total,
            "truncated": history.truncated
        }),
        CommandPayload::OpenOrder(execution)
        | CommandPayload::CloseOrder(execution)
        | CommandPayload::ModifyOrder(execution) => json!({
            "executed": execution.executed,
            "retcode": execution.retcode,
            "comment": execution.comment,
            "ticket": execution.ticket,
            "price": execution.price
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use actix_web::test;
    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::app::create_app;
    use crate::audit::{
        AuditError, AuditProvider, AuditRow, AuditRuntime, AuditTrail, MemoryTrail,
    };
    use crate::broker::{
        AccountLogin, AccountSnapshot, BrokerRuntime, BrokerSettings, ServerName, SymbolSpecRequest,
    };
    use crate::calendar::{
        CalendarError, CalendarEvent, CalendarProvider, CalendarRuntime, EventCalendar, Impact,
    };
    use crate::config::{ConfigError, ServiceConfig};
    use crate::market::{Candle, CandleSeries, MarketError, MarketFeed, MarketProvider};
    use crate::risk::{RiskGate, RiskPolicy};

    #[derive(Debug)]
    struct StubFeed {
        fail: bool,
    }

    #[async_trait]
    impl MarketFeed for StubFeed {
        fn provider(&self) -> MarketProvider {
            MarketProvider::Ea
        }

        async fn candles(&self, request: CandleRequest) -> Result<CandleSeries, MarketError> {
            if self.fail {
                return Err(MarketError::Unavailable {
                    reason: "terminal did not answer".to_owned(),
                });
            }
            Ok(CandleSeries::from_validated(
                request.symbol().clone(),
                request.timeframe(),
                vec![Candle::from_validated(
                    1_700_000_000,
                    1.1,
                    1.2,
                    1.0,
                    1.15,
                    42,
                )],
            ))
        }

        async fn symbol_spec(
            &self,
            symbol: &Symbol,
        ) -> Result<crate::broker::SymbolSpecPayload, MarketError> {
            if self.fail {
                return Err(MarketError::Unavailable {
                    reason: "terminal did not answer".to_owned(),
                });
            }
            Ok(crate::broker::SymbolSpecPayload {
                symbol: symbol.as_str().to_owned(),
                digits: 5,
                point: 0.00001,
                bid: 1.1,
                ask: 1.10012,
                spread_points: 12,
                stop_level_points: 5,
                freeze_level_points: 0,
                lot_min: 0.01,
                lot_max: 100.0,
                lot_step: 0.01,
                tick_value: 0.1,
                tick_size: 0.00001,
                margin_required: 3.29,
                swap_long: -0.72,
                swap_short: -0.31,
                swap_type: 0,
                trade_allowed: true,
            })
        }
    }

    /// Calendar stub answering from a fixed event list or failing outright.
    #[derive(Debug)]
    struct StubCalendar {
        events: Vec<CalendarEvent>,
        fail: bool,
    }

    #[async_trait]
    impl EventCalendar for StubCalendar {
        fn provider(&self) -> CalendarProvider {
            CalendarProvider::Forexfactory
        }

        async fn events(&self, from: i64, until: i64) -> Result<Vec<CalendarEvent>, CalendarError> {
            if self.fail {
                return Err(CalendarError::Transport {
                    reason: "feed down".to_owned(),
                });
            }
            Ok(self
                .events
                .iter()
                .filter(|event| event.time() >= from && event.time() < until)
                .cloned()
                .collect())
        }
    }

    fn config() -> ServiceConfig {
        ServiceConfig::from_source(|name| match name {
            "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
            "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
            "VEYRA_ENV" => Ok("development".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("config must parse")
    }

    fn broker_with_chart() -> BrokerRuntime {
        let settings = BrokerSettings::from_source(|name| match name {
            "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
            "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
            _ => Err(ConfigError::MissingEnvironmentVariable { name }),
        })
        .expect("settings must parse")
        .expect("configured");
        let runtime = BrokerRuntime::from_settings(settings).expect("runtime builds");
        runtime
            .ea_link()
            .expect("ea link")
            .record(AccountSnapshot::new(
                AccountLogin::parse(94168).expect("login"),
                ServerName::parse("IFCMarkets-Real").expect("server"),
                Symbol::parse("EURUSD").expect("symbol"),
                true,
                true,
                0,
                0.0,
            ));
        runtime
    }

    fn build_state(feed: Option<StubFeed>, broker: bool) -> AppState {
        let mut state = AppState::new(
            config(),
            if broker {
                Some(broker_with_chart())
            } else {
                None
            },
            None,
            RiskGate::new(RiskPolicy::default()),
        );
        if let Some(feed) = feed {
            state = state.with_market(Some(crate::market::MarketRuntime::from_feed(Arc::new(
                feed,
            ))));
        }
        state
    }

    #[actix_web::test]
    async fn candles_require_a_configured_feed() {
        let app = test::init_service(create_app(build_state(None, true))).await;
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/market/candles").to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
    }

    #[actix_web::test]
    async fn candles_validate_the_query_before_touching_the_feed() {
        let app = test::init_service(create_app(build_state(
            Some(StubFeed { fail: false }),
            true,
        )))
        .await;

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/candles?symbol=bad%20symbol")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "invalid_symbol");

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/candles?timeframe=H6")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "invalid_timeframe");

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/candles?symbol=EURUSD&bars=500")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "invalid_window");
    }

    #[actix_web::test]
    async fn candles_use_the_chart_symbol_when_omitted() {
        let app = test::init_service(create_app(build_state(
            Some(StubFeed { fail: false }),
            true,
        )))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/candles?timeframe=H4&bars=2")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["symbol"], "EURUSD");
        assert_eq!(body["timeframe"], "H4");
        assert_eq!(body["candles"][0]["close"], 1.15);
        assert_eq!(body["candles"][0]["volume"], 42);
    }

    #[actix_web::test]
    async fn candles_report_a_missing_default_symbol() {
        let app = test::init_service(create_app(build_state(
            Some(StubFeed { fail: false }),
            false,
        )))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/market/candles").to_request(),
        )
        .await;
        assert_eq!(response.status(), 409);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "symbol_unavailable");
    }

    fn audited_state(feed: Option<StubFeed>) -> (AppState, Arc<MemoryTrail>) {
        let trail = Arc::new(MemoryTrail::default());
        let state = build_state(feed, true);
        let state = state.with_audit(Some(crate::audit::AuditRuntime::new(trail.clone())));
        (state, trail)
    }

    #[actix_web::test]
    async fn event_feed_streams_recorded_events_with_cursors() {
        let (state, trail) = audited_state(None);
        let runtime = state.audit().expect("audit").clone();
        runtime
            .try_record(crate::audit::AuditEvent::new(
                crate::audit::AuditKind::CommandQueued,
                serde_json::json!({"kind": "account_snapshot"}),
            ))
            .await;
        runtime
            .try_record(crate::audit::AuditEvent::new(
                crate::audit::AuditKind::ProposalEvaluated,
                serde_json::json!({"outcome": "no_trade"}),
            ))
            .await;
        assert_eq!(trail.events().len(), 2);

        let app = test::init_service(create_app(state.clone())).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/events").to_request()).await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        let events = body["events"].as_array().expect("events");
        assert_eq!(events.len(), 2, "no cursor returns the buffered tail");
        assert_eq!(events[0]["kind"], "command_queued");
        assert_eq!(events[1]["kind"], "proposal_evaluated");
        assert_eq!(events[1]["payload"]["outcome"], "no_trade");
        assert!(events[0]["seq"].as_u64().expect("seq") < events[1]["seq"].as_u64().expect("seq"));
        let cursor = body["next"].as_u64().expect("next");

        // A cursor at the tip answers immediately (wait_ms=0) with nothing.
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/events?after={cursor}&wait_ms=0"))
                .to_request(),
        )
        .await;
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["events"].as_array().expect("events").len(), 0);
        assert_eq!(body["next"].as_u64().expect("next"), cursor);

        // A stale cursor still receives from the buffer.
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/events?after=0&wait_ms=0&limit=1")
                .to_request(),
        )
        .await;
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["events"].as_array().expect("events").len(), 1);
        assert_eq!(body["events"][0]["kind"], "command_queued");
    }

    #[actix_web::test]
    async fn event_feed_requires_an_audit_trail() {
        let app = test::init_service(create_app(build_state(None, true))).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/events").to_request()).await;
        assert_eq!(response.status(), 503);
    }

    #[actix_web::test]
    async fn command_list_reports_pending_commands() {
        let (state, _) = audited_state(None);
        let link = state.broker().expect("broker").ea_link().expect("ea link");
        let id = link.enqueue_account_snapshot();

        let app = test::init_service(create_app(state.clone())).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/commands").to_request()).await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        let commands = body["commands"].as_array().expect("commands");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["id"], id.to_string());
        assert_eq!(commands[0]["kind"], "account_snapshot");
        assert_eq!(commands[0]["status"], "pending");

        let unlinked = test::init_service(create_app(build_state(None, false))).await;
        let response = test::call_service(
            &unlinked,
            test::TestRequest::get().uri("/commands").to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
    }

    #[actix_web::test]
    async fn account_state_reports_controls_and_money_after_a_snapshot() {
        let (state, _) = audited_state(None);
        let link = state.broker().expect("broker").ea_link().expect("ea link");

        let app = test::init_service(create_app(state.clone())).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/account").to_request()).await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["fresh"], true);
        assert_eq!(body["connected"], true);
        assert_eq!(body["tradeAllowed"], true);
        assert_eq!(
            body["liveOrders"], false,
            "the recorded snapshot is disarmed"
        );
        assert!(body["balance"].is_null(), "no snapshot payload yet");

        // Deliver one account_snapshot ack through the real poll path.
        link.enqueue_account_snapshot();
        let ea_app =
            actix_web::test::init_service(crate::broker::ea::create_ea_app(link.clone())).await;
        let hello = serde_json::json!({
            "t": "hb",
            "token": "test-token-1234567890",
            "acct": 94168,
            "server": "IFCMarkets-Real",
            "symbol": "EURUSD",
            "connected": true,
            "tradeAllowed": true,
            "orders": 0,
            "lots": 0.0
        });
        let mut command = None;
        for _ in 0..40 {
            let response = test::call_service(
                &ea_app,
                test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            let body: Value = test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let command = command.expect("snapshot command delivered");
        let ack = serde_json::json!({
            "t": "ack",
            "token": "test-token-1234567890",
            "id": command["id"],
            "ok": true,
            "data": {
                "balance": 20.57,
                "equity": 21.10,
                "freeMargin": 20.10,
                "orders": 1,
                "lots": 0.01,
                "positions": [{
                    "ticket": 123456,
                    "symbol": "EURUSD",
                    "kind": "buy",
                    "lots": 0.01,
                    "price": 1.09500,
                    "profit": 0.53,
                    "magic": 77041
                }],
                "positionsTruncated": false,
                "serverTime": 1_758_000_000,
                "leverage": 100,
                "marginLevel": 12.5
            }
        });
        let response = test::call_service(
            &ea_app,
            test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(ack.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());

        let response =
            test::call_service(&app, test::TestRequest::get().uri("/account").to_request()).await;
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["balance"], 20.57);
        assert_eq!(body["equity"], 21.10);
        assert_eq!(body["freeMargin"], 20.10);
        assert_eq!(body["marginLevel"], 12.5);
        assert_eq!(body["leverage"], 100);
        assert_eq!(body["orders"], 1);
        assert_eq!(body["lots"], 0.01);
        assert_eq!(body["positions"][0]["ticket"], 123456);
        assert_eq!(body["positions"][0]["magic"], 77041);
        assert_eq!(body["login"], 94168);
        assert_eq!(body["server"], "IFCMarkets-Real");
    }

    #[actix_web::test]
    async fn market_spec_returns_contracts_and_reports_failures() {
        let app = test::init_service(create_app(build_state(
            Some(StubFeed { fail: false }),
            true,
        )))
        .await;

        // No symbol defaults to the terminal's chart symbol.
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/market/spec").to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["symbol"], "EURUSD");
        assert_eq!(body["spreadPoints"], 12);
        assert_eq!(body["stopLevelPoints"], 5);
        assert_eq!(body["lotMin"], 0.01);
        assert_eq!(body["marginRequired"], 3.29);
        assert_eq!(body["tradeAllowed"], true);

        // An explicit symbol is honoured.
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/spec?symbol=GBPUSD")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["symbol"], "GBPUSD");

        // Invalid symbols never reach the feed.
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/spec?symbol=bad%20symbol")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "invalid_symbol");

        // A terminal that does not answer is a gateway failure.
        let failing =
            test::init_service(create_app(build_state(Some(StubFeed { fail: true }), true))).await;
        let response = test::call_service(
            &failing,
            test::TestRequest::get()
                .uri("/market/spec?symbol=EURUSD")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 502);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "market_feed_failed");

        // Without a feed the route is unavailable.
        let unlinked = test::init_service(create_app(build_state(None, true))).await;
        let response = test::call_service(
            &unlinked,
            test::TestRequest::get().uri("/market/spec").to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
    }

    #[actix_web::test]
    async fn performance_route_reports_realized_history() {
        let (state, _) = audited_state(Some(StubFeed { fail: false }));
        let link = state.broker().expect("broker").ea_link().expect("ea link");

        // Unavailable without a broker; invalid windows never reach the channel.
        let unlinked = test::init_service(create_app(build_state(None, false))).await;
        let response = test::call_service(
            &unlinked,
            test::TestRequest::get().uri("/performance").to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
        let typed = test::init_service(create_app(state.clone())).await;
        let response = test::call_service(
            &typed,
            test::TestRequest::get()
                .uri("/performance?days=0")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);

        // One history round trip: the route queues, the EA answers.
        let ea_app = test::init_service(crate::broker::ea::create_ea_app(link.clone())).await;
        let route_state = state.clone();
        let task = actix_web::rt::spawn(async move {
            let app = test::init_service(create_app(route_state)).await;
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri("/performance?days=7")
                    .to_request(),
            )
            .await
        });
        let hello = serde_json::json!({
            "t": "hb",
            "token": "test-token-1234567890",
            "acct": 94168,
            "server": "IFCMarkets-Real",
            "symbol": "EURUSD",
            "connected": true,
            "tradeAllowed": true,
            "orders": 0,
            "lots": 0.0
        });
        let mut command = None;
        for _ in 0..40 {
            let response = test::call_service(
                &ea_app,
                test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            let body: Value = test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            actix_web::rt::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let command = command.expect("history command delivered");
        assert_eq!(command["kind"], "order_history");
        assert_eq!(command["history"]["days"], 7);
        assert_eq!(command["history"]["magic"], 77041);

        let ack = serde_json::json!({
            "t": "ack",
            "token": "test-token-1234567890",
            "id": command["id"],
            "ok": true,
            "data": {
                "orders": [{
                    "ticket": 10650830,
                    "symbol": "USDJPY",
                    "kind": "buy",
                    "lots": 0.01,
                    "openPrice": 156.198,
                    "closePrice": 156.41,
                    "openTime": 1_789_699_082_i64,
                    "closeTime": 1_789_707_257_i64,
                    "profit": 1.36,
                    "swap": 0.0,
                    "commission": 0.0,
                    "magic": 77041
                }],
                "total": 1,
                "truncated": false
            }
        });
        let response = test::call_service(
            &ea_app,
            test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(ack.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());

        let response = task.await.expect("task joins");
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["days"], 7);
        assert_eq!(body["report"]["trades"], 1);
        assert_eq!(body["report"]["wins"], 1);
        assert_eq!(body["report"]["win_rate_percent"], 100.0);
        assert_eq!(body["report"]["net_profit"], 1.36);
        assert_eq!(body["report"]["profit_factor"], Value::Null);
        assert_eq!(body["trades"][0]["symbol"], "USDJPY");
        assert_eq!(body["trades"][0]["closePrice"], 156.41);

        // The same command flattens through /commands/{id}.
        let id = command["id"].as_str().expect("command id");
        let app = test::init_service(create_app(state.clone())).await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/commands/{id}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["kind"], "order_history");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["result"]["truncated"], false);

        // A terminal that fails the request is a gateway failure.
        let route_state = state.clone();
        let failing = actix_web::rt::spawn(async move {
            let app = test::init_service(create_app(route_state)).await;
            test::call_service(
                &app,
                test::TestRequest::get().uri("/performance").to_request(),
            )
            .await
        });
        let mut command = None;
        for _ in 0..40 {
            let response = test::call_service(
                &ea_app,
                test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            let body: Value = test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            actix_web::rt::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let command = command.expect("history command delivered again");
        let failure = serde_json::json!({
            "t": "ack",
            "token": "test-token-1234567890",
            "id": command["id"],
            "ok": false,
            "error": "history unavailable"
        });
        let response = test::call_service(
            &ea_app,
            test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(failure.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());
        let response = failing.await.expect("task joins");
        assert_eq!(response.status(), 502);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "history_failed");
        assert_eq!(body["reason"], "history unavailable");
    }

    #[actix_web::test]
    async fn trades_route_requires_a_broker_and_a_valid_window() {
        let unlinked = test::init_service(create_app(build_state(None, false))).await;
        let response = test::call_service(
            &unlinked,
            test::TestRequest::get().uri("/trades").to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "broker_unavailable");

        let (state, _) = audited_state(None);
        let typed = test::init_service(create_app(state)).await;
        let response = test::call_service(
            &typed,
            test::TestRequest::get().uri("/trades?days=0").to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "invalid_window");

        for uri in [
            "/trades?page=0",
            "/trades?pageSize=0",
            "/trades?pageSize=101",
        ] {
            let response =
                test::call_service(&typed, test::TestRequest::get().uri(uri).to_request()).await;
            assert_eq!(response.status(), 400, "{uri}");
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["error"], "invalid_page", "{uri}");
        }
    }

    #[actix_web::test]
    async fn trades_route_classifies_closes_and_reports_a_gateway_failure() {
        let (state, trail) = audited_state(None);
        let link = state.broker().expect("broker").ea_link().expect("ea link");

        // Ticket 10655087 has a recorded entry and an autopilot close: the
        // agent_close reason must win even though its close price also sits
        // on the recorded stop.
        let open_id = "5a3f5c1e-2b1d-4a57-9d27-9b0d2f7e8a10".to_owned();
        trail.record_at(
            time::OffsetDateTime::now_utc() - time::Duration::minutes(20),
            crate::audit::AuditEvent::new(
                crate::audit::AuditKind::ProposalEvaluated,
                serde_json::json!({
                    "outcome": "queued",
                    "command_id": open_id,
                    "rationale": "GBPUSD has the strongest aligned bearish evidence",
                    "stop_loss": 1.3265,
                    "take_profit": 1.3162
                }),
            ),
        );
        trail.record_at(
            time::OffsetDateTime::now_utc() - time::Duration::minutes(19),
            crate::audit::AuditEvent::new(
                crate::audit::AuditKind::CommandCompleted,
                serde_json::json!({
                    "kind": "open_order",
                    "command_id": open_id,
                    "result": {"ticket": 10_655_087}
                }),
            ),
        );
        trail.record_at(
            time::OffsetDateTime::now_utc() - time::Duration::minutes(1),
            crate::audit::AuditEvent::new(
                crate::audit::AuditKind::ProposalEvaluated,
                serde_json::json!({
                    "outcome": "close_queued",
                    "origin": "autopilot_review",
                    "ticket": 10_655_087,
                    "rationale": "thesis invalidated"
                }),
            ),
        );

        // One history round trip: the route queues, the EA answers.
        let ea_app = test::init_service(crate::broker::ea::create_ea_app(link.clone())).await;
        let route_state = state.clone();
        let task = actix_web::rt::spawn(async move {
            let app = test::init_service(create_app(route_state)).await;
            test::call_service(
                &app,
                test::TestRequest::get().uri("/trades?days=7").to_request(),
            )
            .await
        });
        let hello = serde_json::json!({
            "t": "hb",
            "token": "test-token-1234567890",
            "acct": 94168,
            "server": "IFCMarkets-Real",
            "symbol": "EURUSD",
            "connected": true,
            "tradeAllowed": true,
            "orders": 0,
            "lots": 0.0
        });
        let mut command = None;
        for _ in 0..40 {
            let response = test::call_service(
                &ea_app,
                test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            let body: Value = test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            actix_web::rt::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let command = command.expect("history command delivered");
        assert_eq!(command["kind"], "order_history");
        assert_eq!(command["history"]["days"], 7);

        let ack = serde_json::json!({
            "t": "ack",
            "token": "test-token-1234567890",
            "id": command["id"],
            "ok": true,
            "data": {
                "orders": [
                    {
                        "ticket": 10_655_087,
                        "symbol": "GBPUSD",
                        "kind": "sell",
                        "lots": 0.01,
                        "openPrice": 1.32123,
                        "closePrice": 1.3265,
                        "openTime": 1_790_271_000_i64,
                        "closeTime": 1_790_341_680_i64,
                        "profit": -5.31,
                        "swap": 0.0,
                        "commission": 0.0,
                        "magic": 77041
                    },
                    {
                        "ticket": 10_650_830,
                        "symbol": "USDJPY",
                        "kind": "buy",
                        "lots": 0.01,
                        "openPrice": 156.198,
                        "closePrice": 156.41,
                        "openTime": 1_789_699_082_i64,
                        "closeTime": 1_789_707_257_i64,
                        "profit": 1.36,
                        "swap": 0.0,
                        "commission": 0.0,
                        "magic": 77041
                    }
                ],
                "total": 2,
                "truncated": false
            }
        });
        let response = test::call_service(
            &ea_app,
            test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(ack.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());

        let response = task.await.expect("task joins");
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["days"], 7);
        assert_eq!(body["total"], 2);
        assert_eq!(body["truncated"], false);
        assert_eq!(body["page"], 1);
        assert_eq!(body["pageSize"], 20);
        assert_eq!(body["pageCount"], 1);
        // No account_snapshot was ever completed, so the broker offset (and
        // therefore the UTC conversion) is unknown.
        assert!(body["brokerOffsetSecs"].is_null());
        assert_eq!(body["summary"]["count"], 2);
        assert_eq!(body["summary"]["wins"], 1);
        assert_eq!(body["summary"]["losses"], 1);
        assert_eq!(body["summary"]["net"], -3.95);

        // Newest close first.
        assert_eq!(body["trades"][0]["ticket"], 10_655_087);
        assert_eq!(body["trades"][0]["side"], "short");
        assert_eq!(body["trades"][0]["closeReason"], "agent_close");
        assert_eq!(body["trades"][0]["closeDetail"], "thesis invalidated");
        assert_eq!(
            body["trades"][0]["entryRationale"],
            "GBPUSD has the strongest aligned bearish evidence"
        );
        assert_eq!(body["trades"][0]["stopLoss"], 1.3265);
        assert_eq!(body["trades"][0]["takeProfit"], 1.3162);
        assert_eq!(body["trades"][0]["rMultiple"], -1.0);
        assert_eq!(body["trades"][0]["openedAtMs"], 1_790_271_000_000_i64);
        assert_eq!(body["trades"][0]["closedAtMs"], 1_790_341_680_000_i64);

        // No journal evidence at all: closes unclassified.
        assert_eq!(body["trades"][1]["ticket"], 10_650_830);
        assert_eq!(body["trades"][1]["closeReason"], "unknown");
        assert!(body["trades"][1]["entryRationale"].is_null());
        assert!(body["trades"][1]["stopLoss"].is_null());
        assert!(body["trades"][1]["rMultiple"].is_null());

        // A terminal that fails the request is a gateway failure, exactly as
        // for /performance.
        let route_state = state.clone();
        let failing = actix_web::rt::spawn(async move {
            let app = test::init_service(create_app(route_state)).await;
            test::call_service(&app, test::TestRequest::get().uri("/trades").to_request()).await
        });
        let mut command = None;
        for _ in 0..40 {
            let response = test::call_service(
                &ea_app,
                test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            let body: Value = test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            actix_web::rt::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let command = command.expect("history command delivered again");
        let failure = serde_json::json!({
            "t": "ack",
            "token": "test-token-1234567890",
            "id": command["id"],
            "ok": false,
            "error": "history unavailable"
        });
        let response = test::call_service(
            &ea_app,
            test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(failure.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());
        let response = failing.await.expect("task joins");
        assert_eq!(response.status(), 502);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "history_failed");
        assert_eq!(body["reason"], "history unavailable");
    }

    #[actix_web::test]
    async fn sessions_route_reports_the_week_and_the_entry_policy() {
        let (state, _) = audited_state(None);
        let app = test::init_service(create_app(state)).await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/sessions")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;

        // The clock decides these, so assert structure and invariants rather
        // than a fixed moment: a closed market always blocks entries.
        let now = body["now"].as_i64().expect("now");
        let market_state = body["market"]["state"].as_str().expect("state");
        assert!(
            ["open", "rollover", "closed"].contains(&market_state),
            "{market_state}"
        );
        let next_event = body["market"]["nextEvent"].as_str().expect("event");
        assert!(
            ["opens", "closes", "pauses", "resumes"].contains(&next_event),
            "{next_event}"
        );
        assert!(body["market"]["nextAt"].as_i64().expect("nextAt") > now);
        let entries_open = body["entries"]["open"].as_bool().expect("entries.open");
        if market_state != "open" {
            assert!(!entries_open, "a non-open market never admits entries");
            assert!(body["entries"]["blockedBy"].is_string());
        }
        assert_eq!(body["policy"]["fridayEntryCutoffMinute"], 1_140);
        assert_eq!(body["policy"]["sundayEntryOpenMinute"], 1_380);
        assert_eq!(body["policy"]["rolloverBlackout"]["startMinute"], 1_245);
        assert_eq!(body["policy"]["rolloverBlackout"]["endMinute"], 1_335);

        // The weekend preference and its checkpoint window: the countdown is
        // present only while the market is open toward Friday's close, and it
        // always agrees with the close the market block reports.
        let weekend_policy = body["weekend"]["policy"].as_str().expect("weekend policy");
        assert!(
            ["agent", "hold", "flatten"].contains(&weekend_policy),
            "{weekend_policy}"
        );
        match body["weekend"]["closesInSecs"].as_i64() {
            Some(closes_in) => {
                assert!(closes_in > 0, "the checkpoint only runs before the close");
                assert_eq!(market_state, "open");
                assert_eq!(next_event, "closes");
                assert_eq!(
                    body["market"]["nextAt"].as_i64().expect("nextAt") - now,
                    closes_in
                );
            }
            None => assert!(body["weekend"]["closesInSecs"].is_null()),
        }
    }

    #[actix_web::test]
    async fn command_status_flattens_pings_rates_and_failures() {
        use crate::broker::CommandKind;

        let (state, _) = audited_state(Some(StubFeed { fail: false }));
        let link = state.broker().expect("broker").ea_link().expect("ea link");

        // One command per shape the flattening has to cover.
        let ping = link.enqueue(CommandKind::Ping);
        let cached = link.enqueue(CommandKind::AccountSnapshot);
        let rates = link.enqueue_rates(
            crate::broker::RatesRequest::new(&Symbol::parse("EURUSD").expect("symbol"), 240, 1)
                .expect("rates request"),
        );

        let ea_app = test::init_service(crate::broker::ea::create_ea_app(link.clone())).await;
        let hello = serde_json::json!({
            "t": "hb",
            "token": "test-token-1234567890",
            "acct": 94168,
            "server": "IFCMarkets-Real",
            "symbol": "EURUSD",
            "connected": true,
            "tradeAllowed": true,
            "orders": 0,
            "lots": 0.0
        });
        let mut pending = vec![ping, cached, rates];
        while let Some(id) = pending.first().copied() {
            let mut delivered = false;
            for _ in 0..20 {
                let response = test::call_service(
                    &ea_app,
                    test::TestRequest::post()
                        .uri("/ea/poll")
                        .set_payload(hello.to_string())
                        .to_request(),
                )
                .await;
                let body: Value = test::read_body_json(response).await;
                if body["t"] == "cmd" {
                    assert_eq!(body["id"], id.to_string());
                    let ack = match body["kind"].as_str().expect("kind") {
                        "ping" => {
                            serde_json::json!({"t": "ack", "token": "test-token-1234567890", "id": body["id"], "ok": true})
                        }
                        "account_snapshot" => serde_json::json!({
                            "t": "ack", "token": "test-token-1234567890", "id": body["id"], "ok": false,
                            "error": "terminal busy"
                        }),
                        _ => serde_json::json!({
                            "t": "ack", "token": "test-token-1234567890", "id": body["id"], "ok": true,
                            "data": {
                                "symbol": "EURUSD",
                                "timeframeMinutes": 240,
                                "candles": [{"time": 1_700_000_000, "open": 1.1, "high": 1.2, "low": 1.0, "close": 1.15, "volume": 4}]
                            }
                        }),
                    };
                    let response = test::call_service(
                        &ea_app,
                        test::TestRequest::post()
                            .uri("/ea/poll")
                            .set_payload(ack.to_string())
                            .to_request(),
                    )
                    .await;
                    assert!(response.status().is_success());
                    delivered = true;
                    break;
                }
                actix_web::rt::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(delivered, "command {id} was delivered");
            pending.remove(0);
        }

        let app = test::init_service(create_app(state.clone())).await;

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/commands/{ping}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let ping_body: Value = test::read_body_json(response).await;
        assert_eq!(ping_body["status"], "completed");
        assert_eq!(ping_body["result"], serde_json::json!({}));

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/commands/{cached}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let failed: Value = test::read_body_json(response).await;
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"], "terminal busy");

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/commands/{rates}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let rates_body: Value = test::read_body_json(response).await;
        assert_eq!(rates_body["result"]["symbol"], "EURUSD");
        assert_eq!(rates_body["result"]["candles"][0]["close"], 1.15);
    }

    #[actix_web::test]
    async fn calendar_route_lists_events_and_reports_failures() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;

        // No calendar configured: unavailable, like the market routes.
        let app = test::init_service(create_app(build_state(None, true))).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/calendar").to_request()).await;
        assert_eq!(response.status(), 503);

        // A configured calendar lists the scheduled window oldest first.
        let mut state = build_state(None, true);
        state = state.with_calendar(Some(CalendarRuntime::from_feed(Arc::new(StubCalendar {
            events: vec![
                CalendarEvent::new(
                    "Non-Farm Employment Change",
                    "USD",
                    Impact::High,
                    now + 1_800,
                )
                .expect("event"),
                CalendarEvent::new("ECB Rate Decision", "EUR", Impact::High, now + 7_200)
                    .expect("event"),
            ],
            fail: false,
        }))));
        let app = test::init_service(create_app(state.clone())).await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/calendar?hours=1")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["provider"], "forexfactory");
        let events = body["events"].as_array().expect("events");
        assert_eq!(events.len(), 1, "the 1-hour window excludes the ECB print");
        assert_eq!(events[0]["title"], "Non-Farm Employment Change");
        assert_eq!(events[0]["impact"], "high");
        assert_eq!(events[0]["time"], now + 1_800);

        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/calendar?hours=0")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);

        // A provider that cannot answer is a gateway failure.
        let mut failing = build_state(None, true);
        failing = failing.with_calendar(Some(CalendarRuntime::from_feed(Arc::new(StubCalendar {
            events: Vec::new(),
            fail: true,
        }))));
        let app = test::init_service(create_app(failing)).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/calendar").to_request()).await;
        assert_eq!(response.status(), 502);
    }

    #[actix_web::test]
    async fn command_status_flattens_completed_symbol_specs() {
        let (state, _) = audited_state(Some(StubFeed { fail: false }));
        let link = state.broker().expect("broker").ea_link().expect("ea link");
        let id = link.enqueue_symbol_spec(SymbolSpecRequest::new(
            &Symbol::parse("EURUSD").expect("symbol"),
        ));

        // Deliver and acknowledge the command through the real poll path.
        let ea_app = test::init_service(crate::broker::ea::create_ea_app(link.clone())).await;
        let hello = serde_json::json!({
            "t": "hb",
            "token": "test-token-1234567890",
            "acct": 94168,
            "server": "IFCMarkets-Real",
            "symbol": "EURUSD",
            "connected": true,
            "tradeAllowed": true,
            "orders": 0,
            "lots": 0.0
        });
        let mut command = None;
        for _ in 0..40 {
            let response = test::call_service(
                &ea_app,
                test::TestRequest::post()
                    .uri("/ea/poll")
                    .set_payload(hello.to_string())
                    .to_request(),
            )
            .await;
            let body: Value = test::read_body_json(response).await;
            if body["t"] == "cmd" {
                command = Some(body);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let command = command.expect("symbol_spec command delivered");
        assert_eq!(command["kind"], "symbol_spec");
        let ack = serde_json::json!({
            "t": "ack",
            "token": "test-token-1234567890",
            "id": command["id"],
            "ok": true,
            "data": {
                "symbol": "EURUSD",
                "digits": 5,
                "point": 0.00001,
                "spreadPoints": 12,
                "stopLevelPoints": 5,
                "freezeLevelPoints": 0,
                "lotMin": 0.01,
                "lotMax": 100.0,
                "lotStep": 0.01,
                "tickValue": 0.1,
                "tickSize": 0.00001,
                "marginRequired": 3.29,
                "swapLong": -0.72,
                "swapShort": -0.31,
                "swapType": 0,
                "tradeAllowed": true
            }
        });
        let response = test::call_service(
            &ea_app,
            test::TestRequest::post()
                .uri("/ea/poll")
                .set_payload(ack.to_string())
                .to_request(),
        )
        .await;
        assert!(response.status().is_success());

        let app = test::init_service(create_app(state.clone())).await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/commands/{id}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["kind"], "symbol_spec");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["result"]["symbol"], "EURUSD");
        assert_eq!(body["result"]["spreadPoints"], 12);
        assert_eq!(body["result"]["marginRequired"], 3.29);
    }

    #[actix_web::test]
    async fn candle_feed_failures_are_gateway_errors() {
        let app =
            test::init_service(create_app(build_state(Some(StubFeed { fail: true }), true))).await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/market/candles?symbol=EURUSD")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 502);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "market_feed_failed");
        assert!(
            body["reason"]
                .as_str()
                .expect("reason")
                .contains("terminal")
        );
    }

    #[actix_web::test]
    async fn account_snapshot_requires_a_command_channel() {
        let app = test::init_service(create_app(build_state(None, false))).await;
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/commands/account_snapshot")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 503);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "command_channel_unavailable");
    }

    #[actix_web::test]
    async fn account_state_requires_a_broker() {
        let app = test::init_service(create_app(build_state(None, false))).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/account").to_request()).await;
        assert_eq!(response.status(), 503);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "broker_unavailable");
    }

    #[actix_web::test]
    async fn market_spec_reports_a_missing_default_symbol() {
        let app = test::init_service(create_app(build_state(
            Some(StubFeed { fail: false }),
            false,
        )))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get().uri("/market/spec").to_request(),
        )
        .await;
        assert_eq!(response.status(), 409);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "symbol_unavailable");
    }

    #[actix_web::test]
    async fn execute_intent_reports_a_risk_rejection() {
        // The default policy allows no instrument at all, so any draft is
        // rejected before the broker or command channel are ever touched.
        let state = build_state(None, true);
        state.set_trading_enabled(true);
        let app = test::init_service(create_app(state)).await;
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/intents/execute")
                .set_json(serde_json::json!({
                    "symbol": "EURUSD",
                    "side": "buy",
                    "order_type": "market",
                    "volume": 0.01
                }))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["decision"], "rejected");
        assert_eq!(body["code"], "symbol_not_allowed");
    }

    #[actix_web::test]
    async fn queue_staged_close_reports_channel_unavailable() {
        let state = build_state(None, false);
        state.set_trading_enabled(true);
        assert_eq!(
            queue_staged_close(&state, 123).await,
            StagedClose::ChannelUnavailable
        );
    }

    /// Audit trail whose reads always fail, to exercise the degraded path of
    /// routes that read through it.
    #[derive(Debug)]
    struct RefusingTrail;

    #[async_trait]
    impl AuditTrail for RefusingTrail {
        fn provider(&self) -> AuditProvider {
            AuditProvider::Postgres
        }

        async fn record(&self, _event: crate::audit::AuditEvent) -> Result<(), AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }

        async fn recent(&self, _limit: u32) -> Result<Vec<AuditRow>, AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }

        async fn prune(&self, _keep_days: u32) -> Result<u64, AuditError> {
            Err(AuditError::Storage {
                reason: "down".to_owned(),
            })
        }
    }

    #[actix_web::test]
    async fn audit_log_reports_unavailable_storage() {
        let state =
            build_state(None, false).with_audit(Some(AuditRuntime::new(Arc::new(RefusingTrail))));
        let app = test::init_service(create_app(state)).await;
        let response =
            test::call_service(&app, test::TestRequest::get().uri("/audit").to_request()).await;
        assert_eq!(response.status(), 503);
        let body: Value = test::read_body_json(response).await;
        assert_eq!(body["status"], "unavailable");
    }

    mod model_routes {
        use std::sync::Arc;

        use actix_web::test;
        use serde_json::{Value, json};

        use crate::app::create_app;
        use crate::credential::TEST_ADMIN_TOKEN;
        use crate::model::cooldown::test_clock::ManualClock;
        use crate::model::subscription_engine::test_support::app;
        use crate::model::{
            Admission, CooldownFailure, CooldownReason, CooldownRegistry, ModelProvider,
        };
        use crate::runtime_config::test_support::model_overlay;
        use crate::state::RuntimeState;
        use crate::state::test_support::FailingState;
        use crate::subscription_auth::SubscriptionProvider;

        fn cool(registry: &CooldownRegistry, provider: ModelProvider, model: &str) {
            if let Admission::Ready(ticket) = registry.admit(provider, model) {
                ticket.fail(CooldownFailure::new(CooldownReason::InsufficientCredits));
            }
        }

        fn preferred_state(clock: &ManualClock) -> crate::AppState {
            let (state, _store) = app(
                CooldownRegistry::with_clock(clock.clock()),
                &[SubscriptionProvider::Codex],
            );
            model_overlay(state.runtime_config(), &[]);
            crate::runtime_config::rebuild_model(&state).expect("model builds");
            state
        }

        #[actix_web::test]
        async fn status_reports_the_route_and_cooldowns_in_force() {
            let clock = ManualClock::new();
            let state = preferred_state(&clock);
            cool(state.model_cooldowns(), ModelProvider::Codex, "gpt-6-luna");
            let app = test::init_service(create_app(state)).await;
            let response =
                test::call_service(&app, test::TestRequest::get().uri("/status").to_request())
                    .await;
            let body: Value = test::read_body_json(response).await;
            assert_eq!(
                body["model_route"],
                json!([
                    "chatgpt:gpt-6-luna",
                    "deepseek/deepseek-v4.1-flash",
                    "z-ai/glm-5.3-flash"
                ])
            );
            assert_eq!(body["model_provider"], "openrouter");
            assert_eq!(
                body["model_cooldowns"],
                json!([{
                    "provider": "codex",
                    "model": "gpt-6-luna",
                    "reason": "insufficient_credits",
                    "untilMs": 1_767_225_600_000_u64 + 30 * 60 * 1_000,
                    "failures": 1
                }])
            );
            assert_eq!(body["decisions"]["lastModel"], Value::Null);
        }

        #[actix_web::test]
        async fn cooldowns_can_be_listed_and_cleared_by_the_operator() {
            let clock = ManualClock::new();
            let state = preferred_state(&clock);
            cool(
                state.model_cooldowns(),
                ModelProvider::OpenRouter,
                "z-ai/glm-5.3-flash",
            );
            cool(state.model_cooldowns(), ModelProvider::Codex, "gpt-6-luna");
            let app = test::init_service(create_app(state.clone())).await;

            let listed: Value = test::read_body_json(
                test::call_service(
                    &app,
                    test::TestRequest::get()
                        .uri("/model/cooldowns")
                        .to_request(),
                )
                .await,
            )
            .await;
            assert_eq!(listed["model_cooldowns"].as_array().map(Vec::len), Some(2));

            let cleared: Value = test::read_body_json(
                test::call_service(
                    &app,
                    test::TestRequest::post()
                        .uri("/model/cooldowns/clear")
                        .to_request(),
                )
                .await,
            )
            .await;
            assert_eq!(cleared, json!({"cleared": 2, "model_cooldowns": []}));
            assert!(state.model_cooldowns().is_empty());
        }

        #[actix_web::test]
        async fn disconnecting_chatgpt_rebuilds_the_route_and_clears_cooldowns() {
            let clock = ManualClock::new();
            let state = preferred_state(&clock);
            assert!(
                state
                    .model()
                    .is_some_and(|model| model.prefers_subscription())
            );
            cool(
                state.model_cooldowns(),
                ModelProvider::OpenRouter,
                "z-ai/glm-5.3-flash",
            );
            let app = test::init_service(create_app(state.clone())).await;

            let refused = test::call_service(
                &app,
                test::TestRequest::delete()
                    .uri("/model/subscriptions/codex")
                    .to_request(),
            )
            .await;
            assert_eq!(refused.status(), 401, "the operator token is required");
            assert_eq!(state.model_cooldowns().len(), 1);

            let response = test::call_service(
                &app,
                test::TestRequest::delete()
                    .uri("/model/subscriptions/codex")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 200);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body, json!({"deleted": true, "provider": "codex"}));
            let model = state.model().expect("the configured provider remains");
            assert!(!model.prefers_subscription());
            assert_eq!(
                model.route(crate::model::ModelTier::Balanced),
                ["deepseek/deepseek-v4.1-flash", "z-ai/glm-5.3-flash"]
            );
            assert!(state.model_cooldowns().is_empty());
        }

        #[actix_web::test]
        async fn claude_code_disconnects_leave_the_chatgpt_route_and_cooldowns_alone() {
            let clock = ManualClock::new();
            let state = preferred_state(&clock);
            cool(
                state.model_cooldowns(),
                ModelProvider::OpenRouter,
                "z-ai/glm-5.3-flash",
            );
            let before = state.model().expect("configured");
            let app = test::init_service(create_app(state.clone())).await;
            let response = test::call_service(
                &app,
                test::TestRequest::delete()
                    .uri("/model/subscriptions/claude_code")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 200);
            assert_eq!(state.model_cooldowns().len(), 1);
            assert!(
                state
                    .model()
                    .is_some_and(|model| model.same_instance(&before))
            );
        }

        #[actix_web::test]
        async fn a_chatgpt_change_leaves_a_selected_claude_code_runtime_alone() {
            let (state, _store) = app(
                CooldownRegistry::new(),
                &[
                    SubscriptionProvider::Codex,
                    SubscriptionProvider::ClaudeCode,
                ],
            );
            model_overlay(
                state.runtime_config(),
                &[
                    ("VEYRA_MODEL_PROVIDER", "claude_code"),
                    ("VEYRA_MODEL_FAST", "claude-model"),
                    ("VEYRA_MODEL_BALANCED", "claude-model"),
                    ("VEYRA_MODEL_REASONING", "claude-model"),
                    ("VEYRA_MODEL_FALLBACKS", ""),
                ],
            );
            crate::runtime_config::rebuild_model(&state).expect("model builds");
            let before = state.model().expect("configured");
            assert_eq!(before.provider(), ModelProvider::ClaudeCode);
            let app = test::init_service(create_app(state.clone())).await;
            let response = test::call_service(
                &app,
                test::TestRequest::delete()
                    .uri("/model/subscriptions/codex")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 200);
            assert!(
                state
                    .model()
                    .is_some_and(|model| model.same_instance(&before)),
                "the Claude Code engine is not rebuilt"
            );
        }

        #[actix_web::test]
        async fn a_new_model_credential_clears_cooldowns() {
            let clock = ManualClock::new();
            let state = preferred_state(&clock);
            cool(
                state.model_cooldowns(),
                ModelProvider::OpenRouter,
                "deepseek/deepseek-v4.1-flash",
            );
            let app = test::init_service(create_app(state.clone())).await;
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/model/credential")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"key": "a-topped-up-key-123456"}))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 200);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["active"], true);
            assert!(state.model_cooldowns().is_empty());
            assert!(
                state
                    .model()
                    .is_some_and(|model| model.prefers_subscription()),
                "the preference survives a credential change"
            );
        }

        #[actix_web::test]
        async fn model_subscriptions_reports_connection_and_labels() {
            let (state, _store) = app(CooldownRegistry::new(), &[SubscriptionProvider::Codex]);
            let app_service = test::init_service(create_app(state)).await;
            let response = test::call_service(
                &app_service,
                test::TestRequest::get()
                    .uri("/model/subscriptions")
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 200);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["subscriptions"]["codex"]["connected"], true);
            assert_eq!(body["subscriptions"]["codex"]["account_label"], "operator");
            assert_eq!(body["subscriptions"]["claude_code"]["connected"], false);
            assert_eq!(
                body["subscriptions"]["claude_code"]["account_label"],
                Value::Null
            );
        }

        #[actix_web::test]
        async fn starting_a_subscription_requires_a_token_and_a_known_provider() {
            let (state, _store) = app(CooldownRegistry::new(), &[]);
            let app_service = test::init_service(create_app(state)).await;

            let refused = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/subscriptions/start")
                    .set_json(json!({"provider": "codex"}))
                    .to_request(),
            )
            .await;
            assert_eq!(refused.status(), 401);

            let bad_provider = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/subscriptions/start")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"provider": "unknown"}))
                    .to_request(),
            )
            .await;
            assert_eq!(bad_provider.status(), 400);
            assert_eq!(
                test::read_body_json::<Value, _>(bad_provider).await["error"],
                "unsupported_subscription_provider"
            );

            let started = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/subscriptions/start")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"provider": "codex"}))
                    .to_request(),
            )
            .await;
            assert_eq!(started.status(), 200);
            let body: Value = test::read_body_json(started).await;
            assert_eq!(body["provider"], "codex");
            assert!(
                body["authorize_url"]
                    .as_str()
                    .expect("authorize_url")
                    .contains("code_challenge=")
            );
            assert!(!body["state"].as_str().expect("state").is_empty());
        }

        #[actix_web::test]
        async fn completing_a_subscription_validates_before_contacting_the_provider() {
            let (state, _store) = app(CooldownRegistry::new(), &[]);
            let app_service = test::init_service(create_app(state)).await;

            let refused = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/subscriptions/complete")
                    .set_json(json!({"provider": "codex", "callback_value": "abc#xyz"}))
                    .to_request(),
            )
            .await;
            assert_eq!(refused.status(), 401);

            let bad_provider = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/subscriptions/complete")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"provider": "unknown", "callback_value": "abc#xyz"}))
                    .to_request(),
            )
            .await;
            assert_eq!(bad_provider.status(), 400);
            assert_eq!(
                test::read_body_json::<Value, _>(bad_provider).await["error"],
                "unsupported_subscription_provider"
            );

            let bad_callback = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/subscriptions/complete")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"provider": "codex", "callback_value": "   "}))
                    .to_request(),
            )
            .await;
            assert_eq!(bad_callback.status(), 400);
            assert_eq!(
                test::read_body_json::<Value, _>(bad_callback).await["error"],
                "invalid_subscription_callback"
            );

            // A well-formed callback with no pending flow (or the wrong CSRF
            // state) is refused before any network call is ever made.
            let no_pending = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/subscriptions/complete")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"provider": "codex", "callback_value": "abc#xyz"}))
                    .to_request(),
            )
            .await;
            assert_eq!(no_pending.status(), 400);
            assert_eq!(
                test::read_body_json::<Value, _>(no_pending).await["error"],
                "invalid_subscription_state"
            );
        }

        #[actix_web::test]
        async fn deleting_an_unknown_subscription_provider_is_rejected() {
            let (state, _store) = app(CooldownRegistry::new(), &[]);
            let app_service = test::init_service(create_app(state)).await;
            let response = test::call_service(
                &app_service,
                test::TestRequest::delete()
                    .uri("/model/subscriptions/unknown")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 400);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["error"], "unsupported_subscription_provider");
        }

        #[actix_web::test]
        async fn deleting_the_active_codex_subscription_disables_the_model() {
            let (state, _store) = app(CooldownRegistry::new(), &[SubscriptionProvider::Codex]);
            model_overlay(
                state.runtime_config(),
                &[
                    ("VEYRA_MODEL_PROVIDER", "codex"),
                    ("VEYRA_MODEL_FAST", "gpt-6-luna"),
                    ("VEYRA_MODEL_BALANCED", "gpt-6-luna"),
                    ("VEYRA_MODEL_REASONING", "gpt-6-luna"),
                    ("VEYRA_MODEL_FALLBACKS", ""),
                ],
            );
            crate::runtime_config::rebuild_model(&state)
                .expect("the connected subscription builds");
            assert!(state.model().is_some());

            let app_service = test::init_service(create_app(state.clone())).await;
            let response = test::call_service(
                &app_service,
                test::TestRequest::delete()
                    .uri("/model/subscriptions/codex")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 200);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body, json!({"deleted": true, "provider": "codex"}));
            assert!(
                state.model().is_none(),
                "the model that depended on the deleted subscription is disabled"
            );
        }

        #[actix_web::test]
        async fn a_storage_failure_refuses_to_delete_a_subscription() {
            let (state, _store) = app(CooldownRegistry::new(), &[SubscriptionProvider::Codex]);
            let state = state.with_runtime_state(RuntimeState::new(Some(Arc::new(FailingState))));
            let app_service = test::init_service(create_app(state)).await;
            let response = test::call_service(
                &app_service,
                test::TestRequest::delete()
                    .uri("/model/subscriptions/codex")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 503);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["error"], "credential_storage_failed");
        }

        #[actix_web::test]
        async fn setting_a_model_credential_validates_before_persisting() {
            let (state, _store) = app(CooldownRegistry::new(), &[]);
            let app_service = test::init_service(create_app(state)).await;

            let refused = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/credential")
                    .set_json(json!({"key": "irrelevant"}))
                    .to_request(),
            )
            .await;
            assert_eq!(refused.status(), 401);

            let too_long = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/credential")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"key": "x".repeat(4_097)}))
                    .to_request(),
            )
            .await;
            assert_eq!(too_long.status(), 400);
            assert_eq!(
                test::read_body_json::<Value, _>(too_long).await["error"],
                "invalid_model_key"
            );

            let too_short = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/credential")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"key": "short"}))
                    .to_request(),
            )
            .await;
            assert_eq!(too_short.status(), 400);
            assert_eq!(
                test::read_body_json::<Value, _>(too_short).await["error"],
                "invalid_model_key"
            );
        }

        #[actix_web::test]
        async fn deleting_a_model_credential_requires_a_token_and_persists() {
            let (state, _store) = app(CooldownRegistry::new(), &[SubscriptionProvider::Codex]);
            model_overlay(
                state.runtime_config(),
                &[
                    ("VEYRA_MODEL_PROVIDER", "codex"),
                    ("VEYRA_MODEL_FAST", "gpt-6-luna"),
                    ("VEYRA_MODEL_BALANCED", "gpt-6-luna"),
                    ("VEYRA_MODEL_REASONING", "gpt-6-luna"),
                    ("VEYRA_MODEL_FALLBACKS", ""),
                ],
            );
            crate::runtime_config::rebuild_model(&state).expect("model builds");
            let app_service = test::init_service(create_app(state.clone())).await;

            let refused = test::call_service(
                &app_service,
                test::TestRequest::delete()
                    .uri("/model/credential")
                    .to_request(),
            )
            .await;
            assert_eq!(refused.status(), 401);

            let response = test::call_service(
                &app_service,
                test::TestRequest::delete()
                    .uri("/model/credential")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 200);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["saved"], true);
            assert_eq!(body["active"], true);
            assert!(state.model().is_some());
        }

        #[actix_web::test]
        async fn a_storage_failure_refuses_to_save_a_model_credential() {
            let (state, _store) = app(CooldownRegistry::new(), &[]);
            let state = state.with_runtime_state(RuntimeState::new(Some(Arc::new(FailingState))));
            let app_service = test::init_service(create_app(state)).await;
            let response = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/credential")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"key": "a-fresh-key-1234567890"}))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 503);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["error"], "credential_storage_failed");
        }

        #[actix_web::test]
        async fn a_credential_change_with_broken_model_settings_disables_the_model() {
            let (state, _store) = app(CooldownRegistry::new(), &[]);
            // Every model field parses except a required tier, so validation
            // fails only once the fresh credential is already in the overlay.
            model_overlay(state.runtime_config(), &[("VEYRA_MODEL_FAST", "")]);
            let app_service = test::init_service(create_app(state.clone())).await;
            let response = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/model/credential")
                    .insert_header(("x-veyra-admin-token", TEST_ADMIN_TOKEN))
                    .set_json(json!({"key": "a-fresh-key-1234567890"}))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 202);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["saved"], true);
            assert_eq!(body["active"], false);
            assert_eq!(body["reason"], "model_settings_incomplete");
            assert!(state.model().is_none());
        }

        #[actix_web::test]
        async fn a_config_patch_that_fails_to_apply_still_keeps_the_saved_overlay() {
            let (state, _store) = app(CooldownRegistry::new(), &[]);
            let app_service = test::init_service(create_app(state.clone())).await;
            let response = test::call_service(
                &app_service,
                test::TestRequest::post()
                    .uri("/config")
                    .set_json(json!({
                        "VEYRA_MODEL_PROVIDER": "codex",
                        "VEYRA_MODEL_FAST": "gpt-6-luna",
                        "VEYRA_MODEL_BALANCED": "gpt-6-luna",
                        "VEYRA_MODEL_REASONING": "gpt-6-luna",
                        "VEYRA_MODEL_FALLBACKS": ""
                    }))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), 500);
            let body: Value = test::read_body_json(response).await;
            assert_eq!(body["error"], "settings_saved_but_not_applied");
            assert!(
                body["reason"]
                    .as_str()
                    .expect("reason")
                    .contains("connect the selected subscription first")
            );
            // The overlay itself is retained even though the swap failed.
            assert_eq!(
                state.runtime_config().effective()["VEYRA_MODEL_PROVIDER"]["value"],
                "codex"
            );
        }
    }
}
