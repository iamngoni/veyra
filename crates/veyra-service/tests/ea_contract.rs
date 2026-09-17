//! Contract tests for the EA control channel: token authentication, payload
//! validation, the probe round trip, and staleness handling. These exercise
//! the same Actix application the service mounts, in-process.

use std::sync::Arc;
use std::time::Duration;

use actix_web::http::StatusCode;
use actix_web::test;
use serde_json::{Value, json};

use veyra_service::broker::{
    BrokerLink, BrokerProvider, CommandKind, CommandPayload, CommandState, EaLink, EaToken,
    create_ea_app,
};

const TOKEN: &str = "test-token-1234567890";

fn link(stale_after: Duration) -> Arc<EaLink> {
    Arc::new(EaLink::new(
        EaToken::parse(TOKEN).expect("token must validate"),
        stale_after,
        Duration::from_secs(5),
    ))
}

fn body(kind: &str) -> Value {
    json!({
        "t": kind,
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

async fn post(link: Arc<EaLink>, payload: Value) -> (StatusCode, Value) {
    let app = test::init_service(create_ea_app(link)).await;
    let request = test::TestRequest::post()
        .uri("/ea/poll")
        .set_json(&payload)
        .to_request();
    let response = test::call_service(&app, request).await;
    let status = response.status();
    let bytes = test::read_body(response).await;
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[actix_web::test]
async fn rejects_unknown_token() {
    let mut payload = body("hello");
    payload["token"] = json!("wrong-token-000000");
    let (status, response) = post(link(Duration::from_secs(10)), payload).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(response["t"], "error");
    assert_eq!(response["code"], "unauthorized");
}

#[actix_web::test]
async fn hello_is_answered_with_ping_and_records_snapshot() {
    let link = link(Duration::from_secs(10));
    let (status, response) = post(link.clone(), body("hello")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["t"], "ping");

    let report = link.report().await;
    let snapshot = report.snapshot.expect("snapshot must be recorded");
    assert_eq!(snapshot.login().value(), 94168);
    assert_eq!(snapshot.server().as_str(), "IFCMarkets-Real");
    assert_eq!(snapshot.symbol().as_str(), "EURUSD");
    assert!(snapshot.connected());
    assert!(snapshot.trade_allowed());
    assert!(report.fresh);
}

#[actix_web::test]
async fn heartbeat_pings_until_pong_then_goes_quiet() {
    let link = link(Duration::from_secs(10));

    // Before a pong has ever been seen, heartbeats request one.
    let (status, response) = post(link.clone(), body("hb")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["t"], "ping");
    assert!(link.report().await.snapshot.is_some());

    // The pong proves the return path.
    let (status, _) = post(link.clone(), body("pong")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(link.pongs_received(), 1);

    // Afterwards the channel is idle again.
    let (status, response) = post(link.clone(), body("hb")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["t"], "none");
}

#[actix_web::test]
async fn pong_is_counted() {
    let link = link(Duration::from_secs(10));
    let (status, response) = post(link.clone(), body("pong")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["t"], "none");
    assert_eq!(link.pongs_received(), 1);
}

#[actix_web::test]
async fn invalid_payload_is_rejected() {
    let link = link(Duration::from_secs(10));
    for invalid_acct in [json!(0), json!(-5)] {
        let mut payload = body("hello");
        payload["acct"] = invalid_acct;
        let (status, response) = post(link.clone(), payload).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["code"], "invalid_payload");
        assert!(link.report().await.snapshot.is_none());
    }
}

#[actix_web::test]
async fn unknown_kind_and_malformed_json_are_rejected() {
    let mut payload = body("hello");
    payload["t"] = json!("wat");
    let (status, _) = post(link(Duration::from_secs(10)), payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let app = test::init_service(create_ea_app(link(Duration::from_secs(10)))).await;
    let request = test::TestRequest::post()
        .uri("/ea/poll")
        .insert_header(("content-type", "application/json"))
        .set_payload("not json")
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[actix_web::test]
async fn trailing_nulls_are_tolerated() {
    let link = link(Duration::from_secs(10));
    let app = test::init_service(create_ea_app(link.clone())).await;

    let mut payload = serde_json::to_vec(&body("hb")).expect("payload serializes");
    payload.extend_from_slice(&[0, 0, 0]);

    let request = test::TestRequest::post()
        .uri("/ea/poll")
        .insert_header(("content-type", "application/json"))
        .set_payload(payload)
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(link.report().await.snapshot.is_some());
}

#[actix_web::test]
async fn empty_state_is_not_fresh() {
    let report = link(Duration::from_secs(10)).report().await;
    assert!(report.snapshot.is_none());
    assert!(!report.fresh);
}

#[actix_web::test]
async fn old_state_is_stale_but_still_reported() {
    let link = link(Duration::ZERO);
    let (status, _) = post(link.clone(), body("hb")).await;
    assert_eq!(status, StatusCode::OK);

    let report = link.report().await;
    assert!(report.snapshot.is_some());
    assert!(!report.fresh);
}

#[actix_web::test]
async fn queued_command_is_delivered_and_acknowledged() {
    let link = link(Duration::from_secs(10));
    let id = link.enqueue(CommandKind::Ping);
    assert_eq!(
        link.command(id).expect("recorded").state,
        CommandState::Pending
    );

    let (status, response) = post(link.clone(), body("hb")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["t"], "cmd");
    assert_eq!(response["kind"], "ping");
    assert_eq!(response["id"], id.to_string());

    let ack = json!({"t": "ack", "token": TOKEN, "id": id.to_string(), "ok": true});
    let (status, _) = post(link.clone(), ack).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        link.command(id).expect("recorded").state,
        CommandState::Completed {
            payload: CommandPayload::Ping
        }
    );
}

#[actix_web::test]
async fn snapshot_ack_validates_its_typed_payload() {
    let link = link(Duration::from_secs(10));
    let id = link.enqueue(CommandKind::AccountSnapshot);
    let (_, delivered) = post(link.clone(), body("hb")).await;
    assert_eq!(delivered["kind"], "account_snapshot");

    let ack = json!({
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
    });
    let (status, _) = post(link.clone(), ack).await;
    assert_eq!(status, StatusCode::OK);

    match link.command(id).expect("recorded").state {
        CommandState::Completed {
            payload: CommandPayload::AccountSnapshot(snapshot),
        } => {
            assert_eq!(snapshot.balance, 20.57);
            assert_eq!(snapshot.orders, 0);
            assert_eq!(snapshot.server_time, 1_758_000_000);
        }
        other => panic!("unexpected state: {other:?}"),
    }
}

#[actix_web::test]
async fn failed_malformed_and_unknown_acks_are_handled() {
    let link = link(Duration::from_secs(10));

    // Remote-reported failure.
    let failing = link.enqueue(CommandKind::Ping);
    let ack = json!({"t": "ack", "token": TOKEN, "id": failing.to_string(), "ok": false, "error": "nope"});
    post(link.clone(), ack).await;
    assert_eq!(
        link.command(failing).expect("recorded").state,
        CommandState::Failed {
            reason: "nope".to_owned()
        }
    );

    // Successful ack with an invalid payload.
    let snapshot = link.enqueue(CommandKind::AccountSnapshot);
    let ack = json!({"t": "ack", "token": TOKEN, "id": snapshot.to_string(), "ok": true,
                     "data": {"balance": "not-a-number"}});
    post(link.clone(), ack).await;
    match link.command(snapshot).expect("recorded").state {
        CommandState::Failed { reason } => assert!(reason.contains("invalid snapshot payload")),
        other => panic!("unexpected state: {other:?}"),
    }

    // Unknown ids are ignored without error.
    let ack = json!({"t": "ack", "token": TOKEN, "id": "00000000-0000-4000-8000-000000000000", "ok": true});
    let (status, _) = post(link.clone(), ack).await;
    assert_eq!(status, StatusCode::OK);
}

#[actix_web::test]
async fn duplicate_acks_cannot_overwrite_a_finished_command() {
    let link = link(Duration::from_secs(10));
    let id = link.enqueue(CommandKind::Ping);

    let ack = json!({"t": "ack", "token": TOKEN, "id": id.to_string(), "ok": true});
    post(link.clone(), ack).await;

    let late =
        json!({"t": "ack", "token": TOKEN, "id": id.to_string(), "ok": false, "error": "late"});
    post(link.clone(), late).await;

    assert_eq!(
        link.command(id).expect("recorded").state,
        CommandState::Completed {
            payload: CommandPayload::Ping
        }
    );
}

#[actix_web::test]
async fn pending_commands_time_out_without_an_ack() {
    let link = Arc::new(EaLink::new(
        EaToken::parse(TOKEN).expect("token"),
        Duration::from_secs(10),
        Duration::ZERO,
    ));
    let id = link.enqueue(CommandKind::Ping);

    let (status, response) = post(link.clone(), body("hb")).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(response["t"], "cmd");
    assert_eq!(
        link.command(id).expect("recorded").state,
        CommandState::Failed {
            reason: "timeout".to_owned()
        }
    );
}

#[actix_web::test]
async fn polls_missing_identity_fields_are_rejected() {
    for missing in ["server", "symbol"] {
        let link = link(Duration::from_secs(10));
        let mut payload = body("hb");
        payload
            .as_object_mut()
            .expect("payload is an object")
            .remove(missing);

        let (status, response) = post(link, payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["code"], "invalid_payload");
    }
}

#[actix_web::test]
async fn link_reports_its_provider() {
    let link = link(Duration::from_secs(10));
    assert_eq!(BrokerLink::provider(&*link), BrokerProvider::Ea);
}
