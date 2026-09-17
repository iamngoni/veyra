//! Contract tests for the non-executing intent evaluation surface.
//!
//! They drive the same Actix applications the service mounts: the EA channel
//! records live state, and the diagnostics app evaluates a draft against the
//! deterministic gate. Nothing here queues or executes an order.

use std::sync::Arc;

use actix_web::http::StatusCode;
use actix_web::test;
use serde_json::{Value, json};

use veyra_service::AppState;
use veyra_service::app::create_app;
use veyra_service::broker::{BrokerRuntime, BrokerSettings, CommandKind, EaLink, create_ea_app};
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

fn gate(max_open_orders: u32, max_volume: &str) -> RiskGate {
    let policy = RiskPolicy::from_source(|name| match name {
        "VEYRA_RISK_SYMBOLS" => Some("eurusd".to_owned()),
        "VEYRA_RISK_MAX_VOLUME_PER_ORDER" => Some(max_volume.to_owned()),
        "VEYRA_RISK_MAX_OPEN_ORDERS" => Some(max_open_orders.to_string()),
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
        "orders": 0
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

async fn evaluate(state: &AppState, payload: Value) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::post()
        .uri("/intents/evaluate")
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

fn market_draft(symbol: &str) -> Value {
    json!({
        "symbol": symbol,
        "side": "buy",
        "order_type": "market",
        "volume": 0.01
    })
}

/// Feeds the channel one heartbeat and one completed `account_snapshot`, so the
/// link holds a fresh snapshot plus a real order count.
async fn prime_snapshot(link: &Arc<EaLink>) {
    let (status, _) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);

    let id = link.enqueue(CommandKind::AccountSnapshot);
    let (status, delivered) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["id"], id.to_string());

    let (status, _) = poll(
        link.clone(),
        json!({
            "t": "ack",
            "token": TOKEN,
            "id": id.to_string(),
            "ok": true,
            "data": {
                "balance": 20.57,
                "equity": 20.57,
                "freeMargin": 20.57,
                "orders": 0,
                "serverTime": 1_758_000_000
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[actix_web::test]
async fn fresh_state_with_room_approves_and_reports_order_limits() {
    let (runtime, link) = broker();
    prime_snapshot(&link).await;

    let state = AppState::new(test_config(), Some(runtime.clone()), None, gate(2, "0.5"));
    let (status, decision) = evaluate(&state, market_draft("EURUSD")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decision["decision"], "approved");
    assert_eq!(decision["intent"]["symbol"], "EURUSD");
    assert!(decision["intent"]["id"].as_str().is_some());

    // The same live account state rejects as soon as the configured cap is
    // reached; the gate consults real order counts, not an assumption.
    let capped = AppState::new(test_config(), Some(runtime), None, gate(0, "0.5"));
    let (status, decision) = evaluate(&capped, market_draft("EURUSD")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decision["decision"], "rejected");
    assert_eq!(decision["code"], "order_limit_reached");
    assert!(decision["detail"].as_str().is_some());
}

#[actix_web::test]
async fn missing_and_disallowed_state_fail_closed() {
    let (runtime, link) = broker();
    let state = AppState::new(test_config(), Some(runtime), None, gate(2, "0.5"));

    // No heartbeat yet: the link holds no state, so the gate rejects.
    let (status, decision) = evaluate(&state, market_draft("EURUSD")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decision["code"], "account_state_unavailable");

    // Fresh heartbeat, but the instrument is not on the allowlist.
    let (status, _) = poll(link, heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, decision) = evaluate(&state, market_draft("GBPUSD")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decision["code"], "symbol_not_allowed");
}

#[actix_web::test]
async fn malformed_drafts_are_rejected_at_the_boundary() {
    let (runtime, link) = broker();
    let (status, _) = poll(link, heartbeat()).await;
    assert_eq!(status, StatusCode::OK);

    let state = AppState::new(test_config(), Some(runtime), None, gate(2, "0.5"));

    let (status, _) = evaluate(
        &state,
        json!({"symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": -1.0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut unknown_field = market_draft("EURUSD");
    unknown_field["leverage"] = json!(100);
    let (status, _) = evaluate(&state, unknown_field).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
