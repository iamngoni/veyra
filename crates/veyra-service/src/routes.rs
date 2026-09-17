//! The currently supported HTTP contract.
//!
//! All routes are read-only diagnostics. They never expose credentials, account
//! balances, model prompts, or broker state because no such subsystem exists
//! yet. Adding an executable route requires an explicit design change.

use actix_web::HttpResponse;
use actix_web::get;
use actix_web::web::Data;
use serde::Serialize;

use crate::AppState;

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
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    service: &'static str,
    version: &'static str,
    environment: String,
    broker_provider: Option<&'static str>,
    broker_connected: bool,
    trading_enabled: bool,
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
/// Reports diagnostic-control-plane readiness only, not trading readiness.
pub async fn readiness(state: Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(ReadinessResponse {
        status: "ready",
        trading_enabled: state.config().trading_enabled(),
    })
}

#[get("/status")]
/// Returns non-sensitive build and integration status.
pub async fn status(state: Data<AppState>) -> HttpResponse {
    let (broker_provider, broker_connected) = match state.broker() {
        Some(runtime) => {
            let report = runtime.link().report().await;
            let connected = report.fresh
                && report
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.connected());
            (Some(runtime.provider().as_str()), connected)
        }
        None => (None, false),
    };

    HttpResponse::Ok().json(StatusResponse {
        service: "veyra",
        version: env!("CARGO_PKG_VERSION"),
        environment: state.config().environment().to_string(),
        broker_provider,
        broker_connected,
        trading_enabled: state.config().trading_enabled(),
    })
}
