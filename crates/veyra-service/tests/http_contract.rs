//! Integration tests for the externally visible HTTP contract.
//!
//! They build the same Actix app used by the production binary and assert the
//! safe current state so future execution work cannot silently change it.

use actix_web::test;
use veyra_service::AppState;
use veyra_service::app::create_app;
use veyra_service::config::ConfigError;

fn test_state() -> AppState {
    let config = veyra_service::config::ServiceConfig::from_source(|name| match name {
        "VEYRA_BIND_HOST" => Ok("127.0.0.1".to_owned()),
        "VEYRA_BIND_PORT" => Ok("8080".to_owned()),
        "VEYRA_ENV" => Ok("development".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .unwrap();

    AppState::new(config)
}

#[actix_web::test]
async fn health_reports_process_liveness() {
    let app = test::init_service(create_app(test_state())).await;
    let request = test::TestRequest::get().uri("/health").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "veyra");
}

#[actix_web::test]
async fn readiness_reports_trading_disabled() {
    let app = test::init_service(create_app(test_state())).await;
    let request = test::TestRequest::get().uri("/ready").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["trading_enabled"], false);
}

#[actix_web::test]
async fn status_reports_no_broker_connection() {
    let app = test::init_service(create_app(test_state())).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["broker_connected"], false);
    assert_eq!(body["trading_enabled"], false);
    assert_eq!(body["environment"], "development");
}
