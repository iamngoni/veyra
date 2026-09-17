//! Contract tests for the loopback command control surface.
//!
//! They drive the real EA channel application plus the diagnostics app: an
//! approved draft becomes an `order_check` command delivered to the terminal
//! poll, and its acknowledgement becomes a readable command result. Nothing
//! here places an order; the terminal only validates the request.

use std::sync::Arc;

use actix_web::http::StatusCode;
use actix_web::test;
use serde_json::{Value, json};

use veyra_service::AppState;
use veyra_service::app::create_app;
use veyra_service::broker::{BrokerRuntime, BrokerSettings, EaLink, create_ea_app};
use veyra_service::config::{ConfigError, ServiceConfig};
use veyra_service::risk::{RiskGate, RiskPolicy};

const TOKEN: &str = "test-token-1234567890";

fn test_config() -> ServiceConfig {
    ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("test configuration must parse")
}

fn broker() -> (BrokerRuntime, Arc<EaLink>) {
    let settings = BrokerSettings::from_source(|name| match name {
        "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
        "VEYRA_EA_TOKEN" => Ok(TOKEN.to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("broker settings must parse")
    .expect("broker must be configured");
    let runtime = BrokerRuntime::from_settings(settings).expect("runtime must build");
    let link = runtime.ea_link().expect("ea link must exist");
    (runtime, link)
}

fn gate() -> RiskGate {
    let policy = RiskPolicy::from_source(|name| match name {
        "VEYRA_RISK_SYMBOLS" => Some("EURUSD".to_owned()),
        "VEYRA_RISK_MAX_VOLUME_PER_ORDER" => Some("0.5".to_owned()),
        "VEYRA_RISK_MAX_OPEN_ORDERS" => Some("2".to_owned()),
        _ => None,
    })
    .expect("risk policy must parse");
    RiskGate::new(policy)
}

fn heartbeat() -> Value {
    json!({
        "t": "hb",
        "v": 1,
        "token": TOKEN,
        "acct": 94168,
        "server": "IFCMarkets-Real",
        "symbol": "EURUSD",
        "connected": true,
        "tradeAllowed": true,
        "orders": 0,
        "lots": 0.0
    })
}

async fn poll(link: Arc<EaLink>, payload: Value) -> (StatusCode, Value) {
    let app = test::init_service(create_ea_app(link)).await;
    let request = test::TestRequest::post()
        .uri("/ea/poll")
        .set_json(&payload)
        .to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status();
    let bytes = test::read_body(response).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Completes the pong handshake so later polls report "none" instead of ping.
async fn prime(link: &Arc<EaLink>) {
    let (status, reply) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reply["t"], "ping");

    let (status, _) = poll(link.clone(), json!({"t": "pong", "v": 1, "token": TOKEN})).await;
    assert_eq!(status, StatusCode::OK);

    let (_, reply) = poll(link.clone(), heartbeat()).await;
    assert_eq!(reply["t"], "none");
}

async fn check(state: &AppState, payload: Value) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::post()
        .uri("/intents/check")
        .set_json(&payload)
        .to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status();
    let bytes = test::read_body(response).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn command_status(state: &AppState, id: &str) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::get()
        .uri(&format!("/commands/{id}"))
        .to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status();
    let bytes = test::read_body(response).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[actix_web::test]
async fn approved_drafts_become_order_checks_and_report_their_result() {
    let (runtime, link) = broker();
    prime(&link).await;

    let state = AppState::new(test_config(), Some(runtime), None, gate());
    let (status, decision) = check(
        &state,
        json!({"symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": 0.01}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decision["decision"], "approved");
    assert_eq!(decision["command"], "order_check");
    let command_id = decision["command_id"]
        .as_str()
        .expect("approved checks carry a command id")
        .to_owned();

    // The terminal receives the request on its next poll.
    let (status, delivered) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["t"], "cmd");
    assert_eq!(delivered["kind"], "order_check");
    assert_eq!(delivered["id"], command_id);
    assert_eq!(delivered["order"]["symbol"], "EURUSD");
    assert_eq!(delivered["order"]["side"], "buy");
    assert_eq!(delivered["order"]["volume"], 0.01);

    let (status, pending) = command_status(&state, &command_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pending["status"], "pending");

    let (status, _) = poll(
        link.clone(),
        json!({
            "t": "ack",
            "v": 1,
            "token": TOKEN,
            "id": command_id,
            "ok": true,
            "data": {"passed": true, "retcode": 0, "comment": "Done", "margin": 2.19}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, completed) = command_status(&state, &command_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(completed["kind"], "order_check");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["result"]["passed"], true);
    assert_eq!(completed["result"]["retcode"], 0);
    assert_eq!(completed["result"]["margin"], 2.19);
}

#[actix_web::test]
async fn rejected_drafts_never_reach_the_terminal() {
    let (runtime, link) = broker();
    prime(&link).await;

    let state = AppState::new(test_config(), Some(runtime), None, gate());
    let (status, decision) = check(
        &state,
        json!({"symbol": "GBPUSD", "side": "buy", "order_type": "market", "volume": 0.01}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decision["decision"], "rejected");
    assert_eq!(decision["code"], "symbol_not_allowed");

    let (_, reply) = poll(link.clone(), heartbeat()).await;
    assert_eq!(reply["t"], "none", "no command may be queued");
}

#[actix_web::test]
async fn account_snapshot_requests_report_exposure() {
    let (runtime, link) = broker();
    prime(&link).await;

    let state = AppState::new(test_config(), Some(runtime), None, gate());
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::post()
        .uri("/commands/account_snapshot")
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = test::read_body_json(response).await;
    let command_id = body["command_id"]
        .as_str()
        .expect("snapshot requests carry a command id")
        .to_owned();

    let (status, delivered) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["kind"], "account_snapshot");

    let (status, _) = poll(
        link.clone(),
        json!({
            "t": "ack",
            "v": 1,
            "token": TOKEN,
            "id": command_id,
            "ok": true,
            "data": {
                "balance": 20.57,
                "equity": 20.57,
                "freeMargin": 20.57,
                "orders": 1,
                "lots": 0.01,
                "positions": [{
                    "ticket": 123,
                    "symbol": "EURUSD",
                    "kind": "buy",
                    "lots": 0.01,
                    "price": 1.095,
                    "profit": -0.25
                }],
                "positionsTruncated": false,
                "serverTime": 1_758_000_000
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = command_status(&state, &command_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "account_snapshot");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"]["orders"], 1);
    assert_eq!(body["result"]["lots"], 0.01);
    assert_eq!(body["result"]["positions"][0]["kind"], "buy");
    assert_eq!(body["result"]["positions"][0]["ticket"], 123);
    assert_eq!(body["result"]["positionsTruncated"], false);
}

#[actix_web::test]
async fn command_lookup_validates_its_inputs() {
    let (runtime, link) = broker();
    prime(&link).await;

    let state = AppState::new(test_config(), Some(runtime), None, gate());
    let (status, _) = command_status(&state, "not-a-uuid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = command_status(&state, "00000000-0000-4000-8000-000000000000").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_command");
}

#[actix_web::test]
async fn control_routes_require_a_command_channel() {
    let state = AppState::new(test_config(), None, None, gate());
    let (status, body) = check(
        &state,
        json!({"symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": 0.01}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "command_channel_unavailable");

    let (status, _) = command_status(&state, "00000000-0000-4000-8000-000000000000").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
