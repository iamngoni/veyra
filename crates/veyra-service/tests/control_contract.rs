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
use veyra_service::audit::{AuditKind, AuditRuntime, MemoryTrail};
use veyra_service::broker::{
    BrokerRuntime, BrokerSettings, CommandKind, EaLink, ORDER_MAGIC, create_ea_app,
};
use veyra_service::config::{ConfigError, ServiceConfig};
use veyra_service::risk::{RiskGate, RiskPolicy};

const TOKEN: &str = "test-token-1234567890";

/// Wednesday 2026-01-07 12:00 UTC: midweek, mid-session, no window guard.
fn test_now() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_767_787_200)
}

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

fn config_with_trading(enabled: bool) -> ServiceConfig {
    ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        "VEYRA_TRADING_ENABLED" => Ok(enabled.to_string()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("test configuration must parse")
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

async fn execute(state: &AppState, payload: Value) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::post()
        .uri("/intents/execute")
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

async fn reconciliation(state: &AppState) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::get().uri("/reconciliation").to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status();
    let bytes = test::read_body(response).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn modify(state: &AppState, payload: Value) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::post()
        .uri("/intents/modify")
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

async fn close(state: &AppState, ticket: i64) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::post()
        .uri("/intents/close")
        .set_json(json!({ "ticket": ticket }))
        .to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status();
    let bytes = test::read_body(response).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Retains one completed `account_snapshot` carrying a single position with the
/// given magic number, so close validation has state to work with.
async fn retain_position(link: &Arc<EaLink>, magic: u32) {
    let id = link.enqueue(CommandKind::AccountSnapshot);
    let (status, delivered) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["kind"], "account_snapshot");

    let (status, _) = poll(
        link.clone(),
        json!({
            "t": "ack",
            "v": 1,
            "token": TOKEN,
            "id": id.to_string(),
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
                    "magic": magic,
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

    let state =
        AppState::new(test_config(), Some(runtime), None, gate()).with_fixed_now(Some(test_now()));
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

    let state =
        AppState::new(test_config(), Some(runtime), None, gate()).with_fixed_now(Some(test_now()));
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

    let state =
        AppState::new(test_config(), Some(runtime), None, gate()).with_fixed_now(Some(test_now()));
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
                    "magic": 77041,
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

    let state =
        AppState::new(test_config(), Some(runtime), None, gate()).with_fixed_now(Some(test_now()));
    let (status, _) = command_status(&state, "not-a-uuid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = command_status(&state, "00000000-0000-4000-8000-000000000000").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_command");
}

#[actix_web::test]
async fn control_routes_require_a_command_channel() {
    let state = AppState::new(test_config(), None, None, gate())
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));
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

#[actix_web::test]
async fn execution_is_refused_until_trading_is_enabled() {
    let (runtime, link) = broker();
    prime(&link).await;

    let state =
        AppState::new(test_config(), Some(runtime), None, gate()).with_fixed_now(Some(test_now()));
    let (status, body) = execute(
        &state,
        json!({"symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": 0.01}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "trading_disabled");

    // Nothing was queued for the terminal.
    let (_, reply) = poll(link.clone(), heartbeat()).await;
    assert_eq!(reply["t"], "none");
}

#[actix_web::test]
async fn enabled_execution_queues_an_order_and_reports_the_dry_run() {
    let (runtime, link) = broker();
    prime(&link).await;
    let state = AppState::new(config_with_trading(true), Some(runtime), None, gate())
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));

    let (status, body) = execute(
        &state,
        json!({"symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": 0.01}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["decision"], "approved");
    assert_eq!(body["command"], "open_order");
    let command_id = body["command_id"]
        .as_str()
        .expect("execution carries a command id")
        .to_owned();

    let (status, delivered) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["kind"], "open_order");
    assert_eq!(delivered["order"]["symbol"], "EURUSD");
    assert_eq!(delivered["order"]["magic"], 77_041);

    // The terminal validates and reports a dry run while its own live-orders
    // input is disabled; nothing reaches the broker.
    let (status, _) = poll(
        link.clone(),
        json!({
            "t": "ack",
            "v": 1,
            "token": TOKEN,
            "id": command_id,
            "ok": true,
            "data": {
                "executed": false,
                "retcode": 0,
                "comment": "dry run (live orders disabled in EA)",
                "ticket": 0,
                "price": 0.0
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = command_status(&state, &command_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "open_order");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"]["executed"], false);
    assert_eq!(body["result"]["retcode"], 0);
}

#[actix_web::test]
async fn closing_requires_enablement_state_and_ownership() {
    let (runtime, link) = broker();
    prime(&link).await;

    // Disabled: refused before any state is consulted.
    let state = AppState::new(test_config(), Some(runtime.clone()), None, gate())
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));
    let (status, body) = close(&state, 123).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "trading_disabled");

    // Enabled, but no retained snapshot: fail closed.
    let enabled = AppState::new(
        config_with_trading(true),
        Some(runtime.clone()),
        None,
        gate(),
    )
    .with_fixed_now(Some(test_now()));
    let (status, body) = close(&enabled, 123).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "position_state_unavailable");
    let (status, body) = close(&enabled, 0).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_ticket");

    // A Veyra-owned position can be closed; foreign tickets cannot.
    retain_position(&link, ORDER_MAGIC).await;
    let (status, body) = close(&enabled, 999).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_position");

    let (status, body) = close(&enabled, 123).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["command"], "close_order");
    let command_id = body["command_id"]
        .as_str()
        .expect("close carries a command id")
        .to_owned();

    let (status, delivered) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["kind"], "close_order");
    assert_eq!(delivered["close"]["ticket"], 123);
    assert_eq!(delivered["close"]["magic"], ORDER_MAGIC);

    // The terminal reports a dry run while its live-orders input is disabled.
    let (status, _) = poll(
        link.clone(),
        json!({
            "t": "ack",
            "v": 1,
            "token": TOKEN,
            "id": command_id,
            "ok": true,
            "data": {
                "executed": false,
                "retcode": 0,
                "comment": "dry run (live orders disabled in EA)",
                "ticket": 0,
                "price": 0.0
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = command_status(&enabled, &command_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "close_order");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"]["executed"], false);
}

#[actix_web::test]
async fn closing_refuses_positions_veyra_does_not_own() {
    let (runtime, link) = broker();
    prime(&link).await;
    retain_position(&link, 0).await;

    let state = AppState::new(config_with_trading(true), Some(runtime), None, gate())
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));
    let (status, body) = close(&state, 123).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "not_a_veyra_position");

    // Nothing was queued for the terminal.
    let (_, reply) = poll(link.clone(), heartbeat()).await;
    assert_eq!(reply["t"], "none");
}

#[actix_web::test]
async fn modifying_requires_enablement_and_valid_stops() {
    let (runtime, link) = broker();
    prime(&link).await;

    // Disabled: refused before any state is consulted.
    let state = AppState::new(test_config(), Some(runtime.clone()), None, gate())
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));
    let (status, body) = modify(&state, json!({"ticket": 123, "stop_loss": 1.05})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "trading_disabled");

    let enabled = AppState::new(
        config_with_trading(true),
        Some(runtime.clone()),
        None,
        gate(),
    )
    .with_fixed_now(Some(test_now()));
    let (status, body) = modify(&enabled, json!({"ticket": 0, "stop_loss": 1.05})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_ticket");

    // No stops, or unusable stops, are rejected before any state is read.
    let (status, body) = modify(&enabled, json!({"ticket": 123})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_stops");
    let (status, body) = modify(&enabled, json!({"ticket": 123, "take_profit": 0.0})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_stops");

    // Enabled and well-formed, but no retained snapshot: fail closed.
    let (status, body) = modify(&enabled, json!({"ticket": 123, "stop_loss": 1.05})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "position_state_unavailable");
}

#[actix_web::test]
async fn modifying_targets_only_veyra_positions() {
    let (runtime, link) = broker();
    prime(&link).await;
    retain_position(&link, 0).await;

    let state = AppState::new(
        config_with_trading(true),
        Some(runtime.clone()),
        None,
        gate(),
    )
    .with_fixed_now(Some(test_now()))
    .with_fixed_now(Some(test_now()));
    let (status, body) = modify(&state, json!({"ticket": 123, "stop_loss": 1.05})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "not_a_veyra_position");
    let (_, reply) = poll(link.clone(), heartbeat()).await;
    assert_eq!(reply["t"], "none");

    // A Veyra-owned position can have its stops changed.
    retain_position(&link, ORDER_MAGIC).await;
    let (status, body) = modify(&state, json!({"ticket": 123, "stop_loss": 1.05})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["command"], "modify_order");
    let command_id = body["command_id"]
        .as_str()
        .expect("modify carries a command id")
        .to_owned();

    let (status, delivered) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["kind"], "modify_order");
    assert_eq!(delivered["modify"]["ticket"], 123);
    assert_eq!(delivered["modify"]["magic"], ORDER_MAGIC);
    assert_eq!(delivered["modify"]["stop_loss"], 1.05);
    assert!(delivered["modify"].get("take_profit").is_none());

    let (status, _) = poll(
        link.clone(),
        json!({
            "t": "ack",
            "v": 1,
            "token": TOKEN,
            "id": command_id,
            "ok": true,
            "data": {
                "executed": false,
                "retcode": 0,
                "comment": "dry run (live orders disabled in EA)",
                "ticket": 0,
                "price": 0.0
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = command_status(&state, &command_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "modify_order");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["result"]["executed"], false);
}

#[actix_web::test]
async fn reconciliation_walks_from_unavailable_to_drift() {
    // No command channel at all.
    let bare = AppState::new(test_config(), None, None, gate()).with_fixed_now(Some(test_now()));
    let (status, body) = reconciliation(&bare).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "unavailable");

    // A channel that has never heard from the terminal is stale.
    let (runtime, link) = broker();
    let state = AppState::new(test_config(), Some(runtime.clone()), None, gate())
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));
    let (status, body) = reconciliation(&state).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "stale");

    // A fresh heartbeat without a completed snapshot has nothing to assess.
    let (status, _) = poll(link.clone(), heartbeat()).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = reconciliation(&state).await;
    assert_eq!(body["status"], "no_snapshot");

    // One Veyra-managed position reconciles cleanly.
    retain_position(&link, ORDER_MAGIC).await;
    let (_, body) = reconciliation(&state).await;
    assert_eq!(body["status"], "reconciled");
    assert_eq!(body["orders"], 1);
    assert_eq!(body["positions"][0]["managed"], true);
    assert_eq!(body["positions"][0]["ticket"], 123);
    assert_eq!(body["unknownTickets"], json!([]));
    assert!(body["accountAgeSecs"].as_u64().is_some());

    // A foreign position is drift, named by ticket.
    retain_position(&link, 0).await;
    let (_, body) = reconciliation(&state).await;
    assert_eq!(body["status"], "drift");
    assert_eq!(body["unknownTickets"], json!([123]));
    assert_eq!(body["positions"][0]["managed"], false);
}

