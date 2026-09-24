//! Integration tests for the externally visible HTTP contract.
//!
//! They build the same Actix app used by the production binary and assert the
//! safe current state so future execution work cannot silently change it.

use std::sync::Arc;

use actix_web::test;
use veyra_service::AppState;
use veyra_service::app::create_app;
use veyra_service::audit::{
    AuditError, AuditEvent, AuditProvider, AuditRow, AuditRuntime, AuditTrail, MemoryTrail,
};
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
        AccountLogin::parse(123456).expect("login must validate"),
        ServerName::parse("Broker-Test").expect("server must validate"),
        Symbol::parse("EURUSD").expect("symbol must validate"),
        true,
        true,
        0,
        0.0,
    )
    .with_live_orders(true)
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
    assert_eq!(body["market_provider"], serde_json::Value::Null);
    assert_eq!(body["model_provider"], serde_json::Value::Null);
    assert_eq!(body["model_budget"], serde_json::Value::Null);
    assert_eq!(body["autopilot"], serde_json::Value::Null);
    assert_eq!(body["broker_connected"], false);
    // The effective policy is always reported, even with no integrations.
    assert_eq!(body["risk_policy"]["symbols"], serde_json::json!([]));
    assert_eq!(body["risk_policy"]["maxOpenOrders"], 1);
    assert_eq!(body["risk_policy"]["killSwitch"], false);
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
    assert_eq!(
        body["ea_live_orders"], true,
        "the armed EA state is reported even while the service switch is off"
    );
}

#[actix_web::test]
async fn balance_history_only_returns_real_points_for_the_active_account() {
    let broker = ea_broker();
    let link = broker.ea_link().expect("EA link");
    link.record(snapshot());
    let trail = Arc::new(MemoryTrail::default());
    let runtime = AuditRuntime::new(trail);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    for (login, at_ms, balance) in [
        (123456, now_ms - 2_000, 20.0),
        (123457, now_ms - 1_500, 500.0),
        (123456, now_ms - 1_000, 20.5),
    ] {
        runtime
            .try_record(AuditEvent::new(
                veyra_service::audit::AuditKind::BalanceObserved,
                serde_json::json!({
                    "login": login,
                    "server": "Broker-Test",
                    "atMs": at_ms,
                    "balance": balance
                }),
            ))
            .await;
    }
    let app = test::init_service(create_app(
        test_state(Some(broker)).with_audit(Some(runtime)),
    ))
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/account/balance-history?days=30")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["source"], "broker_balance");
    assert!(body["currency"].is_null());
    assert_eq!(body["account"]["login"], 123456);
    assert_eq!(body["points"].as_array().expect("points").len(), 2);
    assert_eq!(body["points"][0]["balance"], 20.0);
    assert_eq!(body["points"][1]["balance"], 20.5);
    assert_eq!(body["firstObservedAtMs"], now_ms - 2_000);
    assert_eq!(body["lastObservedAtMs"], now_ms - 1_000);
    assert_eq!(body["sampled"], false);
    assert_eq!(body["fresh"], true);
    assert!(
        link.recent_commands(10).is_empty(),
        "read must not queue EA commands"
    );
}

#[actix_web::test]
async fn balance_history_handles_disabled_waiting_invalid_and_failed_reads() {
    let app = test::init_service(create_app(test_state(Some(ea_broker())))).await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/account/balance-history")
            .to_request(),
    )
    .await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "disabled");
    assert!(body["points"].as_array().expect("points").is_empty());

    let app = test::init_service(create_app(
        test_state(Some(ea_broker()))
            .with_audit(Some(AuditRuntime::new(Arc::new(MemoryTrail::default())))),
    ))
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/account/balance-history")
            .to_request(),
    )
    .await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "waiting_for_account");
    assert!(body["account"].is_null());

    for days in ["0", "366", "bad"] {
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/account/balance-history?days={days}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 400);
    }

    let broker = ea_broker();
    broker.ea_link().expect("EA link").record(snapshot());
    let app = test::init_service(create_app(
        test_state(Some(broker)).with_audit(Some(AuditRuntime::new(Arc::new(BrokenTrail)))),
    ))
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/account/balance-history")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), 503);
}

