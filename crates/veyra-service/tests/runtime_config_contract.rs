//! Contract tests for the live settings surface.
//!
//! The point of `POST /config` is not that it stores a value but that the
//! running service changes behaviour because of it, so these drive the real
//! app and then assert against the state the trading loop actually reads.

use actix_web::http::StatusCode;
use actix_web::test;
use serde_json::{Value, json};

use veyra_service::AppState;
use veyra_service::app::create_app;
use veyra_service::config::{ConfigError, ServiceConfig};
use veyra_service::risk::{RiskGate, RiskPolicy};

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

fn gate() -> RiskGate {
    let policy = RiskPolicy::from_source(|name| match name {
        "VEYRA_RISK_SYMBOLS" => Some("EURUSD".to_owned()),
        _ => None,
    })
    .expect("risk policy must parse");
    RiskGate::new(policy)
}

fn state(trading: bool) -> AppState {
    AppState::new(config_with_trading(trading), None, None, gate())
}

async fn patch(state: &AppState, body: Value) -> (StatusCode, Value) {
    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::post()
        .uri("/config")
        .set_json(&body)
        .to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status();
    (status, test::read_body_json(response).await)
}

#[actix_web::test]
async fn an_accepted_edit_changes_what_the_trading_loop_reads() {
    let state = state(false);
    assert!(!state.trading_enabled(), "the baseline is disarmed");

    let (status, body) = patch(&state, json!({"VEYRA_TRADING_ENABLED": true})).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        state.trading_enabled(),
        "the switch the execution path reads must have moved, not just the stored value"
    );
}

// The thread that started this: harvesting was opt-in but only at boot, so a
// misbehaving ratchet could not be stopped without restarting with positions
// open.
#[actix_web::test]
async fn profit_harvesting_can_be_switched_off_without_a_restart() {
    let state = state(false);

    let (status, body) = patch(
        &state,
        json!({
            "VEYRA_AUTOPILOT_ENABLED": true,
            "VEYRA_AUTOPILOT_PROFIT_HARVEST": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        state
            .autopilot()
            .expect("autopilot configured")
            .profit_harvest()
            .is_some(),
        "harvesting must arm from the control surface"
    );

    let (status, body) = patch(&state, json!({"VEYRA_AUTOPILOT_PROFIT_HARVEST": false})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        state
            .autopilot()
            .expect("autopilot configured")
            .profit_harvest()
            .is_none(),
        "and disarm again while positions are open"
    );
}

#[actix_web::test]
async fn credentials_are_refused_by_the_control_surface() {
    let state = state(false);

    let (status, body) = patch(&state, json!({"VEYRA_MODEL_API_KEY": "sk-or-stolen"})).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_settings");
    assert_eq!(body["rejected"][0]["field"], "VEYRA_MODEL_API_KEY");
}

#[actix_web::test]
async fn boot_only_infrastructure_is_refused_rather_than_silently_stored() {
    let state = state(false);

    let (status, body) = patch(&state, json!({"VEYRA_BIND_PORT": "9999"})).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["rejected"][0]["field"], "VEYRA_BIND_PORT");
}

// The same rules that validate `.env` validate a console edit, so the control
// surface cannot widen behaviour beyond what a restart would accept.
#[actix_web::test]
async fn a_value_the_env_parser_would_reject_is_refused_here_too() {
    let state = state(false);

    let (status, body) = patch(&state, json!({"VEYRA_AUTOPILOT_INTERVAL_SECS": "5"})).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["rejected"][0]["field"], "VEYRA_AUTOPILOT_INTERVAL_SECS",
        "the section's own parser supplies the refusal: {body}"
    );
}

#[actix_web::test]
async fn one_bad_value_abandons_the_whole_patch() {
    let state = state(false);

    let (status, _) = patch(
        &state,
        json!({
            "VEYRA_TRADING_ENABLED": true,
            "VEYRA_AUTOPILOT_INTERVAL_SECS": "5",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !state.trading_enabled(),
        "a patch that fails validation must change nothing, not its valid half"
    );
}

#[actix_web::test]
async fn the_surface_reports_which_values_have_left_the_baseline() {
    let state = state(false);
    patch(&state, json!({"VEYRA_AUTOPILOT_TRAIL_R": "1.5"})).await;

    let app = test::init_service(create_app(state.clone())).await;
    let request = test::TestRequest::get().uri("/config").to_request();
    let body: Value = test::call_and_read_body_json(&app, request).await;

    assert_eq!(body["settings"]["VEYRA_AUTOPILOT_TRAIL_R"]["value"], "1.5");
    assert_eq!(
        body["settings"]["VEYRA_AUTOPILOT_TRAIL_R"]["overridden"],
        true
    );
    assert_eq!(
        body["settings"]["VEYRA_AUTOPILOT_BREAKEVEN_R"]["overridden"], false,
        "an untouched setting must not look like an operator decision"
    );
    assert!(
        body["settings"]["VEYRA_MODEL_API_KEY"].is_null(),
        "a credential is not part of the live surface at all"
    );
}

#[actix_web::test]
async fn an_empty_patch_is_refused() {
    let state = state(false);
    let (status, body) = patch(&state, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "empty_patch");
}
