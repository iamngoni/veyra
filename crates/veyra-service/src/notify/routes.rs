//! HTTP surface for notification settings, plus their durable storage.
//!
//! * `GET /notifications` — settings with secrets redacted, queue counters,
//!   and recent deliveries. Read-only.
//! * `PUT /notifications` — applies a [`NotifyPatch`]. Requires the operator
//!   token because it can carry provider secrets.
//! * `POST /notifications/test` — sends a test message to one provider now,
//!   using its saved settings. Requires the operator token.
//!
//! Non-secret settings persist in plain runtime state; secrets persist only
//! sealed by the credential vault. A change is saved before it is applied, so
//! a restart never forgets an accepted change.

use actix_web::{HttpRequest, HttpResponse, get, post, put, web};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{NotifyConfig, NotifyPatch, NotifyPrefs, NotifySecrets, ProviderKind};
use crate::AppState;
use crate::credential::CredentialVault;
use crate::state::{RuntimeState, StateKey};

/// Loads saved notification settings, opening the sealed secrets.
///
/// # Errors
/// Returns a reason when stored settings are unreadable or cannot be
/// decrypted, so startup fails instead of silently dropping a provider.
pub async fn load(
    runtime_state: &RuntimeState,
    vault: &CredentialVault,
) -> Result<NotifyConfig, String> {
    let prefs = match runtime_state
        .load_required(StateKey::NotifyPrefs)
        .await
        .map_err(|error| error.to_string())?
    {
        Some(stored) => serde_json::from_value::<NotifyPrefs>(stored)
            .map_err(|error| format!("stored notification settings are unreadable: {error}"))?,
        None => NotifyPrefs::default(),
    };
    let secrets = match runtime_state
        .load_required(StateKey::NotifySecrets)
        .await
        .map_err(|error| error.to_string())?
    {
        Some(stored) => match vault.open_text(&stored)? {
            Some(plain) => serde_json::from_str::<NotifySecrets>(&plain).map_err(|error| {
                format!("stored notification credentials are unreadable: {error}")
            })?,
            None => NotifySecrets::new(),
        },
        None => NotifySecrets::new(),
    };
    Ok(NotifyConfig { prefs, secrets })
}

async fn save(state: &AppState, config: &NotifyConfig) -> Result<(), String> {
    let vault = state
        .credential_vault()
        .ok_or("credential storage is unavailable")?;
    let prefs = serde_json::to_value(&config.prefs).map_err(|error| error.to_string())?;
    let secrets = serde_json::to_string(&config.secrets).map_err(|error| error.to_string())?;
    let sealed = vault.seal_text(&secrets)?;
    let runtime_state = state.runtime_state();
    runtime_state
        .save_required(StateKey::NotifySecrets, &sealed)
        .await
        .map_err(|error| error.to_string())?;
    runtime_state
        .save_required(StateKey::NotifyPrefs, &prefs)
        .await
        .map_err(|error| error.to_string())
}

#[get("/notifications")]
/// Notification settings with secrets redacted, counters, and recent
/// deliveries.
pub async fn notifications(state: web::Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(state.notifier().view())
}

#[put("/notifications")]
/// Applies a settings change: validated as a whole, saved, then applied.
pub async fn update_notifications(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<Value>,
) -> HttpResponse {
    if let Some(rejection) = crate::control::credential_rejection(&request, &state) {
        return rejection;
    }
    let patch: NotifyPatch = match serde_json::from_value(body.into_inner()) {
        Ok(patch) => patch,
        Err(error) => {
            return HttpResponse::BadRequest().json(json!({
                "error": "invalid_notifications",
                "rejected": [{ "field": "body", "reason": error.to_string() }]
            }));
        }
    };
    let notifier = state.notifier();
    let next = match notifier.config().patched(&patch) {
        Ok(next) => next,
        Err(rejected) => {
            return HttpResponse::BadRequest()
                .json(json!({ "error": "invalid_notifications", "rejected": rejected }));
        }
    };
    if let Err(reason) = save(&state, &next).await {
        tracing::warn!(%reason, "notification settings could not be saved");
        return HttpResponse::InternalServerError()
            .json(json!({ "error": "notifications_not_saved", "reason": reason }));
    }
    notifier.set_config(next);
    if let Some(audit) = state.audit() {
        audit
            .try_record(crate::audit::AuditEvent::new(
                crate::audit::AuditKind::RuntimeConfigUpdated,
                json!({
                    "origin": "console",
                    "section": "notifications",
                    "providers": notifier.config().enabled_providers()
                        .iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
                }),
            ))
            .await;
    }
    HttpResponse::Ok().json(notifier.view())
}

/// Body for `POST /notifications/test`.
#[derive(Debug, Deserialize)]
pub struct TestRequest {
    /// Provider wire name.
    pub provider: String,
}

#[post("/notifications/test")]
/// Sends a test message to one provider now, using its saved settings.
pub async fn test_notification(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<TestRequest>,
) -> HttpResponse {
    if let Some(rejection) = crate::control::credential_rejection(&request, &state) {
        return rejection;
    }
    let Some(kind) = ProviderKind::parse(&body.provider) else {
        return HttpResponse::BadRequest().json(json!({ "error": "unknown_provider" }));
    };
    match state.notifier().test(kind).await {
        Ok(()) => HttpResponse::Ok().json(json!({ "ok": true })),
        Err(reason) => HttpResponse::BadGateway()
            .json(json!({ "error": "notification_failed", "reason": reason })),
    }
}
