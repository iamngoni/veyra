//! The currently supported HTTP contract.
//!
//! Every route is either a diagnostic or a deterministic, non-executing
//! evaluation. Nothing here can place, modify, or cancel an order, and no
//! response exposes credentials, account balances, or model prompts. Adding an
//! executable route requires an explicit design change plus the risk gate.

use std::time::SystemTime;

use actix_web::web::{self, Data};
use actix_web::{HttpResponse, get, post};
use serde::Serialize;

use crate::AppState;
use crate::broker::BrokerRuntime;
use crate::risk::{AccountFacts, RiskDecision};
use crate::trading::TradeIntentDraft;

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct ReadinessResponse {
    status: &'static str,
    trading_enabled: bool,
    broker: &'static str,
    audit: &'static str,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    service: &'static str,
    version: &'static str,
    environment: String,
    broker_provider: Option<&'static str>,
    market_provider: Option<&'static str>,
    model_provider: Option<&'static str>,
    jev_provider: Option<&'static str>,
    persistence: Option<&'static str>,
    broker_connected: bool,
    trading_enabled: bool,
    ea_live_orders: bool,
}

#[get("/health")]
/// Returns a fast process liveness response.
pub async fn health() -> HttpResponse {
    HttpResponse::Ok().json(HealthResponse {
        status: "ok",
        service: "veyra",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[get("/ready")]
/// Reports dependency-aware readiness without gating the diagnostic surface:
/// the service always answers, and `status` degrades when a configured
/// dependency is unhealthy.
pub async fn readiness(state: Data<AppState>) -> HttpResponse {
    let broker = match state.broker() {
        None => "unconfigured",
        Some(runtime) => {
            let report = runtime.link().report().await;
            if report.fresh { "connected" } else { "stale" }
        }
    };
    let audit = match state.audit() {
        None => "disabled",
        Some(runtime) => match runtime.trail().recent(1).await {
            Ok(_) => "ok",
            Err(error) => {
                tracing::warn!(%error, "readiness audit probe failed");
                "unavailable"
            }
        },
    };
    let overall = if broker == "stale" || audit == "unavailable" {
        "degraded"
    } else {
        "ready"
    };
    HttpResponse::Ok().json(ReadinessResponse {
        status: overall,
        trading_enabled: state.config().trading_enabled(),
        broker,
        audit,
    })
}

#[get("/status")]
/// Returns non-sensitive build and integration status.
pub async fn status(state: Data<AppState>) -> HttpResponse {
    let (broker_provider, broker_connected, ea_live_orders) = match state.broker() {
        Some(runtime) => {
            let report = runtime.link().report().await;
            let connected = report.fresh
                && report
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.connected());
            let armed = report
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.live_orders());
            (Some(runtime.provider().as_str()), connected, armed)
        }
        None => (None, false, false),
    };

    let market_provider = state.market().map(|runtime| runtime.provider().as_str());
    let model_provider = state.model().map(|runtime| runtime.provider().as_str());
    let jev_provider = state.jev().map(|runtime| runtime.provider().as_str());
    let persistence = state.audit().map(|runtime| runtime.provider().as_str());

    HttpResponse::Ok().json(StatusResponse {
        service: "veyra",
        version: env!("CARGO_PKG_VERSION"),
        environment: state.config().environment().to_string(),
        broker_provider,
        market_provider,
        model_provider,
        jev_provider,
        persistence,
        broker_connected,
        trading_enabled: state.config().trading_enabled(),
        ea_live_orders,
    })
}

#[post("/intents/evaluate")]
/// Evaluates one proposed intent against the deterministic risk gate.
///
/// The response is advisory and non-executing: nothing is queued, transmitted,
/// or executed, and no broker call is made while evaluating. Account facts come
/// from the latest link report; without a fresh report the gate rejects.
pub async fn evaluate_intent(
    state: Data<AppState>,
    draft: web::Json<TradeIntentDraft>,
) -> HttpResponse {
    let account = account_facts(state.broker()).await;
    let decision: RiskDecision =
        state
            .risk()
            .evaluate(&draft.into_inner(), account, SystemTime::now());
    HttpResponse::Ok().json(decision)
}

/// Assembles gate facts from a fresh link report. Any missing input fails
/// closed, so a stale heartbeat or a broken link cannot widen behavior.
pub(crate) async fn account_facts(broker: Option<&BrokerRuntime>) -> Option<AccountFacts> {
    let runtime = broker?;
    let report = runtime.link().report().await;
    if !report.fresh {
        return None;
    }
    let snapshot = report.snapshot?;
    if !snapshot.connected() {
        return None;
    }
    Some(AccountFacts {
        trade_allowed: snapshot.trade_allowed(),
        open_orders: snapshot.open_orders(),
        open_lots: snapshot.open_lots(),
    })
}