#[actix_web::test]
async fn status_reports_autopilot_configuration() {
    let settings = veyra_service::trading::AutopilotSettings::from_source(|name| match name {
        "VEYRA_AUTOPILOT_ENABLED" => Ok("true".to_owned()),
        "VEYRA_AUTOPILOT_SYMBOL" => Ok("EURUSD".to_owned()),
        "VEYRA_AUTOPILOT_TIER" => Ok("reasoning".to_owned()),
        "VEYRA_AUTOPILOT_PROFIT_HARVEST" => Ok("true".to_owned()),
        _ => Err(ConfigError::MissingEnvironmentVariable { name }),
    })
    .expect("settings parse")
    .expect("configured");
    let state = test_state(None).with_autopilot(Some(settings));
    let app = test::init_service(create_app(state)).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let response = test::call_service(&app, request).await;

    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["autopilot"]["enabled"], true);
    assert_eq!(body["autopilot"]["symbol"], "EURUSD");
    assert_eq!(body["autopilot"]["tier"], "reasoning");
    assert_eq!(body["autopilot"]["timeframe"], "H4");
    assert_eq!(body["autopilot"]["bars"], 48);
    assert_eq!(body["autopilot"]["interval_secs"], 300);
    assert_eq!(body["autopilot"]["jev"], "auto");
    assert_eq!(body["autopilot"]["breakeven_r"], 0.0);
    assert_eq!(body["autopilot"]["trail_r"], 0.0);
    assert_eq!(body["autopilot"]["profit_harvest"]["arm_r"], 0.2);
    assert_eq!(body["autopilot"]["profit_harvest"]["trail_r"], 0.2);
    assert_eq!(body["autopilot"]["profit_harvest"]["min_profit"], 0.5);
    assert_eq!(
        body["autopilot"]["profit_harvest"]["giveback_fraction"],
        0.35
    );
    assert_eq!(
        body["autopilot"]["profit_harvest"]["reentry_cooldown_secs"],
        900
    );
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
    assert!(body["decisions"]["lastModel"].is_null());
    assert!(body["decisions"]["lastSuccessfulModel"].is_null());
    assert_eq!(body["model_budget"]["hourLimit"], 0, "unlimited by default");
    assert_eq!(body["model_budget"]["hourCalls"], 0);
    assert_eq!(body["model_budget"]["dayCalls"], 0);
    assert_eq!(body["broker_provider"], serde_json::Value::Null);
    // Without a connected ChatGPT subscription the route is the configured
    // chain for the default (balanced) tier, and nothing is cooling.
    assert_eq!(body["model_route"], serde_json::json!(["vendor/balanced"]));
    assert_eq!(body["model_cooldowns"], serde_json::json!([]));
}

#[actix_web::test]
async fn status_without_a_model_reports_an_empty_route() {
    let app = test::init_service(create_app(test_state(None))).await;
    let request = test::TestRequest::get().uri("/status").to_request();
    let body: serde_json::Value =
        test::read_body_json(test::call_service(&app, request).await).await;
    assert_eq!(body["model_route"], serde_json::json!([]));
    assert_eq!(body["model_cooldowns"], serde_json::json!([]));
}