#[actix_web::test]
async fn queued_commands_and_the_audit_route_share_one_trail() {
    let trail = Arc::new(MemoryTrail::default());
    let (runtime, link) = broker();
    prime(&link).await;
    let state = AppState::new(test_config(), Some(runtime), None, gate())
        .with_audit(Some(AuditRuntime::new(trail.clone())))
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));

    let (status, body) = check(
        &state,
        json!({"symbol": "EURUSD", "side": "buy", "order_type": "market", "volume": 0.01}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let command_id = body["command_id"].as_str().expect("command id").to_owned();

    let events = trail.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), AuditKind::CommandQueued);
    assert_eq!(events[0].payload()["command_id"], command_id.as_str());
    assert_eq!(events[0].payload()["kind"], "order_check");

    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::get().uri("/audit?limit=5").to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["provider"], "postgres");
    assert_eq!(body["events"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["events"][0]["kind"], "command_queued");
    assert_eq!(
        body["events"][0]["payload"]["command_id"],
        command_id.as_str()
    );
}

#[actix_web::test]
async fn the_audit_route_reports_disabled_without_a_trail() {
    let state = AppState::new(test_config(), None, None, gate())
        .with_fixed_now(Some(test_now()))
        .with_fixed_now(Some(test_now()));
    let app = test::init_service(create_app(state)).await;
    let request = test::TestRequest::get().uri("/audit").to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "disabled");
    assert_eq!(body["events"].as_array().map(Vec::len), Some(0));
}
