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
use std::time::SystemTime;

use actix_web::web::{self, Data};
use actix_web::{HttpResponse, get, post};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::audit::{AuditEvent, AuditKind};
use crate::broker::ea::{
    CommandId, CommandKind, CommandPayload, CommandState, EaCloseRequest, EaLink, EaModifyRequest,
    EaOrderRequest, ORDER_MAGIC,
};
use crate::risk::RiskDecision;
use crate::trading::TradeIntentDraft;

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
    let account = crate::routes::account_facts(state.broker()).await;
    match state
        .risk()
        .evaluate(&draft.into_inner(), account, SystemTime::now())
    {
        RiskDecision::Rejected(rejection) => {
            HttpResponse::Ok().json(RiskDecision::Rejected(rejection))
        }
        RiskDecision::Approved(intent) => {
            let command = link.enqueue_order_check(EaOrderRequest::from_intent(&intent));
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
    let command = link.enqueue(CommandKind::AccountSnapshot);
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
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    };
    if !state.config().trading_enabled() {
        return HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }));
    }
    let account = crate::routes::account_facts(state.broker()).await;
    match state
        .risk()
        .evaluate(&draft.into_inner(), account, SystemTime::now())
    {
        RiskDecision::Rejected(rejection) => {
            HttpResponse::Ok().json(RiskDecision::Rejected(rejection))
        }
        RiskDecision::Approved(intent) => {
            let command = link.enqueue_order(EaOrderRequest::from_intent(&intent));
            audit(
                &state,
                AuditKind::CommandQueued,
                json!({
                    "command_id": command.to_string(),
                    "kind": "open_order",
                    "intent_id": intent.id().to_string()
                }),
            )
            .await;
            HttpResponse::Ok().json(json!({
                "decision": "approved",
                "intent_id": intent.id().to_string(),
                "command": "open_order",
                "command_id": command.to_string(),
                "status": "pending"
            }))
        }
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
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    };
    if !state.config().trading_enabled() {
        return HttpResponse::Forbidden().json(json!({ "error": "trading_disabled" }));
    }
    if body.ticket <= 0 {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid_ticket" }));
    }
    let Some(snapshot) = link.last_account() else {
        return HttpResponse::Conflict().json(json!({ "error": "position_state_unavailable" }));
    };
    let Some(position) = snapshot
        .positions
        .iter()
        .find(|position| position.ticket == body.ticket)
    else {
        return HttpResponse::NotFound().json(json!({ "error": "unknown_position" }));
    };
    if position.magic != ORDER_MAGIC {
        return HttpResponse::Conflict().json(json!({ "error": "not_a_veyra_position" }));
    }
    let command = link.enqueue_close(EaCloseRequest::new(position.ticket, position.magic));
    audit(
        &state,
        AuditKind::CommandQueued,
        json!({
            "command_id": command.to_string(),
            "kind": "close_order",
            "ticket": position.ticket
        }),
    )
    .await;
    HttpResponse::Ok().json(json!({
        "command": "close_order",
        "command_id": command.to_string(),
        "ticket": position.ticket,
        "status": "pending"
    }))
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
    let Some(link) = command_link(&state) else {
        return HttpResponse::ServiceUnavailable()
            .json(json!({ "error": "command_channel_unavailable" }));
    };
    if !state.config().trading_enabled() {
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
    let Some(snapshot) = link.last_account() else {
        return HttpResponse::Conflict().json(json!({ "error": "position_state_unavailable" }));
    };
    let Some(position) = snapshot
        .positions
        .iter()
        .find(|position| position.ticket == body.ticket)
    else {
        return HttpResponse::NotFound().json(json!({ "error": "unknown_position" }));
    };
    if position.magic != ORDER_MAGIC {
        return HttpResponse::Conflict().json(json!({ "error": "not_a_veyra_position" }));
    }
    let command = link.enqueue_modify(EaModifyRequest::new(
        position.ticket,
        position.magic,
        body.stop_loss,
        body.take_profit,
    ));
    audit(
        &state,
        AuditKind::CommandQueued,
        json!({
            "command_id": command.to_string(),
            "kind": "modify_order",
            "ticket": position.ticket
        }),
    )
    .await;
    HttpResponse::Ok().json(json!({
        "command": "modify_order",
        "command_id": command.to_string(),
        "ticket": position.ticket,
        "status": "pending"
    }))
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
    let Some(link) = runtime.ea_link() else {
        return HttpResponse::Ok().json(json!({ "status": "unavailable" }));
    };
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
fn command_link(state: &AppState) -> Option<Arc<EaLink>> {
    state.broker()?.ea_link()
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
