//! HTTP surface for the judge selection.
//!
//! * `GET /judge` — selection, fallback availability, key hint, latest test,
//!   and fallback count. Read-only; never returns the key.
//! * `PUT /judge` `{"provider": "openai"|"typesafe"}` — selects the judge;
//!   OpenAI is refused with `409` until its preconditions hold.
//! * `PUT /judge/openai/key` `{"key": "sk-..."}` / `DELETE /judge/openai/key`
//!   — saves (sealed) or removes the OpenAI key; either returns the selection
//!   to TypeSafe.
//! * `POST /judge/openai/test` — one fixed probe through OpenAI only.
//!
//! Every write requires the operator token (`x-veyra-admin-token`) and a
//! configured credential vault; see [`crate::control::credential_rejection`].

use actix_web::{HttpRequest, HttpResponse, delete, get, post, put, web};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{JudgeControl, Refusal};
use crate::AppState;
use crate::jev::{JevProvider, OpenAiApiKey};

/// Body for `PUT /judge`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectBody {
    provider: String,
}

/// Body for `PUT /judge/openai/key`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyBody {
    key: String,
}

fn unavailable() -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(json!({
        "error": "judge_selection_unavailable",
        "reason": "The service was started without judge selection."
    }))
}

fn refused(refusal: Refusal) -> HttpResponse {
    HttpResponse::build(refusal.status).json(json!({
        "error": refusal.code,
        "reason": refusal.reason
    }))
}

fn invalid(code: &'static str, reason: String) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": code, "reason": reason }))
}

/// Authenticates a write and returns the control it applies to.
fn authorized<'a>(
    request: &HttpRequest,
    state: &'a AppState,
) -> Result<&'a JudgeControl, Box<HttpResponse>> {
    if let Some(rejection) = crate::control::credential_rejection(request, state) {
        return Err(Box::new(rejection));
    }
    state.judge_control().ok_or_else(|| Box::new(unavailable()))
}

async fn record_change(state: &AppState, control: &JudgeControl, change: &str) {
    if let Some(audit) = state.audit() {
        audit
            .try_record(crate::audit::AuditEvent::new(
                crate::audit::AuditKind::RuntimeConfigUpdated,
                json!({
                    "origin": "console",
                    "section": "judge",
                    "change": change,
                    "provider": control.provider().as_str(),
                }),
            ))
            .await;
    }
}

#[get("/judge")]
/// The judge selection as the console shows it.
pub async fn judge(state: web::Data<AppState>) -> HttpResponse {
    match state.judge_control() {
        Some(control) => HttpResponse::Ok().json(control.view(&state)),
        None => unavailable(),
    }
}

#[put("/judge")]
/// Selects TypeSafe Jev or OpenAI Decisions (with Jev fallback).
pub async fn select_judge(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<Value>,
) -> HttpResponse {
    let control = match authorized(&request, &state) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    let body: SelectBody = match serde_json::from_value(body.into_inner()) {
        Ok(body) => body,
        Err(error) => return invalid("invalid_judge", error.to_string()),
    };
    let Some(provider) = JevProvider::parse(&body.provider) else {
        return invalid(
            "unknown_judge",
            "provider must be `typesafe` or `openai`".to_owned(),
        );
    };
    if let Err(refusal) = control.select(&state, provider).await {
        return refused(refusal);
    }
    record_change(&state, control, "provider").await;
    HttpResponse::Ok().json(control.view(&state))
}

#[put("/judge/openai/key")]
/// Seals and saves an OpenAI API key; the key is never echoed.
pub async fn set_openai_key(
    request: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<Value>,
) -> HttpResponse {
    let control = match authorized(&request, &state) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    // Parse errors are reported by field, never by value: serde would quote
    // the rejected input, which here is the secret.
    let Ok(body) = serde_json::from_value::<KeyBody>(body.into_inner()) else {
        return invalid(
            "invalid_openai_key",
            "send {\"key\": \"sk-...\"}".to_owned(),
        );
    };
    let key = match OpenAiApiKey::parse(&body.key) {
        Ok(key) => key,
        Err(error) => return invalid("invalid_openai_key", error.to_string()),
    };
    if let Err(refusal) = control.save_key(&state, key).await {
        return refused(refusal);
    }
    record_change(&state, control, "openai_key_saved").await;
    HttpResponse::Ok().json(control.view(&state))
}

#[delete("/judge/openai/key")]
/// Removes the saved OpenAI key; TypeSafe answers from then on.
pub async fn delete_openai_key(request: HttpRequest, state: web::Data<AppState>) -> HttpResponse {
    let control = match authorized(&request, &state) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    if let Err(refusal) = control.remove_key(&state).await {
        return refused(refusal);
    }
    record_change(&state, control, "openai_key_removed").await;
    HttpResponse::Ok().json(control.view(&state))
}

#[post("/judge/openai/test")]
/// Runs the fixed probe through OpenAI only and records the result.
pub async fn test_openai(request: HttpRequest, state: web::Data<AppState>) -> HttpResponse {
    let control = match authorized(&request, &state) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    match control.test(&state).await {
        Ok(record) => HttpResponse::Ok().json(json!({
            "ok": record.ok,
            "latencyMs": record.latency_ms,
            "detail": record.detail,
        })),
        Err(refusal) => refused(refusal),
    }
}