#[actix_web::test]
async fn status_lists_model_cooldowns_and_the_operator_can_clear_them() {
    use std::time::{Duration, UNIX_EPOCH};
    use veyra_service::model::{
        Admission, CooldownFailure, CooldownReason, CooldownRegistry, ModelProvider,
    };

    // A pinned clock keeps `untilMs` exact.
    let now = UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    let cooldowns = CooldownRegistry::with_clock(Arc::new(move || now));
    let state = test_state(None).with_model_cooldowns(cooldowns.clone());
    for (model, reason) in [
        ("z-ai/glm-5.3-flash", CooldownReason::ProviderRejected),
        (
            "deepseek/deepseek-v4.1-flash",
            CooldownReason::InsufficientCredits,
        ),
    ] {
        match cooldowns.admit(ModelProvider::OpenRouter, model) {
            Admission::Ready(ticket) => ticket.fail(CooldownFailure::new(reason)),
            Admission::Cooling(_) => panic!("{model} starts open"),
        }
    }
    let app = test::init_service(create_app(state)).await;

    let request = test::TestRequest::get().uri("/status").to_request();
    let body: serde_json::Value =
        test::read_body_json(test::call_service(&app, request).await).await;
    assert_eq!(
        body["model_cooldowns"],
        serde_json::json!([
            {
                "provider": "openrouter",
                "model": "deepseek/deepseek-v4.1-flash",
                "reason": "insufficient_credits",
                "untilMs": 1_790_001_800_000_u64,
                "failures": 1
            },
            {
                "provider": "openrouter",
                "model": "z-ai/glm-5.3-flash",
                "reason": "provider_rejected",
                "untilMs": 1_790_003_600_000_u64,
                "failures": 1
            }
        ])
    );

    let request = test::TestRequest::post()
        .uri("/model/cooldowns/clear")
        .to_request();
    let response = test::call_service(&app, request).await;
    assert!(response.status().is_success());
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(
        body,
        serde_json::json!({"cleared": 2, "model_cooldowns": []})
    );
    assert!(cooldowns.is_empty());
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

/// Audit store that is always down, to prove readiness degrades.
#[derive(Debug)]
struct BrokenTrail;

#[async_trait::async_trait]
impl AuditTrail for BrokenTrail {
    fn provider(&self) -> AuditProvider {
        AuditProvider::Postgres
    }

    async fn record(&self, _event: AuditEvent) -> Result<(), AuditError> {
        Err(AuditError::Storage {
            reason: "down".to_owned(),
        })
    }

    async fn recent(&self, _limit: u32) -> Result<Vec<AuditRow>, AuditError> {
        Err(AuditError::Storage {
            reason: "down".to_owned(),
        })
    }

    async fn prune(&self, _keep_days: u32) -> Result<u64, AuditError> {
        Err(AuditError::Storage {
            reason: "down".to_owned(),
        })
    }
}

#[actix_web::test]
async fn readiness_is_dependency_aware() {
    // Nothing configured: the process is ready with both dependencies disabled.
    let bare = test::init_service(create_app(test_state(None))).await;
    let request = test::TestRequest::get().uri("/ready").to_request();
    let response = test::call_service(&bare, request).await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["broker"], "unconfigured");
    assert_eq!(body["audit"], "disabled");

    // A configured broker without heartbeats degrades readiness.
    let stale = test::init_service(create_app(test_state(Some(ea_broker())))).await;
    let request = test::TestRequest::get().uri("/ready").to_request();
    let response = test::call_service(&stale, request).await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["broker"], "stale");

    // An unreachable audit store degrades readiness as well.
    let broken = AuditRuntime::new(Arc::new(BrokenTrail));
    let state = test_state(None).with_audit(Some(broken));
    let app = test::init_service(create_app(state)).await;
    let request = test::TestRequest::get().uri("/ready").to_request();
    let response = test::call_service(&app, request).await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["audit"], "unavailable");

    // A fresh terminal heartbeat plus a healthy trail: ready.
    let broker = ea_broker();
    if let Some(link) = broker.ea_link() {
        link.record(snapshot());
    }
    let healthy = AuditRuntime::new(Arc::new(MemoryTrail::default()));
    let state = test_state(Some(broker)).with_audit(Some(healthy));
    let app = test::init_service(create_app(state)).await;
    let request = test::TestRequest::get().uri("/ready").to_request();
    let response = test::call_service(&app, request).await;
    let body: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["broker"], "connected");
    assert_eq!(body["audit"], "ok");
}
