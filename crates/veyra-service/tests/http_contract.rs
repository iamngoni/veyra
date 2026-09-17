//! Integration tests for the externally visible HTTP contract.
//!
//! They build the same Actix app used by the production binary and assert the
//! safe current state so future execution work cannot silently change it.

use std::sync::Arc;

use actix_web::test;
use veyra_service::AppState;
use veyra_service::app::create_app;
use veyra_service::audit::{AuditRuntime, MemoryTrail};
use veyra_service::broker::{
    AccountLogin, AccountSnapshot, BrokerRuntime, BrokerSettings, ServerName, Symbol,
};
use veyra_service::config::{ConfigError, ServiceConfig};
use veyra_service::jev::{JevRuntime, JevSettings};
use veyra_service::risk::{RiskGate, RiskPolicy};

fn test_config() -> ServiceConfig {
    ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("test configuration must parse")
}

fn test_jev() -> JevRuntime {
    let settings = JevSettings::from_source(|name| match name {
        "VEYRA_JEV_API_KEY" => Ok("apikey_test_1234567890".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("jev settings must parse")
    .expect("jev must be configured");
    JevRuntime::from_settings(settings).expect("jev runtime must build")
}

fn test_gate() -> RiskGate {
    // The restrictive default approves nothing unless a test configures it.
    RiskGate::new(RiskPolicy::default())
}

fn test_state(broker: Option<BrokerRuntime>) -> AppState {
    AppState::new(test_config(), broker, None, test_gate())
}

fn ea_broker() -> BrokerRuntime {
    let settings = BrokerSettings::from_source(|name| match name {
        "VEYRA_BROKER_PROVIDER" => Ok("ea".to_owned()),
        "VEYRA_EA_TOKEN" => Ok("test-token-1234567890".to_owned()),
        "VEYRA_EA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_EA_BIND_PORT" => Ok("7801".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("broker settings must parse")
    .expect("broker must be configured");

    BrokerRuntime::from_settings(settings).expect("runtime must build")
}

fn snapshot() -> AccountSnapshot {
    AccountSnapshot::new(
        AccountLogin::parse(94168).expect("login must validate"),
        ServerName::parse("IFCMarkets-Real").expect("server must validate"),
        Symbol::parse("EURUSD").expect("symbol must validate"),
        true,
        true,
        0,
        0.0,
    )
}

#[actix_web::test]
async fn health_reports_process_liveness() {
    let app = test::init_service(create_app(test_state(None))).await;
    let request = test::TestRequest::get().uri("/health").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "veyra");
}

#[actix_web::test]
async fn readiness_reports_trading_disabled() {
    let app = test::init_service(create_app(test_state(None))).await;
    let request = test::TestRequest::get().uri("/ready").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["trading_enabled"], false);
}

#[actix_web::test]
async fn status_reports_no_broker_connection() {
    let app = test::init_service(create_app(test_state(None))).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["broker_provider"], serde_json::Value::Null);
    assert_eq!(body["model_provider"], serde_json::Value::Null);
    assert_eq!(body["broker_connected"], false);
    assert_eq!(body["trading_enabled"], false);
    assert_eq!(body["environment"], "development");
}

#[actix_web::test]
async fn status_reports_broker_link_state() {
    let runtime = ea_broker();
    runtime
        .ea_link()
        .expect("EA link must exist")
        .record(snapshot());

    let app = test::init_service(create_app(test_state(Some(runtime)))).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["broker_provider"], "ea");
    assert_eq!(body["broker_connected"], true);
    assert_eq!(body["trading_enabled"], false);
}

#[actix_web::test]
async fn status_reports_model_provider() {
    let settings = veyra_service::model::ModelSettings::from_source(|name| match name {
        "VEYRA_MODEL_API_KEY" => Ok("test-key-12345678".to_owned()),
        "VEYRA_MODEL_FAST" => Ok("vendor/fast".to_owned()),
        "VEYRA_MODEL_BALANCED" => Ok("vendor/balanced".to_owned()),
        "VEYRA_MODEL_REASONING" => Ok("vendor/reasoning".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("model settings must parse")
    .expect("model must be configured");
    let model =
        veyra_service::model::ModelRuntime::from_settings(settings).expect("model runtime builds");
    assert_eq!(model.engine().provider().as_str(), "openrouter");

    let app = test::init_service(create_app(AppState::new(
        test_config(),
        None,
        Some(model),
        test_gate(),
    )))
    .await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["model_provider"], "openrouter");
    assert_eq!(body["broker_provider"], serde_json::Value::Null);
}

#[actix_web::test]
async fn status_reports_the_configured_jev_provider() {
    let configured =
        test::init_service(create_app(test_state(None).with_jev(Some(test_jev())))).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&configured, request).await;
    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["jev_provider"], "typesafe");

    let unconfigured = test::init_service(create_app(test_state(None))).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&unconfigured, request).await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert!(body["jev_provider"].is_null());
}

#[actix_web::test]
async fn status_reports_the_configured_persistence_provider() {
    let audit = AuditRuntime::new(Arc::new(MemoryTrail::default()));
    let configured = test::init_service(create_app(test_state(None).with_audit(Some(audit)))).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&configured, request).await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["persistence"], "postgres");

    let unconfigured = test::init_service(create_app(test_state(None))).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&unconfigured, request).await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert!(body["persistence"].is_null());
}
